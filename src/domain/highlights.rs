//! What stood out in a finished workout: records, lifts that went up, volume and pace against the
//! last session of the same workout. The wrap-up card and the Sunday recap both read it, so they never
//! disagree. "Earlier" always means logged before the workout started, so a recap computed days later
//! sees the same records the card showed.
use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::db::Connection;
use crate::db::models::{SetLog, WorkoutExercise, WorkoutSession};
use crate::domain::calendar::week_of;
use crate::domain::workout;
use crate::error::Result;
use crate::time::Ts;

/// How a lift's top set beat earlier sessions; each carries the number it beat.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// Heaviest working set ever.
    WeightRecord(f64),
    /// Most reps ever at this weight.
    RepRecord(i64),
    /// Heavier than the last session.
    Heavier(f64),
    /// More reps at the last session's weight.
    MoreReps(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lift {
    pub name: String,
    pub weight: f64,
    pub reps: i64,
    pub change: Change,
}

impl Lift {
    pub fn record(&self) -> bool {
        matches!(self.change, Change::WeightRecord(_) | Change::RepRecord(_))
    }
}

#[derive(Clone, Debug)]
pub struct Highlights {
    pub name: String,
    pub started_at: Ts,
    pub minutes: i64,
    pub sets: usize,
    /// Kilograms moved in working sets (weight × reps).
    pub volume: f64,
    /// Minutes and volume of the previous session of the same workout.
    pub last: Option<(i64, f64)>,
    /// Lifts that beat earlier sessions, in workout order.
    pub lifts: Vec<Lift>,
}

fn working(db: &Connection, workout_id: &str) -> Result<Vec<SetLog>> {
    SetLog::select(
        db,
        "WHERE set_type = 'WORKING' AND workout_exercise_id IN (SELECT id FROM workout_exercise WHERE workout_session_id = ?1) ORDER BY logged_at",
        [workout_id],
    )
}

fn volume(sets: &[SetLog]) -> f64 {
    sets.iter().map(|s| s.weight * s.reps as f64).sum()
}

/// Heaviest set, more reps breaking ties.
fn top<'a>(sets: impl Iterator<Item = &'a SetLog>) -> Option<&'a SetLog> {
    sets.max_by(|a, b| a.weight.total_cmp(&b.weight).then(a.reps.cmp(&b.reps)))
}

/// What `best` beat among `earlier` sets of the same exercise (oldest first). Nothing the first time.
fn change(best: &SetLog, earlier: &[SetLog]) -> Option<Change> {
    let heaviest = earlier.iter().map(|s| s.weight).reduce(f64::max)?;
    if best.weight > heaviest {
        return Some(Change::WeightRecord(heaviest));
    }
    // Same weight only: an old heavy single must not make every lighter set a record.
    if let Some(most) = earlier.iter().filter(|s| s.weight == best.weight).map(|s| s.reps).max()
        && best.reps > most
    {
        return Some(Change::RepRecord(most));
    }
    let session = &earlier.last()?.workout_exercise_id;
    let last = top(earlier.iter().filter(|s| &s.workout_exercise_id == session))?;
    if best.weight > last.weight {
        Some(Change::Heavier(last.weight))
    } else if best.weight == last.weight && best.reps > last.reps {
        Some(Change::MoreReps(last.reps))
    } else {
        None
    }
}

