//! Periodic domain work the scheduler runs (was `runtime watch|weekly|poll-crowd`). Owner: lead.
//! `watch`, `weekly` and `poll_crowd` run in one transaction and queue owner messages as PENDING
//! `notification_delivery` rows; the runtime's outbox sends them afterwards. `publish` commits per write.
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::CalendarSyncState;
use crate::domain::calendar::{auth_alert, sync_calendar};
use crate::domain::calendar_writes::{apply_write, pending_writes};
use crate::domain::{crowd, personal_calendar, weekly};
use crate::error::Result;
use crate::providers::crowd::{from_environment, run_command};
use crate::providers::google::google_provider;
use crate::providers::http::{Net, Transport};
use crate::runtime::outbox::{prepare_event_message, runtime_authority};
use crate::time::Ts;

fn google_state(db: &Connection) -> Result<CalendarSyncState> {
    match CalendarSyncState::get(db, 1)? {
        Some(state) if state.source == "google" => Ok(state),
        _ => crate::bail!("LIVE_CALENDAR_REQUIRED", "Watcher requires dedicated Google calendar auth"),
    }
}

/// Every 60 s: calendar/feed sync, deterministic upkeep, decision events and crowd health alerts.
pub fn watch(db: &Connection, recipient: &str, now: Ts) -> Result<Value> {
    let state = CalendarSyncState::get(db, 1)?.filter(|s| s.source == "google");
    let net = Net::new()?;
    let sync = || match &state {
        Some(state) => sync_calendar(db, &mut google_provider(db, Some(&state.calendar_id), &net)?, now),
        None => Ok(json!({"data": {"weeks": []}, "events": []})),
    };
    watch_with(db, recipient, now, sync, &net)
}

/// `watch` with the calendar sync passed in (tests use the fixture calendar).
fn watch_with(db: &Connection, recipient: &str, now: Ts, sync: impl FnOnce() -> Result<Value>, net: &dyn Transport) -> Result<Value> {
    let authority = runtime_authority(db)?;
    // A calendar outage must not stall the rest of the tick: reminders, nudges, deliveries.
    db.execute_batch("SAVEPOINT calendar_sync")?;
    let (_synced, calendar_error) = match sync() {
        Ok(synced) => {
            db.execute_batch("RELEASE calendar_sync")?;
            (synced, None)
        }
        Err(error) if error.code().starts_with("CALENDAR_") => {
            db.execute_batch("ROLLBACK TO calendar_sync; RELEASE calendar_sync")?;
            (json!({"data": {"weeks": []}, "events": [], "user_message_hint": null}), Some(error.code().to_string()))
        }
        Err(error) => return Err(error),
    };
    // CALENDAR_UNAVAILABLE is transient and says nothing about auth.
    if calendar_error.as_deref() != Some("CALENDAR_UNAVAILABLE")
        && let Some((event_id, message)) = auth_alert(db, calendar_error.is_none(), now)?
    {
        prepare_event_message(db, &event_id, &message, now, recipient, &[])?;
    }
    let _personal = personal_calendar::refresh(db, now, false, net, false)?;
    let rolling = match &authority.template_id {
        Some(template) => weekly::rolling_plan(db, template, now)?,
        None => json!({}),
    };
    weekly::no_show_nudges(db, now)?;
    if authority.crowd_polling_enabled
        && let Some((event_id, message)) = crowd::polling_alert(db, now)?
    {
        prepare_event_message(db, &event_id, &message, now, recipient, &[])?;
    }
    Ok(json!({"calendar_error": calendar_error, "rolling": rolling, "briefed": false}))
}

/// Sunday 19:00 local: sync, draft an empty next week, audit this week and queue the agent event.
pub fn weekly(db: &Connection, now: Ts) -> Result<Value> {
    let authority = runtime_authority(db)?;
    let Some(template) = authority.template_id.clone() else { crate::bail!("TEMPLATE_REQUIRED", "No activated template or confirmed split to plan with") };
    let net = Net::new()?;
    if let Some(state) = CalendarSyncState::get(db, 1)?.filter(|s| s.source == "google") {
        sync_calendar(db, &mut google_provider(db, Some(&state.calendar_id), &net)?, now)?;
    }
    personal_calendar::refresh(db, now, true, &net, false)?;
    weekly::weekly_plan(db, &template, now)
}

/// Every 15 min when crowd polling is enabled: read the gym's live count. A failed read is returned,
/// not raised, so its failure health commits (the stale alert in `watch` tells the owner).
pub fn poll_crowd(db: &Connection, _now: Ts) -> Result<Value> {
    let net = Net::new()?;
    let provider = from_environment(&net, &run_command)?;
    // Retrieval time is taken after the response, like Python.
    crowd::poll(db, provider.as_ref(), None)
}

