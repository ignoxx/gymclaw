//! Double progression: complete the prescribed volume at the target weight, or hold. A heavier weight the
//! owner managed (reps within `REP_SLACK` of the range's bottom) becomes the new baseline, so the next
//! first set offers what they ended on, not the ramp-up set before it.
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

/// Reps a set may fall short of `rep_min` and still count as managed: 6 × 55 on an 8–12 range keeps
/// 55 to build reps on; 4 × 55 is too heavy.
const REP_SLACK: i64 = 2;

/// Weights compared and added in tenths of grams, so 82.5 + 2.5 is exactly 85.
fn milli(weight: f64) -> i64 {
    (weight * 10_000.0).round() as i64
}

pub fn double_progression(
    exercise_id: &str,
    target_weight: f64,
    rep_min: i64,
    rep_max: i64,
    required_sets: usize,
    increment: f64,
    performance: &[PerformanceSet],
) -> ProgressionDecision {
    let complete = performance.len() == required_sets;
    let managed = performance.iter().filter(|s| s.reps >= rep_min - REP_SLACK).map(|s| milli(s.weight)).max();
    let base = managed.filter(|&m| complete && m > milli(target_weight)).unwrap_or(milli(target_weight));
    let achieved = complete && performance.iter().all(|s| milli(s.weight) >= base && s.reps >= rep_max);
    let next_weight = (if achieved { base + milli(increment) } else { base }) as f64 / 10_000.0;
    let reason = if achieved {
        "threshold_achieved"
    } else if !complete {
        "incomplete_volume"
    } else if base > milli(target_weight) {
        "hold_heavier_than_target"
    } else {
        "hold_target_not_met"
    };
    ProgressionDecision {
        exercise_id: exercise_id.into(),
        previous_weight: target_weight,
        next_weight,
        progressed: achieved,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(target: f64, rep_max: i64, sets: usize, increment: f64, performance: &[(f64, i64)]) -> ProgressionDecision {
        let performance: Vec<PerformanceSet> =
            performance.iter().map(|&(weight, reps)| PerformanceSet { weight, reps }).collect();
        double_progression("bench", target, 8, rep_max, sets, increment, &performance)
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
    fn the_heaviest_managed_set_becomes_the_baseline() {
        // 2026-11-13: machine row 50 then 55 three sessions running, and the card kept offering 50.
        assert_eq!(run(50.0, 12, 2, 2.5, &[(50.0, 8), (55.0, 8)]).next_weight, 55.0);
        // Two reps short of 8 still counts; four short is too heavy.
        assert_eq!(run(50.0, 12, 2, 2.5, &[(50.0, 10), (55.0, 6)]).next_weight, 55.0);
        assert_eq!(run(50.0, 12, 2, 2.5, &[(50.0, 10), (55.0, 4)]).next_weight, 50.0);
        // Progress only once every set is at the new weight and at rep max.
        assert_eq!(run(50.0, 12, 2, 2.5, &[(50.0, 12), (55.0, 12)]).next_weight, 55.0);
        assert_eq!(run(55.0, 12, 2, 2.5, &[(55.0, 12), (55.0, 12)]).next_weight, 57.5);
    }

    #[test]
    fn decimal_increment() {
        assert_eq!(run(0.2, 10, 1, 0.1, &[(0.2, 10)]).next_weight, 0.3);
    }
}
