from datetime import datetime, timezone

import pytest
from sqlalchemy import inspect, text
from alembic.autogenerate import compare_metadata
from alembic.migration import MigrationContext
from sqlalchemy.exc import IntegrityError, StatementError
from sqlalchemy.orm import Session

from gymclaw.db import make_engine, initialize
from gymclaw.models import Base, UserProfile, AgentEvent, SetLog


def test_migrations_and_restart(tmp_path):
    url = f"sqlite:///{tmp_path / 'state.db'}"
    engine = make_engine(url)
    initialize(engine)
    initialize(engine)
    assert len(inspect(engine).get_table_names()) == 11
    with engine.connect() as connection:
        assert compare_metadata(MigrationContext.configure(connection), Base.metadata) == []
    with Session(engine) as db, db.begin():
        db.add(UserProfile(prep_minutes=23))
        db.add(AgentEvent(type="test", created_at=datetime(2026, 10, 1, tzinfo=timezone.utc)))
    engine.dispose()
    engine = make_engine(url)
    with Session(engine) as db:
        assert db.get(UserProfile, 1).prep_minutes == 23
        assert db.query(AgentEvent).one().created_at.tzinfo == timezone.utc
        assert db.execute(text("SELECT version_num FROM alembic_version")).scalar()


def test_foreign_keys_and_naive_timestamps(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'state.db'}")
    initialize(engine)
    with Session(engine) as db:
        db.add(SetLog(workout_exercise_id="missing", set_number=1, set_type="WORKING", weight=80, reps=9))
        with pytest.raises(IntegrityError):
            db.commit()
        db.rollback()
        db.add(AgentEvent(type="test", created_at=datetime(2026, 10, 1)))
        with pytest.raises(StatementError, match="Timestamp must include timezone"):
            db.commit()
