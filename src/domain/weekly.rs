//! Rolling maintenance, weekly audit and the Sunday briefing (port of weekly.py). Owner: lead (planning).
use std::collections::{BTreeSet, HashMap};

use jiff::SignedDuration;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::db::models::{
    AgentEvent, CalendarEventSnapshot, CalendarSyncState, CrowdFeedback, LearnedPreference, OnboardingState, PlannedSession, SessionStatus, SetLog, SetType, WorkoutExercise, WorkoutSession,
    WorkoutStatus, WorkoutTemplate,
};
use crate::db::{Connection, Json, new_id};
use crate::domain::availability::session_blocked;
use crate::domain::calendar::{allocation, get_week, week_of};
use crate::domain::calendar_writes::queue_session_write;
use crate::domain::core::emit;
use crate::domain::crowd::{CrowdModel, feel};
use crate::domain::profile::get_profile;
use crate::domain::replanning::{commit_upcoming, replan_weeks, retire_session};
use crate::domain::scheduling::schedule_week;
use crate::domain::templates::get_template;
use crate::error::Result;
use crate::time::Ts;

const NO_SHOW_AFTER: SignedDuration = SignedDuration::from_mins(30);
const CROWD_RECHECK: SignedDuration = SignedDuration::from_mins(30);
const STALE_WORKOUT: SignedDuration = SignedDuration::from_hours(3);
const DAY: SignedDuration = SignedDuration::from_hours(24);

fn days(n: i64) -> jiff::Span {
    jiff::Span::new().days(n)
}

fn median(values: &[i64]) -> Option<Value> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    Some(if n % 2 == 1 { json!(sorted[n / 2]) } else { json!((sorted[n / 2 - 1] + sorted[n / 2]) as f64 / 2.0) })
}

fn demo(value: &Value) -> bool {
    value.get("demo").and_then(Value::as_bool).unwrap_or(false)
}

/// What happened in a week: completed workouts vs target, sets, missed/skipped, durations. With
/// `learn`, updates the typical session duration from the last 90 days.
pub fn audit_week(db: &Connection, week_start: Date, now: Ts, learn: bool) -> Result<Value> {
    if !crate::domain::planning::monday(week_start) {
        crate::bail!("INVALID_WEEK", "week_start must be Monday");
    }
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let workouts = WorkoutSession::select(db, "WHERE status = 'PLAN_UPDATED' AND completed_at <= ?1 ORDER BY rowid", [now])?;
    let completed: Vec<&WorkoutSession> = workouts.iter().filter(|w| w.started_at.is_some_and(|s| week_of(s, &tz) == week_start)).collect();
    let mut sets: Vec<SetLog> = Vec::new();
    let mut labels: Vec<CrowdFeedback> = Vec::new();
    for w in &completed {
        sets.extend(SetLog::select(db, "WHERE workout_exercise_id IN (SELECT id FROM workout_exercise WHERE workout_session_id = ?1)", [&w.id])?);
        labels.extend(CrowdFeedback::select(db, "WHERE workout_session_id = ?1", [&w.id])?);
    }
    let plans: Vec<PlannedSession> = PlannedSession::select(db, "ORDER BY rowid", [])?.into_iter().filter(|p| week_of(p.planned_start_at, &tz) == week_start).collect();
    let durations: Vec<i64> = completed.iter().filter_map(|w| w.actual_duration_seconds).collect();
    if learn {
        let history: Vec<i64> = workouts
            .iter()
            .filter(|w| w.started_at.is_some_and(|s| now - DAY * 90 <= s && s <= now))
            .filter_map(|w| w.actual_duration_seconds)
            .collect();
        if history.len() >= 3 {
            let n = history.len() as f64;
            let value = json!({"median_seconds": median(&history), "demo": workouts.iter().any(|w| demo(&w.template_snapshot))});
            match LearnedPreference::first(db, "WHERE key = 'typical_session_duration'", [])? {
                Some(mut pref) => {
                    pref.value_json = Json(value);
                    pref.evidence_count = history.len() as i64;
                    pref.confidence = (n / (n + 5.0)).min(0.9);
                    pref.last_observed_at = now;
                    pref.update(db)?;
                }
                None => LearnedPreference { id: new_id(), key: "typical_session_duration".into(), value_json: Json(value), confidence: (n / (n + 5.0)).min(0.9),
                    evidence_count: history.len() as i64, first_observed_at: now, last_observed_at: now, source: "completed_workouts".into() }
                .insert(db)?,
            }
        }
    }
    let count = |status: SessionStatus| plans.iter().filter(|p| p.status == status).count();
    Ok(json!({"week_start": week_start.to_string(), "completed_workouts": completed.len(), "target_sessions": profile.weekly_target_sessions,
        "planned_sessions": plans.len(), "missed_sessions": count(SessionStatus::Missed),
        "skipped_sessions": count(SessionStatus::Skipped), "cancelled_sessions": count(SessionStatus::Cancelled),
        "working_sets": sets.iter().filter(|s| s.set_type == SetType::Working).count(), "warmup_sets": sets.iter().filter(|s| s.set_type == SetType::Warmup).count(),
        "median_duration_seconds": median(&durations), "crowd_labels": labels.len(),
        "demo": completed.iter().any(|w| demo(&w.template_snapshot)) || labels.iter().any(|f| demo(&f.features_json))}))
}

