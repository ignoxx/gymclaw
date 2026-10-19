//! Workouts the owner did without the bot (forgot to start it, another gym), logged afterwards
//! (port of gymclaw/services/backfill.py). Owner: WP-train.
use jiff::SignedDuration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{ExerciseStatus, PlannedSession, SessionStatus, SetType};
use crate::domain::adaptation::switch_to;
use crate::domain::coach::end_workout;
use crate::domain::core::mutate;
use crate::domain::profile::get_profile;
use crate::domain::set_parser::SetInput;
use crate::domain::workout::{active_workout, exercises, guide_id, log_set, name, start};
use crate::error::{Error, Result};
use crate::time::Ts;

const SET_SPACING: SignedDuration = SignedDuration::from_mins(3);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PastSet {
    /// Exercise ID, guide_id or exact name.
    pub exercise: String,
    pub reps: i64,
    /// kg; 0 or omitted for bodyweight.
    #[serde(default)]
    pub weight: f64,
}

/// Replay the owner's sets (in the order done) through the normal workout flow, backdated from `at`,
/// so history, progression and the week's plan count them. Off-plan exercises don't change the saved
/// plan. Without `template_id` it is that day's planned session (a missed one counts after all).
pub fn log_past(db: &Connection, sets: &[PastSet], at: Ts, template_id: Option<&str>, now: Ts, request_id: &str) -> Result<Value> {
    if sets.iter().any(|s| s.exercise.is_empty() || s.reps <= 0 || !s.weight.is_finite() || s.weight < 0.0) {
        return Err(Error::domain("INVALID_INPUT", "Each set needs an exercise, reps > 0 and weight ≥ 0"));
    }
    let intent = json!({"at": at, "template_id": template_id, "sets": sets});
    mutate(db, "workout.logged_past", request_id, intent, now, || {
        if sets.is_empty() {
            crate::bail!("ARGUMENT_REQUIRED", "Pass at least one set");
        }
        if at >= now {
            crate::bail!("WORKOUT_NOT_PAST", "That time hasn't happened yet; start the workout instead");
        }
        let zone = get_profile(db)?.tz();
        let day = at.local(&zone).date();
        let planned = PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED', 'MISSED')", [])?.into_iter().find(|s| {
            s.planned_start_at.local(&zone).date() == day && s.workout_template_id.is_some() && template_id.is_none_or(|t| s.workout_template_id.as_deref() == Some(t))
        });
        let Some(chosen) = template_id.map(String::from).or_else(|| planned.as_ref().and_then(|p| p.workout_template_id.clone())) else {
            crate::bail!("ARGUMENT_REQUIRED", "No planned workout on {day}; pass template_id");
        };
        if let Some(mut missed) = planned.clone().filter(|p| p.status == SessionStatus::Missed) {
            missed.status = SessionStatus::Committed;
            missed.update(db)?;
        }
        let step = SET_SPACING.min((now - at) / (sets.len() as i32 + 1));
        start(db, Some(&chosen), planned.as_ref().map(|p| p.id.as_str()), false, at, &format!("{request_id}:start"))?;
        let workout = active_workout(db)?.ok_or_else(|| Error::domain("NO_ACTIVE_WORKOUT", "Workout did not start"))?;
        let mut when = at;
        for (index, item) in sets.iter().enumerate() {
            let key = item.exercise.trim().to_lowercase();
            let active = exercises(db, &workout.id)?.into_iter().find(|e| e.status == ExerciseStatus::Active);
            let matches = active.as_ref().is_some_and(|a| [a.exercise_id.to_lowercase(), name(a).to_lowercase(), guide_id(a).unwrap_or_default().to_lowercase()].contains(&key));
            if !matches {
                switch_to(db, &workout.id, &item.exercise, false, when, &format!("{request_id}:switch:{index}"))?;
            }
            let value = SetInput { weight: item.weight, reps: item.reps, set_type: SetType::Working, rir: None };
            log_set(db, &workout.id, &value, when, &format!("{request_id}:set:{index}"))?;
            when = when + step;
        }
        let summary = end_workout(db, &workout, when, &format!("{request_id}:end"))?.cards.into_iter().next().map(|c| c.text);
        Ok(json!({"workout_id": workout.id, "template_id": chosen, "planned_session_id": workout.planned_session_id, "sets": sets.len(), "summary": summary}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{ExerciseProgression, NotificationJob, SetLog};
    use crate::domain::templates::{Template, get_template, import_template};
    use crate::domain::workout::tests::{at, db, spec};

    #[test]
    fn past_workout_counts_for_progression_without_rewriting_the_plan() {
        let db = db();
        let exercises = vec![
            spec(json!({"id": "bench", "name": "Bench", "role": "press", "guide_id": "bench-press", "primary": true, "working_sets": 3, "target_weight": 80, "warmup_weight": 40})),
            spec(json!({"id": "pec-deck", "name": "Pec deck", "role": "fly", "guide_id": "pec-deck", "working_sets": 2, "target_weight": 50})),
        ];
        import_template(&db, &Template { id: "upper".into(), name: "Upper".into(), exercises, alternatives: vec![], set_duration_seconds: 40, transition_seconds: 60, source: "user".into() }).unwrap();
        // The owner did face pulls instead of the pec deck, and tells us the next day.
        let set = |exercise: &str, reps, weight| PastSet { exercise: exercise.into(), reps, weight };
        let sets = [set("bench", 10, 80.0), set("bench", 10, 80.0), set("bench", 10, 80.0), set("face-pull", 15, 20.0)];
        let logged = log_past(&db, &sets, at(0), Some("upper"), at(86400), "tg-9-past").unwrap();
        assert_eq!(logged["data"]["sets"], 4);
        assert_eq!(log_past(&db, &sets, at(0), Some("upper"), at(86400), "tg-9-past").unwrap(), logged);
        assert_eq!(ExerciseProgression::get(&db, "bench").unwrap().unwrap().next_weight, 82.5);
        assert_eq!(SetLog::select(&db, "", []).unwrap().len(), 4);
        assert!(NotificationJob::first(&db, "WHERE status = 'PENDING'", []).unwrap().is_none());
        assert!(get_template(&db, "upper").unwrap().exercises.iter().any(|e| e.id == "pec-deck"));
    }
}
