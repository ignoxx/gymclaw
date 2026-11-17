//! Session mechanics and non-mutating plan inspection. Policy decisions belong to the agent.
use std::collections::{BTreeSet, HashMap, HashSet};

use jiff::SignedDuration;
use jiff::civil::Date;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{AgentEvent, PlannedSession, SessionStatus};
use crate::domain::calendar::{allocation, week_of};
use crate::domain::calendar_busy::busy_intervals;
use crate::domain::calendar_writes::{cancel_pending, queue_session_write};
use crate::domain::core::mutate;
use crate::domain::notifications::{cancel_session_jobs, schedule_session_jobs};
use crate::domain::planning::{Candidate, overlaps, plan_week, recovery_ok, require_monday, weekday};
use crate::domain::profile::{Profile, get_profile};
use crate::domain::scheduling::{interval, planner_inputs};
use crate::domain::templates::get_template;
use crate::error::Result;
use crate::time::Ts;

const HOURS_48: SignedDuration = SignedDuration::from_hours(48);

/// The session including prep and travel, by the current profile: (get ready, home again).
pub fn expanded(session: &PlannedSession, profile: &Profile) -> (Ts, Ts) {
    let slot = Candidate::at(session.planned_start_at, session.planned_end_at, profile);
    (slot.prep_start, slot.home_at)
}

/// Put `session` at [start, end): its week and its get-ready and leave-home times. A new start drops the
/// old slot's crowd forecast (the `crowd_*` plan keys); the next tick fills it again. Caller saves.
pub fn place(session: &mut PlannedSession, start: Ts, end: Ts, profile: &Profile) {
    if start != session.planned_start_at
        && let Some(plan) = session.workout_plan_json.0.as_object_mut()
    {
        plan.retain(|key, _| !key.starts_with("crowd_"));
    }
    let slot = Candidate::at(start, end, profile);
    (session.planned_start_at, session.planned_end_at) = (start, end);
    (session.prep_start_at, session.leave_home_at) = (slot.prep_start, slot.leave_home);
    session.week_id = week_of(start, &profile.tz()).to_string();
}

fn active(db: &Connection, except: Option<&str>) -> Result<Vec<PlannedSession>> {
    PlannedSession::select(
        db,
        "WHERE status IN ('TENTATIVE', 'COMMITTED', 'STARTED', 'COMPLETED') AND id IS NOT ?1 ORDER BY rowid",
        [except],
    )
}

/// Sessions within 48 h are committed (calendar shows them confirmed) and get their reminders.
pub fn commit_upcoming(db: &Connection, now: Ts) -> Result<()> {
    for mut session in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') ORDER BY rowid", [])? {
        if session.status == SessionStatus::Tentative
            && now < session.planned_start_at
            && session.planned_start_at <= now + HOURS_48
        {
            session.status = SessionStatus::Committed;
            session.source_revision += 1;
            session.save(db)?;
            queue_session_write(db, &session, now, false)?;
        }
        schedule_session_jobs(db, &session, now)?;
    }
    Ok(())
}

/// Inspect existing weeks without changing sessions → their warnings. Conflicts and infeasibility that
/// just appeared become durable agent events; only the agent decides whether an exception or an edit is
/// appropriate.
pub fn inspect_weeks(db: &Connection, weeks: &BTreeSet<Date>, now: Ts) -> Result<Vec<String>> {
    inspect(db, weeks, now, true)
}

