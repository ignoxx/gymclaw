//! Travel/sickness blocks (port of gymclaw/services/availability.py). Owner: lead (planning).
use crate::db::Connection;
use crate::db::models::PlannedSession;
use crate::domain::planning::Interval;
use crate::error::Result;
use crate::time::Ts;

/// Active blocks overlapping [start, end).
pub fn blocks(db: &Connection, start: Ts, end: Ts) -> Result<Vec<Interval>> {
    let _ = (db, start, end);
    todo!("lead")
}

pub fn session_blocked(db: &Connection, session: &PlannedSession) -> Result<bool> {
    let _ = (db, session);
    todo!("lead")
}