pub fn workout(db: &Connection, workout_id: &str) -> Result<Highlights> {
    let Some(session) = WorkoutSession::get(db, workout_id)? else { crate::bail!("WORKOUT_NOT_FOUND", "Unknown workout") };
    let Some(start) = session.started_at else { crate::bail!("WORKOUT_NOT_STARTED", "Workout has no start time") };
    let name = session.template_snapshot["name"].as_str().unwrap_or("Workout").to_string();
    let mut lifts = Vec::new();
    for row in WorkoutExercise::select(db, "WHERE workout_session_id = ?1 ORDER BY position", [workout_id])? {
        let sets = SetLog::select(db, "WHERE set_type = 'WORKING' AND workout_exercise_id = ?1", [&row.id])?;
        let Some(best) = top(sets.iter()) else { continue };
        let earlier = SetLog::select(
            db,
            "WHERE set_type = 'WORKING' AND logged_at < ?1 AND workout_exercise_id IN (SELECT id FROM workout_exercise WHERE exercise_id = ?2 AND workout_session_id != ?3) ORDER BY logged_at",
            (start, &row.exercise_id, workout_id),
        )?;
        if let Some(change) = change(best, &earlier) {
            lifts.push(Lift { name: workout::name(&row).to_string(), weight: best.weight, reps: best.reps, change });
        }
    }
    let sets = working(db, workout_id)?;
    let previous = WorkoutSession::first(
        db,
        "WHERE status = 'PLAN_UPDATED' AND id != ?1 AND completed_at < ?2 AND json_extract(template_snapshot, '$.name') = ?3 ORDER BY completed_at DESC",
        (workout_id, start, &name),
    )?;
    let last = match previous {
        Some(p) => Some((p.actual_duration_seconds.unwrap_or(0).div_euclid(60), volume(&working(db, &p.id)?))),
        None => None,
    };
    Ok(Highlights { name, started_at: start, minutes: session.actual_duration_seconds.unwrap_or(0).div_euclid(60), sets: sets.len(), volume: volume(&sets), last, lifts })
}

/// Finished workouts that started in `week` (Monday, owner-local), oldest first.
pub fn finished_in(db: &Connection, week: Date, tz: &TimeZone) -> Result<Vec<WorkoutSession>> {
    Ok(WorkoutSession::select(db, "WHERE status = 'PLAN_UPDATED' AND started_at IS NOT NULL ORDER BY started_at", [])?
        .into_iter()
        .filter(|w| w.started_at.is_some_and(|s| week_of(s, tz) == week))
        .collect())
}

/// Kilograms moved in `workouts`' working sets.
pub fn total_volume(db: &Connection, workouts: &[WorkoutSession]) -> Result<f64> {
    let mut total = 0.0;
    for w in workouts {
        total += volume(&working(db, &w.id)?);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(session: &str, weight: f64, reps: i64) -> SetLog {
        SetLog { id: String::new(), workout_exercise_id: session.into(), set_number: 1, set_type: crate::db::models::SetType::Working, weight, reps, rir_optional: None,
            logged_at: Ts::now(), rest_started_at: None, rest_due_at: None }
    }

    #[test]
    fn records_beat_everything_before_and_progress_beats_last_time() {
        // Two earlier sessions: a heavy single and last week's 8 × 80.
        let earlier = [set("a", 90.0, 3), set("a", 85.0, 6), set("b", 80.0, 8)];
        assert_eq!(change(&set("x", 92.5, 1), &earlier), Some(Change::WeightRecord(90.0)));
        // 7 reps at 85 beats the 6 done at 85.
        assert_eq!(change(&set("x", 85.0, 7), &earlier), Some(Change::RepRecord(6)));
        assert_eq!(change(&set("x", 80.0, 9), &earlier), Some(Change::RepRecord(8)));
        // Never done at 82.5: heavier than last time, not a rep record against the 90 kg triple.
        assert_eq!(change(&set("x", 82.5, 6), &earlier), Some(Change::Heavier(80.0)));
        // More reps than last time, still short of an older best at that weight: progress, not a record.
        assert_eq!(change(&set("x", 80.0, 9), &[set("a", 80.0, 10), set("b", 80.0, 8)]), Some(Change::MoreReps(8)));
        assert_eq!(change(&set("x", 80.0, 8), &earlier), None);
        // First time: nothing to beat. Bodyweight: reps are the record.
        assert_eq!(change(&set("x", 80.0, 8), &[]), None);
        assert_eq!(change(&set("x", 0.0, 15), &[set("b", 0.0, 12)]), Some(Change::RepRecord(12)));
    }
}
