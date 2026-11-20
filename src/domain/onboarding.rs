//! Mandatory owner interview, then an owner-confirmed plan.
//!
//! Steps: INTERVIEW (one question at a time) → PLAN (owner's plan from a photo, or one gymclaw builds from
//! the catalog) → REVIEW (one summary, owner confirms) → READY. Defaults never count as answers. Ready
//! stays ready: later profile, template or question changes don't send the owner back into setup.
//! Finishing turns on reminders and weekly planning (if runtime isn't configured yet); calendar writes
//! and crowd polling remain opt-in.
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

/// Asked in this order (5 compact questions). Profile-backed answers also write their profile fields.
/// `gym_address` is optional (home trainers set `commute_to_gym_minutes: 0` with no address).
pub const QUESTIONS: [(&str, &str, &[&str]); 5] = [
    (
        "goal",
        "What's your main goal (build muscle, get stronger, lose fat, or general fitness) and how long have you been training?",
        &[],
    ),
    (
        "schedule",
        "How many days a week do you want to train, which days, how long per session, and what time window works?",
        &[
            "weekly_target_sessions",
            "weekdays_allowed",
            "preferred_workout_minutes",
            "earliest_workout_start",
            "latest_workout_finish",
        ],
    ),
    (
        "where",
        "Where do you train (full gym, basic gym, or at home)? If a gym, how long to get ready and get there (address optional)?",
        &["prep_minutes", "commute_to_gym_minutes"],
    ),
    ("preferences", "Do you use kg or lb, and what's your timezone?", &["weight_unit", "timezone"]),
    ("limitations", "Any injuries or exercises you want to avoid?", &[]),
];

pub enum Update<'a> {
    /// Interview answers (question key → owner's words) and the profile fields backing them.
    Answer { answers: &'a Map<String, Value>, profile: Option<&'a Map<String, Value>> },
    /// The owner's plan, set in setup and whenever it changes: its template IDs (also the default
    /// rotation order) and, optionally, the plan in the owner's words.
    ConfirmPlan { templates: &'a [String], plan: Option<&'a str> },
    /// The review fingerprint from `status`.
    Finish(Option<&'a str>),
}

/// Compact JSON with sorted keys, so equal values give equal text.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> =
                keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical(&map[*k]))).collect();
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
        completed_fingerprint: None,
        interview_json: Json(json!({})),
        split_json: Json(vec![]),
        plan_text: None,
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
    let templates = row
        .split_json
        .iter()
        .map(|t| Ok(serde_json::to_value(get_template(db, t)?)?))
        .collect::<Result<Vec<Value>>>()?;
    Ok(fingerprint(&json!({"interview": row.interview_json.0, "profile": get_profile(db)?, "split": templates})))
}

fn missing(row: &OnboardingState) -> Vec<&'static str> {
    QUESTIONS.iter().map(|q| q.0).filter(|key| row.interview_json.get(*key).is_none()).collect()
}

