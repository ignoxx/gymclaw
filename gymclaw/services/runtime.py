"""OpenClaw owns clocks; SQLite owns intent, execution and delivery receipts.

Domain changes commit before outbound sends. SENDING/UNKNOWN never auto-retry:
Telegram has no exactly-once send key, so a lost response needs human resolution.
"""
from datetime import datetime, timedelta
from hashlib import sha256
from pathlib import Path
import sys

from sqlalchemy import select, update
from sqlalchemy.orm import Session

from gymclaw.models import NotificationDelivery, NotificationJob, PlannedSession, WorkoutSession
from gymclaw.providers.openclaw import AutomationProvider, AutomationSpec, validate_route
from gymclaw.services.errors import DomainError
from gymclaw.services.workout import current, emit, rest_complete, utc


def callback_base(engine, *, project_root: Path, python: str = sys.executable) -> tuple[str, ...]:
    database = engine.url.database
    if not database or database == ":memory:":
        raise DomainError("DURABLE_DB_REQUIRED", "Automations require file-backed SQLite DB")
    # Preserve venv symlink: resolving it would switch to system Python.
    interpreter = str(Path(python).absolute())
    return (interpreter, "-m", "gymclaw.cli", "--db-url", f"sqlite:///{Path(database).absolute()}")


def namespace(engine) -> str:
    database = engine.url.database
    if not database or database == ":memory:":
        raise DomainError("DURABLE_DB_REQUIRED", "Runtime requires file-backed SQLite DB")
    return "gymclaw-" + sha256(str(Path(database).absolute()).encode()).hexdigest()[:16] + ":"


def route_args(profile: str, recipient: str) -> tuple[str, ...]:
    validate_route(profile, recipient)
    return ("--openclaw-profile", profile, "--telegram-id", recipient)


def stale_reason(db: Session, job: NotificationJob, *, now: datetime, delivery: NotificationDelivery | None = None) -> str | None:
    if job.status == "CANCELLED":
        return "cancelled"
    if job.kind == "REST":
        workout = db.get(WorkoutSession, job.workout_session_id)
        if workout is None or workout.status in {"WORKOUT_COMPLETE", "POST_ANALYSIS", "PLAN_UPDATED"}:
            return "workout_finished"
        if delivery:
            actual = current(db, workout.id, now=now)
            expected = delivery.result_json["data"]
            if workout.last_action_at != job.handled_at or any(actual.get(key) != expected.get(key) for key in ("status", "active_exercise", "queue", "rest_job")):
                return "workout_advanced"
    else:
        planned = db.get(PlannedSession, job.planned_session_id)
        if planned is None or planned.status != "COMMITTED" or planned.source_revision != job.payload_json.get("revision"):
            return "plan_changed"
        if now > job.due_at + timedelta(minutes=15):
            return "reminder_expired"
    return None


def automation_plan(db: Session, engine, *, now: datetime, recipient: str, profile: str,
                    project_root: Path, python: str = sys.executable, include_watcher: bool = True) -> list[AutomationSpec]:
    now = utc(now)
    base = callback_base(engine, project_root=project_root, python=python)
    route = route_args(profile, recipient)
    prefix = namespace(engine)
    specs = []
    if include_watcher:
        specs.append(AutomationSpec(name=prefix + "calendar-watch", every="60s", cwd=str(project_root.absolute()),
            argv=base + ("runtime", "watch", "--allow-runtime-changes", "--allow-messages") + route))
    for job in db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING").order_by(NotificationJob.due_at, NotificationJob.id)):
        if stale_reason(db, job, now=now):
            continue
        # Catch up missed timers after restart. DB callback still validates true due_at.
        at = max(job.due_at, now + timedelta(seconds=2))
        specs.append(AutomationSpec(name=prefix + "notification:" + job.id, at=at.isoformat(), cwd=str(project_root.absolute()),
            argv=base + ("runtime", "fire", "--job-id", job.id, "--allow-messages") + route))
    return specs


