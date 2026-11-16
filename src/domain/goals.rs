//! Goals: what the owner trains toward, in their words, with an optional measurable target and the
//! agent's current training block. The agent sets and plans them; code stores them and computes progress
//! by kind. Progress reaches the owner in the Sunday recap and the agent in the Sunday wake or on demand
//! (`goal list`); the standing context only names the active goals.
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

use crate::db::models::{BodyWeight, Goal, GoalKind, GoalStatus};
use crate::db::{Connection, new_id};
use crate::domain::core::mutate;
use crate::domain::profile::get_profile;
use crate::domain::workout::g;
use crate::domain::{activities, highlights, trends};
use crate::error::Result;
use crate::time::Ts;

/// What `set` and `update` write; `None` keeps the current value on update.
#[derive(Clone, Debug, Default)]
pub struct Fields {
    pub text: Option<String>,
    pub kind: Option<GoalKind>,
    pub exercise_id: Option<String>,
    pub target: Option<f64>,
    pub target_date: Option<Date>,
    pub block: Option<String>,
}

impl Fields {
    /// The idempotency intent: what a retry must repeat.
    fn intent(&self) -> Value {
        json!({"text": self.text, "kind": self.kind, "exercise_id": self.exercise_id, "target": self.target,
            "target_date": self.target_date.map(|d| d.to_string()), "block": self.block})
    }
}

pub fn active(db: &Connection) -> Result<Vec<Goal>> {
    Goal::select(db, "WHERE status = 'ACTIVE' ORDER BY created_at", [])
}

fn apply(db: &Connection, row: &mut Goal, fields: Fields) -> Result<()> {
    if let Some(text) = fields.text {
        row.text = text.trim().to_string();
    }
    row.kind = fields.kind.unwrap_or(row.kind);
    row.exercise_id = fields.exercise_id.or(row.exercise_id.take());
    row.target = fields.target.or(row.target);
    row.target_date = fields.target_date.map(|d| d.to_string()).or(row.target_date.take());
    row.block_text = fields.block.or(row.block_text.take());
    if row.text.is_empty() {
        crate::bail!("INVALID_INPUT", "Say the goal in the owner's words (text)");
    }
    if row.kind == GoalKind::Lift {
        let Some(exercise) = &row.exercise_id else {
            crate::bail!("ARGUMENT_REQUIRED", "A lift goal needs exercise_id")
        };
        let known: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM workout_exercise WHERE exercise_id = ?1)
                OR EXISTS (SELECT 1 FROM workout_template t, json_each(t.definition_json, '$.exercises') e WHERE e.value ->> 'id' = ?1)",
            [exercise],
            |r| r.get(0),
        )?;
        if !known {
            crate::bail!(
                "EXERCISE_NOT_FOUND",
                "No exercise with id {exercise} in a template or a logged workout (workout_exercise.exercise_id)"
            );
        }
    }
    Ok(())
}

/// A new active goal.
pub fn set(db: &Connection, fields: Fields, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "goal.set", request_id, fields.intent(), now, || {
        let Some(kind) = fields.kind else { crate::bail!("ARGUMENT_REQUIRED", "A new goal needs a kind") };
        let mut row = Goal {
            id: new_id(),
            text: String::new(),
            kind,
            exercise_id: None,
            target: None,
            target_date: None,
            status: GoalStatus::Active,
            block_text: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        };
        apply(db, &mut row, fields)?;
        row.insert(db)?;
        report(db, &row, now)
    })
}

fn load(db: &Connection, id: &str) -> Result<Goal> {
    match Goal::get(db, id)? {
        Some(row) => Ok(row),
        None => crate::bail!("GOAL_NOT_FOUND", "No goal {id}; goal list shows the active ones"),
    }
}

