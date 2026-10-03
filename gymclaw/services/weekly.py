"""Deterministic rolling maintenance, weekly audit and concise briefing data."""
from datetime import date, datetime, timedelta
from hashlib import sha256
import json
from statistics import median
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, CalendarEventSnapshot, CalendarSyncState, CalendarWrite, CrowdFeedback, LearnedPreference, OnboardingState, PlannedSession, SetLog, WorkoutExercise, WorkoutSession
from gymclaw.services.calendar import allocation, get_week, week_of
from gymclaw.services.calendar_writes import queue_session_write
from gymclaw.services.notifications import cancel_session_jobs
from gymclaw.services.profile import get_profile
from gymclaw.services.replanning import commit_upcoming, replan_weeks
from gymclaw.services.scheduling import schedule_week
from gymclaw.services.templates import get_template
from gymclaw.services.workout import emit, utc


def audit_week(db: Session, week_start: date, *, now: datetime, learn: bool = False) -> dict:
    if week_start.weekday() != 0:
        raise ValueError("week_start must be Monday")
    now = utc(now)
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    workouts = list(db.scalars(select(WorkoutSession).where(WorkoutSession.status == "PLAN_UPDATED", WorkoutSession.completed_at <= now)))
    completed = [w for w in workouts if w.started_at and week_of(w.started_at, zone) == week_start]
    ids = [w.id for w in completed]
    sets = list(db.scalars(select(SetLog).join(WorkoutExercise).where(WorkoutExercise.workout_session_id.in_(ids)))) if ids else []
    plans = [p for p in db.scalars(select(PlannedSession)) if week_of(p.planned_start_at, zone) == week_start]
    labels = list(db.scalars(select(CrowdFeedback).where(CrowdFeedback.workout_session_id.in_(ids)))) if ids else []
    durations = [w.actual_duration_seconds for w in completed if w.actual_duration_seconds is not None]
    if learn:
        history = [w.actual_duration_seconds for w in workouts if w.started_at and now - timedelta(days=90) <= w.started_at <= now and w.actual_duration_seconds is not None]
        if len(history) >= 3:
            preference = db.scalar(select(LearnedPreference).where(LearnedPreference.key == "typical_session_duration"))
            if preference is None:
                preference = LearnedPreference(key="typical_session_duration", first_observed_at=now, source="completed_workouts")
                db.add(preference)
            preference.value_json = {"median_seconds": median(history), "demo": any(w.template_snapshot.get("demo", False) for w in workouts)}
            preference.evidence_count, preference.confidence = len(history), min(0.9, len(history) / (len(history) + 5))
            preference.last_observed_at = now
    return {"week_start": week_start.isoformat(), "completed_workouts": len(completed), "target_sessions": profile.weekly_target_sessions,
        "planned_sessions": len(plans), "missed_sessions": sum(p.status == "MISSED" for p in plans),
        "skipped_sessions": sum(p.status == "SKIPPED" for p in plans), "cancelled_sessions": sum(p.status == "CANCELLED" for p in plans),
        "working_sets": sum(s.set_type == "WORKING" for s in sets), "warmup_sets": sum(s.set_type == "WARMUP" for s in sets),
        "median_duration_seconds": median(durations) if durations else None, "crowd_labels": len(labels),
        "demo": any(w.template_snapshot.get("demo", False) for w in completed) or any(f.features_json.get("demo", False) for f in labels)}


def mark_missed(db: Session, *, now: datetime) -> list[str]:
    """Elapsed slot is missed, never fabricated completion; retain calendar history."""
    ids = []
    for plan in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]), PlannedSession.planned_end_at < now)):
        plan.status = "MISSED"
        plan.source_revision += 1
        cancel_session_jobs(db, plan.id, now=now)
        for write in db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == plan.id, CalendarWrite.status == "PENDING")):
            write.status, write.handled_at = "CANCELLED", now
        emit(db, "workout.missed", now, {"session_id": plan.id, "reason": "planned_window_elapsed_without_start"})
        ids.append(plan.id)
    return ids


def first_plan_key(db: Session, week: date, now: datetime, template_id: str) -> str:
    profile = get_profile(db)
    snapshots = [(s.calendar_event_id, s.start_at.isoformat(), s.end_at.isoformat(), s.status, s.raw_json) for s in db.scalars(select(CalendarEventSnapshot).order_by(CalendarEventSnapshot.calendar_event_id))]
    # Stable throughout quiet watcher ticks; calendar/profile changes retry an impossible week.
    material = json.dumps({"profile": profile.model_dump(mode="json"), "calendar": snapshots, "template": template_id,
        "day": now.astimezone(ZoneInfo(profile.timezone)).date().isoformat()}, sort_keys=True)
    return f"rolling:{week}:{sha256(material.encode()).hexdigest()[:32]}"


