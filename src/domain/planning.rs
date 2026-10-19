//! Pure week planner (port of planning.py): deterministic slots; maximize session count first, then
//! aggregate score. Recovery uses local calendar dates, not elapsed 24-hour periods. The search keeps
//! the best slot per day, then considers all weekday combinations (at most 128 per week). No provider
//! data means unknown crowd, never fabricated occupancy. Owner: lead.
use jiff::SignedDuration;
use jiff::civil::Date;
use serde::Serialize;

use crate::domain::profile::Profile;
use crate::error::Result;
use crate::time::Ts;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interval {
    pub start: Ts,
    pub end: Ts,
}

/// Half-open intervals [start, end) overlap.
pub fn overlaps(start: Ts, end: Ts, other: &Interval) -> bool {
    start < other.end && other.start < end
}

/// Planner inputs for one slot start. Missing signals score neutral.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SlotSignal {
    pub start: Ts,
    pub crowd: Option<f64>,
    pub confidence: f64,
    pub calendar_convenience: f64,
    pub adherence: f64,
    pub preferred_time: f64,
}

#[derive(Clone, Debug)]
pub struct PlannerConfig {
    pub step_minutes: i64,
    pub adherence_weight: f64,
    /// Below crowd_preference_weight (default 1): habits break ties, a quiet slot still beats a busy habitual one.
    pub preferred_time_weight: f64,
    pub uncertainty_weight: f64,
    pub shortened_duration_penalty: f64,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        PlannerConfig { step_minutes: 15, adherence_weight: 1.0, preferred_time_weight: 0.4, uncertainty_weight: 0.5, shortened_duration_penalty: 1.0 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScoreParts {
    pub crowd: f64,
    pub calendar: f64,
    pub adherence: f64,
    pub preferred_time: f64,
    pub uncertainty: f64,
    pub duration: f64,
}

impl ScoreParts {
    fn total(&self) -> f64 {
        self.crowd + self.calendar + self.adherence + self.preferred_time + self.uncertainty + self.duration
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub start: Ts,
    pub end: Ts,
    pub prep_start: Ts,
    pub leave_home: Ts,
    pub home_at: Ts,
    pub crowd: Option<f64>,
    pub confidence: f64,
    pub score: f64,
    pub score_parts: Option<ScoreParts>,
}

impl Candidate {
    /// An owner-chosen slot off the planner grid: times only, no score.
    pub fn manual(start: Ts, end: Ts, profile: &Profile) -> Candidate {
        Candidate {
            start,
            end,
            prep_start: start.mins(-(profile.prep_minutes + profile.commute_to_gym_minutes)),
            leave_home: start.mins(-profile.commute_to_gym_minutes),
            home_at: end.mins(profile.commute_home_minutes),
            crowd: None,
            confidence: 0.0,
            score: 0.0,
            score_parts: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub week_start: Date,
    pub sessions: Vec<Candidate>,
    pub existing_count: i64,
    pub minimum_met: bool,
    pub shortfall: i64,
}

#[derive(Default)]
pub struct Inputs<'a> {
    pub busy: &'a [Interval],
    pub unavailable: &'a [Interval],
    pub existing: &'a [Interval],
    pub signals: &'a [SlotSignal],
}

pub fn monday(day: Date) -> bool {
    day.weekday() == jiff::civil::Weekday::Monday
}

pub fn weekday(day: Date) -> i64 {
    day.weekday().to_monday_zero_offset() as i64
}

pub fn recovery_ok(start: Ts, end: Ts, other: &Interval, profile: &Profile) -> bool {
    let tz = profile.tz();
    let (first, last) = (start.local(&tz).date(), end.local(&tz).date());
    let (other_first, other_last) = (other.start.local(&tz).date(), other.end.local(&tz).date());
    let required = profile.minimum_full_rest_days_between_sessions + 1;
    let days = |a: Date, b: Date| (a - b).get_days() as i64;
    days(first, other_last) >= required || days(other_first, last) >= required
}

pub fn generate_candidates(profile: &Profile, week_start: Date, inputs: &Inputs, now: Ts, config: &PlannerConfig) -> Result<Vec<Candidate>> {
    if !monday(week_start) {
        crate::bail!("INVALID_WEEK", "week_start must be Monday");
    }
    let tz = profile.tz();
    let blockers: Vec<&Interval> = inputs.busy.iter().chain(inputs.unavailable).collect();
    let mut durations = vec![profile.preferred_workout_minutes, profile.minimum_workout_minutes];
    durations.sort_unstable_by(|a, b| b.cmp(a));
    durations.dedup();
    let step = SignedDuration::from_mins(config.step_minutes);
    let signals: std::collections::HashMap<Ts, &SlotSignal> = inputs.signals.iter().map(|s| (s.start, s)).collect();
    let neutral = SlotSignal { start: now, crowd: None, confidence: 0.0, calendar_convenience: 0.0, adherence: 0.0, preferred_time: 0.0 };
    let mut result = Vec::new();
    for offset in 0..7 {
        let day = week_start.checked_add(jiff::Span::new().days(offset))?;
        if !profile.weekdays_allowed.contains(&weekday(day)) {
            continue;
        }
        let earliest = Ts(day.to_datetime(profile.earliest_workout_start).to_zoned(tz.clone())?.timestamp());
        let latest = Ts(day.to_datetime(profile.latest_workout_finish).to_zoned(tz.clone())?.timestamp());
        // Iterate real instants: skips nonexistent spring times, keeps both fall folds.
        let mut start = earliest;
        while start < latest {
            let local = start.local(&tz);
            if local.date() != day || local.time() < profile.earliest_workout_start {
                start = start + step;
                continue;
            }
            for &duration in &durations {
                let end = start.mins(duration);
                let prep = start.mins(-(profile.prep_minutes + profile.commute_to_gym_minutes));
                let home = end.mins(profile.commute_home_minutes);
                if prep < now || end > latest {
                    continue;
                }
                if blockers.iter().any(|b| overlaps(prep, home, b)) {
                    continue;
                }
                if inputs.existing.iter().any(|s| !recovery_ok(start, end, s, profile)) {
                    continue;
                }
                let signal = signals.get(&start).copied().unwrap_or(&neutral);
                let confidence = if signal.crowd.is_some() { signal.confidence } else { 0.0 };
                let parts = ScoreParts {
                    crowd: signal.crowd.map_or(0.0, |c| profile.crowd_preference_weight * (1.0 - c)),
                    calendar: profile.calendar_preference_weight * signal.calendar_convenience,
                    adherence: config.adherence_weight * signal.adherence,
                    preferred_time: config.preferred_time_weight * signal.preferred_time,
                    uncertainty: -config.uncertainty_weight * (1.0 - confidence),
                    duration: -config.shortened_duration_penalty * (profile.preferred_workout_minutes - duration) as f64 / profile.preferred_workout_minutes as f64,
                };
                result.push(Candidate {
                    start,
                    end,
                    prep_start: prep,
                    leave_home: start.mins(-profile.commute_to_gym_minutes),
                    home_at: home,
                    crowd: signal.crowd,
                    confidence,
                    score: parts.total(),
                    score_parts: Some(parts),
                });
            }
            start = start + step;
        }
    }
    Ok(result)
}

pub fn plan_week(profile: &Profile, week_start: Date, inputs: &Inputs, now: Ts, config: &PlannerConfig) -> Result<Plan> {
    let tz = profile.tz();
    let week_end = week_start.checked_add(jiff::Span::new().days(7))?;
    let existing_count = inputs.existing.iter().filter(|s| (week_start..week_end).contains(&s.start.local(&tz).date())).count() as i64;
    let capacity = (profile.weekly_target_sessions.min(profile.weekly_max_sessions) - existing_count).max(0) as usize;
    let candidates = generate_candidates(profile, week_start, inputs, now, config)?;
    let mut by_day: std::collections::BTreeMap<Date, Candidate> = Default::default();
    for candidate in candidates {
        let day = candidate.start.local(&tz).date();
        if by_day.get(&day).is_none_or(|current| candidate.score > current.score) {
            by_day.insert(day, candidate);
        }
    }
    let days: Vec<Date> = by_day.keys().copied().collect();
    let rest = profile.minimum_full_rest_days_between_sessions;
    let mut best: Vec<Date> = Vec::new();
    let mut best_key = (-1i64, f64::NEG_INFINITY);
    for size in 0..=capacity.min(days.len()) {
        for selected in combinations(&days, size) {
            if selected.windows(2).any(|w| ((w[1] - w[0]).get_days() as i64) <= rest) {
                continue;
            }
            let key = (size as i64, selected.iter().fold(0.0, |sum, d| sum + by_day[d].score));
            if key.0 > best_key.0 || key.0 == best_key.0 && key.1 > best_key.1 {
                best = selected;
                best_key = key;
            }
        }
    }
    let sessions: Vec<Candidate> = best.iter().map(|d| by_day[d].clone()).collect();
    let total = existing_count + sessions.len() as i64;
    Ok(Plan { week_start, sessions, existing_count, minimum_met: total >= profile.weekly_min_sessions, shortfall: (profile.weekly_target_sessions - total).max(0) })
}

/// All `size`-subsets in lexicographic order, like itertools.combinations.
fn combinations<T: Copy>(items: &[T], size: usize) -> Vec<Vec<T>> {
    if size == 0 {
        return vec![vec![]];
    }
    let mut out = Vec::new();
    for (i, &first) in items.iter().enumerate() {
        for mut rest in combinations(&items[i + 1..], size - 1) {
            rest.insert(0, first);
            out.push(rest);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::zone;

    const WEEK: Date = jiff::civil::date(2026, 10, 12);

    fn at(day: i8, hour: i8, minute: i8) -> Ts {
        Ts(jiff::civil::date(2026, 10, day).at(hour, minute, 0, 0).to_zoned(zone("Europe/Berlin").unwrap()).unwrap().timestamp())
    }

    fn signal(start: Ts, crowd: f64, confidence: f64) -> SlotSignal {
        SlotSignal { start, crowd: Some(crowd), confidence, calendar_convenience: 0.0, adherence: 0.0, preferred_time: 0.0 }
    }

    fn days(plan: &Plan) -> Vec<i64> {
        plan.sessions.iter().map(|c| weekday(c.start.local(&zone("Europe/Berlin").unwrap()).date())).collect()
    }

    fn plan(profile: &Profile, inputs: Inputs) -> Plan {
        plan_week(profile, WEEK, &inputs, at(11, 0, 0), &PlannerConfig::default()).unwrap()
    }

    fn one_a_week() -> Profile {
        Profile { weekly_min_sessions: 1, weekly_target_sessions: 1, weekly_max_sessions: 1, ..Profile::default() }
    }

    #[test]
    fn target_with_rest_days() {
        let p = plan(&Profile::default(), Inputs::default());
        assert_eq!(days(&p), [0, 2, 4]);
        assert!(p.minimum_met && p.shortfall == 0);
        assert_eq!(p.sessions[0].start - p.sessions[0].prep_start, SignedDuration::from_mins(35));
    }

    #[test]
    fn prep_and_travel_must_clear_blockers_but_may_touch() {
        let p = Profile { earliest_workout_start: jiff::civil::time(19, 0, 0, 0), latest_workout_finish: jiff::civil::time(20, 10, 0, 0), weekdays_allowed: vec![0], minimum_workout_minutes: 70, ..Profile::default() };
        let candidates = |busy: &[Interval]| generate_candidates(&p, WEEK, &Inputs { busy, ..Inputs::default() }, at(11, 0, 0), &PlannerConfig::default()).unwrap().len();
        assert_eq!(candidates(&[Interval { start: at(12, 18, 40), end: at(12, 19, 0) }]), 0);
        assert_eq!(candidates(&[Interval { start: at(12, 20, 20), end: at(12, 21, 0) }]), 0);
        assert_eq!(candidates(&[Interval { start: at(12, 18, 0), end: at(12, 18, 25) }, Interval { start: at(12, 20, 30), end: at(12, 21, 0) }]), 1);
    }

    #[test]
    fn recovery_crosses_weeks() {
        let sunday = [Interval { start: at(11, 0, 0), end: at(11, 0, 20) }];
        let p = plan(&Profile::default(), Inputs { existing: &sunday, ..Inputs::default() });
        assert_eq!(p.sessions.len(), 2);
        assert!(!days(&p).contains(&0));
    }

    #[test]
    fn session_count_beats_one_great_slot() {
        let tuesday = [SlotSignal { adherence: 1.0, ..signal(at(13, 12, 0), 0.0, 1.0) }];
        assert_eq!(days(&plan(&Profile::default(), Inputs { signals: &tuesday, ..Inputs::default() })), [0, 2, 4]);
    }

    #[test]
    fn quiet_slot_wins() {
        let quiet = [signal(at(14, 12, 0), 0.2, 0.9)];
        let p = plan(&one_a_week(), Inputs { signals: &quiet, ..Inputs::default() });
        assert_eq!(p.sessions[0].start, quiet[0].start);
        assert!((p.sessions[0].score - 0.75).abs() < 1e-9);
    }

    #[test]
    fn habit_breaks_ties_but_quiet_beats_habitual() {
        let p = Profile { weekdays_allowed: vec![0], ..one_a_week() };
        let busy_habit = SlotSignal { preferred_time: 0.9, ..signal(at(12, 13, 0), 2.0 / 3.0, 0.9) };
        let quiet = SlotSignal { preferred_time: 0.2, ..signal(at(12, 16, 0), 1.0 / 3.0, 0.9) };
        assert_eq!(plan(&p, Inputs { signals: &[busy_habit.clone(), quiet.clone()], ..Inputs::default() }).sessions[0].start, quiet.start);
        let same_crowd = SlotSignal { crowd: Some(2.0 / 3.0), ..quiet };
        assert_eq!(plan(&p, Inputs { signals: &[busy_habit.clone(), same_crowd], ..Inputs::default() }).sessions[0].start, busy_habit.start);
    }

    #[test]
    fn shortened_workout_when_only_minimum_fits() {
        let p = Profile { earliest_workout_start: jiff::civil::time(19, 0, 0, 0), latest_workout_finish: jiff::civil::time(19, 45, 0, 0), weekdays_allowed: vec![0], ..Profile::default() };
        let plan = plan(&p, Inputs::default());
        assert_eq!(plan.sessions[0].end - plan.sessions[0].start, SignedDuration::from_mins(45));
        assert!(!plan.minimum_met);
    }

    #[test]
    fn dst_durations_are_elapsed_time() {
        let tz = zone("Europe/Berlin").unwrap();
        for week in [jiff::civil::date(2026, 3, 23), jiff::civil::date(2026, 10, 19)] {
            let p = Profile { weekdays_allowed: vec![6], earliest_workout_start: jiff::civil::time(1, 0, 0, 0), latest_workout_finish: jiff::civil::time(5, 0, 0, 0), ..Profile::default() };
            let now = Ts(week.to_zoned(tz.clone()).unwrap().timestamp());
            let candidates = generate_candidates(&p, week, &Inputs::default(), now, &PlannerConfig::default()).unwrap();
            assert!(!candidates.is_empty());
            for c in candidates {
                assert!([70, 45].contains(&(c.end - c.start).as_mins()));
                assert!(c.end.local(&tz).hour() <= 5);
            }
        }
    }

    #[test]
    fn determinism_and_travel_buffers() {
        let first = plan(&Profile::default(), Inputs::default());
        assert_eq!(first.sessions, plan(&Profile::default(), Inputs::default()).sessions);
        for c in &first.sessions {
            assert!(c.crowd.is_none() && c.confidence == 0.0);
            assert_eq!(c.start - c.prep_start, SignedDuration::from_mins(35));
            assert_eq!(c.home_at - c.end, SignedDuration::from_mins(20));
        }
    }

    #[test]
    fn travel_entire_week() {
        let travel = [Interval { start: at(12, 0, 0), end: at(19, 0, 0) }];
        let p = plan(&Profile::default(), Inputs { unavailable: &travel, ..Inputs::default() });
        assert!(p.sessions.is_empty() && !p.minimum_met && p.shortfall == 3);
    }

    #[test]
    fn weekly_max_counts_existing_sessions() {
        let first = plan(&Profile::default(), Inputs::default());
        let existing: Vec<Interval> = first.sessions.iter().take(2).map(|c| Interval { start: c.start, end: c.end }).collect();
        let p = Profile { weekly_min_sessions: 1, weekly_target_sessions: 2, weekly_max_sessions: 2, ..Profile::default() };
        let full = plan(&p, Inputs { existing: &existing, ..Inputs::default() });
        assert!(full.sessions.is_empty() && full.existing_count == 2);
    }

    #[test]
    fn no_past_preparation() {
        let now = at(12, 19, 0);
        let candidates = generate_candidates(&Profile::default(), WEEK, &Inputs::default(), now, &PlannerConfig::default()).unwrap();
        assert!(!candidates.is_empty() && candidates.iter().all(|c| c.prep_start >= now));
    }

    #[test]
    fn invalid_profile_and_week() {
        let cases = [
            Profile { weekly_target_sessions: 1, ..Profile::default() },
            Profile { minimum_workout_minutes: 80, ..Profile::default() },
            Profile { timezone: "bad".into(), ..Profile::default() },
            Profile { weekdays_allowed: vec![7], ..Profile::default() },
            Profile { weekdays_allowed: vec![0, 0], ..Profile::default() },
            Profile { prep_minutes: -1, ..Profile::default() },
            Profile { crowd_preference_weight: f64::NAN, ..Profile::default() },
            Profile { latest_workout_finish: jiff::civil::time(6, 0, 0, 0), ..Profile::default() },
        ];
        for profile in cases {
            assert!(profile.validate().is_err(), "{profile:?}");
        }
        let tuesday = jiff::civil::date(2026, 10, 13);
        assert_eq!(plan_week(&Profile::default(), tuesday, &Inputs::default(), at(11, 0, 0), &PlannerConfig::default()).unwrap_err().code(), "INVALID_WEEK");
    }
}
