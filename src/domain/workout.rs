//! Persisted workout state machine (port of gymclaw/services/workout.py). Owner: WP-train.
//!
//! Every mutation runs through `mutate` with a request ID: a retry returns the original result, reuse
//! with a different intent fails. Rest jobs are durable `notification_job` rows; the scheduler and the
//! coach's live countdown fire them. Rows are written back as soon as they change (no autoflush).
use jiff::SignedDuration;
use serde::Serialize;
use serde_json::{Value, json};

use crate::db::models::{ExerciseProgression, ExerciseStatus, JobKind, JobStatus, NotificationJob, PlannedSession, SessionStatus, SetLog, SetType, WorkoutExercise, WorkoutSession, WorkoutStatus};
use crate::db::{Connection, Json, new_id};
use crate::domain::core::{emit, mutate};
use crate::domain::illustrations::{Entry, bodyweight, for_exercise, primary_muscle};
use crate::domain::profile::get_profile;
use crate::domain::set_parser::SetInput;
use crate::domain::templates::{ExerciseSpec, Template, get_template};
use crate::error::{Error, Result};
use crate::time::Ts;

/// Exercise states that still need a decision before the workout can finish.
pub const UNRESOLVED: [ExerciseStatus; 3] = [ExerciseStatus::Pending, ExerciseStatus::Active, ExerciseStatus::Deferred];

pub fn unresolved(status: ExerciseStatus) -> bool {
    UNRESOLVED.contains(&status)
}

fn allowed(from: WorkoutStatus, to: WorkoutStatus) -> bool {
    use WorkoutStatus::*;
    let targets: &[WorkoutStatus] = match from {
        Scheduled => &[Preparing, Arrived],
        Preparing => &[Commuting, Arrived],
        Commuting => &[Arrived],
        Arrived => &[SessionStarted],
        SessionStarted => &[ExerciseActive],
        ExerciseActive => &[SetActive, NextExercise, WorkoutComplete],
        SetActive => &[Resting, ExerciseComplete, NextExercise, ExerciseActive, WorkoutComplete],
        Resting => &[SetActive, NextExercise, ExerciseActive, WorkoutComplete],
        ExerciseComplete => &[NextExercise, Resting, WorkoutComplete],
        NextExercise => &[ExerciseActive, WorkoutComplete],
        WorkoutComplete => &[PostAnalysis],
        PostAnalysis => &[PlanUpdated],
        PlanUpdated => &[],
    };
    targets.contains(&to)
}

/// Move the workout to `to` (saved right away) and record `workout.state_changed`.
pub fn transition(db: &Connection, workout: &mut WorkoutSession, to: WorkoutStatus, now: Ts) -> Result<()> {
    if !allowed(workout.status, to) {
        crate::bail!("INVALID_TRANSITION", "Cannot transition {} to {}", workout.status, to);
    }
    let from = workout.status;
    workout.status = to;
    workout.update(db)?;
    emit(db, "workout.state_changed", now, json!({"workout_id": workout.id, "from": from, "to": to}))?;
    Ok(())
}

/// The workout with an execution snapshot; `now` before its last action fails with TIME_REVERSED.
pub fn load_workout(db: &Connection, workout_id: &str, now: Option<Ts>) -> Result<WorkoutSession> {
    let Some(workout) = WorkoutSession::get(db, workout_id)? else {
        crate::bail!("WORKOUT_NOT_FOUND", "Unknown workout: {workout_id}");
    };
    if workout.template_snapshot.as_object().is_none_or(|o| o.is_empty()) {
        crate::bail!("LEGACY_WORKOUT", "Workout has no execution snapshot; historical records are preserved read-only");
    }
    if let (Some(now), Some(last)) = (now, workout.last_action_at)
        && now < last
    {
        crate::bail!("TIME_REVERSED", "Action timestamp precedes previous workout action");
    }
    Ok(workout)
}

/// Exercises of a workout in position order.
pub fn exercises(db: &Connection, workout_id: &str) -> Result<Vec<WorkoutExercise>> {
    WorkoutExercise::select(db, "WHERE workout_session_id = ?1 ORDER BY position, id", [workout_id])
}

/// Sets of one workout exercise in order, optionally only one set type.
pub fn logs(db: &Connection, exercise_row_id: &str, kind: Option<SetType>) -> Result<Vec<SetLog>> {
    match kind {
        Some(kind) => SetLog::select(db, "WHERE workout_exercise_id = ?1 AND set_type = ?2 ORDER BY logged_at, set_number", rusqlite::params![exercise_row_id, kind]),
        None => SetLog::select(db, "WHERE workout_exercise_id = ?1 ORDER BY logged_at, set_number", [exercise_row_id]),
    }
}

pub fn working_count(db: &Connection, exercise_row_id: &str) -> Result<i64> {
    Ok(logs(db, exercise_row_id, Some(SetType::Working))?.len() as i64)
}

/// The running workout (anything not yet PLAN_UPDATED), latest first.
pub fn active_workout(db: &Connection) -> Result<Option<WorkoutSession>> {
    WorkoutSession::first(db, "WHERE status != 'PLAN_UPDATED' ORDER BY started_at DESC", [])
}

pub fn pending_rest(db: &Connection, workout_id: &str) -> Result<Option<NotificationJob>> {
    NotificationJob::first(db, "WHERE workout_session_id = ?1 AND kind = 'REST' AND status = 'PENDING'", [workout_id])
}

