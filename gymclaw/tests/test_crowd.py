from datetime import datetime, timedelta, timezone
import json

import pytest
from pydantic import ValidationError
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import CrowdFeedback, CrowdObservation, CrowdSourceState, LearnedPreference, WorkoutSession
from gymclaw.providers.crowd import CrowdReading, FixtureCrowdProvider
from gymclaw.providers.mysports import MySportsProvider
from gymclaw.services import crowd
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import update_profile
from gymclaw.services.scheduling import schedule_week

NOW = datetime(2026, 10, 14, 18, tzinfo=timezone.utc)  # Wednesday 20:00 Berlin.


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'crowd.db'}")
    initialize(engine)
    yield engine
    engine.dispose()


def provider(count):
    return FixtureCrowdProvider(CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=count))


def visit(db, at):
    row = WorkoutSession(status="PLAN_UPDATED", started_at=at, arrived_at=at, completed_at=at + timedelta(hours=1),
        template_snapshot={"id": "historical-test-visit"})
    db.add(row); db.flush()
    return row.id


class Response:
    status_code = 200

    def __init__(self, body):
        self.body = body

    def json(self):
        return self.body


class Transport:
    def __init__(self, body):
        self.body = body
        self.calls = []

    def request(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        return Response(self.body)


@pytest.mark.parametrize("source", ["env", "file"])
def test_private_gym_config_without_hardcoded_fallback(tmp_path, monkeypatch, source):
    monkeypatch.chdir(tmp_path)
    for name in ("GYMCLAW_MYSPORTS_STUDIO_ID", "GYMCLAW_MYSPORTS_TENANT", "GYMCLAW_CROWD_CONFIG"):
        monkeypatch.delenv(name, raising=False)
    with pytest.raises(DomainError) as error:
        MySportsProvider.from_environment()
    assert error.value.code == "GYM_API_NOT_CONFIGURED"
    if source == "env":
        monkeypatch.setenv("GYMCLAW_MYSPORTS_STUDIO_ID", "1234567890")
        monkeypatch.setenv("GYMCLAW_MYSPORTS_TENANT", "fixture-tenant")
    else:
        (tmp_path / "data").mkdir()
        (tmp_path / "data/crowd-config.json").write_text(json.dumps({"studio_id": "1234567890", "tenant": "fixture-tenant"}))
    transport = Transport({"value": 7})
    assert MySportsProvider.from_environment(transport=transport).get_reading().raw_value == 7
    assert transport.calls[0][1].endswith("/1234567890/utilization/v2/active-checkin")


def test_invalid_private_gym_config_withholds_values(tmp_path, monkeypatch):
    monkeypatch.setenv("GYMCLAW_CROWD_CONFIG", str(tmp_path / "private.json"))
    monkeypatch.delenv("GYMCLAW_MYSPORTS_STUDIO_ID", raising=False)
    monkeypatch.delenv("GYMCLAW_MYSPORTS_TENANT", raising=False)
    (tmp_path / "private.json").write_text('{"studio_id":"private-invalid-value","tenant":"fixture-tenant"}')
    with pytest.raises(DomainError) as error:
        MySportsProvider.from_environment()
    assert error.value.code == "GYM_API_NOT_CONFIGURED"
    assert "private-invalid-value" not in str(error.value)


def test_mysports_observed_public_contract_no_session_cookie():
    transport = Transport({"value": 7})  # Captured public contract; not live request.
    reading = MySportsProvider(studio_id="1234567890", tenant="fixture-tenant", transport=transport).get_reading()
    assert reading.raw_value == 7 and reading.normalized_value is None
    assert reading.freshness_seconds is None
    method, url, kwargs = transport.calls[0]
    assert method == "GET" and url.endswith("/1234567890/utilization/v2/active-checkin")
    assert kwargs["headers"]["x-tenant"] == "fixture-tenant"
    assert "Cookie" not in kwargs["headers"] and "cookies" not in kwargs
    assert kwargs["allow_redirects"] is False and kwargs["timeout"] == 10


@pytest.mark.parametrize("body", [{}, {"value": -1}, {"value": True}, {"value": "7"}, {"value": 7.5}, {"value": None}])
def test_invalid_count_never_stored(body):
    with pytest.raises(DomainError, match="count unavailable or invalid"):
        MySportsProvider(studio_id="1234567890", tenant="fixture-tenant", transport=Transport(body)).get_reading()


def test_unknown_freshness_change_window_and_restart(engine):
    with Session(engine) as db, db.begin():
        first = crowd.poll(db, provider(7), now=NOW)
        again = crowd.poll(db, provider(7), now=NOW + timedelta(minutes=15))
        changed = crowd.poll(db, provider(9), now=NOW + timedelta(minutes=30))
        assert first["data"]["provider_freshness_seconds"] is None
        assert again["data"]["metadata"]["unchanged_for_seconds"] == 900
        assert changed["data"]["metadata"]["possible_change_window"] == {"after": (NOW + timedelta(minutes=15)).isoformat(), "by": (NOW + timedelta(minutes=30)).isoformat()}
    with Session(engine) as db:
        assert len(db.scalars(select(CrowdObservation)).all()) == 3
        health = crowd.source_health(db, now=NOW + timedelta(minutes=31))["sources"][0]
        assert health["retrieval_age_seconds"] == 60
        assert health["provider_freshness_known"] is False
        prediction = crowd.CrowdModel(db, now=NOW + timedelta(minutes=31)).predict(NOW + timedelta(minutes=31))
        assert prediction["score"] is None  # Count alone is not personalized occupancy.
        assert prediction["features"]["GYM_API"]["raw_value"] == 9


def test_unavailable_source_keeps_history_and_health(engine):
    class Broken:
        source = "GYM_API"
        provider_id = "fixture:GYM_API"
        demo = True
        def get_reading(self):
            raise DomainError("GYM_API_UNAVAILABLE", "No source")
    with Session(engine) as db, db.begin():
        crowd.poll(db, provider(7), now=NOW)
        result = crowd.poll(db, Broken(), now=NOW + timedelta(minutes=15))
        assert not result["data"]["available"]
    with Session(engine) as db:
        assert len(db.scalars(select(CrowdObservation)).all()) == 1
        health = crowd.source_health(db, now=NOW + timedelta(minutes=16))["sources"][0]
        assert health["consecutive_failures"] == 1 and health["last_raw_value"] == 7
        assert health["last_success_at"] == NOW.isoformat()
        assert health["last_error_code"] == "GYM_API_UNAVAILABLE"


def test_feedback_uses_only_prearrival_signals_and_is_idempotent(engine):
    with Session(engine) as db, db.begin():
        crowd.poll(db, provider(40), now=NOW - timedelta(minutes=5))
        workout_id = visit(db, NOW)
        # Later poll is NOT evidence at arrival.
        crowd.poll(db, provider(100), now=NOW + timedelta(minutes=30))
        original = crowd.record_feedback(db, workout_id, rating="BUSY", now=NOW + timedelta(hours=1), request_id="arrival")
        assert crowd.record_feedback(db, workout_id, rating="BUSY", now=NOW + timedelta(hours=2), request_id="arrival") == original
        label = db.scalar(select(CrowdFeedback))
        assert label.observed_at == NOW
        assert label.features_json["GYM_API"]["raw_value"] == 40
        assert label.features_json["GYM_API"]["prediction_before_label"] is None
        assert label.features_json["demo"]
        assert db.get(WorkoutSession, workout_id).crowd_feedback == "BUSY"
        prediction = crowd.CrowdModel(db, now=NOW + timedelta(hours=1)).predict(NOW + timedelta(days=7))
        assert prediction["score"] == pytest.approx(2 / 3)
        assert 0 < prediction["confidence"] < 0.5
        assert prediction["demo"] and prediction["forecast_accuracy_established"] is False
        with pytest.raises(DomainError, match="already has"):
            crowd.record_feedback(db, workout_id, rating="EMPTY", now=NOW + timedelta(hours=2), request_id="different")
        with pytest.raises(DomainError, match="different intent"):
            crowd.record_feedback(db, workout_id, rating="EMPTY", now=NOW + timedelta(hours=2), request_id="arrival")


def test_old_signal_not_paired_and_future_labels_do_not_leak(engine):
    with Session(engine) as db, db.begin():
        crowd.poll(db, provider(7), now=NOW - timedelta(hours=2))
        workout_id = visit(db, NOW)
        crowd.record_feedback(db, workout_id, rating="FINE", now=NOW + timedelta(hours=1), request_id="late")
        label = db.scalar(select(CrowdFeedback))
        assert "GYM_API" not in label.features_json
        assert crowd.CrowdModel(db, now=NOW).predict(NOW)["score"] is None
        assert crowd.CrowdModel(db, now=NOW + timedelta(hours=1)).predict(NOW + timedelta(days=7))["score"] == pytest.approx(1 / 3)


def test_discomfort_evidence_from_labels(engine):
    with Session(engine) as db, db.begin():
        for index in range(3):
            at = NOW - timedelta(days=(2 - index) * 7)
            crowd.poll(db, provider(60 + index), now=at)
            workout_id = visit(db, at)
            crowd.record_feedback(db, workout_id, rating="PACKED", now=at + timedelta(hours=1), request_id=f"label-{index}")
        assert db.get(CrowdSourceState, "GYM_API").evidence_count == 2
        preference = db.scalar(select(LearnedPreference).where(LearnedPreference.key == "crowd_discomfort_reported_count"))
        assert preference.evidence_count == 3 and preference.value_json["median_reported_count"] == 61
        assert "not capacity" in preference.value_json["meaning"]


def test_calibration_monotonic_after_ten_labels_not_capacity_conversion():
    pairs = [(0, 0), (10, 0), (20, 0.3), (30, 0.2), (40, 0.6), (50, 0.7), (60, 0.5), (70, 0.8), (80, 1), (90, 1)]
    values = [crowd.calibrated_count(x, pairs)[0] for x in range(100)]
    assert values == sorted(values)
    assert crowd.calibrated_count(30, pairs)[2] == "personalized_isotonic"
    assert crowd.calibrated_count(500, pairs)[1] < crowd.calibrated_count(50, pairs)[1]
    assert crowd.calibrated_count(100, [])[0] is None


def test_many_polls_do_not_inflate_independent_evidence_days(engine):
    with Session(engine) as db, db.begin():
        for minute in range(15):
            crowd.poll(db, provider(7), now=NOW + timedelta(minutes=minute))
        model = crowd.CrowdModel(db, now=NOW + timedelta(hours=1))
        predicted = model.predict(NOW + timedelta(days=7))
        assert predicted["features"]["GYM_API"]["evidence_days"] == 1
        assert predicted["score"] is None


def test_calendar_planning_uses_arrival_labels(engine):
    with Session(engine) as db, db.begin():
        update_profile(db, {"weekly_min_sessions": 1, "weekly_target_sessions": 1, "weekly_max_sessions": 1, "weekdays_allowed": [2], "earliest_workout_start": "19:00", "latest_workout_finish": "22:00"})
        for hour, rating in [(17, "PACKED"), (18, "EMPTY")]:
            at = NOW.replace(hour=hour) - timedelta(days=7)
            workout_id = visit(db, at)
            crowd.record_feedback(db, workout_id, rating=rating, now=at + timedelta(hours=1), request_id=f"label-{hour}")
        plan = schedule_week(db, NOW.date() + timedelta(days=5), now=NOW, request_id="future-week")
        assert plan["created_count"] == 1
        assert datetime.fromisoformat(plan["sessions"][0]["start"]).hour == 18
        assert plan["sessions"][0]["crowd"] == 0
        assert plan["sessions"][0]["workout_plan"]["crowd_score_kind"] == "personal_perceived_crowd_proxy"


def test_fixture_source_cannot_replace_live_studio(engine):
    with Session(engine) as db, db.begin():
        crowd.poll(db, provider(7), now=NOW)
        with pytest.raises(DomainError, match="another crowd studio"):
            crowd.poll(db, MySportsProvider(studio_id="1234567890", tenant="fixture-tenant", transport=Transport({"value": 7})), now=NOW)


def test_crowd_cli_fixture_feedback_and_health(engine, tmp_path, capsys):
    from gymclaw.cli import main
    fixture = tmp_path / "count.json"
    fixture.write_text(json.dumps({"source": "GYM_API", "metric": "reported_active_count", "raw_value": 7}))
    args = ["--db-url", str(engine.url), "crowd"]
    assert main(args + ["poll", "--fixture", str(fixture), "--now", NOW.isoformat()]) == 0
    assert json.loads(capsys.readouterr().out)["data"]["metadata"]["demo"]
    assert main(args + ["get-source-health", "--now", NOW.isoformat()]) == 0
    assert json.loads(capsys.readouterr().out)["data"]["sources"][0]["last_raw_value"] == 7
    assert main(["crowd", "poll", "--fixture", str(fixture)]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "FIXTURE_DB_REQUIRED"


def test_units_never_mix_counts_with_percentages():
    with pytest.raises(ValidationError):
        CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=7, normalized_value=0.07)
    with pytest.raises(ValidationError):
        CrowdReading(source="GOOGLE", metric="busyness_percentage", raw_value=10, normalized_value=0.1)


def test_polling_alert_once_when_stale_and_once_when_back(engine):
    with Session(engine) as db, db.begin():
        crowd.poll(db, provider(7), now=NOW)
        assert crowd.polling_health(db, now=NOW + timedelta(minutes=30))["status"] == "ok"
        assert crowd.polling_alert(db, now=NOW + timedelta(minutes=30)) is None
        alert = crowd.polling_alert(db, now=NOW + timedelta(hours=1))
        assert alert["message"].startswith("⚠️ No gym check-in data since")
        assert crowd.polling_alert(db, now=NOW + timedelta(hours=2)) is None  # no repeats
        crowd.poll(db, provider(9), now=NOW + timedelta(hours=3))
        assert crowd.polling_alert(db, now=NOW + timedelta(hours=3, minutes=1))["message"].startswith("✅")
        assert crowd.polling_alert(db, now=NOW + timedelta(hours=3, minutes=2)) is None
