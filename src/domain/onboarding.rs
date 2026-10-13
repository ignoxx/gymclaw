//! Mandatory owner interview, then an owner-confirmed plan (port of gymclaw/services/onboarding.py). Owner: WP-train.
//!
//! Steps: INTERVIEW (one question at a time) → PLAN (owner's plan from a photo, or one GymClaw builds from
//! the catalog) → REVIEW (one summary, owner confirms) → READY. Defaults never count as answers. Ready
//! stays ready: later profile or template edits don't send the owner back into setup. Grants no runtime
//! or calendar authority.
use std::collections::BTreeSet;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::db::models::{OnboardingState, RuntimeSettings, WorkoutTemplate};
use crate::db::{Connection, Json};
use crate::domain::core::mutate;
use crate::domain::preview::ensure_ascii;
use crate::domain::profile::{get_profile, update_profile};
use crate::domain::templates::get_template;
use crate::error::Result;
use crate::time::Ts;

/// Asked in this order. Profile-backed answers must also write these profile fields.
pub const QUESTIONS: [(&str, &str, &[&str]); 8] = [
    ("goal", "What's your main goal: build muscle, get stronger, lose fat, or general fitness?", &[]),
    ("experience", "How long have you been training consistently?", &[]),
    ("schedule", "How many days a week do you want to train, and which days work?", &["weekly_target_sessions", "weekdays_allowed"]),
    ("session_length", "How long can one gym session be?", &["preferred_workout_minutes"]),
    ("time_window", "When can you train: earliest start and latest finish?", &["earliest_workout_start", "latest_workout_finish"]),
    ("travel", "Which gym (address), and how long do you need to get ready and to get there?", &["gym_address", "prep_minutes", "commute_to_gym_minutes"]),
    ("equipment", "Where do you train: full gym, basic gym, or at home?", &[]),
    ("limitations", "Any injuries or exercises you want to avoid?", &[]),
];

pub enum Update<'a> {
    /// Interview answers (question key → owner's words) and the profile fields backing them.
    Answer { answers: &'a Map<String, Value>, profile: Option<&'a Map<String, Value>> },
    /// Template IDs in rotation order.
    ConfirmPlan(&'a [String]),
    /// The review fingerprint from `status`.
    Finish(Option<&'a str>),
}

/// Python `json.dumps(value, sort_keys=True, separators=(",", ":"))`.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical(&map[*k]))).collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

fn fingerprint(value: &Value) -> String {
    hex::encode(Sha256::digest(ensure_ascii(&canonical(value)).as_bytes()))
}

fn state(db: &Connection) -> Result<OnboardingState> {
    if let Some(row) = OnboardingState::get(db, 1)? {
        return Ok(row);
    }
    let row = OnboardingState {
        id: 1,
        goal: None,
        profile_fingerprint: None,
        template_id: None,
        template_fingerprint: None,
        completed_fingerprint: None,
        interview_json: Json(json!({})),
        split_json: Json(vec![]),
    };
    row.insert(db)?;
    Ok(row)
}

fn template_options(db: &Connection) -> Result<Vec<Value>> {
    Ok(WorkoutTemplate::select(db, "ORDER BY id", [])?
        .into_iter()
        .map(|t| {
            let d = &t.definition_json;
            json!({"id": t.id, "name": d["name"], "source": d.get("source").cloned().unwrap_or(json!("user")),
                "exercise_count": d["exercises"].as_array().map_or(0, Vec::len)})
        })
        .collect())
}

fn review_fingerprint(db: &Connection, row: &OnboardingState) -> Result<String> {
    let templates = row.split_json.iter().map(|t| Ok(serde_json::to_value(get_template(db, t)?)?)).collect::<Result<Vec<Value>>>()?;
    Ok(fingerprint(&json!({"interview": row.interview_json.0, "profile": get_profile(db)?, "split": templates})))
}

fn missing(row: &OnboardingState) -> Vec<&'static str> {
    QUESTIONS.iter().map(|q| q.0).filter(|key| row.interview_json.get(*key).is_none()).collect()
}

