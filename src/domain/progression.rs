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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(target: f64, rep_max: i64, sets: usize, increment: f64, performance: &[(f64, i64)]) -> ProgressionDecision {
        let performance: Vec<PerformanceSet> = performance.iter().map(|&(weight, reps)| PerformanceSet { weight, reps }).collect();
        double_progression("bench", target, rep_max, sets, increment, &performance)
    }

    #[test]
    fn double_progression_needs_every_set_at_target_and_rep_max() {
        let cases: [(&[(f64, i64)], bool); 5] = [
            (&[(80.0, 10), (80.0, 10), (80.0, 10)], true),
            (&[(80.0, 10), (80.0, 9), (80.0, 10)], false),
            (&[(80.0, 10), (80.0, 10)], false),
            (&[(80.0, 10), (75.0, 10), (80.0, 10)], false),
            (&[(80.0, 12), (80.0, 11), (80.0, 10)], true),
        ];
        for (performance, progressed) in cases {
            let result = run(80.0, 10, 3, 2.5, performance);
            assert_eq!(result.progressed, progressed, "{performance:?}");
            assert_eq!(result.next_weight, if progressed { 82.5 } else { 80.0 });
        }
    }

    #[test]
    fn decimal_increment() {
        assert_eq!(run(0.2, 10, 1, 0.1, &[(0.2, 10)]).next_weight, 0.3);
    }
}
