"""Double progression; complete prescribed volume at target weight or hold."""
from decimal import Decimal
from typing import Annotated

from pydantic import BaseModel, ConfigDict, Field


class PerformanceSet(BaseModel):
    model_config = ConfigDict(extra="forbid")
    weight: Annotated[float, Field(ge=0, allow_inf_nan=False)]
    reps: Annotated[int, Field(gt=0, strict=True)]


class ProgressionDecision(BaseModel):
    exercise_id: str
    previous_weight: float
    next_weight: float
    progressed: bool
    reason: str


def double_progression(exercise_id: str, *, target_weight: float, rep_max: int, required_sets: int, increment: float, performance: tuple[PerformanceSet, ...]) -> ProgressionDecision:
    if required_sets <= 0 or rep_max <= 0 or increment <= 0:
        raise ValueError("Progression thresholds must be positive")
    if not Decimal(str(target_weight)).is_finite() or not Decimal(str(increment)).is_finite() or target_weight < 0:
        raise ValueError("Progression weights must be finite and nonnegative")
    complete = len(performance) == required_sets
    achieved = complete and all(Decimal(str(s.weight)) == Decimal(str(target_weight)) and s.reps >= rep_max for s in performance)
    next_weight = float(Decimal(str(target_weight)) + Decimal(str(increment))) if achieved else target_weight
    return ProgressionDecision(exercise_id=exercise_id, previous_weight=target_weight, next_weight=next_weight, progressed=achieved, reason="threshold_achieved" if achieved else "incomplete_volume" if not complete else "hold_target_not_met")
