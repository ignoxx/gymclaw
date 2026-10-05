"""Preserve manual intent; repair only invalid unlocked future sessions."""
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import PlannedSession
from gymclaw.services.calendar import allocation, week_of
from gymclaw.services.calendar_busy import busy_intervals
from gymclaw.services.calendar_writes import queue_session_write
from gymclaw.services.notifications import cancel_session_jobs, schedule_session_jobs
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import Candidate, Interval, generate_candidates, overlaps, plan_week, recovery_ok
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import emit, utc

ACTIVE = {"TENTATIVE", "COMMITTED", "STARTED", "COMPLETED"}


def within_bounds(session, profile):
    zone = ZoneInfo(profile.timezone)
    start, end = session.planned_start_at.astimezone(zone), session.planned_end_at.astimezone(zone)
    return start.weekday() in profile.weekdays_allowed and start.date() == end.date() and start.time().replace(tzinfo=None) >= profile.earliest_workout_start and end.time().replace(tzinfo=None) <= profile.latest_workout_finish


def interval(session):
    return Interval(start=session.planned_start_at, end=session.planned_end_at)


def expanded(session, profile):
    return (session.planned_start_at - timedelta(minutes=profile.prep_minutes + profile.commute_to_gym_minutes), session.planned_end_at + timedelta(minutes=profile.commute_home_minutes))


def commit_upcoming(db: Session, *, now: datetime):
    for session in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]))):
        if session.status == "TENTATIVE" and now < session.planned_start_at <= now + timedelta(hours=48):
            session.status = "COMMITTED"
            session.source_revision += 1
            queue_session_write(db, session, now=now)
        schedule_session_jobs(db, session, now=now)


def place(db: Session, session: PlannedSession, slot: Candidate, *, week: date, now: datetime, crowd_model):
    """Put a session on a planner slot: times, crowd forecast, workout fit, calendar intent and reminders."""
    session.planned_start_at, session.planned_end_at = slot.start, slot.end
    session.prep_start_at, session.leave_home_at = slot.prep_start, slot.leave_home
    session.expected_finish_at = slot.end
    session.crowd_prediction, session.crowd_confidence = slot.crowd, slot.confidence
    session.status = "COMMITTED" if slot.start <= now + timedelta(hours=48) else "TENTATIVE"
    session.week_id = week.isoformat()
    db.flush()
    allocation(db, session)
    session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if not k.startswith("crowd_")}
    if slot.crowd is not None:
        session.workout_plan_json |= {"crowd_source": "demo_fixture" if crowd_model.predict(slot.start)["demo"] else "local_crowd_model", "crowd_score_kind": "personal_perceived_crowd_proxy"}
    queue_session_write(db, session, now=now)
    schedule_session_jobs(db, session, now=now)


