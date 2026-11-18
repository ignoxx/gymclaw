//! Post-workout analysis and progression.
//! Finishing writes the audit and progression atomically, then requests replanning.
use serde::Serialize;
use serde_json::{Value, json};

use crate::db::models::{
    ExerciseProgression, ExerciseStatus, PlannedSession, SessionStatus, SetLog, SetType, WorkoutExercise, WorkoutStatus,
};
use crate::db::{Connection, new_id};
use crate::domain::core::{emit, mutate};
use crate::domain::progression::{PerformanceSet, ProgressionDecision, double_progression};
use crate::domain::set_parser::{SetInput, parse_block, parse_set};
use crate::domain::templates::{Load, Measure, pick};
use crate::domain::trends;
use crate::domain::workout::{
    self, cancel_rest, exercises, guide_id, kind, load_workout, logs, measure, name, reps_alone, set_measure,
    transition, unresolved, working_count,
};
use crate::error::Result;
use crate::time::Ts;

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

/// What a set is corrected with, by which of seconds and meters it carries.
fn fixed_by(seconds: bool, meters: bool) -> &'static str {
    match (seconds, meters) {
        (true, false) => "minutes",
        (false, true) => "meters",
        _ => "weight and/or reps",
    }
}

/// What `edit_set` changes on one logged set; `None` keeps a field as it is.
#[derive(Debug, Default, Serialize)]
pub struct SetChange {
    pub weight: Option<f64>,
    pub reps: Option<i64>,
    pub seconds: Option<i64>,
    pub meters: Option<f64>,
    /// true: it was a warm-up; false: a working set.
    pub warmup: Option<bool>,
    /// Another exercise of the same workout (id, guide_id or name) the set was really for.
    pub exercise: Option<String>,
    /// The set never happened (a double log, a typo).
    pub delete: bool,
}

/// Sets of one exercise and type numbered 1.. in the order done, after one was moved, retyped or deleted.
fn renumber(db: &Connection, exercise_row_id: &str, set_type: SetType) -> Result<()> {
    let sets = logs(db, exercise_row_id, Some(set_type))?;
    // Two passes: (exercise, type, number) is unique, so numbers first move out of the way.
    let base = sets.iter().map(|s| s.set_number).max().unwrap_or(0) + 1;
    for (i, mut set) in sets.clone().into_iter().enumerate() {
        set.set_number = base + i as i64;
        set.update(db)?;
    }
    for (i, mut set) in sets.into_iter().enumerate() {
        set.set_number = i as i64 + 1;
        set.update(db)?;
    }
    Ok(())
}

