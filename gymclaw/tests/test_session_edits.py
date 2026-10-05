from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

import pytest
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import NotificationJob, PlannedSession
from gymclaw.providers.fixture_calendar import FixtureCalendarProvider
from gymclaw.services import session_edits
from gymclaw.services.calendar import sync_calendar
from gymclaw.services.calendar_writes import apply_write, event_id_for, pending_writes
from gymclaw.services.profile import update_profile
from gymclaw.services.replanning import commit_upcoming
from gymclaw.services.scheduling import schedule_week
from gymclaw.services.templates import ExerciseSpec, Template, import_template

ZONE = ZoneInfo("Europe/Berlin")
NOW = datetime(2026, 10, 11, 19, tzinfo=ZONE)
WEEK = date(2026, 10, 12)


def at(day, hour=19, minute=0):
    return datetime(2026, 10, day, hour, minute, tzinfo=ZONE)


def publish(engine, provider, now):
    with Session(engine) as db:
        ids = [w["id"] for w in pending_writes(db)]
    for write_id in ids:
        with Session(engine) as db, db.begin():
            assert apply_write(db, provider, write_id, now=now)["status"] == "APPLIED"


@pytest.fixture
def planned(tmp_path):
    """A published week of evening sessions (19:00–21:10 window)."""
    engine = make_engine(f"sqlite:///{tmp_path / 'edits.db'}")
    initialize(engine)
    provider = FixtureCalendarProvider()
    specs = [ExerciseSpec(id=key, name=key.title(), role=key, target_weight=50, priority=priority) for key, priority in [("bench", 10), ("row", 9), ("curl", 3)]]
    with Session(engine) as db, db.begin():
        update_profile(db, {"earliest_workout_start": "19:00", "latest_workout_finish": "21:10", "minimum_workout_minutes": 60})
        for template_id in ("push", "legs"):
            import_template(db, Template(id=template_id, name=template_id.title(), exercises=specs, set_duration_seconds=60, transition_seconds=90, source="demo_fixture"))
        sync_calendar(db, provider, now=NOW)
        ids = [s["id"] for s in schedule_week(db, WEEK, template_id="push", now=NOW)["sessions"]]
        commit_upcoming(db, now=NOW)
    publish(engine, provider, NOW + timedelta(minutes=1))
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=NOW + timedelta(minutes=2))
    yield engine, provider, ids
    engine.dispose()


def test_move_pins_outside_window_and_reaches_calendar(planned):
    engine, provider, ids = planned
    friday = ids[-1]
    now = NOW + timedelta(minutes=3)
    with Session(engine) as db, db.begin():
        before = db.get(PlannedSession, friday)
        duration = before.planned_end_at - before.planned_start_at
        result = session_edits.move(db, friday, start=at(16, 11, 30), end=None, now=now, request_id="tg-1-move")
        session = db.get(PlannedSession, friday)
        assert session.user_locked and session.status == "COMMITTED"
        assert session.planned_start_at == at(16, 11, 30) and session.planned_end_at - session.planned_start_at == duration
        assert "outside configured workout time bounds" in result["user_message_hint"]
        jobs = db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == friday, NotificationJob.status == "PENDING"))
        assert {j.due_at.astimezone(ZONE).date() for j in jobs} == {at(16).date()}
        # Same request is a retry, not a second move.
        assert session_edits.move(db, friday, start=at(16, 11, 30), end=None, now=now, request_id="tg-1-move") == result
    publish(engine, provider, now + timedelta(minutes=1))
    assert provider.get_event(event_id_for(friday)).start == at(16, 11, 30)
    with Session(engine) as db, db.begin():
        echo = sync_calendar(db, provider, now=now + timedelta(minutes=2))
        assert not echo["events"] and db.get(PlannedSession, friday).planned_start_at == at(16, 11, 30)


def test_add_and_change_workout_are_pinned(planned):
    engine, provider, _ = planned
    now = NOW + timedelta(minutes=3)
    with Session(engine) as db, db.begin():
        added = session_edits.add(db, template_id="push", start=at(17, 10), end=at(17, 11), now=now, request_id="tg-2-add")["data"]["session"]
        changed = session_edits.set_template(db, added["id"], template_id="legs", now=now, request_id="tg-3-legs")["data"]["session"]
        assert changed["user_locked"] and changed["workout_template_id"] == "legs"
        assert changed["workout_plan"]["template"]["id"] == "legs"
    publish(engine, provider, now + timedelta(minutes=1))
    remote = provider.get_event(event_id_for(added["id"]))
    assert remote.start == at(17, 10) and remote.end == at(17, 11)
    assert provider.events[remote.id]["summary"] == "🏋️ Legs"


def test_cancel_deletes_event_and_keeps_slot_free(planned):
    engine, provider, ids = planned
    now = NOW + timedelta(minutes=3)
    with Session(engine) as db, db.begin():
        cancelled = db.get(PlannedSession, ids[0])
        slot = (cancelled.planned_start_at, cancelled.planned_end_at)
        session_edits.cancel(db, ids[0], now=now, request_id="tg-4-cancel")
        assert cancelled.status == "CANCELLED"
        active = db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"])))
        assert all((s.planned_start_at, s.planned_end_at) != slot for s in active)
    publish(engine, provider, now + timedelta(minutes=1))
    assert provider.get_event(event_id_for(ids[0])).status == "cancelled"
