"""Durable agent event inbox; acknowledgement never deletes history."""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent
from gymclaw.services.errors import DomainError
from gymclaw.services.workout import utc


# Events that need the agent. Everything else is an audit record the code already handled
# (state changes, timers, sends, calendar writes) and would only flood the heartbeat context.
ACTIONABLE = {"notifications.delivery_unknown", "calendar.write_blocked", "crowd.source_unavailable"}


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