/// Push PENDING calendar writes to Google (when calendar writes are enabled). Syncs first so manual
/// edits are seen before any write; each write commits on its own and a conflict stops the batch.
pub fn publish(db: &Connection, now: Ts) -> Result<Value> {
    let state = google_state(db)?;
    let net = Net::new()?;
    let tx = db.unchecked_transaction()?;
    sync_calendar(&tx, &mut google_provider(&tx, Some(&state.calendar_id), &net)?, now)?;
    tx.commit()?;
    let mut results = Vec::new();
    for _ in 0..50 {
        let tx = db.unchecked_transaction()?;
        let Some(write) = pending_writes(&tx)?.into_iter().next() else { break };
        let result = apply_write(&tx, &mut google_provider(&tx, Some(&state.calendar_id), &net)?, write["id"].as_str().unwrap_or_default(), now, true)?;
        tx.commit()?;
        let conflict = result["status"] == "CONFLICT";
        results.push(result);
        if conflict {
            break; // Don't keep changing the calendar while an owner edit needs a sync.
        }
    }
    let remaining = pending_writes(db)?.len();
    let conflict = results.iter().any(|r| r["status"] == "CONFLICT");
    Ok(json!({"published": remaining == 0 && !conflict, "results": results, "remaining_writes": remaining}))
}

#[cfg(test)]
mod tests {
    use jiff::civil::Date;
    use super::*;
    use crate::db::models::PlannedSession;
    use crate::domain::calendar_writes::queue_session_write;
    use crate::domain::profile::update_profile;
    use crate::domain::replanning::commit_upcoming;
    use crate::domain::scheduling::schedule_week;
    use crate::domain::templates::{Template, import_template};
    use crate::error::Error;
    use crate::providers::fixture_calendar::FixtureCalendarProvider;
    use crate::time::iso;

    fn sunday(minutes: i64) -> Ts {
        Ts::parse("2026-10-11T17:00:00+00:00", None).unwrap().mins(minutes)
    }

    /// The "short" template planned for next week, the runtime enabled without calendar-write authority.
    fn setup(changes: Value) -> (Connection, FixtureCalendarProvider) {
        let db = crate::domain::workout::tests::db();
        let template: Template = serde_json::from_value(json!({"id": "short", "name": "Short",
            "exercises": [{"id": "bench", "name": "Bench", "role": "press", "primary": true, "working_sets": 2, "target_weight": 80, "warmup_weight": 50}]})).unwrap();
        import_template(&db, &template).unwrap();
        update_profile(&db, changes.as_object().unwrap()).unwrap();
        schedule_week(&db, Date::constant(2026, 10, 12), sunday(0), None, Some("short"), &[]).unwrap();
        commit_upcoming(&db, sunday(0)).unwrap();
        db.execute("INSERT INTO runtime_settings (id, profile, recipient, project_root, python_path, template_id, enabled, calendar_writes_enabled, crowd_polling_enabled)
            VALUES (1, 'gymclaw', '123', '', '', 'short', 1, 0, 0)", []).unwrap();
        (db, FixtureCalendarProvider::new("unit-calendar", "Europe/Berlin"))
    }

    fn sent(db: &Connection) -> Vec<String> {
        let mut statement = db.prepare("SELECT message FROM notification_delivery WHERE recipient = '123' ORDER BY created_at, rowid").unwrap();
        statement.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }

    fn tick(db: &Connection, now: Ts, calendar: Option<&mut FixtureCalendarProvider>) -> Value {
        let sync = || match calendar {
            Some(calendar) => sync_calendar(db, calendar, now),
            None => Err(Error::domain("CALENDAR_AUTH_REQUIRED", "Google credentials expired or invalid; run `gymclaw calendar-auth` again")),
        };
        watch_with(db, "123", now, sync, &Net::new().unwrap()).unwrap()
    }

    #[test]
    fn calendar_resize_wakes_agent_once_without_fixed_briefing() {
        let (db, mut calendar) = setup(json!({"weekly_min_sessions": 1, "weekly_target_sessions": 1, "weekly_max_sessions": 1}));
        let mut session = PlannedSession::select(&db, "", []).unwrap().remove(0);
        let write = queue_session_write(&db, &session, sunday(0), false).unwrap().unwrap();
        apply_write(&db, &mut calendar, &write.id, sunday(0), false).unwrap();
        session = PlannedSession::get(&db, &session.id).unwrap().unwrap();
        let end = session.planned_end_at.mins(-25);
        let tz = crate::time::zone("Europe/Berlin").unwrap();
        calendar.edit(session.calendar_event_id.as_deref().unwrap(), json!({"end": {"dateTime": iso(&end.local(&tz)), "timeZone": "Europe/Berlin"}})).unwrap();
        tick(&db, sunday(1), Some(&mut calendar));
        assert!(sent(&db).is_empty());
        let events = crate::domain::events::wake_pending(&db).unwrap();
        assert!(events.iter().any(|e| e["type"] == "planning.replan_required"));
        tick(&db, sunday(2), Some(&mut calendar));
        assert_eq!(crate::domain::events::wake_pending(&db).unwrap(), events);
        assert!(sent(&db).is_empty());
    }

    /// Expired Google auth used to fail the whole tick: no reminders, no nudges, no word to the owner.
    #[test]
    fn watch_survives_lost_calendar_auth_and_alerts_once() {
        let (db, mut calendar) = setup(json!({}));
        assert_eq!(tick(&db, sunday(1), None)["calendar_error"], "CALENDAR_AUTH_REQUIRED");
        tick(&db, sunday(2), None);
        let messages = sent(&db);
        assert!(messages.len() == 1 && messages[0].contains("Lost access to Google Calendar"), "{messages:?}");
        tick(&db, sunday(3), Some(&mut calendar));
        let messages = sent(&db);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1], "✅ Google Calendar is connected again.");
    }
}
