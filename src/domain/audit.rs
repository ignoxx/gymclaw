//! Post-workout analysis and progression.
//! Finishing writes the audit and progression atomically, then requests replanning.
use serde_json::{Value, json};

use crate::db::models::{
    AgentEvent, ExerciseProgression, ExerciseStatus, PlannedSession, SessionStatus, SetLog, SetType, WorkoutExercise,
    WorkoutStatus,
};
use crate::db::{Connection, new_id};
use crate::domain::core::{emit, mutate};
use crate::domain::progression::{PerformanceSet, ProgressionDecision, double_progression};
use crate::domain::set_parser::{SetInput, parse_block, parse_set};
use crate::domain::templates::{Load, Measure, pick};
use crate::domain::trends;
use crate::domain::workout::{
    self, cancel_rest, exercises, guide_id, kind, load_workout, logs, measure, name, reps_alone, set_measure,
    transition, unresolved,
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
/// Assisted machines progress downwards: double progression runs on the negated counterweight, and
/// once no help is left, moving to the unassisted version is the agent's call.
fn decide(db: &Connection, exercise: &WorkoutExercise) -> Result<(ProgressionDecision, Vec<SetLog>)> {
    let working = logs(db, &exercise.id, Some(SetType::Working))?;
    let set_kind = kind(exercise);
    let sets: Vec<PerformanceSet> =
        working.iter().map(|s| PerformanceSet { weight: set_kind.signed(s.weight), reps: s.reps }).collect();
    let measured = measure(exercise);
    if measured != Measure::Reps {
        // A ride, a plank or a carry has no weight to progress; the agent adjusts the time or distance target.
        let decision = ProgressionDecision {
            exercise_id: exercise.exercise_id.clone(),
            previous_weight: exercise.target_weight,
            next_weight: exercise.target_weight,
            progressed: false,
            reason: if measured == Measure::Time { "timed" } else { "distance" },
        };
        return Ok((decision, working));
    }
    let required = exercise.config_json.get("working_sets").and_then(Value::as_u64).unwrap_or(0) as usize;
    let increment = exercise.config_json.get("increment").and_then(Value::as_f64).unwrap_or(2.5);
    let target = set_kind.signed(exercise.target_weight);
    let mut decision = double_progression(
        &exercise.exercise_id,
        target,
        exercise.rep_min,
        exercise.rep_max,
        required,
        increment,
        &sets,
    );
    decision.previous_weight = exercise.target_weight;
    if set_kind.load != Load::Assisted {
        // Easy sessions jump further, grinds hold (`trends::sized`). Its signals are kilograms lifted.
        decision = trends::sized(decision, trends::effort_of(db, exercise, &sets)?, increment);
    }
    // Back from signed; an assisted target stops at no help (0).
    decision.next_weight = set_kind.signed(decision.next_weight).max(0.0);
    if set_kind.load == Load::Assisted && exercise.target_weight == 0.0 && decision.progressed {
        decision.progressed = false;
        decision.reason = "no_assistance_left";
    } else if exercise.target_weight == 0.0 && !working.is_empty() {
        // Unknown starting weight: the best working set (heaviest, or least help) becomes the baseline.
        let best = sets.iter().map(|s| s.weight).fold(f64::MIN, f64::max);
        decision.next_weight = set_kind.signed(best).max(0.0);
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

/// The values a set is corrected with: (weight/reps, seconds, meters) given.
fn fixed_by(reps: bool, seconds: bool, meters: bool) -> &'static str {
    match (reps, seconds, meters) {
        (_, true, false) => "minutes",
        (_, false, true) => "meters",
        _ => "weight and/or reps",
    }
}

/// Fix a logged set's weight and/or reps, or a timed set's length, or a distance, during or after its
/// workout. When the finished workout set this exercise's next weight, that weight is recomputed from the
/// corrected sets.
#[allow(clippy::too_many_arguments)]
pub fn correct_set(
    db: &Connection,
    set_id: &str,
    weight: Option<f64>,
    reps: Option<i64>,
    seconds: Option<i64>,
    meters: Option<f64>,
    now: Ts,
    request_id: &str,
) -> Result<Value> {
    let mut intent = json!({"set_id": set_id, "weight": weight, "reps": reps, "seconds": seconds});
    if meters.is_some() {
        intent["meters"] = json!(meters);
    }
    mutate(db, "set.corrected", request_id, intent, now, || {
        let given = [weight.is_some() || reps.is_some(), seconds.is_some(), meters.is_some()];
        if given.iter().all(|g| !g) {
            crate::bail!("ARGUMENT_REQUIRED", "Pass weight and/or reps, or minutes or meters for a time or distance");
        }
        let Some(mut set) = SetLog::get(db, set_id)? else { crate::bail!("SET_NOT_FOUND", "No logged set {set_id}") };
        let how = fixed_by(true, set.seconds.is_some(), set.meters.is_some());
        if given.iter().filter(|g| **g).count() > 1 || fixed_by(given[0], given[1], given[2]) != how {
            crate::bail!("INVALID_SET", "Set {set_id} is fixed with {how}");
        }
        let before = json!({"weight": set.weight, "reps": set.reps, "seconds": set.seconds, "meters": set.meters});
        set.weight = weight.unwrap_or(set.weight);
        set.reps = reps.unwrap_or(set.reps);
        set.seconds = seconds.or(set.seconds);
        set.meters = meters.or(set.meters);
        SetInput { seconds: set.seconds, meters: set.meters, ..SetInput::reps(set.weight, set.reps, set.set_type) }
            .validate()?;
        set.update(db)?;
        let Some(exercise) = WorkoutExercise::get(db, &set.workout_exercise_id)? else {
            crate::bail!("EXERCISE_NOT_FOUND", "Set {set_id} has no exercise")
        };
        let next_weight = refresh_progression(db, &exercise, now)?;
        Ok(
            json!({"set_id": set.id, "workout_id": exercise.workout_session_id, "exercise": name(&exercise), "before": before, "after": {"weight": set.weight, "reps": set.reps, "seconds": set.seconds, "meters": set.meters}, "next_weight": next_weight}),
        )
    })
}

/// A set the owner did but never logged, added to a finished workout ("I cycled 25 min", "I did one more
/// bench set"). `which` is the exercise's id, guide_id or name in that workout; `text` is a set as typed
/// in the workout ("25 min", "2 km", "10x40"). A skipped exercise counts as done, and the first set decides
/// reps, time or distance. Logged at the workout's finish so history keeps its order.
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
        let value = match parse_block(text, measure(&row)) {
            Some(value) => value,
            None => parse_set(text, Some(row.target_weight), reps_alone(&row))?,
        };
        value.validate()?;
        let working = logs(db, &row.id, Some(SetType::Working))?;
        if value.measure() != measure(&row) {
            if !working.is_empty() {
                let how = measure(&row).example();
                crate::bail!("INVALID_SET", "{} is logged as {how} in that workout", name(&row));
            }
            set_measure(db, &mut row, value.measure())?;
        }
        let set = SetLog {
            id: new_id(),
            workout_exercise_id: row.id.clone(),
            set_number: working.iter().map(|s| s.set_number).max().unwrap_or(0) + 1,
            set_type: SetType::Working,
            weight: value.weight,
            reps: value.reps,
            logged_at: workout.completed_at.or(workout.started_at).unwrap_or(now),
            rest_due_at: None,
            seconds: value.seconds,
            meters: value.meters,
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
            "seconds": set.seconds, "meters": set.meters, "status_before": was, "status": row.status, "next_weight": next_weight}),
        )
    })
}

