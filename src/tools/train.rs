//! Workout, coach, template, onboarding and body tools. Owner: WP-train.
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Ctx, Effect, Tool, ToolDef, def};
use crate::domain::backfill::{PastSet, log_past};
use crate::domain::body::{self, WeighIn};
use crate::domain::coach::{self, Action};
use crate::domain::illustrations::search;
use crate::domain::onboarding::{self, Update};
use crate::domain::profile::get_profile;
use crate::domain::templates::{ExerciseEdit, Template, edit_exercise, get_template, import_template, require_illustrations};
use crate::domain::view::CoachView;
use crate::domain::{audit, workout};
use crate::error::Result;
use crate::time::Ts;

pub fn tools() -> Vec<ToolDef> {
    vec![
        def::<Workout>(),
        def::<WorkoutLogPast>(),
        def::<WorkoutAudit>(),
        def::<SetCorrect>(),
        def::<TemplateGet>(),
        def::<TemplateImport>(),
        def::<TemplateEditExercise>(),
        def::<TemplateAddExercise>(),
        def::<TemplateRemoveExercise>(),
        def::<CatalogSearch>(),
        def::<OnboardingStatus>(),
        def::<OnboardingAnswer>(),
        def::<OnboardingConfirmPlan>(),
        def::<OnboardingFinish>(),
        def::<BodyLog>(),
        def::<BodyList>(),
        def::<BodyDelete>(),
    ]
}

