"""Owner edits to single sessions from chat: move, add, cancel, change workout.

Works like a manual calendar edit: the owner's choice is pinned (`user_locked`) and wins over
the planner, even outside the profile window. Conflicts come back as warnings, never refusals.
Unlocked sessions are replanned around it.
"""
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

from sqlalchemy.orm import Session

from gymclaw.models import PlannedSession
from gymclaw.services.calendar import allocation, week_of
from gymclaw.services.calendar_writes import queue_session_write
from gymclaw.services.errors import DomainError
from gymclaw.services.notifications import cancel_session_jobs, schedule_session_jobs
from gymclaw.services.planning import Interval
from gymclaw.services.profile import get_profile
from gymclaw.services.replanning import best_slot, commit_upcoming, replan_weeks
from gymclaw.services.scheduling import session_data
from gymclaw.services.templates import get_template
from gymclaw.services.workout import mutate, utc


def editable(db: Session, session_id: str) -> PlannedSession:
    session = db.get(PlannedSession, session_id)
    if session is None:
        raise DomainError("SESSION_NOT_FOUND", f"Unknown planned session: {session_id}")
    if session.status not in {"TENTATIVE", "COMMITTED"}:
        raise DomainError("SESSION_NOT_EDITABLE", f"Session is {session.status}; only upcoming sessions can change")
    return session


def place(db: Session, session: PlannedSession, start: datetime, end: datetime, *, now: datetime):
    """Set the owner's slot and derived prep/leave times. Old crowd forecasts don't apply to it."""
    Interval(start=start, end=end)
    if start <= now:
        raise DomainError("SESSION_IN_PAST", "New session time must be in the future")
    profile = get_profile(db)
    session.planned_start_at, session.planned_end_at, session.expected_finish_at = start, end, end
    session.prep_start_at = start - timedelta(minutes=profile.prep_minutes + profile.commute_to_gym_minutes)
    session.leave_home_at = start - timedelta(minutes=profile.commute_to_gym_minutes)
    session.week_id = week_of(start, ZoneInfo(profile.timezone)).isoformat()
    session.crowd_prediction, session.crowd_confidence = None, 0
    session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if not k.startswith("crowd_")}


def pin(db: Session, session: PlannedSession, *, now: datetime):
    """Lock the owner's choice, refit the workout to the slot, reschedule reminders, queue the calendar write."""
    session.user_locked = True
    session.status = "COMMITTED"
    session.source_revision += 1
    db.flush()
    allocation(db, session)
    cancel_session_jobs(db, session.id, now=now)
    schedule_session_jobs(db, session, now=now)
    queue_session_write(db, session, now=now, owner_edit=True)


def settle(db: Session, session: PlannedSession, weeks: set, *, now: datetime, done: str) -> dict:
    repaired = replan_weeks(db, weeks, now=now)
    commit_upcoming(db, now=now)
    warnings = " ".join(repaired["warnings"])
    return {"session": session_data(session), "replanning": repaired, "remote_events_changed": False,
        "instruction": f"{done} Heads-up: {warnings}" if warnings else done}


def move(db: Session, session_id: str, *, start: datetime | None = None, end: datetime | None = None, day: date | None = None, now: datetime, request_id: str) -> dict:
    """Pin a session to an exact `start` (`end` defaults to keeping the duration). Without `start`,
    the planner picks the quietest valid slot, on `day` if given."""
    start, now = start and utc(start), utc(now)

    def action():
        session = editable(db, session_id)
        if start is None:
            return best_slot(db, session, now=now, day=day) | {"instruction": "Session moved to the quietest valid slot."}
        zone = ZoneInfo(get_profile(db).timezone)
        old_week = week_of(session.planned_start_at, zone)
        place(db, session, start, utc(end) if end else start + (session.planned_end_at - session.planned_start_at), now=now)
        pin(db, session, now=now)
        return settle(db, session, {old_week, week_of(start, zone)}, now=now, done="Session moved and pinned.")

    intent = {"session_id": session_id, "start": start and start.isoformat(), "end": end and utc(end).isoformat(), "day": day and day.isoformat()}
    return mutate(db, "session.moved", request_id, intent, now, action)


def add(db: Session, *, template_id: str, start: datetime, end: datetime | None, now: datetime, request_id: str) -> dict:
    """Add an extra pinned session. `end` defaults to the preferred workout length."""
    start, now = utc(start), utc(now)

    def action():
        get_template(db, template_id)
        session = PlannedSession(workout_template_id=template_id, workout_plan_json={}, source_revision=0)
        place(db, session, start, utc(end) if end else start + timedelta(minutes=get_profile(db).preferred_workout_minutes), now=now)
        db.add(session)
        pin(db, session, now=now)
        return settle(db, session, {week_of(start, ZoneInfo(get_profile(db).timezone))}, now=now, done="Session added and pinned.")

    return mutate(db, "session.added", request_id, {"template_id": template_id, "start": start.isoformat(), "end": end and utc(end).isoformat()}, now, action)


def cancel(db: Session, session_id: str, *, now: datetime, request_id: str) -> dict:
    """Cancel like a calendar delete: the slot stays free and a replacement is planned if one fits."""
    now = utc(now)

    def action():
        session = editable(db, session_id)
        # Unlock so the calendar delete goes through; deleted_by_user keeps the slot from being reused.
        session.user_locked = False
        session.status = "CANCELLED"
        session.source_revision += 1
        session.workout_plan_json = session.workout_plan_json | {"deleted_by_user": True}
        cancel_session_jobs(db, session.id, now=now)
        queue_session_write(db, session, now=now)
        week = week_of(session.planned_start_at, ZoneInfo(get_profile(db).timezone))
        return settle(db, session, {week}, now=now, done="Session cancelled. Check replanning for a replacement.")

    return mutate(db, "session.cancelled", request_id, {"session_id": session_id}, now, action)


def set_template(db: Session, session_id: str, *, template_id: str, now: datetime, request_id: str) -> dict:
    """Change which workout a session runs. Pins the session so rotation doesn't change it back."""
    now = utc(now)

    def action():
        session = editable(db, session_id)
        get_template(db, template_id)
        session.workout_template_id = template_id
        # Drop the old template snapshot so allocation uses the new template.
        session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if k.startswith("crowd_")}
        pin(db, session, now=now)
        return settle(db, session, set(), now=now, done="Workout changed.")

    return mutate(db, "session.template_changed", request_id, {"session_id": session_id, "template_id": template_id}, now, action)
