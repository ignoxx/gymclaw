from datetime import datetime, timedelta

import pytest
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import AgentEvent, ExerciseProgression, NotificationJob, WorkoutExercise
from gymclaw.services.adaptation import machine_busy, machine_free, skip_exercise, substitute
from gymclaw.services.audit import finish, get_audit
from gymclaw.services.errors import DomainError
from gymclaw.services.set_parser import SetInput
from gymclaw.services.templates import ExerciseSpec, Template, get_template, import_template
from gymclaw.services.workout import current, log_set, start
from gymclaw.tests.test_workout_state import NOW


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'adapt.db'}")
    initialize(engine)
    template = Template(id="adapt", name="Adapt", exercises=[
        ExerciseSpec(id="bench", name="Bench", role="press", primary=True, working_sets=1, target_weight=80, warmup_weight=50),
        ExerciseSpec(id="fly", name="Fly", role="fly", working_sets=2, target_weight=50, rep_max=12, substitutes=["db-fly"]),
        ExerciseSpec(id="raise", name="Raise", role="delts", working_sets=1, target_weight=10, increment=1),
    ], alternatives=[ExerciseSpec(id="db-fly", name="DB Fly", role="fly", working_sets=2, target_weight=12, rep_max=12, increment=1)])
    with Session(engine) as db, db.begin():
        import_template(db, template)
    yield engine
    engine.dispose()


def bench_done(db):
    workout_id = start(db, "adapt", now=NOW, request_id="start")["data"]["workout_id"]
    log_set(db, workout_id, SetInput(weight=50, reps=8, set_type="WARMUP"), now=NOW, request_id="warmup")
    log_set(db, workout_id, SetInput(weight=80, reps=10), now=NOW + timedelta(seconds=40), request_id="bench")
    return workout_id


def test_reorder_retry_deferred_and_audit(engine):
    with Session(engine) as db, db.begin():
        workout_id = bench_done(db)
        result = machine_busy(db, workout_id, now=NOW + timedelta(seconds=60), request_id="busy")
        deferred_id = result["data"]["deferred_exercise_id"]
        assert result["data"]["active_exercise"]["exercise_id"] == "raise"
        assert result["data"]["substitution_options"] == ["db-fly"]
        assert db.get(WorkoutExercise, deferred_id).status == "DEFERRED"
        # Finishing an exercise starts the next one without a rest timer.
        assert not db.scalar(select(NotificationJob).where(NotificationJob.status == "PENDING"))
        log_set(db, workout_id, SetInput(weight=10, reps=10), now=NOW + timedelta(seconds=100), request_id="raise")
    with Session(engine) as db:
        with pytest.raises(DomainError, match="Resolve deferred"):
            finish(db, workout_id, now=NOW + timedelta(seconds=110), request_id="bad-finish")
        db.rollback()
    with Session(engine) as db, db.begin():
        result = machine_free(db, workout_id, deferred_id, now=NOW + timedelta(seconds=120), request_id="free")
        assert result["data"]["active_exercise"]["exercise_id"] == "fly"
        for number in range(2):
            log_set(db, workout_id, SetInput(weight=50, reps=12), now=NOW + timedelta(seconds=160 + 120 * number), request_id=f"fly-{number}")
        result = finish(db, workout_id, now=NOW + timedelta(seconds=300), request_id="finish")
        assert result["data"]["planned_sets"] == result["data"]["completed_sets"] == 4
        assert result["data"]["warmup_sets"] == 1
        assert result["data"]["equipment_waits"] == 1
        assert result["data"]["progression_count"] == 3
        assert db.get(ExerciseProgression, "bench").next_weight == 82.5
        assert db.get(ExerciseProgression, "fly").next_weight == 52.5
        assert result == finish(db, workout_id, now=NOW + timedelta(seconds=310), request_id="finish")
        assert get_audit(db, workout_id) == result["data"]
        second = start(db, "adapt", now=NOW + timedelta(days=2), request_id="next")
        assert second["data"]["active_exercise"]["target_weight"] == 50  # warm-up unchanged
        new_id = second["data"]["active_exercise"]["id"]
        assert db.get(WorkoutExercise, new_id).target_weight == 82.5
        assert len(db.scalars(select(AgentEvent).where(AgentEvent.type == "planning.replan_required")).all()) == 1