/// Change a goal: its words, target, date or the current block.
pub fn update(db: &Connection, id: &str, fields: Fields, now: Ts, request_id: &str) -> Result<Value> {
    let intent = json!({"id": id, "fields": fields.intent()});
    mutate(db, "goal.updated", request_id, intent, now, || {
        let mut row = load(db, id)?;
        apply(db, &mut row, fields)?;
        row.updated_at = now;
        row.update(db)?;
        report(db, &row, now)
    })
}

/// Close a goal as reached or dropped.
pub fn close(db: &Connection, id: &str, status: GoalStatus, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "goal.closed", request_id, json!({"id": id, "status": status}), now, || {
        let mut row = load(db, id)?;
        (row.status, row.closed_at, row.updated_at) = (status, Some(now), now);
        row.update(db)?;
        Ok(json!({"goal": row}))
    })
}

/// Where a goal stands, by kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Current {
    /// Best of the last 3 outings (where the lift stands now) and all-time best.
    Lift { recent: trends::Best, ever: trends::Best },
    /// Latest weigh-in.
    Kg(f64),
    /// Cardio minutes in the week: timed gym work plus activities.
    Minutes(i64),
    /// Finished workouts in the week.
    Workouts(usize),
}

/// Where a goal stands in `week` (Monday, owner-local). None for `other` goals and when nothing is
/// logged yet.
pub fn progress(db: &Connection, goal: &Goal, week: Date, tz: &TimeZone) -> Result<Option<Current>> {
    let end = Ts(week.checked_add(jiff::Span::new().days(7))?.to_zoned(tz.clone())?.timestamp());
    Ok(match goal.kind {
        GoalKind::Lift => match &goal.exercise_id {
            Some(exercise) => {
                trends::lift_best(db, exercise, end)?.map(|(recent, ever)| Current::Lift { recent, ever })
            }
            None => None,
        },
        GoalKind::BodyWeight => {
            BodyWeight::first(db, "WHERE measured_at < ?1 ORDER BY measured_at DESC", [end])?.map(|b| Current::Kg(b.kg))
        }
        GoalKind::CardioMinutesWeek => {
            let workouts = highlights::finished_in(db, week, tz)?;
            let mut seconds = 0;
            for w in &workouts {
                seconds += highlights::workout(db, &w.id)?.timed.iter().map(|(_, s)| s).sum::<i64>();
            }
            let outside: i64 = activities::in_week(db, week, tz)?.iter().map(|o| o.activity.minutes).sum();
            Some(Current::Minutes(seconds / 60 + outside))
        }
        GoalKind::SessionsWeek => Some(Current::Workouts(highlights::finished_in(db, week, tz)?.len())),
        GoalKind::Other => None,
    })
}

/// "Bench 100 kg by March: est. 1-rep max 92.5 / 100 kg (all-time 95)". The all-time best shows
/// only when the recent one is below it. None when there's no number yet.
pub fn line(goal: &Goal, current: Option<Current>) -> Option<String> {
    let ever = match current? {
        Current::Lift { recent, ever } if ever != recent => match ever {
            trends::Best::E1rm(kg) => format!(" (all-time {})", g(kg)),
            trends::Best::Reps(n) => format!(" (all-time {n})"),
        },
        _ => String::new(),
    };
    let (prefix, value, unit) = match current? {
        Current::Lift { recent: trends::Best::E1rm(kg), .. } => ("est. 1-rep max ", g(kg), "kg"),
        Current::Lift { recent: trends::Best::Reps(n), .. } => ("best ", n.to_string(), "reps"),
        Current::Kg(kg) => ("", g(kg), "kg"),
        Current::Minutes(m) => ("", m.to_string(), "min"),
        Current::Workouts(n) => ("", n.to_string(), "workouts"),
    };
    let target = goal.target.map(|t| format!(" / {}", g(t))).unwrap_or_default();
    Some(format!("{}: {prefix}{value}{target} {unit}{ever}", goal.text))
}

