"""Busy equipment keeps unresolved work durable; substitution needs explicit choice.

Alternatives come from the template first, then the illustrated catalog (same primary muscle),
so every swap still has an image and hits the same muscle group.
"""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import ExerciseProgression, SetLog, WorkoutExercise
from gymclaw.services.errors import DomainError
from gymclaw.services.illustrations import catalog, for_exercise, primary_muscle, similar
from gymclaw.services.templates import ExerciseSpec, Template
from gymclaw.services.workout import UNRESOLVED, active_exercise, cancel_rest, choose_next, compatible, exercises, load_workout, logs, make_exercise, mutate, touch, transition, utc, warmup_pending


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
            # Stay on the same muscle group while the machine is taken.
            choose_next(db, workout, now, prefer_muscle=primary_muscle(exercise.config_json.get("guide_id")))
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
        active = next((e for e in exercises(db, workout) if e.status == "ACTIVE"), None)
        if active and not logs(db, active):
            # Freed machine wins over an exercise that hasn't started yet.
            active.status = "PENDING"
            active = None
        if active is None:
            exercise.status = "ACTIVE"
            cancel_rest(db, workout, now)
            transition(db, workout, "NEXT_EXERCISE", now)
            transition(db, workout, "EXERCISE_ACTIVE", now)
            transition(db, workout, "SET_ACTIVE", now)
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
        spec = replacement_spec(db, workout, original, substitute_id)
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
        if was_active and compatible(replacement, exercises(db, workout)):
            replacement.status = "ACTIVE"
            cancel_rest(db, workout, now)
            transition(db, workout, "NEXT_EXERCISE", now)
            transition(db, workout, "EXERCISE_ACTIVE", now)
            transition(db, workout, "SET_ACTIVE", now)
        elif was_active or not any(e.status == "ACTIVE" for e in exercises(db, workout)):
            cancel_rest(db, workout, now)
            transition(db, workout, "NEXT_EXERCISE", now)
            choose_next(db, workout, now)
        result = touch(db, workout, now)
        result["replacement_exercise_id"] = replacement.id
        result["instruction"] = f"Swapped to {spec.name}. " + result["instruction"]
        return result

    return mutate(db, "workout.substituted", request_id, {"workout_id": workout_id, "exercise_id": exercise_id, "substitute_id": substitute_id}, now, action)


def replacement_spec(db: Session, workout, original: WorkoutExercise, substitute_id: str) -> ExerciseSpec:
    """Template alternative, or a catalog exercise for the same primary muscle (weights from history)."""
    if substitute_id in original.config_json["substitutes"]:
        return next(e for e in Template.model_validate(workout.template_snapshot).alternatives if e.id == substitute_id)
    item = catalog().get(substitute_id)
    muscle = primary_muscle(original.config_json.get("guide_id"))
    if item is None or muscle is None or item["primaryMuscle"] != muscle:
        raise DomainError("INVALID_SUBSTITUTION", "Substitute must be a template alternative or a catalog exercise for the same muscle")
    progression = db.get(ExerciseProgression, substitute_id)
    config = original.config_json
    return ExerciseSpec(id=substitute_id, name=item["name"], role=config["role"], guide_id=substitute_id, primary=config["primary"],
        working_sets=original.planned_working_sets, rep_min=original.rep_min, rep_max=original.rep_max,
        target_weight=progression.next_weight if progression else 0, increment=config["increment"], rest_seconds=original.rest_seconds,
        warmup_weight=config.get("warmup_weight"), warmup_reps=config["warmup_reps"])


def alternatives(db: Session, workout_id: str, exercise_id: str | None = None, *, limit: int = 2) -> dict:
    """Read-only swap options: saved template alternatives first, then same-muscle catalog exercises."""
    workout = load_workout(db, workout_id)
    exercise = find_exercise(db, workout, exercise_id)
    used = {e.exercise_id for e in exercises(db, workout)} | {e.config_json.get("guide_id") for e in exercises(db, workout)}
    template = Template.model_validate(workout.template_snapshot)
    options = [{"substitute_id": spec.id, "name": spec.name, "source": "template", "illustration": for_exercise(spec.name, spec.guide_id)}
        for spec in template.alternatives if spec.id in exercise.config_json["substitutes"] and spec.id not in used]
    # Exercises the owner has logged before rank first among catalog options.
    history = set(db.scalars(select(WorkoutExercise.exercise_id).join(SetLog).distinct()))
    guide_id = exercise.config_json.get("guide_id")
    for item in similar(guide_id, exclude=used - {None}, prefer=history, limit=limit) if guide_id else []:
        options.append({"substitute_id": item["guide_id"], "name": item["name"], "source": "catalog", "illustration": item})
    return {"exercise_id": exercise.id, "name": exercise.config_json["name"], "options": options[:limit]}


def next_exercise(db: Session, workout_id: str, exercise_id: str, *, now: datetime, request_id: str) -> dict:
    """Owner moves on: completes the active exercise with the sets done so far, or skips it if none."""
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        ensure_running(workout)
        exercise = find_exercise(db, workout, exercise_id)
        if exercise.status != "ACTIVE":
            raise DomainError("INVALID_EXERCISE_STATE", "Only the active exercise can be ended early")
        done = len(logs(db, exercise, "WORKING"))
        exercise.status = "COMPLETED" if done else "SKIPPED"
        exercise.deferred_reason = None if done else "owner_skipped"
        cancel_rest(db, workout, now)
        transition(db, workout, "NEXT_EXERCISE", now)
        choose_next(db, workout, now)
        return touch(db, workout, now)

    return mutate(db, "workout.exercise_ended", request_id, {"workout_id": workout_id, "exercise_id": exercise_id}, now, action)


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
