"""Agent-selected session edits; derived calendar, allocation and reminder state stays in services."""
from datetime import datetime, timedelta
from typing import Literal
from zoneinfo import ZoneInfo

from pydantic import BaseModel, ConfigDict
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import PlannedSession
from gymclaw.services.calendar import week_of
from gymclaw.services.calendar_writes import queue_session_write
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import Candidate
from gymclaw.services.profile import get_profile
from gymclaw.services.replanning import ACTIVE, place, retire_session, rule_breaks
from gymclaw.services.scheduling import session_data
from gymclaw.services.templates import get_template
from gymclaw.services.workout import mutate, utc


class SessionEdit(BaseModel):
    model_config = ConfigDict(extra="forbid")
    start: datetime | None = None
    end: datetime | None = None
    template_id: str | None = None
    status: Literal["TENTATIVE", "COMMITTED", "CANCELLED", "SKIPPED", "MISSED"] | None = None


def edit_session(db: Session, operation: str, *, changes: SessionEdit, session_id: str | None = None,
                 now: datetime, request_id: str) -> dict:
    """One idempotent mutation. Only input integrity and actual workout history are hard boundaries."""
    now = utc(now)
    if operation not in {"create", "update", "cancel"}:
        raise ValueError("Unknown session operation")
    if operation != "create" and not session_id:
        raise ValueError("--session-id is required")
    if operation == "cancel" and changes.model_fields_set:
        raise ValueError("cancel accepts no --data; use update to combine changes")

    def action():
        profile = get_profile(db)
        zone = ZoneInfo(profile.timezone)
        fields = changes.model_fields_set
        if any(getattr(changes, field) is None for field in fields):
            raise ValueError("Session fields cannot be null")
        creating = operation == "create"
        if creating:
            if session_id or changes.start is None or changes.template_id is None:
                raise ValueError("create requires start and template_id, without --session-id")
            session = PlannedSession(workout_template_id=changes.template_id, user_locked=True, workout_plan_json={})
        else:
            session = db.get(PlannedSession, session_id)
            if session is None:
                raise DomainError("SESSION_NOT_FOUND", "Unknown session; read calendar get-week or sql")
            if session.status in {"STARTED", "COMPLETED"}:
                raise DomainError("SESSION_HAS_WORKOUT", "Edit the actual workout through workout tools")
        if changes.template_id:
            get_template(db, changes.template_id)
            if changes.template_id != session.workout_template_id:
                session.workout_template_id = changes.template_id
                session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if (k.startswith("crowd_") or k == "agent_status")}
        def local(value):
            return utc(value.replace(tzinfo=zone) if value.tzinfo is None else value)
        start = local(changes.start) if changes.start else session.planned_start_at
        duration = timedelta(minutes=profile.preferred_workout_minutes) if creating else session.planned_end_at - session.planned_start_at
        end = local(changes.end) if changes.end else start + duration
        status = "CANCELLED" if operation == "cancel" else changes.status or ("COMMITTED" if creating else session.status)
        if end <= start or status in ACTIVE and end <= now:
            raise ValueError("Session end must follow start; active sessions must end in the future")
        if status == "MISSED" and start > now:
            # This is an explicit agent/owner decision, never invented training history.
            warning = ["marked missed before its start"]
        else:
            warning = []
        slot = Candidate(start=start, end=end, prep_start=start - timedelta(minutes=profile.prep_minutes + profile.commute_to_gym_minutes),
            leave_home=start - timedelta(minutes=profile.commute_to_gym_minutes), home_at=end + timedelta(minutes=profile.commute_home_minutes),
            crowd=None, confidence=0, score=0, score_parts={})
        warnings = warning + rule_breaks(db, session, slot, profile)
        if end - start < timedelta(minutes=profile.minimum_workout_minutes):
            warnings.append("shorter than your minimum workout duration")
        week = week_of(start, zone)
        count = sum(s.id != session.id and s.status in ACTIVE and week_of(s.planned_start_at, zone) == week
                    for s in db.scalars(select(PlannedSession)))
        if status in ACTIVE and count >= profile.weekly_max_sessions:
            warnings.append("exceeds your weekly maximum")
        if creating:
            db.add(session)
        else:
            from gymclaw.services.notifications import cancel_session_jobs
            cancel_session_jobs(db, session.id, now=now)
            session.source_revision += 1
        session.user_locked = True
        place(db, session, slot, week=week, now=now, crowd_model=None, status=status)
        if changes.status is not None:
            session.workout_plan_json = session.workout_plan_json | {"agent_status": status}
        if session.workout_plan_json.get("error"):
            warnings.append("too short for required movements; extend the slot or adjust the template")
        if status in {"CANCELLED", "SKIPPED", "MISSED"}:
            retire_session(db, session, status, now=now)
        else:
            session.status = status
        # Retiring cancels pending patches; cancellation also queues a calendar deletion.
        queue_session_write(db, session, now=now)
        return {"session": session_data(session), "warnings": warnings, "side_effects": []}

    return mutate(db, "planning.session_edited", request_id,
        {"operation": operation, "session_id": session_id, "changes": changes.model_dump(mode="json", exclude_unset=True)}, now, action)
