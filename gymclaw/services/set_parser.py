"""Parse a whole-message set in either order ("9x80", "80kgx9", "9 reps at 80"). Reps first is the default."""
import re
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field

from gymclaw.services.errors import DomainError


class SetInput(BaseModel):
    model_config = ConfigDict(extra="forbid")
    weight: Annotated[float, Field(ge=0, allow_inf_nan=False)]
    reps: Annotated[int, Field(gt=0, strict=True)]
    set_type: Literal["WARMUP", "WORKING"] = "WORKING"
    rir: Annotated[float, Field(ge=0, allow_inf_nan=False)] | None = None


NUMBER = r"\d+(?:[.,]\d+)?"
PAIR = re.compile(rf"({NUMBER})\s*(kg|reps?)?\s*[x×*]\s*({NUMBER})\s*(kg|reps?)?")
AT = re.compile(rf"(\d+)\s*reps?\s*(?:at|@)\s*({NUMBER})\s*(?:kg)?")
MAX_PLAUSIBLE_REPS = 30


def parse_set(text: str, *, set_type: Literal["WARMUP", "WORKING"] = "WORKING", expected_weight: float | None = None) -> SetInput:
    """Order is decided by unit words, then closeness to expected_weight, then rep plausibility, then reps-first."""
    clean = text.strip().lower()
    if match := AT.fullmatch(clean):
        return SetInput(weight=float(match[2].replace(",", ".")), reps=int(match[1]), set_type=set_type)
    match = PAIR.fullmatch(clean)
    if not match:
        raise DomainError("AMBIGUOUS_SET", "How many reps and what weight? E.g. 9x80.")
    left, left_unit, right, right_unit = match.groups()
    a, b = float(left.replace(",", ".")), float(right.replace(",", "."))
    if left_unit == right_unit and left_unit:
        raise DomainError("AMBIGUOUS_SET", "How many reps and what weight? E.g. 9x80.")
    if left_unit == "kg" or (right_unit and right_unit.startswith("rep")):
        weight, reps = a, b
    elif right_unit == "kg" or (left_unit and left_unit.startswith("rep")):
        weight, reps = b, a
    elif not a.is_integer() or not b.is_integer():
        weight, reps = (a, b) if not a.is_integer() else (b, a)
    elif expected_weight is not None and abs(a - expected_weight) != abs(b - expected_weight):
        weight, reps = (a, b) if abs(a - expected_weight) < abs(b - expected_weight) else (b, a)
    elif (a > MAX_PLAUSIBLE_REPS) != (b > MAX_PLAUSIBLE_REPS):
        weight, reps = (a, b) if a > MAX_PLAUSIBLE_REPS else (b, a)
    else:
        reps, weight = a, b
    if not reps.is_integer() or reps <= 0:
        raise DomainError("AMBIGUOUS_SET", "How many reps and what weight? E.g. 9x80.")
    return SetInput(weight=weight, reps=int(reps), set_type=set_type)