/// Where setup stands: step, next question, answers, profile, split and the review fingerprint.
pub fn status(db: &Connection) -> Result<Value> {
    let row = state(db)?;
    let missing = missing(&row);
    let ready = missing.is_empty() && !row.split_json.is_empty() && row.completed_fingerprint.is_some();
    let step = if ready {
        "READY"
    } else if !missing.is_empty() {
        "INTERVIEW"
    } else if row.split_json.is_empty() {
        "PLAN"
    } else {
        "REVIEW"
    };
    let instruction = match step {
        "INTERVIEW" => QUESTIONS.iter().find(|q| q.0 == missing[0]).map_or("", |q| q.1),
        "PLAN" => "Do you have a workout plan? Send a photo, or I'll build one for you.",
        "REVIEW" => "Summarise the answers and split in one short message; finish when the owner confirms.",
        _ => "Setup done. Plan the week; calendar publication still needs its own approval.",
    };
    let settings = RuntimeSettings::get(db, 1)?;
    let fingerprint = if row.split_json.is_empty() { None } else { Some(review_fingerprint(db, &row)?) };
    Ok(json!({
        "step": step, "ready": ready, "next_question": missing.first(), "missing": missing,
        "answers": row.interview_json.0, "profile": get_profile(db)?, "split": row.split_json.0, "templates": template_options(db)?,
        "review_fingerprint": fingerprint,
        "calendar_writes_enabled": settings.as_ref().is_some_and(|s| s.calendar_writes_enabled),
        "runtime_enabled": settings.as_ref().is_some_and(|s| s.enabled), "instruction": instruction,
    }))
}

/// Weekly min/max and minimum session length follow the owner's answers so validation holds.
fn apply_profile(db: &Connection, changes: &Map<String, Value>) -> Result<()> {
    let current = get_profile(db)?;
    let mut derived = changes.clone();
    if let Some(target) = changes.get("weekly_target_sessions") {
        derived.insert("weekly_max_sessions".into(), target.clone());
        if let Some(t) = target.as_i64() {
            derived.insert("weekly_min_sessions".into(), json!(current.weekly_min_sessions.min(t)));
        }
    }
    if let Some(minutes) = changes.get("preferred_workout_minutes").and_then(Value::as_i64) {
        derived.insert("minimum_workout_minutes".into(), json!(current.minimum_workout_minutes.min(minutes)));
    }
    if let Some(commute) = changes.get("commute_to_gym_minutes")
        && !changes.contains_key("commute_home_minutes")
    {
        derived.insert("commute_home_minutes".into(), commute.clone());
    }
    update_profile(db, &derived)?;
    Ok(())
}

