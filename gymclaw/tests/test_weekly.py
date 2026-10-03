from datetime import datetime, timedelta, timezone
import json
from pathlib import Path

import pytest
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.cli import main, parser
from gymclaw.db import initialize, make_engine
from gymclaw.models import AgentEvent, CalendarSyncState, CalendarWrite, NotificationDelivery, PlannedSession, RuntimeSettings
from gymclaw.providers.fixture_calendar import FixtureCalendarProvider
from gymclaw.services import runtime, weekly
from gymclaw.services.calendar_writes import apply_write, pending_writes, queue_session_write
from gymclaw.services.profile import update_profile
from gymclaw.services.replanning import commit_upcoming
from gymclaw.services.scheduling import schedule_week
from gymclaw.services.templates import ExerciseSpec, Template, import_template
from gymclaw.services.errors import DomainError
from gymclaw.tests.test_runtime import FakeRuntime

SUNDAY = datetime(2026, 10, 11, 17, tzinfo=timezone.utc)


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'weekly.db'}")
    initialize(engine)
    with Session(engine) as db, db.begin():
        import_template(db, Template(id="short", name="Short", exercises=[ExerciseSpec(id="bench", name="Bench", role="press", primary=True, working_sets=2, target_weight=80, warmup_weight=50)]))
    yield engine
    engine.dispose()


def test_weekly_plan_retry_one_briefing_and_no_duplicate_calendar_intent(engine):
    with Session(engine) as db, db.begin():
        result = weekly.weekly_plan(db, "short", now=SUNDAY)
        assert result["week_start"] == "2026-10-12"
        assert len(result["plan"]["sessions"]) == 3
        assert result["audit"]["completed_workouts"] == 0
        assert len(pending_writes(db)) == 3
        text = weekly.briefing(db, datetime(2026, 10, 12).date(), result["audit"], published=False)
        assert "Local plan only" in text and "Mon" in text and "Last week: 0/3" in text
        event_id = result["event_id"]
    with Session(engine) as db, db.begin():
        retry = weekly.weekly_plan(db, "short", now=SUNDAY + timedelta(minutes=1))
        assert retry["event_id"] == event_id
        assert len(db.scalars(select(PlannedSession)).all()) == 3
        assert len(pending_writes(db)) == 3
        assert len(db.scalars(select(AgentEvent).where(AgentEvent.type == "planning.weekly_briefing")).all()) == 1


def test_rolling_watch_marks_elapsed_unstarted_session_missed_not_completed(engine):
    with Session(engine) as db, db.begin():
        weekly.weekly_plan(db, "short", now=SUNDAY)
    with Session(engine) as db, db.begin():
        later = SUNDAY + timedelta(days=1, hours=2)
        result = weekly.rolling_plan(db, "short", now=later)
        assert len(result["missed"]) == 1
        missed = db.get(PlannedSession, result["missed"][0])
        assert missed.status == "MISSED"
        assert not db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == missed.id, CalendarWrite.status == "PENDING")).all()
        report = weekly.audit_week(db, datetime(2026, 10, 12).date(), now=later)
        assert report["missed_sessions"] == 1 and report["completed_workouts"] == 0
        count = len(db.scalars(select(AgentEvent).where(AgentEvent.type == "workout.missed")).all())
        weekly.rolling_plan(db, "short", now=later + timedelta(minutes=1))
        assert len(db.scalars(select(AgentEvent).where(AgentEvent.type == "workout.missed")).all()) == count


def test_runtime_weekly_and_crowd_jobs_persist_selected_template(engine):
    provider = FakeRuntime()
    args = dict(now=SUNDAY, recipient="123", project_root=Path.cwd(), allow_runtime_changes=True, allow_messages=True)
    runtime.sync_automations(engine, provider, **args, template_id="short", crowd_polling=True)
    weekly_job = next(spec for spec in provider.created if spec.name.endswith(":weekly-plan"))
    crowd_job = next(spec for spec in provider.created if spec.name.endswith(":crowd-poll"))
    assert weekly_job.cron == "0 19 * * 0" and weekly_job.timezone == "Europe/Berlin"
    assert "weekly" in weekly_job.argv and "--allow-calendar-writes" not in weekly_job.argv
    assert crowd_job.every == "15m" and "poll-crowd" in crowd_job.argv
    runtime.sync_automations(engine, provider, **args)
    assert len(provider.created) == 3  # Watcher + weekly + crowd, no duplicates.
    with Session(engine) as db:
        settings = db.get(RuntimeSettings, 1)
        assert settings.template_id == "short" and settings.crowd_polling_enabled
        assert settings.calendar_writes_enabled is False


