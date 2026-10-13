//! Planning tools: the week, sessions, profile, availability, crowd and events. Owner: lead.
use std::collections::BTreeSet;

use jiff::civil::Date;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Ctx, Tool, ToolDef, def};
use crate::db::models::{BlockKind, PlannedSession};
use crate::domain::calendar::{get_week, week_of};
use crate::domain::profile::{get_profile, update_profile};
use crate::domain::{availability, calendar, calendar_writes, crowd, events, personal_calendar, replanning, scheduling};
use crate::providers::google::google_provider;
use crate::providers::http::Net;
use crate::error::Result;
use crate::time::Ts;

pub fn tools() -> Vec<ToolDef> {
    vec![
        def::<GetWeek>(),
        def::<MoveSession>(),
        def::<AddSession>(),
        def::<SetSessionWorkout>(),
        def::<SkipSession>(),
        def::<PlanWeek>(),
        def::<ReplanWeek>(),
        def::<ProfileGet>(),
        def::<ProfileUpdate>(),
        def::<AvailabilityAdd>(),
        def::<AvailabilityList>(),
        def::<AvailabilityRemove>(),
        def::<CalendarSync>(),
        def::<PersonalCalendarConnect>(),
        def::<PersonalCalendarStatus>(),
        def::<PersonalCalendarDisconnect>(),
        def::<CrowdFeedback>(),
        def::<CrowdStatus>(),
        def::<CrowdForecast>(),
        def::<EventsPending>(),
        def::<EventAck>(),
        def::<CalendarPendingWrites>(),
    ]
}

fn local(cx: &Ctx, text: &str) -> Result<Ts> {
    Ts::parse(text, Some(&get_profile(cx.db)?.tz()))
}

fn local_opt(cx: &Ctx, text: &Option<String>) -> Result<Option<Ts>> {
    text.as_deref().map(|t| local(cx, t)).transpose()
}

fn date(text: &str) -> Result<Date> {
    text.trim().parse().map_err(|_| crate::error::Error::domain("INVALID_DATE", format!("Not a date (YYYY-MM-DD): {text}")))
}

fn date_opt(text: &Option<String>) -> Result<Option<Date>> {
    text.as_deref().map(date).transpose()
}

/// The Monday of the week containing `day` (default: today, owner-local).
fn monday_of(cx: &Ctx, day: &Option<String>) -> Result<Date> {
    let tz = get_profile(cx.db)?.tz();
    Ok(match day {
        Some(d) => week_of(Ts(date(d)?.to_zoned(tz.clone())?.timestamp()), &tz),
        None => week_of(cx.now, &tz),
    })
}

/// Full session id from an id or a unique prefix (≥ 6 chars), so short ids from the state work.
fn session_id(cx: &Ctx, id: &str) -> Result<String> {
    let id = id.trim();
    if PlannedSession::get(cx.db, id)?.is_some() || id.len() < 6 {
        return Ok(id.to_string());
    }
    let matches = PlannedSession::select(cx.db, "WHERE id LIKE ?1 || '%' LIMIT 2", [id])?;
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => crate::bail!("SESSION_NOT_FOUND", "No session with id {id}"),
        _ => crate::bail!("SESSION_AMBIGUOUS", "Several sessions start with {id}; use the full id"),
    }
}

/// `to`, or `leave_at` + commute: the owner says when they can leave, the session starts on arrival.
fn start_time(cx: &Ctx, to: &Option<String>, leave_at: &Option<String>) -> Result<Option<Ts>> {
    if let Some(to) = local_opt(cx, to)? {
        return Ok(Some(to));
    }
    let commute = get_profile(cx.db)?.commute_to_gym_minutes;
    Ok(local_opt(cx, leave_at)?.map(|t| t.mins(commute)))
}

#[derive(Deserialize, JsonSchema)]
pub struct GetWeek {
    /// Any date in the week (YYYY-MM-DD). Default: this week.
    #[serde(default)]
    pub day: Option<String>,
}

