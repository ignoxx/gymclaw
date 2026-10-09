"""Preserve manual intent; repair only invalid unlocked future sessions."""
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import CalendarWrite, PlannedSession
from gymclaw.services.calendar import allocation, week_of
from gymclaw.services.calendar_busy import busy_intervals
from gymclaw.services.calendar_writes import queue_session_write
from gymclaw.services.notifications import cancel_session_jobs, schedule_session_jobs
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import Candidate, Interval, generate_candidates, overlaps, plan_week, recovery_ok
from gymclaw.services.profile import get_profile
from gymclaw.services.templates import get_template
from gymclaw.services.workout import emit, mutate, utc

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
        # Skipped sessions count toward the week: the owner chose not to make them up.
        fixed = [s for s in all_rows if s.status == "SKIPPED" or s.status in ACTIVE and (s.user_locked or s.status in {"STARTED", "COMPLETED"} or s.planned_start_at < now or week_of(s.planned_start_at, zone) != week)]
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


def rule_breaks(db: Session, session: PlannedSession, slot: Candidate, profile) -> list[str]:
    """Planner rules an owner-chosen slot breaks, as short reasons for the owner."""
    zone = ZoneInfo(profile.timezone)
    start, end = slot.start.astimezone(zone), slot.end.astimezone(zone)
    reasons = []
    if start.weekday() not in profile.weekdays_allowed or start.date() != end.date() or start.time() < profile.earliest_workout_start or end.time() > profile.latest_workout_finish:
        reasons.append(f"outside your workout window ({profile.earliest_workout_start:%H:%M}–{profile.latest_workout_finish:%H:%M})")
    day = datetime.combine(start.date(), datetime.min.time(), zone)
    if any(overlaps(slot.prep_start, slot.home_at, b) for b in busy_intervals(db, day - timedelta(days=1), day + timedelta(days=2))):
        reasons.append("overlaps a calendar event")
    others = db.scalars(select(PlannedSession).where(PlannedSession.status.in_(ACTIVE), PlannedSession.id != session.id))
    if any(not recovery_ok(slot.start, slot.end, interval(other), profile) for other in others):
        reasons.append("skips a rest day")
    return reasons


def pick_slot(db: Session, session: PlannedSession, *, now: datetime, minutes: int, anchor: datetime, to: datetime | None = None, day: date | None = None, after: datetime | None = None):
    """Where an owner-requested session goes → (slot, week, warnings, alternatives, signals). `to` is an
    exact start the owner named: a planner slot if one matches, otherwise kept anyway (`minutes` long).
    Without `to`, the best-scoring slot at/after `after`, on `day` (else in `anchor`'s week); if rest days
    leave none, the best slot ignoring them. `warnings` names the planner rules the slot breaks."""
    for name, value in (("--to", to), ("--after", after)):
        if value is not None and (value.tzinfo is None or value.utcoffset() is None):
            raise ValueError(f"{name} must include a UTC offset")
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    target_day = to.astimezone(zone).date() if to else day or (after.astimezone(zone).date() if after else None)
    week = week_of(datetime.combine(target_day, datetime.min.time(), zone) if target_day else anchor, zone)
    window_start = datetime.combine(week - timedelta(days=1), datetime.min.time(), zone)
    others = tuple(interval(s) for s in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(ACTIVE), PlannedSession.id != session.id)))
    deleted = tuple(interval(s) for s in db.scalars(select(PlannedSession).where(PlannedSession.status == "CANCELLED")) if s.workout_plan_json.get("deleted_by_user"))
    from gymclaw.services.crowd import planning_signals
    signals = planning_signals(db, week, now=now)
    busy = busy_intervals(db, window_start, window_start + timedelta(days=9))

    def candidates(existing):
        return [c for c in generate_candidates(profile, week, busy=busy, unavailable=deleted, existing=existing, signals=signals, now=now)
            if (target_day is None or c.start.astimezone(zone).date() == target_day) and (after is None or c.start >= utc(after))]

    found = candidates(others)
    alternatives: list[Candidate] = []
    if to is not None:
        slot = next((c for c in found if c.start == utc(to)), None)
        if slot is None:
            # The owner leads: keep their time even off the planner's grid or rules.
            start = utc(to)
            end = start + timedelta(minutes=minutes)
            if end <= now:
                raise DomainError("SLOT_IN_PAST", f"{to.astimezone(zone):%a %H:%M} would already be over")
            slot = Candidate(start=start, end=end, prep_start=start - timedelta(minutes=profile.prep_minutes + profile.commute_to_gym_minutes),
                leave_home=start - timedelta(minutes=profile.commute_to_gym_minutes), home_at=end + timedelta(minutes=profile.commute_home_minutes),
                crowd=None, confidence=0, score=0, score_parts={})
        return slot, week, rule_breaks(db, session, slot, profile), alternatives, signals
    # A day the owner asked for beats the rest-day rule; the warning says so.
    found = found or (candidates(()) if target_day else [])
    if not found:
        raise DomainError("NO_VALID_SLOT", "No valid slot for that day/week (workout window or calendar); offer a time and use --to")
    ranked = sorted(found, key=lambda c: c.score, reverse=True)
    slot = ranked[0]
    for c in ranked[1:]:
        if len(alternatives) < 2 and all(abs(c.start - other.start) >= timedelta(minutes=60) for other in (slot, *alternatives)):
            alternatives.append(c)
    return slot, week, rule_breaks(db, session, slot, profile), alternatives, signals


def slot_data(session: PlannedSession, slot: Candidate, warnings: list[str], alternatives: list[Candidate]) -> dict:
    return {"session_id": session.id, "to": slot.start.isoformat(), "end": slot.end.isoformat(), "crowd": slot.crowd, "confidence": slot.confidence,
        "workout_template_id": session.workout_template_id, "warnings": warnings,
        "alternatives": [{"start": c.start.isoformat(), "crowd": c.crowd} for c in sorted(alternatives, key=lambda c: c.start)]}


