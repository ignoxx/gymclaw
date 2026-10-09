"""Workouts the owner did without the bot (forgot to start it, another gym), logged afterwards."""
from datetime import datetime, timedelta
from zoneinfo import ZoneInfo

from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import PlannedSession, WorkoutSession
from gymclaw.services import adaptation
from gymclaw.services.coach import end_workout
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.set_parser import SetInput
from gymclaw.services.workout import exercises, log_set, mutate, start, utc

SET_SPACING = timedelta(minutes=3)


class PastSet(BaseModel):
    model_config = ConfigDict(extra="forbid")
    exercise: str = Field(min_length=1)
    reps: int = Field(gt=0)
    weight: float = Field(default=0, ge=0)


def log_past(db: Session, sets: list[dict], *, at: datetime, now: datetime, request_id: str, template_id: str | None = None) -> dict:
    """Replay the owner's sets (in the order done) through the normal workout flow, backdated from `at`,
    so history, progression and the week's plan count them. Off-plan exercises don't change the saved
    plan. Without `template_id` it is that day's planned session (a missed one counts after all)."""
    now, at = utc(now), utc(at)
    done = [PastSet.model_validate(s) for s in sets]

    def action():
        if not done:
            raise DomainError("ARGUMENT_REQUIRED", "Pass at least one set")
        if at >= now:
            raise DomainError("WORKOUT_NOT_PAST", "That time hasn't happened yet; start the workout instead")
        zone = ZoneInfo(get_profile(db).timezone)
        day = at.astimezone(zone).date()
        planned = next((s for s in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED", "MISSED"])))
            if s.planned_start_at.astimezone(zone).date() == day and s.workout_template_id and template_id in {None, s.workout_template_id}), None)
        chosen = template_id or (planned.workout_template_id if planned else None)
        if chosen is None:
            raise DomainError("ARGUMENT_REQUIRED", f"No planned workout on {day}; pass template_id")
        if planned and planned.status == "MISSED":
            planned.status = "COMMITTED"
        step = min(SET_SPACING, (now - at) / (len(done) + 1))
        start(db, chosen, now=at, request_id=f"{request_id}:start", planned_session_id=planned.id if planned else None)
        workout = db.scalar(select(WorkoutSession).where(WorkoutSession.status != "PLAN_UPDATED"))
        when = at
        for index, item in enumerate(done):
            active = next((e for e in exercises(db, workout) if e.status == "ACTIVE"), None)
            if active is None or item.exercise.strip().casefold() not in {active.exercise_id.casefold(), active.config_json["name"].casefold(), (active.config_json.get("guide_id") or "").casefold()}:
                adaptation.switch_to(db, workout.id, item.exercise, now=when, request_id=f"{request_id}:switch:{index}", remember=False)
            log_set(db, workout.id, SetInput(weight=item.weight, reps=item.reps), now=when, request_id=f"{request_id}:set:{index}")
            when += step
        summary = end_workout(db, workout, now=when, request_id=f"{request_id}:end")["cards"]
        return {"workout_id": workout.id, "template_id": chosen, "planned_session_id": workout.planned_session_id, "sets": len(done),
            "summary": summary[0]["text"] if summary else None}

    return mutate(db, "workout.logged_past", request_id, {"at": at.isoformat(), "template_id": template_id, "sets": [s.model_dump() for s in done]}, now, action)
