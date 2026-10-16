//! Session mechanics and non-mutating plan inspection. Policy decisions belong to the agent.
use std::collections::BTreeSet;

use jiff::SignedDuration;
use jiff::civil::Date;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{CalendarWrite, PlannedSession, SessionStatus, WriteStatus};
use crate::domain::calendar::{allocation, week_of};
use crate::domain::calendar_busy::busy_intervals;
use crate::domain::calendar_writes::queue_session_write;
use crate::domain::core::mutate;
use crate::domain::notifications::{cancel_session_jobs, schedule_session_jobs};
use crate::domain::planning::{Candidate, Inputs, PlannerConfig, monday, overlaps, plan_week, recovery_ok, weekday};
use crate::domain::profile::{Profile, get_profile};
use crate::domain::scheduling::{busy_window, interval};
use crate::domain::templates::get_template;
use crate::error::Result;
use crate::time::Ts;

const HOURS_48: SignedDuration = SignedDuration::from_hours(48);

/// The session including prep and travel: (leave-prep start, home again).
pub fn expanded(session: &PlannedSession, profile: &Profile) -> (Ts, Ts) {
    (session.planned_start_at.mins(-(profile.prep_minutes + profile.commute_to_gym_minutes)), session.planned_end_at.mins(profile.commute_home_minutes))
}

fn active(db: &Connection, except: Option<&str>) -> Result<Vec<PlannedSession>> {
    PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED', 'STARTED', 'COMPLETED') AND id IS NOT ?1 ORDER BY rowid", [except])
}

/// Sessions within 48 h are committed (calendar shows them confirmed) and get their reminders.
pub fn commit_upcoming(db: &Connection, now: Ts) -> Result<()> {
    for mut session in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') ORDER BY rowid", [])? {
        if session.status == SessionStatus::Tentative && now < session.planned_start_at && session.planned_start_at <= now + HOURS_48 {
            session.status = SessionStatus::Committed;
            session.source_revision += 1;
            session.save(db)?;
            queue_session_write(db, &session, now, false)?;
        }
        schedule_session_jobs(db, &session, now)?;
    }
    Ok(())
}

