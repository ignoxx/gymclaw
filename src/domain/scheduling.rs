//! Plan a whole week against persisted profile/calendar state.
use jiff::civil::Date;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{AgentEvent, PlannedSession, SessionStatus};
use crate::domain::calendar::week_of;
use crate::domain::calendar_busy::busy_intervals;
use crate::domain::compression::compress_template;
use crate::domain::core::emit;
use crate::domain::crowd::planning_signals;
use crate::domain::planning::{Inputs, Interval, SlotSignal, plan_week};
use crate::domain::profile::{Profile, get_profile};
use crate::domain::templates::get_template;
use crate::domain::workout::historical_set_duration;
use crate::error::Result;
use crate::time::Ts;

/// A planned session as tools and the dashboard show it.
pub fn session_data(row: &PlannedSession) -> Value {
    json!({
        "id": row.id, "week_id": row.week_id, "status": row.status,
        "start": row.planned_start_at, "end": row.planned_end_at, "prep_start": row.prep_start_at,
        "leave_home": row.leave_home_at, "user_locked": row.user_locked,
        "workout_template_id": row.workout_template_id, "workout_plan": row.workout_plan_json, "follows_session_id": row.follows_session_id,
    })
}

pub fn interval(session: &PlannedSession) -> Interval {
    Interval { start: session.planned_start_at, end: session.planned_end_at }
}

/// What the planner works around in `week`: busy time from Sunday before to Tuesday after (calendar,
/// personal feed, availability blocks) and owner-deleted slots, where it never puts a session back, as
/// blockers; every active session for recovery and the weekly count; skipped ones for the count only.
/// No crowd signals.
pub fn planner_inputs(db: &Connection, week_start: Date, profile: &Profile) -> Result<Inputs> {
    let start = Ts(week_start.checked_sub(jiff::Span::new().days(1))?.to_zoned(profile.tz())?.timestamp());
    let end = Ts(week_start.checked_add(jiff::Span::new().days(8))?.to_zoned(profile.tz())?.timestamp());
    let mut inputs = Inputs { busy: busy_intervals(db, start, end)?, ..Inputs::default() };
    for session in PlannedSession::select(db, "ORDER BY rowid", [])? {
        match session.status {
            status if status.is_active() => inputs.existing.push(interval(&session)),
            SessionStatus::Skipped => inputs.skipped.push(interval(&session)),
            SessionStatus::Cancelled if session.deleted_by_user() => inputs.busy.push(interval(&session)),
            _ => {}
        }
    }
    Ok(inputs)
}

