"""Private application projection for localhost dashboard. No credentials or writes."""
from datetime import datetime, timedelta, timezone
import json
import os
from urllib.parse import quote
from zoneinfo import ZoneInfo

from sqlalchemy import create_engine, select
from sqlalchemy.engine import make_url
from sqlalchemy.orm import Session

from gymclaw.models import (AgentEvent, CalendarSyncState, CrowdObservation, NotificationDelivery,
    RuntimeSettings, SetLog, UserProfile, WorkoutExercise, WorkoutSession, PlannedSession)
from gymclaw.services.illustrations import for_exercise
from gymclaw.services.crowd import CrowdModel
from gymclaw.services.profile import Profile
from gymclaw.services.templates import get_template
from gymclaw.services.workout import current


def snapshot(db: Session, *, now: datetime) -> dict:
    row = db.get(UserProfile, 1)
    profile = Profile.model_validate(row) if row else Profile()
    zone = ZoneInfo(profile.timezone)
    today = now.astimezone(zone).date()
    monday = today - timedelta(days=today.weekday())
    settings = db.execute(select(RuntimeSettings.enabled, RuntimeSettings.calendar_writes_enabled,
        RuntimeSettings.crowd_polling_enabled).where(RuntimeSettings.id == 1)).first()
    calendar = db.execute(select(CalendarSyncState.last_synced_at).where(CalendarSyncState.id == 1)).first()
    start = datetime.combine(monday, datetime.min.time(), zone)
    plans = list(db.scalars(select(PlannedSession).where(PlannedSession.planned_start_at >= start,
        PlannedSession.planned_start_at < start+timedelta(days=7), PlannedSession.status != "CANCELLED").order_by(PlannedSession.planned_start_at)))
    sessions = []
    for plan in plans:
        template = get_template(db, plan.workout_template_id) if plan.workout_template_id else None
        sessions.append({"day": plan.planned_start_at.astimezone(zone).strftime("%a"),
            "date": plan.planned_start_at.astimezone(zone).strftime("%d %b"),
            "start": plan.planned_start_at.astimezone(zone).strftime("%H:%M"),
            "end": plan.planned_end_at.astimezone(zone).strftime("%H:%M"), "status": plan.status,
            "template": template.name if template else "Workout", "prep": plan.prep_start_at.astimezone(zone).strftime("%H:%M"),
            "leave": plan.leave_home_at.astimezone(zone).strftime("%H:%M")})
    observations = list(db.scalars(select(CrowdObservation).where(CrowdObservation.source == "GYM_API")
        .order_by(CrowdObservation.observed_at.desc()).limit(24)))
    observations = [item for item in observations if not item.metadata_json.get("demo", False)][:6]
    observations.reverse()
    prediction = CrowdModel(db, now=now).predict(now) if row else {"score": None, "confidence": 0, "label_evidence": 0}
    readings = [{"time": item.observed_at.astimezone(zone).strftime("%H:%M"), "count": item.raw_value,
        "retrieved_at": item.observed_at.isoformat()} for item in observations]
    active = db.scalar(select(WorkoutSession).where(WorkoutSession.status.in_(["SESSION_STARTED", "EXERCISE_ACTIVE", "SET_ACTIVE", "RESTING", "EXERCISE_COMPLETE", "NEXT_EXERCISE", "WORKOUT_COMPLETE"]), WorkoutSession.started_at.is_not(None))
        .order_by(WorkoutSession.started_at.desc()).limit(1))
    workout = None
    if active:
        data = current(db, active.id, now=now)
        exercises = list(db.scalars(select(WorkoutExercise).where(WorkoutExercise.workout_session_id == active.id).order_by(WorkoutExercise.position)))
        items = []
        for exercise in exercises:
            definition = exercise.config_json
            pose = for_exercise(definition["name"], definition.get("guide_id"))
            items.append({"id": exercise.exercise_id, "name": definition["name"],
                "working_sets": exercise.planned_working_sets, "target_weight": exercise.target_weight,
                "rep_min": exercise.rep_min, "rep_max": exercise.rep_max, "svg_url": pose["svg_url"] if pose else None})
        latest = db.scalar(select(SetLog).join(WorkoutExercise).where(WorkoutExercise.workout_session_id == active.id).order_by(SetLog.logged_at.desc()).limit(1))
        rest = data["rest_job"]
        remaining = max(0, int((datetime.fromisoformat(rest["due_at"])-now).total_seconds())) if rest else 0
        active_data = data["active_exercise"]
        if active_data:
            active_data = {key: value for key, value in active_data.items() if key != "illustration"}
        workout = {"name": active.template_snapshot.get("name", "Workout"), "status": data["status"],
            "active": active_data, "exercises": items, "eta": datetime.fromisoformat(data["eta"]).astimezone(zone).strftime("%H:%M"),
            "last_set": {"weight": latest.weight, "reps": latest.reps} if latest else None,
            "rest_seconds": remaining, "rest_intent_prepared": bool(rest), "timer_activated": bool(rest and rest["external_job_id"])}
    labels = {"planning.week_planned": "Week planned", "workout.started": "Workout started",
        "workout.set_logged": "Set saved", "notifications.schedule_required": "Reminder queued",
        "workout.rest_completed": "Rest complete", "crowd.observed": "Attendance saved"}
    events = list(db.execute(select(AgentEvent.type, AgentEvent.created_at).where(AgentEvent.type.in_(labels)).order_by(AgentEvent.created_at.desc()).limit(3)))
    sent = db.scalar(select(NotificationDelivery.handled_at).where(NotificationDelivery.status == "SENT").order_by(NotificationDelivery.handled_at.desc()).limit(1))
    return {"demo": False, "live_database_accessed": True, "captured_at": now.isoformat(),
        "calendar_writes_enabled": bool(settings and settings.calendar_writes_enabled),
        "week": today.strftime("%B %Y"), "timezone": profile.timezone,
        "week_days": [{"name": (monday+timedelta(days=i)).strftime("%a"), "date": (monday+timedelta(days=i)).day} for i in range(7)],
        "sessions": sessions, "target_sessions": profile.weekly_target_sessions, "workout": workout,
        "crowd": {"readings": readings, "poll_every_minutes": 15,
            "polling_live": bool(settings and settings.enabled and settings.crowd_polling_enabled),
            "backend_freshness": "known" if observations and observations[-1].freshness_seconds is not None else "unknown",
            "arrival_labels": prediction["label_evidence"], "personal_score": prediction["score"],
            "confidence": prediction["confidence"]},
        "activity": [{"time": event.created_at.astimezone(zone).strftime("%H:%M"), "text": labels[event.type]} for event in reversed(events)],
        "connections": {"calendar": {"connected": bool(calendar), "last_synced_at": calendar.last_synced_at.isoformat() if calendar and calendar.last_synced_at else None},
            "telegram": {"enabled": bool(settings and settings.enabled), "last_sent_at": sent.isoformat() if sent else None}}}


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
