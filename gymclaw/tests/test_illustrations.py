from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from pydantic import ValidationError
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.services.illustrations import catalog, for_exercise, public_assets
from gymclaw.services.set_parser import SetInput
from gymclaw.services.templates import ExerciseSpec, Template, import_template
from gymclaw.services.workout import start, log_set


def test_catalog_assets_and_no_fuzzy_matching():
    assert len(catalog()) == 302
    assert all(path.is_file() for path in public_assets().values())
    pose = for_exercise("Seated Cable Row")
    assert pose["equipment"] == "Cable" and Path(pose["telegram_png"]).is_file()
    assert pose["primary_muscle"] == "Back" and "license" not in pose
    assert for_exercise("some pulling machine") is None
    assert for_exercise("anything", "../../private") is None
    with pytest.raises(ValidationError):
        ExerciseSpec(id="bad", name="Bad", role="pull", target_weight=1, guide_id="../wrong")


def test_pose_follows_active_exercise_not_previous_set(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'pose.db'}")
    initialize(engine)
    now = datetime(2026, 10, 12, 18, tzinfo=timezone.utc)
    try:
        with Session(engine) as db, db.begin():
            import_template(db, Template(id="poses", name="Poses", exercises=[
                ExerciseSpec(id="press", name="Owner press", role="press", guide_id="bench-press", working_sets=1, target_weight=80),
                ExerciseSpec(id="row", name="Owner row", role="pull", guide_id="seated-row", working_sets=1, target_weight=60)]))
            data = start(db, "poses", now=now, request_id="start")["data"]
            assert data["active_exercise"]["illustration"]["guide_id"] == "bench-press"
            after = log_set(db, data["workout_id"], SetInput(weight=80, reps=9), now=now+timedelta(seconds=40), request_id="set")["data"]
            assert after["active_exercise"]["illustration"]["guide_id"] == "seated-row"
            assert after["active_exercise"]["target_weight"] == 60
    finally:
        engine.dispose()


def test_swap_alternatives_differ_in_equipment():
    from gymclaw.services.illustrations import similar
    # "Rear Delt Fly" and "Bent-Over Rear Delt Raise" are the same dumbbell movement; offer one of them.
    options = similar("cable-rear-delt-fly")
    assert len({o["equipment"] for o in options}) == len(options) == 2
