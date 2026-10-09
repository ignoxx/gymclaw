import json
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.cli import main
from gymclaw.db import make_engine
from gymclaw.models import CalendarCredential, CalendarSyncState, NotificationJob
from gymclaw.services import weekly
from gymclaw.services.profile import get_profile
from gymclaw.tests.test_cli import run
from gymclaw.tests.test_calendar_reconcile import NOW, at, clock
from gymclaw.tests.test_weekly import SUNDAY, engine  # noqa: F401  (engine is a fixture)


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


def test_move_session_picks_valid_slot_or_explains(engine, capsys):
    def call(*args):
        code = main(["--db-url", str(engine.url), *args, "--now", SUNDAY.isoformat()])
        return code, json.loads(capsys.readouterr().out)

    with Session(engine) as db, db.begin():
        sessions = weekly.weekly_plan(db, "short", now=SUNDAY)["plan"]["sessions"]
    friday = next(s for s in sessions if s["start"].startswith("2026-10-16"))
    move = ("calendar", "move", "--session-id", friday["id"], "--request-id", "tg-1-move")
    code, moved = call(*move, "--to", "2026-10-16T12:00:00+02:00")
    assert code == 0 and moved["data"]["to"] == "2026-10-16T10:00:00+00:00"
    assert call(*move, "--to", "2026-10-16T12:00:00+02:00")[1] == moved
    writes = call("calendar", "pending-writes")[1]["data"]["writes"]
    assert any(w["session_id"] == friday["id"] and w["body"]["start"]["dateTime"] == "2026-10-16T12:00:00+02:00" for w in writes)
    # "Later today": best slot at/after the bound, plus spread-out alternatives for buttons.
    code, later = call("calendar", "move", "--session-id", friday["id"], "--request-id", "tg-4-move", "--after", "2026-10-16T14:00:00+02:00")
    starts = [datetime.fromisoformat(t) for t in (later["data"]["to"], *(a["start"] for a in later["data"]["alternatives"]))]
    assert code == 0 and later["data"]["alternatives"]
    assert all(t >= datetime.fromisoformat("2026-10-16T12:00:00+00:00") and t.date().isoformat() == "2026-10-16" for t in starts)
    assert all(abs(x - y) >= timedelta(minutes=60) for i, x in enumerate(starts) for y in starts[i + 1:])
    # The owner's explicit pick (12:00) is liked; the slot it was first planned at is not.
    from gymclaw.services.habits import affinity, evidence
    with Session(engine) as db:
        points = evidence(db, now=SUNDAY, zone=ZoneInfo("Europe/Berlin"))
    first = datetime.fromisoformat(friday["start"]).astimezone(ZoneInfo("Europe/Berlin"))
    assert affinity(points, 12 * 60) > 0.5 > affinity(points, first.hour * 60 + first.minute)
    # A time the owner names is kept even off-grid and outside the window: locked against replanning,
    # still published, and the broken rule named.
    code, kept = call("calendar", "move", "--session-id", friday["id"], "--request-id", "tg-5-move", "--to", "2026-10-16T21:07:00+02:00")
    assert code == 0 and kept["data"]["to"] == "2026-10-16T19:07:00+00:00" and kept["data"]["warnings"] == ["outside your workout window (07:00–22:00)"]
    writes = call("calendar", "pending-writes")[1]["data"]["writes"]
    assert any(w["session_id"] == friday["id"] and w["body"]["start"]["dateTime"] == "2026-10-16T21:07:00+02:00" for w in writes)
    from gymclaw.models import PlannedSession
    from gymclaw.services.replanning import replan_weeks
    with Session(engine) as db, db.begin():
        replan_weeks(db, {date(2026, 10, 12)}, now=SUNDAY)
        row = db.get(PlannedSession, friday["id"])
        assert row.user_locked and row.planned_start_at == datetime.fromisoformat(kept["data"]["to"])
    # "I can leave at 13:00": start is leave + commute.
    code, left = call("calendar", "move", "--session-id", friday["id"], "--request-id", "tg-6-move", "--leave-at", "2026-10-16T13:00:00+02:00")
    with Session(engine) as db:
        commute = get_profile(db).commute_to_gym_minutes
    assert code == 0 and datetime.fromisoformat(left["data"]["to"]) == datetime.fromisoformat("2026-10-16T13:00:00+02:00") + timedelta(minutes=commute)
    # A day the owner asks for wins over the rest-day rule (Thursday follows Wednesday); the warning says so.
    code, thursday = call("calendar", "move", "--session-id", friday["id"], "--request-id", "tg-7-move", "--day", "2026-10-15")
    assert code == 0 and thursday["data"]["to"].startswith("2026-10-15") and thursday["data"]["warnings"] == ["skips a rest day"]


def test_owner_adds_sessions_and_picks_workouts(engine, capsys):
    def call(*args):
        code = main(["--db-url", str(engine.url), *args, "--now", SUNDAY.isoformat()])
        return code, json.loads(capsys.readouterr().out)

    from gymclaw.models import PlannedSession
    from gymclaw.services.replanning import replan_weeks
    from gymclaw.services.templates import ExerciseSpec, Template, import_template
    with Session(engine) as db, db.begin():
        sessions = weekly.weekly_plan(db, "short", now=SUNDAY)["plan"]["sessions"]
        import_template(db, Template(id="legs", name="Legs", exercises=[ExerciseSpec(id="squat", name="Squat", role="squat", target_weight=100)]))
    # Naive times are the owner's local time. The extra session is kept beyond the weekly maximum.
    code, added = call("calendar", "add", "--to", "2026-10-17T10:00", "--template-id", "short", "--request-id", "tg-1-add")
    assert code == 0 and added["data"]["to"] == "2026-10-17T08:00:00+00:00" and added["data"]["warnings"] == ["outside your workout window (07:00–22:00)", "skips a rest day"]
    with Session(engine) as db, db.begin():
        replan_weeks(db, {date(2026, 10, 12)}, now=SUNDAY)
        assert db.get(PlannedSession, added["data"]["session_id"]).status in {"TENTATIVE", "COMMITTED"}
    # "Legs today instead": the session gets that workout and keeps it.
    code, legs = call("calendar", "set-workout", "--session-id", sessions[0]["id"], "--template-id", "legs", "--request-id", "tg-2-set")
    assert code == 0 and legs["data"]["to"] == "legs"
    with Session(engine) as db, db.begin():
        weekly.assign_rotation(db, now=SUNDAY)
        row = db.get(PlannedSession, sessions[0]["id"])
        assert row.user_locked and row.workout_template_id == "legs" and row.workout_plan_json["template"]["id"] == "legs"


def test_profile_window_change_moves_sessions_that_no_longer_fit(engine, capsys):
    with Session(engine) as db, db.begin():
        weekly.weekly_plan(db, "short", now=SUNDAY)
    assert main(["--db-url", str(engine.url), "profile", "update", "--data", '{"earliest_workout_start":"11:00"}', "--now", SUNDAY.isoformat()]) == 0
    changed = json.loads(capsys.readouterr().out)["data"]["replanning"]["changed"]
    assert changed and all(c["action"] == "moved" for c in changed)
    assert all(datetime.fromisoformat(c["to"]).astimezone(ZoneInfo("Europe/Berlin")).hour >= 11 for c in changed)