/// Cancel every pending job of the workout (rest timers).
pub fn cancel_rest(db: &Connection, workout_id: &str, now: Ts) -> Result<()> {
    for mut job in NotificationJob::select(db, "WHERE workout_session_id = ?1 AND status = 'PENDING'", [workout_id])? {
        job.status = JobStatus::Cancelled;
        job.handled_at = Some(now);
        job.update(db)?;
        emit(db, "notifications.cancel_required", now, json!({"job_id": job.id, "external_job_id": job.external_job_id}))?;
    }
    Ok(())
}

pub fn active_exercise(db: &Connection, workout_id: &str) -> Result<WorkoutExercise> {
    match exercises(db, workout_id)?.into_iter().find(|e| e.status == ExerciseStatus::Active) {
        Some(exercise) => Ok(exercise),
        None => crate::bail!("NO_ACTIVE_EXERCISE", "No active exercise; resolve deferred equipment first"),
    }
}

/// `config_json` string field of a workout exercise ("name", "guide_id", …).
pub fn config_str<'a>(exercise: &'a WorkoutExercise, key: &str) -> Option<&'a str> {
    exercise.config_json.get(key).and_then(Value::as_str)
}

pub fn name(exercise: &WorkoutExercise) -> &str {
    config_str(exercise, "name").unwrap_or(&exercise.exercise_id)
}

pub fn guide_id(exercise: &WorkoutExercise) -> Option<&str> {
    config_str(exercise, "guide_id")
}

/// Python truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Unknown working weight (first session): a warm-up has no weight to aim for, so it's skipped.
pub fn warmup_pending(db: &Connection, exercise: &WorkoutExercise) -> Result<bool> {
    Ok(truthy(exercise.config_json.get("warmup_required")) && exercise.target_weight > 0.0 && logs(db, &exercise.id, Some(SetType::Warmup))?.is_empty())
}

/// Template warm-up weight, else about half the working weight on the exercise's increment.
pub fn warmup_weight(exercise: &WorkoutExercise) -> f64 {
    let explicit = exercise.config_json.get("warmup_weight");
    if truthy(explicit) {
        return explicit.and_then(Value::as_f64).unwrap_or_default();
    }
    let increment = exercise.config_json.get("increment");
    let step = if truthy(increment) { increment.and_then(Value::as_f64).unwrap_or(2.5) } else { 2.5 };
    (exercise.target_weight / 2.0 / step).round_ties_even() * step
}

