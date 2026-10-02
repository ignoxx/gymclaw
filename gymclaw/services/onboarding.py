"""Short owner-confirmed setup. Defaults are drafts, not consent or training history."""
import hashlib
import json
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import OnboardingState, RuntimeSettings, WorkoutTemplate
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.templates import get_template
from gymclaw.services.workout import mutate


def fingerprint(value: dict) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def state(db: Session) -> OnboardingState:
    row = db.get(OnboardingState, 1)
    if row is None:
        row = OnboardingState(id=1)
        db.add(row)
        db.flush()
    return row


def template_options(db: Session) -> list[dict]:
    return [{"id": t.id, "name": t.definition_json["name"], "source": t.definition_json.get("source", "user"),
        "exercise_count": len(t.definition_json["exercises"]),
        "fingerprint": fingerprint(get_template(db, t.id).model_dump(mode="json"))}
        for t in db.scalars(select(WorkoutTemplate).order_by(WorkoutTemplate.id))]


def status(db: Session) -> dict:
    row = state(db)
    profile = get_profile(db).model_dump(mode="json")
    profile_hash = fingerprint(profile)
    template = get_template(db, row.template_id).model_dump(mode="json") if row.template_id else None
    template_hash = fingerprint(template) if template else None
    reviewed = bool(row.goal and row.profile_fingerprint == profile_hash and template_hash and row.template_fingerprint == template_hash)
    review_hash = fingerprint({"goal": row.goal, "profile": profile_hash, "template": template_hash})
    ready = reviewed and row.completed_fingerprint == review_hash
    step = "READY" if ready else "GOAL" if not row.goal else "PROFILE" if row.profile_fingerprint != profile_hash else "TEMPLATE" if row.template_fingerprint != template_hash or not template else "REVIEW"
    instructions = {
        "GOAL": "What do you want from training: strength, muscle, or general fitness?",
        "PROFILE": "Review saved schedule, session length and prep/travel times; confirm or change them.",
        "TEMPLATE": "Send your workout plan, or select a saved one. Confirm exercises, sets, reps and starting weights.",
        "REVIEW": "Setup reviewed. Confirm finish to prepare a local plan preview; calendar publication remains separate.",
        "READY": "Setup saved. Ready for a local plan preview; no calendar publication permission implied.",
    }
    settings = db.get(RuntimeSettings, 1)
    return {"step": step, "ready": ready, "goal": row.goal, "profile": profile,
        "profile_fingerprint": profile_hash, "profile_confirmed": row.profile_fingerprint == profile_hash,
        "templates": template_options(db), "template_id": row.template_id,
        "template_fingerprint": template_hash, "review_fingerprint": review_hash,
        "calendar_writes_enabled": bool(settings and settings.calendar_writes_enabled),
        "runtime_enabled": bool(settings and settings.enabled), "instruction": instructions[step]}


def update(db: Session, operation: str, *, now: datetime, request_id: str,
           goal: str | None = None, template_id: str | None = None, expected_fingerprint: str | None = None) -> dict:
    intent = {"goal": goal, "template_id": template_id, "fingerprint": expected_fingerprint}
    def action():
        row = state(db)
        if operation == "set-goal":
            if not isinstance(goal, str) or not 1 <= len(goal.strip()) <= 200:
                raise DomainError("ONBOARDING_GOAL_REQUIRED", "Supply a short declared training goal")
            row.goal = goal.strip()
        elif operation == "confirm-profile":
            if not row.goal:
                raise DomainError("ONBOARDING_ORDER", "Declare training goal first")
            actual = fingerprint(get_profile(db).model_dump(mode="json"))
            if expected_fingerprint != actual:
                raise DomainError("ONBOARDING_REVIEW_STALE", "Profile changed or was not reviewed; read onboarding status again")
            row.profile_fingerprint = actual
        elif operation == "confirm-template":
            if not row.profile_fingerprint or row.profile_fingerprint != fingerprint(get_profile(db).model_dump(mode="json")):
                raise DomainError("ONBOARDING_ORDER", "Confirm current profile first")
            if not template_id:
                raise DomainError("ONBOARDING_TEMPLATE_REQUIRED", "Choose a confirmed workout template")
            template = get_template(db, template_id).model_dump(mode="json")
            if template["source"].lower() in {"demo", "fixture", "synthetic"}:
                raise DomainError("ONBOARDING_REAL_TEMPLATE_REQUIRED", "Demo templates are not real owner training plans")
            actual = fingerprint(template)
            if expected_fingerprint != actual:
                raise DomainError("ONBOARDING_REVIEW_STALE", "Template changed or was not reviewed; read template get again")
            row.template_id, row.template_fingerprint = template_id, actual
        elif operation == "finish":
            current = status(db)
            if current["step"] not in {"REVIEW", "READY"} or expected_fingerprint != current["review_fingerprint"]:
                raise DomainError("ONBOARDING_INCOMPLETE", "Confirm current profile and real template before finishing")
            row.completed_fingerprint = current["review_fingerprint"]
        else:
            raise DomainError("ONBOARDING_OPERATION", "Unsupported onboarding operation")
        if operation != "finish":
            row.completed_fingerprint = None
        db.flush()
        return status(db)
    return mutate(db, "onboarding." + operation, request_id, intent, now, action)
