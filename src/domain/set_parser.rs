//! Parse a whole-message set in either order ("9x80", "80kgx9", "9 reps at 80"). Reps first is the default;
//! a "+" marks the weight too ("+10x8", "8x+10"), and a per-side suffix is ignored ("8/side", "8 per side").
//! Bodyweight exercises also take reps alone ("13", "13 reps", "13x"). Timed exercises take a time
//! with its unit ("25 min", "25m", "90s", "1:30"), distance exercises a distance ("2 km", "40 m").
use std::sync::LazyLock;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::db::models::SetType;
use crate::domain::templates::Measure;
use crate::error::Result;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetInput {
    pub weight: f64,
    pub reps: i64,
    #[serde(default = "working")]
    pub set_type: SetType,
    pub rir: Option<f64>,
    /// A timed set's length (reps 0, weight 0).
    #[serde(default)]
    pub seconds: Option<i64>,
    /// A distance set's length (reps 0, weight 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meters: Option<f64>,
}

fn working() -> SetType {
    SetType::Working
}

impl SetInput {
    /// Reps at a weight (0 for bodyweight).
    pub fn reps(weight: f64, reps: i64, set_type: SetType) -> Self {
        SetInput { weight, reps, set_type, rir: None, seconds: None, meters: None }
    }

    /// A timed working set.
    pub fn timed(seconds: i64) -> Self {
        SetInput { seconds: Some(seconds), ..Self::reps(0.0, 0, SetType::Working) }
    }

    /// A distance working set.
    pub fn distance(meters: f64) -> Self {
        SetInput { meters: Some(meters), ..Self::reps(0.0, 0, SetType::Working) }
    }

    /// What the set measured, from which value it carries.
    pub fn measure(&self) -> Measure {
        match (self.seconds, self.meters) {
            (Some(_), _) => Measure::Time,
            (None, Some(_)) => Measure::Distance,
            (None, None) => Measure::Reps,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !self.weight.is_finite()
            || self.weight < 0.0
            || self.reps < 0
            || self.rir.is_some_and(|r| !r.is_finite() || r < 0.0)
        {
            crate::bail!("INVALID_SET", "Weight must be ≥ 0, reps > 0");
        }
        let block = self.reps != 0 || self.set_type != SetType::Working;
        match (self.seconds, self.meters) {
            (None, None) if self.reps == 0 => crate::bail!("INVALID_SET", "Weight must be ≥ 0, reps > 0"),
            (Some(s), None) if !(1..=6 * 3600).contains(&s) || block => {
                crate::bail!("INVALID_SET", "A timed set is one working set of 1 s to 6 h, without reps")
            }
            (None, Some(m)) if !(m.is_finite() && m > 0.0 && m <= 1_000_000.0) || block => {
                crate::bail!("INVALID_SET", "A distance set is one working set of up to 1000 km, without reps")
            }
            (Some(_), Some(_)) => crate::bail!("INVALID_SET", "A set is a time or a distance, not both"),
            _ => Ok(()),
        }
    }
}

const NUMBER: &str = r"\d+(?:[.,]\d+)?";
static PAIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^(\+\s*)?({NUMBER})\s*(kg|reps?)?\s*[x×*]\s*(\+\s*)?({NUMBER})\s*(kg|reps?)?$")).unwrap()
});
static AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^(\d+)\s*reps?\s*(?:at|@)\s*({NUMBER})\s*(?:kg)?$")).unwrap());
static REPS_ONLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)\s*(?:reps?|[x×*])?$").unwrap());
const MAX_PLAUSIBLE_REPS: f64 = 30.0;
static DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^({NUMBER})\s*(h|hrs?|hours?|m|mins?|minutes?|s|secs?|seconds?)$|^(\d+):([0-5]\d)$")).unwrap()
});
static DISTANCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^({NUMBER})\s*(km|kilomet(?:er|re)s?|m|met(?:er|re)s?)$")).unwrap());
/// "/side", "per side", "each side", "a side", "each": reps per side say nothing the exercise doesn't.
static PER_SIDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*(?:/\s*side|(?:per|each|a)\s+side|each)\b").unwrap());

