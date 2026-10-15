"""Session placement mechanics and planning issue detection; decisions belong to the agent."""
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
from gymclaw.services.planning import Candidate, Interval, generate_candidates, overlaps, recovery_ok
from gymclaw.services.profile import get_profile
from gymclaw.services.templates import get_template
from gymclaw.services.workout import mutate, utc

ACTIVE = {"TENTATIVE", "COMMITTED", "STARTED", "COMPLETED"}


def interval(session):
    return Interval(start=session.planned_start_at, end=session.planned_end_at)


def expanded(session, profile):
    return (session.planned_start_at - timedelta(minutes=profile.prep_minutes + profile.commute_to_gym_minutes), session.planned_end_at + timedelta(minutes=profile.commute_home_minutes))


def commit_upcoming(db: Session, *, now: datetime):
    for session in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]))):
        if session.status == "TENTATIVE" and session.workout_plan_json.get("agent_status") != "TENTATIVE" and now < session.planned_start_at <= now + timedelta(hours=48):
            session.status = "COMMITTED"
            session.source_revision += 1
            queue_session_write(db, session, now=now)
        schedule_session_jobs(db, session, now=now)


def place(db: Session, session: PlannedSession, slot: Candidate, *, week: date, now: datetime, crowd_model, status: str | None = None):
    """Put a session on a planner slot: times, crowd forecast, workout fit, calendar intent and reminders."""
    session.planned_start_at, session.planned_end_at = slot.start, slot.end
    session.prep_start_at, session.leave_home_at = slot.prep_start, slot.leave_home
    session.expected_finish_at = slot.end
    session.crowd_prediction, session.crowd_confidence = slot.crowd, slot.confidence
    session.status = status or ("COMMITTED" if slot.start <= now + timedelta(hours=48) else "TENTATIVE")
    session.week_id = week.isoformat()
    db.flush()
    allocation(db, session)
    session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if not k.startswith("crowd_")}
    if slot.crowd is not None:
        session.workout_plan_json |= {"crowd_source": "demo_fixture" if crowd_model.predict(slot.start)["demo"] else "local_crowd_model", "crowd_score_kind": "personal_perceived_crowd_proxy"}
    queue_session_write(db, session, now=now)
    schedule_session_jobs(db, session, now=now)


def replan_weeks(db: Session, weeks: set[date], *, now: datetime) -> dict:
    """Compatibility entry point: detect planning issues, leave every decision to the agent."""
    from gymclaw.services.events import attention
    now = utc(now)
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    rows = list(db.scalars(select(PlannedSession)))
    warnings = []
    for week in sorted(weeks):
        if week.weekday() != 0:
            raise ValueError("week_start must be Monday")
        week_rows = [s for s in rows if week_of(s.planned_start_at, zone) == week]
        if not week_rows:
            continue
        issues = []
        active = [s for s in week_rows if s.status in ACTIVE]
        for session in active:
            if session.status in {"STARTED", "COMPLETED"} or session.planned_end_at <= now:
                continue
            slot = Candidate(start=session.planned_start_at, end=session.planned_end_at,
                prep_start=session.prep_start_at, leave_home=session.leave_home_at,
                home_at=session.planned_end_at + timedelta(minutes=profile.commute_home_minutes),
                crowd=session.crowd_prediction, confidence=session.crowd_confidence or 0, score=0, score_parts={})
            reasons = rule_breaks(db, session, slot, profile)
            if session.workout_plan_json.get("error"):
                reasons.append("too short for required movements")
            if reasons:
                issues.append({"session_id": session.id, "revision": session.source_revision, "warnings": reasons})
                warnings.extend(f"Session {session.id}: {reason}" for reason in reasons)
        if len(active) > profile.weekly_max_sessions:
            warnings.append(f"Week {week}: exceeds weekly maximum")
            issues.append({"weekly_max": profile.weekly_max_sessions, "count": len(active)})
        # Skips are explicit intent; they don't ask the agent for a make-up every minute.
        count = len(active) + sum(s.status == "SKIPPED" for s in week_rows)
        if count < profile.weekly_target_sessions:
            warnings.append(f"Week {week}: {count}/{profile.weekly_target_sessions} sessions planned")
            issues.append({"target": profile.weekly_target_sessions, "count": count})
        if issues:
            attention(db, "planning.attention_required", {"week_start": week.isoformat(), "issues": issues}, now=now)
    return {"changed": [], "warnings": list(dict.fromkeys(warnings))}


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


