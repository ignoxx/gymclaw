//! Effort and lift trends inferred from what the owner already logs: reps and weight, never rest time
//! (owners miss the timer, log the last set at the next machine, or skip rest on purpose).
//!
//! Effort sizes the default weight jump after a workout (`audit::decide`). Trends reach the agent only in
//! the Sunday wake (`weekly`) and on demand through the `lift_history` view; never in the standing prompt.
use std::collections::{BTreeMap, HashMap};

use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::db::Connection;
use crate::db::models::{SetLog, WorkoutExercise, WorkoutSession};
use crate::domain::calendar::week_of;
use crate::domain::progression::{PerformanceSet, ProgressionDecision};
use crate::domain::templates::Load;
use crate::domain::workout::{self, g};
use crate::error::Result;
use crate::time::Ts;

/// Sessions a lift's best is compared over: "recent best", and how long it must stay flat to stall.
const RECENT: usize = 3;
/// Weeks of hard sets the Sunday wake averages per muscle.
const VOLUME_WEEKS: i64 = 4;

/// How hard an exercise's working sets were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    /// Reps left in the tank: above the range, or at its top while heavier or stronger than lately.
    Easy,
    Normal,
    /// Reps falling off by 4+ across the sets at one weight, or below the range.
    Grind,
}

/// Estimated 1-rep max (Epley). Mirrors the `lift_history` view in migration 008.
pub fn e1rm(weight: f64, reps: i64) -> f64 {
    if reps <= 1 { weight } else { weight * (1.0 + reps as f64 / 30.0) }
}

/// Strength of a session: the best set's estimated 1-rep max, or the most reps when no weight is
/// logged (bodyweight lifts).
fn score(sets: &[PerformanceSet]) -> f64 {
    if sets.iter().all(|s| s.weight == 0.0) {
        return sets.iter().map(|s| s.reps).max().unwrap_or(0) as f64;
    }
    sets.iter().map(|s| e1rm(s.weight, s.reps)).fold(0.0, f64::max)
}

/// Effort of one exercise's working sets (in logged order) against its rep range and target weight, read
/// at the heaviest weight used. `recent_best`: the best `score` of its last sessions before this one.
pub fn effort(
    sets: &[PerformanceSet],
    rep_min: i64,
    rep_max: i64,
    target_weight: f64,
    recent_best: Option<f64>,
) -> Option<Effort> {
    let top = sets.iter().map(|s| s.weight).reduce(f64::max)?;
    let reps: Vec<i64> = sets.iter().filter(|s| s.weight == top).map(|s| s.reps).collect();
    let drop = reps[0] - reps[reps.len() - 1];
    let lowest = *reps.iter().min().expect("the top set is in it");
    Some(if drop >= 4 || lowest < rep_min {
        Effort::Grind
    } else if drop <= 1
        && top >= target_weight
        && (lowest > rep_max
            || lowest == rep_max && (top > target_weight || recent_best.is_some_and(|b| score(sets) > b)))
    {
        Effort::Easy
    } else {
        Effort::Normal
    })
}

/// Sizes double progression's jump by effort: an easy session jumps two increments, a grind holds even
/// though the rep target was hit. A default; the agent can set any target.
pub fn sized(mut decision: ProgressionDecision, effort: Option<Effort>, increment: f64) -> ProgressionDecision {
    let round = |kg: f64| (kg * 10_000.0).round() / 10_000.0;
    if decision.progressed {
        match effort {
            Some(Effort::Easy) => {
                decision.next_weight = round(decision.next_weight + increment);
                decision.reason = "threshold_achieved_easy";
            }
            Some(Effort::Grind) => {
                decision.next_weight = round(decision.next_weight - increment);
                decision.progressed = false;
                decision.reason = "hold_grinding";
            }
            Some(Effort::Normal) | None => {}
        }
    }
    decision
}

/// One exercise in one finished workout: its reps sets in logged order and the targets it was done against.
struct Outing {
    workout_id: String,
    at: Ts,
    exercise_id: String,
    name: String,
    muscle: String,
    rep_min: i64,
    rep_max: i64,
    target_weight: f64,
    sets: Vec<PerformanceSet>,
}

impl Outing {
    fn effort(&self, recent_best: Option<f64>) -> Option<Effort> {
        effort(&self.sets, self.rep_min, self.rep_max, self.target_weight, recent_best)
    }
}

fn muscle(row: &WorkoutExercise) -> String {
    workout::muscle(row).unwrap_or_else(|| "Other".into())
}

