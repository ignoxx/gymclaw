"""Independent crowd contracts. Count and percentage are distinct units."""
from typing import Annotated, Literal, Protocol

from pydantic import BaseModel, ConfigDict, Field, model_validator

from gymclaw.services.errors import DomainError

FiniteNonnegative = Annotated[float, Field(ge=0, allow_inf_nan=False)]
Unit = Annotated[float, Field(ge=0, le=1, allow_inf_nan=False)]


class CrowdReading(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)
    source: Literal["GYM_API", "GOOGLE"]
    metric: Literal["reported_active_count", "busyness_percentage"]
    raw_value: FiniteNonnegative
    normalized_value: Unit | None = None
    freshness_seconds: Annotated[int, Field(ge=0)] | None = None

    @model_validator(mode="after")
    def units(self):
        if self.metric == "reported_active_count":
            if self.source != "GYM_API" or self.raw_value != int(self.raw_value) or self.normalized_value is not None:
                raise ValueError("Reported count must be whole/nonnegative and has no count-to-percentage conversion")
        elif self.source != "GOOGLE" or self.raw_value > 100 or self.normalized_value != self.raw_value / 100:
            raise ValueError("Google percentage proxy must stay separate from headcount")
        return self


class CrowdProvider(Protocol):
    source: str
    provider_id: str
    demo: bool

    def get_reading(self) -> CrowdReading: ...


class GymOccupancyProvider(Protocol):
    def get_current_count(self) -> int: ...


class PopularTime(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)
    weekday: Annotated[int, Field(ge=0, le=6)]
    hour: Annotated[int, Field(ge=0, le=23)]
    percentage: Annotated[float, Field(ge=0, le=100, allow_inf_nan=False)]
    provenance: Literal["provider_weekday_profile"] = "provider_weekday_profile"


class GoogleBusynessProvider(Protocol):
    def get_current_busyness(self) -> CrowdReading: ...
    def get_popular_times(self) -> tuple[PopularTime, ...]: ...


class UnavailableGoogleBusynessProvider:
    """No fabricated scraping/Places API. Implement a verified adapter later."""
    source = "GOOGLE"
    provider_id = "google-unconfigured"
    demo = False

    def get_reading(self) -> CrowdReading:
        raise DomainError("GOOGLE_BUSYNESS_UNAVAILABLE", "Google busyness acquisition not configured; other sources remain usable")

    def get_current_busyness(self) -> CrowdReading:
        return self.get_reading()

    def get_popular_times(self) -> tuple[PopularTime, ...]:
        raise DomainError("GOOGLE_BUSYNESS_UNAVAILABLE", "No verified Google Popular Times acquisition configured")


class FixtureCrowdProvider:
    """Explicit synthetic/imported signal; never labeled a live request."""
    demo = True

    def __init__(self, reading: CrowdReading):
        self.reading = reading
        self.source = reading.source
        self.provider_id = "fixture:" + reading.source

    def get_reading(self) -> CrowdReading:
        return self.reading
