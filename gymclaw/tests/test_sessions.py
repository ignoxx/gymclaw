from datetime import timedelta

import pytest
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import NotificationJob, PlannedSession
from gymclaw.providers.fixture_calendar import FixtureCalendarProvider
from gymclaw.services.calendar_writes import apply_write, pending_writes
from gymclaw.services.errors import DomainError
from gymclaw.services.sessions import SessionEdit, edit_session
from gymclaw.tests.test_weekly import SUNDAY, engine  # noqa: F401


def test_edit_preserves_intent_warnings_and_all_mechanics(engine):
    with Session(engine) as db, db.begin():
        def edit(op, key, data=None, session_id=None):
            return edit_session(db, op, changes=SessionEdit.model_validate(data or {}), session_id=session_id, now=SUNDAY, request_id=key)
        created = edit('create', 'one', {'start': '2026-10-12T05:13:00+02:00', 'end': '2026-10-12T05:14:00+02:00', 'template_id': 'short'})
        sid = created['data']['session']['id']
        assert created['data']['warnings']  # off-window, short: saved anyway
        row = db.get(PlannedSession, sid)
        assert row.user_locked and row.planned_start_at.minute == 13
        assert row.workout_plan_json.get('error')
        assert edit('create', 'one', {'start': '2026-10-12T05:13:00+02:00', 'end': '2026-10-12T05:14:00+02:00', 'template_id': 'short'}) == created
        provider = FixtureCalendarProvider(calendar_id='demo')
        apply_write(db, provider, pending_writes(db)[0]['id'], now=SUNDAY)
        old_jobs = list(db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == sid)))
        changed = edit('update', 'two', {'start': '2026-10-13T10:07:00+02:00', 'end': '2026-10-13T11:07:00+02:00', 'status': 'COMMITTED'}, sid)
        assert changed['data']['side_effects'] == []
        assert all(j.status == 'CANCELLED' for j in old_jobs)
        assert len(db.scalars(select(NotificationJob).where(NotificationJob.planned_session_id == sid, NotificationJob.status == 'PENDING')).all()) == 3
        assert pending_writes(db)[0]['action'] == 'UPDATE'
        assert row.workout_plan_json.get('error') is None
        with pytest.raises(DomainError, match='different intent'):
            edit('update', 'two', {'status': 'CANCELLED'}, sid)
        edit('cancel', 'three', session_id=sid)
        assert row.status == 'CANCELLED' and row.user_locked
        assert [w['action'] for w in pending_writes(db)] == ['DELETE']
        assert not db.scalars(select(NotificationJob).where(NotificationJob.status == 'PENDING')).all()


def test_status_update_and_invalid_template_do_not_fabricate_workouts(engine):
    with Session(engine) as db, db.begin():
        result = edit_session(db, 'create', changes=SessionEdit(start=SUNDAY + timedelta(days=1), template_id='short'), now=SUNDAY, request_id='create')
        sid = result['data']['session']['id']
        with pytest.raises(ValueError):
            edit_session(db, 'update', session_id=sid, changes=SessionEdit(template_id='missing'), now=SUNDAY, request_id='bad')
        edit_session(db, 'update', session_id=sid, changes=SessionEdit(status='TENTATIVE'), now=SUNDAY, request_id='tentative')
        assert not db.scalars(select(NotificationJob).where(NotificationJob.status == 'PENDING')).all()


def test_owner_exception_survives_background_ticks_without_changing_others(engine):
    from gymclaw.services import weekly
    from gymclaw.services.profile import update_profile
    from gymclaw.services.replanning import commit_upcoming
    with Session(engine) as db, db.begin():
        plan = weekly.weekly_plan(db, 'short', now=SUNDAY)
        ids = [s['id'] for s in plan['plan']['sessions']]
        untouched = {sid: (db.get(PlannedSession, sid).planned_start_at, db.get(PlannedSession, sid).workout_template_id) for sid in ids[1:]}
        moved = edit_session(db, 'update', session_id=ids[0], changes=SessionEdit(start='2026-10-13T07:00', status='TENTATIVE'),
            now=SUNDAY, request_id='move-only-monday')
        assert 'skips a rest day' in moved['data']['warnings']
        # Explicit status remains tentative even when deterministic commitment would normally run.
        commit_upcoming(db, now=SUNDAY + timedelta(days=1))
        assert db.get(PlannedSession, ids[0]).status == 'TENTATIVE'
        update_profile(db, {'weekly_min_sessions': 1, 'weekly_target_sessions': 1, 'weekly_max_sessions': 1})
        extra = edit_session(db, 'create', changes=SessionEdit(start='2026-10-14T05:17', template_id='short'),
            now=SUNDAY, request_id='extra-despite-defaults')
        assert 'exceeds your weekly maximum' in extra['data']['warnings']
        for minutes in (1, 2):
            result = weekly.rolling_plan(db, 'short', now=SUNDAY + timedelta(minutes=minutes))
            assert not result['changed'] and not result['rotation']
            assert {sid: (db.get(PlannedSession, sid).planned_start_at, db.get(PlannedSession, sid).workout_template_id) for sid in ids[1:]} == untouched
        assert db.get(PlannedSession, ids[0]).status == 'TENTATIVE'
