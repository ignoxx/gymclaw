from datetime import datetime, timedelta, timezone

import pytest
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import AgentEvent, NotificationJob, SetLog, WorkoutSession, WorkoutExercise
from gymclaw.services.errors import DomainError
from gymclaw.services.set_parser import SetInput, parse_set
from gymclaw.services.templates import ExerciseSpec, Template, get_template, import_template
from gymclaw.services.workout import current, log_set, rest_complete, start, transition

NOW = datetime(2026, 10, 12, 18, tzinfo=timezone.utc)


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'workout.db'}")
    initialize(engine)
    with Session(engine) as db, db.begin():
        import_template(db, Template(id="short", name="Short", exercises=[ExerciseSpec(id="bench", name="Bench", role="press", primary=True, working_sets=2, target_weight=80, warmup_weight=50, rest_seconds=100)]))
    yield engine
    engine.dispose()


def begin(engine):
    with Session(engine) as db, db.begin():
        return start(db, "short", now=NOW, request_id="start")["data"]["workout_id"]


def test_persistent_state_warmup_and_early_set_cancel(engine):
    workout_id = begin(engine)
    with Session(engine) as db, db.begin():
        initial = current(db, workout_id, now=NOW)
        assert initial["active_exercise"]["set_type"] == "WARMUP"
        assert initial["eta"] == (NOW + timedelta(seconds=220)).isoformat()
        log_set(db, workout_id, parse_set("50x8", set_type="WARMUP"), now=NOW + timedelta(seconds=30), request_id="warmup")
        first = log_set(db, workout_id, parse_set("80x9"), now=NOW + timedelta(seconds=60), request_id="set-1")
        job_id = first["data"]["rest_job"]["id"]
        assert first["data"]["status"] == "RESTING"
        assert first["data"]["active_exercise"]["set_number"] == 2
    with Session(engine) as db, db.begin():
        assert current(db, workout_id, now=NOW + timedelta(seconds=65))["rest_job"]["id"] == job_id
        result = log_set(db, workout_id, parse_set("80x10"), now=NOW + timedelta(seconds=80), request_id="set-2")
        assert result["data"]["status"] == "WORKOUT_COMPLETE"
        assert db.get(NotificationJob, job_id).status == "CANCELLED"
        assert result["data"]["rest_job"] is None
        assert len(db.scalars(select(SetLog)).all()) == 3
        assert len(db.scalars(select(SetLog).where(SetLog.set_type == "WORKING")).all()) == 2
        stale = rest_complete(db, job_id, now=NOW + timedelta(seconds=160), request_id="stale")
        assert stale["data"]["stale"]


def test_rest_due_exactly_and_retry(engine):
    workout_id = begin(engine)
    with Session(engine) as db, db.begin():
        log_set(db, workout_id, SetInput(weight=50, reps=8, set_type="WARMUP"), now=NOW, request_id="warmup")
        original = log_set(db, workout_id, SetInput(weight=80, reps=9), now=NOW, request_id="one")
        replay = log_set(db, workout_id, SetInput(weight=80, reps=9), now=NOW + timedelta(seconds=1), request_id="one")
        assert original == replay
        job = db.get(NotificationJob, original["data"]["rest_job"]["id"])
        assert job.due_at == NOW + timedelta(seconds=100)
        job_id = job.id
    with Session(engine) as db:
        with pytest.raises(DomainError, match="not reached"):
            rest_complete(db, job_id, now=NOW + timedelta(seconds=99), request_id="too-soon")
        db.rollback()
    with Session(engine) as db, db.begin():
        result = rest_complete(db, job_id, now=NOW + timedelta(seconds=100), request_id="due")
        assert result["data"]["status"] == "SET_ACTIVE"
        assert result["data"]["rest_job"] is None
        assert "Bench" in result["user_message_hint"]
        assert result == rest_complete(db, job_id, now=NOW + timedelta(seconds=101), request_id="due")
        assert db.get(NotificationJob, job_id).status == "FIRED"
        assert len(db.scalars(select(AgentEvent).where(AgentEvent.type == "workout.rest_completed")).all()) == 1


