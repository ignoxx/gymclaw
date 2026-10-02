from datetime import datetime, timezone

import pytest
from sqlalchemy import func, select
from sqlalchemy.orm import Session

from gymclaw.cli import main
from gymclaw.db import initialize, make_engine
from gymclaw.models import AgentEvent, RuntimeSettings, WorkoutSession
from gymclaw.services import onboarding
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import update_profile
from gymclaw.services.templates import Template, import_template

NOW = datetime(2026, 10, 2, 12, tzinfo=timezone.utc)


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'onboarding.db'}")
    initialize(engine)
    yield engine
    engine.dispose()


def template(source="user", weight=40):
    return Template(id="owner-plan", name="Full body", source=source,
        exercises=[{"id": "row", "name": "Row", "role": "pull", "target_weight": weight}])


def confirm_profile(db):
    onboarding.update(db, "set-goal", goal="Build strength", now=NOW, request_id="goal")
    current = onboarding.status(db)
    return onboarding.update(db, "confirm-profile", expected_fingerprint=current["profile_fingerprint"], now=NOW, request_id="profile")


def test_defaults_are_unconfirmed_and_finish_cannot_grant_authority(engine):
    with Session(engine) as db, db.begin():
        assert onboarding.status(db)["step"] == "GOAL"
        with pytest.raises(DomainError, match="before finishing"):
            onboarding.update(db, "finish", now=NOW, request_id="too-soon")
        confirm_profile(db)
        import_template(db, template())
        current = onboarding.status(db)
        result = onboarding.update(db, "confirm-template", template_id="owner-plan",
            expected_fingerprint=current["templates"][0]["fingerprint"], now=NOW, request_id="template")
        assert result["data"]["step"] == "REVIEW"
        finished = onboarding.update(db, "finish", expected_fingerprint=result["data"]["review_fingerprint"], now=NOW, request_id="finish")
        assert finished["data"]["ready"] and not finished["data"]["calendar_writes_enabled"]
        assert db.get(RuntimeSettings, 1) is None
        assert db.scalar(select(func.count()).select_from(WorkoutSession)) == 0
    with Session(engine) as db:
        assert onboarding.status(db)["step"] == "READY"


def test_profile_and_template_changes_invalidate_review(engine):
    with Session(engine) as db, db.begin():
        confirmed = confirm_profile(db)
        update_profile(db, {"prep_minutes": 25})
        assert onboarding.status(db)["step"] == "PROFILE"
        with pytest.raises(DomainError) as error:
            onboarding.update(db, "confirm-profile", expected_fingerprint=confirmed["data"]["profile_fingerprint"], now=NOW, request_id="stale")
        assert error.value.code == "ONBOARDING_REVIEW_STALE"
        onboarding.update(db, "confirm-profile", expected_fingerprint=onboarding.status(db)["profile_fingerprint"], now=NOW, request_id="fresh")
        import_template(db, template())
        old = onboarding.status(db)["templates"][0]["fingerprint"]
        import_template(db, template(weight=45))
        with pytest.raises(DomainError) as error:
            onboarding.update(db, "confirm-template", template_id="owner-plan", expected_fingerprint=old, now=NOW, request_id="stale-plan")
        assert error.value.code == "ONBOARDING_REVIEW_STALE"


def test_idempotency_and_demo_rejection(engine):
    with Session(engine) as db, db.begin():
        a = onboarding.update(db, "set-goal", goal="Fitness", now=NOW, request_id="one")
        assert onboarding.update(db, "set-goal", goal="Fitness", now=NOW, request_id="one") == a
        assert db.scalar(select(func.count()).select_from(AgentEvent)) == 1
        with pytest.raises(DomainError) as error:
            onboarding.update(db, "set-goal", goal="Strength", now=NOW, request_id="one")
        assert error.value.code == "IDEMPOTENCY_CONFLICT"
        onboarding.update(db, "confirm-profile", expected_fingerprint=onboarding.status(db)["profile_fingerprint"], now=NOW, request_id="review")
        import_template(db, template(source="demo"))
        with pytest.raises(DomainError) as error:
            onboarding.update(db, "confirm-template", template_id="owner-plan", expected_fingerprint=onboarding.status(db)["templates"][0]["fingerprint"], now=NOW, request_id="demo")
        assert error.value.code == "ONBOARDING_REAL_TEMPLATE_REQUIRED"


def test_onboarding_cli_envelope(engine, capsys):
    assert main(["--db-url", str(engine.url), "onboarding", "status"]) == 0
    import json
    result = json.loads(capsys.readouterr().out)
    assert result["data"]["step"] == "GOAL" and result["user_message_hint"]
