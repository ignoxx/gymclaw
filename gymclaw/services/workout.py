"""Persisted workout state machine. Call within a caller-owned transaction.

Every mutation requires a request ID. Retry returns the original JSON result;
reuse with different intent fails. Rest jobs are local durable outbox entries,
not claims that an OpenClaw automation or Telegram message was sent.
"""
from collections.abc import Callable
from datetime import datetime, timedelta, timezone
from statistics import mean
from math import ceil

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, ExerciseProgression, NotificationJob, PlannedSession, SetLog, WorkoutExercise, WorkoutSession
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.set_parser import SetInput
from gymclaw.services.templates import ExerciseSpec, Template, get_template

TRANSITIONS = {
    "SCHEDULED": {"PREPARING", "ARRIVED"},
    "PREPARING": {"COMMUTING", "ARRIVED"},
    "COMMUTING": {"ARRIVED"},
    "ARRIVED": {"SESSION_STARTED"},
    "SESSION_STARTED": {"EXERCISE_ACTIVE"},
    "EXERCISE_ACTIVE": {"SET_ACTIVE", "NEXT_EXERCISE", "WORKOUT_COMPLETE"},
    "SET_ACTIVE": {"RESTING", "EXERCISE_COMPLETE", "NEXT_EXERCISE", "EXERCISE_ACTIVE", "WORKOUT_COMPLETE"},
    "RESTING": {"SET_ACTIVE", "NEXT_EXERCISE", "EXERCISE_ACTIVE", "WORKOUT_COMPLETE"},
    "EXERCISE_COMPLETE": {"NEXT_EXERCISE", "RESTING", "WORKOUT_COMPLETE"},
    "NEXT_EXERCISE": {"EXERCISE_ACTIVE", "WORKOUT_COMPLETE"},
    "WORKOUT_COMPLETE": {"POST_ANALYSIS"},
    "POST_ANALYSIS": {"PLAN_UPDATED"},
    "PLAN_UPDATED": set(),
}
UNRESOLVED = {"PENDING", "ACTIVE", "DEFERRED"}


def utc(value: datetime) -> datetime:
    if value.tzinfo is None or value.utcoffset() is None:
        raise DomainError("INVALID_TIME", "Timestamp must include timezone")
    return value.astimezone(timezone.utc)


def emit(db: Session, kind: str, now: datetime, payload: dict) -> AgentEvent:
    event = AgentEvent(type=kind, created_at=now, payload_json=payload)
    db.add(event)
    db.flush()
    return event


def transition(db: Session, workout: WorkoutSession, state: str, now: datetime):
    if state not in TRANSITIONS.get(workout.status, set()):
        raise DomainError("INVALID_TRANSITION", f"Cannot transition {workout.status} to {state}")
    old = workout.status
    workout.status = state
    emit(db, "workout.state_changed", now, {"workout_id": workout.id, "from": old, "to": state})


def mutate(db: Session, operation: str, request_id: str, intent: dict, now: datetime, action: Callable[[], dict]) -> dict:
    now = utc(now)
    if not request_id or len(request_id) > 200:
        raise DomainError("REQUEST_ID_REQUIRED", "Mutation requires request ID (1–200 characters)")
    request = {"operation": operation, "intent": intent}
    previous = db.scalar(select(AgentEvent).where(AgentEvent.correlation_id == request_id))
    if previous:
        if previous.payload_json.get("request") != request:
            raise DomainError("IDEMPOTENCY_CONFLICT", "Request ID already used with different intent")
        return previous.payload_json["result"]
    data = action()
    event = emit(db, operation, now, {"request": request})
    result = {"data": data, "events": [{"id": event.id, "type": operation}], "user_message_hint": data.get("instruction")}
    event.correlation_id = request_id
    event.payload_json = {"request": request, "result": result}
    db.flush()
    return result


def load_workout(db: Session, workout_id: str, now: datetime | None = None) -> WorkoutSession:
    workout = db.get(WorkoutSession, workout_id)
    if workout is None:
        raise DomainError("WORKOUT_NOT_FOUND", f"Unknown workout: {workout_id}")
    if not workout.template_snapshot:
        raise DomainError("LEGACY_WORKOUT", "Workout has no execution snapshot; historical records are preserved read-only")
    if now is not None and workout.last_action_at and utc(now) < workout.last_action_at:
        raise DomainError("TIME_REVERSED", "Action timestamp precedes previous workout action")
    return workout


def exercises(db: Session, workout: WorkoutSession) -> list[WorkoutExercise]:
    return list(db.scalars(select(WorkoutExercise).where(WorkoutExercise.workout_session_id == workout.id).order_by(WorkoutExercise.position, WorkoutExercise.id)))


