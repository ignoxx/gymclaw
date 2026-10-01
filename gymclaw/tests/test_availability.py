from datetime import datetime, timedelta, timezone
import json

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.cli import main
from gymclaw.db import make_engine
from gymclaw.models import AvailabilityBlock, NotificationJob, PlannedSession
from gymclaw.services import availability, weekly, workout
from gymclaw.services.notifications import schedule_session_jobs
from gymclaw.services.errors import DomainError
from gymclaw.tests.test_weekly import engine, SUNDAY
import pytest


def test_inclusive_local_date_handles_dst_and_restart(engine, capsys):
    args = ["--db-url", str(engine.url), "availability", "add", "--kind", "TRAVEL", "--from-date", "2026-10-25", "--through", "2026-10-25", "--request-id", "dst-trip", "--now", SUNDAY.isoformat()]
    assert main(args) == 0
    first = json.loads(capsys.readouterr().out)["data"]
    assert datetime.fromisoformat(first["end"]) - datetime.fromisoformat(first["start"]) == timedelta(hours=25)
    assert main(args) == 0
    assert json.loads(capsys.readouterr().out)["data"] == first
    restarted = make_engine(str(engine.url))
    with Session(restarted) as db:
        assert len(availability.list_blocks(db, now=SUNDAY)) == 1
        assert db.get(AvailabilityBlock, first["block_id"]).kind == "TRAVEL"
    restarted.dispose()
    assert main(args[:-4] + ["--request-id", "dst-trip", "--now", SUNDAY.isoformat(), "--cancel-locked"]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "IDEMPOTENCY_CONFLICT"


def test_pause_repairs_future_slots_no_makeup_volume_and_expires(engine):
    with Session(engine) as db, db.begin():
        weekly.weekly_plan(db, "short", now=SUNDAY)
        end = SUNDAY + timedelta(days=6)
        result = availability.add(db, start=SUNDAY, end=end, kind="SICK", now=SUNDAY, request_id="pause")
        assert result["data"]["replanning"]["changed"]
        assert all(row.status == "CANCELLED" for row in db.scalars(select(PlannedSession)))
        assert not db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING")).all()
        with pytest.raises(DomainError, match="availability override"):
            workout.start(db, "short", now=SUNDAY + timedelta(hours=1), request_id="blocked-start")
        resumed = weekly.rolling_plan(db, "short", now=end)
        assert resumed["created"]
        assert all(row.planned_start_at >= end for row in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]))))
        workout.start(db, "short", now=end, request_id="allowed-at-end")


def test_locked_slot_preserved_but_reminders_suppressed_until_explicit_removal(engine):
    with Session(engine) as db, db.begin():
        result = weekly.weekly_plan(db, "short", now=SUNDAY)
        row = db.get(PlannedSession, result["plan"]["sessions"][0]["id"])
        row.user_locked = True
        original = row.planned_start_at
        block = availability.add(db, start=SUNDAY, end=SUNDAY + timedelta(days=2), kind="TRAVEL", now=SUNDAY, request_id="travel")
        assert row.id in block["data"]["locked_conflicts"] and row.planned_start_at == original
        schedule_session_jobs(db, row, now=SUNDAY)
        assert not db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == row.id, NotificationJob.status == "PENDING")).all()
        availability.retract(db, block["data"]["block_id"], now=SUNDAY + timedelta(minutes=1), request_id="back-early")
        assert db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == row.id, NotificationJob.status == "PENDING")).all()
        assert row.user_locked and row.planned_start_at == original


def test_explicit_cancel_locked_records_later_user_intent(engine):
    with Session(engine) as db, db.begin():
        result = weekly.weekly_plan(db, "short", now=SUNDAY)
        row = db.get(PlannedSession, result["plan"]["sessions"][0]["id"])
        row.user_locked = True
        availability.add(db, start=SUNDAY, end=SUNDAY + timedelta(days=7), kind="SICK", now=SUNDAY, request_id="cancel-too", cancel_locked=True)
        assert row.status == "CANCELLED" and not row.user_locked
        assert row.workout_plan_json["manual_lock_cancelled_by_request"] == "cancel-too"
        assert row.workout_plan_json["deleted_by_user"]
