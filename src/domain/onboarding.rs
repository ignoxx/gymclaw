//! Mandatory owner interview, then an owner-confirmed plan.
//!
//! Steps: INTERVIEW (three short questions, one at a time) → PLAN (owner's plan from screenshots or a
//! list, or one gymclaw builds from the catalog) → REVIEW (one summary, owner confirms) → READY.
//! Defaults never count as answers, and the interview asks only what planning can't do without: the
//! rest of the profile keeps its defaults until the owner mentions it. Ready stays ready: later
//! profile, template or question changes don't send the owner back into setup.
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

/// One interview question: what the owner is asked, the profile fields its answer must set, and how
/// the agent saves it (only the agent sees `how`).
pub struct Question {
    pub key: &'static str,
    pub ask: &'static str,
    pub needs: &'static [&'static str],
    pub how: &'static str,
}

/// Asked in this order. Anything else (session length, time window, prep, commute, injuries) is set
/// only when the owner brings it up, so the agent never has to make it up to get past a question.
pub const QUESTIONS: [Question; 3] = [
    Question {
        key: "goal",
        ask: "What are you training for? Tap one or say it in your own words; mixing goals is fine.",
        needs: &[],
        how: "Save their words as said (goals, and experience if they gave it); no profile fields.",
    },
    Question {
        key: "routine",
        ask: "How many days a week do you want to train, which days or times usually work, and do you train at a gym or at home?",
        needs: &["weekly_target_sessions", "weekdays_allowed"],
        how: "Set weekly_target_sessions and weekdays_allowed (0 = Mon … 6 = Sun). Set preferred_workout_minutes, \
              earliest_workout_start, latest_workout_finish, prep_minutes, commute_to_gym_minutes or gym_address only if \
              the owner said them; never ask for or invent them. Training at home: commute_to_gym_minutes 0.",
    },
    Question {
        key: "locale",
        ask: "Last one: what city or timezone are you in?",
        needs: &["timezone", "weight_unit"],
        how: "Set timezone (IANA name for their city) and weight_unit from their country: lb in the US, kg almost \
              everywhere else. Don't ask about the unit; name the one you picked when confirming so they can flip it.",
    },
];

pub enum Update<'a> {
    /// Interview answers (question key → owner's words) and the profile fields backing them.
    Answer { answers: &'a Map<String, Value>, profile: Option<&'a Map<String, Value>> },
    /// The owner's plan, set in setup and whenever it changes: its template IDs (also the default
    /// rotation order) and, optionally, the plan in the owner's words.
    ConfirmPlan { templates: &'a [String], plan: Option<&'a str> },
    /// The review fingerprint from `status`, plus the delivery recipient if runtime should be enabled.
    Finish { fingerprint: Option<&'a str>, recipient: Option<&'a str> },
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
    QUESTIONS.iter().map(|q| q.key).filter(|key| row.interview_json.get(*key).is_none()).collect()
}

/// Interview questions not answered yet; None once setup is finished. The prompt hides profile
/// defaults behind these, so the agent doesn't take a seeded default for the owner's answer.
pub fn unanswered(db: &Connection) -> Result<Option<Vec<&'static str>>> {
    let row = state(db)?;
    Ok(row.completed_fingerprint.is_none().then(|| missing(&row)))
}