/// One exercise in a finished workout's report: status and working sets.
fn performed(exercise: &WorkoutExercise, working: &[SetLog]) -> Value {
    json!({
        "id": exercise.id, "exercise_id": exercise.exercise_id, "name": name(exercise), "status": exercise.status,
        "planned_sets": exercise.planned_working_sets, "completed_sets": working.len(),
        "sets": working.iter().map(|s| json!({"weight": s.weight, "reps": s.reps, "seconds": s.seconds, "meters": s.meters})).collect::<Vec<_>>(),
        "substituted_from": exercise.substituted_from_exercise_id, "reason": exercise.deferred_reason,
    })
}

/// A finished workout's exercises and next-weight decisions from its sets as they are now (after
/// corrections), in `finish`'s result envelope: what its finish card is re-rendered from.
pub fn report(db: &Connection, workout_id: &str) -> Result<Value> {
    let (mut performance, mut progressions) = (vec![], vec![]);
    for exercise in exercises(db, workout_id)? {
        let (decision, working) = decide(db, &exercise)?;
        progressions.push(serde_json::to_value(&decision)?);
        performance.push(performed(&exercise, &working));
    }
    Ok(json!({"data": {"workout_id": workout_id, "exercises": performance, "progressions": progressions}}))
}

/// Finish a WORKOUT_COMPLETE workout: progression, PLAN_UPDATED. Mutation result envelope.
/// A workout with nothing logged can't finish (it would count as trained): the agent logs what the owner
/// did, or discards it (`workout::discard`).
pub fn finish(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    if workout::logged_sets(db, workout_id)? == 0 {
        crate::bail!(
            "NOTHING_LOGGED",
            "Nothing is logged, so this workout can't count as done. Log what the owner did first (e.g. \"25 min\"), or discard it if they didn't train."
        );
    }
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
            performance.push(performed(exercise, &working));
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
        // PLAN_UPDATED means training targets persisted. The summary card is the wrap-up; the agent wakes
        // only to fix upcoming sessions (an agent take was praise with nothing in it, 2026-11-13).
        transition(db, &mut workout, WorkoutStatus::PlanUpdated)?;
        emit(db, "planning.replan_required", now, json!({"workout_id": workout.id, "reason": "workout_completed"}))?;
        Ok(audit)
    })
}
