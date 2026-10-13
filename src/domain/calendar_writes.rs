//! Durable calendar write outbox (port of calendar_writes.py). Only GymClaw-owned events; live writes
//! need the explicit calendar-write authority. Owner: lead (planning).
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::db::models::{CalendarEventSnapshot, CalendarWrite, OnboardingState, PlannedSession, SessionStatus, WriteAction, WriteStatus};
use crate::db::{Connection, Json, new_id};
use crate::domain::calendar::{persist_snapshot, reconcile_event, tombstone};
use crate::domain::core::emit;
use crate::domain::replanning::replan_weeks;
use crate::providers::calendar::{CalendarProvider, WRITE_CONFLICT};
use crate::providers::google::bind_calendar;
use crate::domain::profile::get_profile;
use crate::error::Result;
use crate::time::{Ts, iso};

/// Stable Google event id for a session (hex is valid base32hex), so retries never duplicate.
pub fn event_id_for(session_id: &str) -> String {
    hex::encode(Sha256::digest(format!("gymclaw:{session_id}")))
}

/// "Busy · ~13 people checked in (Fridays 18:00, 2 wk)"; honest when there is no data yet.
pub fn describe_crowd(forecast: Option<&Value>, start: &jiff::Zoned) -> String {
    let Some(forecast) = forecast.filter(|f| f.as_object().is_some_and(|o| !o.is_empty())) else {
        return "not enough data yet".into();
    };
    let mut parts: Vec<String> = forecast.get("feel").and_then(Value::as_str).filter(|f| !f.is_empty()).map(String::from).into_iter().collect();
    if let Some(count) = forecast.get("count").filter(|c| !c.is_null()) {
        let basis = if forecast["basis"] == "weekday_hour" {
            format!("{}s {}:00, {} wk", start.strftime("%A"), start.strftime("%H"), forecast["days"])
        } else {
            format!("{}:00 on other days", start.strftime("%H"))
        };
        parts.push(format!("~{count} people checked in ({basis})"));
    }
    parts.join(" · ")
}

/// Google Calendar event colours (peacock, tangerine, basil, grape, banana, tomato, blueberry), one per
/// template in the owner's split so Push/Pull/Legs are recognisable at a glance.
const TEMPLATE_COLORS: [&str; 7] = ["7", "6", "10", "3", "5", "11", "9"];

/// Everything the owner needs without asking: plan, crowd, timings, gym location (calendar apps use it
/// for travel time / time to leave) and a colour per workout type.
pub fn event_body(db: &Connection, session: &PlannedSession) -> Result<Value> {
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let name = session.plan("template").and_then(|t| t.get("name")).and_then(Value::as_str).unwrap_or("Workout");
    let mut crowd = describe_crowd(session.plan("crowd_forecast"), &session.planned_start_at.local(&tz));
    if session.plan("crowd_source").and_then(Value::as_str) == Some("demo_fixture") {
        crowd += " (demo fixture)";
    }
    let home = session.expected_finish_at.mins(profile.commute_home_minutes);
    let hm = |t: Ts| t.local(&tz).strftime("%H:%M").to_string();
    let mut lines = vec![
        format!("Expected crowd: {crowd}"),
        format!("Get ready {} · Leave {} · Home ~{}", hm(session.prep_start_at), hm(session.leave_home_at), hm(home)),
    ];
    if session.workout_template_id.is_some() {
        lines.push(String::new());
        lines.push("Plan:".into());
        lines.extend(crate::domain::preview::plan_lines(db, session)?);
    }
    lines.push(String::new());
    lines.push("Managed by GymClaw. Move or resize freely; it adapts.".into());
    let split = OnboardingState::get(db, 1)?.map(|o| o.split_json.0).unwrap_or_default();
    let color = session.workout_template_id.as_ref().and_then(|t| split.iter().position(|s| s == t)).map(|i| TEMPLATE_COLORS[i % TEMPLATE_COLORS.len()]);
    let mut body = Map::new();
    body.insert("id".into(), json!(session.calendar_event_id.clone().unwrap_or_else(|| event_id_for(&session.id))));
    body.insert("summary".into(), json!(format!("🏋️ {name}")));
    body.insert("description".into(), json!(lines.join("\n")));
    if let Some(address) = profile.gym_address.as_ref().filter(|a| !a.is_empty()) {
        body.insert("location".into(), json!(address));
    }
    if let Some(color) = color {
        body.insert("colorId".into(), json!(color));
    }
    body.insert("start".into(), json!({"dateTime": iso(&session.planned_start_at.local(&tz)), "timeZone": profile.timezone}));
    body.insert("end".into(), json!({"dateTime": iso(&session.planned_end_at.local(&tz)), "timeZone": profile.timezone}));
    body.insert("status".into(), json!(if session.status == SessionStatus::Committed { "confirmed" } else { "tentative" }));
    body.insert("extendedProperties".into(), json!({"private": {"gymclawManaged": "true", "gymclawSessionId": session.id, "gymclawPlanRevision": session.source_revision.to_string()}}));
    body.insert("reminders".into(), json!({"useDefault": false}));
    Ok(Value::Object(body))
}

