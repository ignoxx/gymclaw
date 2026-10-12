//! Mid-workout changes: swap, switch, relabel, skip (port of gymclaw/services/adaptation.py). Owner: WP-train.
use serde_json::Value;

use crate::db::Connection;
use crate::error::Result;
use crate::time::Ts;

/// Skip one workout exercise (row id) with a reason. Mutation result envelope.
pub fn skip_exercise(db: &Connection, workout_id: &str, exercise_row_id: &str, reason: &str, now: Ts, request_id: &str) -> Result<Value> {
    let _ = (db, workout_id, exercise_row_id, reason, now, request_id);
    todo!("WP-train")
}