fn step_and_instruction(
    row: &OnboardingState,
    missing: &[&'static str],
    runtime_enabled: bool,
) -> (&'static str, &'static str) {
    // Once finished, stay READY regardless of later question or profile changes.
    if row.completed_fingerprint.is_some() {
        return (
            "READY",
            if runtime_enabled {
                "Setup done; reminders and weekly planning are on. Wrap up warmly in a few short lines with a couple of tips \
                 (voice notes, scale photos, Strong/Hevy CSV import, the Gym app button, /help), then ask when they want \
                 their first session, with buttons like \"Start tomorrow\", \"Train today\", \"Start next week\". \
                 The week drafts itself from tomorrow on; act on their answer with session_create/session_update. \
                 Calendar writes stay opt-in."
            } else {
                "Setup done. Runtime reminders are currently off; turn them on when the owner asks. \
                 Calendar writes stay opt-in."
            },
        );
    }
    if let Some(next) = missing.first() {
        return ("INTERVIEW", QUESTIONS.iter().find(|q| q.key == *next).map_or("", |q| q.ask));
    }
    if row.split_json.is_empty() {
        return (
            "PLAN",
            "Ask whether they follow a plan (screenshots, a list, a CSV export) or want one built. When they share one, \
             read every workout and every exercise in that same turn: save each workout with template (action import; \
             exercise names as they wrote them), then plan_set in rotation order, and reply with a short summary of the \
             workouts and their exercises to confirm. Photos are gone from later turns, so if you ask before saving, \
             list the exact workouts and exercises you read in the reply.",
        );
    }
    (
        "REVIEW",
        "Summarise in one short message what the owner told you and the plan's workouts (leave out defaults they never \
         mentioned); finish when the owner confirms.",
    )
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
    let (step, instruction) = step_and_instruction(&row, &missing, false);
    let line = match step {
        "INTERVIEW" => {
            let next = QUESTIONS.iter().find(|q| q.key == missing[0]).expect("missing keys are questions");
            format!(
                "Setup: INTERVIEW ({}/{} done) · next: {} (\"{instruction}\"; {}) · missing: {}.",
                QUESTIONS.len() - missing.len(),
                QUESTIONS.len(),
                next.key,
                next.how,
                missing.join(", ")
            )
        }
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
    let settings = RuntimeSettings::get(db, 1)?;
    let runtime_enabled = settings.as_ref().is_some_and(|s| s.enabled);
    let (step, instruction) = step_and_instruction(&row, &missing, runtime_enabled);
    let fingerprint = if row.split_json.is_empty() { None } else { Some(review_fingerprint(db, &row)?) };
    let next = (!ready).then(|| missing.first().copied()).flatten();
    let how = next.and_then(|key| QUESTIONS.iter().find(|q| q.key == key)).map(|q| q.how);
    Ok(json!({
        "step": step, "ready": ready, "next_question": next, "how_to_save": how,
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
fn enable_runtime_once(db: &Connection, recipient: Option<&str>) -> Result<()> {
    if RuntimeSettings::get(db, 1)?.is_some() {
        return Ok(());
    }
    if let Some(recipient) = recipient {
        RuntimeSettings {
            id: 1,
            recipient: recipient.to_string(),
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
        Update::Finish { fingerprint, .. } => {
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
                    answers.keys().map(String::as_str).filter(|k| !QUESTIONS.iter().any(|q| q.key == *k)).collect();
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
                    let required = QUESTIONS.iter().find(|q| q.key == key.as_str()).map_or(&[][..], |q| q.needs);
                    let lacking: Vec<&str> =
                        required.iter().copied().filter(|f| !profile.is_some_and(|p| p.contains_key(*f))).collect();
                    if lacking.is_empty() {
                        accepted.push((key, value));
                    } else {
                        needs_profile.insert(key.clone(), json!(lacking));
                    }
                }
                if accepted.is_empty() {
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
            Update::Finish { fingerprint: expected, recipient } => {
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
                    enable_runtime_once(db, *recipient)?;
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
        let mixed = object(json!({"goal": "Lose fat and build muscle", "routine": "3x, Mon/Wed/Fri, at home"}));
        let partial = update(&db, Update::Answer { answers: &mixed, profile: None }, at(0), "mixed").unwrap();
        assert_eq!(partial["data"]["answers"]["goal"], "Lose fat and build muscle");
        assert_eq!(partial["data"]["needs_profile"]["routine"], json!(["weekly_target_sessions", "weekdays_allowed"]));
        // Purely missing profile fields with no valid answer fails.
        let locale_only = object(json!({"locale": "New York"}));
        let err =
            update(&db, Update::Answer { answers: &locale_only, profile: None }, at(0), "no-profile").unwrap_err();
        assert_eq!(err.code(), "ONBOARDING_PROFILE_REQUIRED");
        // The routine needs only days and count: what the owner didn't mention keeps its default, and
        // training at home zeros the commute both ways.
        let before = get_profile(&db).unwrap();
        let rest = object(json!({"routine": "3x, Mon/Wed/Fri, at home", "locale": "New York"}));
        let profile = object(json!({
            "weekly_target_sessions": 3,
            "weekdays_allowed": [0, 2, 4],
            "commute_to_gym_minutes": 0,
            "weight_unit": "lb",
            "timezone": "America/New_York"
        }));
        let result = update(&db, Update::Answer { answers: &rest, profile: Some(&profile) }, at(0), "rest").unwrap();
        assert_eq!(result["data"]["step"], "PLAN");
        let p = get_profile(&db).unwrap();
        assert_eq!((p.weekly_max_sessions, p.commute_home_minutes), (3, 0));
        assert_eq!(
            (p.preferred_workout_minutes, p.prep_minutes),
            (before.preferred_workout_minutes, before.prep_minutes)
        );
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
            update(&db, Update::Finish { fingerprint: Some("stale"), recipient: None }, at(0), "bad")
                .unwrap_err()
                .code(),
            "ONBOARDING_INCOMPLETE"
        );
        let done = update(
            &db,
            Update::Finish { fingerprint: review["review_fingerprint"].as_str(), recipient: Some("42") },
            at(0),
            "finish",
        )
        .unwrap();
        assert_eq!(
            (done["data"]["ready"].as_bool(), done["data"]["calendar_writes_enabled"].as_bool()),
            (Some(true), Some(false))
        );
        let settings = RuntimeSettings::get(&db, 1).unwrap().expect("runtime enabled from recipient");
        assert_eq!(
            (settings.recipient.as_str(), settings.enabled, settings.calendar_writes_enabled),
            ("42", true, false)
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
        let answers = object(json!({"goal": "Fitness", "routine": "3x at the gym", "locale": "Berlin"}));
        let profile = object(json!({
            "weekly_target_sessions": 3,
            "weekdays_allowed": [0, 2, 4],
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
        update(
            &db,
            Update::Finish { fingerprint: review["review_fingerprint"].as_str(), recipient: None },
            at(0),
            "finish",
        )
        .unwrap();
        assert!(!RuntimeSettings::get(&db, 1).unwrap().unwrap().enabled);
    }
}
