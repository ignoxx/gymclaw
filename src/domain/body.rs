//! Body weight log (port of gymclaw/services/body.py). Owner: WP-train.
use crate::db::Connection;
use crate::error::Result;
use crate::time::Ts;

/// Weekly briefing line about weight trend, or None without data.
pub fn briefing_line(db: &Connection, now: Ts) -> Result<Option<String>> {
    let _ = (db, now);
    todo!("WP-train")
}
