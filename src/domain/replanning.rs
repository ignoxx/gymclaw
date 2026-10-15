//! Owner edits and rolling repair (port of replanning.py). Manual intent is preserved: only invalid,
//! unlocked future sessions are repaired, and a rule the owner broke on purpose (rest day, weekly max)
//! never moves or cancels other sessions. Owner: lead (planning).
use std::collections::BTreeSet;

use jiff::SignedDuration;
use jiff::civil::Date;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{CalendarWrite, PlannedSession, SessionStatus, WriteStatus};
use crate::domain::calendar::{allocation, week_of};
use crate::domain::calendar_busy::busy_intervals;
use crate::domain::calendar_writes::queue_session_write;
use crate::domain::core::{emit, mutate};
use crate::domain::crowd::{CrowdModel, planning_signals};
use crate::domain::notifications::{cancel_session_jobs, schedule_session_jobs};
use crate::domain::planning::{Candidate, Inputs, Interval, PlannerConfig, monday, overlaps, plan_week, recovery_ok, weekday};
use crate::domain::profile::{Profile, get_profile};
use crate::domain::scheduling::{busy_window, interval};
use crate::domain::templates::get_template;
use crate::error::Result;
use crate::time::Ts;

const HOURS_48: SignedDuration = SignedDuration::from_hours(48);

fn within_bounds(session: &PlannedSession, profile: &Profile) -> bool {
    let tz = profile.tz();
    let (start, end) = (session.planned_start_at.local(&tz), session.planned_end_at.local(&tz));
    profile.weekdays_allowed.contains(&weekday(start.date()))
        && start.date() == end.date()
        && start.time() >= profile.earliest_workout_start
        && end.time() <= profile.latest_workout_finish
}

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

/// Put a session on a planner slot: times, crowd forecast, workout fit, calendar intent and reminders.
/// Inserts the session if it is new.
pub fn place(db: &Connection, session: &mut PlannedSession, slot: &Candidate, week: Date, now: Ts, crowd_model: Option<&CrowdModel>) -> Result<()> {
    session.planned_start_at = slot.start;
    session.planned_end_at = slot.end;
    session.prep_start_at = slot.prep_start;
    session.leave_home_at = slot.leave_home;
    session.expected_finish_at = slot.end;
    session.crowd_prediction = slot.crowd;
    session.crowd_confidence = Some(slot.confidence);
    session.status = if slot.start <= now + HOURS_48 { SessionStatus::Committed } else { SessionStatus::Tentative };
    session.week_id = week.to_string();
    allocation(db, session)?;
    session.workout_plan_json.0 = session.crowd_keys(false);
    if slot.crowd.is_some() {
        let demo = crowd_model.is_some_and(|m| m.predict(slot.start).demo);
        session.merge_plan(json!({"crowd_source": if demo { "demo_fixture" } else { "local_crowd_model" }, "crowd_score_kind": "personal_perceived_crowd_proxy"}));
    }
    if PlannedSession::get(db, &session.id)?.is_some() { session.save(db)? } else { session.insert(db)? }
    queue_session_write(db, session, now, false)?;
    schedule_session_jobs(db, session, now)
}