/// Elapsed slot is missed, never fabricated completion; calendar history stays.
pub fn mark_missed(db: &Connection, now: Ts) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for mut plan in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_end_at < ?1 ORDER BY rowid", [now])? {
        retire_session(db, &mut plan, SessionStatus::Missed, now)?;
        emit(db, "workout.missed", now, json!({"session_id": plan.id, "reason": "planned_window_elapsed_without_start"}))?;
        ids.push(plan.id);
    }
    Ok(ids)
}

/// 30 min into a committed slot with no workout started: ask once whether to skip or move it. Unanswered,
/// the slot still ends as MISSED and replanning finds a make-up slot. → (event id, session id, message).
pub fn no_show_nudges(db: &Connection, now: Ts) -> Result<Vec<(String, String, String)>> {
    let tz = get_profile(db)?.tz();
    let mut nudges = Vec::new();
    for session in PlannedSession::select(db, "WHERE status = 'COMMITTED' AND planned_start_at <= ?1 AND planned_end_at > ?2 ORDER BY rowid", (now - NO_SHOW_AFTER, now))? {
        let key = format!("no-show:{}:{}", session.id, session.planned_start_at);
        if AgentEvent::first(db, "WHERE correlation_id = ?1", [&key])?.is_some() || session_blocked(db, &session)? {
            continue;
        }
        // A workout started from chat without the session still means the owner showed up.
        if WorkoutSession::first(db, "WHERE started_at >= ?1", [session.prep_start_at])?.is_some() {
            continue;
        }
        let name = session.plan("template").and_then(|t| t.get("name")).and_then(Value::as_str).unwrap_or("Workout");
        let message = format!("⏰ {name} was planned for {} and hasn't started. Skip it or move it? Or just start when you're there.", session.planned_start_at.local(&tz).strftime("%H:%M"));
        let mut event = emit(db, "planning.no_show", now, json!({"session_id": session.id, "start": session.planned_start_at, "message": message}))?;
        event.correlation_id = Some(key);
        event.update(db)?;
        nudges.push((event.id, session.id, message));
    }
    Ok(nudges)
}

/// Idempotency key for a week's first plan: stable through quiet watcher ticks; calendar/profile changes
/// retry an impossible week. (Not byte-compatible with Python's key; only stability matters.)
fn first_plan_key(db: &Connection, week: Date, now: Ts, template_id: &str) -> Result<String> {
    let profile = get_profile(db)?;
    let snapshots: Vec<Value> = CalendarEventSnapshot::select(db, "ORDER BY calendar_event_id", [])?
        .into_iter()
        .map(|s| json!([s.calendar_event_id, s.start_at, s.end_at, s.status, s.raw_json]))
        .collect();
    let material = json!({"profile": profile, "calendar": snapshots, "template": template_id, "day": now.local(&profile.tz()).date().to_string()});
    let digest = hex::encode(Sha256::digest(material.to_string()));
    Ok(format!("rolling:{week}:{}", &digest[..32]))
}