fn cancel_pending(db: &Connection, session_id: &str, now: Ts) -> Result<()> {
    for mut old in CalendarWrite::select(db, "WHERE planned_session_id = ?1 AND status = 'PENDING' ORDER BY rowid", [session_id])? {
        old.status = WriteStatus::Cancelled;
        old.handled_at = Some(now);
        old.update(db)?;
    }
    Ok(())
}

/// Queue the calendar change for a session's current revision (create, update or delete). Locked
/// sessions still publish: their times come from the owner, never the planner. The session must be saved.
pub fn queue_session_write(db: &Connection, session: &PlannedSession, now: Ts, metadata_only: bool) -> Result<Option<CalendarWrite>> {
    use SessionStatus::*;
    if matches!(session.status, Started | Completed | Skipped | Missed) {
        return Ok(None);
    }
    let action = match (session.status, &session.calendar_event_id) {
        (Cancelled, None) => {
            cancel_pending(db, &session.id, now)?;
            return Ok(None);
        }
        (Cancelled, Some(_)) => WriteAction::Delete,
        (_, Some(_)) => WriteAction::Update,
        (_, None) => WriteAction::Create,
    };
    let snapshot = match &session.calendar_event_id {
        Some(id) => CalendarEventSnapshot::get(db, id)?,
        None => None,
    };
    if action != WriteAction::Create {
        let owned = snapshot.as_ref().is_some_and(|s| s.etag.as_deref().is_some_and(|e| !e.is_empty()) && s.gymclaw_managed && s.gymclaw_session_id.as_deref() == Some(&session.id));
        if !owned {
            emit(db, "calendar.write_blocked", now, json!({"session_id": session.id, "reason": "ownership_or_etag_missing"}))?;
            return Ok(None);
        }
    }
    let mut body = if action == WriteAction::Delete { json!({}) } else { event_body(db, session)? };
    if metadata_only {
        let full = body.as_object().cloned().unwrap_or_default();
        body = Value::Object(["summary", "description", "location", "colorId", "extendedProperties"].iter().filter_map(|k| full.get(*k).map(|v| (k.to_string(), v.clone()))).collect());
    }
    if let Some(existing) = CalendarWrite::first(db, "WHERE planned_session_id = ?1 AND revision = ?2 AND action = ?3", (&session.id, session.source_revision, action))? {
        return Ok(Some(existing));
    }
    cancel_pending(db, &session.id, now)?;
    let write = CalendarWrite {
        id: new_id(), planned_session_id: session.id.clone(), revision: session.source_revision, action,
        event_id: session.calendar_event_id.clone().unwrap_or_else(|| event_id_for(&session.id)),
        expected_etag: snapshot.and_then(|s| s.etag), body_json: Json(body), status: WriteStatus::Pending, created_at: now, handled_at: None,
    };
    write.insert(db)?;
    emit(db, "calendar.write_queued", now, json!({"write_id": write.id, "session_id": session.id, "action": action}))?;
    Ok(Some(write))
}

