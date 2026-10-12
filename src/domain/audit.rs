//! Post-workout analysis and progression (port of gymclaw/services/audit.py). Owner: WP-train.
use serde_json::Value;

use crate::db::Connection;
use crate::error::Result;
use crate::time::Ts;

/// Finish a WORKOUT_COMPLETE workout: progression, PLAN_UPDATED. Mutation result envelope.
pub fn finish(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    let _ = (db, workout_id, now, request_id);
    todo!("WP-train")
}
