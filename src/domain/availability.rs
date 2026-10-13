//! Travel/sickness blocks: persistent owner overrides (port of availability.py). Owner: lead (planning).
use std::collections::BTreeSet;

use jiff::civil::Date;
use serde_json::{Value, json};

use crate::db::models::{AvailabilityBlock, BlockKind, BlockStatus, PlannedSession, SessionStatus};
use crate::db::{Connection, new_id};
use crate::domain::calendar::week_of;
use crate::domain::calendar_writes::queue_session_write;
use crate::domain::core::{emit, mutate};
use crate::domain::notifications::cancel_session_jobs;
use crate::domain::planning::Interval;
use crate::domain::profile::get_profile;
use crate::domain::replanning::{commit_upcoming, expanded, replan_weeks};
use crate::error::Result;
use crate::time::Ts;

/// Active blocks overlapping [start, end).
pub fn blocks(db: &Connection, start: Ts, end: Ts) -> Result<Vec<Interval>> {
    let rows = AvailabilityBlock::select(db, "WHERE status = 'ACTIVE' AND start_at < ?1 AND end_at > ?2", (end, start))?;
    Ok(rows.into_iter().map(|row| Interval { start: row.start_at, end: row.end_at }).collect())
}

/// A block covers the session including prep and travel.
pub fn session_blocked(db: &Connection, session: &PlannedSession) -> Result<bool> {
    let (prep, home) = expanded(session, &get_profile(db)?);
    Ok(!blocks(db, prep, home)?.is_empty())
}

pub fn list_blocks(db: &Connection, now: Ts) -> Result<Vec<Value>> {
    Ok(AvailabilityBlock::select(db, "ORDER BY start_at, id", [])?
        .into_iter()
        .map(|row| json!({"id": row.id, "kind": row.kind, "status": row.status, "start": row.start_at, "end": row.end_at,
            "in_effect": row.status == BlockStatus::Active && row.start_at <= now && now < row.end_at}))
        .collect())
}

fn affected_weeks(db: &Connection, now: Ts) -> Result<BTreeSet<Date>> {
    let tz = get_profile(db)?.tz();
    let mut weeks = BTreeSet::from([week_of(now, &tz), week_of(now + jiff::SignedDuration::from_hours(24 * 7), &tz)]);
    for row in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_start_at > ?1", [now])? {
        weeks.insert(week_of(row.planned_start_at, &tz));
    }
    Ok(weeks)
}

/// Owner is away/sick/busy over [start, end): sessions in it lose their reminders and get replanned;
/// owner-locked ones stay unless `cancel_locked`.
pub fn add(db: &Connection, start: Ts, end: Ts, kind: BlockKind, now: Ts, request_id: &str, cancel_locked: bool) -> Result<Value> {
    if start >= end {
        crate::bail!("INVALID_INTERVAL", "Interval end must follow start");
    }
    let intent = json!({"start": start, "end": end, "kind": kind, "cancel_locked": cancel_locked});
    mutate(db, "availability.added", request_id, intent, now, || {
        let profile = get_profile(db)?;
        let row = AvailabilityBlock { id: new_id(), start_at: start, end_at: end, kind, status: BlockStatus::Active, created_at: now, retracted_at: None };
        row.insert(db)?;
        let mut locked = Vec::new();
        for mut plan in PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_start_at >= ?1 ORDER BY rowid", [now])? {
            let (prep, home) = expanded(&plan, &profile);
            if prep >= end || home <= start {
                continue;
            }
            cancel_session_jobs(db, &plan.id, now)?;
            if plan.user_locked {
                if !cancel_locked {
                    locked.push(plan.id.clone());
                    continue;
                }
                // Explicit later user cancellation supersedes earlier calendar lock.
                plan.user_locked = false;
                plan.status = SessionStatus::Cancelled;
                plan.source_revision += 1;
                plan.merge_plan(json!({"deleted_by_user": true, "manual_lock_cancelled_by_request": request_id}));
                plan.save(db)?;
                queue_session_write(db, &plan, now, false)?;
                emit(db, "availability.locked_session_cancelled", now, json!({"session_id": plan.id, "block_id": row.id, "request_id": request_id}))?;
            }
        }
        let repaired = replan_weeks(db, &affected_weeks(db, now)?, now, false)?;
        let instruction = if locked.is_empty() {
            "Unavailability saved. Affected plan repaired; normal planning resumes after end time."
        } else {
            "Pause saved. Reminders suppressed; manually locked calendar slots need explicit cancellation."
        };
        Ok(json!({"block_id": row.id, "kind": kind, "start": start, "end": end, "locked_conflicts": locked,
            "replanning": repaired, "remote_events_changed": false, "instruction": instruction}))
    })
}

/// Remove a block; planning around it resumes.
pub fn retract(db: &Connection, block_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "availability.retracted", request_id, json!({"block_id": block_id}), now, || {
        let Some(mut row) = AvailabilityBlock::get(db, block_id)? else {
            crate::bail!("AVAILABILITY_NOT_FOUND", "Unknown availability block");
        };
        if now < row.created_at {
            crate::bail!("TIME_REVERSED", "Retraction precedes availability creation");
        }
        row.status = BlockStatus::Retracted;
        row.retracted_at = Some(now);
        row.update(db)?;
        let repaired = replan_weeks(db, &affected_weeks(db, now)?, now, false)?;
        commit_upcoming(db, now)?;
        Ok(json!({"block_id": row.id, "status": row.status, "replanning": repaired, "remote_events_changed": false,
            "instruction": "Availability override removed. Valid future planning resumed."}))
    })
}
