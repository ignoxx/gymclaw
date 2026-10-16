//! Personal calendar as read-only blockers (port of personal_calendar.py). Fetched by the watcher, never
//! written. Connect stores the feed URL locally; each refresh replaces the expanded busy intervals and
//! reports conflicts in weeks whose blockers changed. A failed fetch keeps the last known blockers so an
//! outage never frees up time that is actually busy. Owner: lead (planning).
use std::collections::BTreeSet;

use jiff::SignedDuration;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{PersonalBusyInterval, PersonalCalendarState};
use crate::domain::calendar::week_of;
use crate::domain::core::emit;
use crate::domain::planning::Interval;
use crate::domain::profile::get_profile;
use crate::domain::replanning::{commit_upcoming, inspect_weeks};
use crate::error::Result;
use crate::providers::http::Transport;
use crate::providers::ics;
use crate::time::Ts;

const REFRESH_EVERY: SignedDuration = SignedDuration::from_mins(5);
const HORIZON: SignedDuration = SignedDuration::from_hours(35 * 24);
const LOOKBACK: SignedDuration = SignedDuration::from_hours(24);

pub fn blocks(db: &Connection, start: Ts, end: Ts) -> Result<Vec<Interval>> {
    Ok(PersonalBusyInterval::select(db, "WHERE start_at < ?1 AND end_at > ?2 ORDER BY rowid", (end, start))?
        .into_iter()
        .map(|row| Interval { start: row.start_at, end: row.end_at })
        .collect())
}

/// Connection status without the URL (a capability secret): only its host.
pub fn status(db: &Connection) -> Result<Value> {
    let Some(state) = PersonalCalendarState::get(db, 1)? else { return Ok(json!({"connected": false})) };
    Ok(json!({"connected": true, "host": host(&state.url), "last_success_at": state.last_success_at, "last_error_code": state.last_error_code,
        "blockers": stored(db)?.len()}))
}

/// Hostname of an https URL, like Python's `urlsplit(url).hostname`.
fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    authority.split(':').next().unwrap_or_default().to_lowercase()
}

/// Store the feed and fetch it now; a bad URL or failing feed is an error, nothing half-connected.
pub fn connect(db: &Connection, url: &str, now: Ts, transport: &dyn Transport) -> Result<Value> {
    let url = ics::normalize_url(url)?;
    let state = PersonalCalendarState { id: 1, url, fetched_at: None, last_success_at: None, last_error_code: None };
    match PersonalCalendarState::get(db, 1)? {
        Some(old) => PersonalCalendarState { last_success_at: old.last_success_at, ..state }.update(db)?,
        None => state.insert(db)?,
    }
    refresh(db, now, true, transport, true)
}

pub fn disconnect(db: &Connection, now: Ts) -> Result<Value> {
    let old = stored(db)?;
    db.execute("DELETE FROM personal_busy_interval", [])?;
    db.execute("DELETE FROM personal_calendar_state WHERE id = 1", [])?;
    let mut result = inspect_changes(db, &old, &BTreeSet::new(), now)?;
    result["connected"] = json!(false);
    Ok(result)
}

/// Throttled; a no-op when not connected or fetched recently. Errors are recorded, not raised, unless `strict`.
pub fn refresh(db: &Connection, now: Ts, force: bool, transport: &dyn Transport, strict: bool) -> Result<Value> {
    let quiet = |extra: Value| {
        let mut base = json!({"refreshed": false, "changes": [], "warnings": [], "events": [], "user_message_hint": null});
        base.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        base
    };
    let Some(mut state) = PersonalCalendarState::get(db, 1)? else { return Ok(quiet(json!({}))) };
    if !force && state.fetched_at.is_some_and(|at| now - at < REFRESH_EVERY) {
        return Ok(quiet(json!({})));
    }
    state.fetched_at = Some(now);
    let tz = get_profile(db)?.tz();
    let fetched = ics::fetch(&state.url, transport).and_then(|data| ics::busy_intervals(&data, now - LOOKBACK, now + HORIZON, &tz));
    let fresh: BTreeSet<(Ts, Ts)> = match fetched {
        Ok(intervals) => intervals.into_iter().map(|i| (i.start, i.end)).collect(),
        Err(error) => {
            state.last_error_code = Some(error.code().to_string());
            state.update(db)?;
            if strict {
                return Err(error);
            }
            return Ok(quiet(json!({"error_code": error.code()})));
        }
    };
    state.last_success_at = Some(now);
    state.last_error_code = None;
    state.update(db)?;
    let old = stored(db)?;
    if fresh != old {
        db.execute("DELETE FROM personal_busy_interval", [])?;
        for &(start_at, end_at) in &fresh {
            PersonalBusyInterval { id: None, start_at, end_at }.insert(db)?;
        }
    }
    let mut result = inspect_changes(db, &old, &fresh, now)?;
    result["refreshed"] = json!(true);
    result["blockers"] = json!(fresh.len());
    Ok(result)
}

fn stored(db: &Connection) -> Result<BTreeSet<(Ts, Ts)>> {
    Ok(PersonalBusyInterval::select(db, "", [])?.into_iter().map(|r| (r.start_at, r.end_at)).collect())
}