/// Rotate the owner's split (e.g. Push → Pull → Legs) over upcoming sessions, continuing after the last
/// started workout (a running one counts, so starting it doesn't reshuffle the week). Locked sessions
/// keep their template but still advance the rotation.
pub fn assign_rotation(db: &Connection, now: Ts) -> Result<Vec<Value>> {
    let split = OnboardingState::get(db, 1)?.map(|o| o.split_json.0).unwrap_or_default();
    if split.len() < 2 {
        return Ok(vec![]);
    }
    let last = WorkoutSession::first(db, "ORDER BY started_at DESC", [])?.and_then(|w| w.template_id);
    let index_of = |id: Option<&String>| id.and_then(|id| split.iter().position(|s| s == id));
    let mut position = index_of(last.as_ref()).map_or(0, |i| i + 1);
    let mut changed = Vec::new();
    for mut session in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') ORDER BY planned_start_at, id", [])? {
        let wanted = split[position % split.len()].clone();
        if session.user_locked {
            position = index_of(session.workout_template_id.as_ref()).map_or(position + 1, |i| i + 1);
            continue;
        }
        position += 1;
        if session.workout_template_id.as_deref() == Some(wanted.as_str()) {
            continue;
        }
        session.workout_template_id = Some(wanted.clone());
        // Drop the old template snapshot so allocation uses the rotated template.
        session.workout_plan_json.0 = session.crowd_keys(true);
        session.source_revision += 1;
        allocation(db, &mut session)?;
        session.save(db)?;
        queue_session_write(db, &session, now, false)?;
        changed.push(json!({"session_id": session.id, "template_id": wanted}));
    }
    if !changed.is_empty() {
        emit(db, "planning.rotation_assigned", now, json!({"changes": changed}))?;
    }
    Ok(changed)
}

/// What a rolling replan changed, in the owner's words: "Fri moved to 11:00 (was 10:30) · Wed is now Push".
/// None when nothing visible changed (forecast-only updates stay quiet).
pub fn plan_changes(db: &Connection, rolling: &Value) -> Result<Option<String>> {
    let tz = get_profile(db)?.tz();
    let local = |v: &Value| v.as_str().and_then(|s| Ts::parse(s, None).ok()).map(|t| t.local(&tz));
    let session = |id: &Value| -> Result<Option<PlannedSession>> { id.as_str().map_or(Ok(None), |id| PlannedSession::get(db, id)) };
    let mut parts = Vec::new();
    for change in rolling["changed"].as_array().into_iter().flatten() {
        match change["action"].as_str() {
            Some("moved") => {
                if let (Some(old), Some(new)) = (local(&change["from"]), local(&change["to"])) {
                    parts.push(if old.date() == new.date() {
                        format!("{} moved to {} (was {})", new.strftime("%a"), new.strftime("%H:%M"), old.strftime("%H:%M"))
                    } else {
                        format!("{} moved to {}", old.strftime("%a %H:%M"), new.strftime("%a %H:%M"))
                    });
                }
            }
            Some("replacement") => {
                if let Some(to) = local(&change["to"]) {
                    parts.push(format!("New session {}", to.strftime("%a %H:%M")));
                }
            }
            Some("cancelled_no_valid_slot") => {
                if let Some(s) = session(&change["session_id"])? {
                    parts.push(format!("{} dropped: no free slot", s.planned_start_at.local(&tz).strftime("%a")));
                }
            }
            _ => {}
        }
    }
    for change in rolling["rotation"].as_array().into_iter().flatten() {
        if let Some(s) = session(&change["session_id"])? {
            let name = get_template(db, change["template_id"].as_str().unwrap_or_default())?.name;
            parts.push(format!("{} is now {name}", s.planned_start_at.local(&tz).strftime("%a")));
        }
    }
    if let Some(created) = rolling["created"].as_array().filter(|c| !c.is_empty()) {
        parts.push(format!("{} new sessions planned", created.len()));
    }
    for id in rolling["missed"].as_array().into_iter().flatten() {
        if let Some(s) = session(id)? {
            parts.push(format!("{} marked missed", s.planned_start_at.local(&tz).strftime("%a")));
        }
    }
    Ok((!parts.is_empty()).then(|| format!("📅 Plan updated: {}", parts.join(" · "))))
}

