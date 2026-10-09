"""Busy equipment keeps unresolved work durable; substitution needs explicit choice.

Swap options come from the template first, then the illustrated catalog (same primary muscle),
so every swap still has an image and hits the same muscle group. When the owner names an exercise
themselves (`switch_to`, `relabel`), any exercise goes: the plan adapts to what they actually do.
"""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import ExerciseProgression, SetLog, WorkoutExercise
from gymclaw.services.errors import DomainError
from gymclaw.services.illustrations import artwork, catalog, for_exercise, primary_muscle, search, similar
from gymclaw.services.templates import ExerciseSpec, Template, get_template, remember_swap
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
        if any(e.exercise_id == substitute_id and e not in swapped_away(db, workout) for e in exercises(db, workout)):
            raise DomainError("INVALID_SUBSTITUTION", "Replacement exercise already used in this workout")
        was_active = original.status == "ACTIVE"
        replacement = make_exercise(db, workout, spec, original.position, warmup=needs_warmup, working_sets=remaining)
        replacement.substituted_from_exercise_id = original.exercise_id
        replacement.rest_seconds = original.rest_seconds
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
    """An exercise swapped away earlier in this workout, a template alternative, or a catalog exercise for
    the same primary muscle (weights from history)."""
    back = next((e for e in swapped_away(db, workout) if e.exercise_id == substitute_id), None)
    if back is not None:
        return spec_of(back)
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


def swapped_away(db: Session, workout) -> list[WorkoutExercise]:
    """Exercises swapped out before any set was logged. They stay swappable: the owner may change their mind."""
    return [e for e in exercises(db, workout) if e.status == "SUBSTITUTED" and not logs(db, e, "WORKING")]


def alternatives(db: Session, workout_id: str, exercise_id: str | None = None, *, limit: int = 3) -> dict:
    """Read-only swap options: exercises swapped away from this slot first ("back to A"), then saved template
    alternatives, then same-muscle catalog exercises. Never the exercise being swapped (or the same movement
    under another name) or one still in the workout."""
    workout = load_workout(db, workout_id)
    exercise = find_exercise(db, workout, exercise_id)
    earlier = swapped_away(db, workout)
    rows = [e for e in exercises(db, workout) if e not in earlier]
    used = ({e.exercise_id for e in rows} | {e.config_json.get("guide_id") for e in rows}) - {None}
    pictures = {artwork(slug) for slug in used if slug in catalog()}
    back = {e.exercise_id: e for e in earlier if e.position == exercise.position and e.exercise_id not in used}
    options = [{"substitute_id": e.exercise_id, "name": e.config_json["name"], "source": "workout",
        "illustration": for_exercise(e.config_json["name"], e.config_json.get("guide_id"))} for e in back.values()]
    used |= {e.config_json.get("guide_id") for e in back.values()} - {None}
    template = Template.model_validate(workout.template_snapshot)
    options += [{"substitute_id": spec.id, "name": spec.name, "source": "template", "illustration": for_exercise(spec.name, spec.guide_id)}
        for spec in template.alternatives if spec.id in exercise.config_json["substitutes"] and spec.id not in used
        and not (spec.guide_id in used or spec.guide_id and artwork(spec.guide_id) in pictures)]
    # Exercises the owner has logged before rank first among catalog options.
    history = set(db.scalars(select(WorkoutExercise.exercise_id).join(SetLog).distinct()))
    guide_id = exercise.config_json.get("guide_id")
    exclude = used | set(back) | {o["illustration"]["guide_id"] for o in options if o["illustration"]}
    for item in similar(guide_id, exclude=exclude, prefer=history, limit=limit) if guide_id else []:
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


def resolve(db: Session, workout, query: str, current: WorkoutExercise | None) -> tuple[WorkoutExercise | None, ExerciseSpec]:
    """Owner's exercise → (planned row still to do, its spec) or (None, spec to add). Exact ID, guide ID or
    name (case-insensitive) from this workout, its template, then the catalog. Anything else fails with
    catalog suggestions, so the agent picks one instead of guessing."""
    key = query.strip().casefold()

    def same(*names: str | None) -> bool:
        return key in {n.casefold() for n in names if n}

    rows = exercises(db, workout)
    for row in rows:
        if row.status in {"PENDING", "DEFERRED"} and same(row.exercise_id, row.config_json["name"], row.config_json.get("guide_id")):
            return row, spec_of(row)
    template = Template.model_validate(workout.template_snapshot)
    known = [*template.exercises, *template.alternatives, *(spec_of(r) for r in rows)]
    spec = next((s for s in known if same(s.id, s.name, s.guide_id)), None)
    if spec is None:
        item = catalog().get(key) or next((i for i in catalog().values() if i["name"].casefold() == key), None)
        if item is None:
            options = ", ".join(f"{o['guide_id']} ({o['name']})" for o in search(query, limit=5))
            raise DomainError("EXERCISE_UNKNOWN", f"No exact match for '{query}'. Pass one guide_id: {options or 'try catalog search'}")
        base = current.config_json if current else {}
        spec = ExerciseSpec(id=item["slug"], name=item["name"], role=base.get("role", "accessory"), guide_id=item["slug"],
            working_sets=current.planned_working_sets if current else 3, rep_min=base.get("rep_min", 8), rep_max=base.get("rep_max", 12),
            target_weight=0, increment=base.get("increment", 2.5))
    return None, spec


