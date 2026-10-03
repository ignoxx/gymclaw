"""Read-only projection for the localhost dashboard. Same code for live state and the demo DB.

Run inside the sandbox (`python -m gymclaw.dashboard_state`); opens SQLite read-only, prints JSON.
"""
from collections import defaultdict
from datetime import date, datetime, timedelta, timezone
import json
import os
from statistics import mean
from urllib.parse import quote
from zoneinfo import ZoneInfo

from sqlalchemy import create_engine, select
from sqlalchemy.engine import make_url
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, CrowdObservation, PlannedSession, RuntimeSettings, SetLog, UserProfile, WorkoutExercise, WorkoutSession
from gymclaw.services.illustrations import for_exercise
from gymclaw.services.preview import last_set
from gymclaw.services.profile import Profile
from gymclaw.services.templates import get_template
from gymclaw.services.workout import current, logs

ACTIVITY = {"workout.started": "Workout started", "workout.completed": "Workout finished", "workout.set_logged": "Set logged",
    "workout.substituted": "Exercise swapped", "planning.week_planned": "Week planned", "planning.rotation_assigned": "Split rotated",
    "calendar.write_applied": "Calendar updated", "notifications.sent": "Reminder sent", "crowd.feedback_received": "Crowd rated"}


def hhmm(value: datetime, zone: ZoneInfo) -> str:
    return value.astimezone(zone).strftime("%H:%M")


def svg(name: str, guide_id: str | None) -> str | None:
    picture = for_exercise(name, guide_id)
    return picture["svg_url"] if picture else None


def days(db: Session, zone: ZoneInfo, today: date) -> list[dict]:
    """The next seven days from today, each with its session (if any)."""
    start = datetime.combine(today, datetime.min.time(), zone)
    sessions = {row.planned_start_at.astimezone(zone).date(): row for row in db.scalars(select(PlannedSession).where(
        PlannedSession.planned_start_at >= start, PlannedSession.planned_start_at < start + timedelta(days=7),
        PlannedSession.status.not_in(["CANCELLED", "MISSED"])).order_by(PlannedSession.planned_start_at))}
    result = []
    for offset in range(7):
        day = today + timedelta(days=offset)
        row = sessions.get(day)
        session = None
        if row:
            forecast = row.workout_plan_json.get("crowd_forecast") or {}
            session = {"start": hhmm(row.planned_start_at, zone), "end": hhmm(row.planned_end_at, zone), "status": row.status,
                "template": row.workout_plan_json.get("template", {}).get("name", "Workout"),
                "crowd": forecast.get("feel"), "count": forecast.get("count")}
        result.append({"name": day.strftime("%a"), "date": day.day, "today": offset == 0, "session": session})
    return result


def next_session(db: Session, zone: ZoneInfo, now: datetime) -> dict | None:
    row = db.scalar(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]), PlannedSession.planned_end_at > now,
        PlannedSession.workout_template_id.is_not(None)).order_by(PlannedSession.planned_start_at).limit(1))
    if row is None:
        return None
    template = get_template(db, row.workout_template_id)
    sets = row.workout_plan_json.get("sets", {})
    forecast = row.workout_plan_json.get("crowd_forecast") or {}
    exercises = []
    for spec in template.exercises:
        last = last_set(db, spec.id)
        exercises.append({"name": spec.name, "svg_url": svg(spec.name, spec.guide_id), "sets": sets.get(spec.id, spec.working_sets),
            "reps": f"{spec.rep_min}" if spec.rep_min == spec.rep_max else f"{spec.rep_min}–{spec.rep_max}",
            "last": f"{last.weight:g} × {last.reps}" if last else None})
    start = row.planned_start_at.astimezone(zone)
    return {"template": template.name, "when": f"{start:%a} {start:%H:%M}", "leave": hhmm(row.leave_home_at, zone),
        "crowd": forecast.get("feel"), "count": forecast.get("count"), "exercises": exercises}


