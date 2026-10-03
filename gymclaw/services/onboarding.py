"""Mandatory owner interview, then an owner-confirmed plan. Defaults never count as answers.

Steps: INTERVIEW (one question at a time) → PLAN (owner's plan from a photo, or one GymClaw builds
from the catalog) → REVIEW (one summary, owner confirms) → READY. Ready stays ready: later profile or
template edits don't send the owner back into setup. Grants no runtime or calendar authority.
"""
import hashlib
import json
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import OnboardingState, RuntimeSettings, WorkoutTemplate
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile, update_profile
from gymclaw.services.templates import get_template
from gymclaw.services.workout import mutate

# Asked in this order. Profile-backed answers must also write these profile fields.
QUESTIONS = {
    "goal": ("What's your main goal: build muscle, get stronger, lose fat, or general fitness?", ()),
    "experience": ("How long have you been training consistently?", ()),
    "schedule": ("How many days a week do you want to train, and which days work?", ("weekly_target_sessions", "weekdays_allowed")),
    "session_length": ("How long can one gym session be?", ("preferred_workout_minutes",)),
    "time_window": ("When can you train: earliest start and latest finish?", ("earliest_workout_start", "latest_workout_finish")),
    "travel": ("Which gym (address), and how long do you need to get ready and to get there?", ("gym_address", "prep_minutes", "commute_to_gym_minutes")),
    "equipment": ("Where do you train: full gym, basic gym, or at home?", ()),
    "limitations": ("Any injuries or exercises you want to avoid?", ()),
}


def fingerprint(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def state(db: Session) -> OnboardingState:
    row = db.get(OnboardingState, 1)
    if row is None:
        row = OnboardingState(id=1, interview_json={}, split_json=[])
        db.add(row)
        db.flush()
    return row


def template_options(db: Session) -> list[dict]:
    return [{"id": t.id, "name": t.definition_json["name"], "source": t.definition_json.get("source", "user"),
        "exercise_count": len(t.definition_json["exercises"])} for t in db.scalars(select(WorkoutTemplate).order_by(WorkoutTemplate.id))]


def review_fingerprint(db: Session, row: OnboardingState) -> str:
    templates = [get_template(db, t).model_dump(mode="json") for t in row.split_json]
    return fingerprint({"interview": row.interview_json, "profile": get_profile(db).model_dump(mode="json"), "split": templates})


def status(db: Session) -> dict:
    row = state(db)
    missing = [key for key in QUESTIONS if key not in row.interview_json]
    ready = not missing and bool(row.split_json) and row.completed_fingerprint is not None
    step = "READY" if ready else "INTERVIEW" if missing else "PLAN" if not row.split_json else "REVIEW"
    instructions = {
        "INTERVIEW": QUESTIONS[missing[0]][0] if missing else "",
        "PLAN": "Do you have a workout plan? Send a photo, or I'll build one for you.",
        "REVIEW": "Summarise the answers and split in one short message; finish when the owner confirms.",
        "READY": "Setup done. Plan the week; calendar publication still needs its own approval.",
    }
    settings = db.get(RuntimeSettings, 1)
    return {"step": step, "ready": ready, "next_question": missing[0] if missing else None, "missing": missing,
        "answers": row.interview_json, "profile": get_profile(db).model_dump(mode="json"),
        "split": row.split_json, "templates": template_options(db),
        "review_fingerprint": review_fingerprint(db, row) if row.split_json else None,
        "calendar_writes_enabled": bool(settings and settings.calendar_writes_enabled),
        "runtime_enabled": bool(settings and settings.enabled), "instruction": instructions[step]}


def apply_profile(db: Session, changes: dict):
    """Weekly min/max and minimum session length follow the owner's answers so validation holds."""
    current = get_profile(db)
    derived = dict(changes)
    if "weekly_target_sessions" in changes:
        target = changes["weekly_target_sessions"]
        derived |= {"weekly_max_sessions": target, "weekly_min_sessions": min(current.weekly_min_sessions, target)}
    if "preferred_workout_minutes" in changes:
        derived["minimum_workout_minutes"] = min(current.minimum_workout_minutes, changes["preferred_workout_minutes"])
    if "commute_to_gym_minutes" in changes and "commute_home_minutes" not in changes:
        derived["commute_home_minutes"] = changes["commute_to_gym_minutes"]
    update_profile(db, derived)


def update(db: Session, operation: str, *, now: datetime, request_id: str, answers: dict | None = None,
           profile: dict | None = None, template_ids: list[str] | None = None, expected_fingerprint: str | None = None) -> dict:
    intent = {"answers": answers, "profile": profile, "template_ids": template_ids, "fingerprint": expected_fingerprint}

    def action():
        row = state(db)
        if operation == "answer":
            if not answers or not isinstance(answers, dict):
                raise DomainError("ONBOARDING_ANSWER_REQUIRED", "Supply at least one interview answer")
            unknown = set(answers) - set(QUESTIONS)
            if unknown:
                raise DomainError("ONBOARDING_UNKNOWN_QUESTION", f"Unknown interview keys: {', '.join(sorted(unknown))}")
            if any(not isinstance(v, str) or not 1 <= len(v.strip()) <= 300 for v in answers.values()):
                raise DomainError("ONBOARDING_ANSWER_REQUIRED", "Answers are short owner-stated text (1–300 chars)")
            needed = {field for key in answers for field in QUESTIONS[key][1]}
            if needed - set(profile or {}):
                raise DomainError("ONBOARDING_PROFILE_REQUIRED", "Also set profile fields: " + ", ".join(sorted(needed - set(profile or {}))))
            if profile:
                apply_profile(db, profile)
            row.interview_json = row.interview_json | {key: value.strip() for key, value in answers.items()}
            if "goal" in answers:
                row.goal = answers["goal"].strip()
        elif operation == "confirm-plan":
            if not template_ids:
                raise DomainError("ONBOARDING_TEMPLATE_REQUIRED", "Choose one or more confirmed workout templates, in rotation order")
            for template_id in template_ids:
                if get_template(db, template_id).source.lower() in {"demo", "demo_fixture", "fixture", "synthetic"}:
                    raise DomainError("ONBOARDING_REAL_TEMPLATE_REQUIRED", "Demo templates are not real owner training plans")
            row.split_json = list(template_ids)
            row.template_id = template_ids[0]
        elif operation == "finish":
            current = status(db)
            if current["step"] not in {"REVIEW", "READY"} or expected_fingerprint != current["review_fingerprint"]:
                raise DomainError("ONBOARDING_INCOMPLETE", "Finish the interview and confirm a plan, then use the current review fingerprint")
            row.completed_fingerprint = current["review_fingerprint"]
        else:
            raise DomainError("ONBOARDING_OPERATION", "Unsupported onboarding operation")
        db.flush()
        return status(db)

    return mutate(db, "onboarding." + operation, request_id, intent, now, action)
