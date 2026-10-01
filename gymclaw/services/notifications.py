"""Durable reminders and local debug dispatch; runtime handles external delivery."""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import NotificationJob, PlannedSession
from gymclaw.services.workout import emit, rest_complete, utc


def pending_jobs(db: Session) -> list[dict]:
    return [{"id": job.id, "kind": job.kind, "due_at": job.due_at.isoformat(), "workout_id": job.workout_session_id, "planned_session_id": job.planned_session_id, "external_job_id": job.external_job_id, "payload": job.payload_json} for job in db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING").order_by(NotificationJob.due_at, NotificationJob.id))]


def cancel_session_jobs(db: Session, session_id: str, *, now: datetime):
    for job in db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == session_id, NotificationJob.status == "PENDING")):
        job.status = "CANCELLED"
        job.handled_at = now
        emit(db, "notifications.cancel_required", now, {"job_id": job.id, "external_job_id": job.external_job_id})


def schedule_session_jobs(db: Session, session: PlannedSession, *, now: datetime):
    if session.status != "COMMITTED":
        return
    from gymclaw.services.availability import session_blocked
    if session_blocked(db, session):
        cancel_session_jobs(db, session.id, now=now)
        return
    existing = list(db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == session.id)))
    desired = [("GET_READY", session.prep_start_at, "Gym soon. Start getting ready now."), ("LEAVE", session.leave_home_at, "Leave now. Gym session starts soon."), ("SESSION_START", session.planned_start_at, "Gym session begins. Start workout when you arrive.")]
    for kind, due, message in desired:
        if due <= now:
            continue
        if any(j.kind == kind and j.due_at == due and j.status != "CANCELLED" and j.payload_json.get("revision") == session.source_revision for j in existing):
            continue
        job = NotificationJob(kind=kind, planned_session_id=session.id, due_at=due, payload_json={"session_id": session.id, "revision": session.source_revision, "instruction": message})
        db.add(job)
        db.flush()
        emit(db, "notifications.schedule_required", now, {"job_id": job.id, "due_at": due.isoformat(), "kind": kind})


def dispatch_due(db: Session, *, now: datetime) -> dict:
    now = utc(now)
    jobs = list(db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING", NotificationJob.due_at <= now).order_by(NotificationJob.due_at, NotificationJob.id)))
    results = []
    for job in jobs:
        if job.kind == "REST":
            results.append(rest_complete(db, job.id, now=now, request_id=f"rest-due:{job.id}"))
        else:
            job.status = "FIRED"
            job.handled_at = now
            event = emit(db, "notifications.due", now, {"job_id": job.id, "session_id": job.planned_session_id, "instruction": job.payload_json["instruction"]})
            results.append({"data": event.payload_json, "events": [{"id": event.id, "type": event.type}], "user_message_hint": job.payload_json["instruction"]})
    return {"data": {"processed": len(results), "results": [r["data"] for r in results], "delivery": "local_domain_only"}, "events": [e for r in results for e in r["events"]], "user_message_hint": "\n".join(r["user_message_hint"] for r in results if r["user_message_hint"]) or None}