/// Owner-local time unless the text has an offset.
fn local(cx: &Ctx, text: &str) -> Result<Ts> {
    Ts::parse(text, Some(&get_profile(cx.db)?.tz()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkoutAction {
    /// Read the running workout; sends nothing.
    Status,
    /// Next (or the given) planned session: one image with every exercise plus the plan. For "what's on today".
    Preview,
    /// No args: today's planned session. template_id/planned_session_id only to start something else.
    Start,
    /// `text` like "10x40" (10 reps at 40 kg) logs a set.
    Log,
    /// Resend the current card.
    Card,
    /// Show up to 3 same-muscle alternatives as buttons (only before the first set).
    Swap,
    /// `exercise`: the owner is doing this now (any exercise, any time); finishes or replaces the current one.
    Switch,
    /// `exercise`: what the owner really did; `which`: the logged exercise to fix (default the latest). Works after the workout too.
    Relabel,
    /// `seconds` between sets for this workout; `remember` also makes it the default.
    Rest,
    /// Do the current exercise later.
    Later,
    /// Move on to the next exercise.
    Next,
    /// End early, save, show the summary.
    End,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workout {
    pub action: WorkoutAction,
    /// start: a template to start instead of today's planned session.
    pub template_id: Option<String>,
    /// log: the set, reps first, e.g. "10x40" (bodyweight: "13 reps").
    pub text: Option<String>,
    /// start/preview: a specific planned session.
    pub planned_session_id: Option<String>,
    /// switch/relabel: guide_id or exact exercise name.
    pub exercise: Option<String>,
    /// relabel: the logged exercise to correct.
    pub which: Option<String>,
    /// rest: seconds between sets.
    pub seconds: Option<i64>,
    /// rest: also the default for future workouts.
    #[serde(default)]
    pub remember: bool,
}

/// What the owner now sees, like the old plugin returned it.
fn shown(view: &CoachView) -> Value {
    let texts: Vec<&str> = view.cards.iter().map(|c| c.text.as_str()).collect();
    if view.live_buttons.is_some() { json!({"ack": view.ack, "swap_options": texts}) } else { json!({"ack": view.ack, "card": texts.first()}) }
}

impl Tool for Workout {
    const NAME: &'static str = "workout";
    const DESCRIPTION: &'static str = "Drive the live workout in Telegram. Sends exercise cards (image, target, quick buttons) to the owner directly, \
        so after calling it don't repeat the card; reply only if the owner asked a question. The result's `card` is what the owner now sees. \
        `exercise` takes a guide_id or exact name; on EXERCISE_UNKNOWN pick one of the suggested guide_ids.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let missing = match self.action {
            WorkoutAction::Log if self.text.as_deref().is_none_or(str::is_empty) => Some(("log", "text", r#"{"action":"log","text":"10x40"}"#)),
            WorkoutAction::Switch if self.exercise.as_deref().is_none_or(str::is_empty) => Some(("switch", "exercise", r#"{"action":"switch","exercise":"pec-deck"}"#)),
            WorkoutAction::Relabel if self.exercise.as_deref().is_none_or(str::is_empty) => Some(("relabel", "exercise", r#"{"action":"relabel","exercise":"pec-deck"}"#)),
            WorkoutAction::Rest if self.seconds.is_none() => Some(("rest", "seconds", r#"{"action":"rest","seconds":90}"#)),
            _ => None,
        };
        if let Some((action, field, example)) = missing {
            crate::bail!(
                "ARGUMENT_REQUIRED",
                "{action} needs {field}, e.g. {example}. Don't call it again without it; if you don't know the value, ask the owner."
            );
        }
        let (db, now, rid) = (cx.db, cx.now, cx.request_id.clone());
        let coach_action = |action| coach::handle_action(db, action, self.exercise.as_deref(), self.which.as_deref(), self.seconds, self.remember, now, &rid);
        let view = match self.action {
            WorkoutAction::Status => {
                let running = workout::active_workout(db)?;
                return Ok(json!({"workout": running.map(|w| workout::current(db, &w.id, now)).transpose()?}));
            }
            WorkoutAction::Preview => coach::preview(db, now, self.planned_session_id.as_deref(), cx.data_dir)?,
            WorkoutAction::Log => {
                let view = coach::handle_text(db, self.text.as_deref().unwrap_or_default(), now, &rid)?;
                if !view.handled {
                    // Text that isn't a set, or no workout in a loggable state: nothing happened.
                    crate::bail!("NOT_LOGGED", "Nothing logged: no running set, or the text isn't reps × weight (e.g. 10x40; bodyweight: 13 reps). Check status, then ask the owner.");
                }
                view
            }
            WorkoutAction::Start => {
                workout::start(db, self.template_id.as_deref(), self.planned_session_id.as_deref(), false, now, &rid)?;
                coach_action(Action::Card)?
            }
            WorkoutAction::Card => coach_action(Action::Card)?,
            WorkoutAction::Swap => coach_action(Action::Swap)?,
            WorkoutAction::Switch => coach_action(Action::Switch)?,
            WorkoutAction::Relabel => coach_action(Action::Relabel)?,
            WorkoutAction::Rest => coach_action(Action::Rest)?,
            WorkoutAction::Later => coach_action(Action::Later)?,
            WorkoutAction::Next => coach_action(Action::Next)?,
            WorkoutAction::End => coach_action(Action::End)?,
        };
        let result = shown(&view);
        cx.effects.push(Effect::Coach(view));
        Ok(result)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkoutLogPast {
    /// When the workout started (owner-local unless it has an offset), e.g. "2026-10-12T18:30".
    pub at: String,
    /// Sets in the order done, e.g. [{"exercise":"bench-press","reps":8,"weight":80}].
    pub sets: Vec<PastSet>,
    /// Template to log against; default: that day's planned session.
    pub template_id: Option<String>,
}

impl Tool for WorkoutLogPast {
    const NAME: &'static str = "workout_log_past";
    const DESCRIPTION: &'static str = "Log a workout the owner did without the bot, afterwards. Counts for history, progression and the week's plan \
        (a missed session that day counts after all). Off-plan exercises don't change the saved plan.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let at = local(cx, &self.at)?;
        log_past(cx.db, &self.sets, at, self.template_id.as_deref(), cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkoutAudit {
    pub workout_id: String,
}

impl Tool for WorkoutAudit {
    const NAME: &'static str = "workout_audit";
    const DESCRIPTION: &'static str = "Saved audit of a finished workout: sets, duration, ETA error, progressions, crowd feedback.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        audit::get_audit(cx.db, &self.workout_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetCorrect {
    /// set_log.id
    pub set_id: String,
    pub weight: Option<f64>,
    pub reps: Option<i64>,
}

impl Tool for SetCorrect {
    const NAME: &'static str = "set_correct";
    const DESCRIPTION: &'static str = "Fix a logged set's weight or reps when the owner says it was logged wrong, during or after the workout. \
        Find the set with sql_query on set_log. Recomputes that exercise's next weight if this workout set it.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        audit::correct_set(cx.db, &self.set_id, self.weight, self.reps, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateGet {
    pub template_id: String,
}

impl Tool for TemplateGet {
    const NAME: &'static str = "template_get";
    const DESCRIPTION: &'static str = "A saved workout plan (template) with its exercises, targets and alternatives.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        Ok(serde_json::to_value(get_template(cx.db, &self.template_id)?)?)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateImport {
    /// The whole plan. Every exercise needs a guide_id from catalog_search; the first primary exercise needs warmup_weight.
    pub template: Template,
}

impl Tool for TemplateImport {
    const NAME: &'static str = "template_import";
    const DESCRIPTION: &'static str = "Save a whole workout plan (new, or replacing one with the same id). For small changes use template_edit_exercise.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        require_illustrations(&self.template)?;
        import_template(cx.db, &self.template)
    }
}

/// Upcoming sessions should follow the edited plan via the planning side's `weekly::refresh_plans`
/// (not declared yet; `workout start` always uses the current template regardless).
/// Upcoming sessions (and their calendar events) follow the edited plan.
fn edited(cx: &Ctx, definition: Value) -> Result<Value> {
    Ok(json!({"template": definition, "refreshed_sessions": crate::domain::weekly::refresh_plans(cx.db, cx.now)?}))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateEditExercise {
    pub template_id: String,
    /// The exercise: ID, guide_id or name.
    pub exercise: String,
    /// Fields to change, e.g. {"target_weight":85,"working_sets":4,"rep_min":6,"rep_max":8}. A new target_weight also resets the learned next weight.
    #[serde(default)]
    pub changes: Map<String, Value>,
    /// 0-based new place in the order.
    pub position: Option<usize>,
}

impl Tool for TemplateEditExercise {
    const NAME: &'static str = "template_edit_exercise";
    const DESCRIPTION: &'static str = "Change one exercise of a saved plan: weight, sets, reps, rest, or its place in the order.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        edited(cx, edit_exercise(cx.db, &self.template_id, &self.exercise, ExerciseEdit::Change(&self.changes), self.position)?)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateAddExercise {
    pub template_id: String,
    /// Catalog guide_id or exact catalog name (see catalog_search).
    pub exercise: String,
    /// Fields overriding the defaults, e.g. {"working_sets":3,"rep_min":10,"rep_max":12,"target_weight":20}.
    #[serde(default)]
    pub fields: Map<String, Value>,
    /// 0-based place in the order; default last.
    pub position: Option<usize>,
}

impl Tool for TemplateAddExercise {
    const NAME: &'static str = "template_add_exercise";
    const DESCRIPTION: &'static str = "Add a catalog exercise to a saved plan.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        edited(cx, edit_exercise(cx.db, &self.template_id, &self.exercise, ExerciseEdit::Add(&self.fields), self.position)?)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateRemoveExercise {
    pub template_id: String,
    /// The exercise: ID, guide_id or name.
    pub exercise: String,
}

impl Tool for TemplateRemoveExercise {
    const NAME: &'static str = "template_remove_exercise";
    const DESCRIPTION: &'static str = "Remove an exercise from a saved plan.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        edited(cx, edit_exercise(cx.db, &self.template_id, &self.exercise, ExerciseEdit::Remove, None)?)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogSearch {
    /// Words from the exercise name, e.g. "incline db press".
    #[serde(default)]
    pub query: String,
    /// Exact primary muscle, e.g. "Chest".
    pub muscle: Option<String>,
    /// Exact equipment, e.g. "Cable".
    pub equipment: Option<String>,
    /// Default 8.
    pub limit: Option<usize>,
}

impl Tool for CatalogSearch {
    const NAME: &'static str = "catalog_search";
    const DESCRIPTION: &'static str = "Find illustrated exercises (guide_id, name, equipment, muscle). Plans and swaps only use catalog exercises.";

    fn run(self, _cx: &mut Ctx) -> Result<Value> {
        let results = search(&self.query, self.muscle.as_deref(), self.equipment.as_deref(), self.limit.unwrap_or(8));
        Ok(results.iter().map(|r| json!({"guide_id": r.guide_id, "name": r.name, "equipment": r.equipment, "primary_muscle": r.primary_muscle})).collect())
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnboardingStatus {}

impl Tool for OnboardingStatus {
    const NAME: &'static str = "onboarding_status";
    const DESCRIPTION: &'static str = "Setup progress: step (INTERVIEW, PLAN, REVIEW, READY), the next question, answers so far, and the review fingerprint.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        onboarding::status(cx.db)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnboardingAnswer {
    /// Question key → the owner's words, e.g. {"experience": "2 years"}. Keys: goal, experience, schedule,
    /// session_length, time_window, travel, equipment, limitations.
    pub answers: Map<String, Value>,
    /// Profile fields backing those answers (schedule, session_length, time_window and travel need them).
    pub profile: Option<Map<String, Value>>,
}

impl Tool for OnboardingAnswer {
    const NAME: &'static str = "onboarding_answer";
    const DESCRIPTION: &'static str = "Save the owner's interview answers (only what they actually said; defaults never count).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        onboarding::update(cx.db, Update::Answer { answers: &self.answers, profile: self.profile.as_ref() }, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnboardingConfirmPlan {
    /// The owner-confirmed templates, in rotation order.
    pub template_ids: Vec<String>,
}

impl Tool for OnboardingConfirmPlan {
    const NAME: &'static str = "onboarding_confirm_plan";
    const DESCRIPTION: &'static str = "Set the owner's confirmed plan (split) during setup.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        onboarding::update(cx.db, Update::ConfirmPlan(&self.template_ids), cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnboardingFinish {
    /// review_fingerprint from onboarding_status.
    pub fingerprint: String,
}

impl Tool for OnboardingFinish {
    const NAME: &'static str = "onboarding_finish";
    const DESCRIPTION: &'static str = "Finish setup after the owner confirmed the summary. Grants no calendar or runtime authority.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        onboarding::update(cx.db, Update::Finish(Some(&self.fingerprint)), cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BodyLog {
    /// One weigh-in, or a whole backfill batch.
    pub entries: Vec<WeighIn>,
}

impl Tool for BodyLog {
    const NAME: &'static str = "body_log";
    const DESCRIPTION: &'static str = "Save weigh-ins (from a scale photo or typed). Old photos sent as files: pass the photo path and its capture date is used. \
        The same time twice is skipped as a duplicate. Echo the number back so a misread scale gets caught.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        body::log(cx.db, &self.entries, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BodyList {}

impl Tool for BodyList {
    const NAME: &'static str = "body_list";
    const DESCRIPTION: &'static str = "All weigh-ins (newest first) and the weekly trend.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let zone = get_profile(cx.db)?.tz();
        let entries: Vec<Value> = body::entries(cx.db)?.iter().rev().map(|row| body::entry_data(row, &zone)).collect();
        Ok(json!({"entries": entries, "trend": body::trend(cx.db, cx.now)?}))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BodyDelete {
    pub entry_id: String,
}

impl Tool for BodyDelete {
    const NAME: &'static str = "body_delete";
    const DESCRIPTION: &'static str = "Remove a misread or mistyped weigh-in.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        body::delete(cx.db, &self.entry_id, cx.now, &cx.request_id)
    }
}
