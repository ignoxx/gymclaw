"""Calendar provider contract and minimal normalized Google event payloads."""
from datetime import date, datetime, time, timezone
from typing import Protocol
from zoneinfo import ZoneInfo

from pydantic import AwareDatetime, BaseModel, ConfigDict, model_validator

from gymclaw.services.errors import DomainError


class CalendarEvent(BaseModel):
    model_config = ConfigDict(extra="forbid")
    id: str
    etag: str | None = None
    title: str = ""
    status: str = "confirmed"
    start: AwareDatetime | None = None
    end: AwareDatetime | None = None
    updated: AwareDatetime | None = None
    ical_uid: str | None = None
    managed: bool = False
    session_id: str | None = None
    revision: int | None = None
    raw: dict = {}

    @model_validator(mode="after")
    def valid(self):
        if self.status != "cancelled":
            if self.start is None or self.end is None or self.end <= self.start:
                raise ValueError("Active calendar event requires ordered start/end")
        return self


def parse_time(value: dict, calendar_timezone: str) -> datetime:
    zone = ZoneInfo(value.get("timeZone", calendar_timezone))
    if "dateTime" in value:
        stamp = datetime.fromisoformat(value["dateTime"])
        if stamp.tzinfo is None:
            stamp = stamp.replace(tzinfo=zone)
        return stamp.astimezone(timezone.utc)
    if "date" in value:
        return datetime.combine(date.fromisoformat(value["date"]), time(), zone).astimezone(timezone.utc)
    raise ValueError("Calendar event time must contain dateTime or date")


def parse_event(raw: dict, calendar_timezone: str) -> CalendarEvent:
    private = raw.get("extendedProperties", {}).get("private", {})
    revision = private.get("gymclawPlanRevision")
    if revision is not None:
        try:
            revision = int(revision)
        except (ValueError, TypeError):
            revision = None
    keep = {k: raw[k] for k in ("id", "etag", "summary", "start", "end", "status", "updated", "iCalUID", "transparency", "recurrence", "recurringEventId", "originalStartTime") if k in raw}
    if private:
        keep["extendedProperties"] = {"private": {k: v for k, v in private.items() if k in {"gymclawManaged", "gymclawSessionId", "gymclawPlanRevision"}}}
    cancelled = raw.get("status") == "cancelled"
    return CalendarEvent(id=raw["id"], etag=raw.get("etag"), title=raw.get("summary", ""), status=raw.get("status", "confirmed"), start=parse_time(raw["start"], calendar_timezone) if "start" in raw and not cancelled else None, end=parse_time(raw["end"], calendar_timezone) if "end" in raw and not cancelled else None, updated=datetime.fromisoformat(raw["updated"]) if raw.get("updated") else None, ical_uid=raw.get("iCalUID"), managed=private.get("gymclawManaged") == "true", session_id=private.get("gymclawSessionId"), revision=revision, raw=keep)


class SyncBatch(BaseModel):
    events: tuple[CalendarEvent, ...]
    next_sync_token: str
    timezone: str
    full: bool


class SyncTokenExpired(DomainError):
    def __init__(self):
        super().__init__("CALENDAR_SYNC_TOKEN_EXPIRED", "Calendar requires fresh synchronization")


class CalendarConflict(DomainError):
    def __init__(self):
        super().__init__("CALENDAR_WRITE_CONFLICT", "Calendar event changed remotely; sync before retry")


class CalendarProvider(Protocol):
    calendar_id: str
    source: str

    def incremental_sync(self, sync_token: str | None) -> SyncBatch: ...
    def get_event(self, event_id: str) -> CalendarEvent | None: ...
    def create_gym_event(self, body: dict) -> CalendarEvent: ...
    def update_gym_event(self, event_id: str, body: dict, *, etag: str) -> CalendarEvent: ...
    def delete_gym_event(self, event_id: str, *, etag: str, session_id: str) -> None: ...