/// A workout left open (chat reset, phone away) would block the next start forever. After 3 idle hours:
/// skip what's left as abandoned and finish it, so logged sets and progression are kept.
pub fn close_stale_workouts(db: &Connection, now: Ts) -> Result<Vec<String>> {
    use crate::domain::workout::{UNRESOLVED, exercises};
    let mut closed = Vec::new();
    for workout in WorkoutSession::select(db, "WHERE status != 'PLAN_UPDATED' AND last_action_at < ?1 ORDER BY rowid", [now - STALE_WORKOUT])? {
        // Close at the last action, so duration means training time, not time until we noticed.
        let Some(ended) = workout.last_action_at else { continue };
        for row in exercises(db, &workout.id)? {
            if UNRESOLVED.contains(&row.status) {
                crate::domain::adaptation::skip_exercise(db, &workout.id, &row.id, "abandoned", ended, &format!("stale:{}:{}", workout.id, row.id))?;
            }
        }
        if WorkoutSession::get(db, &workout.id)?.is_some_and(|w| w.status == WorkoutStatus::WorkoutComplete) {
            crate::domain::audit::finish(db, &workout.id, ended, &format!("stale:{}:finish", workout.id))?;
            closed.push(workout.id);
        }
    }
    Ok(closed)
}

/// Worth a calendar edit: first data, a different feel, or ≥3 people and ≥25% change.
fn significant(old: Option<&Value>, new: Option<&Value>) -> bool {
    let Some(new) = new.filter(|n| n.as_object().is_some_and(|o| !o.is_empty())) else { return false };
    let Some(old) = old.filter(|o| o.as_object().is_some_and(|o| !o.is_empty())) else { return true };
    if old.get("feel") != new.get("feel") {
        return true;
    }
    match (old.get("count").and_then(Value::as_f64), new.get("count").and_then(Value::as_f64)) {
        (Some(before), Some(after)) => (after - before).abs() >= 3.0 && (after - before).abs() >= 0.25 * before.max(1.0),
        (before, after) => before != after,
    }
}

/// Keep "Expected crowd" in calendar events current without churn: fill unknown forecasts, then re-check
/// sessions in the next 24 h every 30 min and update only on a significant change. Nothing within 1 h of
/// start, so imminent reminders aren't rescheduled.
pub fn refresh_crowd(db: &Connection, now: Ts) -> Result<Vec<Value>> {
    let model = CrowdModel::new(db, now)?;
    let mut changed = Vec::new();
    for mut session in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_start_at > ?1 ORDER BY rowid", [now.mins(60)])? {
        let old = session.plan("crowd_forecast").filter(|v| v.as_object().is_some_and(|o| !o.is_empty())).cloned();
        let checked = session.plan("crowd_checked_at").and_then(Value::as_str).and_then(|s| Ts::parse(s, None).ok());
        if old.is_some() && (session.planned_start_at > now + DAY || checked.is_some_and(|c| now - c < CROWD_RECHECK)) {
            continue;
        }
        let new = model.forecast(session.planned_start_at);
        let update = significant(old.as_ref(), new.as_ref());
        session.merge_plan(json!({"crowd_checked_at": now}));
        if update {
            session.merge_plan(json!({"crowd_forecast": new}));
            session.source_revision += 1;
        }
        session.save(db)?;
        if update {
            // Description-only edit for published events; unpublished ones get a full create.
            queue_session_write(db, &session, now, session.calendar_event_id.is_some())?;
            changed.push(json!({"session_id": session.id, "forecast": new}));
        }
    }
    Ok(changed)
}

