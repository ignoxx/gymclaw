"""Parse only unambiguous whole-message weight × reps forms."""
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


def parse_set(text: str, *, set_type: Literal["WARMUP", "WORKING"] = "WORKING") -> SetInput:
    number = r"\d+(?:\.\d+)?"
    match = re.fullmatch(rf"\s*({number})\s*(?:kg\s*)?[x×]\s*(\d+)\s*", text, re.IGNORECASE)
    if match:
        weight, reps = match.groups()
    else:
        match = re.fullmatch(rf"\s*(\d+)\s+reps\s+at\s+({number})\s*(?:kg)?\s*", text, re.IGNORECASE)
        if not match:
            raise DomainError("AMBIGUOUS_SET", "What weight (kg) and reps? Use 80x9.")
        reps, weight = match.groups()
    return SetInput(weight=float(weight), reps=int(reps), set_type=set_type)