/// Every finished outing with reps sets of weight lifted (of `exercise_id`, or all), oldest first. An
/// assisted machine's counterweight isn't load, so its sets would read as a grind getting easier.
fn outings(db: &Connection, exercise_id: Option<&str>) -> Result<Vec<Outing>> {
    let started: HashMap<String, Ts> =
        WorkoutSession::select(db, "WHERE status = 'PLAN_UPDATED' AND started_at IS NOT NULL", [])?
            .into_iter()
            .filter_map(|w| Some((w.id, w.started_at?)))
            .collect();
    let mut out = Vec::new();
    for row in WorkoutExercise::select(db, "WHERE ?1 IS NULL OR exercise_id = ?1 ORDER BY position", [exercise_id])? {
        let Some(&at) = started.get(&row.workout_session_id) else { continue };
        if workout::kind(&row).load == Load::Assisted {
            continue;
        }
        let sets: Vec<PerformanceSet> = SetLog::select(
            db,
            "WHERE workout_exercise_id = ?1 AND set_type = 'WORKING' AND seconds IS NULL AND meters IS NULL ORDER BY logged_at, set_number",
            [&row.id],
        )?
        .iter()
        .map(|s| PerformanceSet { weight: s.weight, reps: s.reps })
        .collect();
        if sets.is_empty() {
            continue;
        }
        out.push(Outing {
            workout_id: row.workout_session_id.clone(),
            at,
            exercise_id: row.exercise_id.clone(),
            name: workout::name(&row).to_string(),
            muscle: muscle(&row),
            rep_min: row.rep_min,
            rep_max: row.rep_max,
            target_weight: row.target_weight,
            sets,
        });
    }
    out.sort_by_key(|o| o.at);
    Ok(out)
}

/// Best `score` of the last `RECENT` outings.
fn recent_best(outings: &[&Outing]) -> Option<f64> {
    outings.iter().rev().take(RECENT).map(|o| score(&o.sets)).reduce(f64::max)
}

/// Effort of an exercise in its workout, against its recent best before that workout started.
pub fn effort_of(db: &Connection, exercise: &WorkoutExercise, sets: &[PerformanceSet]) -> Result<Option<Effort>> {
    let start = WorkoutSession::get(db, &exercise.workout_session_id)?.and_then(|w| w.started_at);
    let history = outings(db, Some(&exercise.exercise_id))?;
    let earlier: Vec<&Outing> = history
        .iter()
        .filter(|o| o.workout_id != exercise.workout_session_id && start.is_none_or(|s| o.at < s))
        .collect();
    Ok(effort(sets, exercise.rep_min, exercise.rep_max, exercise.target_weight, recent_best(&earlier)))
}

/// A lift's best, as `lift_best` reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Best {
    /// Estimated 1-rep max in kg.
    E1rm(f64),
    /// Most reps in a set of a bodyweight lift.
    Reps(i64),
}

/// A lift's recent best (last `RECENT` outings) and all-time best before `until`.
pub fn lift_best(db: &Connection, exercise_id: &str, until: Ts) -> Result<Option<(Best, Best)>> {
    let history = outings(db, Some(exercise_id))?;
    let before: Vec<&Outing> = history.iter().filter(|o| o.at < until).collect();
    let bodyweight = before.last().is_some_and(|latest| latest.sets.iter().all(|s| s.weight == 0.0));
    let best =
        |score: f64| if bodyweight { Best::Reps(score as i64) } else { Best::E1rm((score * 10.0).round() / 10.0) };
    let ever = before.iter().map(|o| score(&o.sets)).reduce(f64::max);
    Ok(recent_best(&before).zip(ever).map(|(recent, ever)| (best(recent), best(ever))))
}