/// Planned sessions store a copy of their template. After template edits or swaps, re-allocate from the
/// current template so the calendar and the workout use today's plan.
pub fn refresh_plans(db: &Connection, now: Ts) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for mut session in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') AND workout_template_id IS NOT NULL ORDER BY rowid", [])? {
        let template_id = session.workout_template_id.clone().expect("filtered");
        if WorkoutTemplate::get(db, &template_id)?.is_none() {
            continue;
        }
        let current = serde_json::to_value(get_template(db, &template_id)?)?;
        let stored = session.plan("template").cloned().unwrap_or(json!({}));
        // Allocation tunes set duration from history; only the plan itself matters here.
        if ["name", "exercises", "alternatives"].iter().all(|k| stored.get(*k) == current.get(*k)) {
            continue;
        }
        session.workout_plan_json.0 = session.crowd_keys(true);
        allocation(db, &mut session)?;
        session.source_revision += 1;
        session.save(db)?;
        queue_session_write(db, &session, now, session.calendar_event_id.is_some())?;
        changed.push(session.id);
    }
    Ok(changed)
}

/// The watcher's planning pass: mark missed, repair this and next week, plan a new week inside the
/// rolling horizon, close stale workouts, rotate, refresh plans and crowd forecasts, commit.
pub fn rolling_plan(db: &Connection, template_id: &str, now: Ts) -> Result<Value> {
    get_template(db, template_id)?; // Explicit selected template, never guess from demo import.
    let tz = get_profile(db)?.tz();
    let current_week = week_of(now, &tz);
    let next_week = current_week.checked_add(days(7))?;
    let missed = mark_missed(db, now)?;
    let triggers: Vec<AgentEvent> = AgentEvent::select(db, "WHERE type = 'planning.replan_required' AND handled_at IS NULL ORDER BY rowid", [])?
        .into_iter()
        .filter(|e| matches!(e.payload_json.get("reason").and_then(Value::as_str), Some("crowd_feedback" | "workout_completed")))
        .collect();
    let reconsider = triggers.iter().any(|e| e.payload_json["reason"] == "crowd_feedback");
    let repaired = replan_weeks(db, &BTreeSet::from([current_week, next_week]), now, reconsider)?;
    for mut event in triggers {
        event.handled_at = Some(now);
        event.update(db)?;
    }
    let mut created: Vec<String> = Vec::new();
    for week in [current_week, next_week] {
        let start = Ts(week.to_zoned(tz.clone())?.timestamp());
        // Start new next-week planning only inside rolling 10-day horizon.
        if start > now + DAY * 3 {
            continue;
        }
        if PlannedSession::first(db, "WHERE week_id = ?1", [week.to_string()])?.is_none() {
            let key = first_plan_key(db, week, now, template_id)?;
            let data = schedule_week(db, week, now, Some(&key), Some(template_id))?;
            created.extend(data["sessions"].as_array().into_iter().flatten().filter_map(|s| s["id"].as_str().map(String::from)));
        }
    }
    for id in &created {
        if let Some(session) = PlannedSession::get(db, id)? {
            queue_session_write(db, &session, now, false)?;
        }
    }
    let stale = close_stale_workouts(db, now)?;
    let rotation = assign_rotation(db, now)?;
    let plans = refresh_plans(db, now)?;
    let crowd = refresh_crowd(db, now)?;
    commit_upcoming(db, now)?;
    let mut result = json!({"missed": missed, "created": created, "rotation": rotation, "crowd": crowd, "closed_workouts": stale, "refreshed_plans": plans});
    result.as_object_mut().expect("object").extend(repaired.as_object().cloned().unwrap_or_default());
    Ok(result)
}

