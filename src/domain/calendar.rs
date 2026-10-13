//! Calendar sync and owner-edit reconciliation (port of calendar.py). Sync never performs remote
//! writes. Owner: lead (planning).
use std::collections::{BTreeSet, HashSet};

use jiff::SignedDuration;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

use crate::db::models::{CalendarEventSnapshot, CalendarWrite, PlannedSession, SessionStatus, WriteStatus};
use crate::db::{Connection, Json};
use crate::domain::calendar_writes::{event_id_for, queue_session_write};
use crate::domain::notifications::{cancel_session_jobs, schedule_session_jobs};
use crate::domain::replanning::{commit_upcoming, replan_weeks};
use crate::providers::calendar::{CalendarEvent, CalendarProvider, SYNC_TOKEN_EXPIRED};
use crate::providers::google::bind_calendar;
use crate::domain::compression::compress_template;
use crate::domain::core::emit;
use crate::domain::events::outage_open;
use crate::domain::profile::get_profile;
use crate::domain::scheduling::session_data;
use crate::domain::templates::{Template, get_template};
use crate::domain::workout::historical_set_duration;
use crate::error::{Error, Result};
use crate::time::Ts;

/// Monday of the local week containing `stamp`.
pub fn week_of(stamp: Ts, zone: &TimeZone) -> Date {
    let day = stamp.local(zone).date();
    day.checked_sub(jiff::Span::new().days(day.weekday().to_monday_zero_offset() as i64)).expect("date in range")
}

/// Fit the session's workout into its slot: `workout_plan_json` becomes the compressed plan (or an
/// `error` entry if it can't fit), keeping the `crowd_*` keys. Caller saves the session.
pub fn allocation(db: &Connection, session: &mut PlannedSession) -> Result<()> {
    let Some(template_id) = session.workout_template_id.clone() else { return Ok(()) };
    let mut template: Template = match session.plan("template") {
        Some(snapshot) if !snapshot.is_null() => serde_json::from_value(snapshot.clone()).map_err(|e| Error::domain("INVALID_TEMPLATE", e.to_string()))?,
        _ => get_template(db, &template_id)?,
    };
    template.set_duration_seconds = historical_set_duration(db, template.set_duration_seconds as f64)?.ceil() as i64;
    let budget = (session.planned_end_at - session.planned_start_at).as_secs();
    let crowd = session.crowd_keys(true);
    session.workout_plan_json.0 = match compress_template(&template, &get_profile(db)?, budget) {
        Ok(plan) => plan,
        Err(error @ Error::Domain { .. }) => json!({"template": template, "budget_seconds": budget, "error": error.code()}),
        Err(other) => return Err(other),
    };
    session.merge_plan(crowd);
    Ok(())
}

/// One owner message when Google Calendar access is lost, one when it's back. Never repeats. → (event id, message).
pub fn auth_alert(db: &Connection, ok: bool, now: Ts) -> Result<Option<(String, String)>> {
    let lost = outage_open(db, "calendar.auth_lost", "calendar.auth_restored")?;
    if !ok && !lost {
        let event = emit(db, "calendar.auth_lost", now, json!({}))?;
        return Ok(Some((event.id, "⚠️ Lost access to Google Calendar, so I can't see calendar edits or update events. Moves via chat still work. Reconnect with `calendar auth` on the server.".into())));
    }
    if ok && lost {
        let event = emit(db, "calendar.auth_restored", now, json!({}))?;
        return Ok(Some((event.id, "✅ Google Calendar is connected again.".into())));
    }
    Ok(None)
}

pub fn workout_name(session: &PlannedSession) -> String {
    if let Some(name) = session.plan("template").and_then(|t| t.get("name")).and_then(Value::as_str) {
        return name.to_string();
    }
    title(session.workout_template_id.as_deref().unwrap_or("Workout"))
}

/// Python's str.title(): capitalize each alphabetic run.
fn title(text: &str) -> String {
    let mut out = String::new();
    let mut previous_alpha = false;
    for c in text.chars() {
        if previous_alpha { out.extend(c.to_lowercase()) } else { out.extend(c.to_uppercase()) }
        previous_alpha = c.is_alphabetic();
    }
    out
}

fn active_sessions(db: &Connection) -> Result<Vec<PlannedSession>> {
    PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED', 'STARTED', 'COMPLETED') ORDER BY planned_start_at", [])
}

