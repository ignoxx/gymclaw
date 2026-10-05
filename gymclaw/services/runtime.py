"""OpenClaw owns clocks; SQLite owns intent, execution and delivery receipts.

Domain changes commit before outbound sends. SENDING/UNKNOWN never auto-retry:
Telegram has no exactly-once send key, so a lost response needs human resolution.
"""
from dataclasses import replace
from datetime import datetime, timedelta
import os
from hashlib import sha256
from pathlib import Path
import sys

from sqlalchemy import select, update
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, NotificationDelivery, NotificationJob, OnboardingState, PlannedSession, RuntimeSettings, WorkoutSession
from gymclaw.providers.openclaw import AutomationProvider, AutomationSpec, validate_route
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.templates import get_template
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


def configure_runtime(db: Session, *, profile: str, recipient: str, project_root: Path, python: str,
                      template_id: str | None = None, allow_calendar_writes: bool = False, crowd_polling: bool = False) -> RuntimeSettings:
    validate_route(profile, recipient)
    if template_id:
        get_template(db, template_id)
    scope = {"profile": profile, "recipient": recipient, "project_root": str(project_root.absolute()), "python_path": str(Path(python).absolute())}
    settings = db.get(RuntimeSettings, 1)
    if settings is None:
        settings = RuntimeSettings(**scope)
        db.add(settings); db.flush()
    elif any(getattr(settings, key) != value for key, value in scope.items()):
        raise DomainError("RUNTIME_SCOPE_MISMATCH", "Runtime DB is bound to another owner/profile/deployment path")
    if not settings.enabled:
        raise DomainError("RUNTIME_PAUSED", "Runtime paused; explicit resume required, callbacks cannot reactivate it")
    if template_id:
        settings.template_id = template_id
    if allow_calendar_writes:
        settings.calendar_writes_enabled = True
    if crowd_polling:
        settings.crowd_polling_enabled = True
    return settings


def planning_template(db: Session, settings: RuntimeSettings | None) -> str | None:
    """Explicitly activated template, else the first template of the owner's confirmed split."""
    if settings and settings.template_id:
        return settings.template_id
    row = db.get(OnboardingState, 1)
    return row.split_json[0] if row and row.split_json else None


def runtime_authority(db: Session) -> dict:
    settings = db.get(RuntimeSettings, 1)
    return {"configured": settings is not None, "enabled": settings.enabled if settings else False,
        "template_id": planning_template(db, settings) if settings else None,
        "calendar_writes_enabled": settings.calendar_writes_enabled if settings else False,
        "crowd_polling_enabled": settings.crowd_polling_enabled if settings else False}


def route_args(profile: str, recipient: str) -> tuple[str, ...]:
    validate_route(profile, recipient)
    return ("--openclaw-profile", profile, "--telegram-id", recipient)


def stale_reason(db: Session, job: NotificationJob, *, now: datetime, delivery: NotificationDelivery | None = None) -> str | None:
    if job.status == "CANCELLED":
        return "cancelled"
    if job.kind == "REST":
        from gymclaw.services.availability import blocks
        if blocks(db, now, now + timedelta(microseconds=1)):
            return "availability_override"
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
        from gymclaw.services.availability import session_blocked
        if session_blocked(db, planned):
            return "availability_override"
        if now > job.due_at + timedelta(minutes=15):
            return "reminder_expired"
    return None


def event_stale_reason(db: Session, row: NotificationDelivery, *, now: datetime) -> str | None:
    if now > row.created_at + timedelta(hours=24):
        return "event_message_expired"
    event = db.get(AgentEvent, row.agent_event_id)
    if event.type == "planning.no_show":
        # Started, skipped, moved or already over: the question no longer applies.
        planned = db.get(PlannedSession, event.payload_json["session_id"])
        if planned is None or planned.status != "COMMITTED" or planned.planned_end_at <= now or planned.planned_start_at.isoformat() != event.payload_json["start"]:
            return "session_resolved"
    return None