/// Fill a week with new sessions around the existing ones. Idempotent per `request_id` (the same week and
/// template). `fixture_signals` replace the crowd model's signals (tests).
pub fn schedule_week(
    db: &Connection,
    week_start: Date,
    now: Ts,
    request_id: Option<&str>,
    template_id: Option<&str>,
    fixture_signals: &[SlotSignal],
) -> Result<Value> {
    let request = json!({"week_start": week_start.to_string(), "template_id": template_id});
    if let Some(key) = request_id
        && let Some(previous) = AgentEvent::first(db, "WHERE correlation_id = ?1", [key])?
    {
        // Older events also stored the planner's settings; only the week and template make the request.
        let same = |field: &str| previous.payload_json["request"].get(field).unwrap_or(&Value::Null) == &request[field];
        if !same("week_start") || !same("template_id") {
            crate::bail!("IDEMPOTENCY_CONFLICT", "Request ID already used with different planning input");
        }
        return Ok(previous.payload_json["result"].clone());
    }
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let active = PlannedSession::select(
        db,
        "WHERE status IN ('TENTATIVE', 'COMMITTED', 'STARTED', 'COMPLETED') ORDER BY rowid",
        [],
    )?;
    let mut inputs = planner_inputs(db, week_start, &profile)?;
    inputs.signals =
        if fixture_signals.is_empty() { planning_signals(db, week_start, now)? } else { fixture_signals.to_vec() };
    let plan = plan_week(&profile, week_start, &inputs, now)?;
    let template = match template_id {
        Some(id) => {
            let mut t = get_template(db, id)?;
            t.set_duration_seconds = historical_set_duration(db, t.set_duration_seconds as f64)?.ceil() as i64;
            Some(t)
        }
        None => None,
    };
    let mut created = Vec::new();
    for slot in &plan.sessions {
        let mut row = PlannedSession::new(week_start.to_string(), template_id.map(String::from), false);
        if let Some(t) = &template {
            row.workout_plan_json.0 = compress_template(t, &profile, (slot.end - slot.start).as_secs())?;
        }
        row.planned_start_at = slot.start;
        row.planned_end_at = slot.end;
        row.prep_start_at = slot.prep_start;
        row.leave_home_at = slot.leave_home;
        row.insert(db)?;
        created.push(row);
    }
    let mut week_rows: Vec<&PlannedSession> =
        active.iter().filter(|s| week_of(s.planned_start_at, &tz) == week_start).chain(&created).collect();
    week_rows.sort_by_key(|s| s.planned_start_at);
    let result = json!({
        "week_start": week_start.to_string(),
        "sessions": week_rows.iter().map(|s| session_data(s)).collect::<Vec<_>>(),
        "created_count": created.len(),
        "minimum_met": plan.minimum_met,
        "shortfall": plan.shortfall,
        "source": if fixture_signals.is_empty() { "local_state" } else { "fixture" },
        "calendar_events_created": false,
        "choices": plan.sessions,
    });
    let mut event = emit(db, "planning.week_planned", now, json!({"request": request, "result": result}))?;
    if let Some(key) = request_id {
        event.correlation_id = Some(key.into());
        event.update(db)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Json;
    use crate::db::models::{CalendarEventSnapshot, SessionStatus};

    fn ts(text: &str) -> Ts {
        Ts::parse(text, None).unwrap()
    }

    #[test]
    fn calendar_snapshot_and_locked_session_preserved() {
        let db = crate::db::test::db();
        get_profile(&db).unwrap();
        let (start, end) = (ts("2026-10-15T19:00:00+02:00"), ts("2026-10-15T20:10:00+02:00"));
        CalendarEventSnapshot {
            calendar_event_id: "travel".into(),
            etag: None,
            title: "Travel".into(),
            start_at: ts("2026-10-12T00:00:00+02:00"),
            end_at: ts("2026-10-14T00:00:00+02:00"),
            status: "confirmed".into(),
            gymclaw_managed: false,
            gymclaw_session_id: None,
            user_locked: false,
            raw_json: Json(json!({})),
        }
        .insert(&db)
        .unwrap();
        let mut locked = PlannedSession::new("2026-10-12".into(), None, true);
        locked.id = "locked".into();
        locked.status = SessionStatus::Tentative;
        (locked.planned_start_at, locked.planned_end_at, locked.prep_start_at, locked.leave_home_at) =
            (start, end, start, start);
        locked.insert(&db).unwrap();
        let result =
            schedule_week(&db, jiff::civil::date(2026, 10, 12), ts("2026-10-11T12:00:00+02:00"), None, None, &[])
                .unwrap();
        let sessions = result["sessions"].as_array().unwrap();
        assert_eq!((sessions.len(), sessions[0]["id"].as_str()), (1, Some("locked")));
        assert_eq!(PlannedSession::get(&db, "locked").unwrap().unwrap().planned_start_at, start);
    }

    /// A skipped session counts toward its week: planning the week again doesn't make it up.
    #[test]
    fn skipped_slot_is_not_refilled() {
        let db = crate::db::test::with_short_template();
        let (week, now) = (jiff::civil::date(2026, 10, 12), ts("2026-10-11T12:00:00+02:00"));
        let first = schedule_week(&db, week, now, None, Some("short"), &[]).unwrap();
        let mut skipped = PlannedSession::get(&db, first["sessions"][0]["id"].as_str().unwrap()).unwrap().unwrap();
        skipped.status = SessionStatus::Skipped;
        skipped.update(&db).unwrap();
        assert_eq!(schedule_week(&db, week, now, None, Some("short"), &[]).unwrap()["created_count"], 0);
    }

    /// Request keys stored before the request shrank to week and template still replay.
    #[test]
    fn replays_a_key_stored_with_planner_settings() {
        let db = crate::db::test::with_short_template();
        let request = json!({"week_start": "2026-10-12", "template_id": "short", "fixture": {"busy": [], "signals": [],
            "config": {"step_minutes": 15}}});
        let mut event = emit(
            &db,
            "planning.week_planned",
            ts("2026-10-11T12:00:00Z"),
            json!({"request": request, "result": {"old": 1}}),
        )
        .unwrap();
        event.correlation_id = Some("rolling:old".into());
        event.update(&db).unwrap();
        let week = jiff::civil::date(2026, 10, 12);
        let now = ts("2026-10-11T12:00:00Z");
        assert_eq!(schedule_week(&db, week, now, Some("rolling:old"), Some("short"), &[]).unwrap(), json!({"old": 1}));
        assert_eq!(
            schedule_week(&db, week, now, Some("rolling:old"), None, &[]).unwrap_err().code(),
            "IDEMPOTENCY_CONFLICT"
        );
    }
}
