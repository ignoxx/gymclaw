"""Explicit in-memory demo calendar, never used as evidence of live Google behavior."""
from copy import deepcopy
from hashlib import sha256
import json

from pydantic import BaseModel, ConfigDict

from gymclaw.providers.calendar import CalendarConflict, CalendarEvent, SyncBatch, SyncTokenExpired, parse_event, parse_time
from gymclaw.services.errors import DomainError


class CalendarFixture(BaseModel):
    model_config = ConfigDict(extra="forbid")
    calendar_id: str = "demo-calendar"
    timezone: str = "Europe/Berlin"
    events: list[dict] = []
    full: bool = True


class FixtureCalendarProvider:
    source = "fixture"

    def __init__(self, calendar_id="demo-calendar", *, timezone="Europe/Berlin"):
        self.calendar_id = calendar_id
        self.timezone = timezone
        self.events: dict[str, dict] = {}
        self.changes: list[dict] = []
        self.calls: list[tuple[str, str]] = []
        self.fail_sync = False
        self.expire_token = False
        self.batch_override: SyncBatch | None = None

    @classmethod
    def from_fixture(cls, fixture: CalendarFixture):
        provider = cls(fixture.calendar_id, timezone=fixture.timezone)
        provider.events = {e["id"]: deepcopy(e) for e in fixture.events}
        provider.batch_override = SyncBatch(events=tuple(parse_event(e, fixture.timezone) for e in fixture.events), next_sync_token="fixture:" + sha256(json.dumps(fixture.model_dump(), sort_keys=True).encode()).hexdigest(), timezone=fixture.timezone, full=fixture.full)
        return provider

    def put(self, raw: dict):
        body = deepcopy(raw) | {"etag": f'"fixture-{len(self.changes) + 1}"'}
        self.events[body["id"]] = body
        self.changes.append(body)
        return parse_event(body, self.timezone)

    def edit(self, event_id: str, **changes):
        return self.put(self.events[event_id] | changes)

    def cancel(self, event_id: str):
        return self.put({"id": event_id, "status": "cancelled"})

    def incremental_sync(self, sync_token: str | None) -> SyncBatch:
        if self.fail_sync:
            raise DomainError("CALENDAR_UNAVAILABLE", "Fixture sync intentionally unavailable")
        if self.expire_token and sync_token:
            self.expire_token = False
            raise SyncTokenExpired()
        if self.batch_override:
            return self.batch_override
        previous = int(sync_token.split(":")[1]) if sync_token else 0
        raws = list(self.events.values()) if sync_token is None else self.changes[previous:]
        return SyncBatch(events=tuple(parse_event(e, self.timezone) for e in raws), next_sync_token=f"fixture:{len(self.changes)}", timezone=self.timezone, full=sync_token is None)

    def get_event(self, event_id: str) -> CalendarEvent | None:
        raw = self.events.get(event_id)
        return parse_event(raw, self.timezone) if raw else None

    def create_gym_event(self, body: dict) -> CalendarEvent:
        self.calls.append(("CREATE", body["id"]))
        existing = self.get_event(body["id"])
        if existing:
            if existing.status == "cancelled":
                raise DomainError("CALENDAR_EVENT_DELETED", "Fixture event deleted")
            session_id = body["extendedProperties"]["private"]["gymclawSessionId"]
            if not existing.managed or existing.session_id != session_id:
                raise DomainError("CALENDAR_NOT_OWNED", "Fixture ownership mismatch")
            return existing
        return self.put(body)

    def update_gym_event(self, event_id: str, body: dict, *, etag: str) -> CalendarEvent:
        self.calls.append(("UPDATE", event_id))
        event = self.get_event(event_id)
        if event is None or event.status == "cancelled":
            raise DomainError("CALENDAR_EVENT_DELETED", "Fixture event deleted")
        if not event.managed or event.session_id != body["extendedProperties"]["private"]["gymclawSessionId"]:
            raise DomainError("CALENDAR_NOT_OWNED", "Fixture ownership mismatch")
        if event.etag != etag:
            revision = body["extendedProperties"]["private"]["gymclawPlanRevision"]
            if str(event.revision) == revision and all(key not in body or getattr(event, key) == parse_time(body[key], self.timezone) for key in ("start", "end")):
                return event
            raise CalendarConflict()
        return self.put(self.events[event_id] | body)

    def delete_gym_event(self, event_id: str, *, etag: str, session_id: str) -> None:
        self.calls.append(("DELETE", event_id))
        event = self.get_event(event_id)
        if event is None or event.status == "cancelled":
            return
        if not event.managed or event.session_id != session_id:
            raise DomainError("CALENDAR_NOT_OWNED", "Fixture ownership mismatch")
        if event.etag != etag:
            raise CalendarConflict()
        self.cancel(event_id)