def assign_rotation(db: Session, *, now: datetime) -> list[dict]:
    """Rotate the owner's split (e.g. Push → Pull → Legs) over upcoming sessions, continuing after the
    last completed workout. User-locked sessions keep their template but still advance the rotation."""
    row = db.get(OnboardingState, 1)
    split = row.split_json if row else []
    if len(split) < 2:
        return []
    last = db.scalar(select(WorkoutSession.template_id).where(WorkoutSession.status == "PLAN_UPDATED").order_by(WorkoutSession.completed_at.desc()).limit(1))
    position = split.index(last) + 1 if last in split else 0
    changed = []
    for session in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"])).order_by(PlannedSession.planned_start_at, PlannedSession.id)):
        wanted = split[position % len(split)]
        if session.user_locked:
            position = split.index(session.workout_template_id) + 1 if session.workout_template_id in split else position + 1
            continue
        position += 1
        if session.workout_template_id == wanted:
            continue
        session.workout_template_id = wanted
        # Drop the old template snapshot so allocation uses the rotated template.
        session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if k.startswith("crowd_")}
        session.source_revision += 1
        allocation(db, session)
        queue_session_write(db, session, now=now)
        changed.append({"session_id": session.id, "template_id": wanted})
    if changed:
        emit(db, "planning.rotation_assigned", now, {"changes": changed})
    return changed


CROWD_RECHECK = timedelta(minutes=30)
STALE_WORKOUT = timedelta(hours=3)


def close_stale_workouts(db: Session, *, now: datetime) -> list[str]:
    """A workout left open (chat reset, phone away) would block the next start forever. After 3 idle
    hours: skip what's left as abandoned and finish it, so logged sets and progression are kept."""
    from gymclaw.services import adaptation, audit
    from gymclaw.services.workout import UNRESOLVED, exercises
    closed = []
    for workout in db.scalars(select(WorkoutSession).where(WorkoutSession.status != "PLAN_UPDATED", WorkoutSession.last_action_at < now - STALE_WORKOUT)):
        # Close at the last action, so duration means training time, not time until we noticed.
        ended = workout.last_action_at
        for row in exercises(db, workout):
            if row.status in UNRESOLVED:
                adaptation.skip_exercise(db, workout.id, row.id, reason="abandoned", now=ended, request_id=f"stale:{workout.id}:{row.id}")
        if workout.status == "WORKOUT_COMPLETE":
            audit.finish(db, workout.id, now=ended, request_id=f"stale:{workout.id}:finish")
            closed.append(workout.id)
    return closed


def significant(old: dict | None, new: dict | None) -> bool:
    """Worth a calendar edit: first data, a different feel, or ≥3 people and ≥25% change."""
    if not new:
        return False
    if not old:
        return True
    if old.get("feel") != new.get("feel"):
        return True
    before, after = old.get("count"), new.get("count")
    if before is None or after is None:
        return before != after
    return abs(after - before) >= 3 and abs(after - before) >= 0.25 * max(before, 1)


def refresh_crowd(db: Session, *, now: datetime) -> list[dict]:
    """Keep 'Expected crowd' in calendar events current without churn: fill unknown forecasts, then
    re-check sessions in the next 24 h every 30 min and update only on a significant change. Nothing
    within 1 h of start, so imminent reminders aren't rescheduled."""
    from gymclaw.services.crowd import CrowdModel
    model = CrowdModel(db, now=now)
    changed = []
    for session in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]), PlannedSession.planned_start_at > now + timedelta(hours=1))):
        plan = session.workout_plan_json
        old = plan.get("crowd_forecast")
        checked = plan.get("crowd_checked_at")
        if old and (session.planned_start_at > now + timedelta(hours=24) or checked and now - datetime.fromisoformat(checked) < CROWD_RECHECK):
            continue
        new = model.forecast(session.planned_start_at)
        update = significant(old, new)
        session.workout_plan_json = plan | {"crowd_checked_at": now.isoformat()} | ({"crowd_forecast": new} if update else {})
        if update:
            session.source_revision += 1
            # Description-only edit for published events; unpublished ones get a full create.
            queue_session_write(db, session, now=now, metadata_only=bool(session.calendar_event_id))
            changed.append({"session_id": session.id, "forecast": new})
    return changed


def refresh_plans(db: Session, *, now: datetime) -> list[str]:
    """Planned sessions store a copy of their template. After template edits or swaps, re-allocate
    from the current template so the calendar and the workout use today's plan."""
    from gymclaw.models import WorkoutTemplate
    changed = []
    for session in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]), PlannedSession.workout_template_id.is_not(None))):
        row = db.get(WorkoutTemplate, session.workout_template_id)
        if row is None:
            continue
        current = get_template(db, row.id).model_dump(mode="json")
        stored = session.workout_plan_json.get("template") or {}
        # Allocation tunes set duration from history; only the plan itself matters here.
        if all(stored.get(key) == current[key] for key in ("name", "exercises", "alternatives")):
            continue
        session.workout_plan_json = {k: v for k, v in session.workout_plan_json.items() if k.startswith("crowd_")}
        allocation(db, session)
        session.source_revision += 1
        queue_session_write(db, session, now=now, metadata_only=bool(session.calendar_event_id))
        changed.append(session.id)
    return changed