/// Repair the given weeks: invalid unlocked future sessions move to the best valid slot (or are
/// cancelled), missing ones are added. → `{changed, warnings}`.
pub fn replan_weeks(db: &Connection, weeks: &BTreeSet<Date>, now: Ts, reconsider_tentative: bool) -> Result<Value> {
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let mut changed: Vec<Value> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for &week in weeks {
        if !monday(week) {
            crate::bail!("INVALID_WEEK", "week_start must be Monday");
        }
        let all_rows = PlannedSession::select(db, "ORDER BY planned_start_at, id", [])?;
        let week_rows: Vec<&PlannedSession> = all_rows.iter().filter(|s| week_of(s.planned_start_at, &tz) == week).collect();
        if week_rows.is_empty() {
            continue; // Sync alone must not invent an un-onboarded training plan.
        }
        let (start, end) = busy_window(week, &profile)?;
        let busy = busy_intervals(db, start, end)?;
        // Skipped sessions count toward the week: the owner chose not to make them up.
        let fixed: Vec<&PlannedSession> = all_rows
            .iter()
            .filter(|s| {
                s.status == SessionStatus::Skipped
                    || s.status.is_active()
                        && (s.user_locked || matches!(s.status, SessionStatus::Started | SessionStatus::Completed) || s.planned_start_at < now || week_of(s.planned_start_at, &tz) != week)
            })
            .collect();
        let mut invalid: Vec<PlannedSession> = Vec::new();
        let mut kept: Vec<&PlannedSession> = fixed.clone();
        for &session in &week_rows {
            if fixed.iter().any(|f| f.id == session.id) || !session.status.is_active() {
                continue;
            }
            let (prep, home) = expanded(session, &profile);
            // A rule the owner broke on purpose (rest day, weekly max) is theirs to resolve: other sessions stay.
            let planner_kept: Vec<&&PlannedSession> = kept.iter().filter(|s| !s.user_locked).collect();
            let valid = within_bounds(session, &profile)
                && !busy.iter().any(|b| overlaps(prep, home, b))
                && planner_kept.iter().all(|other| recovery_ok(session.planned_start_at, session.planned_end_at, &interval(other), &profile));
            let reconsider = reconsider_tentative && session.status == SessionStatus::Tentative && session.planned_start_at > now + HOURS_48;
            let in_week = planner_kept.iter().filter(|s| week_of(s.planned_start_at, &tz) == week).count() as i64;
            if valid && !reconsider && in_week < profile.weekly_max_sessions {
                kept.push(session);
            } else {
                invalid.push(session.clone());
            }
        }
        let mut unavailable: Vec<Interval> = all_rows.iter().filter(|s| s.deleted_by_user()).map(interval).collect();
        let horizon = now + SignedDuration::from_hours(24 * 10);
        if horizon < end {
            unavailable.push(Interval { start: horizon, end });
        }
        let signals = planning_signals(db, week, now, 15)?;
        let crowd_model = if signals.is_empty() { None } else { Some(CrowdModel::new(db, now)?) };
        let existing: Vec<Interval> = kept.iter().map(|s| interval(s)).collect();
        let planned = plan_week(&profile, week, &Inputs { busy: &busy, unavailable: &unavailable, existing: &existing, signals: &signals }, now, &PlannerConfig::default())?;
        // Existing legacy plans may still be moved, but no unknown workout is fabricated.
        let template_id = week_rows.iter().find_map(|s| s.workout_template_id.clone());
        for (index, slot) in planned.sessions.iter().enumerate() {
            let (mut session, old_start, same_time) = match invalid.get(index) {
                Some(session) => {
                    let mut session = session.clone();
                    let same_time = session.planned_start_at == slot.start && session.planned_end_at == slot.end;
                    if same_time && session.crowd_prediction == slot.crowd && session.crowd_confidence == Some(slot.confidence) {
                        continue; // Reconsidering unchanged tentative slots is not a new action.
                    }
                    if !same_time {
                        cancel_session_jobs(db, &session.id, now)?;
                    }
                    session.source_revision += 1;
                    let old = session.planned_start_at;
                    (session, Some(old), same_time)
                }
                None => (PlannedSession::new(week.to_string(), template_id.clone(), false), None, false),
            };
            place(db, &mut session, slot, week, now, crowd_model.as_ref())?;
            let action = if same_time { "forecast_updated" } else if old_start.is_some() { "moved" } else { "replacement" };
            changed.push(json!({"session_id": session.id, "from": old_start, "to": slot.start, "action": action}));
        }
        for session in invalid.iter_mut().skip(planned.sessions.len()) {
            session.status = SessionStatus::Cancelled;
            session.source_revision += 1;
            session.save(db)?;
            cancel_session_jobs(db, &session.id, now)?;
            queue_session_write(db, session, now, false)?;
            changed.push(json!({"session_id": session.id, "action": "cancelled_no_valid_slot"}));
        }
        if planned.shortfall > 0 {
            let count = planned.existing_count + planned.sessions.len() as i64;
            warnings.push(format!("Week {week}: {count}/{} sessions feasible; recovery/blockers preserved.", profile.weekly_target_sessions));
        }
        // Re-read: placing may have changed this week's rows.
        let week_rows: Vec<PlannedSession> = PlannedSession::select(db, "ORDER BY planned_start_at, id", [])?.into_iter().filter(|s| week_rows.iter().any(|w| w.id == s.id)).collect();
        if week_rows.iter().filter(|s| s.user_locked && s.status.is_active()).count() as i64 > profile.weekly_max_sessions {
            warnings.push("Manually locked sessions exceed weekly maximum; edited events kept.".into());
        }
        for session in week_rows.iter().filter(|s| s.user_locked && s.status.is_active()) {
            let (prep, home) = expanded(session, &profile);
            let (local_start, local_end) = (session.planned_start_at.local(&tz), session.planned_end_at.local(&tz));
            if local_start.date() != local_end.date() || local_start.time() < profile.earliest_workout_start || local_end.time() > profile.latest_workout_finish {
                warnings.push(format!("Manually edited session {} is outside configured workout time bounds; edited event kept.", session.id));
            }
            if busy.iter().any(|b| overlaps(prep, home, b)) {
                warnings.push(format!("Manually edited session {} overlaps a blocker including prep/travel; kept for explicit resolution.", session.id));
            }
            if fixed.iter().any(|other| other.id != session.id && !recovery_ok(session.planned_start_at, session.planned_end_at, &interval(other), &profile)) {
                warnings.push(format!("Manually edited session {} conflicts with recovery; edited event kept.", session.id));
            }
            if session.plan("error").is_some_and(|e| !e.is_null()) {
                warnings.push(format!("Manually edited session {} is too short for required movement; adjust slot explicitly.", session.id));
            }
        }
    }
    if !changed.is_empty() {
        emit(db, "planning.replanned", now, json!({"changes": changed, "warnings": warnings}))?;
    }
    let mut unique: Vec<String> = Vec::new();
    for w in warnings {
        if !unique.contains(&w) {
            unique.push(w);
        }
    }
    Ok(json!({"changed": changed, "warnings": unique}))
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
        if id.is_some() && (session.status == SessionStatus::Started || session.planned_end_at <= now) {
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
        let status = patch.status.unwrap_or(if id.is_some() { session.status } else { SessionStatus::Tentative });
        let slot = Candidate::manual(start, end, &profile);
        let mut warnings = rule_breaks(db, &session.id, &slot, &profile)?;
        let week = week_of(start, &profile.tz());
        if active(db, Some(&session.id))?.iter().filter(|s| week_of(s.planned_start_at, &profile.tz()) == week).count() as i64 >= profile.weekly_max_sessions {
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
        if session.plan("error").is_some_and(|e| !e.is_null()) {
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

        let repaired = replan_weeks(&db, &BTreeSet::from([week]), now, false).unwrap();
        assert_eq!(repaired["changed"], json!([]));
        for other in &before[1..] {
            let after = PlannedSession::get(&db, &other.id).unwrap().unwrap();
            assert_eq!((after.status, after.planned_start_at), (other.status, other.planned_start_at));
        }
    }
}