def test_guardrails_rollback_and_id_conflict(engine):
    workout_id = begin(engine)
    with Session(engine) as db:
        for action in [
            lambda: start(db, "short", now=NOW, request_id="duplicate-active"),
            lambda: start(db, "unknown", now=NOW, request_id="start"),
            lambda: transition(db, db.get(WorkoutSession, workout_id), "PLAN_UPDATED", NOW),
            lambda: log_set(db, workout_id, SetInput(weight=50, reps=8, set_type="WARMUP"), now=NOW - timedelta(seconds=1), request_id="past"),
        ]:
            with pytest.raises(DomainError):
                action()
            db.rollback()
        assert not db.scalars(select(SetLog)).all()
        assert db.get(WorkoutSession, workout_id).status == "SET_ACTIVE"


@pytest.mark.parametrize("text", ["80x9", "80 x 9", "80kg x9", "9 reps at 80", "80 KG × 9", "9 reps at 80kg", "9x80kg", "80kgx9 reps", "9x80"])
def test_set_parser(text):
    assert parse_set(text) == SetInput(weight=80, reps=9)


def test_set_parser_uses_expected_weight_when_order_is_unclear():
    assert parse_set("12x10", expected_weight=10) == SetInput(weight=10, reps=12)
    assert parse_set("12x10") == SetInput(weight=12, reps=10)
    assert parse_set("8x22,5") == SetInput(weight=22.5, reps=8)


def test_working_set_skips_pending_warmup(engine):
    workout_id = begin(engine)
    with Session(engine) as db, db.begin():
        result = log_set(db, workout_id, SetInput(weight=80, reps=9), now=NOW, request_id="straight-in")["data"]
        assert result["active_exercise"]["set_type"] == "WORKING"
        assert result["active_exercise"]["set_number"] == 2
        assert result["active_exercise"]["last_set"] == {"weight": 80, "reps": 9}


@pytest.mark.parametrize("text", ["80x9 then 80x10", "80", "9 reps", "-80x9", "80x9?", "80/9", "80kg x 9kg"])
def test_ambiguous_set_rejected(text):
    with pytest.raises(DomainError, match="What weight"):
        parse_set(text)


def test_template_edits_do_not_mutate_active_workout(engine):
    workout_id = begin(engine)
    with Session(engine) as db, db.begin():
        definition = get_template(db, "short").model_dump()
        definition["exercises"][0]["working_sets"] = 1
        definition["exercises"][0]["warmup_weight"] = 20
        import_template(db, Template.model_validate(definition))
        snapshot = current(db, workout_id, now=NOW)["active_exercise"]
        assert snapshot["working_sets"] == 2
        assert snapshot["target_weight"] == 50


def test_eta_learns_from_completed_set_timing(engine):
    from gymclaw.services.audit import finish
    workout_id = begin(engine)
    with Session(engine) as db, db.begin():
        log_set(db, workout_id, SetInput(weight=50, reps=8, set_type="WARMUP"), now=NOW + timedelta(seconds=30), request_id="warmup")
        log_set(db, workout_id, SetInput(weight=80, reps=10), now=NOW + timedelta(seconds=90), request_id="first")
        log_set(db, workout_id, SetInput(weight=80, reps=10), now=NOW + timedelta(seconds=240), request_id="second")
        finish(db, workout_id, now=NOW + timedelta(seconds=250), request_id="finish")
    with Session(engine) as db, db.begin():
        later = NOW + timedelta(days=2)
        result = start(db, "short", now=later, request_id="next")["data"]
        # Observed durations: 60s after warm-up and 50s after planned rest → mean 55s.
        assert result["eta"] == (later + timedelta(seconds=3 * 55 + 100)).isoformat()


def test_only_first_primary_gets_warmup(engine):
    with Session(engine) as db, db.begin():
        import_template(db, Template(id="two", name="Two", exercises=[ExerciseSpec(id="a", name="A", role="press", primary=True, working_sets=1, target_weight=10, warmup_weight=5), ExerciseSpec(id="b", name="B", role="pull", primary=True, working_sets=1, target_weight=10)]))
        result = start(db, "two", now=NOW, request_id="two")["data"]
        workout_id = result["workout_id"]
        log_set(db, workout_id, SetInput(weight=5, reps=8, set_type="WARMUP"), now=NOW, request_id="warmup")
        result = log_set(db, workout_id, SetInput(weight=10, reps=9), now=NOW, request_id="working")["data"]
        assert result["active_exercise"]["exercise_id"] == "b"
        assert result["active_exercise"]["set_type"] == "WORKING"
        assert len(db.scalars(select(WorkoutExercise)).all()) == 2