def active_workout(db: Session, zone: ZoneInfo, now: datetime) -> dict | None:
    workout = db.scalar(select(WorkoutSession).where(WorkoutSession.status != "PLAN_UPDATED", WorkoutSession.started_at.is_not(None))
        .order_by(WorkoutSession.started_at.desc()).limit(1))
    if workout is None:
        return None
    data = current(db, workout.id, now=now)
    rows = list(db.scalars(select(WorkoutExercise).where(WorkoutExercise.workout_session_id == workout.id).order_by(WorkoutExercise.position)))
    exercises = [{"id": row.id, "name": row.config_json["name"], "svg_url": svg(row.config_json["name"], row.config_json.get("guide_id")),
        "sets": row.planned_working_sets, "done": len(logs(db, row, "WORKING")), "status": row.status}
        for row in rows if row.status not in {"SUBSTITUTED"}]
    active = data["active_exercise"]
    latest = db.scalar(select(SetLog).join(WorkoutExercise).where(WorkoutExercise.workout_session_id == workout.id, SetLog.set_type == "WORKING")
        .order_by(SetLog.logged_at.desc()).limit(1))
    return {"template": workout.template_snapshot.get("name", "Workout"), "eta": hhmm(datetime.fromisoformat(data["eta"]), zone),
        "active": {"id": active["id"], "name": active["name"], "set_type": active["set_type"], "set_number": active["set_number"],
            "sets": active["working_sets"], "weight": active["target_weight"], "rep_min": active["rep_min"], "rep_max": active["rep_max"]} if active else None,
        "rest_until": data["rest_job"]["due_at"] if data["rest_job"] else None,
        "last_set": f"{latest.weight:g} × {latest.reps}" if latest else None, "exercises": exercises}


def crowd(db: Session, zone: ZoneInfo, now: datetime, polling: bool) -> dict:
    """Latest count, the most recent day's readings, and a typical curve for that weekday."""
    rows = [row for row in db.scalars(select(CrowdObservation).where(CrowdObservation.source == "GYM_API", CrowdObservation.observed_at <= now,
        CrowdObservation.observed_at >= now - timedelta(days=90)).order_by(CrowdObservation.observed_at))]
    if not rows:
        return {"now": None, "day": None, "readings": [], "typical": [], "polling": polling}
    latest = rows[-1]
    day = latest.observed_at.astimezone(zone).date()
    readings = [{"time": hhmm(row.observed_at, zone), "count": row.raw_value} for row in rows if row.observed_at.astimezone(zone).date() == day]
    # Typical: per hour, mean of daily means on the same weekday (other dates only).
    hourly = defaultdict(lambda: defaultdict(list))
    for row in rows:
        local = row.observed_at.astimezone(zone)
        if local.date() != day and local.weekday() == day.weekday():
            hourly[local.hour][local.date()].append(row.raw_value)
    typical = [{"hour": hour, "count": round(mean(mean(v) for v in dates.values()), 1)} for hour, dates in sorted(hourly.items())]
    return {"now": {"count": latest.raw_value, "at": latest.observed_at.isoformat(), "time": hhmm(latest.observed_at, zone)},
        "day": "Today" if day == now.astimezone(zone).date() else day.strftime("%a %d %b"), "weekday": day.strftime("%A"),
        "readings": readings, "typical": typical, "polling": polling}


def snapshot(db: Session, *, now: datetime, demo: bool = False) -> dict:
    row = db.get(UserProfile, 1)
    profile = Profile.model_validate(row) if row else Profile()
    zone = ZoneInfo(profile.timezone)
    today = now.astimezone(zone).date()
    settings = db.get(RuntimeSettings, 1)
    events = list(db.execute(select(AgentEvent.type, AgentEvent.created_at).where(AgentEvent.type.in_(ACTIVITY))
        .order_by(AgentEvent.created_at.desc()).limit(12)))
    return {"demo": demo, "live_database_accessed": not demo, "captured_at": now.isoformat(), "timezone": profile.timezone,
        "range": f"{today:%-d %b} – {today + timedelta(days=6):%-d %b}", "days": days(db, zone, today),
        "next": next_session(db, zone, now), "workout": active_workout(db, zone, now),
        "crowd": crowd(db, zone, now, bool(settings and settings.enabled and settings.crowd_polling_enabled)),
        "activity": [{"time": hhmm(e.created_at, zone), "day": e.created_at.astimezone(zone).strftime("%a"), "text": ACTIVITY[e.type]} for e in events]}


def main():
    engine = None
    try:
        url = make_url(os.environ.get("GYMCLAW_DB_URL", "sqlite:///data/gymclaw.db"))
        if url.get_backend_name() != "sqlite" or not url.database or url.database == ":memory:":
            raise ValueError("Persistent SQLite required")
        # SQLite URI mode=ro prevents creation, migration and accidental writes.
        engine = create_engine(f"sqlite:///file:{quote(url.database, safe='/')}?mode=ro&uri=true")
        with Session(engine) as db:
            value = snapshot(db, now=datetime.now(timezone.utc))
        print(json.dumps(value, allow_nan=False))
    except Exception:
        print(json.dumps({"ok": False, "error": "LIVE_STATE_UNAVAILABLE"}))
        return 1
    finally:
        if engine:
            engine.dispose()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