def automation_plan(db: Session, engine, *, now: datetime, recipient: str, profile: str,
                    project_root: Path, python: str = sys.executable, include_watcher: bool = True) -> list[AutomationSpec]:
    now = utc(now)
    base = callback_base(engine, project_root=project_root, python=python)
    route = route_args(profile, recipient)
    prefix = namespace(engine)
    specs = []
    settings = db.get(RuntimeSettings, 1)
    if settings and (settings.profile != profile or settings.recipient != recipient):
        raise DomainError("RUNTIME_SCOPE_MISMATCH", "Runtime belongs to another owner/profile")
    if include_watcher:
        specs.append(AutomationSpec(name=prefix + "calendar-watch", every="60s", cwd=str(project_root.absolute()),
            argv=base + ("runtime", "watch", "--allow-runtime-changes", "--allow-messages") + route))
        if settings and planning_template(db, settings):
            specs.append(AutomationSpec(name=prefix + "weekly-plan", cron="0 19 * * 0", timezone=get_profile(db).timezone,
                cwd=str(project_root.absolute()), argv=base + ("runtime", "weekly", "--allow-runtime-changes", "--allow-messages") + route))
        if settings and settings.crowd_polling_enabled:
            specs.append(AutomationSpec(name=prefix + "crowd-poll", every="15m", cwd=str(project_root.absolute()),
                argv=base + ("runtime", "poll-crowd") + route))
    for job in db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING").order_by(NotificationJob.due_at, NotificationJob.id)):
        if stale_reason(db, job, now=now):
            continue
        # Catch up missed timers after restart. DB callback still validates true due_at.
        # The coach plugin announces rest end in-process; the cron ping is a fallback after a restart.
        grace = timedelta(seconds=30) if job.kind == "REST" else timedelta()
        at = max(job.due_at + grace, now + timedelta(seconds=2))
        specs.append(AutomationSpec(name=prefix + "notification:" + job.id, at=at.isoformat(), cwd=str(project_root.absolute()),
            argv=base + ("runtime", "fire", "--job-id", job.id, "--allow-messages") + route))
    config = os.environ.get("OPENCLAW_CONFIG_PATH")
    if config:
        # Managed NemoClaw owns config; callbacks retain its path, never credentials.
        config = str(Path(config).absolute())
        specs = [replace(spec, env=(("OPENCLAW_CONFIG_PATH", config),)) for spec in specs]
    return specs


def sync_automations(engine, provider: AutomationProvider, *, now: datetime, recipient: str,
                     project_root: Path, allow_runtime_changes: bool = False, allow_messages: bool = False,
                     python: str = sys.executable, include_watcher: bool = True, template_id: str | None = None,
                     allow_calendar_writes: bool = False, crowd_polling: bool = False, configure: bool = True) -> dict:
    if not allow_runtime_changes or not allow_messages:
        raise DomainError("RUNTIME_NOT_APPROVED", "Installing executable automations requires --allow-runtime-changes AND --allow-messages")
    validate_route(provider.profile, recipient)
    if configure:
        with Session(engine) as db, db.begin():
            configure_runtime(db, profile=provider.profile, recipient=recipient, project_root=project_root, python=python,
                template_id=template_id, allow_calendar_writes=allow_calendar_writes, crowd_polling=crowd_polling)
    with Session(engine) as db:
        settings = db.get(RuntimeSettings, 1)
        if settings and not settings.enabled:
            raise DomainError("RUNTIME_PAUSED", "Runtime paused; callbacks cannot resume it")
        authority = runtime_authority(db)
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
        if not (name in {prefix + "calendar-watch", prefix + "weekly-plan", prefix + "crowd-poll"} or name.startswith(prefix + "notification:")):
            continue
        if spec is None:
            provider.remove(row["id"])
            results.append({"action": "removed", "id": row["id"], "name": name})
            continue
        if name in seen:
            raise DomainError("AUTOMATION_DUPLICATE", "Duplicate managed automation; inspect dedicated runtime before changing it")
        seen.add(name)
        payload = row.get("payload", {})
        if payload.get("kind") != "command" or payload.get("argv") != list(spec.argv) or payload.get("cwd") != spec.cwd or (payload.get("env") or {}) != dict(spec.env) or row.get("delivery", {}).get("mode") != "none":
            raise DomainError("AUTOMATION_DRIFT", "Managed automation command/route changed; explicit operator review required")
        schedule = row.get("schedule", {})
        expected_interval = {"60s": 60000, "15m": 900000}.get(spec.every)
        if (spec.every and (schedule.get("kind") != "every" or schedule.get("everyMs") != expected_interval)) or (spec.at and schedule.get("kind") != "at") or (spec.cron and (schedule.get("kind") != "cron" or schedule.get("expr") != spec.cron or schedule.get("tz") != spec.timezone)):
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
    return {"profile": provider.profile, "results": results, "delivery": "openclaw_command_callbacks", **authority}


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


def prepare_event_message(db: Session, event_id: str, *, message: str, now: datetime, recipient: str, profile: str, related_events: tuple[str, ...] = ()) -> dict:
    """Freeze proactive calendar/weekly output; one delivery per durable event."""
    validate_route(profile, recipient)
    source_event = db.get(AgentEvent, event_id)
    if source_event is None:
        raise DomainError("EVENT_NOT_FOUND", "Unknown proactive message event")
    existing = db.scalar(select(NotificationDelivery).where(NotificationDelivery.agent_event_id == event_id))
    if existing:
        if existing.recipient != recipient or existing.runtime_profile != profile:
            raise DomainError("DELIVERY_ROUTE_MISMATCH", "Event delivery belongs to another owner/runtime")
        return delivery_data(existing)
    if source_event.type == "calendar.update_briefing":
        for old in db.scalars(select(NotificationDelivery).join(AgentEvent, NotificationDelivery.agent_event_id == AgentEvent.id).where(
            AgentEvent.type == "calendar.update_briefing", NotificationDelivery.status == "PENDING",
            NotificationDelivery.runtime_profile == profile, NotificationDelivery.recipient == recipient)):
            old.status, old.handled_at = "CANCELLED", utc(now)
            acknowledge_delivery_events(db, old, utc(now))
            emit(db, "notifications.superseded", utc(now), {"delivery_id": old.id, "new_event_id": event_id})
    row = NotificationDelivery(agent_event_id=event_id, recipient=recipient, runtime_profile=profile, message=message,
        result_json={"related_events": list(related_events)}, created_at=utc(now))
    db.add(row); db.flush()
    return delivery_data(row)


