"""Read-only ICS feed (iCloud "Public Calendar" link, Google secret iCal address).

GET only, nothing is ever written back. Yields busy intervals; titles/notes are discarded.
"""
from datetime import date, datetime, time, timezone
from typing import Protocol
from urllib.parse import urlsplit
from zoneinfo import ZoneInfo

import icalendar
import recurring_ical_events
import requests

from gymclaw.services.errors import DomainError
from gymclaw.services.planning import Interval

MAX_BYTES = 5_000_000


class Transport(Protocol):
    def get(self, url: str, **kwargs) -> requests.Response: ...


def normalize_url(url: str) -> str:
    """Apple shares feeds as webcal://; fetch them over https."""
    url = url.strip()
    if url.startswith("webcal://"):
        url = "https://" + url.removeprefix("webcal://")
    parts = urlsplit(url)
    if parts.scheme != "https" or not parts.hostname:
        raise DomainError("PERSONAL_CALENDAR_URL_INVALID", "Personal calendar feed must be an https:// or webcal:// ICS link")
    return url


def fetch(url: str, transport: Transport | None = None) -> bytes:
    try:
        response = (transport or requests).get(url, timeout=30, headers={"Accept": "text/calendar"})
    except Exception as error:
        # Feed URL is a capability secret; never put it in errors.
        raise DomainError("PERSONAL_CALENDAR_UNAVAILABLE", "Personal calendar feed request failed") from error
    if response.status_code >= 400:
        raise DomainError("PERSONAL_CALENDAR_UNAVAILABLE", f"Personal calendar feed failed (HTTP {response.status_code})")
    if len(response.content) > MAX_BYTES:
        raise DomainError("PERSONAL_CALENDAR_TOO_LARGE", "Personal calendar feed exceeds size limit")
    return response.content


def _aware(value: date | datetime, zone: ZoneInfo) -> datetime:
    if not isinstance(value, datetime):
        value = datetime.combine(value, time())
    if value.tzinfo is None:
        value = value.replace(tzinfo=zone)  # Floating times and all-day dates use profile zone.
    return value.astimezone(timezone.utc)


def busy_intervals(data: bytes, start: datetime, end: datetime, zone: ZoneInfo) -> tuple[Interval, ...]:
    """Expand recurrences/exceptions in [start, end). Free (TRANSP:TRANSPARENT) and cancelled events don't block."""
    try:
        calendar = icalendar.Calendar.from_ical(data)
        # Query floating times and all-day dates in the same zone used below.
        occurrences = recurring_ical_events.of(calendar).between(start.astimezone(zone), end.astimezone(zone))
        result = set()
        for event in occurrences:
            if str(event.get("TRANSP", "")).upper() == "TRANSPARENT" or str(event.get("STATUS", "")).upper() == "CANCELLED":
                continue
            begin = _aware(event.start, zone)
            finish = _aware(event.end, zone)  # RFC 5545 defaults: all-day lasts one day, timed has zero length.
            if finish > begin and begin < end and finish > start:
                result.add((begin, finish))
    except Exception as error:
        raise DomainError("PERSONAL_CALENDAR_INVALID", "Personal calendar feed could not be parsed") from error
    return tuple(Interval(start=s, end=e) for s, e in sorted(result))