fn set_warmup_required(db: &Connection, exercise: &mut WorkoutExercise, required: bool) -> Result<()> {
    if let Some(config) = exercise.config_json.as_object_mut() {
        config.insert("warmup_required".into(), Value::Bool(required));
    }
    exercise.update(db)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Prerequisites (`requires_completed`) are done, also via a substitute.
pub fn compatible(exercise: &WorkoutExercise, rows: &[WorkoutExercise]) -> bool {
    let completed: Vec<&str> = rows
        .iter()
        .filter(|e| e.status == ExerciseStatus::Completed)
        .flat_map(|e| std::iter::once(e.exercise_id.as_str()).chain(e.substituted_from_exercise_id.as_deref()))
        .collect();
    string_list(exercise.config_json.get("requires_completed")).iter().all(|r| completed.contains(&r.as_str()))
}

/// Activate the next pending exercise in plan order; `prefer_muscle` pulls same-muscle work forward.
pub fn choose_next(db: &Connection, workout: &mut WorkoutSession, now: Ts, prefer_muscle: Option<&str>) -> Result<()> {
    let rows = exercises(db, &workout.id)?;
    let ready: Vec<&WorkoutExercise> = rows.iter().filter(|e| e.status == ExerciseStatus::Pending && compatible(e, &rows)).collect();
    let same = ready.iter().find(|e| prefer_muscle.is_some() && primary_muscle(guide_id(e)).as_deref() == prefer_muscle);
    if let Some(next) = same.or(ready.first()) {
        let mut next = (*next).clone();
        next.status = ExerciseStatus::Active;
        next.update(db)?;
        transition(db, workout, WorkoutStatus::ExerciseActive, now)?;
        transition(db, workout, WorkoutStatus::SetActive, now)?;
    } else if rows.iter().any(|e| unresolved(e.status)) {
        // No unresolved exercise is silently discarded. Agent must retry/substitute/skip.
        transition(db, workout, WorkoutStatus::ExerciseActive, now)?;
    } else {
        transition(db, workout, WorkoutStatus::WorkoutComplete, now)?;
    }
    Ok(())
}

/// Average seconds per set from history, else `default_seconds`.
pub fn historical_set_duration(db: &Connection, default_seconds: f64) -> Result<f64> {
    let history = SetLog::select(
        db,
        "WHERE workout_exercise_id IN (SELECT e.id FROM workout_exercise e JOIN workout_session w ON w.id = e.workout_session_id WHERE w.status = 'PLAN_UPDATED') \
         ORDER BY workout_exercise_id, logged_at",
        [],
    )?;
    let durations: Vec<f64> = history
        .windows(2)
        .filter(|pair| pair[0].workout_exercise_id == pair[1].workout_exercise_id)
        .map(|pair| (pair[1].logged_at - pair[0].rest_due_at.unwrap_or(pair[0].logged_at)).as_secs_f64())
        .filter(|s| (10.0..=180.0).contains(s))
        .collect();
    Ok(if durations.is_empty() { default_seconds } else { durations.iter().sum::<f64>() / durations.len() as f64 })
}

fn snapshot_number(workout: &WorkoutSession, key: &str) -> f64 {
    workout.template_snapshot.get(key).and_then(Value::as_f64).unwrap_or_default()
}

/// Seconds as a duration, rounded to microseconds like Python's timedelta.
pub fn seconds(value: f64) -> SignedDuration {
    SignedDuration::from_micros((value * 1e6).round() as i64)
}

pub fn estimate_finish(db: &Connection, workout: &WorkoutSession, now: Ts) -> Result<Ts> {
    let rows: Vec<WorkoutExercise> = exercises(db, &workout.id)?.into_iter().filter(|e| unresolved(e.status)).collect();
    if rows.is_empty() {
        return Ok(workout.completed_at.or(workout.last_action_at).unwrap_or(now));
    }
    let duration = historical_set_duration(db, snapshot_number(workout, "set_duration_seconds"))?;
    let mut total = pending_rest(db, &workout.id)?.map_or(0.0, |job| (job.due_at - now).as_secs_f64().max(0.0));
    for (index, exercise) in rows.iter().enumerate() {
        let remaining = (exercise.planned_working_sets - working_count(db, &exercise.id)?).max(0) as f64;
        total += remaining * duration;
        if warmup_pending(db, exercise)? {
            total += duration;
        }
        total += (remaining - 1.0).max(0.0) * exercise.rest_seconds as f64;
        if index < rows.len() - 1 {
            total += exercise.rest_seconds as f64 + snapshot_number(workout, "transition_seconds");
        }
    }
    Ok(now + seconds(total))
}

/// Python's `{:g}`: 80 → "80", 22.5 → "22.5".
pub fn g(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let exponent = value.abs().log10().floor() as i32;
    if !(-4..6).contains(&exponent) {
        let text = format!("{value:.5e}");
        let (mantissa, exp) = text.split_once('e').unwrap_or((&text, "0"));
        let mantissa = if mantissa.contains('.') { mantissa.trim_end_matches('0').trim_end_matches('.') } else { mantissa };
        let exp: i32 = exp.parse().unwrap_or(0);
        return format!("{mantissa}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let text = format!("{value:.*}", (5 - exponent).max(0) as usize);
    if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text }
}

/// '8–10 × 80 kg' (reps first, how the owner says it); unknown weight (0) shows reps only.
pub fn format_target(rep_min: i64, rep_max: i64, target_weight: f64) -> String {
    let reps = if rep_min == rep_max { rep_min.to_string() } else { format!("{rep_min}–{rep_max}") };
    if target_weight != 0.0 { format!("{reps} × {} kg", g(target_weight)) } else { format!("{reps} reps") }
}

/// Latest working set for this exercise: this workout first, else any earlier workout.
pub fn last_working_set(db: &Connection, exercise: &WorkoutExercise) -> Result<Option<SetLog>> {
    if let Some(own) = logs(db, &exercise.id, Some(SetType::Working))?.pop() {
        return Ok(Some(own));
    }
    crate::domain::preview::last_set(db, &exercise.exercise_id)
}

/// The next working set as (weight, reps): this workout's last set, else the prescribed target, else
/// last time's set. What the coach card offers and what a typed set is read against.
pub fn next_working_set(db: &Connection, exercise: &WorkoutExercise) -> Result<Option<(f64, i64)>> {
    if let Some(own) = logs(db, &exercise.id, Some(SetType::Working))?.pop() {
        return Ok(Some((own.weight, own.reps)));
    }
    if exercise.target_weight != 0.0 {
        return Ok(Some((exercise.target_weight, exercise.rep_min)));
    }
    Ok(crate::domain::preview::last_set(db, &exercise.exercise_id)?.map(|s| (s.weight, s.reps)))
}

/// Weight the owner most likely means when a typed set's order is unclear: the one the card offers.
pub fn expected_weight(db: &Connection, workout_id: &str) -> Result<Option<f64>> {
    let workout = load_workout(db, workout_id, None)?;
    let Some(active) = exercises(db, &workout.id)?.into_iter().find(|e| e.status == ExerciseStatus::Active) else { return Ok(None) };
    Ok(next_working_set(db, &active)?.map(|(weight, _)| weight))
}

#[derive(Clone, Debug, Serialize)]
pub struct LastSet {
    pub weight: f64,
    pub reps: i64,
}

/// The exercise the owner should do now, as `current` reports it.
#[derive(Clone, Debug, Serialize)]
pub struct Active {
    pub illustration: Option<Entry>,
    pub id: String,
    pub exercise_id: String,
    pub name: String,
    pub primary_muscle: Option<String>,
    /// Nothing logged yet on it (warm-up included).
    pub new_exercise: bool,
    pub last_set: Option<LastSet>,
    pub set_type: SetType,
    pub set_number: i64,
    pub working_sets: i64,
    pub bodyweight: bool,
    pub target_weight: f64,
    pub rep_min: i64,
    pub rep_max: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestJob {
    pub id: String,
    pub due_at: Ts,
    pub external_job_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueueItem {
    pub id: String,
    pub exercise_id: String,
    pub status: ExerciseStatus,
    pub remaining_sets: i64,
    pub reason: Option<String>,
}

/// Workout state as the old `workout current` JSON.
#[derive(Clone, Debug, Serialize)]
pub struct State {
    pub workout_id: String,
    pub status: WorkoutStatus,
    pub active_exercise: Option<Active>,
    pub rest_job: Option<RestJob>,
    pub eta: Ts,
    pub initial_eta: Option<Ts>,
    pub seconds_vs_initial_eta: Option<f64>,
    pub queue: Vec<QueueItem>,
    pub instruction: String,
}

/// Typed workout state; `current` is its JSON.
pub fn state(db: &Connection, workout_id: &str, now: Ts) -> Result<State> {
    let workout = load_workout(db, workout_id, None)?;
    let rows = exercises(db, &workout.id)?;
    let rest = pending_rest(db, &workout.id)?;
    let mut instruction = "Workout complete. Finish to save audit.".to_string();
    let mut active_data = None;
    if let Some(active) = rows.iter().find(|e| e.status == ExerciseStatus::Active) {
        let is_warmup = warmup_pending(db, active)?;
        let last = last_working_set(db, active)?;
        let guide = guide_id(active);
        let warmup_reps = active.config_json.get("warmup_reps").and_then(Value::as_i64).unwrap_or(8);
        let data = Active {
            illustration: for_exercise(name(active), guide),
            id: active.id.clone(),
            exercise_id: active.exercise_id.clone(),
            name: name(active).to_string(),
            primary_muscle: primary_muscle(guide),
            new_exercise: logs(db, &active.id, None)?.is_empty(),
            last_set: last.map(|l| LastSet { weight: l.weight, reps: l.reps }),
            set_type: if is_warmup { SetType::Warmup } else { SetType::Working },
            set_number: if is_warmup { 1 } else { working_count(db, &active.id)? + 1 },
            working_sets: active.planned_working_sets,
            bodyweight: bodyweight(guide),
            target_weight: if is_warmup { warmup_weight(active) } else { active.target_weight },
            rep_min: if is_warmup { warmup_reps } else { active.rep_min },
            rep_max: if is_warmup { warmup_reps } else { active.rep_max },
        };
        let step = if is_warmup { "warm-up".to_string() } else { format!("set {}/{}", data.set_number, active.planned_working_sets) };
        instruction = format!("{} · {step} · {}", data.name, format_target(data.rep_min, data.rep_max, data.target_weight));
        active_data = Some(data);
    } else if rows.iter().any(|e| unresolved(e.status)) {
        instruction = "Deferred equipment unresolved. Retry, substitute, or explicitly skip.".into();
    }
    if rest.is_some() {
        instruction = "Rest.".into();
    }
    if workout.status == WorkoutStatus::PlanUpdated {
        instruction = "Workout audit saved.".into();
    }
    let eta = estimate_finish(db, &workout, now)?;
    let mut queue = vec![];
    for e in &rows {
        let remaining_sets = (e.planned_working_sets - working_count(db, &e.id)?).max(0);
        queue.push(QueueItem { id: e.id.clone(), exercise_id: e.exercise_id.clone(), status: e.status, remaining_sets, reason: e.deferred_reason.clone() });
    }
    Ok(State {
        workout_id: workout.id.clone(),
        status: workout.status,
        active_exercise: active_data,
        rest_job: rest.map(|r| RestJob { id: r.id, due_at: r.due_at, external_job_id: r.external_job_id }),
        eta,
        initial_eta: workout.initial_eta,
        seconds_vs_initial_eta: workout.initial_eta.map(|initial| (eta - initial).as_secs_f64()),
        queue,
        instruction,
    })
}

/// Workout state as the old `workout current` JSON (status, active_exercise, queue, rest_job, …).
pub fn current(db: &Connection, workout_id: &str, now: Ts) -> Result<Value> {
    Ok(serde_json::to_value(state(db, workout_id, now)?)?)
}

/// Record the action time and a fresh ETA; returns `current`.
pub fn touch(db: &Connection, workout: &mut WorkoutSession, now: Ts) -> Result<Value> {
    workout.last_action_at = Some(now);
    workout.update(db)?;
    workout.final_eta = Some(estimate_finish(db, workout, now)?);
    workout.update(db)?;
    current(db, &workout.id, now)
}

/// Insert a workout exercise from a template spec. Learned progression wins over the template weight.
pub fn make_exercise(db: &Connection, workout_id: &str, spec: &ExerciseSpec, position: i64, warmup: bool, working_sets: Option<i64>) -> Result<WorkoutExercise> {
    let profile = get_profile(db)?;
    let progression = ExerciseProgression::get(db, &spec.id)?;
    let mut config = serde_json::to_value(spec)?;
    config["warmup_required"] = Value::Bool(warmup);
    let row = WorkoutExercise {
        id: new_id(),
        workout_session_id: workout_id.into(),
        exercise_id: spec.id.clone(),
        position,
        status: ExerciseStatus::Pending,
        // Python's `working_sets or spec.working_sets`: 0 falls back too.
        planned_working_sets: working_sets.filter(|n| *n != 0).unwrap_or(spec.working_sets),
        rep_min: spec.rep_min,
        rep_max: spec.rep_max,
        target_weight: progression.map_or(spec.target_weight, |p| p.next_weight),
        rest_seconds: profile.rest_seconds(),
        substituted_from_exercise_id: None,
        deferred_reason: None,
        config_json: Json(config),
    };
    row.insert(db)?;
    Ok(row)
}

/// A workout can start on it: upcoming, or missed earlier the same local day (the owner showed up late).
/// An older missed session is history; a make-up workout gets its own session.
pub fn startable(db: &Connection, session: &PlannedSession, now: Ts) -> Result<bool> {
    let zone = get_profile(db)?.tz();
    Ok(session.status.is_open() || (session.status == SessionStatus::Missed && session.planned_start_at.local(&zone).date() == now.local(&zone).date()))
}

/// The startable planned session on the owner's local today closest to now, even if its slot already passed
/// or was already marked missed (the owner showed up late).
pub fn todays_session(db: &Connection, now: Ts) -> Result<Option<PlannedSession>> {
    let zone = get_profile(db)?.tz();
    let today = now.local(&zone).date();
    let rows = PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED', 'MISSED') AND workout_template_id IS NOT NULL", [])?;
    Ok(rows.into_iter().filter(|s| s.planned_start_at.local(&zone).date() == today).min_by_key(|s| (s.planned_start_at - now).abs()))
}

/// An availability block (travel, sick) is in effect at `now`.
fn blocked_at(db: &Connection, now: Ts) -> Result<bool> {
    Ok(!crate::domain::availability::blocks(db, now, now + SignedDuration::from_micros(1))?.is_empty())
}

/// Start a workout. Without `template_id` it starts the given planned session, else today's one.
pub fn start(db: &Connection, template_id: Option<&str>, planned_session_id: Option<&str>, by_button: bool, now: Ts, request_id: &str) -> Result<Value> {
    let intent = json!({"template_id": template_id, "planned_session_id": planned_session_id});
    let (template_id, planned_session_id) = match template_id {
        Some(t) => (t.to_string(), planned_session_id.map(String::from)),
        None => {
            let planned = match planned_session_id {
                Some(id) => PlannedSession::get(db, id)?,
                None => todays_session(db, now)?,
            };
            match planned {
                Some(PlannedSession { workout_template_id: Some(t), id, .. }) => (t, Some(id)),
                _ => crate::bail!("NO_PLANNED_SESSION", "No planned workout today; pass template_id"),
            }
        }
    };
    mutate(db, "workout.started", request_id, intent, now, || {
        if blocked_at(db, now)? {
            crate::bail!("WORKOUT_UNAVAILABLE", "User availability override active; remove/change it explicitly before starting");
        }
        if active_workout(db)?.is_some() {
            crate::bail!("WORKOUT_ALREADY_ACTIVE", "Finish existing workout before starting another");
        }
        let mut template = get_template(db, &template_id)?;
        let mut planned = match &planned_session_id {
            Some(id) => match PlannedSession::get(db, id)? {
                Some(p) if startable(db, &p, now)? => Some(p),
                _ => crate::bail!("INVALID_PLAN", "Planned session is missing or no longer startable"),
            },
            None => None,
        };
        let mut allocation: Option<Value> = None;
        if let Some(planned) = planned.as_mut() {
            // Always the current template: the session's stored copy may predate edits and swaps.
            template.set_duration_seconds = historical_set_duration(db, template.set_duration_seconds as f64)?.ceil() as i64;
            let budget = (planned.planned_end_at - planned.planned_start_at).as_secs();
            let mut plan = crate::domain::compression::compress_template(&template, &get_profile(db)?, budget)?;
            if let (Some(plan), Some(stored)) = (plan.as_object_mut(), planned.workout_plan_json.as_object()) {
                plan.extend(stored.iter().filter(|(k, _)| k.starts_with("crowd_")).map(|(k, v)| (k.clone(), v.clone())));
            }
            planned.workout_plan_json = Json(plan.clone());
            planned.update(db)?;
            allocation = Some(plan);
        }
        let mut workout = WorkoutSession {
            id: new_id(),
            planned_session_id: planned_session_id.clone(),
            template_id: Some(template_id.clone()),
            template_snapshot: Json(serde_json::to_value(&template)?),
            last_action_at: Some(now),
            started_at: Some(now),
            arrived_at: Some(now),
            started_by_button: by_button,
            completed_at: None,
            status: WorkoutStatus::Scheduled,
            initial_eta: None,
            final_eta: None,
            actual_duration_seconds: None,
            crowd_feedback: None,
            waited_for_equipment_count: 0,
            notes: None,
        };
        workout.insert(db)?;
        let first_primary = template.exercises.iter().find(|e| e.primary).map(|e| e.id.clone());
        for (index, spec) in template.exercises.iter().enumerate() {
            let mut row = make_exercise(db, &workout.id, spec, index as i64, first_primary.as_deref() == Some(spec.id.as_str()), None)?;
            if let Some(plan) = &allocation {
                row.planned_working_sets = plan["sets"][&spec.id].as_i64().unwrap_or(0);
                if row.planned_working_sets == 0 {
                    row.status = ExerciseStatus::Skipped;
                    row.deferred_reason = Some("calendar_time_budget".into());
                }
                row.update(db)?;
            }
        }
        // start is explicit arrival intent, not fabricated preparation/travel history.
        transition(db, &mut workout, WorkoutStatus::Arrived, now)?;
        transition(db, &mut workout, WorkoutStatus::SessionStarted, now)?;
        choose_next(db, &mut workout, now, None)?;
        if let Some(mut planned) = planned {
            crate::domain::notifications::cancel_session_jobs(db, &planned.id, now)?;
            planned.status = SessionStatus::Started;
            planned.workout_template_id = Some(template_id.clone());
            planned.update(db)?;
        }
        workout.initial_eta = Some(estimate_finish(db, &workout, now)?);
        touch(db, &mut workout, now)
    })
}

/// Log one set on the active exercise. Rest follows every working set except the very last one.
pub fn log_set(db: &Connection, workout_id: &str, value: &SetInput, now: Ts, request_id: &str) -> Result<Value> {
    value.validate()?;
    let intent = json!({"workout_id": workout_id, "set": value});
    mutate(db, "workout.set_logged", request_id, intent, now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        if !matches!(workout.status, WorkoutStatus::SetActive | WorkoutStatus::Resting) {
            crate::bail!("INVALID_TRANSITION", "Cannot log set in {}", workout.status);
        }
        let mut active = active_exercise(db, workout_id)?;
        let needs_warmup = warmup_pending(db, &active)?;
        if value.set_type == SetType::Working && needs_warmup {
            // Going straight to working sets skips the warm-up; it is a suggestion, not a gate.
            set_warmup_required(db, &mut active, false)?;
        }
        if value.set_type == SetType::Warmup && !needs_warmup {
            crate::bail!("UNEXPECTED_WARMUP", "No warm-up is pending for this exercise");
        }
        cancel_rest(db, workout_id, now)?;
        if workout.status == WorkoutStatus::Resting {
            transition(db, &mut workout, WorkoutStatus::SetActive, now)?;
        }
        let mut row = SetLog {
            id: new_id(),
            workout_exercise_id: active.id.clone(),
            set_number: logs(db, &active.id, Some(value.set_type))?.len() as i64 + 1,
            set_type: value.set_type,
            weight: value.weight,
            reps: value.reps,
            rir_optional: value.rir,
            logged_at: now,
            rest_started_at: None,
            rest_due_at: None,
        };
        row.insert(db)?;
        if value.set_type == SetType::Working {
            let completed = working_count(db, &active.id)? >= active.planned_working_sets;
            if completed {
                active.status = ExerciseStatus::Completed;
                active.update(db)?;
                transition(db, &mut workout, WorkoutStatus::ExerciseComplete, now)?;
                transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
                choose_next(db, &mut workout, now, None)?;
            }
            // Rest after every working set, also before the next exercise (with the finished one's rest
            // time). None after the last exercise, or when only deferred exercises are left.
            if (!completed && workout.status != WorkoutStatus::WorkoutComplete) || workout.status == WorkoutStatus::SetActive {
                let due = now + SignedDuration::from_secs(active.rest_seconds);
                row.rest_started_at = Some(now);
                row.rest_due_at = Some(due);
                row.update(db)?;
                let job = NotificationJob {
                    id: new_id(),
                    kind: JobKind::Rest,
                    workout_session_id: Some(workout.id.clone()),
                    planned_session_id: None,
                    set_log_id: Some(row.id.clone()),
                    due_at: due,
                    status: JobStatus::Pending,
                    payload_json: Json(json!({"workout_id": workout.id})),
                    external_job_id: None,
                    handled_at: None,
                };
                job.insert(db)?;
                emit(db, "notifications.schedule_required", now, json!({"job_id": job.id, "due_at": due, "kind": "REST"}))?;
                // Deferred-only queue stays EXERCISE_ACTIVE; rest is still durable.
                if workout.status == WorkoutStatus::SetActive {
                    transition(db, &mut workout, WorkoutStatus::Resting, now)?;
                }
            }
        }
        let mut result = touch(db, &mut workout, now)?;
        result["logged_set"] = json!({"id": row.id, "weight": row.weight, "reps": row.reps, "set_type": row.set_type, "set_number": row.set_number});
        Ok(result)
    })
}

/// A due REST job: rest is over. Mutation result envelope; `user_message_hint` is the message to send.
pub fn rest_complete(db: &Connection, job_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.rest_completed", request_id, json!({"job_id": job_id}), now, || {
        let mut job = match NotificationJob::get(db, job_id)? {
            Some(job) if job.kind == JobKind::Rest => job,
            _ => crate::bail!("JOB_NOT_FOUND", "Unknown rest job"),
        };
        let mut workout = load_workout(db, job.workout_session_id.as_deref().unwrap_or_default(), Some(now))?;
        if job.status != JobStatus::Pending {
            return Ok(json!({"job_id": job.id, "stale": true, "status": job.status}));
        }
        if now < job.due_at {
            crate::bail!("REST_NOT_DUE", "Rest timer has not reached due_at");
        }
        job.status = JobStatus::Fired;
        job.handled_at = Some(now);
        job.update(db)?;
        if workout.status == WorkoutStatus::Resting {
            transition(db, &mut workout, WorkoutStatus::SetActive, now)?;
        }
        let mut result = touch(db, &mut workout, now)?;
        result["instruction"] = Value::String(format!("⏱ Rest over · {}", result["instruction"].as_str().unwrap_or_default()));
        Ok(result)
    })
}

pub fn skip_warmup(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.warmup_skipped", request_id, json!({"workout_id": workout_id}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        let mut active = active_exercise(db, workout_id)?;
        if !warmup_pending(db, &active)? {
            crate::bail!("UNEXPECTED_WARMUP", "No warm-up is pending for this exercise");
        }
        set_warmup_required(db, &mut active, false)?;
        touch(db, &mut workout, now)
    })
}

