//! Personal calendar as read-only blockers (port of personal_calendar.py). Fetched by the watcher, never
//! written. Connect stores the feed URL locally; each refresh replaces the expanded busy intervals and
//! replans only the weeks whose blockers changed. A failed fetch keeps the last known blockers so an
//! outage never frees up time that is actually busy. Owner: lead (planning).
use crate::db::Connection;
use crate::db::models::PersonalBusyInterval;
use crate::domain::planning::Interval;
use crate::error::Result;
use crate::time::Ts;

pub fn blocks(db: &Connection, start: Ts, end: Ts) -> Result<Vec<Interval>> {
    Ok(PersonalBusyInterval::select(db, "WHERE start_at < ?1 AND end_at > ?2 ORDER BY rowid", (end, start))?
        .into_iter()
        .map(|row| Interval { start: row.start_at, end: row.end_at })
        .collect())
}