def upcoming(db: Session) -> dict[str, tuple]:
    return {s.id: (s.status, s.planned_start_at, s.workout_template_id) for s in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"])))}


def side_effects(db: Session, before: dict[str, tuple], session_id: str) -> list[dict]:
    """Other upcoming sessions an owner edit changed (from `upcoming` taken before it), so a reply never claims they stayed."""
    changed = []
    for other_id, old in before.items():
        other = db.get(PlannedSession, other_id)
        if other_id != session_id and (other.status, other.planned_start_at, other.workout_template_id) != old:
            changed.append({"session_id": other_id, "status": other.status, "start": other.planned_start_at.isoformat(), "workout_template_id": other.workout_template_id})
    return changed


def slot_data(session: PlannedSession, slot: Candidate, warnings: list[str], alternatives: list[Candidate]) -> dict:
    return {"session_id": session.id, "to": slot.start.isoformat(), "end": slot.end.isoformat(), "crowd": slot.crowd, "confidence": slot.confidence,
        "workout_template_id": session.workout_template_id, "warnings": warnings,
        "alternatives": [{"start": c.start.isoformat(), "crowd": c.crowd} for c in sorted(alternatives, key=lambda c: c.start)]}


def move_session(db: Session, session_id: str, *, now: datetime, request_id: str, to: datetime | None = None, day: date | None = None, after: datetime | None = None) -> dict:
    """Owner-requested move of one session that hasn't ended (a no-show can still be moved), including
    one the owner edited in the calendar. Placement follows `pick_slot`; the session is locked, so
    replanning neither moves it nor "repairs" other sessions around it. `alternatives` lists the next-best starts (spread apart) so the owner can
    pick another in one tap."""
    now = utc(now)

    def action():
        session = db.get(PlannedSession, session_id)
        if session is None or session.status not in {"TENTATIVE", "COMMITTED"} or session.planned_end_at <= now:
            raise DomainError("SESSION_NOT_MOVABLE", "Only tentative or committed sessions that haven't ended can be moved")
        minutes = int((session.planned_end_at - session.planned_start_at).total_seconds() // 60)
        slot, week, warnings, alternatives, signals = pick_slot(db, session, now=now, minutes=minutes, anchor=session.planned_start_at, to=to, day=day, after=after)
        old_start, before = session.planned_start_at, upcoming(db)
        if slot.start != old_start or slot.end != session.planned_end_at:
            cancel_session_jobs(db, session.id, now=now)
        session.source_revision += 1
        session.user_locked = True
        from gymclaw.services.crowd import CrowdModel
        place(db, session, slot, week=week, now=now, crowd_model=CrowdModel(db, now=now) if signals else None)
        return {"from": old_start.isoformat()} | slot_data(session, slot, warnings, alternatives) | {"side_effects": side_effects(db, before, session.id)}

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
        profile, before = get_profile(db), upcoming(db)
        session = PlannedSession(workout_template_id=template_id, user_locked=True)
        slot, week, warnings, alternatives, signals = pick_slot(db, session, now=now, minutes=profile.preferred_workout_minutes, anchor=now, to=to, day=day, after=after)
        crowd_model = CrowdModel(db, now=now) if signals else None  # Queries before `add`: autoflush can't insert a session without times.
        db.add(session)
        place(db, session, slot, week=week, now=now, crowd_model=crowd_model)
        if template_id is None:
            assign_rotation(db, now=now, session_ids={session.id})
        return slot_data(session, slot, warnings, alternatives) | {"side_effects": side_effects(db, before, session.id)}

    return mutate(db, "planning.session_added", request_id, {"template_id": template_id, "to": to.isoformat() if to else None, "day": day.isoformat() if day else None}
        | ({"after": after.isoformat()} if after else {}), now, action)


def retire_session(db: Session, session: PlannedSession, status: str, *, now: datetime):
    """End a session that won't happen. Its calendar event stays as history; reminders and unsent edits are dropped."""
    session.status = status
    session.source_revision += 1
    cancel_session_jobs(db, session.id, now=now)
    for write in db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == session.id, CalendarWrite.status == "PENDING")):
        write.status, write.handled_at = "CANCELLED", now
