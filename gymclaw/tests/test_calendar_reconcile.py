from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

import pytest
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import CalendarEventSnapshot, CalendarSyncState, CalendarWrite, NotificationJob, PlannedSession, SetLog, WorkoutExercise, WorkoutSession
from gymclaw.providers.fixture_calendar import FixtureCalendarProvider
from gymclaw.services.calendar import sync_calendar
from gymclaw.services.calendar_writes import apply_write, event_id_for, pending_writes, queue_session_write
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import SlotSignal
from gymclaw.services.profile import update_profile
from gymclaw.services.replanning import commit_upcoming
from gymclaw.services.scheduling import PlanningFixture, schedule_week
from gymclaw.services.templates import ExerciseSpec, Template, import_template
from gymclaw.services.workout import start

ZONE = ZoneInfo("Europe/Berlin")
NOW = datetime(2026, 10, 11, 19, tzinfo=ZONE)
WEEK = date(2026, 10, 12)


def at(day, hour=19, minute=0):
    return datetime(2026, 10, day, hour, minute, tzinfo=ZONE)


def clock(stamp):
    return {"dateTime": stamp.isoformat(), "timeZone": "Europe/Berlin"}


def publish(engine, provider, now):
    with Session(engine) as db:
        ids = [w["id"] for w in pending_writes(db)]
    for write_id in ids:
        with Session(engine) as db, db.begin():
            assert apply_write(db, provider, write_id, now=now)["status"] in {"APPLIED", "CANCELLED"}


@pytest.fixture
def setup(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'calendar.db'}")
    initialize(engine)
    provider = FixtureCalendarProvider()
    specs = [ExerciseSpec(id=key, name=key.title(), role=key, primary=key in {"bench", "row"}, warmup_weight=50 if key == "bench" else None, target_weight=80 if key == "bench" else 50, priority=priority) for key, priority in [("bench", 10), ("row", 9), ("fly", 6), ("raise", 5), ("curl", 3), ("triceps", 2)]]
    with Session(engine) as db, db.begin():
        update_profile(db, {"earliest_workout_start": "19:00", "latest_workout_finish": "21:10", "minimum_workout_minutes": 70, "default_compound_rest_seconds": 180, "default_accessory_rest_seconds": 120})
        import_template(db, Template(id="long", name="Long", exercises=specs, set_duration_seconds=60, transition_seconds=90, source="demo_fixture"))
        sync_calendar(db, provider, now=NOW)
        fixture = PlanningFixture(signals=(SlotSignal(start=at(14, 20), crowd=0, confidence=1),))
        result = schedule_week(db, WEEK, template_id="long", fixture=fixture, now=NOW)
        commit_upcoming(db, now=NOW)
        ids = [s["id"] for s in result["sessions"]]
        for session_id in ids:
            queue_session_write(db, db.get(PlannedSession, session_id), now=NOW)
    publish(engine, provider, NOW + timedelta(minutes=1))
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=NOW + timedelta(minutes=2))
        assert all(not db.get(PlannedSession, i).user_locked for i in ids)
    yield engine, provider, ids
    engine.dispose()


def test_move_kept_reminders_replaced_other_sessions_replanned(setup):
    engine, provider, ids = setup
    wed_id = event_id_for(ids[1])
    with Session(engine) as db, db.begin():
        old = db.get(PlannedSession, ids[1])
        old.status = "COMMITTED"
        from gymclaw.services.notifications import schedule_session_jobs
        schedule_session_jobs(db, old, now=NOW)
        old_jobs = list(db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == old.id)))
        old_job_ids = [j.id for j in old_jobs]
        assert len(old_jobs) == 3
    provider.edit(wed_id, start=clock(at(15, 19)), end=clock(at(15, 20, 10)))
    with Session(engine) as db, db.begin():
        result = sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        moved = db.get(PlannedSession, ids[1])
        assert moved.user_locked and moved.planned_start_at == at(15, 19)
        assert moved.prep_start_at == at(15, 18, 25)
        assert moved.leave_home_at == at(15, 18, 40)
        assert moved.expected_finish_at == at(15, 20, 10)
        assert moved.crowd_prediction is None  # Wednesday prediction isn't Thursday evidence.
        assert all(db.get(NotificationJob, i).status == "CANCELLED" for i in old_job_ids)
        new_jobs = list(db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == moved.id, NotificationJob.status == "PENDING")))
        assert len(new_jobs) == 3 and all(j.due_at.date() == at(15).date() for j in new_jobs)
        assert db.get(PlannedSession, ids[2]).status == "CANCELLED"
        assert result["data"]["changes"] and result["user_message_hint"]
        assert not result["data"]["remote_events_changed"]
    publish(engine, provider, NOW + timedelta(minutes=4))
    with Session(engine) as db, db.begin():
        result = sync_calendar(db, provider, now=NOW + timedelta(minutes=5))
        assert db.get(PlannedSession, ids[1]).planned_start_at == at(15)
        assert not result["events"]  # Own echo must not produce another edit/replan.
    remote = provider.get_event(wed_id)
    assert remote.start == at(15)
    assert "Get ready: 18:25" in provider.events[wed_id]["description"]
    assert provider.get_event(event_id_for(ids[2])).status == "cancelled"