/// A time with its unit in seconds: "25 min", "25m", "1.5h", "90s", "1:30" (minutes:seconds). A bare
/// number is not a time: on a reps card it means reps.
pub fn parse_duration(text: &str) -> Option<i64> {
    let clean = text.trim().to_lowercase();
    let m = DURATION.captures(&clean)?;
    let seconds = if let (Some(minutes), Some(secs)) = (m.get(3), m.get(4)) {
        minutes.as_str().parse::<f64>().ok()? * 60.0 + secs.as_str().parse::<f64>().ok()?
    } else {
        let unit = match &m[2][..1] {
            "h" => 3600.0,
            "m" => 60.0,
            _ => 1.0,
        };
        number(&m[1]) * unit
    };
    Some(seconds.round() as i64).filter(|s| *s > 0)
}

/// A distance in meters: "2 km", "2.5km", "500 meters". A bare "m" is meters only when `m_is_meters`
/// (a distance exercise); elsewhere "25m" is minutes.
pub fn parse_distance(text: &str, m_is_meters: bool) -> Option<f64> {
    let clean = text.trim().to_lowercase();
    let m = DISTANCE.captures(&clean)?;
    let meters = match &m[2] {
        "m" if !m_is_meters => return None,
        unit if unit.starts_with('k') => number(&m[1]) * 1000.0,
        _ => number(&m[1]),
    };
    Some(meters).filter(|m| m.is_finite() && *m > 0.0)
}

/// A time or distance block as a working set, for exercises done for time or distance. On a distance
/// exercise "40 m" is meters, elsewhere minutes.
pub fn parse_block(text: &str, measure: Measure) -> Option<SetInput> {
    let distance = parse_distance(text, measure == Measure::Distance).map(SetInput::distance);
    distance.or_else(|| parse_duration(text).map(SetInput::timed))
}

fn ambiguous<T>() -> Result<T> {
    crate::bail!("AMBIGUOUS_SET", "How many reps and what weight? E.g. 9x80.")
}

fn number(text: &str) -> f64 {
    text.replace(',', ".").parse().unwrap_or(f64::NAN)
}

