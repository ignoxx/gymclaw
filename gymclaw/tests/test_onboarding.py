from datetime import datetime, timezone
import json

import pytest
from sqlalchemy import func, select
from sqlalchemy.orm import Session

from gymclaw.cli import main
from gymclaw.db import initialize, make_engine
from gymclaw.models import AgentEvent, RuntimeSettings, WorkoutSession
from gymclaw.services import onboarding
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile, update_profile
from gymclaw.services.templates import Template, import_template

NOW = datetime(2026, 10, 2, 12, tzinfo=timezone.utc)
PROFILE_ANSWERS = (
    {"schedule": "3x, Mon/Wed/Fri", "session_length": "60 min", "time_window": "10:30 to 22:00", "travel": "10 min prep, 15 min drive"},
    {"weekly_target_sessions": 3, "weekdays_allowed": [0, 2, 4], "preferred_workout_minutes": 60,
     "earliest_workout_start": "10:30", "latest_workout_finish": "22:00", "prep_minutes": 10, "commute_to_gym_minutes": 15,
     "gym_address": "Example Str. 1, Berlin"},
)


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'onboarding.db'}")
    initialize(engine)
    yield engine
    engine.dispose()


def template(template_id="owner-plan", source="user", weight=40):
    return Template(id=template_id, name=template_id.title(), source=source,
        exercises=[{"id": "row", "name": "Row", "role": "pull", "guide_id": "seated-row", "target_weight": weight}])


def interview(db):
    onboarding.update(db, "answer", answers={"goal": "Build muscle", "experience": "2 years", "equipment": "full gym", "limitations": "none"}, now=NOW, request_id="basics")
    return onboarding.update(db, "answer", answers=PROFILE_ANSWERS[0], profile=PROFILE_ANSWERS[1], now=NOW, request_id="schedule")


def test_interview_is_mandatory_and_defaults_never_count(engine):
    with Session(engine) as db, db.begin():
        current = onboarding.status(db)
        assert current["step"] == "INTERVIEW" and current["next_question"] == "goal"
        assert len(current["missing"]) == len(onboarding.QUESTIONS)
        with pytest.raises(DomainError) as error:
            onboarding.update(db, "answer", answers={"schedule": "3x a week"}, now=NOW, request_id="no-profile")
        assert error.value.code == "ONBOARDING_PROFILE_REQUIRED"
        with pytest.raises(DomainError, match="Finish the interview"):
            onboarding.update(db, "finish", now=NOW, request_id="too-soon")
        assert interview(db)["data"]["step"] == "PLAN"
        profile = get_profile(db)
        assert profile.weekly_max_sessions == 3 and profile.minimum_workout_minutes == 45 and profile.commute_home_minutes == 15


def test_plan_review_finish_grants_no_authority_and_stays_ready(engine):
    with Session(engine) as db, db.begin():
        interview(db)
        import_template(db, template("push"))
        import_template(db, template("pull"))
        result = onboarding.update(db, "confirm-plan", template_ids=["push", "pull"], now=NOW, request_id="plan")["data"]
        assert result["step"] == "REVIEW" and result["split"] == ["push", "pull"]
        finished = onboarding.update(db, "finish", expected_fingerprint=result["review_fingerprint"], now=NOW, request_id="finish")["data"]
        assert finished["ready"] and not finished["calendar_writes_enabled"]
        assert db.get(RuntimeSettings, 1) is None
        assert db.scalar(select(func.count()).select_from(WorkoutSession)) == 0
        # Later tweaks (e.g. a sleep floor) don't send the owner back into setup.
        update_profile(db, {"earliest_workout_start": "11:00"})
        import_template(db, template("push", weight=45))
        assert onboarding.status(db)["step"] == "READY"


def test_idempotency_and_demo_rejection(engine):
    with Session(engine) as db, db.begin():
        a = onboarding.update(db, "answer", answers={"goal": "Fitness"}, now=NOW, request_id="one")
        assert onboarding.update(db, "answer", answers={"goal": "Fitness"}, now=NOW, request_id="one") == a
        assert db.scalar(select(func.count()).select_from(AgentEvent)) == 1
        with pytest.raises(DomainError) as error:
            onboarding.update(db, "answer", answers={"goal": "Strength"}, now=NOW, request_id="one")
        assert error.value.code == "IDEMPOTENCY_CONFLICT"
        import_template(db, template(source="demo"))
        with pytest.raises(DomainError) as error:
            onboarding.update(db, "confirm-plan", template_ids=["owner-plan"], now=NOW, request_id="demo")
        assert error.value.code == "ONBOARDING_REAL_TEMPLATE_REQUIRED"


def test_onboarding_cli_envelope(engine, capsys):
    assert main(["--db-url", str(engine.url), "onboarding", "status"]) == 0
    result = json.loads(capsys.readouterr().out)
    assert result["data"]["step"] == "INTERVIEW" and result["user_message_hint"].startswith("What's your main goal")
    assert main(["--db-url", str(engine.url), "onboarding", "answer", "--answers", '{"experience": "new"}', "--request-id", "cli"]) == 0
    assert json.loads(capsys.readouterr().out)["data"]["answers"] == {"experience": "new"}
