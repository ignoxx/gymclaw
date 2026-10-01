"""Transactional planning against persisted profile/calendar state."""
from datetime import date, datetime, timezone
from zoneinfo import ZoneInfo

from pydantic import BaseModel, ConfigDict
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, CalendarEventSnapshot, PlannedSession
from gymclaw.services.planning import Interval, SlotSignal, PlannerConfig, plan_week
from gymclaw.services.profile import get_profile


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
    }


def schedule_week(db: Session, week_start: date, *, now: datetime, fixture: PlanningFixture = PlanningFixture(), request_id: str | None = None) -> dict:
    if request_id:
        previous = db.scalar(select(AgentEvent).where(AgentEvent.correlation_id == request_id))
        if previous:
            if previous.payload_json["request"] != {"week_start": week_start.isoformat(), "fixture": fixture.model_dump(mode="json")}:
                raise ValueError("Request ID already used with different planning input")
            return previous.payload_json["result"]
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    active = tuple(db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED", "STARTED", "COMPLETED"]))))
    existing = tuple(Interval(start=s.planned_start_at, end=s.planned_end_at) for s in active)
    calendar = tuple(db.scalars(select(CalendarEventSnapshot).where(CalendarEventSnapshot.status != "cancelled", CalendarEventSnapshot.gymclaw_managed.is_(False))))
    busy = fixture.busy + tuple(Interval(start=e.start_at, end=e.end_at) for e in calendar)
    plan = plan_week(profile, week_start, busy=busy, unavailable=fixture.unavailable, existing=existing, signals=fixture.signals, now=now, config=fixture.config)
    created = []
    for slot in plan.sessions:
        row = PlannedSession(week_id=week_start.isoformat(), planned_start_at=slot.start, planned_end_at=slot.end, prep_start_at=slot.prep_start, leave_home_at=slot.leave_home, expected_finish_at=slot.end, crowd_prediction=slot.crowd, crowd_confidence=slot.confidence)
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
    event = AgentEvent(type="planning.week_planned", created_at=now.astimezone(timezone.utc), correlation_id=request_id, payload_json={"request": {"week_start": week_start.isoformat(), "fixture": fixture.model_dump(mode="json")}, "result": result})
    db.add(event)
    db.flush()
    return result