fn inspect_changes(db: &Connection, old: &BTreeSet<(Ts, Ts)>, new: &BTreeSet<(Ts, Ts)>, now: Ts) -> Result<Value> {
    let tz = get_profile(db)?.tz();
    // Only future changes matter; a blocker sliding out of the lookback window is not news.
    let changed: Vec<_> = old.symmetric_difference(new).filter(|(_, end)| *end > now).collect();
    if changed.is_empty() {
        return Ok(json!({"changes": [], "warnings": [], "events": [], "user_message_hint": null}));
    }
    let mut weeks = BTreeSet::new();
    for &&(start, end) in &changed {
        let (mut week, last) = (week_of(start.max(now), &tz), week_of(end.min(now + HORIZON), &tz));
        while week <= last {
            weeks.insert(week);
            week = week.checked_add(jiff::Span::new().days(7))?;
        }
    }
    let replanned = inspect_weeks(db, &weeks, now)?;
    commit_upcoming(db, now)?;
    let event = emit(db, "calendar.personal_blockers_changed", now, json!({"added": new.difference(old).count(), "removed": old.difference(new).count(),
        "changes": replanned["changed"], "warnings": replanned["warnings"]}))?;
    let warnings: Vec<&str> = replanned["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let hint = if !warnings.is_empty() {
        json!(format!("Personal calendar changed. {}", warnings.join(" ")))
    } else {
        Value::Null
    };
    Ok(json!({"changes": replanned["changed"], "warnings": replanned["warnings"], "events": [{"id": event.id, "type": event.r#type}], "user_message_hint": hint}))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::db::models::PlannedSession;
    use crate::domain::calendar_busy::busy_intervals;
    use crate::domain::templates::{Template, import_template};
    use crate::providers::http::{Request, Response};

    /// Stand-in for the iCloud published feed; counts requests.
    struct Feed {
        status: u16,
        body: Vec<u8>,
        calls: Cell<usize>,
    }

    impl Transport for Feed {
        fn send(&self, _: &Request) -> Result<Response> {
            self.calls.set(self.calls.get() + 1);
            Ok(Response { status: self.status, body: self.body.clone() })
        }
    }

    fn feed(status: u16, body: String) -> Feed {
        Feed { status, body: body.into_bytes(), calls: Cell::new(0) }
    }

    #[test]
    fn personal_blocker_reports_conflict_and_outage_keeps_blockers() {
        let db = crate::domain::workout::tests::db();
        let sunday = Ts::parse("2026-10-11T17:00:00+00:00", None).unwrap();
        let template: Template = serde_json::from_value(json!({"id": "short", "name": "Short",
            "exercises": [{"id": "bench", "name": "Bench", "role": "press", "primary": true, "working_sets": 2, "target_weight": 80, "warmup_weight": 50}]})).unwrap();
        import_template(&db, &template).unwrap();
        let plan = crate::domain::weekly::weekly_plan(&db, "short", sunday).unwrap();
        let id = plan["plan"]["sessions"].as_array().unwrap().last().unwrap()["id"].as_str().unwrap().to_string();
        let original = PlannedSession::get(&db, &id).unwrap().unwrap().planned_start_at;
        let utc = |t: Ts| t.0.strftime("%Y%m%dT%H%M%SZ").to_string();
        let (busy_start, busy_end) = (original.mins(-60), original.mins(120));
        let ics = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{0}\r\nDTSTART:{0}\r\nDTEND:{1}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", utc(busy_start), utc(busy_end));
        let up = feed(200, ics);

        let result = connect(&db, "webcal://p42-caldav.icloud.com/published/2/secret", sunday, &up).unwrap();
        assert!(result["user_message_hint"].as_str().unwrap().starts_with("Personal calendar changed"));
        assert_eq!(result["changes"], json!([]));
        assert!(!crate::domain::events::wake_pending(&db).unwrap().is_empty());
        assert_eq!(PlannedSession::get(&db, &id).unwrap().unwrap().planned_start_at, original);
        assert!(busy_intervals(&db, sunday, sunday.mins(8 * 24 * 60)).unwrap().iter().any(|b| b.start == busy_start));
        assert!(!status(&db).unwrap().to_string().contains("secret"));

        // Throttled: the 60 s watcher doesn't hit the feed every minute.
        assert_eq!(refresh(&db, sunday.mins(1), false, &up, false).unwrap()["refreshed"], false);
        assert_eq!(up.calls.get(), 1);

        // An outage records the error but keeps the last known blockers.
        let down = feed(503, String::new());
        assert_eq!(refresh(&db, sunday.mins(10), false, &down, false).unwrap()["error_code"], "PERSONAL_CALENDAR_UNAVAILABLE");
        assert_eq!(PersonalCalendarState::get(&db, 1).unwrap().unwrap().last_error_code.as_deref(), Some("PERSONAL_CALENDAR_UNAVAILABLE"));
        assert_eq!(status(&db).unwrap()["blockers"], 1);

        // An unchanged feed is not news.
        let again = refresh(&db, sunday.mins(20), false, &up, false).unwrap();
        assert!(again["refreshed"] == true && again["events"] == json!([]));

        disconnect(&db, sunday.mins(30)).unwrap();
        assert_eq!(status(&db).unwrap()["connected"], false);
        assert!(blocks(&db, sunday, sunday.mins(8 * 24 * 60)).unwrap().is_empty());
    }
}