fn step_and_instruction(row: &OnboardingState, missing: &[&'static str]) -> (&'static str, &'static str) {
    // Once finished, stay READY regardless of later question or profile changes.
    if row.completed_fingerprint.is_some() {
        return (
            "READY",
            "Setup done; reminders and weekly planning are on (the week drafts within a minute). \
             Give one short wrap-up with a few tips (voice notes, scale photos, Strong/Hevy CSV import, Gym app button, /help). \
             Calendar writes stay opt-in.",
        );
    }
    if !missing.is_empty() {
        let prompt = QUESTIONS.iter().find(|q| q.0 == missing[0]).map_or("", |q| q.1);
        return ("INTERVIEW", prompt);
    }
    if row.split_json.is_empty() {
        return ("PLAN", "Do you have a workout plan? Send a photo, CSV export, or I'll build one for you.");
    }
    ("REVIEW", "Summarise the answers and split in one short message; finish when the owner confirms.")
}

/// Where setup stands: step, next question, answers, profile, split and the review fingerprint.
/// Setup is done once the owner confirmed the summary; it never restarts.
pub fn finished(db: &Connection) -> Result<bool> {
    Ok(OnboardingState::get(db, 1)?.is_some_and(|o| o.completed_fingerprint.is_some()))
}

/// Lightweight setup summary for `context::snapshot` (None once finished). Avoids loading templates or hashing.
pub fn prompt_summary(db: &Connection) -> Result<Option<String>> {
    let row = state(db)?;
    if row.completed_fingerprint.is_some() {
        return Ok(None);
    }
    let missing = missing(&row);
    let (step, instruction) = step_and_instruction(&row, &missing);
    let line = match step {
        "INTERVIEW" => format!(
            "Setup: INTERVIEW ({}/{} done) · next: {} (\"{instruction}\") · missing: {}.",
            QUESTIONS.len() - missing.len(),
            QUESTIONS.len(),
            missing[0],
            missing.join(", ")
        ),
        "PLAN" => format!("Setup: PLAN · {instruction}"),
        _ => format!(
            "Setup: REVIEW · {instruction} (call onboarding_status for review_fingerprint before onboarding_finish)"
        ),
    };
    Ok(Some(line))
}

pub fn status(db: &Connection) -> Result<Value> {
    let row = state(db)?;
    let missing = missing(&row);
    let ready = row.completed_fingerprint.is_some();
    let (step, instruction) = step_and_instruction(&row, &missing);
    let settings = RuntimeSettings::get(db, 1)?;
    let fingerprint = if row.split_json.is_empty() { None } else { Some(review_fingerprint(db, &row)?) };
    Ok(json!({
        "step": step, "ready": ready, "next_question": (!ready).then(|| missing.first().copied()).flatten(),
        "missing": if ready { vec![] } else { missing },
        "answers": row.interview_json.0, "profile": get_profile(db)?, "split": row.split_json.0, "plan": row.plan_text, "templates": template_options(db)?,
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

/// Enable reminders and weekly planning on the first transition to finished, only when no runtime row
/// exists yet (never overrides a later pause). Calendar writes and crowd polling stay off.
fn enable_runtime_once(db: &Connection) -> Result<()> {
    if RuntimeSettings::get(db, 1)?.is_some() {
        return Ok(());
    }
    if let Ok(owner) = crate::config::owner_id() {
        RuntimeSettings {
            id: 1,
            recipient: owner.to_string(),
            enabled: true,
            calendar_writes_enabled: false,
            crowd_polling_enabled: false,
        }
        .insert(db)?;
    }
    Ok(())
}

/// Answer questions, confirm the plan or finish. Mutation result envelope with the new `status`.
pub fn update(db: &Connection, update: Update, now: Ts, request_id: &str) -> Result<Value> {
    let (operation, intent) = match &update {
        Update::Answer { answers, profile } => {
            ("answer", json!({"answers": answers, "profile": profile, "template_ids": null, "fingerprint": null}))
        }
        Update::ConfirmPlan { templates, plan } => (
            "confirm-plan",
            json!({"answers": null, "profile": null, "template_ids": templates, "plan": plan, "fingerprint": null}),
        ),
        Update::Finish(fingerprint) => {
            ("finish", json!({"answers": null, "profile": null, "template_ids": null, "fingerprint": fingerprint}))
        }
    };
    mutate(db, &format!("onboarding.{operation}"), request_id, intent, now, || {
        let mut row = state(db)?;
        let mut needs_profile: Map<String, Value> = Map::new();
        match &update {
            Update::Answer { answers, profile } => {
                if answers.is_empty() {
                    crate::bail!("ONBOARDING_ANSWER_REQUIRED", "Supply at least one interview answer");
                }
                let unknown: BTreeSet<&str> =
                    answers.keys().map(String::as_str).filter(|k| !QUESTIONS.iter().any(|q| q.0 == *k)).collect();
                if !unknown.is_empty() {
                    crate::bail!(
                        "ONBOARDING_UNKNOWN_QUESTION",
                        "Unknown interview keys: {}",
                        unknown.into_iter().collect::<Vec<_>>().join(", ")
                    );
                }
                if answers.values().any(|v| v.as_str().is_none_or(|s| !(1..=300).contains(&s.trim().chars().count()))) {
                    crate::bail!("ONBOARDING_ANSWER_REQUIRED", "Answers are short owner-stated text (1–300 chars)");
                }
                // Save every answer whose required profile fields were supplied; report any incomplete
                // ones instead of rejecting the whole batch (#114).
                let mut accepted: Vec<(&String, &Value)> = Vec::new();
                for (key, value) in answers.iter() {
                    let required = QUESTIONS.iter().find(|q| q.0 == key.as_str()).map_or(&[][..], |q| q.2);
                    let lacking: Vec<&str> =
                        required.iter().copied().filter(|f| !profile.is_some_and(|p| p.contains_key(*f))).collect();
                    if lacking.is_empty() {
                        accepted.push((key, value));
                    } else {
                        needs_profile.insert(key.clone(), json!(lacking));
                    }
                }
                if accepted.is_empty() && profile.is_none_or(Map::is_empty) {
                    let fields: BTreeSet<&str> = needs_profile
                        .values()
                        .filter_map(Value::as_array)
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    crate::bail!(
                        "ONBOARDING_PROFILE_REQUIRED",
                        "Also set profile fields: {}",
                        fields.into_iter().collect::<Vec<_>>().join(", ")
                    );
                }
                if let Some(profile) = profile.filter(|p| !p.is_empty()) {
                    apply_profile(db, profile)?;
                }
                if let Some(interview) = row.interview_json.as_object_mut() {
                    for (key, value) in accepted {
                        interview.insert(key.clone(), json!(value.as_str().unwrap_or_default().trim()));
                    }
                }
            }
            Update::ConfirmPlan { templates, plan } => {
                if templates.is_empty() {
                    crate::bail!(
                        "ONBOARDING_TEMPLATE_REQUIRED",
                        "Choose one or more confirmed workout templates, in rotation order"
                    );
                }
                for id in templates.iter() {
                    if ["demo", "demo_fixture", "fixture", "synthetic"]
                        .contains(&get_template(db, id)?.source.to_lowercase().as_str())
                    {
                        crate::bail!(
                            "ONBOARDING_REAL_TEMPLATE_REQUIRED",
                            "Demo templates are not real owner training plans"
                        );
                    }
                }
                row.split_json = Json(templates.to_vec());
                if let Some(plan) = plan {
                    row.plan_text = Some(plan.trim().to_string()).filter(|p| !p.is_empty());
                }
            }
            Update::Finish(expected) => {
                let current = status(db)?;
                let fingerprint = current["review_fingerprint"].as_str();
                if !matches!(current["step"].as_str(), Some("REVIEW" | "READY"))
                    || fingerprint.is_none()
                    || *expected != fingerprint
                {
                    crate::bail!(
                        "ONBOARDING_INCOMPLETE",
                        "Finish the interview and confirm a plan, then use the current review fingerprint"
                    );
                }
                let first_finish = row.completed_fingerprint.is_none();
                row.completed_fingerprint = fingerprint.map(String::from);
                if first_finish {
                    enable_runtime_once(db)?;
                }
            }
        }
        row.update(db)?;
        let mut out = status(db)?;
        if !needs_profile.is_empty() {
            out["needs_profile"] = Value::Object(needs_profile);
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::WeightUnit;
    use crate::db::test::db;
    use crate::domain::templates::{Template, import_template};
    use crate::domain::workout::tests::{at, spec};

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn template(db: &Connection, id: &str, source: &str) {
        let row =
            spec(json!({"id": "row", "name": "Row", "role": "pull", "guide_id": "seated-row", "target_weight": 40}));
        import_template(
            db,
            &Template {
                id: id.into(),
                name: id.into(),
                exercises: vec![row],
                alternatives: vec![],
                set_duration_seconds: 40,
                transition_seconds: 60,
                source: source.into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn interview_plan_review_finish() {
        let db = db();
        assert_eq!(status(&db).unwrap()["next_question"], "goal");
        // Batch with one missing profile answer still saves valid answers (#114).
        let mixed = object(json!({"goal": "Build muscle, 2 years", "schedule": "3x, Mon/Wed/Fri"}));
        let partial = update(&db, Update::Answer { answers: &mixed, profile: None }, at(0), "mixed").unwrap();
        assert_eq!(partial["data"]["answers"]["goal"], "Build muscle, 2 years");
        assert!(partial["data"]["needs_profile"]["schedule"].is_array());
        // Purely missing profile fields with no valid answer fails.
        let sched_only = object(json!({"schedule": "3x, Mon/Wed/Fri"}));
        let err = update(&db, Update::Answer { answers: &sched_only, profile: None }, at(0), "no-profile").unwrap_err();
        assert_eq!(err.code(), "ONBOARDING_PROFILE_REQUIRED");
        // Home gym needs no gym_address and zeros commute.
        let rest = object(json!({
            "schedule": "3x, Mon/Wed/Fri, 60 min, 10:30-22:00",
            "where": "at home",
            "preferences": "lb, America/New_York",
            "limitations": "none"
        }));
        let profile = object(json!({
            "weekly_target_sessions": 3,
            "weekdays_allowed": [0, 2, 4],
            "preferred_workout_minutes": 60,
            "earliest_workout_start": "10:30",
            "latest_workout_finish": "22:00",
            "prep_minutes": 5,
            "commute_to_gym_minutes": 0,
            "weight_unit": "lb",
            "timezone": "America/New_York"
        }));
        let result = update(&db, Update::Answer { answers: &rest, profile: Some(&profile) }, at(0), "rest").unwrap();
        assert_eq!(result["data"]["step"], "PLAN");
        let p = get_profile(&db).unwrap();
        assert_eq!((p.weekly_max_sessions, p.minimum_workout_minutes, p.commute_home_minutes), (3, 45, 0));
        assert_eq!((p.weight_unit, p.timezone.as_str(), p.gym_address), (WeightUnit::Lb, "America/New_York", None));
        template(&db, "demo", "demo");
        assert_eq!(
            update(&db, Update::ConfirmPlan { templates: &["demo".into()], plan: None }, at(0), "demo")
                .unwrap_err()
                .code(),
            "ONBOARDING_REAL_TEMPLATE_REQUIRED"
        );
        template(&db, "push", "user");
        let review = update(&db, Update::ConfirmPlan { templates: &["push".into()], plan: None }, at(0), "plan")
            .unwrap()["data"]
            .clone();
        assert_eq!(review["step"], "REVIEW");
        assert_eq!(
            update(&db, Update::Finish(Some("stale")), at(0), "bad").unwrap_err().code(),
            "ONBOARDING_INCOMPLETE"
        );
        let done = update(&db, Update::Finish(review["review_fingerprint"].as_str()), at(0), "finish").unwrap();
        assert_eq!(
            (done["data"]["ready"].as_bool(), done["data"]["calendar_writes_enabled"].as_bool()),
            (Some(true), Some(false))
        );
        // Later edits or old interview_json keys don't send the owner back into setup.
        db.execute("UPDATE onboarding_state SET interview_json = '{\"legacy\": \"yes\"}' WHERE id = 1", []).unwrap();
        assert_eq!(status(&db).unwrap()["step"], "READY");
        assert!(prompt_summary(&db).unwrap().is_none());
    }

    #[test]
    fn finish_never_overrides_paused_runtime_and_is_idempotent() {
        let db = db();
        // Pre-seed a paused runtime row: finishing must never unpause it.
        RuntimeSettings { enabled: false, ..crate::db::test::authority() }.insert(&db).unwrap();
        let answers = object(json!({
            "goal": "Fitness",
            "schedule": "3x",
            "where": "full gym",
            "preferences": "kg, Europe/Berlin",
            "limitations": "none"
        }));
        let profile = object(json!({
            "weekly_target_sessions": 3,
            "weekdays_allowed": [0, 2, 4],
            "preferred_workout_minutes": 60,
            "earliest_workout_start": "10:30",
            "latest_workout_finish": "22:00",
            "prep_minutes": 10,
            "commute_to_gym_minutes": 15,
            "weight_unit": "kg",
            "timezone": "Europe/Berlin"
        }));
        let first = update(&db, Update::Answer { answers: &answers, profile: Some(&profile) }, at(0), "one").unwrap();
        assert_eq!(
            update(&db, Update::Answer { answers: &answers, profile: Some(&profile) }, at(0), "one").unwrap(),
            first
        );
        assert_eq!(
            update(&db, Update::Answer { answers: &object(json!({"goal": "Strength"})), profile: None }, at(0), "one")
                .unwrap_err()
                .code(),
            "IDEMPOTENCY_CONFLICT"
        );
        template(&db, "push", "user");
        let review = update(&db, Update::ConfirmPlan { templates: &["push".into()], plan: None }, at(0), "plan")
            .unwrap()["data"]
            .clone();
        update(&db, Update::Finish(review["review_fingerprint"].as_str()), at(0), "finish").unwrap();
        assert!(!RuntimeSettings::get(&db, 1).unwrap().unwrap().enabled);
    }
}
