//! Gym crowd: polling, personal crowd model, felt-crowd feedback (port of crowd.py). Owner: lead.
use serde_json::Value;

use crate::db::Connection;
use crate::error::Result;
use crate::time::Ts;

/// Felt-crowd ratings the owner can pick, in order.
pub const LABELS: [(&str, &str); 4] = [("EMPTY", "Empty"), ("FINE", "A few"), ("BUSY", "Busy"), ("PACKED", "Packed")];

/// Owner-facing word for a 0–1 crowd score.
pub fn feel(score: Option<f64>) -> Option<&'static str> {
    let _ = score;
    todo!("lead")
}

/// How busy it felt after a workout. Mutation result envelope.
pub fn record_feedback(db: &Connection, workout_id: &str, rating: &str, now: Ts, request_id: &str) -> Result<Value> {
    let _ = (db, workout_id, rating, now, request_id);
    todo!("lead")
}

/// Crowd polling health for the dashboard and alerts.
pub fn polling_health(db: &Connection, now: Ts) -> Result<Value> {
    let _ = (db, now);
    todo!("lead")
}