/// What the Sunday wake says about one lift trained in the reviewed week, when anything stands out:
/// "Bench: est. 1-rep max 92→92→92→91, stalled; last 80 kg × 10/8/7 on 8–10, grinding". `history` is
/// the lift's outings up to the end of that week, oldest first.
fn lift_line(history: &[&Outing]) -> Option<String> {
    let latest = history.last()?;
    let scores: Vec<f64> = history.iter().map(|o| score(&o.sets)).collect();
    let n = scores.len();
    // Flat against the sessions just before, not an old peak: climbing back after a break isn't a stall.
    let stalled = n > RECENT
        && scores[n - RECENT..].iter().copied().fold(f64::MIN, f64::max)
            <= scores[n.saturating_sub(2 * RECENT)..n - RECENT].iter().copied().fold(f64::MIN, f64::max);
    let efforts: Vec<Option<Effort>> =
        (0..n).map(|i| history[i].effort(recent_best(&history[i.saturating_sub(RECENT)..i]))).collect();
    let easy = efforts.iter().rev().take_while(|e| **e == Some(Effort::Easy)).count();
    let grinding = efforts[n - 1] == Some(Effort::Grind);
    if !stalled && easy < 2 && !grinding {
        return None;
    }
    let bodyweight = latest.sets.iter().all(|s| s.weight == 0.0);
    let mut parts = Vec::new();
    if stalled {
        let series: Vec<String> = scores[n - RECENT - 1..].iter().map(|s| g((s * 10.0).round() / 10.0)).collect();
        let what = if bodyweight { "best reps" } else { "est. 1-rep max" };
        parts.push(format!("{what} {}, stalled", series.join("→")));
    }
    if easy >= 2 {
        parts.push(format!("{easy} easy sessions in a row"));
    }
    let top = latest.sets.iter().map(|s| s.weight).fold(0.0, f64::max);
    let reps: Vec<String> = latest.sets.iter().filter(|s| s.weight == top).map(|s| s.reps.to_string()).collect();
    let weight = if bodyweight { String::new() } else { format!("{} kg × ", g(top)) };
    let mut last = format!("last {weight}{} on {}–{}", reps.join("/"), latest.rep_min, latest.rep_max);
    if grinding {
        last += ", grinding";
    }
    parts.push(last);
    Some(format!("{}: {}", latest.name, parts.join("; ")))
}