/// Order is decided by unit words or "+", then rep plausibility (only one side above 30 is the weight), then closeness to `expected_weight`, then reps-first.
pub fn parse_set(text: &str, set_type: SetType, expected_weight: Option<f64>, bodyweight: bool) -> Result<SetInput> {
    let clean = PER_SIDE.replace_all(&text.trim().to_lowercase(), "").trim().to_string();
    let set = |weight: f64, reps: i64| Ok(SetInput::reps(weight, reps, set_type));
    if bodyweight
        && let Some(m) = REPS_ONLY.captures(&clean)
        && let Ok(reps) = m[1].parse::<i64>()
        && reps > 0
    {
        return set(0.0, reps);
    }
    if let Some(m) = AT.captures(&clean) {
        return set(number(&m[2]), m[1].parse().unwrap_or(0));
    }
    let Some(m) = PAIR.captures(&clean) else { return ambiguous() };
    let (a, b) = (number(&m[2]), number(&m[5]));
    // "+" marks the weight like "kg" does: "+10x8" is 8 reps with 10 kg added.
    let unit = |plus: usize, unit: usize| m.get(unit).map(|u| u.as_str()).or(m.get(plus).map(|_| "kg"));
    let (left, right) = (unit(1, 3), unit(4, 6));
    let rep_unit = |u: Option<&str>| u.is_some_and(|u| u.starts_with("rep"));
    if left.is_some() && left == right {
        return ambiguous();
    }
    let integer = |v: f64| v.fract() == 0.0;
    let (weight, reps) = if left == Some("kg") || rep_unit(right) {
        (a, b)
    } else if right == Some("kg") || rep_unit(left) {
        (b, a)
    } else if !integer(a) || !integer(b) {
        if !integer(a) { (a, b) } else { (b, a) }
    } else if (a > MAX_PLAUSIBLE_REPS) != (b > MAX_PLAUSIBLE_REPS) {
        if a > MAX_PLAUSIBLE_REPS { (a, b) } else { (b, a) }
    } else if let Some(expected) = expected_weight.filter(|e| (a - e).abs() != (b - e).abs()) {
        if (a - expected).abs() < (b - expected).abs() { (a, b) } else { (b, a) }
    } else {
        (b, a)
    };
    if !integer(reps) || reps <= 0.0 {
        return ambiguous();
    }
    set(weight, reps as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<SetInput> {
        parse_set(text, SetType::Working, None, false)
    }

    fn set(weight: f64, reps: i64) -> SetInput {
        SetInput::reps(weight, reps, SetType::Working)
    }

    #[test]
    fn either_order_with_units_and_separators() {
        for text in [
            "80x9",
            "80 x 9",
            "80kg x9",
            "9 reps at 80",
            "80 KG × 9",
            "9 reps at 80kg",
            "9x80kg",
            "80kgx9 reps",
            "9x80",
        ] {
            assert_eq!(parse(text).unwrap(), set(80.0, 9), "{text}");
        }
    }

    #[test]
    fn uses_expected_weight_when_order_is_unclear() {
        assert_eq!(parse_set("12x10", SetType::Working, Some(10.0), false).unwrap(), set(10.0, 12));
        assert_eq!(parse("12x10").unwrap(), set(10.0, 12)); // reps first by default
        assert_eq!(parse("8x22,5").unwrap(), set(22.5, 8));
    }

    #[test]
    fn implausible_reps_beat_a_stale_expected_weight() {
        // Expected 10 kg (a stale target) must not turn "49x10" into 49 reps at 10 kg.
        assert_eq!(parse_set("49x10", SetType::Working, Some(10.0), false).unwrap(), set(49.0, 10));
        assert_eq!(parse_set("59x7", SetType::Working, Some(7.0), false).unwrap(), set(59.0, 7));
    }

    #[test]
    fn bodyweight_set_is_reps_alone() {
        for text in ["13", "13 reps", "13x", "13 ×"] {
            assert_eq!(parse_set(text, SetType::Working, None, true).unwrap(), set(0.0, 13), "{text}");
        }
    }

    #[test]
    fn times_need_a_unit() {
        for (text, seconds) in
            [("25 min", 1500), ("25m", 1500), ("25 Minutes", 1500), ("1.5h", 5400), ("90s", 90), ("1:30", 90)]
        {
            assert_eq!(parse_duration(text), Some(seconds), "{text}");
            assert!(parse(text).is_err(), "{text} is not reps × weight");
        }
        for text in ["25", "10x40", "0 min", "1:75", "25 min 30"] {
            assert_eq!(parse_duration(text), None, "{text}");
        }
    }

    #[test]
    fn added_weight_and_per_side() {
        for text in ["+10x8", "8x+10", "8 x +10 kg", "8/side x 10kg", "+10 x 8 per side"] {
            assert_eq!(parse(text).unwrap(), set(10.0, 8), "{text}");
        }
        for text in ["8/side", "8 per side", "8 each"] {
            assert_eq!(parse_set(text, SetType::Working, None, true).unwrap(), set(0.0, 8), "{text}");
        }
    }

    #[test]
    fn distances_and_the_meaning_of_m() {
        for (text, meters) in [("2 km", 2000.0), ("2.5km", 2500.0), ("500 meters", 500.0), ("40 metres", 40.0)] {
            assert_eq!(parse_block(text, Measure::Time), Some(SetInput::distance(meters)), "{text}");
        }
        // A bare "m" is meters on a distance exercise, minutes on a timed one.
        assert_eq!(parse_block("40 m", Measure::Distance), Some(SetInput::distance(40.0)));
        assert_eq!(parse_block("40 m", Measure::Time), Some(SetInput::timed(2400)));
        assert_eq!(parse_block("40", Measure::Distance), None);
    }

    #[test]
    fn ambiguous_set_rejected() {
        for text in ["80x9 then 80x10", "80", "9 reps", "-80x9", "80x9?", "80/9", "80kg x 9kg", "+8x+10"] {
            let error = parse(text).unwrap_err();
            assert!(error.to_string().contains("How many reps"), "{text}: {error}");
        }
    }
}
