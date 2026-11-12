//! Post-workout analysis and progression.
//! Finishing writes the audit and progression atomically, then requests replanning.
use serde_json::{Value, json};

use crate::db::models::{
    AgentEvent, ExerciseProgression, ExerciseStatus, PlannedSession, SessionStatus, SetLog, SetType, WorkoutExercise,
    WorkoutStatus,
};
use crate::db::{Connection, new_id};
use crate::domain::core::{emit, mutate};
use crate::domain::illustrations::bodyweight;
use crate::domain::progression::{PerformanceSet, ProgressionDecision, double_progression};
use crate::domain::set_parser::{SetInput, parse_duration, parse_set};
use crate::domain::templates::Measure;
use crate::domain::templates::pick;
use crate::domain::workout::{
    cancel_rest, exercises, guide_id, load_workout, logs, measure, name, set_measure, transition, unresolved,
};
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
    if measure(exercise) == Measure::Time {
        // A ride or a plank has no weight to progress; the agent adjusts the time target.
        let decision = ProgressionDecision {
            exercise_id: exercise.exercise_id.clone(),
            previous_weight: exercise.target_weight,
            next_weight: exercise.target_weight,
            progressed: false,
            reason: "timed",
        };
        return Ok((decision, working));
    }
    let required = exercise.config_json.get("working_sets").and_then(Value::as_u64).unwrap_or(0) as usize;
    let increment = exercise.config_json.get("increment").and_then(Value::as_f64).unwrap_or(2.5);
    let mut decision =
        double_progression(&exercise.exercise_id, exercise.target_weight, exercise.rep_max, required, increment, &sets);
    if exercise.target_weight == 0.0 && !working.is_empty() {
        // Unknown starting weight: the heaviest working set becomes the baseline target.
        decision.next_weight = working.iter().map(|s| s.weight).fold(f64::MIN, f64::max);
        decision.progressed = false;
        decision.reason = "baseline_from_first_session";
    }
    Ok((decision, working))
}

/// When this exercise's finished workout set its next weight, recompute it from the sets as they are now.
fn refresh_progression(db: &Connection, exercise: &WorkoutExercise, now: Ts) -> Result<Option<f64>> {
    let Some(mut progression) = ExerciseProgression::get(db, &exercise.exercise_id)?
        .filter(|p| p.source_workout_id == exercise.workout_session_id)
    else {
        return Ok(None);
    };
    progression.next_weight = decide(db, exercise)?.0.next_weight;
    progression.updated_at = now;
    progression.update(db)?;
    Ok(Some(progression.next_weight))
}

/// Fix a logged set's weight and/or reps, or a timed set's length, during or after its workout. When
/// the finished workout set this exercise's next weight, that weight is recomputed from the corrected sets.
pub fn correct_set(
    db: &Connection,
    set_id: &str,
    weight: Option<f64>,
    reps: Option<i64>,
    seconds: Option<i64>,
    now: Ts,
    request_id: &str,
) -> Result<Value> {
    let intent = json!({"set_id": set_id, "weight": weight, "reps": reps, "seconds": seconds});
    mutate(db, "set.corrected", request_id, intent, now, || {
        if weight.is_none() && reps.is_none() && seconds.is_none() {
            crate::bail!("ARGUMENT_REQUIRED", "Pass weight and/or reps, or minutes for a timed set");
        }
        let Some(mut set) = SetLog::get(db, set_id)? else { crate::bail!("SET_NOT_FOUND", "No logged set {set_id}") };
        if set.seconds.is_some() != seconds.is_some() || (seconds.is_some() && (weight.is_some() || reps.is_some())) {
            let how = if set.seconds.is_some() { "minutes" } else { "weight and/or reps" };
            crate::bail!("INVALID_SET", "Set {set_id} is fixed with {how}");
        }
        let before = json!({"weight": set.weight, "reps": set.reps, "seconds": set.seconds});
        set.weight = weight.unwrap_or(set.weight);
        set.reps = reps.unwrap_or(set.reps);
        set.seconds = seconds.or(set.seconds);
        SetInput { weight: set.weight, reps: set.reps, set_type: set.set_type, rir: None, seconds: set.seconds }
            .validate()?;
        set.update(db)?;
        let Some(exercise) = WorkoutExercise::get(db, &set.workout_exercise_id)? else {
            crate::bail!("EXERCISE_NOT_FOUND", "Set {set_id} has no exercise")
        };
        let next_weight = refresh_progression(db, &exercise, now)?;
        Ok(
            json!({"set_id": set.id, "exercise": name(&exercise), "before": before, "after": {"weight": set.weight, "reps": set.reps, "seconds": set.seconds}, "next_weight": next_weight}),
        )
    })
}