/// `wake: false` records the weeks' warnings as standing without waking the agent: the editor of
/// those weeks already decided about them.
fn inspect(db: &Connection, weeks: &BTreeSet<Date>, now: Ts, wake: bool) -> Result<Vec<String>> {
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let rows = PlannedSession::select(db, "ORDER BY planned_start_at, id", [])?;
    let known = known_warnings(db)?;
    let (mut warnings, mut fresh) = (Vec::new(), Vec::new());
    for &week in weeks {
        require_monday(week)?;
        let sessions: Vec<_> =
            rows.iter().filter(|s| week_of(s.planned_start_at, &tz) == week && s.status.is_active()).collect();
        if !rows.iter().any(|s| week_of(s.planned_start_at, &tz) == week) {
            fresh.extend(new_warnings(db, week, &[], &known, now)?);
            continue;
        }
        let from = warnings.len();
        for session in sessions.iter().filter(|s| s.status.is_open() && s.planned_end_at > now) {
            let slot = Candidate::at(session.planned_start_at, session.planned_end_at, &profile);
            for warning in rule_breaks(db, &session.id, &slot, &profile)? {
                warnings.push(format!("Session {}: {warning}", session.id));
            }
            if session.plan("error").is_some_and(|v| !v.is_null()) {
                warnings.push(format!("Session {}: slot too short for required movements", session.id));
            }
        }
        if sessions.len() as i64 > profile.weekly_max_sessions {
            warnings.push(format!("Week {week}: exceeds weekly maximum"));
        }
        let plan = plan_week(&profile, week, &planner_inputs(db, week, &profile)?, now)?;
        let count = sessions.len() as i64;
        if count < profile.weekly_target_sessions {
            warnings.push(format!("Week {week}: {count}/{} sessions saved; {} additional default slots feasible. Agent decides whether to add or accept shortfall.", profile.weekly_target_sessions, plan.sessions.len()));
        }
        fresh.extend(new_warnings(db, week, &warnings[from..], &known, now)?);
    }
    if wake && !fresh.is_empty() {
        crate::domain::core::emit(db, "planning.attention", now, json!({"warnings": fresh}))?;
    }
    Ok(warnings)
}

/// Every warning currently standing: the newest `planning.warnings` event of each week (written when a
/// week's list changes and handled at once, so it never wakes the agent). Warnings name their session or
/// week, so a session moved to another week with the same problem is still the same warning.
fn known_warnings(db: &Connection) -> Result<HashSet<String>> {
    let mut weeks: HashMap<String, Vec<String>> = HashMap::new();
    for event in AgentEvent::select(db, "WHERE type = 'planning.warnings' ORDER BY created_at, rowid", [])? {
        let week = event.payload_json["week"].as_str().unwrap_or_default().to_string();
        weeks.insert(week, serde_json::from_value(event.payload_json.0["active"].clone())?);
    }
    Ok(weeks.into_values().flatten().collect())
}

/// Records the week's current warnings and returns the ones not already standing (`known`, taken
/// before this inspection). A warning wakes the agent when it appears, stays quiet while it persists
/// (the agent already decided about it) and wakes it again if it goes away and comes back.
fn new_warnings(
    db: &Connection,
    week: Date,
    current: &[String],
    known: &HashSet<String>,
    now: Ts,
) -> Result<Vec<String>> {
    let week = week.to_string();
    let before = AgentEvent::first(
        db,
        "WHERE type = 'planning.warnings' AND payload_json ->> 'week' = ?1 ORDER BY created_at DESC, rowid DESC",
        [&week],
    )?
    .map(|e| serde_json::from_value::<Vec<String>>(e.payload_json.0["active"].clone()))
    .transpose()?
    .unwrap_or_default();
    if before != current {
        let mut event =
            crate::domain::core::emit(db, "planning.warnings", now, json!({"week": week, "active": current}))?;
        event.handled_at = Some(now);
        event.update(db)?;
    }
    Ok(current.iter().filter(|w| !known.contains(*w)).cloned().collect())
}