def logs(db: Session, exercise: WorkoutExercise, kind: str | None = None) -> list[SetLog]:
    query = select(SetLog).where(SetLog.workout_exercise_id == exercise.id)
    if kind:
        query = query.where(SetLog.set_type == kind)
    return list(db.scalars(query.order_by(SetLog.logged_at, SetLog.set_number)))


def pending_rest(db: Session, workout: WorkoutSession) -> NotificationJob | None:
    return db.scalar(select(NotificationJob).where(NotificationJob.workout_session_id == workout.id, NotificationJob.kind == "REST", NotificationJob.status == "PENDING"))


def cancel_rest(db: Session, workout: WorkoutSession, now: datetime):
    for job in db.scalars(select(NotificationJob).where(NotificationJob.workout_session_id == workout.id, NotificationJob.status == "PENDING")):
        job.status = "CANCELLED"
        job.handled_at = now
        emit(db, "notifications.cancel_required", now, {"job_id": job.id, "external_job_id": job.external_job_id})


def active_exercise(db: Session, workout: WorkoutSession) -> WorkoutExercise:
    exercise = next((e for e in exercises(db, workout) if e.status == "ACTIVE"), None)
    if exercise is None:
        raise DomainError("NO_ACTIVE_EXERCISE", "No active exercise; resolve deferred equipment first")
    return exercise


def warmup_pending(db: Session, exercise: WorkoutExercise) -> bool:
    return exercise.config_json.get("warmup_required", False) and not logs(db, exercise, "WARMUP")


def compatible(exercise: WorkoutExercise, rows: list[WorkoutExercise]) -> bool:
    completed = {e.exercise_id for e in rows if e.status == "COMPLETED"}
    completed |= {e.substituted_from_exercise_id for e in rows if e.status == "COMPLETED" and e.substituted_from_exercise_id}
    return set(exercise.config_json.get("requires_completed", [])) <= completed


def choose_next(db: Session, workout: WorkoutSession, now: datetime):
    rows = exercises(db, workout)
    next_row = next((e for e in rows if e.status == "PENDING" and compatible(e, rows)), None)
    if next_row:
        next_row.status = "ACTIVE"
        transition(db, workout, "EXERCISE_ACTIVE", now)
        transition(db, workout, "SET_ACTIVE", now)
    elif any(e.status in UNRESOLVED for e in rows):
        # No unresolved exercise is silently discarded. Agent must retry/substitute/skip.
        transition(db, workout, "EXERCISE_ACTIVE", now)
    else:
        transition(db, workout, "WORKOUT_COMPLETE", now)


def historical_set_duration(db: Session, default_seconds: float) -> float:
    historical = list(db.scalars(select(SetLog).join(WorkoutExercise).join(WorkoutSession).where(WorkoutSession.status == "PLAN_UPDATED").order_by(SetLog.workout_exercise_id, SetLog.logged_at)))
    durations = []
    for previous, current in zip(historical, historical[1:]):
        if previous.workout_exercise_id == current.workout_exercise_id:
            seconds = (current.logged_at - (previous.rest_due_at or previous.logged_at)).total_seconds()
            if 10 <= seconds <= 180:
                durations.append(seconds)
    return mean(durations) if durations else default_seconds


def estimate_finish(db: Session, workout: WorkoutSession, now: datetime) -> datetime:
    rows = [e for e in exercises(db, workout) if e.status in UNRESOLVED]
    if not rows:
        return workout.completed_at or workout.last_action_at or now
    duration = historical_set_duration(db, workout.template_snapshot["set_duration_seconds"])
    rest_job = pending_rest(db, workout)
    seconds = max(0, (rest_job.due_at - now).total_seconds()) if rest_job else 0
    for index, exercise in enumerate(rows):
        remaining = max(0, exercise.planned_working_sets - len(logs(db, exercise, "WORKING")))
        seconds += remaining * duration
        if warmup_pending(db, exercise):
            seconds += duration
        seconds += max(0, remaining - 1) * exercise.rest_seconds
        if index < len(rows) - 1:
            seconds += exercise.rest_seconds + workout.template_snapshot["transition_seconds"]
    return now + timedelta(seconds=seconds)


