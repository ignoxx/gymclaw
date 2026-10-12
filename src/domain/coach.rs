//! Telegram coach flows: cards, taps and typed sets (port of gymclaw/services/coach.py). Owner: WP-train.
use crate::db::Connection;
use crate::db::models::WorkoutSession;
use crate::domain::view::CoachView;
use crate::error::Result;
use crate::time::Ts;

/// Typed text during a workout ("10x40"). `handled: false` when it isn't a set; the agent gets it.
pub fn handle_text(db: &Connection, text: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    let _ = (db, text, now, request_id);
    todo!("WP-train")
}

/// A `gc:<action>:<ref>` button tap.
pub fn handle_tap(db: &Connection, payload: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    let _ = (db, payload, now, request_id);
    todo!("WP-train")
}

/// The live countdown reached rest_until: close the rest and show the next set card.
pub fn handle_rest_over(db: &Connection, now: Ts) -> Result<CoachView> {
    let _ = (db, now);
    todo!("WP-train")
}

/// End early: skip what's left, finish, summary card.
pub fn end_workout(db: &Connection, workout: &WorkoutSession, now: Ts, request_id: &str) -> Result<CoachView> {
    let _ = (db, workout, now, request_id);
    todo!("WP-train")
}