/// Planner rules an owner-chosen slot breaks, as short reasons for the owner.
fn rule_breaks(db: &Connection, session_id: &str, slot: &Candidate, profile: &Profile) -> Result<Vec<String>> {
    let tz = profile.tz();
    let (start, end) = (slot.start.local(&tz), slot.end.local(&tz));
    let mut reasons = Vec::new();
    if !profile.weekdays_allowed.contains(&weekday(start.date()))
        || start.date() != end.date()
        || start.time() < profile.earliest_workout_start
        || end.time() > profile.latest_workout_finish
    {
        reasons.push(format!(
            "outside your workout window ({}–{})",
            profile.earliest_workout_start.strftime("%H:%M"),
            profile.latest_workout_finish.strftime("%H:%M")
        ));
    }
    let day = Ts(start.date().to_zoned(tz.clone())?.timestamp());
    if busy_intervals(db, day - SignedDuration::from_hours(24), day + SignedDuration::from_hours(48))?
        .iter()
        .any(|b| overlaps(slot.prep_start, slot.home_at, b))
    {
        reasons.push("overlaps a calendar event".into());
    }
    if active(db, Some(session_id))?.iter().any(|other| !recovery_ok(slot.start, slot.end, &interval(other), profile)) {
        reasons.push("skips a rest day".into());
    }
    Ok(reasons)
}

/// Editable session state. Status describes scheduling/history only; it never fabricates workout logs.
#[derive(Clone, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionPatch {
    pub start: Option<Ts>,
    pub end: Option<Ts>,
    pub template_id: Option<String>,
    pub status: Option<SessionStatus>,
}

/// One atomic owner/agent edit. Policy violations warn; malformed times/templates fail. All saved
/// edits lock the session and reconcile allocation, calendar intent and notification jobs together.
/// A missed session, or a skipped one whose slot has begun, keeps that outcome: rescheduling it
/// creates a new session that follows it, so history shows the miss and the make-up.
pub fn edit_session(
    db: &Connection,
    id: Option<&str>,
    patch: SessionPatch,
    now: Ts,
    request_id: &str,
) -> Result<Value> {
    mutate(db, "planning.session_edited", request_id, json!({"session_id": id, "patch": patch}), now, || {
        let profile = get_profile(db)?;
        let mut session = match id {
            Some(id) => PlannedSession::get(db, id)?
                .ok_or_else(|| crate::error::Error::domain("SESSION_NOT_FOUND", "Unknown session"))?,
            None => PlannedSession::new(String::new(), None, true),
        };
        // Scheduling status alone is editable; actual training history belongs to workout tools.
        let trained = crate::db::models::WorkoutSession::first(
            db,
            "WHERE planned_session_id = ?1 AND started_at IS NOT NULL",
            [&session.id],
        )?
        .is_some();
        // A missed or skipped slot can still be rescheduled (owner is running late, or makes it up later).
        let outcome = session.status == SessionStatus::Missed
            || (session.status == SessionStatus::Skipped && session.planned_start_at <= now);
        if id.is_some() && (trained || (session.planned_end_at <= now && !outcome)) {
            crate::bail!(
                "SESSION_NOT_EDITABLE",
                "Use workout tools for a running or historical workout; session edits are for upcoming slots"
            );
        }
        let from = id.map(|_| session.planned_start_at);
        let revived = id.is_some() && outcome && patch.status.is_none_or(SessionStatus::is_open);
        if revived {
            // Same workout, times and plan as a starting point; its own identity, calendar event and reminders.
            let original = session.id.clone();
            session = PlannedSession {
                id: crate::db::new_id(),
                calendar_event_id: None,
                source_revision: 0,
                created_at: now,
                follows_session_id: Some(original),
                ..session
            };
        }
        let new = id.is_none() || revived;
        let start = patch
            .start
            .or(id.map(|_| session.planned_start_at))
            .ok_or_else(|| crate::error::Error::domain("INVALID_ARGUMENTS", "Creating a session requires start"))?;
        let duration = if id.is_some() {
            (session.planned_end_at - session.planned_start_at).as_mins()
        } else {
            profile.preferred_workout_minutes
        };
        let end = patch.end.unwrap_or_else(|| start.mins(duration));
        if end <= start || end <= now {
            crate::bail!("INVALID_SESSION_TIME", "End must follow start and be in the future");
        }
        if let Some(template) = &patch.template_id {
            get_template(db, template)?;
            session.workout_template_id = Some(template.clone());
            session.workout_plan_json.0 = session.crowd_keys(true);
        }
        if id.is_none() && session.workout_template_id.is_none() {
            crate::bail!(
                "TEMPLATE_REQUIRED",
                "Creating a session requires a real template_id; inspect the split or templates first"
            );
        }
        let status = patch.status.unwrap_or(if !new {
            session.status
        } else if start <= now + HOURS_48 {
            SessionStatus::Committed
        } else {
            SessionStatus::Tentative
        });
        let slot = Candidate::at(start, end, &profile);
        let mut warnings = if status.is_active() { rule_breaks(db, &session.id, &slot, &profile)? } else { Vec::new() };
        let week = week_of(start, &profile.tz());
        let mut edited = BTreeSet::from([week]);
        if id.is_some() {
            edited.insert(week_of(session.planned_start_at, &profile.tz()));
        }
        if status.is_active()
            && active(db, Some(&session.id))?
                .iter()
                .filter(|s| week_of(s.planned_start_at, &profile.tz()) == week)
                .count() as i64
                >= profile.weekly_max_sessions
        {
            warnings.push("exceeds your weekly maximum".into());
        }
        cancel_session_jobs(db, &session.id, now)?;
        session.user_locked = true;
        session.source_revision += 1;
        place(&mut session, start, end, &profile);
        session.status = status;
        allocation(db, &mut session)?;
        if status.is_active() && session.plan("error").is_some_and(|e| !e.is_null()) {
            warnings.push("slot is too short for required movements; adjust the workout or duration".into());
        }
        if new {
            session.insert(db)?;
        } else {
            session.save(db)?;
        }
        if !status.is_open() {
            // Retire old queued writes too; a cancel then queues an owned calendar deletion.
            retire_session(db, &mut session, status, now)?;
        }
        queue_session_write(db, &session, now, false)?;
        if status.is_open() {
            schedule_session_jobs(db, &session, now)?;
        }
        // Its warnings (and what it does to its neighbours) are this edit's answer, not news later.
        inspect(db, &edited, now, false)?;
        Ok(json!({"session": crate::domain::scheduling::session_data(&session), "from": from, "warnings": warnings}))
    })
}

