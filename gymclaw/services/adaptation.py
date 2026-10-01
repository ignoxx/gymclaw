"""Busy equipment keeps unresolved work durable; substitution needs explicit choice."""
from datetime import datetime

from sqlalchemy.orm import Session

from gymclaw.services.errors import DomainError
from gymclaw.services.templates import Template
from gymclaw.services.workout import UNRESOLVED, active_exercise, cancel_rest, choose_next, exercises, load_workout, logs, make_exercise, mutate, touch, transition, utc, warmup_pending


def ensure_running(workout):
    if workout.status not in {"SET_ACTIVE", "RESTING", "EXERCISE_ACTIVE"}:
        raise DomainError("INVALID_TRANSITION", f"Cannot adapt workout in {workout.status}")


def find_exercise(db, workout, exercise_id):
    if exercise_id is None:
        return active_exercise(db, workout)
    row = next((e for e in exercises(db, workout) if e.id == exercise_id), None)
    if row is None:
        raise DomainError("EXERCISE_NOT_FOUND", "Exercise does not belong to workout")
    return row


def machine_busy(db: Session, workout_id: str, *, now: datetime, request_id: str, exercise_id: str | None = None) -> dict:
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        ensure_running(workout)
        exercise = find_exercise(db, workout, exercise_id)
        if exercise.status not in {"ACTIVE", "DEFERRED"}:
            raise DomainError("INVALID_EXERCISE_STATE", "Only active or deferred equipment can be marked busy")
        was_active = exercise.status == "ACTIVE"
        exercise.status = "DEFERRED"
        exercise.deferred_reason = "equipment_busy"
        workout.waited_for_equipment_count += 1
        if was_active:
            cancel_rest(db, workout, now)
            transition(db, workout, "NEXT_EXERCISE", now)
            choose_next(db, workout, now)
        result = touch(db, workout, now)
        result["deferred_exercise_id"] = exercise.id
        result["substitution_options"] = exercise.config_json["substitutes"]
        if result["active_exercise"]:
            result["instruction"] = f"{exercise.config_json['name']} deferred. " + result["instruction"] + ". Retry deferred equipment later."
        else:
            result["instruction"] = f"{exercise.config_json['name']} still unresolved. Retry equipment, choose substitution, or explicitly skip."
        return result

    return mutate(db, "workout.machine_busy", request_id, {"workout_id": workout_id, "exercise_id": exercise_id}, now, action)


def machine_free(db: Session, workout_id: str, exercise_id: str, *, now: datetime, request_id: str) -> dict:
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        ensure_running(workout)
        exercise = find_exercise(db, workout, exercise_id)
        if exercise.status != "DEFERRED":
            raise DomainError("INVALID_EXERCISE_STATE", "Equipment is not deferred")
        exercise.status = "PENDING"
        exercise.deferred_reason = None
        if not any(e.status == "ACTIVE" for e in exercises(db, workout)):
            cancel_rest(db, workout, now)
            transition(db, workout, "NEXT_EXERCISE", now)
            choose_next(db, workout, now)
        return touch(db, workout, now)

    return mutate(db, "workout.machine_free", request_id, {"workout_id": workout_id, "exercise_id": exercise_id}, now, action)


def substitute(db: Session, workout_id: str, exercise_id: str, substitute_id: str, *, now: datetime, request_id: str) -> dict:
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        ensure_running(workout)
        original = find_exercise(db, workout, exercise_id)
        if original.status not in {"ACTIVE", "DEFERRED"}:
            raise DomainError("INVALID_EXERCISE_STATE", "Only active/deferred exercise can be substituted")
        if substitute_id not in original.config_json["substitutes"]:
            raise DomainError("INVALID_SUBSTITUTION", "Substitution must be a configured same-role alternative")
        template = Template.model_validate(workout.template_snapshot)
        spec = next(e for e in template.alternatives if e.id == substitute_id)
        remaining = original.planned_working_sets - len(logs(db, original, "WORKING"))
        needs_warmup = warmup_pending(db, original)
        if needs_warmup and spec.warmup_weight is None:
            raise DomainError("WARMUP_REQUIRED", "Primary replacement needs explicit warm-up weight")
        if any(e.exercise_id == substitute_id for e in exercises(db, workout)):
            raise DomainError("INVALID_SUBSTITUTION", "Replacement exercise already used in this workout")
        was_active = original.status == "ACTIVE"
        replacement = make_exercise(db, workout, spec, original.position, warmup=needs_warmup, working_sets=remaining)
        replacement.substituted_from_exercise_id = original.exercise_id
        # Neither original nor replacement prerequisites may be bypassed.
        replacement.config_json = replacement.config_json | {"requires_completed": sorted(set(original.config_json["requires_completed"]) | set(spec.requires_completed))}
        original.status = "SUBSTITUTED"
        original.deferred_reason = f"substituted_with:{substitute_id}"
        if was_active or not any(e.status == "ACTIVE" for e in exercises(db, workout)):
            cancel_rest(db, workout, now)
            transition(db, workout, "NEXT_EXERCISE", now)
            choose_next(db, workout, now)
        result = touch(db, workout, now)
        result["replacement_exercise_id"] = replacement.id
        result["instruction"] = f"Switching {original.config_json['name']} → {spec.name}; same {spec.role} role, {remaining} working sets remaining. " + result["instruction"]
        return result

    return mutate(db, "workout.substituted", request_id, {"workout_id": workout_id, "exercise_id": exercise_id, "substitute_id": substitute_id}, now, action)


def skip_exercise(db: Session, workout_id: str, exercise_id: str, *, reason: str, now: datetime, request_id: str) -> dict:
    now = utc(now)
    if not reason.strip():
        raise DomainError("REASON_REQUIRED", "Explicit skip requires reason")

    def action():
        workout = load_workout(db, workout_id, now)
        ensure_running(workout)
        exercise = find_exercise(db, workout, exercise_id)
        if exercise.status not in UNRESOLVED:
            raise DomainError("INVALID_EXERCISE_STATE", "Exercise is already resolved")
        was_active = exercise.status == "ACTIVE"
        exercise.status = "SKIPPED"
        exercise.deferred_reason = reason
        if was_active or not any(e.status == "ACTIVE" for e in exercises(db, workout)):
            cancel_rest(db, workout, now)
            transition(db, workout, "NEXT_EXERCISE", now)
            choose_next(db, workout, now)
        return touch(db, workout, now)

    return mutate(db, "workout.exercise_skipped", request_id, {"workout_id": workout_id, "exercise_id": exercise_id, "reason": reason}, now, action)
