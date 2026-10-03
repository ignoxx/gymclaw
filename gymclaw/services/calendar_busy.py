"""Expand busy recurring masters/exceptions locally within a bounded horizon."""
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

from dateutil.rrule import rrulestr
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import CalendarEventSnapshot, CalendarSyncState, PlannedSession
from gymclaw.providers.calendar import parse_time
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import Interval, overlaps


def busy_intervals(db: Session, start: datetime, end: datetime) -> tuple[Interval, ...]:
    state = db.get(CalendarSyncState, 1)
    calendar_zone = state.timezone if state else "Europe/Berlin"
    snapshots = list(db.scalars(select(CalendarEventSnapshot)))
    own_ids = {s.calendar_event_id for s in db.scalars(select(PlannedSession)) if s.calendar_event_id}
    excluded = {(e.raw_json.get("recurringEventId"), parse_time(e.raw_json["originalStartTime"], calendar_zone)) for e in snapshots if e.raw_json.get("recurringEventId") and e.raw_json.get("originalStartTime")}
    from gymclaw.services import availability, personal_calendar
    result = [*availability.blocks(db, start, end), *personal_calendar.blocks(db, start, end)]
    for event in snapshots:
        raw = event.raw_json
        if event.status == "cancelled" or event.calendar_event_id in own_ids or raw.get("transparency") == "transparent":
            continue
        if not raw.get("recurrence"):
            if overlaps(start, end, Interval(start=event.start_at, end=event.end_at)):
                result.append(Interval(start=event.start_at, end=event.end_at))
            continue
        zone = ZoneInfo(raw.get("start", {}).get("timeZone", calendar_zone))
        all_day = "date" in raw.get("start", {})
        local_start = event.start_at.astimezone(zone)
        duration = event.end_at - event.start_at
        if all_day:
            day_count = (event.end_at.astimezone(zone).date() - local_start.date()).days
            dtstart = local_start.replace(tzinfo=None)
            lower = (start - timedelta(days=day_count + 1)).astimezone(zone).replace(tzinfo=None)
            upper = end.astimezone(zone).replace(tzinfo=None)
        else:
            dtstart = local_start
            lower = (start - duration - timedelta(days=1)).astimezone(zone)
            upper = end.astimezone(zone)
        try:
            rule = rrulestr("\n".join(raw["recurrence"]), dtstart=dtstart, forceset=True)
            for index, occurrence in enumerate(rule.xafter(lower, count=5001, inc=True)):
                if occurrence >= upper:
                    break
                if index == 5000:
                    raise DomainError("CALENDAR_RECURRENCE_TOO_LARGE", "Too many recurring blockers; simplify calendar recurrence")
                occurrence_start = occurrence.replace(tzinfo=zone).astimezone(timezone.utc) if all_day else occurrence.astimezone(timezone.utc)
                occurrence_end = (occurrence.replace(tzinfo=zone) + timedelta(days=day_count)).astimezone(timezone.utc) if all_day else occurrence_start + duration
                if (event.calendar_event_id, occurrence_start) in excluded:
                    continue
                interval = Interval(start=occurrence_start, end=occurrence_end)
                if overlaps(start, end, interval):
                    result.append(interval)
                if len(result) > 5000:
                    raise DomainError("CALENDAR_RECURRENCE_TOO_LARGE", "Too many recurring blockers; simplify calendar recurrence")
        except (ValueError, TypeError) as error:
            if isinstance(error, DomainError):
                raise
            raise DomainError("CALENDAR_RECURRENCE_INVALID", "Calendar recurrence could not be expanded; plan unchanged") from error
    return tuple(result)