def test_separate_calendar_write_authority_and_pause_cannot_be_undone_by_callback(engine, capsys):
    provider = FakeRuntime()
    runtime.sync_automations(engine, provider, now=SUNDAY, recipient="123", project_root=Path.cwd(),
        allow_runtime_changes=True, allow_messages=True, template_id="short", allow_calendar_writes=True)
    args = ["--db-url", str(engine.url), "runtime"]
    assert main(args + ["revoke-calendar-writes"]) == 0
    assert json.loads(capsys.readouterr().out)["data"]["calendar_writes_enabled"] is False
    assert main(args + ["pause"]) == 0
    assert json.loads(capsys.readouterr().out)["data"]["enabled"] is False
    with pytest.raises(DomainError, match="paused"):
        runtime.sync_automations(engine, provider, now=SUNDAY, recipient="123", project_root=Path.cwd(),
            allow_runtime_changes=True, allow_messages=True, configure=False)
    with pytest.raises(DomainError, match="paused"):
        runtime.sync_automations(engine, provider, now=SUNDAY, recipient="123", project_root=Path.cwd(),
            allow_runtime_changes=True, allow_messages=True)
    assert main(args + ["resume", "--allow-runtime-changes", "--allow-messages"]) == 0
    value = json.loads(capsys.readouterr().out)["data"]
    assert value["enabled"] and not value["calendar_writes_enabled"]


def test_generic_outbox_dedup_and_pending_calendar_ack_superseded(engine):
    with Session(engine) as db, db.begin():
        first = AgentEvent(type="calendar.update_briefing", created_at=SUNDAY)
        second = AgentEvent(type="calendar.update_briefing", created_at=SUNDAY + timedelta(seconds=1))
        db.add_all([first, second]); db.flush()
        one = runtime.prepare_event_message(db, first.id, message="Old", now=SUNDAY, recipient="123", profile="gymclaw")
        two = runtime.prepare_event_message(db, second.id, message="Latest", now=SUNDAY + timedelta(seconds=1), recipient="123", profile="gymclaw")
        assert runtime.prepare_event_message(db, second.id, message="Latest", now=SUNDAY, recipient="123", profile="gymclaw")["id"] == two["id"]
        assert db.get(NotificationDelivery, one["id"]).status == "CANCELLED"
    provider = FakeRuntime()
    assert runtime.deliver(engine, provider, two["id"], now=SUNDAY + timedelta(seconds=1), allow_messages=True)["status"] == "SENT"
    assert provider.sent == [("123", "Latest")]
    with Session(engine) as db:
        assert db.get(AgentEvent, two["event_id"]).handled_at is not None