/// The Sunday run: rolling plan, last week's audit, next week's plan, and the briefing event (once per week).
pub fn weekly_plan(db: &Connection, template_id: &str, now: Ts) -> Result<Value> {
    let tz = get_profile(db)?.tz();
    let current_week = week_of(now, &tz);
    let next_week = current_week.checked_add(days(7))?;
    let previous = if now.local(&tz).date().weekday() == jiff::civil::Weekday::Sunday { current_week } else { current_week.checked_sub(days(7))? };
    let rolling = rolling_plan(db, template_id, now)?;
    let report = audit_week(db, previous, now, true)?;
    schedule_week(db, next_week, now, Some(&format!("weekly-plan:{next_week}:{template_id}")), Some(template_id))?;
    assign_rotation(db, now)?;
    commit_upcoming(db, now)?;
    for session in PlannedSession::select(db, "WHERE week_id = ?1 AND status IN ('TENTATIVE', 'COMMITTED') ORDER BY rowid", [next_week.to_string()])? {
        queue_session_write(db, &session, now, false)?;
    }
    let key = format!("weekly-brief:{next_week}");
    let event = match AgentEvent::first(db, "WHERE correlation_id = ?1", [&key])? {
        Some(event) => event,
        None => {
            let mut event = emit(db, "planning.weekly_briefing", now, json!({"week_start": next_week.to_string(), "template_id": template_id}))?;
            event.correlation_id = Some(key);
            event.update(db)?;
            event
        }
    };
    Ok(json!({"week_start": next_week.to_string(), "audit": report, "rolling": rolling, "plan": get_week(db, next_week)?, "event_id": event.id}))
}

const CROWD_EMOJI: [(&str, &str); 4] = [("Empty", "🟢"), ("A few", "🟡"), ("Busy", "🟠"), ("Packed", "🔴")];
/// Below this the crowd model is mostly guessing; the briefing leaves crowd out instead.
const CROWD_MIN_CONFIDENCE: f64 = 0.5;

/// Consecutive weeks, ending with `week_start`, that hit the weekly session target.
fn streak(db: &Connection, week_start: Date, target: i64, zone: &TimeZone) -> Result<i64> {
    let mut counts: HashMap<Date, i64> = HashMap::new();
    for w in WorkoutSession::select(db, "WHERE status = 'PLAN_UPDATED' AND started_at IS NOT NULL", [])? {
        *counts.entry(week_of(w.started_at.expect("filtered"), zone)).or_default() += 1;
    }
    let mut weeks = 0;
    while target > 0 && counts.get(&week_start.checked_sub(days(7 * weeks))?).copied().unwrap_or(0) >= target {
        weeks += 1;
    }
    Ok(weeks)
}

/// Python's `{x:g}` for weights: no trailing ".0".
pub fn fmt_g(x: f64) -> String {
    if x.fract() == 0.0 { format!("{}", x as i64) } else { format!("{x}") }
}

/// "Bench 82.5 kg (+2.5)" for every exercise that progressed during `week_start`'s week.
fn new_targets(db: &Connection, week_start: Date, zone: &TimeZone) -> Result<Vec<String>> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: HashMap<String, Value> = HashMap::new();
    for event in AgentEvent::select(db, "WHERE type = 'progression.updated' ORDER BY created_at", [])? {
        if week_of(event.created_at, zone) == week_start {
            let id = event.payload_json["exercise_id"].as_str().unwrap_or_default().to_string();
            if !latest.contains_key(&id) {
                order.push(id.clone());
            }
            latest.insert(id, event.payload_json.0);
        }
    }
    let mut targets = Vec::new();
    for id in order {
        let decision = &latest[&id];
        let workout = decision["workout_id"].as_str().unwrap_or_default();
        let row = WorkoutExercise::first(db, "WHERE workout_session_id = ?1 AND exercise_id = ?2", [workout, &id])?;
        let name = row.and_then(|r| r.config_json.get("name").and_then(Value::as_str).map(String::from)).unwrap_or(id);
        let (next, previous) = (decision["next_weight"].as_f64().unwrap_or(0.0), decision["previous_weight"].as_f64().unwrap_or(0.0));
        targets.push(format!("{name} {} kg (+{})", fmt_g(next), fmt_g(next - previous)));
    }
    Ok(targets)
}