def spec_of(row: WorkoutExercise) -> ExerciseSpec:
    return ExerciseSpec.model_validate({k: v for k, v in row.config_json.items() if k in ExerciseSpec.model_fields})


def switch_to(db: Session, workout_id: str, query: str, *, now: datetime, request_id: str, remember: bool = True) -> dict:
    """Owner says what they're doing now. Any exercise, any time: the current one is finished with the
    sets done so far, replaced if not started (and, if `remember`, the template keeps the replacement),
    or put back in the queue if the owner just wants another planned exercise first."""
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        ensure_running(workout)
        current = next((e for e in exercises(db, workout) if e.status == "ACTIVE"), None)
        row, spec = resolve(db, workout, query, current)
        result_extra = {"replaced_exercise_id": None}
        if current is not None and spec.id == current.exercise_id:
            return touch(db, workout, now) | result_extra
        started = current is not None and bool(logs(db, current, "WORKING"))
        if current is not None:
            if started:
                current.status = "COMPLETED"
            elif row is None:
                current.status, current.deferred_reason = "SUBSTITUTED", f"substituted_with:{spec.id}"
                result_extra["replaced_exercise_id"] = current.exercise_id
            else:
                current.status = "PENDING"
        if row is None:
            position = current.position if current else max(e.position for e in exercises(db, workout)) + 1
            row = make_exercise(db, workout, spec, position, working_sets=current.planned_working_sets if current and not started else None)
            if result_extra["replaced_exercise_id"]:
                row.substituted_from_exercise_id = current.exercise_id
        if current is not None:
            row.rest_seconds = current.rest_seconds
        row.status = "ACTIVE"
        row.deferred_reason = None
        cancel_rest(db, workout, now)
        transition(db, workout, "NEXT_EXERCISE", now)
        transition(db, workout, "EXERCISE_ACTIVE", now)
        transition(db, workout, "SET_ACTIVE", now)
        if remember and result_extra["replaced_exercise_id"] and workout.template_id:
            remember_swap(db, workout.template_id, current.exercise_id, spec)
        return touch(db, workout, now) | result_extra

    return mutate(db, "workout.switched", request_id, {"workout_id": workout_id, "to": query}, now, action)


def relabel(db: Session, workout_id: str, query: str, *, which: str | None = None, now: datetime, request_id: str) -> dict:
    """Owner did a different exercise than the one logged ("that was dumbbell RDL, not single-leg").
    Keeps the sets, renames the exercise, moves this workout's progression to it and puts it in the
    template where the logged one was. Works on finished workouts too. `which` picks the logged
    exercise (ID, guide ID or name); default is the latest exercise with sets."""
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        rows = [e for e in exercises(db, workout) if logs(db, e, "WORKING")]
        key = (which or "").strip().casefold()
        if key:
            row = next((e for e in rows if key in {e.id[:8], e.exercise_id.casefold(), e.config_json["name"].casefold(), (e.config_json.get("guide_id") or "").casefold()}), None)
        else:
            row = max(rows, key=lambda e: logs(db, e, "WORKING")[-1].logged_at, default=None)
        if row is None:
            raise DomainError("EXERCISE_NOT_FOUND", "No logged exercise matches; pass which= with its name")
        _, spec = resolve(db, workout, query, row)
        old = row.exercise_id
        if spec.id == old:
            return {"workout_id": workout.id, "exercise_id": row.id, "from": old, "to": old}
        row.exercise_id = spec.id
        row.config_json = row.config_json | {"id": spec.id, "name": spec.name, "guide_id": spec.guide_id}
        progression = db.get(ExerciseProgression, old)
        if progression is not None and progression.source_workout_id == workout.id:
            moved = db.get(ExerciseProgression, spec.id) or ExerciseProgression(exercise_id=spec.id)
            moved.next_weight, moved.source_workout_id, moved.updated_at = progression.next_weight, workout.id, now
            db.add(moved)
            db.delete(progression)
        if workout.template_id and any(e.id == old for e in get_template(db, workout.template_id).exercises):
            remember_swap(db, workout.template_id, old, spec)
        if workout.status != "PLAN_UPDATED":
            workout.last_action_at = now
        db.flush()
        return {"workout_id": workout.id, "exercise_id": row.id, "from": old, "to": spec.id, "name": spec.name}

    return mutate(db, "workout.relabeled", request_id, {"workout_id": workout_id, "to": query, "which": which}, now, action)
