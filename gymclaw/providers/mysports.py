"""Observed public MySports route, not a documented freshness/occupancy guarantee.

Only active-checkin is collected. Dated /today percentages and undated weekly
profiles are intentionally not used as headcounts or measured future attendance.
"""
import re
from typing import Protocol

from pydantic import BaseModel, ConfigDict, Field, ValidationError
import requests

from gymclaw.providers.crowd import CrowdReading
from gymclaw.services.errors import DomainError


class Transport(Protocol):
    def request(self, method: str, url: str, **kwargs) -> requests.Response: ...


class ActiveCount(BaseModel):
    model_config = ConfigDict(extra="ignore")
    value: int = Field(ge=0, strict=True)


class MySportsProvider:
    source = "GYM_API"
    demo = False

    def __init__(self, *, studio_id: str = "1234567890", tenant: str = "fixture-tenant", transport: Transport = requests):
        if not re.fullmatch(r"[0-9]{1,20}", studio_id) or not re.fullmatch(r"[a-zA-Z0-9-]{1,100}", tenant):
            raise ValueError("Invalid MySports studio ID or tenant")
        self.provider_id = f"mysports:{tenant}:{studio_id}"
        self.transport = transport
        self.url = f"https://www.mysports.com/nox/public/v1/studios/{studio_id}/utilization/v2/active-checkin"
        self.tenant = tenant

    def get_current_count(self) -> int:
        try:
            response = self.transport.request("GET", self.url, headers={"x-tenant": self.tenant, "Accept": "application/json"},
                timeout=10, allow_redirects=False)
            if response.status_code != 200:
                raise DomainError("GYM_API_UNAVAILABLE", f"MySports public request unavailable (HTTP {response.status_code}); no new count stored")
            return ActiveCount.model_validate(response.json()).value
        except DomainError:
            raise
        except (requests.RequestException, ValueError, ValidationError) as error:
            raise DomainError("GYM_API_INVALID_RESPONSE", "MySports count unavailable or invalid; response/credentials withheld") from error

    def get_reading(self) -> CrowdReading:
        return CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=self.get_current_count())