def delivery_data(row: NotificationDelivery) -> dict:
    return {"id": row.id, "job_id": row.notification_job_id, "status": row.status,
        "created_at": row.created_at.isoformat(), "handled_at": row.handled_at.isoformat() if row.handled_at else None,
        "external_message_id": row.external_message_id, "event_id": row.agent_event_id}


def acknowledge_delivery_events(db: Session, row: NotificationDelivery, now: datetime):
    if row.agent_event_id:
        db.get(AgentEvent, row.agent_event_id).handled_at = now
        for event_id in row.result_json.get("related_events", []):
            event = db.get(AgentEvent, event_id)
            if event:
                event.handled_at = now


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
        settings = db.get(RuntimeSettings, 1)
        if settings and (not settings.enabled or settings.profile != provider.profile or settings.recipient != row.recipient):
            raise DomainError("RUNTIME_PAUSED", "Delivery authority paused or owner/profile mismatch")
        if row.status != "PENDING":
            return delivery_data(row)
        if row.notification_job_id:
            job = db.get(NotificationJob, row.notification_job_id)
            reason = stale_reason(db, job, now=now, delivery=row)
        else:
            reason = event_stale_reason(db, row, now=now)
        if reason:
            row.status, row.handled_at = "CANCELLED", now
            if row.agent_event_id:
                db.get(AgentEvent, row.agent_event_id).handled_at = now
            emit(db, "notifications.delivery_suppressed", now, {"delivery_id": row.id, "reason": reason})
            return delivery_data(row)
        row.status, row.handled_at = "SENDING", now
        recipient, message = row.recipient, row.message
        # The session reminder starts the workout in one tap (handled by the gymclaw-coach plugin).
        buttons = (("▶️ Start workout", f"gc:begin:{job.planned_session_id[:8]}"),) if row.notification_job_id and job.kind == "SESSION_START" else ()
    # Claim committed; no network I/O under SQLite write transaction.
    try:
        receipt = provider.send(recipient, message, buttons)
        if not isinstance(receipt, str) or not receipt:
            raise ValueError("Message receipt missing")
    except Exception:
        with Session(engine) as db, db.begin():
            db.execute(update(NotificationDelivery).where(NotificationDelivery.id == delivery_id, NotificationDelivery.status == "SENDING").values(status="UNKNOWN"))
            emit(db, "notifications.delivery_unknown", now, {"delivery_id": delivery_id, "instruction": "Check Telegram before explicitly resolving; do not auto-retry."})
        raise DomainError("TELEGRAM_DELIVERY_UNKNOWN", "Delivery outcome unknown; inspect Telegram, then runtime resolve. No automatic resend.") from None
    with Session(engine) as db, db.begin():
        db.execute(update(NotificationDelivery).where(NotificationDelivery.id == delivery_id, NotificationDelivery.status == "SENDING").values(status="SENT", external_message_id=receipt))
        row = db.get(NotificationDelivery, delivery_id)
        acknowledge_delivery_events(db, row, now)
        emit(db, "notifications.sent", now, {"delivery_id": delivery_id, "message_id": receipt})
        return delivery_data(row)


def resolve_delivery(db: Session, delivery_id: str, *, outcome: str, now: datetime) -> dict:
    """Explicit operator decision only. Must verify no sender process is running."""
    row = db.get(NotificationDelivery, delivery_id)
    if row is None:
        raise DomainError("DELIVERY_NOT_FOUND", "Unknown notification delivery")
    if row.status not in {"UNKNOWN", "SENDING"} or outcome not in {"sent", "not-sent"}:
        raise DomainError("INVALID_DELIVERY_RESOLUTION", "Resolve only ambiguous delivery with sent or not-sent")
    row.status = "SENT" if outcome == "sent" else "PENDING"
    row.handled_at = utc(now)
    if outcome == "sent":
        acknowledge_delivery_events(db, row, utc(now))
    emit(db, "notifications.delivery_resolved", utc(now), {"delivery_id": row.id, "outcome": outcome, "source": "operator_confirmation"})
    return delivery_data(row)
