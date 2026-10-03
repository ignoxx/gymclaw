"""Personal calendar as read-only blockers. Fetched by the watcher, never written.

Connect stores the feed URL locally; each refresh replaces the expanded busy intervals
and replans only the weeks whose blockers changed. A failed fetch keeps the last known
blockers so an outage never frees up time that is actually busy.
"""
from datetime import datetime, timedelta
from urllib.parse import urlsplit
from zoneinfo import ZoneInfo

from sqlalchemy import delete, select
from sqlalchemy.orm import Session

from gymclaw.models import PersonalBusyInterval, PersonalCalendarState
from gymclaw.providers import ics_feed
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import Interval
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import emit, utc

REFRESH_EVERY = timedelta(minutes=5)
HORIZON = timedelta(days=35)
LOOKBACK = timedelta(days=1)


def blocks(db: Session, start: datetime, end: datetime) -> tuple[Interval, ...]:
    return tuple(Interval(start=row.start_at, end=row.end_at) for row in db.scalars(select(PersonalBusyInterval).where(
        PersonalBusyInterval.start_at < end, PersonalBusyInterval.end_at > start)))


def status(db: Session) -> dict:
    state = db.get(PersonalCalendarState, 1)
    if state is None:
        return {"connected": False}
    return {"connected": True, "host": urlsplit(state.url).hostname, "last_success_at": state.last_success_at and state.last_success_at.isoformat(),
        "last_error_code": state.last_error_code, "blockers": len(_stored(db))}


def connect(db: Session, url: str, *, now: datetime, transport=None) -> dict:
    url = ics_feed.normalize_url(url)
    state = db.get(PersonalCalendarState, 1) or PersonalCalendarState(id=1, url=url)
    state.url, state.fetched_at, state.last_error_code = url, None, None
    db.add(state)
    db.flush()
    return refresh(db, now=now, force=True, transport=transport, strict=True)


def disconnect(db: Session, *, now: datetime) -> dict:
    old = _stored(db)
    db.execute(delete(PersonalBusyInterval))
    state = db.get(PersonalCalendarState, 1)
    if state:
        db.delete(state)
    db.flush()
    return _replan(db, old, set(), now=utc(now)) | {"connected": False}


def refresh(db: Session, *, now: datetime, force: bool = False, transport=None, strict: bool = False) -> dict:
    """Throttled; no-op when not connected or fetched recently. Errors are recorded, not raised, unless strict."""
    now = utc(now)
    state = db.get(PersonalCalendarState, 1)
    if state is None or not force and state.fetched_at and now - state.fetched_at < REFRESH_EVERY:
        return {"refreshed": False, "changes": [], "warnings": [], "events": [], "user_message_hint": None}
    state.fetched_at = now
    try:
        data = ics_feed.fetch(state.url, transport)
        fresh = {(i.start, i.end) for i in ics_feed.busy_intervals(data, now - LOOKBACK, now + HORIZON, ZoneInfo(get_profile(db).timezone))}
    except DomainError as error:
        state.last_error_code = error.code
        if strict:
            raise
        return {"refreshed": False, "error_code": error.code, "changes": [], "warnings": [], "events": [], "user_message_hint": None}
    state.last_success_at, state.last_error_code = now, None
    old = _stored(db)
    if fresh != old:
        db.execute(delete(PersonalBusyInterval))
        db.add_all(PersonalBusyInterval(start_at=s, end_at=e) for s, e in sorted(fresh))
        db.flush()
    return _replan(db, old, fresh, now=now) | {"refreshed": True, "blockers": len(fresh)}


def _stored(db: Session) -> set[tuple[datetime, datetime]]:
    return {(row.start_at, row.end_at) for row in db.scalars(select(PersonalBusyInterval))}


def _replan(db: Session, old: set, new: set, *, now: datetime) -> dict:
    from gymclaw.services.calendar import week_of
    from gymclaw.services.replanning import commit_upcoming, replan_weeks
    zone = ZoneInfo(get_profile(db).timezone)
    # Only future changes matter; a blocker sliding out of the lookback window is not news.
    changed = {(s, e) for s, e in old ^ new if e > now}
    if not changed:
        return {"changes": [], "warnings": [], "events": [], "user_message_hint": None}
    weeks = set()
    for s, e in changed:
        week, last = week_of(max(s, now), zone), week_of(min(e, now + HORIZON), zone)
        while week <= last:
            weeks.add(week)
            week += timedelta(days=7)
    replanned = replan_weeks(db, weeks, now=now)
    commit_upcoming(db, now=now)
    event = emit(db, "calendar.personal_blockers_changed", now, {"added": len(new - old), "removed": len(old - new), "changes": replanned["changed"], "warnings": replanned["warnings"]})
    event.handled_at = now  # Replan already ran; agent only communicates.
    hint = None
    if replanned["warnings"]:
        hint = "Personal calendar changed. " + " ".join(replanned["warnings"])
    elif replanned["changed"]:
        hint = "Personal calendar changed. Affected gym sessions replanned; preparation/leave times updated."
    return {"changes": replanned["changed"], "warnings": replanned["warnings"], "events": [{"id": event.id, "type": event.type}], "user_message_hint": hint}