def test_delete_tombstone_not_recreated_valid_replacement_found(setup):
    engine, provider, ids = setup
    with Session(engine) as db, db.begin():
        update_profile(db, {"weekdays_allowed": [0, 1, 2, 3, 4, 5, 6]})
    deleted_event_id = event_id_for(ids[1])
    provider.cancel(deleted_event_id)
    with Session(engine) as db, db.begin():
        result = sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        original = db.get(PlannedSession, ids[1])
        assert original.status == "CANCELLED" and original.workout_plan_json["deleted_by_user"]
        new_rows = list(db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["COMMITTED", "TENTATIVE"]))))
        assert len(new_rows) == 3
        replacement = next(s for s in new_rows if s.id not in ids)
        assert replacement.id != ids[1] and replacement.planned_start_at.astimezone(ZONE).weekday() == 6
        assert all(w["event_id"] != deleted_event_id for w in pending_writes(db))
    publish(engine, provider, NOW + timedelta(minutes=4))
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=NOW + timedelta(minutes=5))
        assert db.get(CalendarEventSnapshot, deleted_event_id).status == "cancelled"
    assert provider.get_event(deleted_event_id).status == "cancelled"


def test_resize_compresses_to_45_minutes_and_start_uses_snapshot(setup):
    engine, provider, ids = setup
    wed_id = event_id_for(ids[1])
    provider.edit(wed_id, end=clock(at(14, 20, 45)))
    with Session(engine) as db, db.begin():
        response = sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        session = db.get(PlannedSession, ids[1])
        assert session.user_locked
        plan = session.workout_plan_json
        assert plan["estimated_duration_seconds"] <= 45 * 60
        assert plan["sets"]["bench"] == plan["sets"]["row"] == 3
        assert plan["sets"]["triceps"] == plan["sets"]["curl"] == 0
        assert response["user_message_hint"]
        result = start(db, "long", planned_session_id=session.id, now=at(14, 20), request_id="start-short")["data"]
        assert datetime.fromisoformat(result["eta"]) <= at(14, 20, 45)
        rows = list(db.scalars(select(WorkoutExercise)))
        assert next(e for e in rows if e.exercise_id == "raise").planned_working_sets == 2
        assert next(e for e in rows if e.exercise_id == "raise").config_json["working_sets"] == 3
        assert next(e for e in rows if e.exercise_id == "triceps").status == "SKIPPED"


def test_compression_uses_learned_set_duration(setup):
    engine, provider, ids = setup
    with Session(engine) as db, db.begin():
        db.add(WorkoutSession(id="historical", status="PLAN_UPDATED", template_snapshot={"source": "test_fixture"}))
        db.flush()
        db.add(WorkoutExercise(id="historical-bench", workout_session_id="historical", exercise_id="bench", position=0, status="COMPLETED", planned_working_sets=2, rep_min=8, rep_max=10, target_weight=80, rest_seconds=180))
        db.flush()
        first = at(5)
        db.add_all([SetLog(workout_exercise_id="historical-bench", set_number=1, set_type="WORKING", weight=80, reps=10, logged_at=first, rest_due_at=first + timedelta(seconds=180)), SetLog(workout_exercise_id="historical-bench", set_number=2, set_type="WORKING", weight=80, reps=10, logged_at=first + timedelta(seconds=300))])
    provider.edit(event_id_for(ids[1]), end=clock(at(14, 20, 45)))
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        session = db.get(PlannedSession, ids[1])
        assert session.workout_plan_json["template"]["set_duration_seconds"] == 120
        assert session.workout_plan_json["estimated_duration_seconds"] <= 2700
        result = start(db, "long", planned_session_id=session.id, now=at(14, 20), request_id="start-learned")["data"]
        assert datetime.fromisoformat(result["eta"]) <= at(14, 20, 45)


def test_too_short_edit_preserved_and_explained(setup):
    engine, provider, ids = setup
    provider.edit(event_id_for(ids[1]), end=clock(at(14, 20, 1)))
    with Session(engine) as db, db.begin():
        response = sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        session = db.get(PlannedSession, ids[1])
        assert session.planned_end_at == at(14, 20, 1) and session.user_locked
        assert session.workout_plan_json["error"] == "WORKOUT_SLOT_TOO_SHORT"
        assert "too short" in response["user_message_hint"]


def test_all_day_travel_cancels_without_compressing_missed_volume(setup):
    engine, provider, ids = setup
    provider.put({"id": "travel", "summary": "Travel", "start": {"date": "2026-10-12"}, "end": {"date": "2026-10-19"}})
    with Session(engine) as db, db.begin():
        result = sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        assert all(db.get(PlannedSession, i).status == "CANCELLED" for i in ids)
        assert "0/3" in result["user_message_hint"]
        assert not db.scalars(select(PlannedSession).where(PlannedSession.status == "TENTATIVE")).all()