/// A goal with its progress this week, as tools return it.
fn report(db: &Connection, goal: &Goal, now: Ts) -> Result<Value> {
    let tz = get_profile(db)?.tz();
    let current = progress(db, goal, crate::domain::calendar::week_of(now, &tz), &tz)?;
    Ok(json!({"goal": goal, "progress": line(goal, current)}))
}

/// Active goals with this week's progress, for `goal list`.
pub fn list(db: &Connection, now: Ts) -> Result<Value> {
    let goals: Vec<Value> = active(db)?.iter().map(|g| report(db, g, now)).collect::<Result<_>>()?;
    Ok(json!({"goals": goals}))
}

/// The Sunday wake's view of each active goal: progress over `week`, and the current block.
pub fn review(db: &Connection, week: Date, tz: &TimeZone) -> Result<Vec<Value>> {
    active(db)?
        .iter()
        .map(|goal| {
            let progress = line(goal, progress(db, goal, week, tz)?);
            Ok(json!({"id": goal.id, "goal": goal.text, "target_date": goal.target_date, "progress": progress, "block": goal.block_text}))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Intensity;

    #[test]
    fn cardio_counts_timed_gym_work_and_activities() {
        let db = crate::db::test::db();
        let tz = get_profile(&db).unwrap().tz();
        let now = Ts::parse("2026-11-04T12:00:00", Some(&tz)).unwrap();
        let fields = Fields {
            text: Some("150 min cardio a week".into()),
            kind: Some(GoalKind::CardioMinutesWeek),
            target: Some(150.0),
            ..Fields::default()
        };
        set(&db, fields, now, "g1").unwrap();
        let goal = active(&db).unwrap().remove(0);
        let run = activities::Fields {
            kind: Some("run".into()),
            at: Some(now),
            minutes: Some(40),
            intensity: Some(Intensity::Hard),
            ..Default::default()
        };
        activities::log(&db, run, now, "a1").unwrap();
        // A timed bike block in a finished workout that week.
        db.execute_batch(
            "INSERT INTO workout_session (id, status, started_at, completed_at, waited_for_equipment_count)
                VALUES ('w', 'PLAN_UPDATED', '2026-11-03 17:00:00', '2026-11-03 18:00:00', 0);
             INSERT INTO workout_exercise (id, workout_session_id, exercise_id, position, status, planned_working_sets, rep_min, rep_max, target_weight, rest_seconds)
                VALUES ('e', 'w', 'bike', 0, 'COMPLETED', 1, 0, 0, 0, 0);
             INSERT INTO set_log (id, workout_exercise_id, set_number, set_type, weight, reps, logged_at, seconds)
                VALUES ('s', 'e', 1, 'WORKING', 0, 0, '2026-11-03 17:30:00', 1500);",
        )
        .unwrap();
        let week = crate::domain::calendar::week_of(now, &tz);
        assert_eq!(
            line(&goal, progress(&db, &goal, week, &tz).unwrap()).unwrap(),
            "150 min cardio a week: 65 / 150 min"
        );
        // A lift goal needs an exercise that exists.
        let lift = Fields {
            text: Some("bench 100".into()),
            kind: Some(GoalKind::Lift),
            exercise_id: Some("nope".into()),
            ..Fields::default()
        };
        assert_eq!(set(&db, lift, now, "g2").unwrap_err().code(), "EXERCISE_NOT_FOUND");
        // A lift goal counts where the lift stands now; a higher all-time best shows beside it.
        let bench = Goal { text: "Bench 100".into(), target: Some(100.0), ..goal };
        let at = |recent, ever| line(&bench, Some(Current::Lift { recent, ever })).unwrap();
        assert_eq!(
            at(trends::Best::E1rm(92.5), trends::Best::E1rm(95.0)),
            "Bench 100: est. 1-rep max 92.5 / 100 kg (all-time 95)"
        );
        assert_eq!(at(trends::Best::E1rm(95.0), trends::Best::E1rm(95.0)), "Bench 100: est. 1-rep max 95 / 100 kg");
    }
}