def test_substitute_only_remaining_volume_and_hold_partial_target(engine):
    with Session(engine) as db, db.begin():
        workout_id = bench_done(db)
        logged = log_set(db, workout_id, SetInput(weight=50, reps=12), now=NOW + timedelta(seconds=100), request_id="fly-1")
        fly_id = logged["data"]["active_exercise"]["id"]
        result = machine_busy(db, workout_id, now=NOW + timedelta(seconds=120), request_id="busy")
        assert result["data"]["active_exercise"]["exercise_id"] == "raise"
        replacement = substitute(db, workout_id, fly_id, "db-fly", now=NOW + timedelta(seconds=130), request_id="swap")
        replacement_id = replacement["data"]["replacement_exercise_id"]
        assert db.get(WorkoutExercise, replacement_id).planned_working_sets == 1
        log_set(db, workout_id, SetInput(weight=10, reps=10), now=NOW + timedelta(seconds=160), request_id="raise")
        assert current(db, workout_id, now=NOW + timedelta(seconds=160))["active_exercise"]["exercise_id"] == "db-fly"
        log_set(db, workout_id, SetInput(weight=12, reps=12), now=NOW + timedelta(seconds=200), request_id="db-fly")
        audit = finish(db, workout_id, now=NOW + timedelta(seconds=210), request_id="finish")["data"]
        assert audit["planned_sets"] == audit["completed_sets"] == 4
        assert audit["substitutions"] == 1
        assert db.get(ExerciseProgression, "db-fly").next_weight == 12
        assert db.get(ExerciseProgression, "fly") is None


def test_dependency_blocks_unsafe_reordering_and_explicit_skips(engine):
    with Session(engine) as db, db.begin():
        import_template(db, Template(id="dependent", name="Dependent", exercises=[ExerciseSpec(id="a", name="A", role="press", target_weight=10, working_sets=1), ExerciseSpec(id="b", name="B", role="press", target_weight=10, working_sets=1, requires_completed=["a"])]))
        workout_id = start(db, "dependent", now=NOW, request_id="dependent")["data"]["workout_id"]
        blocked = machine_busy(db, workout_id, now=NOW, request_id="busy")["data"]
        assert blocked["active_exercise"] is None
        queue = blocked["queue"]
        skipped = skip_exercise(db, workout_id, queue[0]["id"], reason="equipment unavailable", now=NOW, request_id="skip-a")["data"]
        assert skipped["active_exercise"] is None
        skip_exercise(db, workout_id, queue[1]["id"], reason="dependency skipped", now=NOW, request_id="skip-b")
        result = finish(db, workout_id, now=NOW, request_id="finish")["data"]
        assert result["skips"] == 2 and result["completed_sets"] == 0
        assert not db.scalars(select(ExerciseProgression)).all()


def test_eta_includes_deferred_volume_and_elapsed_time(engine):
    with Session(engine) as db, db.begin():
        workout_id = bench_done(db)
        blocked = machine_busy(db, workout_id, now=NOW + timedelta(seconds=60), request_id="busy")["data"]
        assert any(e["exercise_id"] == "fly" and e["remaining_sets"] == 2 for e in blocked["queue"])
        # Cancelling rest may improve ETA; unresolved sets still count, not disappear.
        assert (datetime.fromisoformat(blocked["eta"]) - (NOW + timedelta(seconds=60))).total_seconds() >= 3 * 40
        later = current(db, workout_id, now=NOW + timedelta(seconds=120))
        assert later["eta"] > blocked["eta"]


def test_replacement_prerequisites_are_preserved(engine):
    with Session(engine) as db, db.begin():
        raw = get_template(db, "adapt").model_dump()
        raw["alternatives"][0]["requires_completed"] = ["raise"]
        import_template(db, Template.model_validate(raw))
        workout_id = bench_done(db)
        exercise_id = current(db, workout_id, now=NOW)["active_exercise"]["id"]
        result = substitute(db, workout_id, exercise_id, "db-fly", now=NOW + timedelta(seconds=50), request_id="swap")["data"]
        assert result["active_exercise"]["exercise_id"] == "raise"
        replacement = db.get(WorkoutExercise, result["replacement_exercise_id"])
        assert replacement.status == "PENDING"
        assert replacement.config_json["requires_completed"] == ["raise"]


def test_invalid_substitution_preserves_state(engine):
    with Session(engine) as db, db.begin():
        workout_id = bench_done(db)
        exercise_id = current(db, workout_id, now=NOW)["active_exercise"]["id"]
    with Session(engine) as db:
        with pytest.raises(DomainError, match="template alternative or a catalog exercise"):
            substitute(db, workout_id, exercise_id, "bench", now=NOW + timedelta(seconds=50), request_id="bad")
        db.rollback()
        assert db.get(WorkoutExercise, exercise_id).status == "ACTIVE"