/// The week's planned workouts for owner messages, e.g. "This week: Mon 12:15 Legs · Wed 10:30 Push."
/// Build it after rotation so names match the calendar.
pub fn week_lineup(db: &Connection, week_start: Date, now: Ts) -> Result<String> {
    let tz = get_profile(db)?.tz();
    let sessions: Vec<String> = active_sessions(db)?
        .iter()
        .filter(|s| week_of(s.planned_start_at, &tz) == week_start)
        .map(|s| format!("{} {}", s.planned_start_at.local(&tz).strftime("%a %H:%M"), workout_name(s)))
        .collect();
    let label = if week_start == week_of(now, &tz) { "This week".to_string() } else { format!("Week of {} {}", week_start.strftime("%b"), week_start.day()) };
    let list = if sessions.is_empty() { "no workouts".to_string() } else { sessions.join(" · ") };
    Ok(format!("{label}: {list}."))
}

/// Every session of the week starting `week_start` (any status).
pub fn get_week(db: &Connection, week_start: Date) -> Result<Value> {
    if !crate::domain::planning::monday(week_start) {
        crate::bail!("INVALID_WEEK", "week_start must be Monday");
    }
    let tz = get_profile(db)?.tz();
    let sessions: Vec<Value> = PlannedSession::select(db, "ORDER BY planned_start_at", [])?
        .iter()
        .filter(|s| week_of(s.planned_start_at, &tz) == week_start)
        .map(session_data)
        .collect();
    Ok(json!({"week_start": week_start.to_string(), "sessions": sessions}))
}

pub fn is_closed(status: SessionStatus) -> bool {
    matches!(status, SessionStatus::Cancelled | SessionStatus::Skipped | SessionStatus::Missed | SessionStatus::Completed)
}

/// Cache a remote event. Tombstones keep their ownership provenance and merge into the last raw payload.
pub fn persist_snapshot(db: &Connection, event: &CalendarEvent, now: Ts, locked: bool) -> Result<CalendarEventSnapshot> {
    let existing = CalendarEventSnapshot::get(db, &event.id)?;
    let fresh = existing.is_none();
    let mut row = existing.unwrap_or_else(|| CalendarEventSnapshot {
        calendar_event_id: event.id.clone(), ical_uid: None, etag: None, title: String::new(), start_at: event.start.unwrap_or(now),
        end_at: event.end.unwrap_or(now), status: String::new(), updated_at_remote: None, gymclaw_managed: event.managed, gymclaw_session_id: None,
        gymclaw_plan_revision: None, user_locked: false, last_seen_at: now, raw_json: Json(json!({})),
    });
    row.ical_uid = event.ical_uid.clone().or(row.ical_uid);
    row.etag = event.etag.clone();
    if !event.title.is_empty() {
        row.title = event.title.clone();
    }
    row.start_at = event.start.unwrap_or(row.start_at);
    row.end_at = event.end.unwrap_or(row.end_at);
    row.status = event.status.clone();
    row.updated_at_remote = event.updated;
    if !event.cancelled() {
        row.gymclaw_managed = event.managed;
        row.gymclaw_session_id = event.session_id.clone();
        row.gymclaw_plan_revision = event.revision;
    }
    row.user_locked |= locked;
    row.last_seen_at = now;
    if event.cancelled() {
        let mut raw = row.raw_json.0.as_object().cloned().unwrap_or_default();
        raw.extend(event.raw.clone());
        row.raw_json = Json(Value::Object(raw));
    } else {
        row.raw_json = Json(Value::Object(event.raw.clone()));
    }
    if fresh { row.insert(db)? } else { row.update(db)? }
    Ok(row)
}

fn cancel_writes(db: &Connection, session_id: &str, now: Ts) -> Result<()> {
    for mut write in CalendarWrite::select(db, "WHERE planned_session_id = ?1 AND status = 'PENDING' ORDER BY rowid", [session_id])? {
        write.status = WriteStatus::Cancelled;
        write.handled_at = Some(now);
        write.update(db)?;
    }
    Ok(())
}