pub fn pending_writes(db: &Connection) -> Result<Vec<Value>> {
    Ok(CalendarWrite::select(db, "WHERE status = 'PENDING' ORDER BY created_at, id", [])?
        .into_iter()
        .map(|w| json!({"id": w.id, "session_id": w.planned_session_id, "action": w.action, "event_id": w.event_id, "revision": w.revision, "body": w.body_json}))
        .collect())
}

/// Send one queued write. Live Google writes need `allow_writes` (the calendar-write authority). A
/// write for an older revision is cancelled; ownership/etag conflicts end as CONFLICT plus a sync request.
pub fn apply_write(db: &Connection, provider: &mut dyn CalendarProvider, write_id: &str, now: Ts, allow_writes: bool) -> Result<Value> {
    if provider.source() == "google" && !allow_writes {
        crate::bail!("CALENDAR_WRITES_NOT_APPROVED", "Live writes require explicit --allow-writes; inspect pending-writes first");
    }
    bind_calendar(db, provider.calendar_id(), provider.source())?;
    let Some(mut write) = CalendarWrite::get(db, write_id)? else { crate::bail!("CALENDAR_WRITE_NOT_FOUND", "Unknown queued calendar write") };
    if write.status != WriteStatus::Pending {
        return Ok(json!({"write_id": write.id, "status": write.status, "retried": true}));
    }
    let Some(mut session) = PlannedSession::get(db, &write.planned_session_id)? else { crate::bail!("SESSION_NOT_FOUND", "Write for unknown session") };
    // A calendar edit after queueing bumps the revision, so a stale write never overwrites it.
    if write.revision != session.source_revision {
        write.status = WriteStatus::Cancelled;
        write.handled_at = Some(now);
        write.update(db)?;
        return Ok(json!({"write_id": write.id, "status": write.status}));
    }
    match send(db, provider, &write, &mut session, now) {
        Ok(()) => {
            write.status = WriteStatus::Applied;
            write.handled_at = Some(now);
            write.update(db)?;
            let event = emit(db, "calendar.write_applied", now, json!({"write_id": write.id, "session_id": session.id, "action": write.action, "event_id": write.event_id}))?;
            Ok(json!({"write_id": write.id, "status": write.status, "event": {"id": event.id, "type": event.r#type}}))
        }
        Err(error) if matches!(error.code(), WRITE_CONFLICT | "CALENDAR_NOT_OWNED" | "CALENDAR_EVENT_DELETED") => {
            write.status = WriteStatus::Conflict;
            write.handled_at = Some(now);
            write.update(db)?;
            let event = emit(db, "calendar.sync_required", now, json!({"write_id": write.id, "reason": error.code()}))?;
            Ok(json!({"write_id": write.id, "status": write.status, "error": error.code(), "event": {"id": event.id, "type": event.r#type}}))
        }
        Err(error) => Err(error),
    }
}

fn send(db: &Connection, provider: &mut dyn CalendarProvider, write: &CalendarWrite, session: &mut PlannedSession, now: Ts) -> Result<()> {
    let etag = write.expected_etag.as_deref().unwrap_or_default();
    let remote = match write.action {
        WriteAction::Create => provider.create_gym_event(&write.body_json)?,
        WriteAction::Update => provider.update_gym_event(&write.event_id, &write.body_json, etag)?,
        WriteAction::Delete => {
            provider.delete_gym_event(&write.event_id, etag, &session.id)?;
            persist_snapshot(db, &tombstone(&write.event_id), now, false)?;
            return Ok(());
        }
    };
    if !remote.managed || remote.session_id.as_deref() != Some(&session.id) || remote.id != write.event_id {
        crate::bail!("CALENDAR_NOT_OWNED", "Provider returned unexpected event ownership; sync required");
    }
    session.calendar_event_id = Some(remote.id.clone());
    session.save(db)?;
    // A retry may find an event created earlier and since edited by the owner.
    if remote.start != Some(session.planned_start_at) || remote.end != Some(session.planned_end_at) {
        let (affected, _) = reconcile_event(db, &remote, now)?;
        replan_weeks(db, &affected, now, false)?;
    } else {
        persist_snapshot(db, &remote, now, false)?;
        if write.action == WriteAction::Create && remote.revision != Some(session.source_revision) {
            queue_session_write(db, session, now, false)?;
        }
    }
    Ok(())
}