/// Week-level trends for the Sunday wake: one line per lift trained in `week` that stalled, came easy
/// twice in a row or ground out its sets, then hard sets per primary muscle against the 4 weeks before.
/// A hard set is a working set of reps; warm-ups and timed work don't count.
pub fn weekly(db: &Connection, week: Date, tz: &TimeZone) -> Result<Vec<String>> {
    let all = outings(db, None)?;
    let end = week.checked_add(jiff::Span::new().days(7))?;
    let mut by_lift: BTreeMap<&str, Vec<&Outing>> = BTreeMap::new();
    for o in all.iter().filter(|o| week_of(o.at, tz) < end) {
        by_lift.entry(&o.exercise_id).or_default().push(o);
    }
    let mut trained: Vec<&Vec<&Outing>> =
        by_lift.values().filter(|h| h.last().is_some_and(|o| week_of(o.at, tz) == week)).collect();
    trained.sort_by_key(|h| h.last().map(|o| o.at));
    let mut lines: Vec<String> = trained.into_iter().filter_map(|h| lift_line(h)).collect();

    let start = week.checked_sub(jiff::Span::new().days(7 * VOLUME_WEEKS))?;
    let mut sets: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for o in all.iter().filter(|o| (start..end).contains(&week_of(o.at, tz))) {
        let entry = sets.entry(&o.muscle).or_default();
        if week_of(o.at, tz) == week { entry.0 += o.sets.len() } else { entry.1 += o.sets.len() }
    }
    if !sets.is_empty() {
        let parts: Vec<String> = sets
            .iter()
            .map(|(m, (now, before))| {
                format!("{m} {now} (avg {})", g((*before as f64 / VOLUME_WEEKS as f64 * 10.0).round() / 10.0))
            })
            .collect();
        lines.push(format!(
            "Hard sets this week by muscle (avg of the {VOLUME_WEEKS} weeks before): {}",
            parts.join(", ")
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sets(weight: f64, reps: &[i64]) -> Vec<PerformanceSet> {
        reps.iter().map(|&reps| PerformanceSet { weight, reps }).collect()
    }

    #[test]
    fn effort_reads_range_drop_off_and_weight() {
        let on = |w: f64, reps: &[i64], best: Option<f64>| effort(&sets(w, reps), 8, 10, 80.0, best);
        // The issue's example: 12/12/11 on 8–10 is too light.
        assert_eq!(on(80.0, &[12, 12, 11], None), Some(Effort::Easy));
        // Hit the top of the range at the target: normal, unless stronger than lately or heavier than asked.
        assert_eq!(on(80.0, &[10, 10, 10], Some(e1rm(80.0, 10))), Some(Effort::Normal));
        assert_eq!(on(80.0, &[10, 10, 10], Some(100.0)), Some(Effort::Easy));
        assert_eq!(on(85.0, &[10, 10, 10], None), Some(Effort::Easy));
        // Lighter than the target by choice: never easy.
        assert_eq!(on(70.0, &[12, 12, 12], None), Some(Effort::Normal));
        // Falling off by 4 is a grind even above the range; below the range too.
        assert_eq!(on(80.0, &[14, 12, 10], None), Some(Effort::Grind));
        assert_eq!(on(80.0, &[9, 8, 7], None), Some(Effort::Grind));
        assert_eq!(on(80.0, &[10, 9, 8], None), Some(Effort::Normal));
        // A lighter back-off set doesn't count as drop-off.
        let mut backoff = sets(80.0, &[11, 11]);
        backoff.push(PerformanceSet { weight: 60.0, reps: 6 });
        assert_eq!(effort(&backoff, 8, 10, 80.0, None), Some(Effort::Easy));
    }

    #[test]
    fn effort_sizes_the_jump() {
        let decision = |progressed| ProgressionDecision {
            exercise_id: "bench".into(),
            previous_weight: 80.0,
            next_weight: if progressed { 82.5 } else { 80.0 },
            progressed,
            reason: "threshold_achieved",
        };
        assert_eq!(sized(decision(true), Some(Effort::Easy), 2.5).next_weight, 85.0);
        let held = sized(decision(true), Some(Effort::Grind), 2.5);
        assert_eq!((held.next_weight, held.progressed), (80.0, false));
        assert_eq!(sized(decision(true), Some(Effort::Normal), 2.5), decision(true));
        // Nothing to size when double progression held.
        assert_eq!(sized(decision(false), Some(Effort::Easy), 2.5), decision(false));
        assert_eq!(sized(decision(true), Some(Effort::Easy), 0.1).next_weight, 82.6);
    }

    #[test]
    fn finished_workouts_size_the_next_weight_by_effort() {
        use crate::db::models::ExerciseProgression;
        use crate::domain::backfill::{PastSet, log_past};
        use crate::domain::workout::tests::at;
        let db = crate::db::test::with_short_template();
        let next = |reps: [i64; 2], day: i64| {
            let sets: Vec<PastSet> = reps
                .iter()
                .map(|&reps| PastSet { exercise: "bench".into(), reps, weight: 80.0, minutes: None, meters: None })
                .collect();
            let done = at(day * 86_400);
            log_past(&db, &sets, done, Some("short"), done.mins(60), &format!("past-{day}")).unwrap();
            ExerciseProgression::get(&db, "bench").unwrap().unwrap().next_weight
        };
        // Short's bench: 2 working sets of 8–10 at 80. Grinding to the rep max holds; sailing past it jumps twice.
        assert_eq!(next([14, 10], 0), 80.0);
        assert_eq!(next([12, 12], 1), 85.0);
    }

    fn outing(day: i64, weight: f64, reps: &[i64]) -> Outing {
        Outing {
            workout_id: format!("w{day}"),
            at: Ts::parse("2026-10-01T18:00:00Z", None).unwrap().mins(day * 24 * 60),
            exercise_id: "bench".into(),
            name: "Bench".into(),
            muscle: "Chest".into(),
            rep_min: 8,
            rep_max: 10,
            target_weight: weight,
            sets: sets(weight, reps),
        }
    }

    #[test]
    fn lift_lines_flag_stalls_easy_streaks_and_grinds() {
        let line = |history: &[Outing]| lift_line(&history.iter().collect::<Vec<_>>());
        // Progressing normally: nothing to say.
        assert_eq!(line(&[outing(0, 80.0, &[10, 9, 8]), outing(3, 82.5, &[10, 9, 8])]), None);
        let stalled = [
            outing(0, 80.0, &[10, 9, 9]),
            outing(3, 80.0, &[10, 9, 8]),
            outing(7, 80.0, &[10, 9, 8]),
            outing(10, 80.0, &[10, 8, 6]),
        ];
        assert_eq!(
            line(&stalled).unwrap(),
            "Bench: est. 1-rep max 106.7→106.7→106.7→106.7, stalled; last 80 kg × 10/8/6 on 8–10, grinding"
        );
        // Climbing back after a break isn't a stall, even below an old peak.
        let back = [
            outing(0, 100.0, &[10, 9, 8]),
            outing(30, 70.0, &[10, 9, 8]),
            outing(33, 72.5, &[10, 9, 8]),
            outing(36, 75.0, &[10, 9, 8]),
            outing(39, 77.5, &[10, 9, 8]),
            outing(42, 80.0, &[10, 9, 8]),
            outing(45, 82.5, &[10, 9, 8]),
        ];
        assert_eq!(line(&back), None);
        let easy = [outing(0, 60.0, &[12, 12, 12]), outing(3, 60.0, &[12, 12, 11])];
        assert_eq!(line(&easy).unwrap(), "Bench: 2 easy sessions in a row; last 60 kg × 12/12/11 on 8–10");
    }
}