def move_session(db: Session, session_id: str, *, now: datetime, request_id: str, to: datetime | None = None, day: date | None = None, after: datetime | None = None) -> dict:
    """Owner-requested move of one session that hasn't ended (a no-show can still be moved), including
    one the owner edited in the calendar. Placement follows `pick_slot`; a slot that breaks the rules is
    locked against replanning. `alternatives` lists the next-best starts (spread apart) so the owner can
    pick another in one tap."""
    now = utc(now)

    def action():
        session = db.get(PlannedSession, session_id)
        if session is None or session.status not in {"TENTATIVE", "COMMITTED"} or session.planned_end_at <= now:
            raise DomainError("SESSION_NOT_MOVABLE", "Only tentative or committed sessions that haven't ended can be moved")
        minutes = int((session.planned_end_at - session.planned_start_at).total_seconds() // 60)
        slot, week, warnings, alternatives, signals = pick_slot(db, session, now=now, minutes=minutes, anchor=session.planned_start_at, to=to, day=day, after=after)
        old_start = session.planned_start_at
        if slot.start != old_start or slot.end != session.planned_end_at:
            cancel_session_jobs(db, session.id, now=now)
        session.source_revision += 1
        # A slot that breaks the rules would be "repaired" by the next replan; the lock keeps the owner's choice.
        # An existing lock (calendar edit, owner-chosen workout) stays.
        session.user_locked = session.user_locked or bool(warnings)
        from gymclaw.services.crowd import CrowdModel
        place(db, session, slot, week=week, now=now, crowd_model=CrowdModel(db, now=now) if signals else None)
        return {"from": old_start.isoformat()} | slot_data(session, slot, warnings, alternatives)

    return mutate(db, "planning.session_moved", request_id, {"session_id": session_id, "to": to.isoformat() if to else None, "day": day.isoformat() if day else None}
        | ({"after": after.isoformat()} if after else {}), now, action)


def add_session(db: Session, *, now: datetime, request_id: str, template_id: str | None = None, to: datetime | None = None, day: date | None = None, after: datetime | None = None) -> dict:
    """Owner wants an extra session on top of the week's plan. Placement follows `pick_slot` (default:
    this week). It is locked, so replanning never drops it for exceeding the weekly maximum. Without
    `template_id` it takes its place in the rotation."""
    now = utc(now)

    def action():
        from gymclaw.services.crowd import CrowdModel
        from gymclaw.services.weekly import assign_rotation
        if template_id:
            get_template(db, template_id)
        profile = get_profile(db)
        session = PlannedSession(workout_template_id=template_id, user_locked=True)
        slot, week, warnings, alternatives, signals = pick_slot(db, session, now=now, minutes=profile.preferred_workout_minutes, anchor=now, to=to, day=day, after=after)
        db.add(session)
        place(db, session, slot, week=week, now=now, crowd_model=CrowdModel(db, now=now) if signals else None)
        if template_id is None:
            assign_rotation(db, now=now)
        return slot_data(session, slot, warnings, alternatives)

    return mutate(db, "planning.session_added", request_id, {"template_id": template_id, "to": to.isoformat() if to else None, "day": day.isoformat() if day else None}
        | ({"after": after.isoformat()} if after else {}), now, action)


def set_workout(db: Session, session_id: str, template_id: str, *, now: datetime, request_id: str) -> dict:
    """Owner picks the workout for one session ("legs today instead of pull"). The session is locked so
    rotation keeps it; later sessions rotate on from it."""
    now = utc(now)

    def action():
        from gymclaw.services.weekly import assign_rotation
        session = db.get(PlannedSession, session_id)
        if session is None or session.status not in {"TENTATIVE", "COMMITTED"}:
            raise DomainError("SESSION_NOT_EDITABLE", "Only tentative or committed sessions can change workout; a running one switches exercises instead")
        get_template(db, template_id)
        old = session.workout_template_id
        session.workout_template_id = template_id
        session.user_locked = True
        session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if k.startswith("crowd_")}
        session.source_revision += 1
        allocation(db, session)
        queue_session_write(db, session, now=now)
        return {"session_id": session.id, "from": old, "to": template_id, "rotation": assign_rotation(db, now=now)}

    return mutate(db, "planning.workout_set", request_id, {"session_id": session_id, "template_id": template_id}, now, action)


def retire_session(db: Session, session: PlannedSession, status: str, *, now: datetime):
    """End a session that won't happen. Its calendar event stays as history; reminders and unsent edits are dropped."""
    session.status = status
    session.source_revision += 1
    cancel_session_jobs(db, session.id, now=now)
    for write in db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == session.id, CalendarWrite.status == "PENDING")):
        write.status, write.handled_at = "CANCELLED", now


def skip_session(db: Session, session_id: str, *, now: datetime, request_id: str) -> dict:
    """Owner skips a session, upcoming or due (e.g. answering the no-show nudge). It still counts
    toward the week, so no make-up session is added; `move_session` is the make-up path."""
    now = utc(now)

    def action():
        session = db.get(PlannedSession, session_id)
        if session is None or session.status not in {"TENTATIVE", "COMMITTED"} or session.planned_end_at <= now:
            raise DomainError("SESSION_NOT_SKIPPABLE", "Only tentative or committed sessions that haven't ended can be skipped")
        retire_session(db, session, "SKIPPED", now=now)
        return {"session_id": session.id, "status": session.status}

    return mutate(db, "planning.session_skipped", request_id, {"session_id": session_id}, now, action)
