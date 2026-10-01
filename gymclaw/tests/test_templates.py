from pathlib import Path

import pytest
from alembic import command
from alembic.config import Config
from pydantic import ValidationError
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import WorkoutSession, WorkoutExercise, SetLog
from gymclaw.services.templates import Template, get_template, import_template


def test_template_roundtrip_and_validation(tmp_path):
    template = Template.model_validate_json(Path("config/exercises.seed.json").read_text())
    engine = make_engine(f"sqlite:///{tmp_path / 'state.db'}")
    initialize(engine)
    with Session(engine) as db, db.begin():
        import_template(db, template)
    with Session(engine) as db:
        assert get_template(db, template.id) == template
    raw = template.model_dump()
    for change in ["warmup", "dependency", "role", "duplicate"]:
        modified = template.model_dump()
        if change == "warmup":
            modified["exercises"][0]["warmup_weight"] = None
        elif change == "dependency":
            modified["exercises"][0]["requires_completed"] = ["pec-deck"]
        elif change == "role":
            modified["alternatives"][0]["role"] = "press"
        else:
            modified["alternatives"][0]["id"] = raw["exercises"][0]["id"]
        with pytest.raises(ValidationError):
            Template.model_validate(modified)


def test_upgrade_preserves_existing_workout_and_logs(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'old.db'}")
    config = Config("alembic.ini")
    with engine.begin() as connection:
        config.attributes["connection"] = connection
        command.upgrade(config, "227579a5c228")
        connection.exec_driver_sql("INSERT INTO workout_session (id,status,waited_for_equipment_count) VALUES ('w','SET_ACTIVE',0)")
        connection.exec_driver_sql("INSERT INTO workout_exercise (id,workout_session_id,exercise_id,position,status,planned_working_sets,rep_min,rep_max,target_weight,rest_seconds) VALUES ('e','w','bench',0,'ACTIVE',3,8,10,80,150)")
        connection.exec_driver_sql("INSERT INTO set_log (id,workout_exercise_id,set_number,set_type,weight,reps,logged_at) VALUES ('s','e',1,'WORKING',80,9,'2026-10-01 10:00:00')")
    initialize(engine)
    initialize(engine)
    with Session(engine) as db:
        assert db.get(WorkoutSession, "w").template_snapshot == {}
        assert db.get(WorkoutExercise, "e").config_json == {}
        assert db.get(SetLog, "s").reps == 9
        assert db.connection().exec_driver_sql("PRAGMA foreign_keys").scalar() == 1