/// Fix one logged set, during or after its workout: its weight and/or reps (or a timed set's length, or a
/// distance), whether it was a warm-up, which exercise it was for, or delete it. The sets left are
/// renumbered in the order done, and when the finished workout set an exercise's next weight, it is
/// recomputed from the sets as they are now. The coach stays on the exercise it is on.
pub fn edit_set(db: &Connection, set_id: &str, change: SetChange, now: Ts, request_id: &str) -> Result<Value> {
    let intent = json!({"set_id": set_id, "change": change});
    mutate(db, "set.corrected", request_id, intent, now, || {
        let SetChange { weight, reps, seconds, meters, warmup, exercise, delete } = &change;
        let numbers = [weight.is_some() || reps.is_some(), seconds.is_some(), meters.is_some()];
        let edits = numbers.iter().any(|g| *g) || warmup.is_some() || exercise.is_some();
        if !edits && !delete {
            crate::bail!(
                "ARGUMENT_REQUIRED",
                "Pass what changes: weight and/or reps (or minutes or meters), warmup, exercise, or delete"
            );
        }
        if edits && *delete {
            crate::bail!("INVALID_ARGUMENT", "delete removes the set; pass it alone");
        }
        let Some(mut set) = SetLog::get(db, set_id)? else { crate::bail!("SET_NOT_FOUND", "No logged set {set_id}") };
        let Some(from) = WorkoutExercise::get(db, &set.workout_exercise_id)? else {
            crate::bail!("EXERCISE_NOT_FOUND", "Set {set_id} has no exercise")
        };
        let before = json!({"exercise": name(&from), "set_type": set.set_type, "set_number": set.set_number,
            "weight": set.weight, "reps": set.reps, "seconds": set.seconds, "meters": set.meters});
        let was_type = set.set_type;
        if *delete {
            // A rest timer started by this set keeps running; it just no longer points at it.
            db.execute("UPDATE notification_job SET set_log_id = NULL WHERE set_log_id = ?1", [&set.id])?;
            db.execute("DELETE FROM set_log WHERE id = ?1", [&set.id])?;
            renumber(db, &from.id, was_type)?;
            let next_weight = refresh_progression(db, &from, now)?;
            return Ok(json!({"set_id": set.id, "workout_id": from.workout_session_id, "exercise": name(&from),
                "before": before, "deleted": true, "next_weight": next_weight}));
        }
        if numbers.iter().any(|g| *g) {
            let how = fixed_by(set.seconds.is_some(), set.meters.is_some());
            if numbers.iter().filter(|g| **g).count() > 1 || fixed_by(numbers[1], numbers[2]) != how {
                crate::bail!("INVALID_SET", "Set {set_id} is fixed with {how}");
            }
            set.weight = weight.unwrap_or(set.weight);
            set.reps = reps.unwrap_or(set.reps);
            set.seconds = seconds.or(set.seconds);
            set.meters = meters.or(set.meters);
        }
        if let Some(warmup) = warmup {
            set.set_type = if *warmup { SetType::Warmup } else { SetType::Working };
        }
        let mut to = match exercise {
            Some(which) => find_exercise(db, &from.workout_session_id, which)?,
            None => from.clone(),
        };
        let value =
            SetInput { seconds: set.seconds, meters: set.meters, ..SetInput::reps(set.weight, set.reps, set.set_type) };
        value.validate()?;
        if to.id != from.id && value.measure() != measure(&to) {
            if working_count(db, &to.id)? > 0 {
                let how = measure(&to).example();
                crate::bail!("INVALID_SET", "{} is logged as {how} in that workout", name(&to));
            }
            set_measure(db, &mut to, value.measure())?;
        }
        if to.id != from.id || set.set_type != was_type {
            // Last in its new group for now; renumbering puts it in the order done.
            let group = logs(db, &to.id, Some(set.set_type))?;
            set.set_number = group.iter().map(|s| s.set_number).max().unwrap_or(0) + 1;
            set.workout_exercise_id = to.id.clone();
        }
        set.update(db)?;
        renumber(db, &from.id, was_type)?;
        renumber(db, &to.id, set.set_type)?;
        let set = SetLog::get(db, set_id)?.unwrap_or(set);
        let mut next_weight = refresh_progression(db, &to, now)?;
        if to.id != from.id {
            refresh_progression(db, &from, now)?;
            // In a finished workout the exercise it moved to counts as done, as a set added later does.
            let finished = load_workout(db, &to.workout_session_id, None)?.status == WorkoutStatus::PlanUpdated;
            if finished && set.set_type == SetType::Working && to.status != ExerciseStatus::Completed {
                to.status = ExerciseStatus::Completed;
                to.deferred_reason = None;
                to.update(db)?;
                next_weight = refresh_progression(db, &to, now)?;
            }
        }
        Ok(json!({"set_id": set.id, "workout_id": to.workout_session_id, "exercise": name(&to), "before": before,
            "after": {"exercise": name(&to), "set_type": set.set_type, "set_number": set.set_number, "weight": set.weight,
                "reps": set.reps, "seconds": set.seconds, "meters": set.meters},
            "next_weight": next_weight}))
    })
}