/// A set the owner did but never logged, added to a finished workout ("I cycled 25 min", "I did one more
/// bench set"). `which` is the exercise's id, guide_id or name in that workout; `text` is a set as typed
/// in the workout ("25 min", "10x40"). A skipped exercise counts as done, and the first set decides
/// reps or time. Logged at the workout's finish so history keeps its order.
pub fn add_set(db: &Connection, workout_id: &str, which: &str, text: &str, now: Ts, request_id: &str) -> Result<Value> {
    let intent = json!({"workout_id": workout_id, "which": which, "text": text});
    mutate(db, "set.added", request_id, intent, now, || {
        let workout = load_workout(db, workout_id, None)?;
        if workout.status != WorkoutStatus::PlanUpdated {
            crate::bail!("WORKOUT_RUNNING", "That workout is still running: log without workout_id");
        }
        let rows: Vec<WorkoutExercise> =
            exercises(db, workout_id)?.into_iter().filter(|e| e.status != ExerciseStatus::Substituted).collect();
        let keys: Vec<[Option<&str>; 3]> =
            rows.iter().map(|e| [Some(e.exercise_id.as_str()), Some(name(e)), guide_id(e)]).collect();
        let Some(index) = pick(which, &keys)? else {
            let names: Vec<&str> = rows.iter().map(name).collect();
            crate::bail!("EXERCISE_NOT_FOUND", "'{which}' isn't in that workout ({})", names.join(", "));
        };
        let mut row = rows[index].clone();
        let value = match parse_duration(text) {
            Some(seconds) => SetInput::timed(seconds),
            None => parse_set(text, SetType::Working, Some(row.target_weight), bodyweight(guide_id(&row)))?,
        };
        value.validate()?;
        let working = logs(db, &row.id, Some(SetType::Working))?;
        let timed = value.seconds.is_some();
        if timed != (measure(&row) == Measure::Time) {
            if !working.is_empty() {
                let how = if timed { "reps × weight" } else { "a time, e.g. 25 min" };
                crate::bail!("INVALID_SET", "{} is logged as {how} in that workout", name(&row));
            }
            set_measure(db, &mut row, timed)?;
        }
        let set = SetLog {
            id: new_id(),
            workout_exercise_id: row.id.clone(),
            set_number: working.iter().map(|s| s.set_number).max().unwrap_or(0) + 1,
            set_type: SetType::Working,
            weight: value.weight,
            reps: value.reps,
            rir_optional: None,
            logged_at: workout.completed_at.or(workout.started_at).unwrap_or(now),
            rest_started_at: None,
            rest_due_at: None,
            seconds: value.seconds,
        };
        set.insert(db)?;
        let was = row.status;
        if was != ExerciseStatus::Completed {
            row.status = ExerciseStatus::Completed;
            row.deferred_reason = None;
            row.update(db)?;
        }
        let next_weight = refresh_progression(db, &row, now)?;
        Ok(
            json!({"workout_id": workout.id, "exercise": name(&row), "set_id": set.id, "weight": set.weight, "reps": set.reps,
            "seconds": set.seconds, "status_before": was, "status": row.status, "next_weight": next_weight}),
        )
    })
}

/// Wakes the agent for its take under the summary card. The guidance rides on the event, so it costs
/// context only on the turn that needs it; as a generic replan event the agent read it as housekeeping
/// and stayed silent.
const WRAP_UP: &str = "Wrap-up due. The summary card in your context already shows the numbers (records, pace, volume, \
    next targets, the week so far). Add your take, up to three short lines: what the session means (a record after weeks \
    stuck, a comeback, grinding through a bad day, what got skipped) and one thing to aim for next time. Never repeat the card's numbers.";

/// Finish a WORKOUT_COMPLETE workout: progression, PLAN_UPDATED. Mutation result envelope.
pub fn finish(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.completed", request_id, json!({"workout_id": workout_id}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        if workout.status == WorkoutStatus::PlanUpdated {
            crate::bail!(
                "WORKOUT_ALREADY_FINISHED",
                "Workout already finished; read audit or retry original request ID"
            );
        }
        let rows = exercises(db, workout_id)?;
        if rows.iter().any(|e| unresolved(e.status)) {
            crate::bail!(
                "UNRESOLVED_EXERCISES",
                "Resolve deferred/pending exercises or explicitly skip before finishing"
            );
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
            if exercise.status == ExerciseStatus::Completed && measure(exercise) == Measure::Reps {
                let target = ExerciseProgression {
                    exercise_id: exercise.exercise_id.clone(),
                    next_weight: decision.next_weight,
                    updated_at: now,
                    source_workout_id: workout.id.clone(),
                };
                if ExerciseProgression::get(db, &exercise.exercise_id)?.is_some() {
                    target.update(db)?
                } else {
                    target.insert(db)?
                }
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
                "sets": working.iter().map(|s| json!({"weight": s.weight, "reps": s.reps, "seconds": s.seconds})).collect::<Vec<_>>(),
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
        let planned_sets: i64 =
            snapshot["exercises"].as_array().into_iter().flatten().filter_map(|e| e["working_sets"].as_i64()).sum();
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
        // PLAN_UPDATED means training targets persisted.
        transition(db, &mut workout, WorkoutStatus::PlanUpdated, now)?;
        emit(db, "workout.wrap_up", now, json!({"workout_id": workout.id, "instruction": WRAP_UP}))?;
        Ok(audit)
    })
}