/// Owner is ready before the timer: cancel the rest and go to the next set.
pub fn skip_rest(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.rest_skipped", request_id, json!({"workout_id": workout_id}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        if workout.status != WorkoutStatus::Resting {
            crate::bail!("NOT_RESTING", "No rest to skip");
        }
        cancel_rest(db, workout_id, now)?;
        transition(db, &mut workout, WorkoutStatus::SetActive, now)?;
        touch(db, &mut workout, now)
    })
}

/// Owner wants a different rest between sets: applies to every exercise not done yet in this workout.
/// A countdown already running keeps its time; the next one uses the new rest.
pub fn set_rest(db: &Connection, workout_id: &str, seconds: i64, now: Ts, request_id: &str) -> Result<Value> {
    if !(10..=600).contains(&seconds) {
        crate::bail!("INVALID_REST", "Rest must be 10–600 seconds");
    }
    mutate(db, "workout.rest_changed", request_id, json!({"workout_id": workout_id, "seconds": seconds}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        for mut row in exercises(db, workout_id)? {
            if unresolved(row.status) {
                row.rest_seconds = seconds;
                row.update(db)?;
            }
        }
        touch(db, &mut workout, now)
    })
}

/// Shorten the running rest countdown. Cutting past zero ends the rest (same as skipping it).
/// The fallback ping keeps its old time; it finds the job already closed and stays silent.
pub fn cut_rest(db: &Connection, workout_id: &str, seconds: i64, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.rest_cut", request_id, json!({"workout_id": workout_id, "seconds": seconds}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        let job = pending_rest(db, workout_id)?;
        let Some(mut job) = job.filter(|_| workout.status == WorkoutStatus::Resting) else {
            crate::bail!("NOT_RESTING", "No rest to shorten");
        };
        let due = job.due_at - SignedDuration::from_secs(seconds);
        if due <= now {
            cancel_rest(db, workout_id, now)?;
            transition(db, &mut workout, WorkoutStatus::SetActive, now)?;
        } else {
            job.due_at = due;
            job.update(db)?;
            if let Some(mut set) = SetLog::get(db, job.set_log_id.as_deref().unwrap_or_default())? {
                set.rest_due_at = Some(due);
                set.update(db)?;
            }
        }
        touch(db, &mut workout, now)
    })
}

