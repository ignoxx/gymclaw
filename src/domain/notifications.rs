//! Durable session reminders (port of notifications.py). The runtime scheduler fires them. Owner: lead.
use serde_json::{Value, json};

use crate::db::models::{JobKind, JobStatus, NotificationJob, PlannedSession, SessionStatus};
use crate::db::{Connection, Json, new_id};
use crate::domain::core::emit;
use crate::error::Result;
use crate::time::Ts;

pub fn pending_jobs(db: &Connection) -> Result<Vec<Value>> {
    Ok(NotificationJob::select(db, "WHERE status = 'PENDING' ORDER BY due_at, id", [])?
        .into_iter()
        .map(|job| json!({"id": job.id, "kind": job.kind, "due_at": job.due_at, "workout_id": job.workout_session_id,
            "planned_session_id": job.planned_session_id, "external_job_id": job.external_job_id, "payload": job.payload_json}))
        .collect())
}

/// Cancel a planned session's pending reminders.
pub fn cancel_session_jobs(db: &Connection, session_id: &str, now: Ts) -> Result<()> {
    for mut job in NotificationJob::select(db, "WHERE planned_session_id = ?1 AND status = 'PENDING' ORDER BY rowid", [session_id])? {
        job.status = JobStatus::Cancelled;
        job.handled_at = Some(now);
        job.update(db)?;
        emit(db, "notifications.cancel_required", now, json!({"job_id": job.id, "external_job_id": job.external_job_id}))?;
    }
    Ok(())
}

/// Get-ready, leave and start reminders for a committed session, once per revision.
pub fn schedule_session_jobs(db: &Connection, session: &PlannedSession, now: Ts) -> Result<()> {
    if session.status != SessionStatus::Committed {
        return Ok(());
    }
    if crate::domain::availability::session_blocked(db, session)? {
        return cancel_session_jobs(db, &session.id, now);
    }
    let existing = NotificationJob::select(db, "WHERE planned_session_id = ?1", [&session.id])?;
    let desired = [
        (JobKind::GetReady, session.prep_start_at, "Gym soon. Start getting ready now."),
        (JobKind::Leave, session.leave_home_at, "Leave now. Gym session starts soon."),
        (JobKind::SessionStart, session.planned_start_at, "Gym session begins. Tap Start when you're there."),
    ];
    for (kind, due, message) in desired {
        if due <= now {
            continue;
        }
        let revision = json!(session.source_revision);
        if existing.iter().any(|j| j.kind == kind && j.due_at == due && j.status != JobStatus::Cancelled && j.payload_json.get("revision") == Some(&revision)) {
            continue;
        }
        let job = NotificationJob {
            id: new_id(), kind, workout_session_id: None, planned_session_id: Some(session.id.clone()), set_log_id: None, due_at: due,
            status: JobStatus::Pending, payload_json: Json(json!({"session_id": session.id, "revision": session.source_revision, "instruction": message})),
            external_job_id: None, handled_at: None,
        };
        job.insert(db)?;
        emit(db, "notifications.schedule_required", now, json!({"job_id": job.id, "due_at": due, "kind": kind}))?;
    }
    Ok(())
}
