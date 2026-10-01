from datetime import time
import os
from typing import Annotated, Literal
from zoneinfo import ZoneInfo

from pydantic import BaseModel, ConfigDict, Field, model_validator, field_validator
from sqlalchemy.orm import Session

from gymclaw.models import UserProfile

Nonnegative = Annotated[int, Field(ge=0)]
Positive = Annotated[int, Field(gt=0)]
Weight = Annotated[float, Field(ge=0, allow_inf_nan=False)]


class Profile(BaseModel):
    model_config = ConfigDict(extra="forbid", from_attributes=True)
    timezone: str = "Europe/Berlin"
    weekly_min_sessions: Nonnegative = 2
    weekly_target_sessions: Nonnegative = 3
    weekly_max_sessions: Annotated[int, Field(ge=0, le=7)] = 3
    weekdays_allowed: list[Annotated[int, Field(ge=0, le=6)]] = [0, 1, 2, 3, 4]
    minimum_full_rest_days_between_sessions: Nonnegative = 1
    earliest_workout_start: time = time(7)
    latest_workout_finish: time = time(22)
    preferred_workout_minutes: Positive = 70
    minimum_workout_minutes: Positive = 45
    prep_minutes: Nonnegative = 15
    commute_to_gym_minutes: Nonnegative = 20
    commute_home_minutes: Nonnegative = 20
    warmup_policy: Literal["first_primary_exercise_one_warmup_set"] = "first_primary_exercise_one_warmup_set"
    default_compound_rest_seconds: Positive = 150
    default_accessory_rest_seconds: Positive = 90
    crowd_preference_weight: Weight = 1
    calendar_preference_weight: Weight = 1
    recovery_weight: Weight = 1

    @field_validator("timezone")
    @classmethod
    def timezone_exists(cls, value):
        try:
            ZoneInfo(value)
        except (KeyError, ValueError) as error:
            raise ValueError("Unknown timezone") from error
        return value

    @model_validator(mode="after")
    def valid_constraints(self):
        if not self.weekly_min_sessions <= self.weekly_target_sessions <= self.weekly_max_sessions:
            raise ValueError("Weekly minimum <= target <= maximum required")
        if self.minimum_workout_minutes > self.preferred_workout_minutes:
            raise ValueError("Minimum duration exceeds preferred duration")
        if self.earliest_workout_start.tzinfo or self.latest_workout_finish.tzinfo:
            raise ValueError("Workout bounds must be local times without timezone")
        if self.earliest_workout_start >= self.latest_workout_finish:
            raise ValueError("Workout bounds must fit within one local day")
        if len(set(self.weekdays_allowed)) != len(self.weekdays_allowed):
            raise ValueError("Duplicate allowed weekdays")
        return self


def get_profile(db: Session) -> Profile:
    row = db.get(UserProfile, 1)
    if row is None:
        profile = Profile(timezone=os.environ.get("GYMCLAW_TIMEZONE", "Europe/Berlin"))
        row = UserProfile(id=1, **profile.model_dump(mode="json"))
        db.add(row)
        db.flush()
    return Profile.model_validate(row)


def update_profile(db: Session, changes: dict) -> Profile:
    profile = Profile.model_validate(get_profile(db).model_dump(mode="json") | changes)
    row = db.get(UserProfile, 1)
    for key, value in profile.model_dump(mode="json").items():
        setattr(row, key, value)
    db.flush()
    return profile
