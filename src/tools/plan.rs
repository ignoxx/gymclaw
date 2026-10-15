//! Planning tools: the week, sessions, profile, availability, crowd and events. Owner: lead.
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
        def::<SessionCreate>(),
        def::<SessionUpdate>(),
        def::<PlanWeek>(),
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
#[serde(deny_unknown_fields)]
pub struct SessionFields {
    /// Exact start, owner-local unless an offset is supplied. Required on create.
    pub start: Option<String>,
    /// Exact end. Default: preserve duration (create: profile duration).
    pub end: Option<String>,
    /// A real workout template. Required on create.
    pub template_id: Option<String>,
    /// Any scheduling status; this does not create workout/set logs. CANCELLED deletes the owned
    /// calendar event; SKIPPED/MISSED keep it as history. Default: preserve (create: TENTATIVE).
    pub status: Option<crate::db::models::SessionStatus>,
}

impl SessionFields {
    fn patch(self, cx: &Ctx) -> Result<replanning::SessionPatch> {
        Ok(replanning::SessionPatch { start: local_opt(cx, &self.start)?, end: local_opt(cx, &self.end)?, template_id: self.template_id, status: self.status })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionCreate {
    pub fields: SessionFields,
}

impl Tool for SessionCreate {
    const NAME: &'static str = "session_create";
    const DESCRIPTION: &'static str = "Create an upcoming session at exact times with a real template. Rules are defaults: violations return warnings, never silently change your choice. Locks the session. Calendar writes, allocation, reminders and retries are handled together.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        replanning::edit_session(cx.db, None, self.fields.patch(cx)?, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdate {
    pub session_id: String,
    /// Change any combination in one call. Omitted fields stay as saved.
    pub fields: SessionFields,
}

impl Tool for SessionUpdate {
    const NAME: &'static str = "session_update";
    const DESCRIPTION: &'static str = "Edit any upcoming session's exact time, duration, template or status; cancel with status CANCELLED. Owner/agent edits lock it and affect no other sessions. Report warnings. Use sql_query/get_week to choose a session, time or template first.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let id = session_id(cx, &self.session_id)?;
        replanning::edit_session(cx.db, Some(&id), self.fields.patch(cx)?, cx.now, &cx.request_id)
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
        let before: std::collections::BTreeSet<_> = PlannedSession::select(cx.db, "", [])?.into_iter().map(|s| s.id).collect();
        let planned = scheduling::schedule_week(cx.db, week, cx.now, Some(&cx.request_id), Some(&self.template_id), &[])?;
        replanning::commit_upcoming(cx.db, cx.now)?;
        for data in planned["sessions"].as_array().into_iter().flatten() {
            if let Some(mut session) = PlannedSession::get(cx.db, data["id"].as_str().unwrap_or_default())? {
                if !before.contains(&session.id) {
                    session.user_locked = true;
                    session.save(cx.db)?;
                }
                calendar_writes::queue_session_write(cx.db, &session, cx.now, false)?;
            }
        }
        Ok(planned)
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
    const DESCRIPTION: &'static str = "Change profile fields (validated as a whole). Existing sessions stay saved; `replanning.warnings` lists conflicts for you to resolve with session_update.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let mut data = serde_json::to_value(update_profile(cx.db, &self.changes)?)?;
        let weeks = availability::affected_weeks(cx.db, cx.now)?;
        data["replanning"] = replanning::inspect_weeks(cx.db, &weeks, cx.now)?;
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
    const DESCRIPTION: &'static str = "The owner is away, sick or busy for a while: no sessions or reminders then; conflicts are reported for your decision.";

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
    const DESCRIPTION: &'static str = "Pull the owner's Google Calendar edits now (moves, resizes, deletes, new blockers) and report conflicts for your decision. Runs every minute anyway.";

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
    const DESCRIPTION: &'static str = "Use the owner's personal calendar feed as read-only busy time. Fetches it now and reports affected sessions. Never repeat the URL back.";

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
    const DESCRIPTION: &'static str = "Stop using the personal calendar feed; its blockers are dropped; existing sessions stay saved.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        personal_calendar::disconnect(cx.db, cx.now)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::Connection;
    use crate::db::models::{AgentEvent, AvailabilityBlock};
    use crate::domain::templates::{Template, import_template};
    use crate::domain::weekly::weekly_plan;
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
    fn raw_session_edits_keep_intent_and_reconcile_mechanics() {
        let db = db();
        let before = week_start_ids(&db);
        let args = json!({"fields": {"start": "2026-10-13T23:07", "end": "2026-10-13T23:17", "template_id": "short"}});
        let created = call(&db, "session_create", args.clone(), sunday(), "raw-create").unwrap();
        assert_eq!(call(&db, "session_create", args, sunday(), "raw-create").unwrap(), created);
        let id = created["data"]["session"]["id"].as_str().unwrap();
        assert!(created["data"]["warnings"].as_array().unwrap().len() >= 3);
        assert_eq!(created["data"]["session"]["start"], "2026-10-13T21:07:00+00:00");
        assert_eq!(created["data"]["session"]["user_locked"], true);
        assert!(!writes_start(&db, id).is_empty());
        let changed = call(&db, "session_update", json!({"session_id": id, "fields": {"start": "2026-10-14T12:00", "end": "2026-10-14T13:10", "status": "COMMITTED"}}), sunday(), "raw-update").unwrap();
        assert_eq!(changed["data"]["side_effects"], json!([]));
        let jobs: i64 = db.query_row("SELECT count(*) FROM notification_job WHERE planned_session_id=?1 AND status='PENDING'", [id], |r| r.get(0)).unwrap();
        assert!(jobs > 0);
        assert!(call(&db, "session_update", json!({"session_id": id, "fields": {"end": "2026-10-14T11:00"}}), sunday(), "bad-time").is_err());
        let cancelled = call(&db, "session_update", json!({"session_id": id, "fields": {"status": "CANCELLED"}}), sunday(), "raw-cancel").unwrap();
        assert_eq!(cancelled["data"]["session"]["status"], "CANCELLED");
        assert!(PlannedSession::get(&db, id).unwrap().unwrap().deleted_by_user());
        let jobs: i64 = db.query_row("SELECT count(*) FROM notification_job WHERE planned_session_id=?1 AND status='PENDING'", [id], |r| r.get(0)).unwrap();
        assert_eq!(jobs, 0);
        for row in before {
            let after = PlannedSession::get(&db, row["id"].as_str().unwrap()).unwrap().unwrap();
            assert_eq!(after.planned_start_at.iso(), row["start"]);
        }
    }

    #[test]
    fn profile_window_change_reports_conflicts_and_keeps_sessions() {
        let db = db();
        let before = week_start_ids(&db);
        let updated = call(&db, "profile_update", json!({"changes": {"earliest_workout_start": "11:00"}}), sunday(), "p").unwrap();
        assert_eq!(updated["replanning"]["changed"], json!([]));
        assert!(!updated["replanning"]["warnings"].as_array().unwrap().is_empty());
        for row in before {
            assert_eq!(PlannedSession::get(&db, row["id"].as_str().unwrap()).unwrap().unwrap().planned_start_at.iso(), row["start"]);
        }
        let events = events::wake_pending(&db).unwrap();
        for event in &events { events::ack(&db, event["id"].as_str().unwrap(), sunday()).unwrap(); }
        call(&db, "profile_update", json!({"changes": {"earliest_workout_start": "11:00"}}), sunday(), "p2").unwrap();
        assert!(events::wake_pending(&db).unwrap().is_empty());
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
    fn planner_events_are_actionable_until_acknowledged() {
        let db = db();
        crate::domain::core::emit(&db, "planning.replan_required", sunday(), json!({})).unwrap();
        assert_eq!(call(&db, "events_pending", json!({}), sunday(), "e").unwrap()["events"].as_array().unwrap().len(), 1);
        let inbox = call(&db, "events_pending", json!({"all": true}), sunday(), "e").unwrap()["events"].as_array().unwrap().clone();
        let actionable = inbox.iter().find(|e| e["type"] == "planning.replan_required").unwrap();
        call(&db, "event_ack", json!({"event_id": actionable["id"]}), sunday(), "ack").unwrap();
        let after = call(&db, "events_pending", json!({"all": true}), sunday(), "e").unwrap()["events"].as_array().unwrap().clone();
        assert!(after.iter().all(|e| e["id"] != actionable["id"]));
        assert!(AgentEvent::get(&db, actionable["id"].as_str().unwrap()).unwrap().unwrap().handled_at.is_some());
    }
}