def current(db: Session, workout_id: str, *, now: datetime) -> dict:
    now = utc(now)
    workout = load_workout(db, workout_id)
    rows = exercises(db, workout)
    active = next((e for e in rows if e.status == "ACTIVE"), None)
    rest = pending_rest(db, workout)
    instruction = "Workout complete. Finish to save audit."
    active_data = None
    if active:
        is_warmup = warmup_pending(db, active)
        active_data = {
            "id": active.id, "exercise_id": active.exercise_id, "name": active.config_json["name"],
            "set_type": "WARMUP" if is_warmup else "WORKING",
            "set_number": 1 if is_warmup else len(logs(db, active, "WORKING")) + 1,
            "working_sets": active.planned_working_sets,
            "target_weight": active.config_json["warmup_weight"] if is_warmup else active.target_weight,
            "rep_min": active.config_json["warmup_reps"] if is_warmup else active.rep_min,
            "rep_max": active.config_json["warmup_reps"] if is_warmup else active.rep_max,
        }
        instruction = f"{active_data['name']} · {active_data['set_type'].lower()} set {active_data['set_number']} · {active_data['target_weight']:g} kg · {active_data['rep_min']}–{active_data['rep_max']} reps"
    elif any(e.status in UNRESOLVED for e in rows):
        instruction = "Deferred equipment unresolved. Retry, substitute, or explicitly skip."
    if rest:
        instruction = "Rest."
    if workout.status == "PLAN_UPDATED":
        instruction = "Workout audit saved."
    eta = estimate_finish(db, workout, now)
    return {
        "workout_id": workout.id, "status": workout.status, "active_exercise": active_data,
        "rest_job": {"id": rest.id, "due_at": rest.due_at.isoformat(), "external_job_id": rest.external_job_id} if rest else None,
        "eta": eta.isoformat(), "initial_eta": workout.initial_eta.isoformat() if workout.initial_eta else None,
        "seconds_vs_initial_eta": (eta - workout.initial_eta).total_seconds() if workout.initial_eta else None,
        "queue": [{"id": e.id, "exercise_id": e.exercise_id, "status": e.status, "remaining_sets": max(0, e.planned_working_sets - len(logs(db, e, "WORKING"))), "reason": e.deferred_reason} for e in rows],
        "instruction": instruction,
    }


def touch(db: Session, workout: WorkoutSession, now: datetime) -> dict:
    workout.last_action_at = now
    db.flush()
    workout.final_eta = estimate_finish(db, workout, now)
    return current(db, workout.id, now=now)


def make_exercise(db: Session, workout: WorkoutSession, spec: ExerciseSpec, position: int, *, warmup: bool = False, working_sets: int | None = None) -> WorkoutExercise:
    profile = get_profile(db)
    progression = db.get(ExerciseProgression, spec.id)
    row = WorkoutExercise(workout_session_id=workout.id, exercise_id=spec.id, position=position, planned_working_sets=working_sets or spec.working_sets, rep_min=spec.rep_min, rep_max=spec.rep_max, target_weight=progression.next_weight if progression else spec.target_weight, rest_seconds=spec.rest_seconds or (profile.default_compound_rest_seconds if spec.primary else profile.default_accessory_rest_seconds), config_json=spec.model_dump(mode="json") | {"warmup_required": warmup})
    db.add(row)
    db.flush()
    return row


def start(db: Session, template_id: str, *, now: datetime, request_id: str, planned_session_id: str | None = None) -> dict:
    now = utc(now)

    def action():
        if db.scalar(select(WorkoutSession).where(WorkoutSession.status != "PLAN_UPDATED")):
            raise DomainError("WORKOUT_ALREADY_ACTIVE", "Finish existing workout before starting another")
        template = get_template(db, template_id)
        planned = db.get(PlannedSession, planned_session_id) if planned_session_id else None
        if planned_session_id and (planned is None or planned.status not in {"TENTATIVE", "COMMITTED"}):
            raise DomainError("INVALID_PLAN", "Planned session is missing or no longer startable")
        allocation = None
        if planned:
            from gymclaw.services.compression import compress_template
            if planned.workout_plan_json.get("template", {}).get("id") == template_id:
                allocation = planned.workout_plan_json
                if allocation.get("error"):
                    raise DomainError("WORKOUT_SLOT_TOO_SHORT", "Calendar slot needs explicit adjustment before starting")
                template = Template.model_validate(allocation["template"])
            template = template.model_copy(update={"set_duration_seconds": ceil(historical_set_duration(db, template.set_duration_seconds))})
            allocation = compress_template(template, get_profile(db), int((planned.planned_end_at - planned.planned_start_at).total_seconds()))
            if planned.workout_plan_json.get("crowd_source"):
                allocation["crowd_source"] = planned.workout_plan_json["crowd_source"]
            planned.workout_plan_json = allocation
        workout = WorkoutSession(template_id=template_id, template_snapshot=template.model_dump(mode="json"), planned_session_id=planned_session_id, started_at=now, arrived_at=now, last_action_at=now)
        db.add(workout)
        db.flush()
        first_primary = next((e.id for e in template.exercises if e.primary), None)
        for index, spec in enumerate(template.exercises):
            row = make_exercise(db, workout, spec, index, warmup=spec.id == first_primary)
            if allocation:
                row.planned_working_sets = allocation["sets"][spec.id]
                if row.planned_working_sets == 0:
                    row.status = "SKIPPED"
                    row.deferred_reason = "calendar_time_budget"
        # start is explicit arrival intent, not fabricated preparation/travel history.
        transition(db, workout, "ARRIVED", now)
        transition(db, workout, "SESSION_STARTED", now)
        choose_next(db, workout, now)
        if planned:
            from gymclaw.services.notifications import cancel_session_jobs
            cancel_session_jobs(db, planned.id, now=now)
            planned.status = "STARTED"
            planned.workout_template_id = template_id
        workout.initial_eta = estimate_finish(db, workout, now)
        return touch(db, workout, now)

    return mutate(db, "workout.started", request_id, {"template_id": template_id, "planned_session_id": planned_session_id}, now, action)


