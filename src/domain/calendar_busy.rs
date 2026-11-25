//! Busy time the planner avoids: availability blocks and the personal ICS feed.
use crate::db::Connection;
use crate::domain::planning::Interval;
use crate::domain::{availability, personal_calendar};
use crate::error::Result;
use crate::time::Ts;

pub fn busy_intervals(db: &Connection, start: Ts, end: Ts) -> Result<Vec<Interval>> {
    let mut result = availability::blocks(db, start, end)?;
    result.extend(personal_calendar::blocks(db, start, end)?);
    Ok(result)
}
