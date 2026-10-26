//! Rolling maintenance, weekly audit and the Sunday briefing (port of weekly.py). Owner: lead (planning).
use std::collections::{BTreeSet, HashMap};

use jiff::SignedDuration;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::db::models::{
    AgentEvent, CalendarEventSnapshot, CalendarSyncState, CrowdFeedback, LearnedPreference, OnboardingState, PlannedSession, SessionStatus, SetLog, SetType, WorkoutExercise, WorkoutSession,
    WorkoutTemplate,
};
use crate::db::{Connection, Json, new_id};
use crate::domain::availability::session_blocked;
use crate::domain::calendar::{allocation, get_week, week_of};
use crate::domain::calendar_writes::queue_session_write;
use crate::domain::core::emit;
use crate::domain::crowd::{CrowdModel, feel};
use crate::domain::profile::get_profile;
use crate::domain::replanning::{commit_upcoming, inspect_weeks, retire_session};
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

/// What happened in a week: completed workouts vs target, each workout's outcome, sets, durations. With
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
    let plans: Vec<PlannedSession> = PlannedSession::select(db, "ORDER BY planned_start_at, rowid", [])?.into_iter().filter(|p| week_of(p.planned_start_at, &tz) == week_start).collect();
    let rotates = OnboardingState::get(db, 1)?.is_some_and(|o| o.split_json.0.len() >= 2);
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
    Ok(json!({"week_start": week_start.to_string(), "completed_workouts": completed.len(), "target_sessions": profile.weekly_target_sessions,
        "workouts": workout_outcomes(&plans, &completed, rotates),
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

/// 30 min into an unstarted committed slot: queue one agent event. The agent decides how to respond.
/// An elapsed slot still ends as MISSED; a make-up session requires an agent decision.
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

/// What became of each workout slot in the week, in order, e.g. pull completed, legs missed, push skipped.
/// With a split rotation a missed or skipped workout moves on to the next slot, so a later slot of the same
/// workout supersedes it: Legs missed Thursday, carried to Friday and missed again is one missed workout,
/// not two. Cancelled slots were moved or dropped, so they don't count.
fn workout_outcomes(plans: &[PlannedSession], completed: &[&WorkoutSession], rotates: bool) -> Vec<Value> {
    let mut outcomes: Vec<(String, &str)> = Vec::new();
    for plan in plans {
        let outcome = match plan.status {
            SessionStatus::Cancelled => continue,
            SessionStatus::Missed => "missed",
            SessionStatus::Skipped => "skipped",
            SessionStatus::Started | SessionStatus::Completed => "completed",
            SessionStatus::Tentative | SessionStatus::Committed => "planned",
        };
        let workout = plan.workout_template_id.clone().unwrap_or_default();
        let carried = outcomes.iter().rposition(|(w, o)| *w == workout && matches!(*o, "missed" | "skipped")).filter(|_| rotates);
        match carried {
            Some(i) => outcomes[i].1 = outcome,
            None => outcomes.push((workout, outcome)),
        }
    }
    // Workouts started without a planned slot.
    for workout in completed.iter().filter(|w| w.planned_session_id.is_none()) {
        outcomes.push((workout.template_id.clone().unwrap_or_default(), "completed"));
    }
    outcomes.into_iter().map(|(workout, outcome)| json!({"workout": workout, "outcome": outcome})).collect()
}

/// Give each upcoming session the split workout (e.g. Push / Pull / Legs) trained least recently, counting
/// started workouts (a running one too, so starting it doesn't reshuffle the week) and the sessions before
/// it. In steady state that's the plain rotation; after a skip or an owner swap the missed workout comes
/// next instead of repeating the last one. Locked sessions keep their template.
pub fn assign_rotation(db: &Connection, now: Ts) -> Result<Vec<Value>> {
    let split = OnboardingState::get(db, 1)?.map(|o| o.split_json.0).unwrap_or_default();
    if split.len() < 2 {
        return Ok(vec![]);
    }
    let mut last: HashMap<String, Ts> = HashMap::new();
    for workout in WorkoutSession::select(db, "WHERE template_id IS NOT NULL AND started_at IS NOT NULL", [])? {
        let (Some(template), Some(at)) = (workout.template_id, workout.started_at) else { continue };
        let entry = last.entry(template).or_insert(at);
        *entry = (*entry).max(at);
    }
    let mut changed = Vec::new();
    for mut session in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') ORDER BY planned_start_at, id", [])? {
        // Never-trained workouts go first (None < Some), ties in split order.
        let wanted = split.iter().enumerate().min_by_key(|(i, t)| (last.get(*t).copied(), *i)).map(|(_, t)| t.clone()).expect("split has entries");
        if session.user_locked {
            if let Some(template) = &session.workout_template_id {
                last.insert(template.clone(), session.planned_start_at);
            }
            continue;
        }
        last.insert(wanted.clone(), session.planned_start_at);
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

/// Flag an idle workout once; the agent decides whether the owner paused or finished. Logged
/// sets and unfinished movements stay intact until that decision.
pub fn detect_stale_workouts(db: &Connection, now: Ts) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for workout in WorkoutSession::select(db, "WHERE status != 'PLAN_UPDATED' AND last_action_at < ?1 ORDER BY rowid", [now - STALE_WORKOUT])? {
        crate::domain::events::request(db, "workout.stale", json!({"workout_id": workout.id, "last_action_at": workout.last_action_at}), now)?;
        ids.push(workout.id);
    }
    Ok(ids)
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
    for mut session in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') AND workout_template_id IS NOT NULL AND user_locked = 0 ORDER BY rowid", [])? {
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

/// Deterministic upkeep: mark elapsed slots missed, inspect conflicts, draft previously empty weeks,
/// rotate defaults, refresh unlocked plan snapshots/crowd, and reconcile reminder jobs.
pub fn rolling_plan(db: &Connection, template_id: &str, now: Ts) -> Result<Value> {
    get_template(db, template_id)?; // Explicit selected template, never guess from demo import.
    let tz = get_profile(db)?.tz();
    let current_week = week_of(now, &tz);
    let next_week = current_week.checked_add(days(7))?;
    let missed = mark_missed(db, now)?;
    let repaired = inspect_weeks(db, &BTreeSet::from([current_week, next_week]), now)?;
    let mut created: Vec<String> = Vec::new();
    for week in [current_week, next_week] {
        let start = Ts(week.to_zoned(tz.clone())?.timestamp());
        // Start new next-week planning only inside rolling 10-day horizon.
        if start > now + DAY * 3 {
            continue;
        }
        if PlannedSession::first(db, "WHERE week_id = ?1", [week.to_string()])?.is_none() {
            let key = first_plan_key(db, week, now, template_id)?;
            let data = schedule_week(db, week, now, Some(&key), Some(template_id), &[])?;
            created.extend(data["sessions"].as_array().into_iter().flatten().filter_map(|s| s["id"].as_str().map(String::from)));
        }
    }
    for id in &created {
        if let Some(session) = PlannedSession::get(db, id)? {
            queue_session_write(db, &session, now, false)?;
        }
    }
    let stale = detect_stale_workouts(db, now)?;
    let rotation = assign_rotation(db, now)?;
    let plans = refresh_plans(db, now)?;
    let crowd = refresh_crowd(db, now)?;
    commit_upcoming(db, now)?;
    let mut result = json!({"missed": missed, "created": created, "rotation": rotation, "crowd": crowd, "stale_workouts": stale, "refreshed_plans": plans});
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
    if PlannedSession::first(db, "WHERE week_id = ?1", [next_week.to_string()])?.is_none() {
        schedule_week(db, next_week, now, Some(&format!("weekly-plan:{next_week}:{template_id}")), Some(template_id), &[])?;
    }
    assign_rotation(db, now)?;
    commit_upcoming(db, now)?;
    for session in PlannedSession::select(db, "WHERE week_id = ?1 AND status IN ('TENTATIVE', 'COMMITTED') ORDER BY rowid", [next_week.to_string()])? {
        queue_session_write(db, &session, now, false)?;
    }
    let key = format!("weekly-brief:{next_week}");
    let event = match AgentEvent::first(db, "WHERE correlation_id = ?1", [&key])? {
        Some(event) => event,
        None => {
            let recent = recent_sessions(db, previous.checked_sub(days(7 * (REVIEW_WEEKS - 1)))?, next_week, &tz)?;
            let payload = json!({"week_start": next_week.to_string(), "template_id": template_id, "audit": report, "recent_sessions": recent});
            let mut event = emit(db, "planning.weekly_briefing", now, payload)?;
            event.correlation_id = Some(key);
            event.update(db)?;
            event
        }
    };
    Ok(json!({"week_start": next_week.to_string(), "audit": report, "rolling": rolling, "plan": get_week(db, next_week)?, "event_id": event.id}))
}

/// Weeks of sessions the Sunday review looks back on.
const REVIEW_WEEKS: i64 = 4;

/// Sessions from `from` until `until` for the agent's weekly review, one line each in local time:
/// "Thu Oct 22 12:45 legs MISSED (owner's pick)". Cancelled slots were moved or dropped, so they're left out.
fn recent_sessions(db: &Connection, from: Date, until: Date, tz: &TimeZone) -> Result<Vec<String>> {
    let sessions = PlannedSession::select(db, "WHERE status != 'CANCELLED' ORDER BY planned_start_at", [])?;
    Ok(sessions
        .into_iter()
        .filter(|s| (from..until).contains(&s.planned_start_at.local(tz).date()))
        .map(|s| {
            let pick = if s.user_locked { " (owner's pick)" } else { "" };
            let workout = s.workout_template_id.as_deref().unwrap_or("workout");
            format!("{} {workout} {}{pick}", s.planned_start_at.local(tz).strftime("%a %b %-d %H:%M"), s.status.as_str())
        })
        .collect())
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
        // commit_upcoming confirms sessions 48 h ahead and reconciles reminders.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{CalendarWrite, NotificationDelivery};
    use crate::domain::calendar::allocation;
    use crate::domain::calendar_writes::{describe_crowd, event_body, pending_writes};
    use crate::domain::crowd::record_feedback;
    use crate::domain::profile::update_profile;
    use crate::domain::replanning::{edit_session, SessionPatch};
    use crate::domain::templates::{Template, import_template};
    use crate::domain::workout::start;
    use crate::runtime::outbox::{event_stale_reason, prepare_event_message};

    fn ts(text: &str) -> Ts {
        Ts::parse(text, None).unwrap()
    }

    /// Sunday 2026-10-11 19:00 Berlin, plus `minutes`.
    fn sunday(minutes: i64) -> Ts {
        ts("2026-10-11T17:00:00+00:00").mins(minutes)
    }

    fn template(db: &Connection, value: Value) {
        import_template(db, &serde_json::from_value::<Template>(value).unwrap()).unwrap();
    }

    /// Fresh DB with the one-exercise "short" template.
    fn db() -> Connection {
        let db = crate::domain::workout::tests::db();
        template(&db, json!({"id": "short", "name": "Short", "exercises": [{"id": "bench", "name": "Bench", "role": "press", "primary": true, "working_sets": 2, "target_weight": 80, "warmup_weight": 50}]}));
        db
    }

    fn sessions(db: &Connection) -> Vec<PlannedSession> {
        PlannedSession::select(db, "ORDER BY planned_start_at", []).unwrap()
    }

    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn row(db: &Connection, week: &str, template: Option<&str>, status: SessionStatus, start: Ts, minutes: i64) -> PlannedSession {
        let mut session = PlannedSession::new(week.into(), template.map(String::from), false);
        session.status = status;
        (session.planned_start_at, session.planned_end_at, session.prep_start_at, session.leave_home_at, session.expected_finish_at) = (start, start.mins(minutes), start, start, start.mins(minutes));
        session.insert(db).unwrap();
        session
    }

    fn split(db: &Connection, templates: &[&str]) {
        db.execute("INSERT INTO onboarding_state (id, interview_json, split_json) VALUES (1, '{}', ?1)", [json!(templates).to_string()]).unwrap();
    }

    #[test]
    fn weekly_briefing_carries_recent_sessions_for_the_review() {
        let db = db();
        weekly_plan(&db, "short", sunday(0)).unwrap();
        let mut first = sessions(&db).remove(0);
        first.status = SessionStatus::Missed;
        first.update(&db).unwrap();
        let mut second = sessions(&db).remove(1);
        second.status = SessionStatus::Cancelled;
        second.update(&db).unwrap();
        let next = weekly_plan(&db, "short", sunday(7 * 24 * 60)).unwrap();
        let event = AgentEvent::get(&db, next["event_id"].as_str().unwrap()).unwrap().unwrap();
        let recent: Vec<String> = serde_json::from_value(event.payload_json["recent_sessions"].clone()).unwrap();
        // Last week's two remaining slots; the cancelled one and next week's plan are left out.
        assert_eq!(recent.len(), 2, "{recent:?}");
        assert!(recent[0].starts_with("Mon Oct 12 ") && recent[0].ends_with(" short MISSED"), "{recent:?}");
    }

    #[test]
    fn weekly_plan_retry_one_briefing_and_no_duplicate_calendar_intent() {
        let db = db();
        let result = weekly_plan(&db, "short", sunday(0)).unwrap();
        assert_eq!(result["week_start"], "2026-10-12");
        assert_eq!(result["plan"]["sessions"].as_array().unwrap().len(), 3);
        assert_eq!(result["audit"]["completed_workouts"], 0);
        assert_eq!(pending_writes(&db).unwrap().len(), 3);
        let text = briefing(&db, Date::constant(2026, 10, 12), &result["audit"], false, sunday(0)).unwrap();
        assert!(text.contains("Local plan only") && text.contains("Mon") && text.contains("Last week 0/3"), "{text}");
        let retry = weekly_plan(&db, "short", sunday(1)).unwrap();
        assert_eq!(retry["event_id"], result["event_id"]);
        assert_eq!(sessions(&db).len(), 3);
        assert_eq!(pending_writes(&db).unwrap().len(), 3);
        assert_eq!(count(&db, "SELECT count(*) FROM agent_event WHERE type = 'planning.weekly_briefing'"), 1);
    }

    #[test]
    fn briefing_hides_uncertain_crowd_and_lists_new_targets() {
        let db = db();
        let result = weekly_plan(&db, "short", sunday(0)).unwrap();
        let mut rows = sessions(&db);
        (rows[0].crowd_prediction, rows[0].crowd_confidence) = (Some(0.9), Some(0.7));
        (rows[1].crowd_prediction, rows[1].crowd_confidence) = (Some(0.1), Some(0.3));
        rows[0].save(&db).unwrap();
        rows[1].save(&db).unwrap();
        emit(&db, "progression.updated", sunday(-2 * 24 * 60), json!({"exercise_id": "bench", "previous_weight": 80, "next_weight": 82.5, "workout_id": "gone"})).unwrap();
        let text = briefing(&db, Date::constant(2026, 10, 12), &result["audit"], true, sunday(0)).unwrap();
        assert!(text.starts_with("🏋️ Your week · Oct 12–18"), "{text}");
        assert!(text.contains("🔴 Packed") && !text.contains("Empty") && !text.contains("score"), "{text}");
        assert!(text.contains("can still move if your calendar changes"));
        assert!(text.contains("\n🎯 New targets: bench 82.5 kg (+2.5)\n"), "{text}");
        assert!(text.ends_with("⚖️ Snap a photo of your scale anytime and I'll track your weight."));
    }

    #[test]
    fn rolling_watch_marks_elapsed_unstarted_session_missed_not_completed() {
        let db = db();
        weekly_plan(&db, "short", sunday(0)).unwrap();
        let later = sunday(26 * 60);
        let result = rolling_plan(&db, "short", later).unwrap();
        let missed = result["missed"].as_array().unwrap();
        assert_eq!(missed.len(), 1);
        let missed = PlannedSession::get(&db, missed[0].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(missed.status, SessionStatus::Missed);
        assert!(CalendarWrite::select(&db, "WHERE planned_session_id = ?1 AND status = 'PENDING'", [&missed.id]).unwrap().is_empty());
        let report = audit_week(&db, Date::constant(2026, 10, 12), later, false).unwrap();
        assert_eq!((&report["workouts"][0], &report["completed_workouts"]), (&json!({"workout": "short", "outcome": "missed"}), &json!(0)));
        let before = count(&db, "SELECT count(*) FROM agent_event WHERE type = 'workout.missed'");
        rolling_plan(&db, "short", later.mins(1)).unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM agent_event WHERE type = 'workout.missed'"), before);
    }

    #[test]
    fn start_without_template_starts_todays_session_even_when_late() {
        let db = db();
        weekly_plan(&db, "short", sunday(0)).unwrap();
        let first = sessions(&db).remove(0);
        let error = start(&db, None, None, false, first.planned_start_at.mins(-24 * 60), "yesterday").unwrap_err();
        assert!(error.to_string().contains("No planned workout today"), "{error}");
        // Its slot is over and already marked missed; the owner is just late.
        let mut missed = first.clone();
        missed.status = SessionStatus::Missed;
        missed.update(&db).unwrap();
        // A missed session from an earlier day stays history.
        let error = start(&db, None, Some(&first.id), false, first.planned_end_at.mins(24 * 60), "next-day").unwrap_err();
        assert_eq!(error.code(), "INVALID_PLAN");
        start(&db, None, None, false, first.planned_end_at.mins(10), "late").unwrap();
        assert_eq!(PlannedSession::get(&db, &first.id).unwrap().unwrap().status, SessionStatus::Started);
    }

    #[test]
    fn new_arrival_label_wakes_agent_and_keeps_saved_slots() {
        let db = db();
        let changes = json!({"weekly_min_sessions": 1, "weekly_target_sessions": 1, "weekly_max_sessions": 1, "weekdays_allowed": [2],
            "earliest_workout_start": "19:00", "latest_workout_finish": "22:00"});
        update_profile(&db, changes.as_object().unwrap()).unwrap();
        let original = weekly_plan(&db, "short", sunday(0)).unwrap()["plan"]["sessions"][0].clone();
        assert_eq!(ts(original["start"].as_str().unwrap()).0.strftime("%H").to_string(), "17"); // 19:00 local cold start.
        let visit = new_id();
        db.execute("INSERT INTO workout_session (id, status, arrived_at, started_at, completed_at, waited_for_equipment_count, template_snapshot) VALUES (?1, 'PLAN_UPDATED', ?2, ?2, ?3, 0, '{\"demo\": true}')",
            (&visit, ts("2026-10-07T18:00:00+00:00"), ts("2026-10-07T19:00:00+00:00"))).unwrap();
        record_feedback(&db, &visit, "EMPTY", sunday(1), "quiet-visit").unwrap();
        let result = rolling_plan(&db, "short", sunday(2)).unwrap();
        let id = original["id"].as_str().unwrap();
        let mut updated = PlannedSession::get(&db, id).unwrap().unwrap();
        assert_eq!(updated.planned_start_at.0.strftime("%H").to_string(), "17"); // Saved time stays; the agent decides.
        assert!(result["changed"].as_array().unwrap().is_empty() && updated.status == SessionStatus::Tentative);
        assert!(!crate::domain::events::wake_pending(&db).unwrap().is_empty());
        updated.user_locked = true;
        updated.save(&db).unwrap();
        let event = emit(&db, "planning.replan_required", sunday(0), json!({"reason": "crowd_feedback"})).unwrap();
        rolling_plan(&db, "short", sunday(3)).unwrap();
        assert_eq!(PlannedSession::get(&db, id).unwrap().unwrap().planned_start_at, updated.planned_start_at);
        assert!(AgentEvent::get(&db, &event.id).unwrap().unwrap().handled_at.is_none());
    }

    #[test]
    fn carried_workout_counts_once() {
        let db = db();
        weekly_plan(&db, "short", sunday(0)).unwrap();
        let session = PlannedSession::select(&db, "ORDER BY rowid", []).unwrap().remove(0);
        let slot = |workout: &str, status| PlannedSession { workout_template_id: Some(workout.into()), status, ..session.clone() };
        use SessionStatus::*;
        // Last week as the owner lived it: Legs moved Thu -> Fri and missed both times, Push skipped.
        let week = [slot("pull", Completed), slot("legs", Missed), slot("legs", Missed), slot("push", Skipped), slot("pull", Cancelled)];
        let outcomes = |week: &[PlannedSession], rotates| -> Vec<String> {
            workout_outcomes(week, &[], rotates).iter().map(|o| format!("{} {}", o["workout"].as_str().unwrap(), o["outcome"].as_str().unwrap())).collect()
        };
        assert_eq!(outcomes(&week, true), ["pull completed", "legs missed", "push skipped"]);
        assert_eq!(outcomes(&[slot("legs", Missed), slot("legs", Completed)], true), ["legs completed"]);
        // Without a rotation every slot is its own workout.
        assert_eq!(outcomes(&week, false).len(), 4);
    }

    #[test]
    fn split_gives_least_recent_workout_and_respects_locks() {
        let db = crate::domain::workout::tests::db();
        let base = ts("2026-10-05T10:00:00+00:00");
        for name in ["push", "pull", "legs"] {
            let mut title = name.to_string();
            title[..1].make_ascii_uppercase();
            template(&db, json!({"id": name, "name": title, "exercises": [{"id": format!("{name}-x"), "name": "X", "role": "x", "guide_id": "bench-press", "target_weight": 0}]}));
        }
        split(&db, &["push", "pull", "legs"]);
        let workout = |template: &str, status: &str, at: Ts| {
            db.execute("INSERT INTO workout_session (id, template_id, status, template_snapshot, started_at, waited_for_equipment_count) VALUES (?1, ?2, ?3, '{\"x\": 1}', ?4, 0)",
                (new_id(), template, status, at)).unwrap();
        };
        workout("push", "PLAN_UPDATED", base.mins(-3 * 24 * 60));
        let mut ids = Vec::new();
        for day in 0..4 {
            let mut session = row(&db, "2026-10-05", Some("push"), SessionStatus::Tentative, base.mins(day * 2 * 24 * 60), 60);
            if day == 1 {
                session.user_locked = true; // The owner pinned Push here.
                session.save(&db).unwrap();
            }
            ids.push(session.id);
        }
        let templates = |db: &Connection| ids.iter().map(|id| PlannedSession::get(db, id).unwrap().unwrap().workout_template_id.unwrap()).collect::<Vec<_>>();
        assign_rotation(&db, base.mins(-24 * 60)).unwrap();
        // Last done: Push → Pull, then the locked Push, then Legs (never trained), then Pull.
        assert_eq!(templates(&db), ["pull", "push", "legs", "pull"]);
        assert_eq!(PlannedSession::get(&db, &ids[0]).unwrap().unwrap().workout_plan_json["template"]["id"], "pull");
        assert!(assign_rotation(&db, base.mins(-24 * 60)).unwrap().is_empty());
        // A running workout counts: starting Pull doesn't hand Pull to the next session again.
        workout("pull", "SET_ACTIVE", base.mins(-60));
        assign_rotation(&db, base.mins(-24 * 60)).unwrap();
        assert_eq!(templates(&db), ["legs", "push", "pull", "legs"]);
        // Owner swapped Push for Pull and Push got skipped: Push comes next, not Legs again.
        workout("legs", "PLAN_UPDATED", base.mins(-30));
        let mut swapped = PlannedSession::get(&db, &ids[1]).unwrap().unwrap();
        swapped.workout_template_id = Some("pull".into());
        swapped.save(&db).unwrap();
        assign_rotation(&db, base.mins(-24 * 60)).unwrap();
        assert_eq!(templates(&db), ["push", "pull", "legs", "push"]);
    }

    #[test]
    fn crowd_forecast_text_and_significant_changes() {
        let start = ts("2026-10-09T19:00:00+00:00").local(&jiff::tz::TimeZone::UTC);
        assert_eq!(describe_crowd(None, &start), "not enough data yet");
        assert_eq!(describe_crowd(Some(&json!({"count": 13, "basis": "weekday_hour", "days": 2, "feel": "Busy"})), &start), "Busy · ~13 people checked in (Fridays 19:00, 2 wk)");
        assert_eq!(describe_crowd(Some(&json!({"count": 8, "basis": "hour", "days": 1, "feel": null})), &start), "~8 people checked in (19:00 on other days)");
        let old = json!({"count": 12, "feel": "Busy"});
        assert!(significant(None, Some(&json!({"count": 5, "feel": null}))));
        assert!(!significant(Some(&old), Some(&json!({"count": 14, "feel": "Busy"})))); // +2: noise
        assert!(significant(Some(&old), Some(&json!({"count": 16, "feel": "Busy"})))); // +4 and +33%
        assert!(significant(Some(&old), Some(&json!({"count": 12, "feel": "Packed"}))));
        assert!(!significant(Some(&old), None));
    }

    #[test]
    fn refresh_crowd_fills_unknown_then_respects_window() {
        let db = crate::domain::workout::tests::db();
        let now = ts("2026-10-02T12:00:00+00:00");
        db.execute("INSERT INTO crowd_observation (id, observed_at, source, raw_value, metadata_json) VALUES ('o', ?1, 'GYM_API', 13, '{}')", [now.mins(-7 * 24 * 60 + 360)]).unwrap();
        let soon = row(&db, "2026-09-28", None, SessionStatus::Committed, now.mins(360), 60);
        let imminent = row(&db, "2026-09-28", None, SessionStatus::Committed, now.mins(30), 30);
        let changed = refresh_crowd(&db, now).unwrap();
        assert_eq!(changed.iter().map(|c| c["session_id"].clone()).collect::<Vec<_>>(), [json!(soon.id)]);
        let soon = PlannedSession::get(&db, &soon.id).unwrap().unwrap();
        assert_eq!((&soon.workout_plan_json["crowd_forecast"]["count"], soon.source_revision), (&json!(13), 2));
        assert!(PlannedSession::get(&db, &imminent.id).unwrap().unwrap().plan("crowd_forecast").is_none());
        assert!(refresh_crowd(&db, now.mins(10)).unwrap().is_empty()); // Throttled.
    }

    #[test]
    fn refresh_plans_replaces_stale_copy_once() {
        let db = crate::domain::workout::tests::db();
        let now = ts("2026-10-03T08:00:00+00:00");
        let pull = |sets: i64| json!({"id": "pull", "name": "Pull", "exercises": [{"id": "row", "name": "Row", "role": "pull", "guide_id": "seated-row", "target_weight": 0, "working_sets": sets}]});
        template(&db, pull(3));
        let mut session = row(&db, "2026-10-05", Some("pull"), SessionStatus::Tentative, now.mins(2 * 24 * 60), 60);
        allocation(&db, &mut session).unwrap();
        session.save(&db).unwrap();
        template(&db, pull(2));
        assert_eq!(refresh_plans(&db, now).unwrap(), [session.id.clone()]);
        assert_eq!(PlannedSession::get(&db, &session.id).unwrap().unwrap().workout_plan_json["template"]["exercises"][0]["working_sets"], 2);
        assert!(refresh_plans(&db, now).unwrap().is_empty()); // No churn on the next tick.
    }

    #[test]
    fn event_body_has_plan_location_and_split_colour() {
        let db = crate::domain::workout::tests::db();
        for name in ["push", "pull"] {
            let mut title = name.to_string();
            title[..1].make_ascii_uppercase();
            template(&db, json!({"id": name, "name": title, "exercises": [{"id": format!("{name}-row"), "name": "Row", "role": "x", "guide_id": "seated-row", "working_sets": 2, "rep_min": 8, "rep_max": 12, "target_weight": 0}]}));
        }
        split(&db, &["push", "pull"]);
        update_profile(&db, json!({"gym_address": "Example Str. 1, Berlin"}).as_object().unwrap()).unwrap();
        let start = ts("2026-10-05T08:30:00+00:00");
        let mut session = row(&db, "2026-10-05", Some("pull"), SessionStatus::Committed, start, 60);
        (session.prep_start_at, session.leave_home_at) = (start.mins(-35), start.mins(-20));
        allocation(&db, &mut session).unwrap();
        session.save(&db).unwrap();
        let body = event_body(&db, &session).unwrap();
        assert_eq!((&body["summary"], &body["location"], &body["colorId"]), (&json!("🏋️ Pull"), &json!("Example Str. 1, Berlin"), &json!("6")));
        let description = body["description"].as_str().unwrap();
        assert!(description.contains("Get ready 09:55 · Leave 10:10 · Home ~11:50"), "{description}");
        assert!(description.contains("1. Row · 2 sets of 8–12"), "{description}");
    }

    #[test]
    fn no_show_nudges_once_then_skip_counts_toward_the_week() {
        let db = db();
        weekly_plan(&db, "short", sunday(0)).unwrap();
        let first = sessions(&db).remove(0);
        assert_eq!(first.status, SessionStatus::Committed);
        assert!(no_show_nudges(&db, first.planned_start_at.mins(29)).unwrap().is_empty());
        let late = first.planned_start_at.mins(31);
        let nudges = no_show_nudges(&db, late).unwrap();
        let [(event_id, session_id, message)] = nudges.as_slice() else { panic!("{nudges:?}") };
        assert!(session_id == &first.id && message.contains("hasn't started. Skip it or move it?"), "{message}");
        assert!(no_show_nudges(&db, late.mins(1)).unwrap().is_empty());
        let delivery = prepare_event_message(&db, event_id, message, late, "123", &[]).unwrap();
        edit_session(&db, Some(&first.id), SessionPatch { status: Some(SessionStatus::Skipped), ..Default::default() }, late, "tg-1-skip").unwrap();
        assert_eq!(PlannedSession::get(&db, &first.id).unwrap().unwrap().status, SessionStatus::Skipped);
        // The skip is final: rolling replanning keeps the other two sessions, no make-up.
        rolling_plan(&db, "short", late.mins(5)).unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM planned_session WHERE status IN ('TENTATIVE', 'COMMITTED')"), 2);
        // Answered before the send went out: the question is dropped, not sent.
        let row = NotificationDelivery::get(&db, delivery["id"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(event_stale_reason(&db, &row, late.mins(5)).unwrap(), Some("session_resolved"));
    }

    #[test]
    fn no_show_is_quiet_when_a_workout_started_and_can_still_be_moved() {
        let db = db();
        weekly_plan(&db, "short", sunday(0)).unwrap();
        let first = sessions(&db).remove(0);
        let late = first.planned_start_at.mins(31);
        let moved = edit_session(&db, Some(&first.id), SessionPatch { start: Some(late.mins(60)), ..Default::default() }, late, "tg-2-move").unwrap();
        assert!(ts(moved["data"]["session"]["start"].as_str().unwrap()) > late, "{moved}");
        assert!(matches!(PlannedSession::get(&db, &first.id).unwrap().unwrap().status, SessionStatus::Tentative | SessionStatus::Committed));
        weekly_plan(&db, "short", sunday(0)).unwrap();
        let first = sessions(&db).remove(0);
        // Started from chat without linking the planned session: still counts as showing up.
        start(&db, Some("short"), None, false, first.planned_start_at, "unlinked").unwrap();
        assert_eq!(PlannedSession::get(&db, &first.id).unwrap().unwrap().status, SessionStatus::Committed);
        assert!(no_show_nudges(&db, first.planned_start_at.mins(31)).unwrap().is_empty());
    }
}