def test_invalidated_token_full_sync_keeps_deletion_provenance(setup):
    engine, provider, ids = setup
    del provider.events[event_id_for(ids[1])]
    provider.expire_token = True
    with Session(engine) as db, db.begin():
        result = sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        assert result["data"]["full_sync"]
        assert db.get(PlannedSession, ids[1]).workout_plan_json["deleted_by_user"]
        assert db.get(CalendarSyncState, 1).sync_token


def test_failed_sync_preserves_plan_and_token(setup):
    engine, provider, ids = setup
    with Session(engine) as db:
        token = db.get(CalendarSyncState, 1).sync_token
        prior = db.get(PlannedSession, ids[1]).planned_start_at
    provider.fail_sync = True
    with Session(engine) as db:
        with pytest.raises(DomainError, match="unavailable"):
            sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        db.rollback()
        assert db.get(CalendarSyncState, 1).sync_token == token
        assert db.get(PlannedSession, ids[1]).planned_start_at == prior


def test_locked_hard_conflicts_are_kept_and_explained(setup):
    engine, provider, ids = setup
    provider.edit(event_id_for(ids[0]), start=clock(at(12, 20)), end=clock(at(12, 21, 10)))
    provider.edit(event_id_for(ids[1]), start=clock(at(13)), end=clock(at(13, 20, 10)))
    with Session(engine) as db, db.begin():
        result = sync_calendar(db, provider, now=NOW + timedelta(minutes=3))
        assert db.get(PlannedSession, ids[0]).user_locked
        assert db.get(PlannedSession, ids[1]).user_locked
        assert db.get(PlannedSession, ids[1]).planned_start_at == at(13)
        assert "conflicts with recovery" in result["user_message_hint"]


def test_write_retry_after_remote_success_no_duplicate_events(setup):
    engine, provider, ids = setup
    with Session(engine) as db, db.begin():
        result = schedule_week(db, date(2026, 10, 19), template_id="long", now=NOW + timedelta(minutes=3))
        session = db.get(PlannedSession, result["sessions"][0]["id"])
        write = queue_session_write(db, session, now=NOW + timedelta(minutes=3))
        write_id, body = write.id, write.body_json
    count = len(provider.events)
    provider.create_gym_event(body)  # Simulate crash after Google accepted, before DB ack.
    with Session(engine) as db, db.begin():
        assert apply_write(db, provider, write_id, now=NOW + timedelta(minutes=4))["status"] == "APPLIED"
        assert apply_write(db, provider, write_id, now=NOW + timedelta(minutes=4))["retried"]
    assert len(provider.events) == count + 1


def test_patch_retry_after_remote_success_is_acknowledged(setup):
    engine, provider, ids = setup
    with Session(engine) as db, db.begin():
        session = db.get(PlannedSession, ids[1])
        session.planned_start_at -= timedelta(minutes=15)
        session.planned_end_at -= timedelta(minutes=15)
        session.prep_start_at -= timedelta(minutes=15)
        session.leave_home_at -= timedelta(minutes=15)
        session.expected_finish_at = session.planned_end_at
        session.source_revision += 1
        write = queue_session_write(db, session, now=NOW + timedelta(minutes=3))
        write_id, body, etag, event_id = write.id, write.body_json, write.expected_etag, write.event_id
    provider.update_gym_event(event_id, body, etag=etag)  # Response lost.
    changes = len(provider.changes)
    with Session(engine) as db, db.begin():
        assert apply_write(db, provider, write_id, now=NOW + timedelta(minutes=4))["status"] == "APPLIED"
        assert not db.get(PlannedSession, ids[1]).user_locked
    assert len(provider.changes) == changes


def test_live_write_gate_runs_before_any_provider_call(setup):
    engine, provider, ids = setup
    provider.source = "google"  # Spy only; no Google transport.
    calls = len(provider.calls)
    with Session(engine) as db:
        with pytest.raises(DomainError) as error:
            apply_write(db, provider, "anything", now=NOW)
        assert error.value.code == "CALENDAR_WRITES_NOT_APPROVED"
    assert len(provider.calls) == calls


def test_remote_write_race_never_overwrites_manual_move(setup):
    engine, provider, ids = setup
    target_id = event_id_for(ids[1])
    with Session(engine) as db, db.begin():
        session = db.get(PlannedSession, ids[1])
        session.planned_start_at += timedelta(minutes=15)
        session.planned_end_at += timedelta(minutes=15)
        session.source_revision += 1
        queued = queue_session_write(db, session, now=NOW + timedelta(minutes=3))
        write_id = queued.id
    provider.edit(target_id, start=clock(at(15)), end=clock(at(15, 20, 10)))
    with Session(engine) as db, db.begin():
        result = apply_write(db, provider, write_id, now=NOW + timedelta(minutes=4))
        assert result["status"] == "CONFLICT"
        assert db.get(CalendarWrite, write_id).status == "CONFLICT"
    assert provider.get_event(target_id).start == at(15)
    with Session(engine) as db, db.begin():
        sync_calendar(db, provider, now=NOW + timedelta(minutes=5))
        assert db.get(PlannedSession, ids[1]).user_locked
        assert db.get(PlannedSession, ids[1]).planned_start_at == at(15)