/// Sunday Telegram message: last week's result, next week's sessions, new progression targets and one
/// body-weight line (the only place GymClaw asks for a weigh-in). Plain text plus emoji (no markdown).
pub fn briefing(db: &Connection, week_start: Date, report: &Value, published: bool, now: Ts) -> Result<String> {
    let tz = get_profile(db)?.tz();
    let plan = get_week(db, week_start)?;
    let sessions = plan["sessions"].as_array().cloned().unwrap_or_default();
    let fixture = CalendarSyncState::get(db, 1)?.is_some_and(|s| s.source == "fixture");
    let is_demo = report["demo"].as_bool().unwrap_or(false) || fixture || sessions.iter().any(|s| s["workout_plan"]["crowd_source"] == "demo_fixture");
    let end = week_start.checked_add(days(6))?;
    let span = if week_start.month() == end.month() {
        format!("{} {}–{}", week_start.strftime("%b"), week_start.day(), end.day())
    } else {
        format!("{} {} – {} {}", week_start.strftime("%b"), week_start.day(), end.strftime("%b"), end.day())
    };
    let mut lines = vec![format!("🏋️ Your week · {span}{}", if is_demo { " · DEMO" } else { "" })];
    if !published {
        lines.push("⚠️ Local plan only — calendar publication not confirmed.".into());
    }
    let previous: Date = report["week_start"].as_str().unwrap_or_default().parse()?;
    let (done, target) = (report["completed_workouts"].as_i64().unwrap_or(0), report["target_sessions"].as_i64().unwrap_or(0));
    let weeks = streak(db, previous, target, &tz)?;
    let mut recap = format!("Last week {done}/{target}");
    if target > 0 && done >= target {
        recap += " ✅";
    }
    if weeks >= 2 {
        recap += &format!(" · {weeks} weeks in a row 🔥");
    }
    lines.extend([String::new(), recap, String::new()]);
    let mut listed = Vec::new();
    let mut flexible = Vec::new();
    for session in &sessions {
        let status = session["status"].as_str().unwrap_or_default();
        if !matches!(status, "TENTATIVE" | "COMMITTED") {
            continue;
        }
        let start = Ts::parse(session["start"].as_str().unwrap_or_default(), None)?;
        let finish = Ts::parse(session["expected_finish"].as_str().unwrap_or_default(), None)?;
        let at = start.local(&tz);
        let minutes = ((finish - start).as_secs_f64() / 60.0).round_ties_even() as i64;
        let name = session["workout_plan"]["template"]["name"].as_str().unwrap_or("Workout");
        let mut parts = vec![at.strftime("%a %H:%M").to_string(), name.to_string(), format!("~{minutes} min")];
        let label = if session["confidence"].as_f64().unwrap_or(0.0) >= CROWD_MIN_CONFIDENCE { feel(session["crowd"].as_f64()) } else { None };
        if let Some(label) = label {
            let emoji = CROWD_EMOJI.iter().find(|(l, _)| *l == label).map_or("", |(_, e)| *e);
            parts.push(format!("{emoji} {label}"));
        }
        listed.push(parts.join(" · "));
        if status == "TENTATIVE" {
            flexible.push(at.strftime("%a").to_string());
        }
    }
    if listed.is_empty() {
        lines.push("No sessions fit this week — calendar/recovery constraints kept.".into());
    }
    lines.extend(listed);
    if !flexible.is_empty() {
        // commit_upcoming locks sessions 48 h ahead; until then replanning may still move them.
        let days = if flexible.len() == 1 { flexible[0].clone() } else { format!("{} & {}", flexible[..flexible.len() - 1].join(", "), flexible[flexible.len() - 1]) };
        lines.extend([String::new(), format!("{days} can still move if your calendar changes.")]);
    }
    let targets = new_targets(db, previous, &tz)?;
    if !targets.is_empty() {
        lines.extend([String::new(), format!("🎯 New targets: {}", targets.join(", "))]);
    }
    if let Some(weight) = crate::domain::body::briefing_line(db, now)? {
        lines.extend([String::new(), weight]);
    }
    Ok(lines.join("\n"))
}