/// End a session that won't happen: its reminders and unsent calendar edits are dropped. A cancelled
/// session was called off by the owner (in chat or by deleting its event): it is locked and marked
/// deleted, so the planner never puts a session back there.
pub fn retire_session(db: &Connection, session: &mut PlannedSession, status: SessionStatus, now: Ts) -> Result<()> {
    if status == SessionStatus::Cancelled {
        session.user_locked = true;
        session.merge_plan(json!({"deleted_by_user": true}));
    }
    session.status = status;
    session.source_revision += 1;
    session.save(db)?;
    cancel_session_jobs(db, &session.id, now)?;
    cancel_pending(db, &session.id, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::ToSpan;

    use crate::db::test::db;
    use crate::domain::scheduling::draft_week;

    /// 2026-10-12: "move today's workout to tomorrow, don't update the others". The owner's move skips a
    /// rest day; replanning must not cancel or move the other sessions to "repair" that.
    #[test]
    fn owner_move_leaves_other_sessions_alone() {
        let db = db();
        let tz = get_profile(&db).unwrap().tz();
        let week = jiff::civil::date(2026, 10, 12);
        let now = Ts::parse("2026-10-11T12:00:00+00:00", None).unwrap();
        let ids = draft_week(&db, week, now, None, None, false, &[]).unwrap().created;
        let before: Vec<PlannedSession> = ids.iter().map(|id| PlannedSession::get(&db, id).unwrap().unwrap()).collect();
        assert_eq!(before.iter().map(|s| weekday(s.planned_start_at.local(&tz).date())).collect::<Vec<_>>(), [0, 2, 4]);

        let tuesday = before[0].planned_start_at + SignedDuration::from_hours(24);
        let moved = edit_session(
            &db,
            Some(&ids[0]),
            SessionPatch { start: Some(tuesday), ..Default::default() },
            now,
            "move-1",
        )
        .unwrap();
        assert_eq!(moved["data"]["warnings"], json!(["skips a rest day"]));

        inspect_weeks(&db, &BTreeSet::from([week]), now).unwrap();
        for other in &before[1..] {
            let after = PlannedSession::get(&db, &other.id).unwrap().unwrap();
            assert_eq!((after.status, after.planned_start_at), (other.status, other.planned_start_at));
        }

        // The agent got its edit's warnings in the tool result: they are not raised again as news.
        let attention =
            |db: &Connection| AgentEvent::select(db, "WHERE type = 'planning.attention' ORDER BY rowid", []).unwrap();
        assert!(attention(&db).is_empty());

        // A change from outside (a drag in the calendar) raises a warning when it appears, not again
        // while it persists, and again if it returns.
        let drag = |db: &Connection, id: &str, start: Ts| {
            let mut session = PlannedSession::get(db, id).unwrap().unwrap();
            session.planned_end_at = start + (session.planned_end_at - session.planned_start_at);
            session.planned_start_at = start;
            session.update(db).unwrap();
            inspect_weeks(db, &BTreeSet::from([week, week.checked_add(7.days()).unwrap()]), now).unwrap()
        };
        let raised = |db: &Connection| -> Vec<String> {
            serde_json::from_value(attention(db).last().unwrap().payload_json.0["warnings"].clone()).unwrap()
        };
        let thursday = before[2].planned_start_at - SignedDuration::from_hours(24);
        let all = drag(&db, &ids[2], thursday).len();
        let clash = raised(&db);
        assert!(
            !clash.is_empty() && clash.len() < all && clash.iter().all(|w| w.ends_with("skips a rest day")),
            "{clash:?}"
        );
        let count = attention(&db).len();
        inspect_weeks(&db, &BTreeSet::from([week]), now).unwrap();
        assert_eq!(attention(&db).len(), count);
        // Thursday back to Friday resolves it; dragging it again raises it again.
        drag(&db, &ids[2], before[2].planned_start_at);
        let count = attention(&db).len();
        drag(&db, &ids[2], thursday);
        assert_eq!((attention(&db).len(), raised(&db)), (count + 1, clash));

        // A session moved to the next week with the same problem doesn't raise it again.
        let late =
            Ts(before[1].planned_start_at.local(&tz).date().at(21, 30, 0, 0).to_zoned(tz.clone()).unwrap().timestamp());
        drag(&db, &ids[1], late);
        let window = raised(&db)
            .into_iter()
            .find(|w| w.contains(&ids[1]) && w.contains("outside your workout window"))
            .expect("window warning raised");
        let count = attention(&db).len();
        drag(&db, &ids[1], late + SignedDuration::from_hours(7 * 24));
        assert!(
            attention(&db)[count..]
                .iter()
                .all(|e| !e.payload_json.0["warnings"].as_array().unwrap().contains(&json!(window)))
        );
    }

    /// 2026-11-05: a missed Thursday Push was moved to Sunday as Cardio and skipped there; the row was
    /// revived in place, so the review only ever saw "Sun cardio SKIPPED". The miss now stays.
    #[test]
    fn rescheduling_a_missed_session_keeps_the_miss() {
        let db = db();
        let week = jiff::civil::date(2026, 10, 12);
        let now = Ts::parse("2026-10-11T12:00:00+00:00", None).unwrap();
        let id = draft_week(&db, week, now, None, None, false, &[]).unwrap().created.remove(0);
        let first = PlannedSession::get(&db, &id).unwrap().unwrap();
        let later = first.planned_end_at.mins(1);
        crate::domain::weekly::mark_missed(&db, later).unwrap();

        let makeup = first.planned_start_at + SignedDuration::from_hours(48);
        let moved =
            edit_session(&db, Some(&id), SessionPatch { start: Some(makeup), ..Default::default() }, later, "makeup")
                .unwrap();
        let new = &moved["data"]["session"];
        assert_ne!(new["id"], json!(id));
        assert_eq!(
            (new["follows_session_id"].as_str(), new["status"].as_str()),
            (Some(id.as_str()), Some("COMMITTED"))
        );
        assert_eq!(PlannedSession::get(&db, &id).unwrap().unwrap().status, SessionStatus::Missed);
    }
}
