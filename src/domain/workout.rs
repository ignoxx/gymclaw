//! Persisted workout state machine (port of gymclaw/services/workout.py). Owner: WP-train.
//! Signatures below are the contract other modules call; keep them.
use serde_json::Value;

use crate::db::Connection;
use crate::db::models::{ExerciseStatus, SetLog, SetType, WorkoutExercise, WorkoutSession};
use crate::error::Result;
use crate::time::Ts;

/// Exercise states that still need a decision before the workout can finish.
pub const UNRESOLVED: [ExerciseStatus; 3] = [ExerciseStatus::Pending, ExerciseStatus::Active, ExerciseStatus::Deferred];

/// The workout with an execution snapshot; `now` before its last action fails with TIME_REVERSED.
pub fn load_workout(db: &Connection, workout_id: &str, now: Option<Ts>) -> Result<WorkoutSession> {
    let _ = (db, workout_id, now);
    todo!("WP-train")
}

/// Exercises of a workout in position order.
pub fn exercises(db: &Connection, workout_id: &str) -> Result<Vec<WorkoutExercise>> {
    let _ = (db, workout_id);
    todo!("WP-train")
}

/// Sets of one workout exercise in order, optionally only one set type.
pub fn logs(db: &Connection, exercise_row_id: &str, kind: Option<SetType>) -> Result<Vec<SetLog>> {
    let _ = (db, exercise_row_id, kind);
    todo!("WP-train")
}

/// Average seconds per set from history, else `default_seconds`.
pub fn historical_set_duration(db: &Connection, default_seconds: f64) -> Result<f64> {
    let _ = (db, default_seconds);
    todo!("WP-train")
}

/// Workout state as the old `workout current` JSON (status, active_exercise, queue, rest_job, …).
pub fn current(db: &Connection, workout_id: &str, now: Ts) -> Result<Value> {
    let _ = (db, workout_id, now);
    todo!("WP-train")
}

/// A due REST job: rest is over. Mutation result envelope; `user_message_hint` is the message to send.
pub fn rest_complete(db: &Connection, job_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    let _ = (db, job_id, now, request_id);
    todo!("WP-train")
}