def replan_weeks(db: Session, weeks: set[date], *, now: datetime, reconsider_tentative: bool = False) -> dict:
    now = utc(now)
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    changed, warnings = [], []
    for week in sorted(weeks):
        if week.weekday() != 0:
            raise ValueError("week_start must be Monday")
        all_rows = list(db.scalars(select(PlannedSession).order_by(PlannedSession.planned_start_at, PlannedSession.id)))
        week_rows = [s for s in all_rows if week_of(s.planned_start_at, zone) == week]
        if not week_rows:
            continue  # Sync alone must not invent an un-onboarded training plan.
        start = datetime.combine(week - timedelta(days=1), datetime.min.time(), zone)
        end = start + timedelta(days=9)
        busy = busy_intervals(db, start, end)
        fixed = [s for s in all_rows if s.status in ACTIVE and (s.user_locked or s.status in {"STARTED", "COMPLETED"} or s.planned_start_at < now or week_of(s.planned_start_at, zone) != week)]
        invalid, kept = [], list(fixed)
        for session in week_rows:
            if session in fixed or session.status not in ACTIVE:
                continue
            prep, home = expanded(session, profile)
            valid = within_bounds(session, profile) and not any(overlaps(prep, home, b) for b in busy) and all(recovery_ok(session.planned_start_at, session.planned_end_at, interval(other), profile) for other in kept)
            reconsider = reconsider_tentative and session.status == "TENTATIVE" and session.planned_start_at > now + timedelta(hours=48)
            if valid and not reconsider and sum(week_of(s.planned_start_at, zone) == week for s in kept) < profile.weekly_max_sessions:
                kept.append(session)
            else:
                invalid.append(session)
        deleted = tuple(interval(s) for s in all_rows if s.workout_plan_json.get("deleted_by_user"))
        horizon = now + timedelta(days=10)
        unavailable = deleted + ((Interval(start=horizon, end=end),) if horizon < end else ())
        from gymclaw.services.crowd import CrowdModel, planning_signals
        signals = planning_signals(db, week, now=now)
        crowd_model = CrowdModel(db, now=now) if signals else None
        planned = plan_week(profile, week, existing=tuple(interval(s) for s in kept), busy=busy, unavailable=unavailable, signals=signals, now=now)
        source_template = next((s for s in week_rows if s.workout_template_id), None)
        if source_template is None:
            # Existing legacy plans may still be moved, but no unknown workout is fabricated.
            template_id = None
        else:
            template_id = source_template.workout_template_id
        for index, slot in enumerate(planned.sessions):
            if index < len(invalid):
                session = invalid[index]
                old_start = session.planned_start_at
                same_time = session.planned_start_at == slot.start and session.planned_end_at == slot.end
                if same_time and session.crowd_prediction == slot.crowd and session.crowd_confidence == slot.confidence:
                    continue  # Reconsidering unchanged tentative slots is not a new action.
                if not same_time:
                    cancel_session_jobs(db, session.id, now=now)
                session.source_revision += 1
            else:
                session = PlannedSession(week_id=week.isoformat(), workout_template_id=template_id)
                db.add(session)
                old_start = None
                same_time = False
            place(db, session, slot, week=week, now=now, crowd_model=crowd_model)
            changed.append({"session_id": session.id, "from": old_start.isoformat() if old_start else None, "to": slot.start.isoformat(), "action": "forecast_updated" if same_time else "moved" if old_start else "replacement"})
        for session in invalid[len(planned.sessions):]:
            session.status = "CANCELLED"
            session.source_revision += 1
            cancel_session_jobs(db, session.id, now=now)
            queue_session_write(db, session, now=now)
            changed.append({"session_id": session.id, "action": "cancelled_no_valid_slot"})
        if planned.shortfall:
            count = planned.existing_count + len(planned.sessions)
            warnings.append(f"Week {week.isoformat()}: {count}/{profile.weekly_target_sessions} sessions feasible; recovery/blockers preserved.")
        if sum(s.user_locked and s.status in ACTIVE for s in week_rows) > profile.weekly_max_sessions:
            warnings.append("Manually locked sessions exceed weekly maximum; edited events kept.")
        for session in week_rows:
            if not session.user_locked or session.status not in ACTIVE:
                continue
            prep, home = expanded(session, profile)
            local_start, local_end = session.planned_start_at.astimezone(zone), session.planned_end_at.astimezone(zone)
            if local_start.date() != local_end.date() or local_start.time().replace(tzinfo=None) < profile.earliest_workout_start or local_end.time().replace(tzinfo=None) > profile.latest_workout_finish:
                warnings.append(f"Manually edited session {session.id} is outside configured workout time bounds; edited event kept.")
            if any(overlaps(prep, home, b) for b in busy):
                warnings.append(f"Manually edited session {session.id} overlaps a blocker including prep/travel; kept for explicit resolution.")
            if any(not recovery_ok(session.planned_start_at, session.planned_end_at, interval(other), profile) for other in fixed if other.id != session.id):
                warnings.append(f"Manually edited session {session.id} conflicts with recovery; edited event kept.")
            if session.workout_plan_json.get("error"):
                warnings.append(f"Manually edited session {session.id} is too short for required movement; adjust slot explicitly.")
        db.flush()
    if changed:
        emit(db, "planning.replanned", now, {"changes": changed, "warnings": warnings})
    return {"changed": changed, "warnings": list(dict.fromkeys(warnings))}


def best_slot(db: Session, session: PlannedSession, *, now: datetime, day: date | None = None) -> dict:
    """Move one session to the best-scoring (quietest) valid slot, on `day` if given, else anywhere in
    its week. Planner rules apply: workout window, calendar, rest days. Exact owner times use
    `session_edits.move` instead. The planner owns the result, so any earlier pin is released."""
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    week = week_of(datetime.combine(day, datetime.min.time(), zone) if day else session.planned_start_at, zone)
    window_start = datetime.combine(week - timedelta(days=1), datetime.min.time(), zone)
    others = tuple(interval(s) for s in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(ACTIVE), PlannedSession.id != session.id)))
    deleted = tuple(interval(s) for s in db.scalars(select(PlannedSession).where(PlannedSession.status == "CANCELLED")) if s.workout_plan_json.get("deleted_by_user"))
    from gymclaw.services.crowd import CrowdModel, planning_signals
    signals = planning_signals(db, week, now=now)
    candidates = [c for c in generate_candidates(profile, week, busy=busy_intervals(db, window_start, window_start + timedelta(days=9)), unavailable=deleted, existing=others, signals=signals, now=now)
        if day is None or c.start.astimezone(zone).date() == day]
    if not candidates:
        raise DomainError("NO_VALID_SLOT", "No valid slot for that day/week (workout window, calendar or rest days). Pass an exact --start to pin a time anyway")
    slot = max(candidates, key=lambda c: c.score)
    old_start = session.planned_start_at
    cancel_session_jobs(db, session.id, now=now)
    session.user_locked = False
    session.source_revision += 1
    place(db, session, slot, week=week, now=now, crowd_model=CrowdModel(db, now=now) if signals else None)
    return {"session_id": session.id, "from": old_start.isoformat(), "to": slot.start.isoformat(), "end": slot.end.isoformat(), "crowd": slot.crowd, "confidence": slot.confidence}