/// Apply one remote event: owner deletes/moves/resizes of gym sessions become locked intent; other
/// events are blockers. Own-write echoes are ignored. → (weeks to replan, emitted events).
pub fn reconcile_event(db: &Connection, event: &CalendarEvent, now: Ts) -> Result<(BTreeSet<Date>, Vec<Value>)> {
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let previous = CalendarEventSnapshot::get(db, &event.id)?;
    let mut session = PlannedSession::first(db, "WHERE calendar_event_id = ?1", [&event.id])?;
    if session.is_none() && event.managed && let Some(id) = &event.session_id
        && let Some(mut candidate) = PlannedSession::get(db, id)?
        && candidate.calendar_event_id.is_none() && event.id == event_id_for(&candidate.id)
    {
        candidate.calendar_event_id = Some(event.id.clone());
        candidate.save(db)?;
        session = Some(candidate);
    }
    let mut weeks = BTreeSet::new();
    let mut emitted = Vec::new();
    let mut manual_update = false;
    let blocker_weeks = |weeks: &mut BTreeSet<Date>| {
        weeks.insert(week_of(now, &tz));
        weeks.insert(week_of(now + SignedDuration::from_hours(24 * 7), &tz));
    };
    let transparent = |raw: &Value| raw.get("transparency").and_then(Value::as_str) == Some("transparent");
    let raw = Value::Object(event.raw.clone());
    if let Some(session) = session.as_mut() {
        let old_week = week_of(session.planned_start_at, &tz);
        if event.cancelled() {
            if !is_closed(session.status) {
                session.status = SessionStatus::Cancelled;
                session.user_locked = true;
                session.merge_plan(json!({"deleted_by_user": true}));
                session.source_revision += 1;
                session.save(db)?;
                cancel_session_jobs(db, &session.id, now)?;
                cancel_writes(db, &session.id, now)?;
                let log = emit(db, "calendar.gym_event_deleted", now, json!({"session_id": session.id, "event_id": event.id}))?;
                emitted.push(json!({"id": log.id, "type": log.r#type}));
                weeks.insert(old_week);
            }
        } else {
            let (start, end) = (event.start.ok_or_else(|| Error::domain("CALENDAR_INVALID_RESPONSE", "event without start"))?, event.end.unwrap_or(now));
            let own_echo = event.revision == Some(session.source_revision) && start == session.planned_start_at && end == session.planned_end_at;
            let times_changed = match &previous {
                Some(p) => p.start_at != start || p.end_at != end,
                None => session.planned_start_at != start || session.planned_end_at != end,
            };
            if times_changed && !own_echo && !is_closed(session.status) {
                let old_duration = session.planned_end_at - session.planned_start_at;
                if start != session.planned_start_at {
                    // Old-slot forecast is not evidence about the owner's newly chosen slot.
                    session.crowd_prediction = None;
                    session.crowd_confidence = Some(0.0);
                }
                session.planned_start_at = start;
                session.planned_end_at = end;
                session.prep_start_at = start.mins(-(profile.prep_minutes + profile.commute_to_gym_minutes));
                session.leave_home_at = start.mins(-profile.commute_to_gym_minutes);
                session.expected_finish_at = end;
                session.week_id = week_of(start, &tz).to_string();
                session.user_locked = true;
                manual_update = true;
                if session.status != SessionStatus::Started {
                    session.status = SessionStatus::Committed;
                }
                session.source_revision += 1;
                allocation(db, session)?;
                session.save(db)?;
                cancel_session_jobs(db, &session.id, now)?;
                cancel_writes(db, &session.id, now)?;
                schedule_session_jobs(db, session, now)?;
                let kind = if old_duration != end - start { "calendar.gym_event_resized" } else { "calendar.gym_event_changed" };
                let log = emit(db, kind, now, json!({"session": session_data(session), "event_id": event.id}))?;
                emitted.push(json!({"id": log.id, "type": kind, "session_id": session.id}));
                weeks.extend([old_week, week_of(start, &tz)]);
            }
        }
    } else if !event.cancelled() {
        let meaningful = previous.as_ref().is_none_or(|p| {
            Some(p.start_at) != event.start || Some(p.end_at) != event.end || p.status == "cancelled"
                || p.raw_json.get("recurrence") != raw.get("recurrence") || p.raw_json.get("transparency") != raw.get("transparency")
        });
        if meaningful && (!transparent(&raw) || previous.as_ref().is_some_and(|p| !transparent(&p.raw_json))) {
            let kind = if transparent(&raw) { "calendar.blocker_removed" } else { "calendar.blocker_added" };
            let log = emit(db, kind, now, json!({"event_id": event.id}))?;
            emitted.push(json!({"id": log.id, "type": log.r#type}));
            blocker_weeks(&mut weeks);
        }
    } else if previous.as_ref().is_some_and(|p| p.status != "cancelled") || previous.is_none() && raw.get("recurringEventId").is_some_and(|v| !v.is_null() && v != "") {
        let log = emit(db, "calendar.blocker_removed", now, json!({"event_id": event.id}))?;
        emitted.push(json!({"id": log.id, "type": log.r#type}));
        blocker_weeks(&mut weeks);
    }
    persist_snapshot(db, event, now, session.as_ref().is_some_and(|s| s.user_locked))?;
    if manual_update && let Some(session) = &session {
        queue_session_write(db, session, now, true)?;
    }
    Ok((weeks, emitted))
}

/// Pull calendar changes, reconcile them, replan affected weeks and describe what happened. Never
/// writes remotely; queued writes go through `apply_write`.
pub fn sync_calendar(db: &Connection, provider: &mut dyn CalendarProvider, now: Ts) -> Result<Value> {
    let mut state = bind_calendar(db, provider.calendar_id(), provider.source())?;
    if state.last_synced_at.is_some_and(|last| now < last) {
        crate::bail!("TIME_REVERSED", "Sync timestamp precedes last calendar sync");
    }
    // Fetch every page before mutating cached events or plans. A failed fetch leaves state intact.
    let batch = match provider.incremental_sync(state.sync_token.as_deref()) {
        Err(error) if error.code() == SYNC_TOKEN_EXPIRED => provider.incremental_sync(None)?,
        other => other?,
    };
    state.timezone = batch.timezone.clone();
    let mut incoming = batch.events.clone();
    if batch.full {
        let ids: HashSet<&str> = batch.events.iter().map(|e| e.id.as_str()).collect();
        for snapshot in CalendarEventSnapshot::select(db, "WHERE status != 'cancelled' ORDER BY rowid", [])? {
            if !ids.contains(snapshot.calendar_event_id.as_str()) {
                let id = snapshot.calendar_event_id;
                incoming.push(tombstone(&id));
            }
        }
    }
    let mut affected = BTreeSet::new();
    let mut emitted = Vec::new();
    // Owner changes all reconcile before replanning, so batch order is immaterial.
    for event in &incoming {
        let (weeks, logs) = reconcile_event(db, event, now)?;
        affected.extend(weeks);
        emitted.extend(logs);
    }
    state.sync_token = Some(batch.next_sync_token.clone());
    state.last_synced_at = Some(now);
    state.update(db)?;
    let replanned = if affected.is_empty() { json!({"changed": [], "warnings": []}) } else { replan_weeks(db, &affected, now, false)? };
    commit_upcoming(db, now)?;
    let changes = replanned["changed"].as_array().cloned().unwrap_or_default();
    let warnings: Vec<String> = replanned["warnings"].as_array().into_iter().flatten().filter_map(|w| w.as_str().map(String::from)).collect();
    if !emitted.is_empty() || !changes.is_empty() || !warnings.is_empty() {
        let mut event = emit(db, "planning.replan_required", now, json!({"reason": "calendar_changed", "handled_locally": true, "changes": changes, "warnings": warnings}))?;
        // The deterministic replan already ran; the agent communicates, it doesn't redo the work.
        event.handled_at = Some(now);
        event.update(db)?;
        emitted.push(json!({"id": event.id, "type": event.r#type}));
    }
    let tz = get_profile(db)?.tz();
    // Name what happened: a vague "sessions replanned" hides that a workout was dropped.
    let mut parts = Vec::new();
    for e in &emitted {
        let kind = e["type"].as_str().unwrap_or_default();
        if kind == "calendar.gym_event_changed" || kind == "calendar.gym_event_resized" {
            let Some(session) = PlannedSession::get(db, e["session_id"].as_str().unwrap_or_default())? else { continue };
            let (start, end) = (session.planned_start_at.local(&tz), session.planned_end_at.local(&tz));
            parts.push(if kind == "calendar.gym_event_changed" {
                format!("{} moved to {}.", workout_name(&session), start.strftime("%a %H:%M"))
            } else {
                format!("{} now {}–{}.", workout_name(&session), start.strftime("%a %H:%M"), end.strftime("%H:%M"))
            });
        }
    }
    let mut dropped = Vec::new();
    for change in changes.iter().filter(|c| c["action"] == "cancelled_no_valid_slot") {
        if let Some(session) = PlannedSession::get(db, change["session_id"].as_str().unwrap_or_default())? {
            dropped.push(format!("{} on {}", workout_name(&session), session.planned_start_at.local(&tz).strftime("%a")));
        }
    }
    if !dropped.is_empty() {
        parts.push(format!("{} dropped: no slot left this week that fits your rest days and calendar.", dropped.join(", ")));
    }
    // The drop line already explains the shortfall warning.
    parts.extend(warnings.iter().filter(|w| dropped.is_empty() || !w.contains("sessions feasible")).cloned());
    if emitted.iter().any(|e| e["type"] == "calendar.gym_event_resized") {
        parts.push("Workout compressed to the new slot length.".into());
    }
    if parts.is_empty() && !changes.is_empty() {
        parts.push("Other gym sessions replanned; preparation/leave times updated.".into());
    }
    let queued: i64 = db.query_row("SELECT count(*) FROM calendar_write WHERE status = 'PENDING'", [], |r| r.get(0))?;
    Ok(json!({
        "data": {"calendar_id": provider.calendar_id(), "source": provider.source(), "full_sync": batch.full, "events_received": batch.events.len(),
            "changes": changes, "warnings": warnings, "weeks": affected.iter().map(Date::to_string).collect::<Vec<_>>(),
            "calendar_writes_queued": queued, "remote_events_changed": false},
        "events": emitted,
        "user_message_hint": if parts.is_empty() { Value::Null } else { json!(parts.join(" ")) },
    }))
}

/// A minimal deletion payload, as Google sends for removed events.
pub fn tombstone(id: &str) -> CalendarEvent {
    CalendarEvent {
        id: id.into(), etag: None, title: String::new(), status: "cancelled".into(), start: None, end: None, updated: None, ical_uid: None,
        managed: false, session_id: None, revision: None, raw: json!({"id": id, "status": "cancelled"}).as_object().cloned().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::db::models::{CalendarSyncState, NotificationJob, WorkoutExercise};
    use crate::domain::calendar_writes::{apply_write, pending_writes};
    use crate::domain::planning::SlotSignal;
    use crate::domain::profile::update_profile;
    use crate::domain::scheduling::schedule_week;
    use crate::domain::templates::import_template;
    use crate::domain::workout::start;
    use crate::providers::calendar::SyncBatch;
    use crate::providers::fixture_calendar::FixtureCalendarProvider;
    use crate::time::iso;

    fn berlin() -> TimeZone {
        crate::time::zone("Europe/Berlin").unwrap()
    }

    fn at(day: i8, hour: i8, minute: i8) -> Ts {
        Ts::parse(&format!("2026-10-{day:02}T{hour:02}:{minute:02}:00"), Some(&berlin())).unwrap()
    }

    fn clock(stamp: Ts) -> Value {
        json!({"dateTime": iso(&stamp.local(&berlin())), "timeZone": "Europe/Berlin"})
    }

    /// Sunday 2026-10-11 19:00 Berlin, plus `minutes`.
    fn now(minutes: i64) -> Ts {
        at(11, 19, 0).mins(minutes)
    }

    fn session(db: &Connection, id: &str) -> PlannedSession {
        PlannedSession::get(db, id).unwrap().unwrap()
    }

    fn publish(db: &Connection, provider: &mut FixtureCalendarProvider, now: Ts) {
        for write in pending_writes(db).unwrap() {
            let status = apply_write(db, provider, write["id"].as_str().unwrap(), now, false).unwrap()["status"].clone();
            assert!(status == "APPLIED" || status == "CANCELLED", "{status}");
        }
    }

    fn changes(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// Mon/Wed/Fri "Long" sessions (Wed at 20:00, steered by a crowd signal), published and synced back.
    fn setup() -> (Connection, FixtureCalendarProvider, Vec<String>) {
        let db = crate::domain::workout::tests::db();
        let mut provider = FixtureCalendarProvider::default();
        update_profile(&db, &changes(json!({"earliest_workout_start": "19:00", "latest_workout_finish": "21:10", "minimum_workout_minutes": 70,
            "default_compound_rest_seconds": 180, "default_accessory_rest_seconds": 120}))).unwrap();
        let specs: Vec<Value> = [("bench", 10), ("row", 9), ("fly", 6), ("raise", 5), ("curl", 3), ("triceps", 2)]
            .map(|(key, priority)| {
                let mut name = key.to_string();
                name[..1].make_ascii_uppercase();
                json!({"id": key, "name": name, "role": key, "primary": key == "bench" || key == "row", "warmup_weight": if key == "bench" { json!(50) } else { Value::Null },
                    "target_weight": if key == "bench" { 80 } else { 50 }, "priority": priority})
            })
            .into();
        let template = serde_json::from_value(json!({"id": "long", "name": "Long", "exercises": specs, "set_duration_seconds": 60, "transition_seconds": 90, "source": "demo_fixture"})).unwrap();
        import_template(&db, &template).unwrap();
        sync_calendar(&db, &mut provider, now(0)).unwrap();
        let signal = SlotSignal { start: at(14, 20, 0), crowd: Some(0.0), confidence: 1.0, calendar_convenience: 0.0, adherence: 0.0, preferred_time: 0.0 };
        let result = schedule_week(&db, Date::constant(2026, 10, 12), now(0), None, Some("long"), &[signal]).unwrap();
        commit_upcoming(&db, now(0)).unwrap();
        let ids: Vec<String> = result["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect();
        for id in &ids {
            queue_session_write(&db, &session(&db, id), now(0), false).unwrap();
        }
        publish(&db, &mut provider, now(1));
        sync_calendar(&db, &mut provider, now(2)).unwrap();
        assert!(ids.iter().all(|id| !session(&db, id).user_locked));
        (db, provider, ids)
    }

    #[test]
    fn move_kept_reminders_replaced_other_sessions_stay() {
        let (db, mut provider, ids) = setup();
        let wed = event_id_for(&ids[1]);
        let mut old = session(&db, &ids[1]);
        old.status = SessionStatus::Committed;
        old.save(&db).unwrap();
        schedule_session_jobs(&db, &old, now(0)).unwrap();
        let old_jobs = NotificationJob::select(&db, "WHERE planned_session_id = ?1", [&old.id]).unwrap();
        assert_eq!(old_jobs.len(), 3);
        provider.edit(&wed, json!({"start": clock(at(15, 19, 0)), "end": clock(at(15, 20, 10))})).unwrap();
        let result = sync_calendar(&db, &mut provider, now(3)).unwrap();
        let moved = session(&db, &ids[1]);
        assert!(moved.user_locked);
        assert_eq!((moved.planned_start_at, moved.prep_start_at, moved.leave_home_at, moved.expected_finish_at), (at(15, 19, 0), at(15, 18, 25), at(15, 18, 40), at(15, 20, 10)));
        assert_eq!(moved.crowd_prediction, None); // Wednesday's prediction isn't Thursday evidence.
        assert!(old_jobs.iter().all(|j| NotificationJob::get(&db, &j.id).unwrap().unwrap().status == crate::db::models::JobStatus::Cancelled));
        let new_jobs = NotificationJob::select(&db, "WHERE planned_session_id = ?1 AND status = 'PENDING'", [&moved.id]).unwrap();
        assert_eq!(new_jobs.len(), 3);
        assert!(new_jobs.iter().all(|j| j.due_at.local(&berlin()).date() == Date::constant(2026, 10, 15)));
        // Thu now skips the rest day before Fri; that's the owner's call, so Fri stays as planned.
        let friday = session(&db, &ids[2]);
        assert!(friday.status == SessionStatus::Tentative && !friday.user_locked);
        assert_eq!(result["user_message_hint"], "Long moved to Thu 19:00.");
        assert_eq!(result["data"]["remote_events_changed"], false);
        publish(&db, &mut provider, now(4));
        let result = sync_calendar(&db, &mut provider, now(5)).unwrap();
        assert_eq!(session(&db, &ids[1]).planned_start_at, at(15, 19, 0));
        assert_eq!(result["events"], json!([])); // The own echo is not another edit.
        assert_eq!(provider.get_event(&wed).unwrap().unwrap().start, Some(at(15, 19, 0)));
        let description = provider.raw(&wed).unwrap()["description"].as_str().unwrap();
        assert!(description.contains("Get ready 18:25") && description.contains("1. Bench"), "{description}");
        assert_ne!(provider.get_event(&event_id_for(&ids[2])).unwrap().unwrap().status, "cancelled");
    }

    #[test]
    fn delete_tombstone_not_recreated_valid_replacement_found() {
        let (db, mut provider, ids) = setup();
        update_profile(&db, &changes(json!({"weekdays_allowed": [0, 1, 2, 3, 4, 5, 6]}))).unwrap();
        let deleted = event_id_for(&ids[1]);
        provider.cancel(&deleted).unwrap();
        sync_calendar(&db, &mut provider, now(3)).unwrap();
        let original = session(&db, &ids[1]);
        assert!(original.status == SessionStatus::Cancelled && original.deleted_by_user());
        let live = PlannedSession::select(&db, "WHERE status IN ('COMMITTED', 'TENTATIVE')", []).unwrap();
        assert_eq!(live.len(), 3);
        let replacement = live.iter().find(|s| !ids.contains(&s.id)).unwrap();
        assert_eq!(replacement.planned_start_at.local(&berlin()).weekday(), jiff::civil::Weekday::Sunday);
        assert!(pending_writes(&db).unwrap().iter().all(|w| w["event_id"] != deleted.as_str()));
        publish(&db, &mut provider, now(4));
        sync_calendar(&db, &mut provider, now(5)).unwrap();
        assert_eq!(CalendarEventSnapshot::get(&db, &deleted).unwrap().unwrap().status, "cancelled");
        assert_eq!(provider.get_event(&deleted).unwrap().unwrap().status, "cancelled");
    }

    #[test]
    fn resize_compresses_to_45_minutes() {
        let (db, mut provider, ids) = setup();
        provider.edit(&event_id_for(&ids[1]), json!({"end": clock(at(14, 20, 45))})).unwrap();
        let response = sync_calendar(&db, &mut provider, now(3)).unwrap();
        let resized = session(&db, &ids[1]);
        assert!(resized.user_locked);
        let plan = &resized.workout_plan_json;
        assert!(plan["estimated_duration_seconds"].as_i64().unwrap() <= 45 * 60);
        assert_eq!([&plan["sets"]["bench"], &plan["sets"]["row"], &plan["sets"]["triceps"], &plan["sets"]["curl"]], [&json!(3), &json!(3), &json!(0), &json!(0)]);
        assert!(response["user_message_hint"].is_string());
        let started = start(&db, Some("long"), Some(&resized.id), false, at(14, 20, 0), "start-short").unwrap();
        assert!(Ts::parse(started["data"]["eta"].as_str().unwrap(), None).unwrap() <= at(14, 20, 45));
        let rows = WorkoutExercise::select(&db, "ORDER BY position", []).unwrap();
        let raise = rows.iter().find(|e| e.exercise_id == "raise").unwrap();
        assert_eq!((raise.planned_working_sets, raise.config_json["working_sets"].as_i64()), (2, Some(3)));
        assert_eq!(rows.iter().find(|e| e.exercise_id == "triceps").unwrap().status, crate::db::models::ExerciseStatus::Skipped);
    }

    #[test]
    fn compression_uses_learned_set_duration() {
        let (db, mut provider, ids) = setup();
        let first = at(5, 19, 0);
        db.execute_batch(&format!(
            "INSERT INTO workout_session (id, status, waited_for_equipment_count, template_snapshot) VALUES ('historical', 'PLAN_UPDATED', 0, '{{\"source\": \"test_fixture\"}}');
             INSERT INTO workout_exercise (id, workout_session_id, exercise_id, position, status, planned_working_sets, rep_min, rep_max, target_weight, rest_seconds)
                 VALUES ('historical-bench', 'historical', 'bench', 0, 'COMPLETED', 2, 8, 10, 80, 180);
             INSERT INTO set_log (id, workout_exercise_id, set_number, set_type, weight, reps, logged_at, rest_due_at) VALUES
                 ('s1', 'historical-bench', 1, 'WORKING', 80, 10, '{}', '{}'), ('s2', 'historical-bench', 2, 'WORKING', 80, 10, '{}', NULL);",
            first.db(), first.mins(3).db(), first.mins(5).db()
        ))
        .unwrap();
        provider.edit(&event_id_for(&ids[1]), json!({"end": clock(at(14, 20, 45))})).unwrap();
        sync_calendar(&db, &mut provider, now(3)).unwrap();
        let resized = session(&db, &ids[1]);
        assert_eq!(resized.workout_plan_json["template"]["set_duration_seconds"], 120);
        assert!(resized.workout_plan_json["estimated_duration_seconds"].as_i64().unwrap() <= 2700);
        let started = start(&db, Some("long"), Some(&resized.id), false, at(14, 20, 0), "start-learned").unwrap();
        assert!(Ts::parse(started["data"]["eta"].as_str().unwrap(), None).unwrap() <= at(14, 20, 45));
    }

    #[test]
    fn too_short_edit_preserved_and_explained() {
        let (db, mut provider, ids) = setup();
        provider.edit(&event_id_for(&ids[1]), json!({"end": clock(at(14, 20, 1))})).unwrap();
        let response = sync_calendar(&db, &mut provider, now(3)).unwrap();
        let short = session(&db, &ids[1]);
        assert!(short.planned_end_at == at(14, 20, 1) && short.user_locked);
        assert_eq!(short.workout_plan_json["error"], "WORKOUT_SLOT_TOO_SHORT");
        assert!(response["user_message_hint"].as_str().unwrap().contains("too short"));
    }

    #[test]
    fn all_day_travel_cancels_without_compressing_missed_volume() {
        let (db, mut provider, ids) = setup();
        provider.put(json!({"id": "travel", "summary": "Travel", "start": {"date": "2026-10-12"}, "end": {"date": "2026-10-19"}})).unwrap();
        let result = sync_calendar(&db, &mut provider, now(3)).unwrap();
        assert!(ids.iter().all(|id| session(&db, id).status == SessionStatus::Cancelled));
        assert!(result["user_message_hint"].as_str().unwrap().starts_with("Long on Mon, Long on Wed, Long on Fri dropped"));
        assert!(PlannedSession::select(&db, "WHERE status = 'TENTATIVE'", []).unwrap().is_empty());
    }

    #[test]
    fn invalidated_token_full_sync_keeps_deletion_provenance() {
        let (db, mut provider, ids) = setup();
        provider.forget(&event_id_for(&ids[1]));
        provider.expire_token = true;
        let result = sync_calendar(&db, &mut provider, now(3)).unwrap();
        assert_eq!(result["data"]["full_sync"], true);
        assert!(session(&db, &ids[1]).deleted_by_user());
        assert!(CalendarSyncState::get(&db, 1).unwrap().unwrap().sync_token.is_some());
    }

    #[test]
    fn failed_sync_preserves_plan_and_token() {
        let (db, mut provider, ids) = setup();
        let token = CalendarSyncState::get(&db, 1).unwrap().unwrap().sync_token;
        let prior = session(&db, &ids[1]).planned_start_at;
        provider.fail_sync = true;
        let error = sync_calendar(&db, &mut provider, now(3)).unwrap_err();
        assert!(error.to_string().contains("unavailable"), "{error}");
        assert_eq!(CalendarSyncState::get(&db, 1).unwrap().unwrap().sync_token, token);
        assert_eq!(session(&db, &ids[1]).planned_start_at, prior);
    }

    #[test]
    fn locked_hard_conflicts_are_kept_and_explained() {
        let (db, mut provider, ids) = setup();
        provider.edit(&event_id_for(&ids[0]), json!({"start": clock(at(12, 20, 0)), "end": clock(at(12, 21, 10))})).unwrap();
        provider.edit(&event_id_for(&ids[1]), json!({"start": clock(at(13, 19, 0)), "end": clock(at(13, 20, 10))})).unwrap();
        let result = sync_calendar(&db, &mut provider, now(3)).unwrap();
        assert!(session(&db, &ids[0]).user_locked && session(&db, &ids[1]).user_locked);
        assert_eq!(session(&db, &ids[1]).planned_start_at, at(13, 19, 0));
        assert!(result["user_message_hint"].as_str().unwrap().contains("conflicts with recovery"));
    }

    #[test]
    fn write_retry_after_remote_success_no_duplicate_events() {
        let (db, mut provider, _) = setup();
        let result = schedule_week(&db, Date::constant(2026, 10, 19), now(3), None, Some("long"), &[]).unwrap();
        let next = session(&db, result["sessions"][0]["id"].as_str().unwrap());
        let write = queue_session_write(&db, &next, now(3), false).unwrap().unwrap();
        let count = provider.event_count();
        provider.create_gym_event(&write.body_json).unwrap(); // Crash after Google accepted, before our ack.
        assert_eq!(apply_write(&db, &mut provider, &write.id, now(4), false).unwrap()["status"], "APPLIED");
        assert_eq!(apply_write(&db, &mut provider, &write.id, now(4), false).unwrap()["retried"], true);
        assert_eq!(provider.event_count(), count + 1);
    }

    #[test]
    fn patch_retry_after_remote_success_is_acknowledged() {
        let (db, mut provider, ids) = setup();
        let mut wed = session(&db, &ids[1]);
        for t in [&mut wed.planned_start_at, &mut wed.planned_end_at, &mut wed.prep_start_at, &mut wed.leave_home_at] {
            *t = t.mins(-15);
        }
        wed.expected_finish_at = wed.planned_end_at;
        wed.source_revision += 1;
        wed.save(&db).unwrap();
        let write = queue_session_write(&db, &wed, now(3), false).unwrap().unwrap();
        provider.update_gym_event(&write.event_id, &write.body_json, write.expected_etag.as_deref().unwrap()).unwrap(); // Response lost.
        let changes = provider.changes.len();
        assert_eq!(apply_write(&db, &mut provider, &write.id, now(4), false).unwrap()["status"], "APPLIED");
        assert!(!session(&db, &ids[1]).user_locked);
        assert_eq!(provider.changes.len(), changes);
    }

    /// A provider claiming to be Google that fails the test on any call.
    struct LiveSpy;

    impl CalendarProvider for LiveSpy {
        fn calendar_id(&self) -> &str {
            "primary"
        }
        fn source(&self) -> &'static str {
            "google"
        }
        fn incremental_sync(&mut self, _: Option<&str>) -> Result<SyncBatch> {
            unreachable!()
        }
        fn get_event(&mut self, _: &str) -> Result<Option<CalendarEvent>> {
            unreachable!()
        }
        fn create_gym_event(&mut self, _: &Value) -> Result<CalendarEvent> {
            unreachable!()
        }
        fn update_gym_event(&mut self, _: &str, _: &Value, _: &str) -> Result<CalendarEvent> {
            unreachable!()
        }
        fn delete_gym_event(&mut self, _: &str, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn live_write_gate_runs_before_any_provider_call() {
        let db = crate::domain::workout::tests::db();
        assert_eq!(apply_write(&db, &mut LiveSpy, "anything", now(0), false).unwrap_err().code(), "CALENDAR_WRITES_NOT_APPROVED");
    }

    #[test]
    fn remote_write_race_never_overwrites_manual_move() {
        let (db, mut provider, ids) = setup();
        let target = event_id_for(&ids[1]);
        let mut wed = session(&db, &ids[1]);
        wed.planned_start_at = wed.planned_start_at.mins(15);
        wed.planned_end_at = wed.planned_end_at.mins(15);
        wed.source_revision += 1;
        wed.save(&db).unwrap();
        let write = queue_session_write(&db, &wed, now(3), false).unwrap().unwrap();
        provider.edit(&target, json!({"start": clock(at(15, 19, 0)), "end": clock(at(15, 20, 10))})).unwrap();
        assert_eq!(apply_write(&db, &mut provider, &write.id, now(4), false).unwrap()["status"], "CONFLICT");
        assert_eq!(CalendarWrite::get(&db, &write.id).unwrap().unwrap().status, WriteStatus::Conflict);
        assert_eq!(provider.get_event(&target).unwrap().unwrap().start, Some(at(15, 19, 0)));
        sync_calendar(&db, &mut provider, now(5)).unwrap();
        assert!(session(&db, &ids[1]).user_locked);
        assert_eq!(session(&db, &ids[1]).planned_start_at, at(15, 19, 0));
    }
}
