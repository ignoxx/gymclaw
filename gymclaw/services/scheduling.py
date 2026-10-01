"""Transactional planning against persisted profile/calendar state."""
from datetime import date, datetime, timedelta, timezone
from zoneinfo import ZoneInfo
from math import ceil

from pydantic import BaseModel, ConfigDict
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, PlannedSession
from gymclaw.services.planning import Interval, SlotSignal, PlannerConfig, plan_week
from gymclaw.services.profile import get_profile
from gymclaw.services.calendar_busy import busy_intervals
from gymclaw.services.compression import compress_template
from gymclaw.services.templates import get_template
from gymclaw.services.workout import historical_set_duration


class PlanningFixture(BaseModel):
    """Explicit demo inputs, never represented as live provider observations."""
    model_config = ConfigDict(extra="forbid")
    busy: tuple[Interval, ...] = ()
    unavailable: tuple[Interval, ...] = ()
    signals: tuple[SlotSignal, ...] = ()
    config: PlannerConfig = PlannerConfig()


def session_data(row: PlannedSession) -> dict:
    return {
        "id": row.id,
        "week_id": row.week_id,
        "status": row.status,
        "start": row.planned_start_at.isoformat(),
        "end": row.planned_end_at.isoformat(),
        "prep_start": row.prep_start_at.isoformat(),
        "leave_home": row.leave_home_at.isoformat(),
        "expected_finish": row.expected_finish_at.isoformat(),
        "crowd": row.crowd_prediction,
        "confidence": row.crowd_confidence,
        "user_locked": row.user_locked,
        "workout_template_id": row.workout_template_id,
        "workout_plan": row.workout_plan_json,
    }


def schedule_week(db: Session, week_start: date, *, now: datetime, fixture: PlanningFixture = PlanningFixture(), request_id: str | None = None, template_id: str | None = None) -> dict:
    request = {"week_start": week_start.isoformat(), "fixture": fixture.model_dump(mode="json")}
    if template_id:
        request["template_id"] = template_id
    if request_id:
        previous = db.scalar(select(AgentEvent).where(AgentEvent.correlation_id == request_id))
        if previous:
            if previous.payload_json.get("request") != request:
                raise ValueError("Request ID already used with different planning input")
            return previous.payload_json["result"]
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    active = tuple(db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED", "STARTED", "COMPLETED"]))))
    existing = tuple(Interval(start=s.planned_start_at, end=s.planned_end_at) for s in active)
    start = datetime.combine(week_start - timedelta(days=1), datetime.min.time(), zone)
    end = start + timedelta(days=9)
    busy = fixture.busy + busy_intervals(db, start, end)
    deleted = tuple(Interval(start=s.planned_start_at, end=s.planned_end_at) for s in db.scalars(select(PlannedSession).where(PlannedSession.status == "CANCELLED")) if s.workout_plan_json.get("deleted_by_user"))
    plan = plan_week(profile, week_start, busy=busy, unavailable=fixture.unavailable + deleted, existing=existing, signals=fixture.signals, now=now, config=fixture.config)
    template = get_template(db, template_id) if template_id else None
    if template:
        template = template.model_copy(update={"set_duration_seconds": ceil(historical_set_duration(db, template.set_duration_seconds))})
    created = []
    for slot in plan.sessions:
        row = PlannedSession(week_id=week_start.isoformat(), workout_template_id=template_id, workout_plan_json=compress_template(template, profile, int((slot.end - slot.start).total_seconds())) if template else {}, planned_start_at=slot.start, planned_end_at=slot.end, prep_start_at=slot.prep_start, leave_home_at=slot.leave_home, expected_finish_at=slot.end, crowd_prediction=slot.crowd, crowd_confidence=slot.confidence)
        if slot.crowd is not None and fixture.signals:
            row.workout_plan_json = row.workout_plan_json | {"crowd_source": "demo_fixture"}
        db.add(row)
        db.flush()
        created.append(row)
    week_rows = [s for s in active if s.planned_start_at.astimezone(zone).date().isocalendar()[:2] == week_start.isocalendar()[:2]] + created
    result = {
        "week_start": week_start.isoformat(),
        "sessions": [session_data(s) for s in sorted(week_rows, key=lambda s: s.planned_start_at)],
        "created_count": len(created),
        "minimum_met": plan.minimum_met,
        "shortfall": plan.shortfall,
        "source": "fixture" if fixture != PlanningFixture() else "local_state",
        "calendar_events_created": False,
        "choices": [s.model_dump(mode="json") for s in plan.sessions],
    }
    event = AgentEvent(type="planning.week_planned", created_at=now.astimezone(timezone.utc), correlation_id=request_id, payload_json={"request": request, "result": result})
    db.add(event)
    db.flush()
    return result