def rolling_plan(db: Session, template_id: str, *, now: datetime) -> dict:
    now = utc(now)
    get_template(db, template_id)  # Explicit selected template, never guess from demo import.
    zone = ZoneInfo(get_profile(db).timezone)
    current_week = week_of(now, zone)
    next_week = current_week + timedelta(days=7)
    missed = mark_missed(db, now=now)
    triggers = [event for event in db.scalars(select(AgentEvent).where(AgentEvent.type == "planning.replan_required", AgentEvent.handled_at.is_(None)))
        if event.payload_json.get("reason") in {"crowd_feedback", "workout_completed"}]
    reconsider = any(event.payload_json.get("reason") == "crowd_feedback" for event in triggers)
    repaired = replan_weeks(db, {current_week, next_week}, now=now, reconsider_tentative=reconsider)
    for event in triggers:
        event.handled_at = now
    created = []
    for week in (current_week, next_week):
        start = datetime.combine(week, datetime.min.time(), zone)
        # Start new next-week planning only inside rolling 10-day horizon.
        if start > now + timedelta(days=3):
            continue
        if not db.scalar(select(PlannedSession.id).where(PlannedSession.week_id == week.isoformat())):
            data = schedule_week(db, week, now=now, template_id=template_id, request_id=first_plan_key(db, week, now, template_id))
            created.extend(s["id"] for s in data["sessions"])
    for session_id in created:
        queue_session_write(db, db.get(PlannedSession, session_id), now=now)
    stale = close_stale_workouts(db, now=now)
    rotation = assign_rotation(db, now=now)
    plans = refresh_plans(db, now=now)
    crowd = refresh_crowd(db, now=now)
    commit_upcoming(db, now=now)
    return {"missed": missed, "created": created, "rotation": rotation, "crowd": crowd, "closed_workouts": stale, "refreshed_plans": plans, **repaired}


def weekly_plan(db: Session, template_id: str, *, now: datetime) -> dict:
    """One durable weekly action. Retry reads current plan, preserves original event ID."""
    now = utc(now)
    zone = ZoneInfo(get_profile(db).timezone)
    current_week = week_of(now, zone)
    next_week = current_week + timedelta(days=7)
    previous = current_week if now.astimezone(zone).weekday() == 6 else current_week - timedelta(days=7)
    rolling = rolling_plan(db, template_id, now=now)
    report = audit_week(db, previous, now=now, learn=True)
    schedule_week(db, next_week, now=now, template_id=template_id, request_id=f"weekly-plan:{next_week}:{template_id}")
    assign_rotation(db, now=now)
    commit_upcoming(db, now=now)
    for session in db.scalars(select(PlannedSession).where(PlannedSession.week_id == next_week.isoformat(), PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]))):
        queue_session_write(db, session, now=now)
    key = f"weekly-brief:{next_week}"
    event = db.scalar(select(AgentEvent).where(AgentEvent.correlation_id == key))
    if event is None:
        event = emit(db, "planning.weekly_briefing", now, {"week_start": next_week.isoformat(), "template_id": template_id})
        event.correlation_id = key
    return {"week_start": next_week.isoformat(), "audit": report, "rolling": rolling, "plan": get_week(db, next_week), "event_id": event.id}


def briefing(db: Session, week_start: date, report: dict, *, published: bool) -> str:
    zone = ZoneInfo(get_profile(db).timezone)
    plan = get_week(db, week_start)
    state = db.get(CalendarSyncState, 1)
    demo = report.get("demo") or state is not None and state.source == "fixture" or any(s["workout_plan"].get("crowd_source") == "demo_fixture" for s in plan["sessions"])
    lines = ["GYMCLAW · NEXT WEEK" + (" · DEMO" if demo else "")]
    if not published:
        lines.append("Local plan only — calendar publication not confirmed.")
    for session in plan["sessions"]:
        if session["status"] not in {"TENTATIVE", "COMMITTED"}:
            continue
        at = datetime.fromisoformat(session["start"]).astimezone(zone)
        name = session["workout_plan"].get("template", {}).get("name", "Workout")
        tentative = " · tentative" if session["status"] == "TENTATIVE" else ""
        lines.append(f"{at:%a %H:%M} · {name}{tentative}")
        if session["crowd"] is not None:
            lines.append(f"Crowd personal score: {session['crowd']:.2f}/1 · heuristic confidence {session['confidence'] or 0:.0%}")
    if len(lines) <= (1 if published else 2):
        lines.append("No valid sessions. Recovery/calendar constraints kept.")
    lines.append(f"Last week: {report['completed_workouts']}/{report['target_sessions']} completed.")
    return "\n".join(lines)