def sync_automations(engine, provider: AutomationProvider, *, now: datetime, recipient: str,
                     project_root: Path, allow_runtime_changes: bool = False, allow_messages: bool = False,
                     python: str = sys.executable, include_watcher: bool = True) -> dict:
    if not allow_runtime_changes or not allow_messages:
        raise DomainError("RUNTIME_NOT_APPROVED", "Installing executable automations requires --allow-runtime-changes AND --allow-messages")
    validate_route(provider.profile, recipient)
    with Session(engine) as db:
        specs = automation_plan(db, engine, now=now, recipient=recipient, profile=provider.profile,
            project_root=project_root, python=python, include_watcher=include_watcher)
    desired = {spec.name: spec for spec in specs}
    prefix = namespace(engine)
    remote = provider.list_jobs()  # Failure here must not remove or create anything.
    owned = [row for row in remote if row["name"].startswith(prefix)]
    results, seen = [], set()
    for row in owned:
        name = row["name"]
        spec = desired.get(name)
        # Don't manage unrelated jobs, even under this namespace.
        if not (name == prefix + "calendar-watch" or name.startswith(prefix + "notification:")):
            continue
        if spec is None:
            provider.remove(row["id"])
            results.append({"action": "removed", "id": row["id"], "name": name})
            continue
        if name in seen:
            raise DomainError("AUTOMATION_DUPLICATE", "Duplicate managed automation; inspect dedicated runtime before changing it")
        seen.add(name)
        payload = row.get("payload", {})
        if payload.get("kind") != "command" or payload.get("argv") != list(spec.argv) or payload.get("cwd") != spec.cwd or row.get("delivery", {}).get("mode") != "none":
            raise DomainError("AUTOMATION_DRIFT", "Managed automation command/route changed; explicit operator review required")
        schedule = row.get("schedule", {})
        if (spec.every and (schedule.get("kind") != "every" or schedule.get("everyMs") != 60000)) or (spec.at and schedule.get("kind") != "at"):
            raise DomainError("AUTOMATION_DRIFT", "Managed automation schedule changed; explicit operator review required")
        if row.get("enabled") is not True:
            results.append({"action": "disabled_requires_review", "id": row["id"], "name": name})
            continue
        if name.startswith(prefix + "notification:"):
            with Session(engine) as db, db.begin():
                job = db.get(NotificationJob, name.removeprefix(prefix + "notification:"))
                job.external_job_id = row["id"]
        results.append({"action": "adopted", "id": row["id"], "name": name})
    for spec in specs:
        if spec.name in seen:
            continue
        # declaration-key lets Gateway recover concurrent/lost creation responses.
        external_id = provider.create(spec)
        if spec.name.startswith(prefix + "notification:"):
            with Session(engine) as db, db.begin():
                job = db.get(NotificationJob, spec.name.removeprefix(prefix + "notification:"))
                job.external_job_id = external_id
        results.append({"action": "created", "id": external_id, "name": spec.name})
    return {"profile": provider.profile, "results": results, "delivery": "openclaw_command_callbacks", "calendar_writes_enabled": False}


def prepare_notification(db: Session, job_id: str, *, now: datetime, recipient: str, profile: str) -> dict:
    """Advance due domain job and enqueue exact output in the same transaction."""
    now = utc(now)
    validate_route(profile, recipient)
    job = db.get(NotificationJob, job_id)
    if job is None:
        raise DomainError("JOB_NOT_FOUND", "Unknown notification job")
    existing = db.scalar(select(NotificationDelivery).where(NotificationDelivery.notification_job_id == job_id))
    if existing:
        if existing.recipient != recipient or existing.runtime_profile != profile:
            raise DomainError("DELIVERY_ROUTE_MISMATCH", "Stored delivery belongs to a different owner/runtime")
        return {"delivery_id": existing.id, "status": existing.status, "result": existing.result_json}
    if job.status != "PENDING":
        return {"stale": True, "status": job.status}
    if now < job.due_at:
        raise DomainError("NOTIFICATION_NOT_DUE", "Notification has not reached due_at")
    reason = stale_reason(db, job, now=now)
    if reason:
        job.status, job.handled_at = "CANCELLED", now
        emit(db, "notifications.suppressed", now, {"job_id": job.id, "reason": reason})
        return {"stale": True, "reason": reason}
    if job.kind == "REST":
        result = rest_complete(db, job_id, now=now, request_id=f"rest-due:{job_id}")
    else:
        job.status, job.handled_at = "FIRED", now
        event = emit(db, "notifications.due", now, {"job_id": job.id, "session_id": job.planned_session_id})
        result = {"data": {"job_id": job.id}, "events": [{"id": event.id, "type": event.type}], "user_message_hint": job.payload_json["instruction"]}
    message = result.get("user_message_hint")
    if not message:
        return {"stale": True, "result": result}
    delivery = NotificationDelivery(notification_job_id=job_id, recipient=recipient, runtime_profile=profile,
        message=message, result_json=result, created_at=now)
    db.add(delivery)
    db.flush()
    return {"delivery_id": delivery.id, "status": delivery.status, "result": result}


