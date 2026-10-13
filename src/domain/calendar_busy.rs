//! Busy time the planner avoids (port of calendar_busy.py): availability blocks, the personal ICS feed,
//! and gym-calendar events that aren't GymClaw's own, with recurring masters expanded locally within a
//! bounded horizon. Owner: lead (planning).
use std::collections::HashSet;

use serde_json::Value;

use crate::db::Connection;
use crate::db::models::{CalendarEventSnapshot, CalendarSyncState, PlannedSession};
use crate::domain::planning::{Interval, overlaps};
use crate::domain::{availability, personal_calendar};
use crate::error::Result;
use crate::time::Ts;

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
/// (moved or cancelled) exception event.
fn recurring_busy(snapshots: &[CalendarEventSnapshot], masters: &[&CalendarEventSnapshot], calendar_zone: &str, start: Ts, end: Ts, already: usize) -> Result<Vec<Interval>> {
    let _ = (snapshots, masters, calendar_zone, start, end, already);
    todo!("expand with crate::providers recurrence once WP-providers lands")
}
