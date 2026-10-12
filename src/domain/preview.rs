//! Session previews: plan lines and the exercise grid image (port of gymclaw/services/preview.py). Owner: WP-train.
use crate::db::Connection;
use crate::db::models::{PlannedSession, SetLog};
use crate::error::Result;

/// Most recent working set of a template exercise id, for "last time" hints.
pub fn last_set(db: &Connection, exercise_id: &str) -> Result<Option<SetLog>> {
    let _ = (db, exercise_id);
    todo!("WP-train")
}

/// One line per planned exercise ("1. Bench 3×8–10 @ 80 kg"), used in calendar event bodies.
pub fn plan_lines(db: &Connection, session: &PlannedSession) -> Result<Vec<String>> {
    let _ = (db, session);
    todo!("WP-train")
}
