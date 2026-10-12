//! Double progression: complete the prescribed volume at the target weight, or hold.
use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct PerformanceSet {
    pub weight: f64,
    pub reps: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProgressionDecision {
    pub exercise_id: String,
    pub previous_weight: f64,
    pub next_weight: f64,
    pub progressed: bool,
    pub reason: &'static str,
}

/// Weights compared and added in tenths of grams, so 82.5 + 2.5 is exactly 85 (Python used Decimal).
fn milli(weight: f64) -> i64 {
    (weight * 10_000.0).round() as i64
}

pub fn double_progression(exercise_id: &str, target_weight: f64, rep_max: i64, required_sets: usize, increment: f64, performance: &[PerformanceSet]) -> ProgressionDecision {
    let complete = performance.len() == required_sets;
    let achieved = complete && performance.iter().all(|s| milli(s.weight) == milli(target_weight) && s.reps >= rep_max);
    let next_weight = if achieved { (milli(target_weight) + milli(increment)) as f64 / 10_000.0 } else { target_weight };
    let reason = if achieved {
        "threshold_achieved"
    } else if !complete {
        "incomplete_volume"
    } else {
        "hold_target_not_met"
    };
    ProgressionDecision { exercise_id: exercise_id.into(), previous_weight: target_weight, next_weight, progressed: achieved, reason }
}
