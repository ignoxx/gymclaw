"""Local durable job dispatch. External automation/message delivery is not implemented."""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import NotificationJob
from gymclaw.services.workout import rest_complete, utc


def pending_jobs(db: Session) -> list[dict]:
    return [{"id": job.id, "kind": job.kind, "due_at": job.due_at.isoformat(), "workout_id": job.workout_session_id, "external_job_id": job.external_job_id, "payload": job.payload_json} for job in db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING").order_by(NotificationJob.due_at, NotificationJob.id))]


def dispatch_due(db: Session, *, now: datetime) -> dict:
    now = utc(now)
    jobs = list(db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING", NotificationJob.kind == "REST", NotificationJob.due_at <= now).order_by(NotificationJob.due_at, NotificationJob.id)))
    results = [rest_complete(db, job.id, now=now, request_id=f"rest-due:{job.id}") for job in jobs]
    return {"data": {"processed": len(results), "results": [r["data"] for r in results], "delivery": "local_domain_only"}, "events": [e for r in results for e in r["events"]], "user_message_hint": "\n".join(r["user_message_hint"] for r in results if r["user_message_hint"]) or None}
