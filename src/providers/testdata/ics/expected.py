"""Regenerate expected.json from the Python implementation (the spec).

Run from the repo root: PYTHONPATH=. .venv/bin/python src/providers/testdata/ics/expected.py
"""
import json
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

from gymclaw.providers import ics_feed
from gymclaw.services.errors import DomainError

HERE = Path(__file__).parent
# (feed, window start, window end, profile zone): windows like personal_calendar's (-1 day, +35 days) plus edges.
CASES = [
    ("basic.ics", "2026-10-18T00:00:00+02:00", "2026-11-08T00:00:00+01:00", "Europe/Berlin"),
    ("basic.ics", "2026-10-18T22:00:00+00:00", "2026-10-18T23:00:00+00:00", "Europe/Berlin"),
    ("basic.ics", "2026-10-18T00:00:00+00:00", "2026-11-08T00:00:00+00:00", "America/New_York"),
    ("dst.ics", "2026-10-11T00:00:00+02:00", "2026-11-15T00:00:00+01:00", "Europe/Berlin"),
    ("dst.ics", "2026-10-11T00:00:00+00:00", "2026-11-15T00:00:00+00:00", "Asia/Tokyo"),
    ("spring.ics", "2027-03-20T00:00:00+01:00", "2027-04-24T00:00:00+02:00", "Europe/Berlin"),
    ("rules.ics", "2026-10-18T00:00:00+02:00", "2026-11-22T00:00:00+01:00", "Europe/Berlin"),
    ("rules.ics", "2027-02-20T00:00:00+01:00", "2027-03-27T00:00:00+01:00", "Europe/Berlin"),
    ("overrides.ics", "2026-10-18T00:00:00+02:00", "2026-11-22T00:00:00+01:00", "Europe/Berlin"),
    ("overrides.ics", "2026-11-22T00:00:00+01:00", "2026-12-27T00:00:00+01:00", "Europe/Berlin"),
    ("overrides.ics", "2026-10-27T00:00:00+01:00", "2026-10-27T15:30:00+01:00", "Europe/Berlin"),
]


def run(feed, start, end, zone):
    try:
        busy = ics_feed.busy_intervals((HERE / feed).read_bytes(), datetime.fromisoformat(start), datetime.fromisoformat(end), ZoneInfo(zone))
    except DomainError as error:
        return {"error": error.code}
    return [[i.start.isoformat(), i.end.isoformat()] for i in busy]


cases = [{"feed": f, "start": s, "end": e, "zone": z, "expected": run(f, s, e, z)} for f, s, e, z in CASES]
(HERE / "expected.json").write_text(json.dumps(cases, indent=1) + "\n")

# Google recurring masters: domain/calendar_busy.py feeds `recurrence` lines to dateutil.
from dateutil.rrule import rrulestr  # noqa: E402

BERLIN = ZoneInfo("Europe/Berlin")
RECURRENCE = [
    (["RRULE:FREQ=WEEKLY;BYDAY=SU"], "2026-10-19T19:00:00", "2026-10-23T00:00:00", "2026-10-27T00:00:00", True),
    (["RRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20261026"], "2026-09-28T00:00:00", "2026-10-01T00:00:00", "2026-11-30T00:00:00", False),
    (["RRULE:FREQ=DAILY;COUNT=10", "EXDATE;TZID=Europe/Berlin:20261024T073000,20261026T073000"], "2026-10-22T07:30:00", "2026-10-20T00:00:00", "2026-12-01T00:00:00", True),
    (["RRULE:FREQ=MONTHLY;BYDAY=-1FR;UNTIL=20270301T000000Z"], "2026-01-30T18:00:00", "2026-06-01T00:00:00", "2027-06-01T00:00:00", True),
    (["RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH;WKST=SU"], "2026-10-13T06:45:00", "2026-11-01T00:00:00", "2026-12-15T00:00:00", True),
    (["RRULE:FREQ=HOURLY"], "2026-01-01T00:00:00", "2026-01-01T00:00:00", "2026-12-01T00:00:00", True),
    (["RRULE:FREQ=SOMETIMES"], "2026-01-01T00:00:00", "2026-01-01T00:00:00", "2026-12-01T00:00:00", True),
]


def recurrence(lines, dtstart, lower, upper, zoned):
    tz = BERLIN if zoned else None
    start, low, high = (datetime.fromisoformat(v).replace(tzinfo=tz) for v in (dtstart, lower, upper))
    result = []
    try:
        for index, occurrence in enumerate(rrulestr("\n".join(lines), dtstart=start, forceset=True).xafter(low, count=5001, inc=True)):
            if occurrence >= high:
                break
            if index == 5000:
                return {"error": "CALENDAR_RECURRENCE_TOO_LARGE"}
            result.append(occurrence.isoformat())
    except (ValueError, TypeError):
        return {"error": "CALENDAR_RECURRENCE_INVALID"}
    return result


cases = [{"lines": l, "dtstart": d, "lower": lo, "upper": u, "zoned": z, "expected": recurrence(l, d, lo, u, z)} for l, d, lo, u, z in RECURRENCE]
(HERE / "recurrence.json").write_text(json.dumps(cases, indent=1) + "\n")
