"""Configurable crowd sources: every gym exposes its check-in count differently, if at all.

Config is private JSON at data/crowd-config.json (or GYMCLAW_CROWD_CONFIG), one of:

  {"kind": "mysports", "studio_id": "123", "tenant": "my-gym"}
  {"kind": "http", "url": "https://…", "headers": {…}, "json_path": "data.0.count"}
  {"kind": "http", "url": "https://…", "regex": "(\\d+) people"}
  {"kind": "command", "argv": ["python3", "data/crowd-source.py"]}

The file holds everything needed to read the count; nothing else configures the source.
A file without "kind" is the original MySports shape. See docs/crowd.md.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter, ValidationError
import requests

from gymclaw.providers.crowd import CrowdReading
from gymclaw.providers.mysports import MySportsConfig, MySportsProvider, Transport
from gymclaw.services.errors import DomainError


class HttpSource(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["http"]
    url: str = Field(pattern=r"^https://")
    headers: dict[str, str] = {}
    # Exactly one: dot path into the JSON body ("data.0.count") or a regex whose first group is the count.
    json_path: str | None = None
    regex: str | None = None


class CommandSource(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["command"]
    # Run from the repo root without a shell. Stdout must be the count, alone or as {"value": n}.
    argv: list[str] = Field(min_length=1)
    timeout_seconds: int = Field(default=30, ge=1, le=120)


class MySportsSource(MySportsConfig):
    kind: Literal["mysports"] = "mysports"


SourceConfig = TypeAdapter(Annotated[HttpSource | CommandSource | MySportsSource, Field(discriminator="kind")])


def count(value) -> int:
    """Whole, nonnegative count from a JSON value or text; anything else is invalid."""
    if isinstance(value, str) and re.fullmatch(r"\s*\d+\s*", value):
        return int(value)
    if isinstance(value, int) and not isinstance(value, bool) and value >= 0:
        return value
    raise ValueError("Not a whole nonnegative count")


def dig(body, path: str):
    for key in path.split("."):
        body = body[int(key)] if isinstance(body, list) else body[key]
    return body


class HttpCountProvider:
    source = "GYM_API"
    demo = False

    def __init__(self, config: HttpSource, *, transport: Transport = requests):
        if (config.json_path is None) == (config.regex is None):
            raise ValueError("Set exactly one of json_path or regex")
        if config.regex is not None and re.compile(config.regex).groups < 1:
            raise ValueError("regex needs a group for the count")
        self.config, self.transport = config, transport
        # Headers and the extraction rule can select another gym at the same URL, so they're part of the identity.
        self.provider_id = f"http:{config.url}#" + hashlib.sha256(config.model_dump_json().encode()).hexdigest()[:12]

    def get_reading(self) -> CrowdReading:
        try:
            response = self.transport.request("GET", self.config.url, headers=self.config.headers, timeout=10, allow_redirects=False)
            if response.status_code != 200:
                raise DomainError("GYM_API_UNAVAILABLE", f"Crowd source unavailable (HTTP {response.status_code}); no new count stored")
            if self.config.json_path is not None:
                value = count(dig(response.json(), self.config.json_path))
            else:
                match = re.search(self.config.regex, response.text)
                value = count(match.group(1) if match else None)
        except DomainError:
            raise
        except (requests.RequestException, ValueError, LookupError, TypeError, IndexError) as error:
            raise DomainError("GYM_API_INVALID_RESPONSE", "Crowd count unavailable or invalid; response withheld") from error
        return CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=value)


class CommandCountProvider:
    source = "GYM_API"
    demo = False

    def __init__(self, config: CommandSource, *, run=subprocess.run):
        self.config, self.run = config, run
        self.provider_id = "command:" + " ".join(config.argv)

    def get_reading(self) -> CrowdReading:
        try:
            done = self.run(self.config.argv, capture_output=True, text=True, timeout=self.config.timeout_seconds)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise DomainError("GYM_API_UNAVAILABLE", "Crowd source command failed to run or timed out") from error
        if done.returncode != 0:
            raise DomainError("GYM_API_UNAVAILABLE", f"Crowd source command exited {done.returncode}; output withheld")
        try:
            out = done.stdout.strip()
            value = count(json.loads(out)["value"] if out.startswith("{") else out)
        except (ValueError, LookupError, TypeError) as error:
            raise DomainError("GYM_API_INVALID_RESPONSE", "Crowd source command printed no valid count") from error
        return CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=value)


def from_environment(*, transport: Transport = requests, run=subprocess.run):
    """The configured crowd provider, or GYM_API_NOT_CONFIGURED. Config values never appear in errors."""
    try:
        values = json.loads(Path(os.environ.get("GYMCLAW_CROWD_CONFIG", "data/crowd-config.json")).read_text())
        if not isinstance(values, dict):
            raise ValueError("Invalid crowd config")
        if "kind" not in values:
            # Original MySports file: only the two identifiers count.
            values = {"kind": "mysports", "studio_id": values.get("studio_id"), "tenant": values.get("tenant")}
        config = SourceConfig.validate_python(values)
        if isinstance(config, MySportsSource):
            return MySportsProvider(studio_id=config.studio_id, tenant=config.tenant, transport=transport)
        if isinstance(config, HttpSource):
            return HttpCountProvider(config, transport=transport)
        return CommandCountProvider(config, run=run)
    except (OSError, ValueError, ValidationError, re.error):
        raise DomainError("GYM_API_NOT_CONFIGURED", "No valid crowd source configured; see docs/crowd.md") from None