/// Parse a stored template snapshot (the workout's copy of its template).
pub fn snapshot(workout: &WorkoutSession) -> Result<Template> {
    serde_json::from_value(workout.template_snapshot.0.clone()).map_err(|e| Error::domain("INVALID_TEMPLATE", format!("Workout snapshot: {e}")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::set_parser::parse_set;
    use crate::domain::templates::import_template;

    pub fn db() -> Connection {
        let path = std::env::temp_dir().join(format!("gymclaw-test-{}.db", new_id()));
        let mut db = crate::db::open(&path).unwrap();
        crate::db::migrate(&mut db).unwrap();
        db
    }

    pub fn at(seconds: i64) -> Ts {
        Ts::parse("2026-10-12T18:00:00+00:00", None).unwrap() + SignedDuration::from_secs(seconds)
    }

    pub fn spec(value: Value) -> ExerciseSpec {
        serde_json::from_value(value).unwrap()
    }

    fn set(weight: f64, reps: i64, set_type: SetType) -> SetInput {
        SetInput { weight, reps, set_type, rir: None }
    }

    fn short(db: &Connection) -> String {
        crate::domain::profile::update_profile(db, &serde_json::from_value(json!({"default_accessory_rest_seconds": 100})).unwrap()).unwrap();
        let bench = spec(json!({"id": "bench", "name": "Bench", "role": "press", "primary": true, "working_sets": 2, "target_weight": 80, "warmup_weight": 50, "rest_seconds": 100}));
        import_template(db, &Template { id: "short".into(), name: "Short".into(), exercises: vec![bench], alternatives: vec![], set_duration_seconds: 40, transition_seconds: 60, source: "user".into() }).unwrap();
        start(db, Some("short"), None, false, at(0), "start").unwrap()["data"]["workout_id"].as_str().unwrap().to_string()
    }

    #[test]
    fn warmup_rest_and_early_set_cancel() {
        let db = db();
        let id = short(&db);
        let initial = state(&db, &id, at(0)).unwrap();
        assert_eq!(initial.active_exercise.unwrap().set_type, SetType::Warmup);
        assert_eq!(initial.eta, at(220));
        log_set(&db, &id, &parse_set("8x50", SetType::Warmup, None, false).unwrap(), at(30), "warmup").unwrap();
        let first = log_set(&db, &id, &parse_set("9x80", SetType::Working, None, false).unwrap(), at(60), "set-1").unwrap();
        let job_id = first["data"]["rest_job"]["id"].as_str().unwrap().to_string();
        assert_eq!(first["data"]["status"], "RESTING");
        assert_eq!(first["data"]["active_exercise"]["set_number"], 2);
        // A retry returns the first result, not a second set.
        assert_eq!(log_set(&db, &id, &set(80.0, 9, SetType::Working), at(61), "set-1").unwrap(), first);
        let done = log_set(&db, &id, &set(80.0, 10, SetType::Working), at(80), "set-2").unwrap();
        assert_eq!(done["data"]["status"], "WORKOUT_COMPLETE");
        assert!(done["data"]["rest_job"].is_null());
        assert_eq!(NotificationJob::get(&db, &job_id).unwrap().unwrap().status, JobStatus::Cancelled);
        assert_eq!(rest_complete(&db, &job_id, at(160), "stale").unwrap()["data"]["stale"], true);
    }

    #[test]
    fn rest_is_due_exactly() {
        let db = db();
        let id = short(&db);
        log_set(&db, &id, &set(80.0, 9, SetType::Working), at(0), "one").unwrap();
        let job = pending_rest(&db, &id).unwrap().unwrap();
        assert_eq!(job.due_at, at(100));
        assert_eq!(rest_complete(&db, &job.id, at(99), "early").unwrap_err().code(), "REST_NOT_DUE");
        let done = rest_complete(&db, &job.id, at(100), "due").unwrap();
        assert_eq!(done["data"]["status"], "SET_ACTIVE");
        assert!(done["user_message_hint"].as_str().unwrap().starts_with("⏱ Rest over · Bench"));
        assert_eq!(NotificationJob::get(&db, &job.id).unwrap().unwrap().status, JobStatus::Fired);
    }

    #[test]
    fn working_set_skips_pending_warmup_and_time_cannot_reverse() {
        let db = db();
        let id = short(&db);
        assert_eq!(log_set(&db, &id, &set(50.0, 8, SetType::Warmup), at(-1), "past").unwrap_err().code(), "TIME_REVERSED");
        let result = log_set(&db, &id, &set(80.0, 9, SetType::Working), at(0), "straight-in").unwrap();
        let active = &result["data"]["active_exercise"];
        assert_eq!((active["set_type"].as_str(), active["set_number"].as_i64()), (Some("WORKING"), Some(2)));
        assert_eq!(active["last_set"], json!({"weight": 80.0, "reps": 9}));
    }

    #[test]
    fn eta_learns_from_completed_set_timing() {
        let db = db();
        let id = short(&db);
        log_set(&db, &id, &set(50.0, 8, SetType::Warmup), at(30), "warmup").unwrap();
        log_set(&db, &id, &set(80.0, 10, SetType::Working), at(90), "first").unwrap();
        log_set(&db, &id, &set(80.0, 10, SetType::Working), at(240), "second").unwrap();
        crate::domain::audit::finish(&db, &id, at(250), "finish").unwrap();
        let later = at(2 * 86400);
        let result = start(&db, Some("short"), None, false, later, "next").unwrap();
        // Observed durations: 60 s after the warm-up and 50 s after the planned rest → mean 55 s.
        assert_eq!(result["data"]["eta"], json!(Ts(later.0) + SignedDuration::from_secs(3 * 55 + 100)));
    }

    #[test]
    fn python_g_format() {
        assert_eq!([g(80.0), g(22.5), g(0.5), g(102.25), g(-1.2)], ["80", "22.5", "0.5", "102.25", "-1.2"]);
    }

    #[test]
    fn pose_follows_active_exercise_not_previous_set() {
        let db = db();
        let press = spec(json!({"id": "press", "name": "Owner press", "role": "press", "guide_id": "bench-press", "working_sets": 1, "target_weight": 80}));
        let row = spec(json!({"id": "row", "name": "Owner row", "role": "pull", "guide_id": "seated-row", "working_sets": 1, "target_weight": 60}));
        import_template(&db, &Template { id: "poses".into(), name: "Poses".into(), exercises: vec![press, row], alternatives: vec![], set_duration_seconds: 40, transition_seconds: 60, source: "user".into() }).unwrap();
        let data = start(&db, Some("poses"), None, false, at(0), "start").unwrap()["data"].clone();
        assert_eq!(data["active_exercise"]["illustration"]["guide_id"], "bench-press");
        let after = log_set(&db, data["workout_id"].as_str().unwrap(), &set(80.0, 9, SetType::Working), at(40), "set").unwrap()["data"].clone();
        assert_eq!(after["active_exercise"]["illustration"]["guide_id"], "seated-row");
        assert_eq!(after["active_exercise"]["target_weight"], 60.0);
    }

    #[test]
    fn rest_complete_retry_returns_the_first_result() {
        let db = db();
        let id = short(&db);
        log_set(&db, &id, &set(50.0, 8, SetType::Warmup), at(0), "warmup").unwrap();
        let job = log_set(&db, &id, &set(80.0, 9, SetType::Working), at(0), "one").unwrap()["data"]["rest_job"]["id"].as_str().unwrap().to_string();
        let due = rest_complete(&db, &job, at(100), "due").unwrap();
        assert!(due["data"]["rest_job"].is_null() && due["user_message_hint"].as_str().unwrap().contains("Bench"));
        assert_eq!(rest_complete(&db, &job, at(101), "due").unwrap(), due);
        let events: i64 = db.query_row("SELECT count(*) FROM agent_event WHERE type = 'workout.rest_completed'", [], |r| r.get(0)).unwrap();
        assert_eq!(events, 1);
    }

    #[test]
    fn guardrails_rollback_and_id_conflict() {
        let db = db();
        let id = short(&db);
        type Action<'a> = &'a dyn Fn(&Connection) -> Result<Value>;
        let actions: [Action; 4] = [
            &|db| start(db, Some("short"), None, false, at(0), "duplicate-active"),
            &|db| start(db, Some("unknown"), None, false, at(0), "start"),
            &|db| transition(db, &mut load_workout(db, &id, None)?, WorkoutStatus::PlanUpdated, at(0)).map(|_| Value::Null),
            &|db| log_set(db, &id, &set(50.0, 8, SetType::Warmup), at(-1), "past"),
        ];
        for action in actions {
            let tx = db.unchecked_transaction().unwrap();
            assert!(action(&tx).is_err());
            drop(tx); // rolled back, like the tool layer does on error
        }
        assert!(SetLog::select(&db, "", []).unwrap().is_empty());
        assert_eq!(load_workout(&db, &id, None).unwrap().status, WorkoutStatus::SetActive);
    }

    #[test]
    fn template_edits_do_not_mutate_active_workout() {
        let db = db();
        let id = short(&db);
        let mut template = get_template(&db, "short").unwrap();
        template.exercises[0].working_sets = 1;
        template.exercises[0].warmup_weight = Some(20.0);
        import_template(&db, &template).unwrap();
        let active = current(&db, &id, at(0)).unwrap()["active_exercise"].clone();
        assert_eq!((active["working_sets"].as_i64(), active["target_weight"].as_f64()), (Some(2), Some(50.0)));
    }

    #[test]
    fn only_first_primary_gets_warmup() {
        let db = db();
        let a = spec(json!({"id": "a", "name": "A", "role": "press", "primary": true, "working_sets": 1, "target_weight": 10, "warmup_weight": 5}));
        let b = spec(json!({"id": "b", "name": "B", "role": "pull", "primary": true, "working_sets": 1, "target_weight": 10}));
        import_template(&db, &Template { id: "two".into(), name: "Two".into(), exercises: vec![a, b], alternatives: vec![], set_duration_seconds: 40, transition_seconds: 60, source: "user".into() }).unwrap();
        let id = start(&db, Some("two"), None, false, at(0), "two").unwrap()["data"]["workout_id"].as_str().unwrap().to_string();
        log_set(&db, &id, &set(5.0, 8, SetType::Warmup), at(0), "warmup").unwrap();
        let result = log_set(&db, &id, &set(10.0, 9, SetType::Working), at(0), "working").unwrap()["data"].clone();
        assert_eq!((result["active_exercise"]["exercise_id"].as_str(), result["active_exercise"]["set_type"].as_str()), (Some("b"), Some("WORKING")));
        assert_eq!(exercises(&db, &id).unwrap().len(), 2);
    }
}
