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
    const DESCRIPTION: &'static str = "Change which workout an upcoming session does (\"legs today instead of pull\"). Later sessions get whichever split workout was trained longest ago, so a swapped-out one comes next.";

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
    const DESCRIPTION: &'static str = "Change profile fields (validated as a whole). Upcoming sessions that no longer fit are replanned right away; `replanning` says what moved.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let mut data = serde_json::to_value(update_profile(cx.db, &self.changes)?)?;
        let weeks = availability::affected_weeks(cx.db, cx.now)?;
        data["replanning"] = replanning::replan_weeks(cx.db, &weeks, cx.now, false)?;
        Ok(data)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::Connection;
    use crate::db::models::{AgentEvent, AvailabilityBlock, SessionStatus};
    use crate::domain::habits::{affinity, evidence};
    use crate::domain::replanning::replan_weeks;
    use crate::domain::templates::{Template, import_template};
    use crate::domain::weekly::{assign_rotation, weekly_plan};
    use crate::error::Error;

    fn ts(text: &str) -> Ts {
        Ts::parse(text, None).unwrap()
    }

    /// Sunday 2026-10-11 19:00 Berlin.
    fn sunday() -> Ts {
        ts("2026-10-11T17:00:00+00:00")
    }

    fn template(db: &Connection, value: Value) {
        import_template(db, &serde_json::from_value::<Template>(value).unwrap()).unwrap();
    }

    fn db() -> Connection {
        let db = crate::domain::workout::tests::db();
        template(&db, json!({"id": "short", "name": "Short", "exercises": [{"id": "bench", "name": "Bench", "role": "press", "primary": true, "working_sets": 2, "target_weight": 80, "warmup_weight": 50}]}));
        db
    }

    /// One tool call, like `gymclaw call <tool> '<json>' --now <now> --request-id <id>` minus the transaction.
    fn call(db: &Connection, tool: &str, args: Value, now: Ts, request_id: &str) -> Result<Value> {
        let dir = std::env::temp_dir();
        let mut cx = Ctx { db, now, request_id: request_id.into(), data_dir: &dir, effects: vec![] };
        crate::tools::call(&mut cx, tool, args)
    }

    fn week_start_ids(db: &Connection) -> Vec<Value> {
        weekly_plan(db, "short", sunday()).unwrap()["plan"]["sessions"].as_array().unwrap().clone()
    }

    fn writes_start(db: &Connection, session_id: &str) -> Vec<String> {
        let writes = call(db, "calendar_pending_writes", json!({}), sunday(), "w").unwrap()["writes"].as_array().unwrap().clone();
        writes.iter().filter(|w| w["session_id"] == session_id).map(|w| w["body"]["start"]["dateTime"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn move_session_picks_valid_slot_or_explains() {
        let db = db();
        let sessions = week_start_ids(&db);
        let friday = sessions.iter().find(|s| s["start"].as_str().unwrap().starts_with("2026-10-16")).unwrap().clone();
        let id = friday["id"].as_str().unwrap();
        let moved = call(&db, "move_session", json!({"session_id": id, "to": "2026-10-16T12:00:00+02:00"}), sunday(), "tg-1-move").unwrap();
        assert_eq!(moved["data"]["to"], "2026-10-16T10:00:00+00:00");
        assert_eq!(call(&db, "move_session", json!({"session_id": id, "to": "2026-10-16T12:00:00+02:00"}), sunday(), "tg-1-move").unwrap(), moved);
        assert!(writes_start(&db, id).contains(&"2026-10-16T12:00:00+02:00".to_string()));
        // "Later today": best slot at/after the bound, plus spread-out alternatives for buttons.
        let later = call(&db, "move_session", json!({"session_id": id, "after": "2026-10-16T14:00:00+02:00"}), sunday(), "tg-4-move").unwrap();
        let alternatives = later["data"]["alternatives"].as_array().unwrap();
        assert!(!alternatives.is_empty());
        let starts: Vec<Ts> = std::iter::once(&later["data"]["to"]).chain(alternatives.iter().map(|a| &a["start"])).map(|t| ts(t.as_str().unwrap())).collect();
        assert!(starts.iter().all(|t| *t >= ts("2026-10-16T12:00:00+00:00") && t.iso().starts_with("2026-10-16")));
        for (i, x) in starts.iter().enumerate() {
            assert!(starts[i + 1..].iter().all(|y| (*x - *y).abs().as_mins() >= 60), "{starts:?}");
        }
        // The owner's explicit pick (12:00) is liked; the slot it was first planned at is not.
        let tz = get_profile(&db).unwrap().tz();
        let points = evidence(&db, sunday(), &tz).unwrap();
        let first = ts(friday["start"].as_str().unwrap()).local(&tz);
        assert!(affinity(&points, 12.0 * 60.0) > 0.5 && 0.5 > affinity(&points, f64::from(first.hour()) * 60.0 + f64::from(first.minute())));
        // A time the owner names is kept even off-grid and outside the window: locked against
        // replanning, still published, and the broken rule named.
        let kept = call(&db, "move_session", json!({"session_id": id, "to": "2026-10-16T21:07:00+02:00"}), sunday(), "tg-5-move").unwrap();
        assert_eq!(kept["data"]["to"], "2026-10-16T19:07:00+00:00");
        assert_eq!(kept["data"]["warnings"], json!(["outside your workout window (07:00–22:00)"]));
        assert!(writes_start(&db, id).contains(&"2026-10-16T21:07:00+02:00".to_string()));
        replan_weeks(&db, &BTreeSet::from([jiff::civil::date(2026, 10, 12)]), sunday(), false).unwrap();
        let row = PlannedSession::get(&db, id).unwrap().unwrap();
        assert!(row.user_locked && row.planned_start_at == ts("2026-10-16T19:07:00+00:00"));
        // "I can leave at 13:00": start is leave + commute.
        let left = call(&db, "move_session", json!({"session_id": id, "leave_at": "2026-10-16T13:00:00+02:00"}), sunday(), "tg-6-move").unwrap();
        let commute = get_profile(&db).unwrap().commute_to_gym_minutes;
        assert_eq!(ts(left["data"]["to"].as_str().unwrap()), ts("2026-10-16T13:00:00+02:00").mins(commute));
        // A day the owner asks for wins over the rest-day rule (Thursday follows Wednesday); the warning says so.
        let thursday = call(&db, "move_session", json!({"session_id": id, "day": "2026-10-15"}), sunday(), "tg-7-move").unwrap();
        assert!(thursday["data"]["to"].as_str().unwrap().starts_with("2026-10-15"));
        assert_eq!(thursday["data"]["warnings"], json!(["skips a rest day"]));
    }

    #[test]
    fn owner_adds_sessions_and_picks_workouts() {
        let db = db();
        let sessions = week_start_ids(&db);
        template(&db, json!({"id": "legs", "name": "Legs", "exercises": [{"id": "squat", "name": "Squat", "role": "squat", "target_weight": 100}]}));
        // Naive times are the owner's local time. The extra session is kept beyond the weekly maximum.
        let added = call(&db, "add_session", json!({"to": "2026-10-17T10:00", "template_id": "short"}), sunday(), "tg-1-add").unwrap();
        assert_eq!(added["data"]["to"], "2026-10-17T08:00:00+00:00");
        assert_eq!(added["data"]["warnings"], json!(["outside your workout window (07:00–22:00)", "skips a rest day"]));
        replan_weeks(&db, &BTreeSet::from([jiff::civil::date(2026, 10, 12)]), sunday(), false).unwrap();
        let status = PlannedSession::get(&db, added["data"]["session_id"].as_str().unwrap()).unwrap().unwrap().status;
        assert!(matches!(status, SessionStatus::Tentative | SessionStatus::Committed), "{status:?}");
        // "Legs today instead": the session gets that workout and keeps it.
        let id = sessions[0]["id"].as_str().unwrap();
        let legs = call(&db, "set_session_workout", json!({"session_id": id, "template_id": "legs"}), sunday(), "tg-2-set").unwrap();
        assert_eq!(legs["data"]["to"], "legs");
        assign_rotation(&db, sunday()).unwrap();
        let row = PlannedSession::get(&db, id).unwrap().unwrap();
        assert!(row.user_locked && row.workout_template_id.as_deref() == Some("legs"));
        assert_eq!(row.workout_plan_json["template"]["id"], "legs");
    }

    /// Python's `profile update` replanned on its own; the Rust tool contract leaves that to `replan_week`.
    #[test]
    fn profile_window_change_moves_sessions_that_no_longer_fit() {
        let db = db();
        week_start_ids(&db);
        let updated = call(&db, "profile_update", json!({"changes": {"earliest_workout_start": "11:00"}}), sunday(), "p").unwrap();
        let changed = updated["replanning"]["changed"].as_array().unwrap().clone();
        assert!(!changed.is_empty() && changed.iter().all(|c| c["action"] == "moved"), "{changed:?}");
        let tz = get_profile(&db).unwrap().tz();
        assert!(changed.iter().all(|c| ts(c["to"].as_str().unwrap()).local(&tz).hour() >= 11));
    }

    #[test]
    fn inclusive_local_date_handles_dst_and_retry() {
        let db = db();
        let args = json!({"kind": "TRAVEL", "from_date": "2026-10-25", "through": "2026-10-25"});
        let first = call(&db, "availability_add", args.clone(), sunday(), "dst-trip").unwrap()["data"].clone();
        assert_eq!((ts(first["end"].as_str().unwrap()) - ts(first["start"].as_str().unwrap())).as_hours(), 25);
        assert_eq!(call(&db, "availability_add", args, sunday(), "dst-trip").unwrap()["data"], first);
        assert_eq!(AvailabilityBlock::select(&db, "", []).unwrap().len(), 1);
        let conflict = json!({"kind": "TRAVEL", "from_date": "2026-10-25", "through": "2026-10-25", "cancel_locked": true});
        assert_eq!(call(&db, "availability_add", conflict, sunday(), "dst-trip").unwrap_err().code(), "IDEMPOTENCY_CONFLICT");
    }

    #[test]
    fn plan_week_restart_and_idempotency() {
        let db = db();
        assert_eq!(call(&db, "profile_update", json!({"changes": {"prep_minutes": 25}}), sunday(), "p").unwrap()["prep_minutes"], 25);
        assert_eq!(call(&db, "profile_get", json!({}), sunday(), "g").unwrap()["prep_minutes"], 25);
        let now = ts("2026-10-11T12:00:00+02:00");
        let args = json!({"day": "2026-10-12", "template_id": "short"});
        let first = call(&db, "plan_week", args.clone(), now, "week-1").unwrap();
        assert_eq!(first["sessions"].as_array().unwrap().len(), 3);
        assert_eq!(call(&db, "plan_week", args.clone(), now, "week-1").unwrap(), first);
        let again = call(&db, "plan_week", args, now, "week-2").unwrap();
        assert_eq!((again["created_count"].as_i64(), again["sessions"].as_array().unwrap().len()), (Some(0), 3));
        assert_eq!(PlannedSession::select(&db, "", []).unwrap().len(), 3);
        let other_week = call(&db, "plan_week", json!({"day": "2026-10-19", "template_id": "short"}), now, "week-1").unwrap_err();
        assert!(other_week.to_string().contains("different planning input"), "{other_week}");
    }

    #[test]
    fn errors_roll_back_and_naive_times_are_local() {
        let db = db();
        for (tool, args) in [("profile_update", json!({"changes": {"prep_minutes": -1}})), ("profile_update", json!({"changes": []})), ("unknown", json!({}))] {
            let tx = db.unchecked_transaction().unwrap();
            let error: Error = call(&tx, tool, args, sunday(), "bad").unwrap_err();
            assert!(["INVALID_PROFILE", "INVALID_ARGUMENTS", "UNKNOWN_TOOL"].contains(&error.code()), "{}", error.code());
            drop(tx);
        }
        assert_eq!(get_profile(&db).unwrap().prep_minutes, 15);
        // A naive time is the owner's local time (Europe/Berlin).
        let blocked = call(&db, "availability_add", json!({"kind": "UNAVAILABLE", "start": "2026-10-13T12:00", "end": "2026-10-13T13:00"}), sunday(), "naive").unwrap();
        assert_eq!(blocked["data"]["start"], "2026-10-13T10:00:00+00:00");
    }

    #[test]
    fn planner_events_stay_out_of_the_inbox_until_asked() {
        let db = db();
        crate::domain::core::emit(&db, "planning.replan_required", sunday(), json!({})).unwrap();
        assert_eq!(call(&db, "events_pending", json!({}), sunday(), "e").unwrap()["events"], json!([]));
        let inbox = call(&db, "events_pending", json!({"all": true}), sunday(), "e").unwrap()["events"].as_array().unwrap().clone();
        let actionable = inbox.iter().find(|e| e["type"] == "planning.replan_required").unwrap();
        call(&db, "event_ack", json!({"event_id": actionable["id"]}), sunday(), "ack").unwrap();
        let after = call(&db, "events_pending", json!({"all": true}), sunday(), "e").unwrap()["events"].as_array().unwrap().clone();
        assert!(after.iter().all(|e| e["id"] != actionable["id"]));
        assert!(AgentEvent::get(&db, actionable["id"].as_str().unwrap()).unwrap().unwrap().handled_at.is_some());
    }
}