/// Inspect existing weeks without changing sessions. Conflicts and infeasibility become durable,
/// deduplicated agent events; only the agent decides whether an exception or an edit is appropriate.
pub fn inspect_weeks(db: &Connection, weeks: &BTreeSet<Date>, now: Ts) -> Result<Value> {
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let rows = PlannedSession::select(db, "ORDER BY planned_start_at, id", [])?;
    let mut warnings = Vec::new();
    for &week in weeks {
        if !monday(week) { crate::bail!("INVALID_WEEK", "week_start must be Monday"); }
        let sessions: Vec<_> = rows.iter().filter(|s| week_of(s.planned_start_at, &tz) == week && s.status.is_active()).collect();
        if !rows.iter().any(|s| week_of(s.planned_start_at, &tz) == week) { continue; }
        for session in sessions.iter().filter(|s| s.status.is_open() && s.planned_end_at > now) {
            let slot = Candidate::manual(session.planned_start_at, session.planned_end_at, &profile);
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
        let existing: Vec<_> = rows.iter().filter(|s| s.status.is_active() || s.status == SessionStatus::Skipped).map(interval).collect();
        let (start, end) = busy_window(week, &profile)?;
        let busy = busy_intervals(db, start, end)?;
        let unavailable = crate::domain::scheduling::deleted_intervals(db)?;
        let plan = plan_week(&profile, week, &Inputs { busy: &busy, unavailable: &unavailable, existing: &existing, signals: &[] }, now, &PlannerConfig::default())?;
        let count = sessions.len() as i64;
        if count < profile.weekly_target_sessions {
            warnings.push(format!("Week {week}: {count}/{} sessions saved; {} additional default slots feasible. Agent decides whether to add or accept shortfall.", profile.weekly_target_sessions, plan.sessions.len()));
        }
    }
    if !warnings.is_empty() {
        crate::domain::events::request(db, "planning.attention", json!({"warnings": warnings}), now)?;
    }
    Ok(json!({"changed": [], "warnings": warnings}))
}

/// Planner rules an owner-chosen slot breaks, as short reasons for the owner.
fn rule_breaks(db: &Connection, session_id: &str, slot: &Candidate, profile: &Profile) -> Result<Vec<String>> {
    let tz = profile.tz();
    let (start, end) = (slot.start.local(&tz), slot.end.local(&tz));
    let mut reasons = Vec::new();
    if !profile.weekdays_allowed.contains(&weekday(start.date())) || start.date() != end.date() || start.time() < profile.earliest_workout_start || end.time() > profile.latest_workout_finish {
        reasons.push(format!("outside your workout window ({}–{})", profile.earliest_workout_start.strftime("%H:%M"), profile.latest_workout_finish.strftime("%H:%M")));
    }
    let day = Ts(start.date().to_zoned(tz.clone())?.timestamp());
    if busy_intervals(db, day - SignedDuration::from_hours(24), day + SignedDuration::from_hours(48))?.iter().any(|b| overlaps(slot.prep_start, slot.home_at, b)) {
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
pub fn edit_session(db: &Connection, id: Option<&str>, patch: SessionPatch, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "planning.session_edited", request_id, json!({"session_id": id, "patch": patch}), now, || {
        let profile = get_profile(db)?;
        let mut session = match id {
            Some(id) => PlannedSession::get(db, id)?.ok_or_else(|| crate::error::Error::domain("SESSION_NOT_FOUND", "Unknown session"))?,
            None => PlannedSession::new(String::new(), None, true),
        };
        // Scheduling status alone is editable; actual training history belongs to workout tools.
        let trained = crate::db::models::WorkoutSession::first(db, "WHERE planned_session_id = ?1 AND started_at IS NOT NULL", [&session.id])?.is_some();
        if id.is_some() && (trained || session.planned_end_at <= now) {
            crate::bail!("SESSION_NOT_EDITABLE", "Use workout tools for a running or historical workout; session edits are for upcoming slots");
        }
        let start = patch.start.or(id.map(|_| session.planned_start_at)).ok_or_else(|| crate::error::Error::domain("INVALID_ARGUMENTS", "Creating a session requires start"))?;
        let duration = if id.is_some() { (session.planned_end_at - session.planned_start_at).as_mins() } else { profile.preferred_workout_minutes };
        let end = patch.end.unwrap_or_else(|| start.mins(duration));
        if end <= start || end <= now {
            crate::bail!("INVALID_SESSION_TIME", "End must follow start and be in the future");
        }
        if let Some(template) = &patch.template_id {
            get_template(db, template)?;
            session.workout_template_id = Some(template.clone());
            session.workout_plan_json.0 = session.crowd_keys(true);
        }
        // A raw create chooses the next split workout without changing any existing session.
        if id.is_none() && session.workout_template_id.is_none() {
            crate::bail!("TEMPLATE_REQUIRED", "Creating a session requires a real template_id; inspect the split or templates first");
        }
        let status = patch.status.unwrap_or(if id.is_some() { session.status } else if start <= now + HOURS_48 { SessionStatus::Committed } else { SessionStatus::Tentative });
        let slot = Candidate::manual(start, end, &profile);
        let mut warnings = if status.is_active() { rule_breaks(db, &session.id, &slot, &profile)? } else { Vec::new() };
        let week = week_of(start, &profile.tz());
        if status.is_active() && active(db, Some(&session.id))?.iter().filter(|s| week_of(s.planned_start_at, &profile.tz()) == week).count() as i64 >= profile.weekly_max_sessions {
            warnings.push("exceeds your weekly maximum".into());
        }
        cancel_session_jobs(db, &session.id, now)?;
        session.user_locked = true;
        session.source_revision += 1;
        session.planned_start_at = start;
        session.planned_end_at = end;
        session.prep_start_at = slot.prep_start;
        session.leave_home_at = slot.leave_home;
        session.expected_finish_at = end;
        session.week_id = week.to_string();
        session.status = status;
        allocation(db, &mut session)?;
        if status.is_active() && session.plan("error").is_some_and(|e| !e.is_null()) {
            warnings.push("slot is too short for required movements; adjust the workout or duration".into());
        }
        if status == SessionStatus::Cancelled {
            session.merge_plan(json!({"deleted_by_user": true}));
        }
        if id.is_some() { session.save(db)?; } else { session.insert(db)?; }
        if !status.is_open() {
            // Retire old queued writes too; a cancel then queues an owned calendar deletion.
            retire_session(db, &mut session, status, now)?;
        }
        queue_session_write(db, &session, now, false)?;
        if status.is_open() { schedule_session_jobs(db, &session, now)?; }
        Ok(json!({"session": crate::domain::scheduling::session_data(&session), "warnings": warnings, "side_effects": []}))
    })
}

/// End a session that won't happen. Its calendar event stays as history; reminders and unsent edits are dropped.
pub fn retire_session(db: &Connection, session: &mut PlannedSession, status: SessionStatus, now: Ts) -> Result<()> {
    session.status = status;
    session.source_revision += 1;
    session.save(db)?;
    cancel_session_jobs(db, &session.id, now)?;
    for mut write in CalendarWrite::select(db, "WHERE planned_session_id = ?1 AND status = 'PENDING' ORDER BY rowid", [&session.id])? {
        write.status = WriteStatus::Cancelled;
        write.handled_at = Some(now);
        write.update(db)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::scheduling::schedule_week;
    use crate::domain::workout::tests::db;

    /// 2026-10-12: "move today's workout to tomorrow, don't update the others". The owner's move skips a
    /// rest day; replanning must not cancel or move the other sessions to "repair" that.
    #[test]
    fn owner_move_leaves_other_sessions_alone() {
        let db = db();
        let tz = get_profile(&db).unwrap().tz();
        let week = jiff::civil::date(2026, 10, 12);
        let now = Ts::parse("2026-10-11T12:00:00+00:00", None).unwrap();
        let planned = schedule_week(&db, week, now, None, None, &[]).unwrap();
        let ids: Vec<String> = planned["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect();
        let before: Vec<PlannedSession> = ids.iter().map(|id| PlannedSession::get(&db, id).unwrap().unwrap()).collect();
        assert_eq!(before.iter().map(|s| weekday(s.planned_start_at.local(&tz).date())).collect::<Vec<_>>(), [0, 2, 4]);

        let tuesday = before[0].planned_start_at + SignedDuration::from_hours(24);
        let moved = edit_session(&db, Some(&ids[0]), SessionPatch { start: Some(tuesday), ..Default::default() }, now, "move-1").unwrap();
        assert_eq!(moved["data"]["warnings"], json!(["skips a rest day"]));
        assert_eq!(moved["data"]["side_effects"], json!([]));

        let repaired = inspect_weeks(&db, &BTreeSet::from([week]), now).unwrap();
        assert_eq!(repaired["changed"], json!([]));
        for other in &before[1..] {
            let after = PlannedSession::get(&db, &other.id).unwrap().unwrap();
            assert_eq!((after.status, after.planned_start_at), (other.status, other.planned_start_at));
        }
    }
}