/// Answer questions, confirm the plan or finish. Mutation result envelope with the new `status`.
pub fn update(db: &Connection, update: Update, now: Ts, request_id: &str) -> Result<Value> {
    let (operation, intent) = match &update {
        Update::Answer { answers, profile } => ("answer", json!({"answers": answers, "profile": profile, "template_ids": null, "fingerprint": null})),
        Update::ConfirmPlan(ids) => ("confirm-plan", json!({"answers": null, "profile": null, "template_ids": ids, "fingerprint": null})),
        Update::Finish(fingerprint) => ("finish", json!({"answers": null, "profile": null, "template_ids": null, "fingerprint": fingerprint})),
    };
    mutate(db, &format!("onboarding.{operation}"), request_id, intent, now, || {
        let mut row = state(db)?;
        match &update {
            Update::Answer { answers, profile } => {
                if answers.is_empty() {
                    crate::bail!("ONBOARDING_ANSWER_REQUIRED", "Supply at least one interview answer");
                }
                let unknown: BTreeSet<&str> = answers.keys().map(String::as_str).filter(|k| !QUESTIONS.iter().any(|q| q.0 == *k)).collect();
                if !unknown.is_empty() {
                    crate::bail!("ONBOARDING_UNKNOWN_QUESTION", "Unknown interview keys: {}", unknown.into_iter().collect::<Vec<_>>().join(", "));
                }
                if answers.values().any(|v| v.as_str().is_none_or(|s| !(1..=300).contains(&s.trim().chars().count()))) {
                    crate::bail!("ONBOARDING_ANSWER_REQUIRED", "Answers are short owner-stated text (1–300 chars)");
                }
                let needed: BTreeSet<&str> = QUESTIONS.iter().filter(|q| answers.contains_key(q.0)).flat_map(|q| q.2.iter().copied()).filter(|f| !profile.is_some_and(|p| p.contains_key(*f))).collect();
                if !needed.is_empty() {
                    crate::bail!("ONBOARDING_PROFILE_REQUIRED", "Also set profile fields: {}", needed.into_iter().collect::<Vec<_>>().join(", "));
                }
                if let Some(profile) = profile.filter(|p| !p.is_empty()) {
                    apply_profile(db, profile)?;
                }
                if let Some(interview) = row.interview_json.as_object_mut() {
                    for (key, value) in answers.iter() {
                        interview.insert(key.clone(), json!(value.as_str().unwrap_or_default().trim()));
                    }
                }
                if let Some(goal) = answers.get("goal").and_then(Value::as_str) {
                    row.goal = Some(goal.trim().into());
                }
            }
            Update::ConfirmPlan(ids) => {
                if ids.is_empty() {
                    crate::bail!("ONBOARDING_TEMPLATE_REQUIRED", "Choose one or more confirmed workout templates, in rotation order");
                }
                for id in ids.iter() {
                    if ["demo", "demo_fixture", "fixture", "synthetic"].contains(&get_template(db, id)?.source.to_lowercase().as_str()) {
                        crate::bail!("ONBOARDING_REAL_TEMPLATE_REQUIRED", "Demo templates are not real owner training plans");
                    }
                }
                row.split_json = Json(ids.to_vec());
                row.template_id = Some(ids[0].clone());
            }
            Update::Finish(expected) => {
                let current = status(db)?;
                let fingerprint = current["review_fingerprint"].as_str();
                if !matches!(current["step"].as_str(), Some("REVIEW" | "READY")) || fingerprint.is_none() || *expected != fingerprint {
                    crate::bail!("ONBOARDING_INCOMPLETE", "Finish the interview and confirm a plan, then use the current review fingerprint");
                }
                row.completed_fingerprint = fingerprint.map(String::from);
            }
        }
        row.update(db)?;
        status(db)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::templates::{Template, import_template};
    use crate::domain::workout::tests::{at, db, spec};

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn template(db: &Connection, id: &str, source: &str) {
        let row = spec(json!({"id": "row", "name": "Row", "role": "pull", "guide_id": "seated-row", "target_weight": 40}));
        import_template(db, &Template { id: id.into(), name: id.into(), exercises: vec![row], alternatives: vec![], set_duration_seconds: 40, transition_seconds: 60, source: source.into() }).unwrap();
    }

    #[test]
    fn interview_plan_review_finish() {
        let db = db();
        assert_eq!(status(&db).unwrap()["next_question"], "goal");
        let schedule = object(json!({"schedule": "3x, Mon/Wed/Fri", "session_length": "60 min", "time_window": "10:30 to 22:00", "travel": "10 min prep, 15 min drive"}));
        let err = update(&db, Update::Answer { answers: &schedule, profile: None }, at(0), "no-profile").unwrap_err();
        assert_eq!(err.code(), "ONBOARDING_PROFILE_REQUIRED");
        let basics = object(json!({"goal": "Build muscle", "experience": "2 years", "equipment": "full gym", "limitations": "none"}));
        update(&db, Update::Answer { answers: &basics, profile: None }, at(0), "basics").unwrap();
        let profile = object(json!({"weekly_target_sessions": 3, "weekdays_allowed": [0, 2, 4], "preferred_workout_minutes": 60, "earliest_workout_start": "10:30",
            "latest_workout_finish": "22:00", "prep_minutes": 10, "commute_to_gym_minutes": 15, "gym_address": "Example Str. 1, Berlin"}));
        let result = update(&db, Update::Answer { answers: &schedule, profile: Some(&profile) }, at(0), "schedule").unwrap();
        assert_eq!(result["data"]["step"], "PLAN");
        let p = get_profile(&db).unwrap();
        assert_eq!((p.weekly_max_sessions, p.minimum_workout_minutes, p.commute_home_minutes), (3, 45, 15));
        template(&db, "demo", "demo");
        assert_eq!(update(&db, Update::ConfirmPlan(&["demo".into()]), at(0), "demo").unwrap_err().code(), "ONBOARDING_REAL_TEMPLATE_REQUIRED");
        template(&db, "push", "user");
        let review = update(&db, Update::ConfirmPlan(&["push".into()]), at(0), "plan").unwrap()["data"].clone();
        assert_eq!(review["step"], "REVIEW");
        assert_eq!(update(&db, Update::Finish(Some("stale")), at(0), "bad").unwrap_err().code(), "ONBOARDING_INCOMPLETE");
        let done = update(&db, Update::Finish(review["review_fingerprint"].as_str()), at(0), "finish").unwrap();
        assert_eq!((done["data"]["ready"].as_bool(), done["data"]["calendar_writes_enabled"].as_bool()), (Some(true), Some(false)));
        // Later edits don't send the owner back into setup.
        template(&db, "push", "user-edited");
        assert_eq!(status(&db).unwrap()["step"], "READY");
    }

}
