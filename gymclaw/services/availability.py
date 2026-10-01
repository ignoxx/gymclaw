"""Persistent user overrides. No medical advice or remote blocker creation."""
from datetime import datetime, timedelta
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AvailabilityBlock, PlannedSession
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import Interval
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import emit, mutate, utc


def blocks(db: Session, start: datetime, end: datetime) -> tuple[Interval, ...]:
    return tuple(Interval(start=row.start_at, end=row.end_at) for row in db.scalars(select(AvailabilityBlock).where(
        AvailabilityBlock.status == "ACTIVE", AvailabilityBlock.start_at < end, AvailabilityBlock.end_at > start)))


def session_blocked(db: Session, session: PlannedSession) -> bool:
    from gymclaw.services.replanning import expanded
    prep, home = expanded(session, get_profile(db))
    return bool(blocks(db, prep, home))


def list_blocks(db: Session, *, now: datetime) -> list[dict]:
    now = utc(now)
    return [{"id": row.id, "kind": row.kind, "status": row.status, "start": row.start_at.isoformat(), "end": row.end_at.isoformat(),
        "in_effect": row.status == "ACTIVE" and row.start_at <= now < row.end_at} for row in db.scalars(select(AvailabilityBlock).order_by(AvailabilityBlock.start_at, AvailabilityBlock.id))]


def affected_weeks(db: Session, now: datetime) -> set:
    from gymclaw.services.calendar import week_of
    zone = ZoneInfo(get_profile(db).timezone)
    return {week_of(now, zone), week_of(now + timedelta(days=7), zone)} | {
        week_of(row.planned_start_at, zone) for row in db.scalars(select(PlannedSession).where(
            PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]), PlannedSession.planned_start_at > now))}


def add(db: Session, *, start: datetime, end: datetime, kind: str, now: datetime, request_id: str, cancel_locked: bool = False) -> dict:
    start, end, now = utc(start), utc(end), utc(now)
    Interval(start=start, end=end)
    if kind not in {"TRAVEL", "SICK", "UNAVAILABLE"}:
        raise ValueError("Unknown availability kind")

    def action():
        from gymclaw.services.calendar_writes import queue_session_write
        from gymclaw.services.notifications import cancel_session_jobs
        from gymclaw.services.replanning import expanded, replan_weeks
        profile = get_profile(db)
        row = AvailabilityBlock(start_at=start, end_at=end, kind=kind, created_at=now)
        db.add(row); db.flush()
        locked = []
        for plan in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]), PlannedSession.planned_start_at >= now)):
            prep, home = expanded(plan, profile)
            if prep >= end or home <= start:
                continue
            cancel_session_jobs(db, plan.id, now=now)
            if plan.user_locked:
                if not cancel_locked:
                    locked.append(plan.id)
                    continue
                # Explicit later user cancellation supersedes earlier calendar lock.
                plan.user_locked = False
                plan.status = "CANCELLED"
                plan.source_revision += 1
                plan.workout_plan_json |= {"deleted_by_user": True, "manual_lock_cancelled_by_request": request_id}
                queue_session_write(db, plan, now=now)
                emit(db, "availability.locked_session_cancelled", now, {"session_id": plan.id, "block_id": row.id, "request_id": request_id})
        repaired = replan_weeks(db, affected_weeks(db, now), now=now)
        return {"block_id": row.id, "kind": kind, "start": start.isoformat(), "end": end.isoformat(), "locked_conflicts": locked,
            "replanning": repaired, "remote_events_changed": False,
            "instruction": "Pause saved. Reminders suppressed; manually locked calendar slots need explicit cancellation." if locked else "Unavailability saved. Affected plan repaired; normal planning resumes after end time."}

    return mutate(db, "availability.added", request_id, {"start": start.isoformat(), "end": end.isoformat(), "kind": kind, "cancel_locked": cancel_locked}, now, action)


def retract(db: Session, block_id: str, *, now: datetime, request_id: str) -> dict:
    now = utc(now)
    def action():
        from gymclaw.services.replanning import replan_weeks, commit_upcoming
        row = db.get(AvailabilityBlock, block_id)
        if row is None:
            raise DomainError("AVAILABILITY_NOT_FOUND", "Unknown availability block")
        if now < row.created_at:
            raise DomainError("TIME_REVERSED", "Retraction precedes availability creation")
        row.status, row.retracted_at = "RETRACTED", now
        db.flush()
        repaired = replan_weeks(db, affected_weeks(db, now), now=now)
        commit_upcoming(db, now=now)
        return {"block_id": row.id, "status": row.status, "replanning": repaired, "remote_events_changed": False, "instruction": "Availability override removed. Valid future planning resumed."}
    return mutate(db, "availability.retracted", request_id, {"block_id": block_id}, now, action)
