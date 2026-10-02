"""Crowd reading contract: the gym's reported active check-in count."""
from typing import Annotated, Literal, Protocol

from pydantic import BaseModel, ConfigDict, Field, model_validator


FiniteNonnegative = Annotated[float, Field(ge=0, allow_inf_nan=False)]
Unit = Annotated[float, Field(ge=0, le=1, allow_inf_nan=False)]


class CrowdReading(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)
    source: Literal["GYM_API"]
    metric: Literal["reported_active_count"]
    raw_value: FiniteNonnegative
    normalized_value: Unit | None = None
    freshness_seconds: Annotated[int, Field(ge=0)] | None = None

    @model_validator(mode="after")
    def units(self):
        if self.raw_value != int(self.raw_value) or self.normalized_value is not None:
            raise ValueError("Reported count must be whole/nonnegative and has no count-to-percentage conversion")
        return self


class CrowdProvider(Protocol):
    source: str
    provider_id: str
    demo: bool

    def get_reading(self) -> CrowdReading: ...


class GymOccupancyProvider(Protocol):
    def get_current_count(self) -> int: ...


class FixtureCrowdProvider:
    """Explicit synthetic/imported signal; never labeled a live request."""
    demo = True

    def __init__(self, reading: CrowdReading):
        self.reading = reading
        self.source = reading.source
        self.provider_id = "fixture:" + reading.source

    def get_reading(self) -> CrowdReading:
        return self.reading
