import json
from datetime import timedelta

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import make_engine
from gymclaw.models import CalendarCredential, CalendarSyncState, NotificationJob
from gymclaw.tests.test_cli import run
from gymclaw.tests.test_calendar_reconcile import NOW, at, clock


def test_calendar_json_cli_plan_publish_manual_edit_and_restart(tmp_path):
    url = f"sqlite:///{tmp_path / 'state.db'}"
    run(tmp_path, "db", "init")
    run(tmp_path, "template", "import", "--file", "config/exercises.seed.json")
    run(tmp_path, "profile", "update", "--data", '{"earliest_workout_start":"19:00", "latest_workout_finish":"21:10", "minimum_workout_minutes":70}')
    initial = tmp_path / "initial.json"
    initial.write_text(json.dumps({"calendar_id": "demo-calendar", "events": []}))
    prefix = ("--db-url", url, "calendar")
    code, plan = run(tmp_path, *prefix, "plan-week", "--fixture", str(initial), "--week-start", "2026-10-12", "--template-id", "upper-a", "--request-id", "week", "--now", NOW.isoformat())
    assert code == 0 and plan["ok"]
    assert len(plan["data"]["sessions"]) == len(plan["data"]["writes"]) == 3
    assert not plan["data"]["remote_events_changed"]
    wed = next(w for w in plan["data"]["writes"] if w["body"]["start"]["dateTime"].startswith("2026-10-14"))
    assert run(tmp_path, *prefix, "pending-writes")[1]["data"]["calendar_id"] == "demo-calendar"
    code, published = run(tmp_path, *prefix, "publish", "--fixture", str(initial), "--now", (NOW + timedelta(minutes=1)).isoformat())
    assert code == 0 and published["data"]["source"] == "fixture"
    assert len(published["data"]["results"]) == 3 and published["data"]["remaining_writes"] == 0
    edited = tmp_path / "edited.json"
    edited.write_text(json.dumps({"calendar_id": "demo-calendar", "full": False, "events": [wed["body"] | {"etag": '"manual"', "start": clock(at(15)), "end": clock(at(15, 20, 10))}]}))
    code, synced = run(tmp_path, *prefix, "sync", "--fixture", str(edited), "--now", (NOW + timedelta(minutes=2)).isoformat())
    assert code == 0 and synced["ok"]
    week = run(tmp_path, *prefix, "get-week", "--week-start", "2026-10-12")[1]["data"]
    locked = next(s for s in week["sessions"] if s["id"] == wed["session_id"])
    assert locked["user_locked"] and locked["start"].startswith("2026-10-15")
    again = run(tmp_path, *prefix, "sync", "--fixture", str(edited), "--now", (NOW + timedelta(minutes=3)).isoformat())[1]
    assert not again["events"]
    engine = make_engine(url)
    with Session(engine) as db:
        assert db.get(CalendarSyncState, 1).source == "fixture"
        assert db.get(CalendarCredential, 1) is None
        assert len(db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == wed["session_id"], NotificationJob.status == "PENDING")).all()) == 3
    engine.dispose()


def test_calendar_cli_safe_defaults_and_scope_errors(tmp_path):
    run(tmp_path, "db", "init")
    fixture = tmp_path / "fixture.json"
    fixture.write_text('{"calendar_id":"demo", "events":[]}')
    url = f"sqlite:///{tmp_path / 'state.db'}"
    for args, code in [
        (("calendar", "publish"), "CALENDAR_WRITES_NOT_APPROVED"),
        (("calendar", "sync", "--fixture", str(fixture)), "FIXTURE_DB_REQUIRED"),
        (("calendar", "auth", "--calendar-id", "primary"), "CALENDAR_ID_REQUIRED"),
    ]:
        status, result = run(tmp_path, *args)
        assert status == 1 and result["error"]["code"] == code
    assert run(tmp_path, "--db-url", url, "calendar", "sync", "--fixture", str(fixture))[1]["ok"]
    status, result = run(tmp_path, "calendar", "auth", "--calendar-id", "real-dedicated")
    assert status == 1 and result["error"]["code"] == "CALENDAR_SCOPE_MISMATCH"
