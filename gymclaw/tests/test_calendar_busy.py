from datetime import datetime
from zoneinfo import ZoneInfo

from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.providers.fixture_calendar import FixtureCalendarProvider
from gymclaw.services.calendar import sync_calendar
from gymclaw.services.calendar_busy import busy_intervals

ZONE = ZoneInfo("Europe/Berlin")


def test_recurring_blocker_honors_dst_and_moved_exception(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'recurring.db'}")
    initialize(engine)
    provider = FixtureCalendarProvider()
    provider.put({"id": "weekly", "start": {"dateTime": "2026-10-19T19:00:00+02:00", "timeZone": "Europe/Berlin"}, "end": {"dateTime": "2026-10-19T20:00:00+02:00"}, "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=SU"]})
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=datetime(2026, 10, 24, tzinfo=ZONE))
        busy = busy_intervals(db, datetime(2026, 10, 24, tzinfo=ZONE), datetime(2026, 10, 27, tzinfo=ZONE))
        assert len(busy) == 1
        assert busy[0].start == datetime(2026, 10, 25, 19, tzinfo=ZONE)
        assert busy[0].start.isoformat() == "2026-10-25T18:00:00+00:00"
    provider.put({"id": "exception", "recurringEventId": "weekly", "originalStartTime": {"dateTime": "2026-10-25T19:00:00+01:00"}, "start": {"dateTime": "2026-10-26T20:00:00+01:00"}, "end": {"dateTime": "2026-10-26T21:00:00+01:00"}})
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=datetime(2026, 10, 24, 1, tzinfo=ZONE))
        busy = busy_intervals(db, datetime(2026, 10, 24, tzinfo=ZONE), datetime(2026, 10, 27, tzinfo=ZONE))
        assert len(busy) == 1 and busy[0].start == datetime(2026, 10, 26, 20, tzinfo=ZONE)
    engine.dispose()


def test_all_day_recurring_cancellation_and_transparent_event(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'recurring.db'}")
    initialize(engine)
    provider = FixtureCalendarProvider()
    provider.put({"id": "weekly", "start": {"date": "2026-09-28"}, "end": {"date": "2026-09-29"}, "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20261026"]})
    provider.put({"id": "free", "start": {"date": "2026-10-12"}, "end": {"date": "2026-10-19"}, "transparency": "transparent"})
    provider.put({"id": "cancelled-instance", "status": "cancelled", "recurringEventId": "weekly", "originalStartTime": {"date": "2026-10-12"}})
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=datetime(2026, 10, 11, tzinfo=ZONE))
        assert not busy_intervals(db, datetime(2026, 10, 12, tzinfo=ZONE), datetime(2026, 10, 19, tzinfo=ZONE))
    engine.dispose()
