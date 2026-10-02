import json
from datetime import timedelta

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import make_engine
from gymclaw.models import ExerciseProgression, NotificationJob, PlannedSession, SetLog
from gymclaw.tests.test_cli import run
from gymclaw.tests.test_workout_state import NOW


def test_cli_full_fake_workout_and_restart(tmp_path):
    assert run(tmp_path, "db", "init")[1]["ok"]
    template = tmp_path / "short.json"
    template.write_text(json.dumps({"id": "demo", "name": "Demo", "source": "demo_fixture", "exercises": [
        {"id": "bench", "name": "Bench", "guide_id": "bench-press", "role": "press", "primary": True, "working_sets": 2, "target_weight": 80, "warmup_weight": 50},
        {"id": "fly", "name": "Fly", "guide_id": "cable-fly", "role": "fly", "working_sets": 1, "target_weight": 50, "substitutes": ["db-fly"]},
        {"id": "raise", "name": "Raise", "guide_id": "lateral-raise", "role": "delts", "working_sets": 1, "target_weight": 10}],
        "alternatives": [{"id": "db-fly", "name": "DB Fly", "guide_id": "dumbbell-fly", "role": "fly", "working_sets": 1, "target_weight": 12}]}))
    unillustrated = tmp_path / "bare.json"
    unillustrated.write_text(json.dumps({"id": "bare", "name": "Bare", "exercises": [{"id": "x", "name": "Mystery machine", "role": "x", "target_weight": 0}]}))
    assert run(tmp_path, "template", "import", "--file", str(unillustrated))[1]["error"]["code"] == "ILLUSTRATION_REQUIRED"
    assert run(tmp_path, "template", "import", "--file", str(template))[1]["ok"]
    assert run(tmp_path, "template", "get", "--template-id", "demo")[1]["data"]["source"] == "demo_fixture"
    plan = run(tmp_path, "schedule", "plan-week", "--week-start", "2026-10-12", "--now", "2026-10-11T12:00:00+02:00")[1]["data"]
    planned_id = plan["sessions"][0]["id"]
    result = run(tmp_path, "workout", "start", "--template-id", "demo", "--planned-session-id", planned_id, "--now", NOW.isoformat(), "--request-id", "start")[1]
    workout_id = result["data"]["workout_id"]
    assert result["data"]["active_exercise"]["set_type"] == "WARMUP"

    def command(operation, seconds, request_id, *args):
        code, response = run(tmp_path, "workout", operation, "--workout-id", workout_id, "--now", (NOW + timedelta(seconds=seconds)).isoformat(), "--request-id", request_id, *args)
        assert code == 0, response
        return response

    command("log-set", 30, "warmup", "--text", "50x8", "--set-type", "WARMUP")
    first = command("log-set", 60, "bench-1", "--text", "80x10")
    assert first["data"]["status"] == "RESTING"
    assert first == command("log-set", 61, "bench-1", "--text", "80x10")
    assert run(tmp_path, "notifications", "pending")[1]["data"][0]["id"] == first["data"]["rest_job"]["id"]
    due = run(tmp_path, "notifications", "due", "--now", (NOW + timedelta(seconds=210)).isoformat())[1]
    assert due["data"]["processed"] == 1
    assert "Bench" in due["user_message_hint"] and "Rest over" in due["user_message_hint"]
    assert run(tmp_path, "notifications", "due", "--now", (NOW + timedelta(seconds=210)).isoformat())[1]["data"]["processed"] == 0
    command("log-set", 250, "bench-2", "--weight", "80", "--reps", "10")
    busy = command("machine-busy", 260, "busy")
    fly_id = busy["data"]["deferred_exercise_id"]
    assert busy["data"]["active_exercise"]["exercise_id"] == "raise"
    command("log-set", 300, "raise", "--text", "10x10")
    code, blocked = run(tmp_path, "workout", "finish", "--workout-id", workout_id, "--now", (NOW + timedelta(seconds=310)).isoformat(), "--request-id", "bad-finish")
    assert code == 1 and blocked["error"]["code"] == "UNRESOLVED_EXERCISES"
    retry = command("machine-busy", 310, "still-busy", "--exercise-id", fly_id)
    assert retry["data"]["substitution_options"] == ["db-fly"]
    swapped = command("substitute", 320, "swap", "--exercise-id", fly_id, "--substitute-id", "db-fly")
    assert swapped["data"]["active_exercise"]["exercise_id"] == "db-fly"
    command("log-set", 360, "db-fly", "--text", "12x10")
    final = command("finish", 380, "finish")
    assert final["data"]["completed_sets"] == final["data"]["planned_sets"] == 4
    assert final["data"]["substitutions"] == 1
    assert final["data"]["equipment_waits"] == 2
    assert final == command("finish", 390, "finish")
    assert run(tmp_path, "audit", "workout", "--workout-id", workout_id)[1]["data"] == final["data"]
    assert run(tmp_path, "workout", "current", "--workout-id", workout_id, "--now", (NOW + timedelta(seconds=400)).isoformat())[1]["data"]["status"] == "PLAN_UPDATED"
    inbox = run(tmp_path, "events", "pending")[1]["data"]
    actionable = next(e for e in inbox if e["type"] == "planning.replan_required")
    assert run(tmp_path, "events", "ack", "--event-id", actionable["id"], "--now", (NOW + timedelta(seconds=400)).isoformat())[1]["ok"]
    assert all(e["id"] != actionable["id"] for e in run(tmp_path, "events", "pending")[1]["data"])
    engine = make_engine(f"sqlite:///{tmp_path / 'state.db'}")
    with Session(engine) as db:
        assert db.get(PlannedSession, planned_id).status == "COMPLETED"
        assert db.get(ExerciseProgression, "bench").next_weight == 82.5
        assert len(db.scalars(select(SetLog)).all()) == 5
        assert not db.scalars(select(NotificationJob).where(NotificationJob.status == "PENDING")).all()
    engine.dispose()


def test_cli_workout_errors_are_json(tmp_path):
    run(tmp_path, "db", "init")
    for args, expected in [
        (("workout", "start", "--template-id", "missing", "--request-id", "start"), "INVALID_INPUT"),
        (("workout", "current", "--workout-id", "missing"), "WORKOUT_NOT_FOUND"),
        (("workout", "log-set", "--workout-id", "missing", "--text", "80x9", "--weight", "80", "--request-id", "log"), "INVALID_INPUT"),
        (("workout", "log-set", "--workout-id", "missing", "--text", "80x9 maybe 10", "--request-id", "log"), "WORKOUT_NOT_FOUND"),
        (("workout", "finish", "--workout-id", "missing"), "INVALID_INPUT"),
    ]:
        code, response = run(tmp_path, *args)
        assert code == 1 and response["error"]["code"] == expected
