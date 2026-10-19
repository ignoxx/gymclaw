//! Post-workout analysis and progression (port of gymclaw/services/audit.py). Owner: WP-train.
//! Finishing writes the audit and progression atomically, then requests replanning.
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{AgentEvent, ExerciseProgression, ExerciseStatus, PlannedSession, SessionStatus, SetLog, SetType, WorkoutExercise, WorkoutStatus};
use crate::domain::core::{emit, mutate};
use crate::domain::progression::{PerformanceSet, ProgressionDecision, double_progression};
use crate::domain::workout::{cancel_rest, exercises, load_workout, logs, name, transition, unresolved};
use crate::error::Result;
use crate::time::Ts;

/// The saved audit of a finished workout, with the current crowd feedback.
pub fn get_audit(db: &Connection, workout_id: &str) -> Result<Value> {
    let workout = load_workout(db, workout_id, None)?;
    for event in AgentEvent::select(db, "WHERE type = 'workout.completed'", [])? {
        let result = event.payload_json.pointer("/result/data").cloned().unwrap_or(Value::Null);
        if result.get("workout_id").and_then(Value::as_str) == Some(workout_id) {
            // Arrival label may be supplied after finish; don't return stale feedback.
            let mut result = result;
            result["crowd_feedback"] = json!(workout.crowd_feedback);
            return Ok(result);
        }
    }
    crate::bail!("AUDIT_NOT_READY", "Finish workout before requesting audit")
}

/// Next-weight decision for one exercise from its logged working sets, which it also returns.
fn decide(db: &Connection, exercise: &WorkoutExercise) -> Result<(ProgressionDecision, Vec<SetLog>)> {
    let working = logs(db, &exercise.id, Some(SetType::Working))?;
    let sets: Vec<PerformanceSet> = working.iter().map(|s| PerformanceSet { weight: s.weight, reps: s.reps }).collect();
    let required = exercise.config_json.get("working_sets").and_then(Value::as_u64).unwrap_or(0) as usize;
    let increment = exercise.config_json.get("increment").and_then(Value::as_f64).unwrap_or(2.5);
    let mut decision = double_progression(&exercise.exercise_id, exercise.target_weight, exercise.rep_max, required, increment, &sets);
    if exercise.target_weight == 0.0 && !working.is_empty() {
        // Unknown starting weight: the heaviest working set becomes the baseline target.
        decision.next_weight = working.iter().map(|s| s.weight).fold(f64::MIN, f64::max);
        decision.progressed = false;
        decision.reason = "baseline_from_first_session";
    }
    Ok((decision, working))
}

/// Fix a logged set's weight and/or reps, during or after its workout. When the finished workout set
/// this exercise's next weight, that weight is recomputed from the corrected sets.
pub fn correct_set(db: &Connection, set_id: &str, weight: Option<f64>, reps: Option<i64>, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "set.corrected", request_id, json!({"set_id": set_id, "weight": weight, "reps": reps}), now, || {
        if weight.is_none() && reps.is_none() {
            crate::bail!("ARGUMENT_REQUIRED", "Pass weight and/or reps");
        }
        let Some(mut set) = SetLog::get(db, set_id)? else { crate::bail!("SET_NOT_FOUND", "No logged set {set_id}") };
        let before = json!({"weight": set.weight, "reps": set.reps});
        set.weight = weight.unwrap_or(set.weight);
        set.reps = reps.unwrap_or(set.reps);
        set.update(db)?;
        let Some(exercise) = WorkoutExercise::get(db, &set.workout_exercise_id)? else { crate::bail!("EXERCISE_NOT_FOUND", "Set {set_id} has no exercise") };
        let mut next_weight = None;
        if let Some(mut progression) = ExerciseProgression::get(db, &exercise.exercise_id)?.filter(|p| p.source_workout_id == exercise.workout_session_id) {
            progression.next_weight = decide(db, &exercise)?.0.next_weight;
            progression.updated_at = now;
            progression.update(db)?;
            next_weight = Some(progression.next_weight);
        }
        Ok(json!({"set_id": set.id, "exercise": name(&exercise), "before": before, "after": {"weight": set.weight, "reps": set.reps}, "next_weight": next_weight}))
    })
}

