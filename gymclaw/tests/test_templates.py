from pathlib import Path

import pytest
from alembic import command
from alembic.config import Config
from pydantic import ValidationError
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import NotificationJob, WorkoutSession, WorkoutExercise, SetLog
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


def test_calendar_upgrade_preserves_existing_rest_jobs(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'rest.db'}")
    config = Config("alembic.ini")
    with engine.begin() as connection:
        config.attributes["connection"] = connection
        command.upgrade(config, "f546f352a874")
        connection.exec_driver_sql("INSERT INTO workout_session (id,status,waited_for_equipment_count) VALUES ('w','RESTING',0)")
        connection.exec_driver_sql("INSERT INTO workout_exercise (id,workout_session_id,exercise_id,position,status,planned_working_sets,rep_min,rep_max,target_weight,rest_seconds) VALUES ('e','w','bench',0,'ACTIVE',3,8,10,80,150)")
        connection.exec_driver_sql("INSERT INTO set_log (id,workout_exercise_id,set_number,set_type,weight,reps,logged_at) VALUES ('s','e',1,'WORKING',80,9,'2026-10-01 10:00:00')")
        connection.exec_driver_sql("INSERT INTO notification_job (id,kind,workout_session_id,set_log_id,due_at,status,payload_json) VALUES ('j','REST','w','s','2026-10-01 10:02:30','PENDING','{}')")
    initialize(engine)
    with Session(engine) as db:
        job = db.get(NotificationJob, "j")
        assert job.workout_session_id == "w" and job.planned_session_id is None
        assert job.status == "PENDING" and job.set_log_id == "s"
        assert not db.connection().exec_driver_sql("PRAGMA foreign_key_check").all()
    engine.dispose()


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


def test_edit_one_exercise_without_reimport(tmp_path):
    from gymclaw.tests.test_cli import run
    run(tmp_path, "db", "init")
    run(tmp_path, "template", "import", "--file", "config/exercises.seed.json")
    edit = ("template", "edit-exercise", "--template-id", "upper-a")
    code, edited = run(tmp_path, *edit, "--exercise", "bench press", "--data", '{"target_weight":85,"working_sets":4}')
    bench = edited["data"]["template"]["exercises"][0]
    assert code == 0 and bench["target_weight"] == 85 and bench["working_sets"] == 4
    code, added = run(tmp_path, "template", "add-exercise", "--template-id", "upper-a", "--exercise", "face-pull", "--position", "2")
    assert code == 0 and [e["id"] for e in added["data"]["template"]["exercises"]] == ["bench", "row", "face-pull", "pec-deck", "lateral-raise"]
    code, removed = run(tmp_path, "template", "remove-exercise", "--template-id", "upper-a", "--exercise", "Pec Deck")
    assert code == 0 and "pec-deck" not in [e["id"] for e in removed["data"]["template"]["exercises"]]
    code, missing = run(tmp_path, *edit, "--exercise", "squat", "--data", "{}")
    assert code == 1 and missing["error"]["code"] == "EXERCISE_NOT_FOUND" and "Bench Press" in missing["error"]["message"]
