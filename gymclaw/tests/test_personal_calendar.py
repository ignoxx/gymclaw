from datetime import datetime, timedelta, timezone
from types import SimpleNamespace
from zoneinfo import ZoneInfo

from sqlalchemy.orm import Session
import pytest

from gymclaw.models import PersonalCalendarState, PlannedSession
from gymclaw.providers import ics_feed
from gymclaw.services import personal_calendar, weekly
from gymclaw.services.calendar_busy import busy_intervals
from gymclaw.services.errors import DomainError
from gymclaw.tests.test_weekly import engine, SUNDAY  # noqa: F401

ZONE = ZoneInfo("Europe/Berlin")


def ics(*events: str) -> bytes:
    body = "".join(f"BEGIN:VEVENT\r\n{e.strip()}\r\nEND:VEVENT\r\n" for e in events)
    return f"BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n".encode()


class Feed:
    """Stand-in for the iCloud published feed; records every request."""
    def __init__(self, data: bytes, status: int = 200):
        self.data, self.status, self.calls = data, status, 0

    def get(self, url, **kwargs):
        self.calls += 1
        return SimpleNamespace(status_code=self.status, content=self.data)


def event(start: datetime, end: datetime, *extra: str) -> str:
    stamp = lambda d: d.astimezone(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return "\r\n".join((f"UID:{stamp(start)}", f"DTSTART:{stamp(start)}", f"DTEND:{stamp(end)}", *extra))


def test_feed_expands_recurrence_and_skips_free_cancelled():
    data = ics(
        "UID:weekly\r\nDTSTART;TZID=Europe/Berlin:20261019T190000\r\nDTEND;TZID=Europe/Berlin:20261019T200000\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEXDATE;TZID=Europe/Berlin:20261026T190000",
        "UID:allday\r\nDTSTART;VALUE=DATE:20261021",
        "UID:floating\r\nDTSTART:20261022T120000\r\nDURATION:PT1H",
        "UID:free\r\nDTSTART:20261023T120000Z\r\nDTEND:20261023T130000Z\r\nTRANSP:TRANSPARENT",
        "UID:off\r\nDTSTART:20261024T120000Z\r\nDTEND:20261024T130000Z\r\nSTATUS:CANCELLED")
    busy = ics_feed.busy_intervals(data, datetime(2026, 10, 18, tzinfo=ZONE), datetime(2026, 11, 8, tzinfo=ZONE), ZONE)
    assert [(b.start, b.end) for b in busy] == [
        (datetime(2026, 10, 19, 19, tzinfo=ZONE), datetime(2026, 10, 19, 20, tzinfo=ZONE)),
        (datetime(2026, 10, 21, tzinfo=ZONE), datetime(2026, 10, 22, tzinfo=ZONE)),
        (datetime(2026, 10, 22, 12, tzinfo=ZONE), datetime(2026, 10, 22, 13, tzinfo=ZONE)),
        (datetime(2026, 11, 2, 19, tzinfo=ZONE), datetime(2026, 11, 2, 20, tzinfo=ZONE))]


def test_floating_and_all_day_events_at_utc_window_boundary():
    data = ics(
        "UID:floating-boundary\r\nDTSTART:20261019T003000\r\nDTEND:20261019T013000",
        "UID:day-boundary\r\nDTSTART;VALUE=DATE:20261019")
    start = datetime(2026, 10, 18, 22, tzinfo=timezone.utc)
    end = start + timedelta(hours=1)
    busy = ics_feed.busy_intervals(data, start, end, ZONE)
    assert [(b.start, b.end) for b in busy] == [
        (start, datetime(2026, 10, 19, 22, tzinfo=timezone.utc)),
        (start + timedelta(minutes=30), end + timedelta(minutes=30))]


def test_webcal_link_normalized_and_plain_http_rejected():
    assert ics_feed.normalize_url("webcal://p42-caldav.icloud.com/published/2/abc") == "https://p42-caldav.icloud.com/published/2/abc"
    with pytest.raises(DomainError, match="https"):
        ics_feed.normalize_url("http://example.com/cal.ics")


def test_personal_blocker_moves_session_and_outage_keeps_blockers(engine):  # noqa: F811
    with Session(engine) as db, db.begin():
        plan = weekly.weekly_plan(db, "short", now=SUNDAY)
        session = db.get(PlannedSession, plan["plan"]["sessions"][-1]["id"])
        original = session.planned_start_at
        feed = Feed(ics(event(original - timedelta(hours=1), original + timedelta(hours=2))))
        result = personal_calendar.connect(db, "webcal://p42-caldav.icloud.com/published/2/secret", now=SUNDAY, transport=feed)
        assert result["changes"] and result["user_message_hint"].startswith("Personal calendar changed")
        assert session.planned_start_at != original
        assert any(b.start == original - timedelta(hours=1) for b in busy_intervals(db, SUNDAY, SUNDAY + timedelta(days=8)))
        assert "secret" not in str(personal_calendar.status(db))

        # Throttled: the 60 s watcher doesn't hit the feed every minute.
        assert not personal_calendar.refresh(db, now=SUNDAY + timedelta(minutes=1), transport=feed)["refreshed"]
        assert feed.calls == 1

        # Outage records the error but keeps last known blockers.
        down = Feed(b"", status=503)
        failed = personal_calendar.refresh(db, now=SUNDAY + timedelta(minutes=10), transport=down)
        assert failed["error_code"] == "PERSONAL_CALENDAR_UNAVAILABLE"
        assert db.get(PersonalCalendarState, 1).last_error_code == "PERSONAL_CALENDAR_UNAVAILABLE"
        assert personal_calendar.status(db)["blockers"] == 1

        # Unchanged feed is not news.
        again = personal_calendar.refresh(db, now=SUNDAY + timedelta(minutes=20), transport=feed)
        assert again["refreshed"] and not again["events"]

        personal_calendar.disconnect(db, now=SUNDAY + timedelta(minutes=30))
        assert not personal_calendar.status(db)["connected"]
        assert not personal_calendar.blocks(db, SUNDAY, SUNDAY + timedelta(days=8))