/// Finish a WORKOUT_COMPLETE workout: progression, PLAN_UPDATED. Mutation result envelope.
pub fn finish(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.completed", request_id, json!({"workout_id": workout_id}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        if workout.status == WorkoutStatus::PlanUpdated {
            crate::bail!("WORKOUT_ALREADY_FINISHED", "Workout already finished; read audit or retry original request ID");
        }
        let rows = exercises(db, workout_id)?;
        if rows.iter().any(|e| unresolved(e.status)) {
            crate::bail!("UNRESOLVED_EXERCISES", "Resolve deferred/pending exercises or explicitly skip before finishing");
        }
        if workout.status != WorkoutStatus::WorkoutComplete {
            crate::bail!("INVALID_TRANSITION", "Cannot finish workout in {}", workout.status);
        }
        cancel_rest(db, workout_id, now)?;
        transition(db, &mut workout, WorkoutStatus::PostAnalysis, now)?;
        let duration = workout.started_at.map_or(0, |s| (now - s).as_secs());
        workout.completed_at = Some(now);
        workout.last_action_at = Some(now);
        workout.actual_duration_seconds = Some(duration);
        workout.update(db)?;
        let (mut performance, mut progressions) = (vec![], vec![]);
        let (mut warmups, mut completed_sets, mut progressed) = (0, 0, 0);
        for exercise in &rows {
            let (decision, working) = decide(db, exercise)?;
            warmups += logs(db, &exercise.id, Some(SetType::Warmup))?.len();
            completed_sets += working.len();
            if exercise.status == ExerciseStatus::Completed {
                let target = ExerciseProgression { exercise_id: exercise.exercise_id.clone(), next_weight: decision.next_weight, updated_at: now, source_workout_id: workout.id.clone() };
                if ExerciseProgression::get(db, &exercise.exercise_id)?.is_some() { target.update(db)? } else { target.insert(db)? }
                if decision.progressed {
                    let mut payload = serde_json::to_value(&decision)?;
                    payload["workout_id"] = json!(workout.id);
                    emit(db, "progression.updated", now, payload)?;
                }
            }
            progressed += decision.progressed as usize;
            progressions.push(serde_json::to_value(&decision)?);
            performance.push(json!({
                "id": exercise.id, "exercise_id": exercise.exercise_id, "name": name(exercise), "status": exercise.status,
                "planned_sets": exercise.planned_working_sets, "completed_sets": working.len(),
                "sets": working.iter().map(|s| json!({"weight": s.weight, "reps": s.reps})).collect::<Vec<_>>(),
                "substituted_from": exercise.substituted_from_exercise_id, "reason": exercise.deferred_reason,
            }));
        }
        if let Some(id) = &workout.planned_session_id
            && let Some(mut planned) = PlannedSession::get(db, id)?
        {
            planned.status = SessionStatus::Completed;
            planned.update(db)?;
        }
        let snapshot = &workout.template_snapshot;
        let planned_sets: i64 = snapshot["exercises"].as_array().into_iter().flatten().filter_map(|e| e["working_sets"].as_i64()).sum();
        let template_name = snapshot["name"].as_str().unwrap_or_default().to_string();
        let error = |eta: Option<Ts>| eta.map(|eta| (now - eta).as_secs_f64());
        let count = |status: ExerciseStatus| rows.iter().filter(|e| e.status == status).count();
        let audit = json!({
            "workout_id": workout.id, "template_name": template_name,
            "planned_sets": planned_sets, "completed_sets": completed_sets, "warmup_sets": warmups,
            "actual_duration_seconds": duration,
            "completed_at": now, "initial_eta": workout.initial_eta, "last_estimated_finish": workout.final_eta,
            "initial_eta_error_seconds": error(workout.initial_eta), "last_eta_error_seconds": error(workout.final_eta),
            "substitutions": count(ExerciseStatus::Substituted), "skips": count(ExerciseStatus::Skipped),
            "equipment_waits": workout.waited_for_equipment_count, "crowd_feedback": workout.crowd_feedback,
            "progressions": progressions, "progression_count": progressed, "exercises": performance,
            "instruction": format!("{template_name} complete. {completed_sets}/{planned_sets} working sets · {} min · {progressed} progressions.", duration.div_euclid(60)),
        });
        // PLAN_UPDATED means training targets persisted; calendar replanning is queued.
        transition(db, &mut workout, WorkoutStatus::PlanUpdated, now)?;
        emit(db, "planning.replan_required", now, json!({"workout_id": workout.id, "reason": "workout_completed"}))?;
        Ok(audit)
    })
}
