"""Workout completion writes audit and progression atomically, then requests replanning."""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, ExerciseProgression, PlannedSession
from gymclaw.services.errors import DomainError
from gymclaw.services.progression import PerformanceSet, double_progression
from gymclaw.services.workout import UNRESOLVED, cancel_rest, emit, exercises, load_workout, logs, mutate, transition, utc


def get_audit(db: Session, workout_id: str) -> dict:
    load_workout(db, workout_id)
    for event in db.scalars(select(AgentEvent).where(AgentEvent.type == "workout.completed")):
        result = event.payload_json.get("result", {}).get("data", {})
        if result.get("workout_id") == workout_id:
            return result
    raise DomainError("AUDIT_NOT_READY", "Finish workout before requesting audit")


def finish(db: Session, workout_id: str, *, now: datetime, request_id: str) -> dict:
    now = utc(now)

    def action():
        workout = load_workout(db, workout_id, now)
        if workout.status == "PLAN_UPDATED":
            raise DomainError("WORKOUT_ALREADY_FINISHED", "Workout already finished; read audit or retry original request ID")
        rows = exercises(db, workout)
        if any(e.status in UNRESOLVED for e in rows):
            raise DomainError("UNRESOLVED_EXERCISES", "Resolve deferred/pending exercises or explicitly skip before finishing")
        if workout.status != "WORKOUT_COMPLETE":
            raise DomainError("INVALID_TRANSITION", f"Cannot finish workout in {workout.status}")
        cancel_rest(db, workout, now)
        transition(db, workout, "POST_ANALYSIS", now)
        workout.completed_at = now
        workout.last_action_at = now
        workout.actual_duration_seconds = int((now - workout.started_at).total_seconds())
        performance = []
        progression = []
        warmups = 0
        completed_sets = 0
        for exercise in rows:
            working = logs(db, exercise, "WORKING")
            warmups += len(logs(db, exercise, "WARMUP"))
            completed_sets += len(working)
            decision = double_progression(exercise.exercise_id, target_weight=exercise.target_weight, rep_max=exercise.rep_max, required_sets=exercise.config_json["working_sets"], increment=exercise.config_json["increment"], performance=tuple(PerformanceSet(weight=s.weight, reps=s.reps) for s in working))
            if exercise.status == "COMPLETED":
                target = db.get(ExerciseProgression, exercise.exercise_id)
                if target is None:
                    target = ExerciseProgression(exercise_id=exercise.exercise_id)
                    db.add(target)
                target.next_weight = decision.next_weight
                target.source_workout_id = workout.id
                target.updated_at = now
                if decision.progressed:
                    emit(db, "progression.updated", now, decision.model_dump(mode="json") | {"workout_id": workout.id})
            progression.append(decision.model_dump(mode="json"))
            performance.append({"id": exercise.id, "exercise_id": exercise.exercise_id, "name": exercise.config_json["name"], "status": exercise.status, "planned_sets": exercise.planned_working_sets, "completed_sets": len(working), "sets": [{"weight": s.weight, "reps": s.reps} for s in working], "substituted_from": exercise.substituted_from_exercise_id, "reason": exercise.deferred_reason})
        if workout.planned_session_id:
            db.get(PlannedSession, workout.planned_session_id).status = "COMPLETED"
        planned_sets = sum(e["working_sets"] for e in workout.template_snapshot["exercises"])
        audit = {
            "workout_id": workout.id, "template_name": workout.template_snapshot["name"],
            "planned_sets": planned_sets, "completed_sets": completed_sets, "warmup_sets": warmups,
            "actual_duration_seconds": workout.actual_duration_seconds,
            "completed_at": now.isoformat(), "initial_eta": workout.initial_eta.isoformat(),
            "last_estimated_finish": workout.final_eta.isoformat(),
            "initial_eta_error_seconds": (now - workout.initial_eta).total_seconds(),
            "last_eta_error_seconds": (now - workout.final_eta).total_seconds(),
            "substitutions": sum(e.status == "SUBSTITUTED" for e in rows),
            "skips": sum(e.status == "SKIPPED" for e in rows),
            "equipment_waits": workout.waited_for_equipment_count,
            "crowd_feedback": workout.crowd_feedback,
            "progressions": progression, "progression_count": sum(p["progressed"] for p in progression),
            "exercises": performance,
            "instruction": f"{workout.template_snapshot['name']} complete. {completed_sets}/{planned_sets} working sets · {workout.actual_duration_seconds // 60} min · {sum(p['progressed'] for p in progression)} progressions.",
        }
        # PLAN_UPDATED means training targets persisted; calendar replanning is queued.
        transition(db, workout, "PLAN_UPDATED", now)
        emit(db, "planning.replan_required", now, {"workout_id": workout.id, "reason": "workout_completed"})
        return audit

    return mutate(db, "workout.completed", request_id, {"workout_id": workout_id}, now, action)
