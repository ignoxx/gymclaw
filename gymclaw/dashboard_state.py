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

from gymclaw.models import AgentEvent, BodyWeight, CrowdFeedback, CrowdObservation, PlannedSession, RuntimeSettings, SetLog, UserProfile, WorkoutExercise, WorkoutSession, WorkoutTemplate
from gymclaw.services.crowd import polling_health
from gymclaw.services.illustrations import for_exercise, primary_muscle, search
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
            "last": f"{last.reps} × {last.weight:g} kg" if last else None})
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
        "last_set": f"{latest.reps} × {latest.weight:g} kg" if latest else None, "exercises": exercises}


def local_minutes(at: datetime, zone: ZoneInfo) -> int:
    """Wall-clock minutes since 1970-01-01 in `zone`; lets the browser slice days/weeks without timezone math."""
    return int((at.astimezone(zone).replace(tzinfo=None) - datetime(1970, 1, 1)).total_seconds() // 60)


def crowd(db: Session, zone: ZoneInfo, now: datetime, polling: bool) -> dict:
    """Latest count plus the full reading history (last ~13 months) as compact [local minute, count] pairs.
    The dashboard derives day/24h/week/month windows and the typical-weekday curve from `series`."""
    rows = list(db.scalars(select(CrowdObservation).where(CrowdObservation.source == "GYM_API", CrowdObservation.observed_at <= now,
        CrowdObservation.observed_at >= now - timedelta(days=400)).order_by(CrowdObservation.observed_at)))
    latest = rows[-1] if rows else None
    return {"now": {"count": latest.raw_value, "at": latest.observed_at.isoformat(), "time": hhmm(latest.observed_at, zone)} if latest else None,
        "local_now": local_minutes(now, zone), "series": [[local_minutes(row.observed_at, zone), row.raw_value] for row in rows],
        "polling": polling}


def closest_muscle(name: str) -> str | None:
    """Last resort for renamed/unmapped exercises: the best catalog match by name."""
    match = search(name, limit=1)
    return match[0]["primary_muscle"] if match else None


def history(db: Session, zone: ZoneInfo, now: datetime) -> list[dict]:
    """Finished workouts, newest first, with every working set. PRs: a heavier top set than any
    earlier session of that exercise."""
    workouts = list(db.scalars(select(WorkoutSession).where(WorkoutSession.status == "PLAN_UPDATED", WorkoutSession.completed_at.is_not(None))
        .order_by(WorkoutSession.completed_at)))
    ratings = {f.workout_session_id: f.rating.title() for f in db.scalars(select(CrowdFeedback))}
    # Early workouts predate guide_ids; current templates know each exercise's illustration/muscle.
    guides = {spec["id"]: spec.get("guide_id") for row in db.scalars(select(WorkoutTemplate))
        for spec in row.definition_json["exercises"] + row.definition_json.get("alternatives", [])}
    best: dict[str, float] = {}
    result = []
    for workout in workouts:
        exercises, volume, sets, prs, last_log, abandoned = [], 0.0, 0, [], None, False
        for row in db.scalars(select(WorkoutExercise).where(WorkoutExercise.workout_session_id == workout.id).order_by(WorkoutExercise.position)):
            abandoned |= row.deferred_reason == "abandoned"
            done = logs(db, row, "WORKING")
            if done:
                last_log = max(last_log or done[-1].logged_at, done[-1].logged_at)
            if not done:
                continue
            top = max(s.weight for s in done)
            if row.exercise_id in best and top > best[row.exercise_id]:
                prs.append(row.config_json["name"])
            best[row.exercise_id] = max(best.get(row.exercise_id, 0), top)
            volume += sum(s.weight * s.reps for s in done)
            sets += len(done)
            exercises.append({"id": row.exercise_id, "name": row.config_json["name"],
                "muscle": primary_muscle(row.config_json.get("guide_id") or guides.get(row.exercise_id) or row.exercise_id) or closest_muscle(row.config_json["name"]),
                "sets": [{"weight": s.weight, "reps": s.reps} for s in done]})
        started = workout.started_at.astimezone(zone)
        seconds = workout.actual_duration_seconds or 0
        if abandoned and last_log:
            # Closed automatically hours later: count only the time actually training.
            seconds = int((last_log - workout.started_at).total_seconds())
        result.append({"id": workout.id, "date": started.date().isoformat(), "label": f"{started:%a %-d %b}", "time": f"{started:%H:%M}",
            "template": workout.template_snapshot.get("name", "Workout"), "minutes": seconds // 60,
            "sets": sets, "volume": round(volume), "prs": prs, "crowd": ratings.get(workout.id), "exercises": exercises})
    return list(reversed(result))


def insights(db: Session, zone: ZoneInfo, now: datetime, profile: Profile, done: list[dict]) -> dict:
    """Numbers that answer: am I consistent, am I progressing, what am I training, when is it quiet."""
    today = now.astimezone(zone).date()
    monday = today - timedelta(days=today.weekday())
    weeks = []
    for back in range(7, -1, -1):
        start = monday - timedelta(weeks=back)
        weeks.append({"label": f"{start:%-d %b}", "start": start.isoformat(), "count": sum(1 for w in done if start <= date.fromisoformat(w["date"]) < start + timedelta(days=7))})
    def volume_between(start: date, end: date) -> int:
        return sum(w["volume"] for w in done if start <= date.fromisoformat(w["date"]) < end)
    progress: dict[str, dict] = {}
    for workout in reversed(done):
        for exercise in workout["exercises"]:
            top = max(exercise["sets"], key=lambda s: (s["weight"], s["reps"]))
            entry = progress.setdefault(exercise["id"], {"name": exercise["name"], "points": []})
            entry["points"].append({"date": workout["date"], "label": workout["label"], "weight": top["weight"], "reps": top["reps"]})
    muscles: dict[str, dict] = defaultdict(lambda: {"volume": 0.0, "sets": 0})
    for workout in done:
        if date.fromisoformat(workout["date"]) >= today - timedelta(days=28):
            for exercise in workout["exercises"]:
                key = exercise["muscle"] or "Other"
                muscles[key]["volume"] += sum(s["weight"] * s["reps"] for s in exercise["sets"])
                muscles[key]["sets"] += len(exercise["sets"])
    # Gym crowd by weekday × hour: mean of daily means, so busy days don't count twice.
    cells = defaultdict(lambda: defaultdict(list))
    for row in db.scalars(select(CrowdObservation).where(CrowdObservation.source == "GYM_API", CrowdObservation.observed_at >= now - timedelta(days=90))):
        local = row.observed_at.astimezone(zone)
        cells[(local.weekday(), local.hour)][local.date()].append(row.raw_value)
    heatmap = [{"weekday": d, "hour": h, "count": round(mean(mean(v) for v in dates.values()), 1), "days": len(dates)} for (d, h), dates in sorted(cells.items())]
    recent = [w for w in done if date.fromisoformat(w["date"]) >= today - timedelta(days=30)]
    return {"target": profile.weekly_target_sessions, "weeks": weeks,
        "kpis": {"workouts_30d": len(recent), "this_week": weeks[-1]["count"],
            # Rolling windows: a Monday morning shouldn't read as "-100% vs last week".
            "volume_7d": volume_between(today - timedelta(days=6), today + timedelta(days=1)),
            "volume_prev_7d": volume_between(today - timedelta(days=13), today - timedelta(days=6)),
            "prs_30d": sum(len(w["prs"]) for w in recent)},
        "progress": [{"id": key, **value} for key, value in progress.items()],
        "muscles": sorted(({"muscle": key, "volume": round(v["volume"]), "sets": v["sets"]} for key, v in muscles.items()), key=lambda m: -m["volume"]),
        "heatmap": heatmap}


def body_weight(db: Session, zone: ZoneInfo, now: datetime) -> list[dict]:
    """Every weigh-in, oldest first, with the average of the 7 days up to it (the trend line)."""
    rows = list(db.scalars(select(BodyWeight).where(BodyWeight.measured_at <= now).order_by(BodyWeight.measured_at)))
    points = []
    for row in rows:
        week = [r.kg for r in rows if row.measured_at - timedelta(days=7) < r.measured_at <= row.measured_at]
        local = row.measured_at.astimezone(zone)
        points.append({"date": local.date().isoformat(), "label": f"{local:%a %-d %b %Y}", "kg": row.kg, "avg": round(mean(week), 1)})
    return points


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
        "crowd": crowd(db, zone, now, bool(settings and settings.enabled and settings.crowd_polling_enabled)) | {"health": polling_health(db, now=now)},
        "history": (done := history(db, zone, now)), "insights": insights(db, zone, now, profile, done) | {"weight": body_weight(db, zone, now)},
        "activity": [{"time": hhmm(e.created_at, zone), "day": e.created_at.astimezone(zone).strftime("%a"), "text": ACTIVITY[e.type]} for e in events]}


def read_snapshot(url: str | None = None) -> dict:
    """Live snapshot of a persistent SQLite DB (default: GYMCLAW_DB_URL), opened read-only."""
    url = make_url(url or os.environ.get("GYMCLAW_DB_URL", "sqlite:///data/gymclaw.db"))
    if url.get_backend_name() != "sqlite" or not url.database or url.database == ":memory:":
        raise ValueError("Persistent SQLite required")
    # SQLite URI mode=ro prevents creation, migration and accidental writes.
    engine = create_engine(f"sqlite:///file:{quote(url.database, safe='/')}?mode=ro&uri=true")
    try:
        with Session(engine) as db:
            return snapshot(db, now=datetime.now(timezone.utc))
    finally:
        engine.dispose()


def main():
    try:
        print(json.dumps(read_snapshot(), allow_nan=False))
    except Exception:
        print(json.dumps({"ok": False, "error": "LIVE_STATE_UNAVAILABLE"}))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