/// The exercise `which` names in a workout: its row id, exercise id, guide_id or name. Substituted rows
/// don't count.
pub fn find_exercise(db: &Connection, workout_id: &str, which: &str) -> Result<WorkoutExercise> {
    let rows: Vec<WorkoutExercise> =
        exercises(db, workout_id)?.into_iter().filter(|e| e.status != ExerciseStatus::Substituted).collect();
    if let Some(row) = rows.iter().find(|e| e.id == which.trim()) {
        return Ok(row.clone());
    }
    let keys: Vec<[Option<&str>; 3]> =
        rows.iter().map(|e| [Some(e.exercise_id.as_str()), Some(name(e)), guide_id(e)]).collect();
    match pick(which, &keys)? {
        Some(index) => Ok(rows[index].clone()),
        None => {
            let names: Vec<&str> = rows.iter().map(name).collect();
            crate::bail!("EXERCISE_NOT_FOUND", "'{which}' isn't in that workout ({})", names.join(", "))
        }
    }
}

/// A set added to a named exercise outside the coach's flow: one the owner did but never logged in a
/// finished workout ("I cycled 25 min", "one more bench set"), or one for another exercise than the
/// coach's current one in a running workout ("leg press set 2 was 95"). The coach stays where it is.
/// `which` is the exercise's id, guide_id or name in that workout; `text` is a set as typed ("25 min",
/// "2 km", "10x40"). In a finished workout a skipped exercise counts as done and the set is logged at
/// the finish, so history keeps its order; in a running one an exercise not yet started counts as done
/// once its planned sets are in. The first set decides reps, time or distance.
pub fn add_set(
    db: &Connection,
    workout_id: &str,
    which: &str,
    text: &str,
    set_type: SetType,
    now: Ts,
    request_id: &str,
) -> Result<Value> {
    let intent = json!({"workout_id": workout_id, "which": which, "text": text, "set_type": set_type});
    mutate(db, "set.added", request_id, intent, now, || {
        let workout = load_workout(db, workout_id, None)?;
        let finished = workout.status == WorkoutStatus::PlanUpdated;
        let mut row = find_exercise(db, workout_id, which)?;
        let value = match parse_block(text, measure(&row)) {
            Some(value) => value,
            None => parse_set(text, Some(row.target_weight), reps_alone(&row))?,
        };
        let value = SetInput { set_type, ..value };
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
            set_number: logs(db, &row.id, Some(set_type))?.iter().map(|s| s.set_number).max().unwrap_or(0) + 1,
            set_type,
            weight: value.weight,
            reps: value.reps,
            logged_at: if finished { workout.completed_at.or(workout.started_at).unwrap_or(now) } else { now },
            rest_due_at: None,
            seconds: value.seconds,
            meters: value.meters,
        };
        set.insert(db)?;
        let was = row.status;
        let done = if finished {
            set_type == SetType::Working
        } else {
            row.status == ExerciseStatus::Pending && working_count(db, &row.id)? >= row.planned_working_sets
        };
        if done && was != ExerciseStatus::Completed {
            row.status = ExerciseStatus::Completed;
            row.deferred_reason = None;
            row.update(db)?;
        }
        let next_weight = refresh_progression(db, &row, now)?;
        Ok(json!({"workout_id": workout.id, "exercise": name(&row), "set_id": set.id, "set_type": set.set_type,
            "set_number": set.set_number, "weight": set.weight, "reps": set.reps, "seconds": set.seconds,
            "meters": set.meters, "status_before": was, "status": row.status, "next_weight": next_weight}))
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

/// A finished workout's audit from its sets as they are now (after corrections): sets, duration, ETA
/// error, crowd feedback, and each exercise with its next-weight decision. What `workout_audit` returns
/// and a re-rendered finish card shows.
pub fn report(db: &Connection, workout_id: &str) -> Result<Value> {
    if load_workout(db, workout_id, None)?.status != WorkoutStatus::PlanUpdated {
        crate::bail!("AUDIT_NOT_READY", "Finish workout before requesting audit");
    }
    Ok(audited(db, workout_id)?.0)
}

/// The audit (see `report`) and each exercise's decision, for `finish` to save.
fn audited(db: &Connection, workout_id: &str) -> Result<(Value, Vec<(WorkoutExercise, ProgressionDecision)>)> {
    let workout = load_workout(db, workout_id, None)?;
    let rows = exercises(db, workout_id)?;
    let (mut performance, mut progressions, mut decisions) = (vec![], vec![], vec![]);
    let (mut warmups, mut completed_sets, mut progressed) = (0, 0, 0);
    for exercise in rows.iter().cloned() {
        let (decision, working) = decide(db, &exercise)?;
        warmups += logs(db, &exercise.id, Some(SetType::Warmup))?.len();
        completed_sets += working.len();
        progressed += decision.progressed as usize;
        progressions.push(serde_json::to_value(&decision)?);
        performance.push(performed(&exercise, &working));
        decisions.push((exercise, decision));
    }
    let snapshot = &workout.template_snapshot;
    let planned_sets: i64 =
        snapshot["exercises"].as_array().into_iter().flatten().filter_map(|e| e["working_sets"].as_i64()).sum();
    let error = |eta: Option<Ts>| workout.completed_at.zip(eta).map(|(done, eta)| (done - eta).as_secs_f64());
    let count = |status: ExerciseStatus| rows.iter().filter(|e| e.status == status).count();
    let audit = json!({
        "workout_id": workout.id, "template_name": snapshot["name"].as_str().unwrap_or_default(),
        "planned_sets": planned_sets, "completed_sets": completed_sets, "warmup_sets": warmups,
        "actual_duration_seconds": workout.actual_duration_seconds,
        "completed_at": workout.completed_at, "initial_eta": workout.initial_eta, "last_estimated_finish": workout.final_eta,
        "initial_eta_error_seconds": error(workout.initial_eta), "last_eta_error_seconds": error(workout.final_eta),
        "substitutions": count(ExerciseStatus::Substituted), "skips": count(ExerciseStatus::Skipped),
        "equipment_waits": workout.waited_for_equipment_count, "crowd_feedback": workout.crowd_feedback,
        "progressions": progressions, "progression_count": progressed, "exercises": performance,
    });
    Ok((audit, decisions))
}

/// Finish a WORKOUT_COMPLETE workout: save each next weight, PLAN_UPDATED; returns its `report`.
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
        if exercises(db, workout_id)?.iter().any(|e| unresolved(e.status)) {
            crate::bail!(
                "UNRESOLVED_EXERCISES",
                "Resolve deferred/pending exercises or explicitly skip before finishing"
            );
        }
        if workout.status != WorkoutStatus::WorkoutComplete {
            crate::bail!("INVALID_TRANSITION", "Cannot finish workout in {}", workout.status);
        }
        cancel_rest(db, workout_id, now)?;
        workout.completed_at = Some(now);
        workout.last_action_at = Some(now);
        workout.actual_duration_seconds = Some(workout.started_at.map_or(0, |s| (now - s).as_secs()));
        // PLAN_UPDATED means training targets persisted. The summary card is the wrap-up; the agent wakes
        // only to fix upcoming sessions (an agent take was praise with nothing in it, 2026-11-13).
        transition(db, &mut workout, WorkoutStatus::PlanUpdated)?;
        let (audit, decisions) = audited(db, workout_id)?;
        for (exercise, decision) in decisions {
            if exercise.status != ExerciseStatus::Completed || measure(&exercise) != Measure::Reps {
                continue;
            }
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
        if let Some(id) = &workout.planned_session_id
            && let Some(mut planned) = PlannedSession::get(db, id)?
        {
            planned.status = SessionStatus::Completed;
            planned.update(db)?;
        }
        emit(db, "planning.replan_required", now, json!({"workout_id": workout.id, "reason": "workout_completed"}))?;
        Ok(audit)
    })
}