impl Tool for GetWeek {
    const NAME: &'static str = "get_week";
    const DESCRIPTION: &'static str = "All planned sessions of a week (any status) with times, workout and crowd forecast.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        get_week(cx.db, monday_of(cx, &self.day)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Placement {
    /// Exact start, owner-local unless it has an offset (e.g. "2026-10-14T11:45"). Kept even if it breaks
    /// planner rules; `warnings` then names them.
    #[serde(default)]
    pub to: Option<String>,
    /// When the owner can leave home; the session starts after the commute. Instead of `to`.
    #[serde(default)]
    pub leave_at: Option<String>,
    /// Without `to`: pick the best slot on this date (YYYY-MM-DD).
    #[serde(default)]
    pub day: Option<String>,
    /// Without `to`: only starts at/after this time (implies its day).
    #[serde(default)]
    pub after: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveSession {
    pub session_id: String,
    #[serde(flatten)]
    pub placement: Placement,
}

impl Tool for MoveSession {
    const NAME: &'static str = "move_session";
    const DESCRIPTION: &'static str = "Move one upcoming session. Without `to`, picks the best slot (quiet first, then habits) on `day`/after `after`, \
        else in its week. The session is locked: other sessions are never moved because of it. Report `warnings` and any `side_effects`.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let id = session_id(cx, &self.session_id)?;
        let p = &self.placement;
        replanning::move_session(cx.db, &id, cx.now, &cx.request_id, start_time(cx, &p.to, &p.leave_at)?, date_opt(&p.day)?, local_opt(cx, &p.after)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddSession {
    /// Workout template id. Default: next in the rotation.
    #[serde(default)]
    pub template_id: Option<String>,
    #[serde(flatten)]
    pub placement: Placement,
}

impl Tool for AddSession {
    const NAME: &'static str = "add_session";
    const DESCRIPTION: &'static str = "Add an extra session on top of the plan (default: best slot this week). It is locked, so it's never dropped for the weekly maximum.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let p = &self.placement;
        replanning::add_session(cx.db, cx.now, &cx.request_id, self.template_id.as_deref(), start_time(cx, &p.to, &p.leave_at)?, date_opt(&p.day)?, local_opt(cx, &p.after)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SetSessionWorkout {
    pub session_id: String,
    pub template_id: String,
}

impl Tool for SetSessionWorkout {
    const NAME: &'static str = "set_session_workout";
    const DESCRIPTION: &'static str = "Change which workout an upcoming session does (\"legs today instead of pull\"). Later sessions rotate on from it.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let id = session_id(cx, &self.session_id)?;
        replanning::set_workout(cx.db, &id, &self.template_id, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SkipSession {
    pub session_id: String,
}

impl Tool for SkipSession {
    const NAME: &'static str = "skip_session";
    const DESCRIPTION: &'static str = "Skip a session that hasn't ended. It counts toward the week, so no make-up is added (moving is the make-up path).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let id = session_id(cx, &self.session_id)?;
        replanning::skip_session(cx.db, &id, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct PlanWeek {
    /// Any date in the week (YYYY-MM-DD).
    pub day: String,
    pub template_id: String,
}

impl Tool for PlanWeek {
    const NAME: &'static str = "plan_week";
    const DESCRIPTION: &'static str = "Fill a week with sessions around the existing ones (normally automatic on Sundays and in the rolling plan).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let week = monday_of(cx, &Some(self.day))?;
        let planned = scheduling::schedule_week(cx.db, week, cx.now, Some(&cx.request_id), Some(&self.template_id), &[])?;
        replanning::commit_upcoming(cx.db, cx.now)?;
        for data in planned["sessions"].as_array().into_iter().flatten() {
            if let Some(session) = PlannedSession::get(cx.db, data["id"].as_str().unwrap_or_default())? {
                calendar_writes::queue_session_write(cx.db, &session, cx.now, false)?;
            }
        }
        Ok(planned)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ReplanWeek {
    /// Any date in the week (YYYY-MM-DD). Default: this week.
    #[serde(default)]
    pub day: Option<String>,
}

impl Tool for ReplanWeek {
    const NAME: &'static str = "replan_week";
    const DESCRIPTION: &'static str = "Repair a week: unlocked future sessions that broke a rule (calendar clash, rest day) move or drop. Locked ones stay.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let week = monday_of(cx, &self.day)?;
        let repaired = replanning::replan_weeks(cx.db, &BTreeSet::from([week]), cx.now, false)?;
        let mut data = get_week(cx.db, week)?;
        data.as_object_mut().expect("object").extend(repaired.as_object().cloned().unwrap_or_default());
        Ok(data)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ProfileGet {}

impl Tool for ProfileGet {
    const NAME: &'static str = "profile_get";
    const DESCRIPTION: &'static str = "The owner's training constraints: timezone, sessions per week, allowed weekdays, workout window, durations, commute, rest.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        Ok(serde_json::to_value(get_profile(cx.db)?)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ProfileUpdate {
    /// Profile fields to change, e.g. {"weekly_target_sessions": 4, "latest_workout_finish": "21:00"}.
    pub changes: Map<String, Value>,
}

impl Tool for ProfileUpdate {
    const NAME: &'static str = "profile_update";
    const DESCRIPTION: &'static str = "Change profile fields (validated as a whole). Future planning uses them; call replan_week if this week should follow now.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        Ok(serde_json::to_value(update_profile(cx.db, &self.changes)?)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AvailabilityAdd {
    pub kind: BlockKind,
    /// Start time (owner-local unless offset). Default: now. Or use `from_date`.
    #[serde(default)]
    pub start: Option<String>,
    /// First whole day (YYYY-MM-DD) instead of `start`.
    #[serde(default)]
    pub from_date: Option<String>,
    /// End time (exclusive). Or use `through`.
    #[serde(default)]
    pub end: Option<String>,
    /// Last whole day (YYYY-MM-DD), inclusive, instead of `end`.
    #[serde(default)]
    pub through: Option<String>,
    /// Also cancel sessions the owner placed themselves in that time.
    #[serde(default)]
    pub cancel_locked: bool,
}

impl Tool for AvailabilityAdd {
    const NAME: &'static str = "availability_add";
    const DESCRIPTION: &'static str = "The owner is away, sick or busy for a while: no sessions or reminders then; affected sessions are replanned.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        if self.start.is_some() && self.from_date.is_some() || self.end.is_some() && self.through.is_some() {
            crate::bail!("INVALID_ARGUMENTS", "Use either start/from_date and either end/through");
        }
        let tz = get_profile(cx.db)?.tz();
        let midnight = |d: Date| -> Result<Ts> { Ok(Ts(d.to_zoned(tz.clone())?.timestamp())) };
        let start = match (&self.start, &self.from_date) {
            (Some(s), _) => local(cx, s)?,
            (None, Some(d)) => midnight(date(d)?)?,
            (None, None) => cx.now,
        };
        let end = match (&self.end, &self.through) {
            (Some(e), _) => local(cx, e)?,
            (None, Some(d)) => midnight(date(d)?.tomorrow()?)?,
            (None, None) => crate::bail!("INVALID_ARGUMENTS", "Missing end or inclusive through"),
        };
        availability::add(cx.db, start, end, self.kind, cx.now, &cx.request_id, self.cancel_locked)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AvailabilityList {}

impl Tool for AvailabilityList {
    const NAME: &'static str = "availability_list";
    const DESCRIPTION: &'static str = "Away/sick/busy blocks, with whether each is in effect now.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        Ok(json!({"blocks": availability::list_blocks(cx.db, cx.now)?}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AvailabilityRemove {
    pub block_id: String,
}

impl Tool for AvailabilityRemove {
    const NAME: &'static str = "availability_remove";
    const DESCRIPTION: &'static str = "Remove an away/sick/busy block; planning around it resumes.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        availability::retract(cx.db, &self.block_id, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CrowdFeedback {
    pub workout_id: String,
    /// EMPTY, FINE ("a few"), BUSY or PACKED.
    pub rating: String,
}

impl Tool for CrowdFeedback {
    const NAME: &'static str = "crowd_feedback";
    const DESCRIPTION: &'static str = "Record how busy the gym felt during a workout. Future crowd estimates learn from it.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        crowd::record_feedback(cx.db, &self.workout_id, &self.rating, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CrowdStatus {}

impl Tool for CrowdStatus {
    const NAME: &'static str = "crowd_status";
    const DESCRIPTION: &'static str = "Whether gym check-in data is flowing, and source health.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        Ok(json!({"polling": crowd::polling_health(cx.db, cx.now)?, "sources": crowd::source_health(cx.db, cx.now)?["sources"]}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CrowdForecast {
    /// When (owner-local unless offset). Default: now.
    #[serde(default)]
    pub at: Option<String>,
}

impl Tool for CrowdForecast {
    const NAME: &'static str = "crowd_forecast";
    const DESCRIPTION: &'static str = "Expected crowd at a time: typical check-ins and the owner's felt crowd (Empty/A few/Busy/Packed) with confidence.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let at = local_opt(cx, &self.at)?.unwrap_or(cx.now);
        let model = crowd::CrowdModel::new(cx.db, cx.now)?;
        Ok(json!({"forecast": model.forecast(at), "prediction": model.predict(at).json}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct EventsPending {
    /// Include audit events the code already handled.
    #[serde(default)]
    pub all: bool,
}

impl Tool for EventsPending {
    const NAME: &'static str = "events_pending";
    const DESCRIPTION: &'static str = "Unhandled events that need attention (unknown deliveries, blocked calendar writes, crowd source down).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        Ok(json!({"events": events::pending(cx.db, self.all)?}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct EventAck {
    pub event_id: String,
}

impl Tool for EventAck {
    const NAME: &'static str = "event_ack";
    const DESCRIPTION: &'static str = "Mark an event handled once the owner knows about it.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        events::ack(cx.db, &self.event_id, cx.now)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CalendarPendingWrites {}

impl Tool for CalendarPendingWrites {
    const NAME: &'static str = "calendar_pending_writes";
    const DESCRIPTION: &'static str = "Calendar changes queued for Google Calendar but not published yet.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        Ok(json!({"writes": calendar_writes::pending_writes(cx.db)?}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CalendarSync {}

impl Tool for CalendarSync {
    const NAME: &'static str = "calendar_sync";
    const DESCRIPTION: &'static str = "Pull the owner's Google Calendar edits now (moves, resizes, deletes, new blockers) and replan around them. Runs every minute anyway.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let net = Net::new()?;
        let mut provider = google_provider(cx.db, None, &net)?;
        calendar::sync_calendar(cx.db, &mut provider, cx.now)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct PersonalCalendarConnect {
    /// The owner's published calendar link (webcal:// or https://, e.g. iCloud "Public Calendar").
    pub url: String,
}

impl Tool for PersonalCalendarConnect {
    const NAME: &'static str = "personal_calendar_connect";
    const DESCRIPTION: &'static str = "Use the owner's personal calendar feed as read-only busy time. Fetches it now and replans affected weeks. Never repeat the URL back.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        personal_calendar::connect(cx.db, &self.url, cx.now, &Net::new()?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct PersonalCalendarStatus {}

impl Tool for PersonalCalendarStatus {
    const NAME: &'static str = "personal_calendar_status";
    const DESCRIPTION: &'static str = "Whether a personal calendar feed is connected, its host, last fetch and blocker count.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        personal_calendar::status(cx.db)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct PersonalCalendarDisconnect {}

impl Tool for PersonalCalendarDisconnect {
    const NAME: &'static str = "personal_calendar_disconnect";
    const DESCRIPTION: &'static str = "Stop using the personal calendar feed; its blockers are dropped and affected weeks replanned.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        personal_calendar::disconnect(cx.db, cx.now)
    }
}
