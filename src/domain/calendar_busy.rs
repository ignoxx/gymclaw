//! Busy time the planner avoids (port of calendar_busy.py): availability blocks, the personal ICS feed,
//! and gym-calendar events that aren't GymClaw's own, with recurring masters expanded locally within a
//! bounded horizon. Owner: lead (planning).
use std::collections::HashSet;

use jiff::{SignedDuration, ToSpan};
use serde_json::Value;

use crate::db::Connection;
use crate::db::models::{CalendarEventSnapshot, CalendarSyncState, PlannedSession};
use crate::domain::planning::{Interval, overlaps};
use crate::domain::{availability, personal_calendar};
use crate::error::Result;
use crate::providers::calendar::parse_time;
use crate::providers::recurrence::{self, When};
use crate::time::{Ts, zone};

pub fn busy_intervals(db: &Connection, start: Ts, end: Ts) -> Result<Vec<Interval>> {
    let calendar_zone = CalendarSyncState::get(db, 1)?.map(|s| s.timezone).unwrap_or_else(|| "Europe/Berlin".into());
    let snapshots = CalendarEventSnapshot::select(db, "ORDER BY rowid", [])?;
    let own_ids: HashSet<String> = PlannedSession::select(db, "WHERE calendar_event_id IS NOT NULL", [])?.into_iter().filter_map(|s| s.calendar_event_id).collect();
    let mut result = availability::blocks(db, start, end)?;
    result.extend(personal_calendar::blocks(db, start, end)?);
    let mut recurring = Vec::new();
    for event in &snapshots {
        let raw = &event.raw_json;
        if event.status == "cancelled" || own_ids.contains(&event.calendar_event_id) || raw.get("transparency").and_then(Value::as_str) == Some("transparent") {
            continue;
        }
        if raw.get("recurrence").is_none_or(|r| r.as_array().is_none_or(Vec::is_empty)) {
            let interval = Interval { start: event.start_at, end: event.end_at };
            if overlaps(start, end, &interval) {
                result.push(interval);
            }
        } else {
            recurring.push(event);
        }
    }
    if !recurring.is_empty() {
        result.extend(recurring_busy(&snapshots, &recurring, &calendar_zone, start, end, result.len())?);
    }
    Ok(result)
}

/// Occurrences of recurring masters overlapping [start, end), minus instances that have their own
/// (moved or cancelled) exception event. All-day masters expand as dates in their zone.
fn recurring_busy(snapshots: &[CalendarEventSnapshot], masters: &[&CalendarEventSnapshot], calendar_zone: &str, start: Ts, end: Ts, already: usize) -> Result<Vec<Interval>> {
    let mut excluded = HashSet::new();
    for e in snapshots {
        if let (Some(master), Some(original)) = (e.raw_json.get("recurringEventId").and_then(Value::as_str), e.raw_json.get("originalStartTime")) {
            excluded.insert((master.to_string(), parse_time(original, calendar_zone)?));
        }
    }
    let mut result = Vec::new();
    for event in masters {
        let raw = &event.raw_json;
        let lines: Vec<&str> = raw["recurrence"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let tz = zone(raw.get("start").and_then(|s| s.get("timeZone")).and_then(Value::as_str).unwrap_or(calendar_zone))?;
        let all_day = raw.get("start").is_some_and(|s| s.get("date").is_some());
        let local_start = event.start_at.local(&tz);
        let duration = event.end_at - event.start_at;
        let (dtstart, lower, upper, days) = if all_day {
            let days = (event.end_at.local(&tz).date() - local_start.date()).get_days() as i64;
            let lower = (start - SignedDuration::from_hours(24 * (days + 1))).local(&tz).date();
            (When::Date(local_start.date()), When::Date(lower), When::Date(end.local(&tz).date()), days)
        } else {
            let wall = |t: Ts| When::Zoned(t.local(&tz).datetime(), tz.clone());
            (wall(event.start_at), wall(start - duration - SignedDuration::from_hours(24)), wall(end), 0)
        };
        for occurrence in recurrence::occurrences(&lines, &dtstart, &lower, &upper, 5000)? {
            let occurrence_start = occurrence.to_ts(&tz)?;
            let occurrence_end = if all_day {
                Ts(occurrence.civil().to_zoned(tz.clone())?.checked_add(days.days())?.timestamp())
            } else {
                occurrence_start + duration
            };
            if excluded.contains(&(event.calendar_event_id.clone(), occurrence_start)) {
                continue;
            }
            let interval = Interval { start: occurrence_start, end: occurrence_end };
            if overlaps(start, end, &interval) {
                result.push(interval);
            }
            if already + result.len() > 5000 {
                crate::bail!("CALENDAR_RECURRENCE_TOO_LARGE", "Too many recurring blockers; simplify calendar recurrence");
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::Json;

    fn ts(text: &str) -> Ts {
        Ts::parse(text, None).unwrap()
    }

    fn event(db: &Connection, id: &str, status: &str, start: &str, end: &str, raw: Value) {
        CalendarEventSnapshot {
            calendar_event_id: id.into(), ical_uid: None, etag: None, title: "t".into(), start_at: ts(start), end_at: ts(end), status: status.into(),
            updated_at_remote: None, gymclaw_managed: false, gymclaw_session_id: None, gymclaw_plan_revision: None, user_locked: false,
            last_seen_at: ts(start), raw_json: Json(raw),
        }
        .insert(db)
        .unwrap();
    }

    // Expected values are the Python oracle's output: wall-clock times across the DST change, the
    // cancelled Wed 28 instance dropped, and all-day spans following local midnight.
    #[test]
    fn recurring_masters_match_python() {
        let db = crate::domain::workout::tests::db();
        event(&db, "m", "confirmed", "2026-10-05T16:00:00Z", "2026-10-05T17:00:00Z",
            json!({"start": {"dateTime": "2026-10-05T18:00:00+02:00", "timeZone": "Europe/Berlin"}, "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO,WE"]}));
        event(&db, "x", "cancelled", "2026-10-28T17:00:00Z", "2026-10-28T18:00:00Z",
            json!({"recurringEventId": "m", "originalStartTime": {"dateTime": "2026-10-28T18:00:00+01:00", "timeZone": "Europe/Berlin"}}));
        event(&db, "d", "confirmed", "2026-10-02T22:00:00Z", "2026-10-04T22:00:00Z", json!({"start": {"date": "2026-10-03"}, "recurrence": ["RRULE:FREQ=WEEKLY;COUNT=5"]}));
        let busy: Vec<(String, String)> = busy_intervals(&db, ts("2026-10-23T00:00:00Z"), ts("2026-11-03T00:00:00Z")).unwrap().into_iter().map(|i| (i.start.iso(), i.end.iso())).collect();
        let expected = [
            ("2026-10-26T17:00:00+00:00", "2026-10-26T18:00:00+00:00"),
            ("2026-11-02T17:00:00+00:00", "2026-11-02T18:00:00+00:00"),
            ("2026-10-23T22:00:00+00:00", "2026-10-25T23:00:00+00:00"),
            ("2026-10-30T23:00:00+00:00", "2026-11-01T23:00:00+00:00"),
        ];
        assert_eq!(busy, expected.map(|(a, b)| (a.to_string(), b.to_string())));
    }
}
