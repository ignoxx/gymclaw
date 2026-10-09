import json
import os
from pathlib import Path
import subprocess
import sys

from datetime import date, datetime
from sqlalchemy.orm import Session
from sqlalchemy import select

from gymclaw.db import make_engine, initialize
from gymclaw.models import AgentEvent, CalendarEventSnapshot, PlannedSession
from gymclaw.services.profile import get_profile, update_profile
from gymclaw.services.scheduling import schedule_week


def run(tmp_path, *args):
    env = os.environ | {"GYMCLAW_DB_URL": f"sqlite:///{tmp_path / 'state.db'}"}
    result = subprocess.run([sys.executable, "-m", "gymclaw.cli", *args], capture_output=True, text=True, env=env)
    assert not result.stderr, result.stderr
    return result.returncode, json.loads(result.stdout)


def test_cli_restart_plan_and_idempotency(tmp_path):
    assert run(tmp_path, "db", "init")[1]["ok"]
    assert run(tmp_path, "profile", "update", "--data", '{"prep_minutes":25}')[1]["data"]["prep_minutes"] == 25
    assert run(tmp_path, "profile", "get")[1]["data"]["prep_minutes"] == 25
    args = ("schedule", "plan-week", "--week-start", "2026-10-12", "--now", "2026-10-11T12:00:00+02:00", "--request-id", "week-1", "--json")
    code, first = run(tmp_path, *args)
    assert code == 0 and first["ok"]
    assert len(first["data"]["sessions"]) == 3
    assert first == run(tmp_path, *args)[1]
    again = run(tmp_path, "planning", "plan-week", "--week-start", "2026-10-12", "--now", "2026-10-11T12:00:00+02:00")[1]
    assert again["data"]["created_count"] == 0
    assert len(again["data"]["sessions"]) == 3
    engine = make_engine(f"sqlite:///{tmp_path / 'state.db'}")
    with Session(engine) as db:
        assert len(db.scalars(select(PlannedSession)).all()) == 3
        assert len(db.scalars(select(AgentEvent)).all()) == 2


def test_cli_errors_json_and_rollback(tmp_path):
    run(tmp_path, "db", "init")
    for args in [("profile", "update", "--data", '{"prep_minutes":-1}'), ("profile", "update", "--data", "[]"), ("unknown",)]:
        code, result = run(tmp_path, *args)
        assert code == 1 and not result["ok"]
        assert result["error"]["code"] == "INVALID_INPUT"
    assert run(tmp_path, "profile", "get")[1]["data"]["prep_minutes"] == 15


def test_cli_forgives_common_guesses(tmp_path):
    run(tmp_path, "db", "init")
    # A wrong or missing operation lists every command, so one failed call is enough to fix it.
    code, lost = run(tmp_path, "calendar")
    assert code == 1 and "calendar auth|sync" in lost["error"]["message"] and "crowd poll|" in lost["error"]["message"]
    # Guessed names map to the real operation; a naive time is the owner's local time (Europe/Berlin).
    code, prediction = run(tmp_path, "crowd", "now", "--fixture", "missing.json")
    assert code == 1 and "invalid choice" not in prediction["error"]["message"]
    code, planned = run(tmp_path, "schedule", "plan-week", "--week-start", "2026-10-12", "--now", "2026-10-11T12:00:00")
    assert code == 0 and planned["data"]["sessions"][0]["start"] > "2026-10-11T10:00:00+00:00"


def test_calendar_snapshot_and_locked_session_preserved(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'state.db'}")
    initialize(engine)
    start = datetime.fromisoformat("2026-10-15T19:00:00+02:00")
    end = datetime.fromisoformat("2026-10-15T20:10:00+02:00")
    with Session(engine) as db, db.begin():
        get_profile(db)
        db.add(CalendarEventSnapshot(calendar_event_id="travel", title="Travel", start_at=datetime.fromisoformat("2026-10-12T00:00:00+02:00"), end_at=datetime.fromisoformat("2026-10-14T00:00:00+02:00")))
        db.add(PlannedSession(id="locked", week_id="2026-10-12", user_locked=True, planned_start_at=start, planned_end_at=end, prep_start_at=start, leave_home_at=start, expected_finish_at=end))
    with Session(engine) as db, db.begin():
        result = schedule_week(db, date(2026, 10, 12), now=datetime.fromisoformat("2026-10-11T12:00:00+02:00"))
        assert len(result["sessions"]) == 1
        assert result["sessions"][0]["id"] == "locked"
        assert db.get(PlannedSession, "locked").planned_start_at == start


def test_request_id_input_conflict(tmp_path):
    run(tmp_path, "db", "init")
    run(tmp_path, "schedule", "plan-week", "--week-start", "2026-10-12", "--now", "2026-10-11T00:00:00+02:00", "--request-id", "same")
    code, result = run(tmp_path, "schedule", "plan-week", "--week-start", "2026-10-19", "--request-id", "same")
    assert code == 1 and "different planning input" in result["error"]["message"]


def test_fixture_explicit_and_persisted(tmp_path):
    run(tmp_path, "db", "init")
    fixture = tmp_path / "travel.json"
    fixture.write_text(json.dumps({"unavailable": [{"start": "2026-10-12T00:00:00+02:00", "end": "2026-10-19T00:00:00+02:00"}]}))
    result = run(tmp_path, "schedule", "plan-week", "--week-start", "2026-10-12", "--now", "2026-10-11T00:00:00+02:00", "--fixture", str(fixture))[1]
    assert result["ok"] and result["data"]["source"] == "fixture"
    assert not result["data"]["sessions"]
    engine = make_engine(f"sqlite:///{tmp_path / 'state.db'}")
    with Session(engine) as db:
        assert db.scalar(select(AgentEvent)).payload_json["request"]["fixture"]["unavailable"]