def test_weekly_cli_fixture_preview_never_activates_or_queues_send(engine, tmp_path, capsys):
    fixture = tmp_path / "calendar.json"
    fixture.write_text(json.dumps({"calendar_id": "demo-calendar", "events": [], "full": True}))
    args = ["--db-url", str(engine.url), "runtime", "weekly", "--template-id", "short", "--telegram-id", "123", "--fixture", str(fixture), "--now", SUNDAY.isoformat()]
    assert main(args) == 0
    result = json.loads(capsys.readouterr().out)["data"]
    assert "DEMO" in result["briefing"] and "Local plan only" in result["briefing"]
    assert result["delivery"]["queued"] is False
    with Session(engine) as db:
        assert db.get(RuntimeSettings, 1) is None
        assert not db.scalars(select(NotificationDelivery)).all()
    assert main(args + ["--allow-messages"]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "FIXTURE_SCOPE_REQUIRED"
    assert main(["--db-url", str(engine.url), "runtime", "sync", "--telegram-id", "123", "--allow-runtime-changes", "--allow-messages"]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "LIVE_CALENDAR_REQUIRED"


def test_calendar_resize_watch_proactively_sends_once_without_publication(engine, monkeypatch):
    from gymclaw.runtime_cli import runtime_command
    class FakeGoogle(FixtureCalendarProvider):
        source = "google"  # Synthetic Google boundary; HTTP remains blocked.
    calendar = FakeGoogle("unit-calendar")
    provider = FakeRuntime()
    with Session(engine) as db, db.begin():
        update_profile(db, {"weekly_min_sessions": 1, "weekly_target_sessions": 1, "weekly_max_sessions": 1})
        db.add(CalendarSyncState(id=1, calendar_id=calendar.calendar_id, timezone="Europe/Berlin", source="google"))
        plan = schedule_week(db, datetime(2026, 10, 12).date(), now=SUNDAY, template_id="short")["sessions"][0]
        row = db.get(PlannedSession, plan["id"])
        commit_upcoming(db, now=SUNDAY)
        write = queue_session_write(db, row, now=SUNDAY)
        apply_write(db, calendar, write.id, now=SUNDAY, allow_writes=True)
        event_id = row.calendar_event_id
        end = row.planned_end_at - timedelta(minutes=25)
        runtime.configure_runtime(db, profile="gymclaw", recipient="123", project_root=Path.cwd(), python=__import__("sys").executable)
    calendar.edit(event_id, end={"dateTime": end.isoformat(), "timeZone": "Europe/Berlin"})
    monkeypatch.setattr("gymclaw.runtime_cli.google_provider", lambda *a: calendar)
    monkeypatch.setattr("gymclaw.runtime_cli.OpenClawProvider", lambda *a: provider)
    def forbidden(*a, **k):
        raise AssertionError("No continuing calendar-write authority")
    monkeypatch.setattr("gymclaw.runtime_cli.publish_command", forbidden)
    args = parser().parse_args(["--db-url", str(engine.url), "runtime", "watch", "--telegram-id", "123", "--allow-runtime-changes", "--allow-messages"])
    result = runtime_command(engine, args, now_override=SUNDAY + timedelta(minutes=1))
    assert result["data"]["publication"]["published"] is False
    assert len(provider.sent) == 1
    assert "Workout slot resized" in provider.sent[0][1]
    assert "publication pending approval" in provider.sent[0][1]
    runtime_command(engine, args, now_override=SUNDAY + timedelta(minutes=2))
    assert len(provider.sent) == 1


def test_new_arrival_label_reconsiders_tentative_slot_but_not_user_lock(engine):
    from gymclaw.models import WorkoutSession
    from gymclaw.services.crowd import record_feedback
    with Session(engine) as db, db.begin():
        update_profile(db, {"weekly_min_sessions": 1, "weekly_target_sessions": 1, "weekly_max_sessions": 1,
            "weekdays_allowed": [2], "earliest_workout_start": "19:00", "latest_workout_finish": "22:00"})
        original = weekly.weekly_plan(db, "short", now=SUNDAY)["plan"]["sessions"][0]
        assert datetime.fromisoformat(original["start"]).hour == 17  # 19:00 local cold start.
        at = (SUNDAY - timedelta(days=4)).replace(hour=18)
        visit = WorkoutSession(status="PLAN_UPDATED", arrived_at=at, started_at=at, completed_at=at + timedelta(hours=1), template_snapshot={"demo": True})
        db.add(visit); db.flush()
        record_feedback(db, visit.id, rating="EMPTY", now=SUNDAY + timedelta(minutes=1), request_id="quiet-visit")
        result = weekly.rolling_plan(db, "short", now=SUNDAY + timedelta(minutes=2))
        updated = db.get(PlannedSession, original["id"])
        assert updated.planned_start_at.hour == 18  # 20:00 local learned quiet slot.
        assert result["changed"] and updated.status == "TENTATIVE"
        updated.user_locked = True
        original_time = updated.planned_start_at
        event = AgentEvent(type="planning.replan_required", created_at=SUNDAY, payload_json={"reason": "crowd_feedback"})
        db.add(event); db.flush()
        weekly.rolling_plan(db, "short", now=SUNDAY + timedelta(minutes=3))
        assert updated.planned_start_at == original_time
        assert event.handled_at is not None


def test_split_rotates_after_last_completed_workout_and_respects_locks(tmp_path):
    from gymclaw.models import OnboardingState, PlannedSession, WorkoutSession
    from gymclaw.services.templates import ExerciseSpec, Template, import_template
    from gymclaw.services.weekly import assign_rotation

    engine = make_engine(f"sqlite:///{tmp_path / 'rotation.db'}")
    initialize(engine)
    base = datetime(2026, 10, 5, 10, tzinfo=timezone.utc)
    try:
        with Session(engine) as db, db.begin():
            for name in ("push", "pull", "legs"):
                import_template(db, Template(id=name, name=name.title(), exercises=[ExerciseSpec(id=f"{name}-x", name="X", role="x", guide_id="bench-press", target_weight=0)]))
            db.add(OnboardingState(id=1, interview_json={}, split_json=["push", "pull", "legs"]))
            db.add(WorkoutSession(template_id="push", status="PLAN_UPDATED", template_snapshot={"x": 1}, completed_at=base - timedelta(days=3)))
            sessions = []
            for day in range(4):
                start = base + timedelta(days=2 * day)
                sessions.append(PlannedSession(week_id="2026-10-05", workout_template_id="push", status="TENTATIVE", planned_start_at=start,
                    planned_end_at=start + timedelta(hours=1), prep_start_at=start, leave_home_at=start, expected_finish_at=start + timedelta(hours=1)))
            sessions[1].user_locked = True  # owner pinned Push here
            db.add_all(sessions)
            db.flush()
            assign_rotation(db, now=base - timedelta(days=1))
            # Last done: Push → Pull, then the locked Push, then Pull again, then Legs.
            assert [s.workout_template_id for s in sessions] == ["pull", "push", "pull", "legs"]
            assert sessions[0].workout_plan_json["template"]["id"] == "pull"
            assert assign_rotation(db, now=base - timedelta(days=1)) == []
    finally:
        engine.dispose()


def test_crowd_forecast_text_and_significant_changes():
    from gymclaw.services.calendar_writes import describe_crowd
    from gymclaw.services.weekly import significant
    start = datetime(2026, 10, 9, 19, tzinfo=timezone.utc)
    assert describe_crowd(None, start) == "not enough data yet"
    assert describe_crowd({"count": 13, "basis": "weekday_hour", "days": 2, "feel": "Busy"}, start) == "Busy · ~13 people checked in (Fridays 19:00, 2 wk)"
    assert describe_crowd({"count": 8, "basis": "hour", "days": 1, "feel": None}, start) == "~8 people checked in (19:00 on other days)"
    old = {"count": 12, "feel": "Busy"}
    assert significant(None, {"count": 5, "feel": None})
    assert not significant(old, {"count": 14, "feel": "Busy"})  # +2: noise
    assert significant(old, {"count": 16, "feel": "Busy"})  # +4 and +33%
    assert significant(old, {"count": 12, "feel": "Packed"})
    assert not significant(old, None)


def test_refresh_crowd_fills_unknown_then_respects_window(tmp_path):
    from gymclaw.models import CrowdObservation
    from gymclaw.services.weekly import refresh_crowd
    engine = make_engine(f"sqlite:///{tmp_path / 'crowd.db'}")
    initialize(engine)
    now = datetime(2026, 10, 2, 12, tzinfo=timezone.utc)
    try:
        with Session(engine) as db, db.begin():
            db.add(CrowdObservation(observed_at=now - timedelta(days=7) + timedelta(hours=6), source="GYM_API", raw_value=13, metadata_json={}))
            soon = PlannedSession(week_id="2026-09-28", status="COMMITTED", planned_start_at=now + timedelta(hours=6), planned_end_at=now + timedelta(hours=7),
                prep_start_at=now + timedelta(hours=5), leave_home_at=now + timedelta(hours=5), expected_finish_at=now + timedelta(hours=7))
            imminent = PlannedSession(week_id="2026-09-28", status="COMMITTED", planned_start_at=now + timedelta(minutes=30), planned_end_at=now + timedelta(hours=1),
                prep_start_at=now, leave_home_at=now, expected_finish_at=now + timedelta(hours=1))
            db.add_all([soon, imminent]); db.flush()
            changed = refresh_crowd(db, now=now)
            assert [c["session_id"] for c in changed] == [soon.id]
            assert soon.workout_plan_json["crowd_forecast"]["count"] == 13 and soon.source_revision == 2
            assert "crowd_forecast" not in imminent.workout_plan_json
            assert refresh_crowd(db, now=now + timedelta(minutes=10)) == []  # throttled
    finally:
        engine.dispose()


def test_refresh_plans_replaces_stale_copy_once(tmp_path):
    from gymclaw.models import PlannedSession
    from gymclaw.services.calendar import allocation
    from gymclaw.services.weekly import refresh_plans
    engine = make_engine(f"sqlite:///{tmp_path / 'plans.db'}")
    initialize(engine)
    now = datetime(2026, 10, 3, 8, tzinfo=timezone.utc)
    spec = dict(id="row", name="Row", role="pull", guide_id="seated-row", target_weight=0)
    try:
        with Session(engine) as db, db.begin():
            import_template(db, Template(id="pull", name="Pull", exercises=[ExerciseSpec(**spec, working_sets=3)]))
            start = now + timedelta(days=2)
            session = PlannedSession(week_id="2026-10-05", workout_template_id="pull", status="TENTATIVE", planned_start_at=start,
                planned_end_at=start + timedelta(hours=1), prep_start_at=start, leave_home_at=start, expected_finish_at=start + timedelta(hours=1))
            db.add(session); db.flush()
            allocation(db, session)
            import_template(db, Template(id="pull", name="Pull", exercises=[ExerciseSpec(**spec, working_sets=2)]))
            assert refresh_plans(db, now=now) == [session.id]
            assert session.workout_plan_json["template"]["exercises"][0]["working_sets"] == 2
            assert refresh_plans(db, now=now) == []  # no churn on the next tick
    finally:
        engine.dispose()


def test_event_body_has_plan_location_and_split_colour(tmp_path):
    from gymclaw.models import OnboardingState
    from gymclaw.services.calendar import allocation
    from gymclaw.services.calendar_writes import event_body
    engine = make_engine(f"sqlite:///{tmp_path / 'event.db'}")
    initialize(engine)
    start = datetime(2026, 10, 5, 8, 30, tzinfo=timezone.utc)
    try:
        with Session(engine) as db, db.begin():
            for name in ("push", "pull"):
                import_template(db, Template(id=name, name=name.title(), exercises=[ExerciseSpec(id=f"{name}-row", name="Row", role="x", guide_id="seated-row", working_sets=2, rep_min=8, rep_max=12, target_weight=0)]))
            db.add(OnboardingState(id=1, interview_json={}, split_json=["push", "pull"]))
            update_profile(db, {"gym_address": "Example Str. 1, Berlin"})
            session = PlannedSession(week_id="2026-10-05", workout_template_id="pull", status="COMMITTED", planned_start_at=start,
                planned_end_at=start + timedelta(hours=1), prep_start_at=start - timedelta(minutes=35), leave_home_at=start - timedelta(minutes=20), expected_finish_at=start + timedelta(hours=1))
            db.add(session); db.flush()
            allocation(db, session)
            body = event_body(db, session)
            assert body["summary"] == "🏋️ Pull" and body["location"] == "Example Str. 1, Berlin" and body["colorId"] == "6"
            assert "Get ready 09:55 · Leave 10:10 · Home ~11:50" in body["description"]
            assert "1. Row · 2×8–12" in body["description"]
    finally:
        engine.dispose()