def log_set(db: Session, workout_id: str, value: SetInput, *, now: datetime, request_id: str) -> dict:
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        if workout.status not in {"SET_ACTIVE", "RESTING"}:
            raise DomainError("INVALID_TRANSITION", f"Cannot log set in {workout.status}")
        active = active_exercise(db, workout)
        needs_warmup = warmup_pending(db, active)
        if value.set_type == "WORKING" and needs_warmup:
            raise DomainError("WARMUP_REQUIRED", "Log first primary warm-up explicitly before working sets")
        if value.set_type == "WARMUP" and not needs_warmup:
            raise DomainError("UNEXPECTED_WARMUP", "No warm-up is pending for this exercise")
        cancel_rest(db, workout, now)
        if workout.status == "RESTING":
            transition(db, workout, "SET_ACTIVE", now)
        row = SetLog(workout_exercise_id=active.id, set_number=len(logs(db, active, value.set_type)) + 1, set_type=value.set_type, weight=value.weight, reps=value.reps, rir_optional=value.rir, logged_at=now)
        db.add(row)
        db.flush()
        if value.set_type == "WORKING":
            completed = len(logs(db, active, "WORKING")) >= active.planned_working_sets
            if completed:
                active.status = "COMPLETED"
                transition(db, workout, "EXERCISE_COMPLETE", now)
                transition(db, workout, "NEXT_EXERCISE", now)
                choose_next(db, workout, now)
            if workout.status != "WORKOUT_COMPLETE":
                row.rest_started_at = now
                row.rest_due_at = now + timedelta(seconds=active.rest_seconds)
                job = NotificationJob(kind="REST", workout_session_id=workout.id, set_log_id=row.id, due_at=row.rest_due_at, payload_json={"workout_id": workout.id})
                db.add(job)
                db.flush()
                emit(db, "notifications.schedule_required", now, {"job_id": job.id, "due_at": row.rest_due_at.isoformat(), "kind": "REST"})
                # Deferred-only queue stays EXERCISE_ACTIVE; rest is still durable.
                if workout.status == "SET_ACTIVE":
                    transition(db, workout, "RESTING", now)
        result = touch(db, workout, now)
        result["logged_set"] = {"id": row.id, "weight": row.weight, "reps": row.reps, "set_type": row.set_type, "set_number": row.set_number}
        return result

    return mutate(db, "workout.set_logged", request_id, {"workout_id": workout_id, "set": value.model_dump(mode="json")}, now, action)


def rest_complete(db: Session, job_id: str, *, now: datetime, request_id: str) -> dict:
    now = utc(now)

    def action():
        job = db.get(NotificationJob, job_id)
        if job is None or job.kind != "REST":
            raise DomainError("JOB_NOT_FOUND", "Unknown rest job")
        workout = load_workout(db, job.workout_session_id, now)
        if job.status != "PENDING":
            return {"job_id": job.id, "stale": True, "status": job.status}
        if now < job.due_at:
            raise DomainError("REST_NOT_DUE", "Rest timer has not reached due_at")
        job.status = "FIRED"
        job.handled_at = now
        if workout.status == "RESTING":
            transition(db, workout, "SET_ACTIVE", now)
        result = touch(db, workout, now)
        result["instruction"] = "Rest complete. " + result["instruction"]
        return result

    return mutate(db, "workout.rest_completed", request_id, {"job_id": job_id}, now, action)