def delivery_data(row: NotificationDelivery) -> dict:
    return {"id": row.id, "job_id": row.notification_job_id, "status": row.status,
        "created_at": row.created_at.isoformat(), "handled_at": row.handled_at.isoformat() if row.handled_at else None,
        "external_message_id": row.external_message_id}


def deliver(engine, provider: AutomationProvider, delivery_id: str, *, now: datetime, allow_messages: bool = False) -> dict:
    if not allow_messages:
        raise DomainError("MESSAGES_NOT_APPROVED", "Live Telegram delivery requires --allow-messages")
    now = utc(now)
    with Session(engine) as db, db.begin():
        db.connection().exec_driver_sql("BEGIN IMMEDIATE")
        row = db.get(NotificationDelivery, delivery_id)
        if row is None:
            raise DomainError("DELIVERY_NOT_FOUND", "Unknown notification delivery")
        if row.runtime_profile != provider.profile:
            raise DomainError("DELIVERY_ROUTE_MISMATCH", "Delivery belongs to another OpenClaw profile")
        if row.status != "PENDING":
            return delivery_data(row)
        job = db.get(NotificationJob, row.notification_job_id)
        reason = stale_reason(db, job, now=now, delivery=row)
        if reason:
            row.status, row.handled_at = "CANCELLED", now
            emit(db, "notifications.delivery_suppressed", now, {"delivery_id": row.id, "reason": reason})
            return delivery_data(row)
        row.status, row.handled_at = "SENDING", now
        recipient, message = row.recipient, row.message
    # Claim committed; no network I/O under SQLite write transaction.
    try:
        receipt = provider.send(recipient, message)
    except Exception:
        with Session(engine) as db, db.begin():
            db.execute(update(NotificationDelivery).where(NotificationDelivery.id == delivery_id, NotificationDelivery.status == "SENDING").values(status="UNKNOWN"))
            emit(db, "notifications.delivery_unknown", now, {"delivery_id": delivery_id, "instruction": "Check Telegram before explicitly resolving; do not auto-retry."})
        raise DomainError("TELEGRAM_DELIVERY_UNKNOWN", "Delivery outcome unknown; inspect Telegram, then runtime resolve. No automatic resend.") from None
    with Session(engine) as db, db.begin():
        db.execute(update(NotificationDelivery).where(NotificationDelivery.id == delivery_id, NotificationDelivery.status == "SENDING").values(status="SENT", external_message_id=receipt))
        emit(db, "notifications.sent", now, {"delivery_id": delivery_id, "message_id": receipt})
        return delivery_data(db.get(NotificationDelivery, delivery_id))


def resolve_delivery(db: Session, delivery_id: str, *, outcome: str, now: datetime) -> dict:
    """Explicit operator decision only. Must verify no sender process is running."""
    row = db.get(NotificationDelivery, delivery_id)
    if row is None:
        raise DomainError("DELIVERY_NOT_FOUND", "Unknown notification delivery")
    if row.status not in {"UNKNOWN", "SENDING"} or outcome not in {"sent", "not-sent"}:
        raise DomainError("INVALID_DELIVERY_RESOLUTION", "Resolve only ambiguous delivery with sent or not-sent")
    row.status = "SENT" if outcome == "sent" else "PENDING"
    row.handled_at = utc(now)
    emit(db, "notifications.delivery_resolved", utc(now), {"delivery_id": row.id, "outcome": outcome, "source": "operator_confirmation"})
    return delivery_data(row)
