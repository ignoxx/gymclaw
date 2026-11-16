//! Goals and training outside the gym. Lift trends need no tool: `sql_query` reads the `lift_history` view.
use jiff::civil::Date;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{Ctx, Tool, ToolDef, def};
use crate::db::models::{GoalKind, GoalStatus, Intensity};
use crate::domain::profile::get_profile;
use crate::domain::{activities, goals};
use crate::error::{Error, Result};
use crate::time::Ts;

pub fn tools() -> Vec<ToolDef> {
    vec![def::<GoalTool>(), def::<ActivityTool>()]
}

fn date(text: &str) -> Result<Date> {
    text.trim().parse().map_err(|_| Error::domain("INVALID_DATE", format!("Not a date (YYYY-MM-DD): {text}")))
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GoalAction {
    Set,
    /// Pass only what changes.
    Update,
    /// Reached.
    Done,
    /// Given up.
    Drop,
    /// Active goals with progress.
    List,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoalTool {
    pub action: GoalAction,
    pub goal_id: Option<String>,
    /// The owner's words.
    pub text: Option<String>,
    pub kind: Option<GoalKind>,
    /// lift: workout_exercise.exercise_id.
    pub exercise_id: Option<String>,
    /// kg (lift: est. 1-rep max), min/week or workouts/week.
    pub target: Option<f64>,
    pub target_date: Option<String>,
    /// Your 4–6 week block toward it, deload included.
    pub block: Option<String>,
}

impl Tool for GoalTool {
    const NAME: &'static str = "goal";
    const DESCRIPTION: &'static str = "Owner goals; code tracks progress, you plan the block.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let fields = goals::Fields {
            text: self.text,
            kind: self.kind,
            exercise_id: self.exercise_id,
            target: self.target,
            target_date: self.target_date.as_deref().map(date).transpose()?,
            block: self.block,
        };
        let id = || self.goal_id.as_deref().ok_or_else(|| Error::domain("ARGUMENT_REQUIRED", "Pass goal_id"));
        match self.action {
            GoalAction::Set => goals::set(cx.db, fields, cx.now, &cx.request_id),
            GoalAction::Update => goals::update(cx.db, id()?, fields, cx.now, &cx.request_id),
            GoalAction::Done => goals::close(cx.db, id()?, GoalStatus::Done, cx.now, &cx.request_id),
            GoalAction::Drop => goals::close(cx.db, id()?, GoalStatus::Dropped, cx.now, &cx.request_id),
            GoalAction::List => goals::list(cx.db, cx.now),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActivityAction {
    Log,
    /// Pass only what changes; `until` ends a weekly one.
    Update,
    /// Logged by mistake.
    Remove,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivityTool {
    pub action: ActivityAction,
    pub activity_id: Option<String>,
    /// Owner's words: "run 8k".
    pub kind: Option<String>,
    /// Start; a date alone is midday. Default now.
    pub at: Option<String>,
    pub minutes: Option<i64>,
    /// Default moderate.
    pub intensity: Option<Intensity>,
    pub notes: Option<String>,
    /// Same weekday and time every week.
    pub weekly: Option<bool>,
    /// Weekly: last day (YYYY-MM-DD).
    pub until: Option<String>,
}

impl Tool for ActivityTool {
    const NAME: &'static str = "activity";
    const DESCRIPTION: &'static str = "Training outside the gym (runs, sports, hikes), one-off or weekly; shown in get_week, \
        minutes count toward cardio goals.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let tz = get_profile(cx.db)?.tz();
        let at = match self.at.as_deref().map(str::trim) {
            Some(day) if day.len() == 10 => Some(Ts(date(day)?.at(12, 0, 0, 0).to_zoned(tz)?.timestamp())),
            Some(text) => Some(Ts::parse(text, Some(&tz))?),
            None => None,
        };
        let mut fields = activities::Fields {
            kind: self.kind,
            at,
            minutes: self.minutes,
            intensity: self.intensity,
            notes: self.notes,
            weekly: self.weekly,
            until: self.until.as_deref().map(date).transpose()?,
        };
        let id = || self.activity_id.as_deref().ok_or_else(|| Error::domain("ARGUMENT_REQUIRED", "Pass activity_id"));
        match self.action {
            ActivityAction::Log => {
                fields.at = fields.at.or(Some(cx.now));
                activities::log(cx.db, fields, cx.now, &cx.request_id)
            }
            ActivityAction::Update => activities::update(cx.db, id()?, fields, cx.now, &cx.request_id),
            ActivityAction::Remove => activities::remove(cx.db, id()?, cx.now, &cx.request_id),
        }
    }
}
