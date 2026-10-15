"""Durable agent event inbox; acknowledgement never deletes history."""
from datetime import datetime
from hashlib import sha256
import json

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent
from gymclaw.services.errors import DomainError
from gymclaw.services.workout import utc


# Events that need the agent. Everything else is an audit record the code already handled
# (state changes, timers, sends, calendar writes) and would only flood the heartbeat context.
ACTIONABLE = {"notifications.delivery_unknown", "calendar.write_blocked", "crowd.source_unavailable",
    "planning.attention_required", "planning.replan_required", "planning.no_show", "workout.missed",
    "planning.weekly_briefing", "planning.week_started", "workout.stale", "coach.fallback",
    "calendar.personal_blockers_changed"}


def pending(db: Session, *, include_all: bool = False) -> list[dict]:
    query = select(AgentEvent).where(AgentEvent.handled_at.is_(None))
    if not include_all:
        query = query.where(AgentEvent.type.in_(ACTIONABLE))
    return [{"id": e.id, "type": e.type, "created_at": e.created_at.isoformat(), "payload": e.payload_json, "correlation_id": e.correlation_id} for e in db.scalars(query.order_by(AgentEvent.created_at, AgentEvent.id))]


def ack(db: Session, event_id: str, *, now: datetime) -> dict:
    now = utc(now)
    event = db.get(AgentEvent, event_id)
    if event is None:
        raise DomainError("EVENT_NOT_FOUND", "Unknown event")
    if now < event.created_at:
        raise DomainError("TIME_REVERSED", "Acknowledgement precedes event")
    if event.handled_at is None:
        event.handled_at = now
    return {"id": event.id, "handled_at": event.handled_at.isoformat()}


def outage_open(db: Session, lost: str, restored: str) -> bool:
    """Whether the latest of a lost/restored event pair is `lost`. Owner alerts dedupe on this, not on
    `handled_at`: delivering the alert marks its event handled while the outage is still on."""
    latest = db.scalar(select(AgentEvent.type).where(AgentEvent.type.in_([lost, restored])).order_by(AgentEvent.created_at.desc(), AgentEvent.id.desc()).limit(1))
    return latest == lost


def attention(db: Session, kind: str, payload: dict, *, now: datetime):
    """Emit once per distinct situation, including after acknowledgement; quiet ticks stay cheap."""
    from gymclaw.services.workout import emit
    key = "attention:" + sha256(json.dumps([kind, payload], sort_keys=True).encode()).hexdigest()
    previous = db.scalar(select(AgentEvent).where(AgentEvent.correlation_id == key))
    if previous:
        return previous
    event = emit(db, kind, now, payload)
    event.correlation_id = key
    return event
