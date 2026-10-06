"""Small OpenClaw CLI adapter; never installs or edits Gateway config.

Contract researched against official CLI docs/source. Live acceptance still
requires the locally installed version. Transport injection keeps tests offline.
"""
from collections.abc import Callable
from dataclasses import dataclass
import json
import os
import re
import shutil
import subprocess
from typing import Protocol

from gymclaw.services.errors import DomainError


def validate_route(profile: str, recipient: str):
    if not re.fullmatch(r"gymclaw(?:-[a-z0-9-]+)?", profile):
        raise DomainError("RUNTIME_PROFILE_REQUIRED", "Use dedicated gymclaw or gymclaw-* OpenClaw profile; never default")
    if not re.fullmatch(r"[1-9][0-9]{0,19}", recipient):
        raise DomainError("TELEGRAM_OWNER_REQUIRED", "Use one verified positive numeric Telegram user ID, not a group or username")


@dataclass(frozen=True)
class AutomationSpec:
    name: str
    argv: tuple[str, ...]
    cwd: str
    at: str | None = None
    every: str | None = None
    cron: str | None = None
    timezone: str | None = None
    env: tuple[tuple[str, str], ...] = ()

    def __post_init__(self):
        if sum(value is not None for value in (self.at, self.every, self.cron)) != 1:
            raise ValueError("Automation requires exactly one schedule")


class AutomationProvider(Protocol):
    profile: str

    def list_jobs(self) -> list[dict]: ...
    def create(self, spec: AutomationSpec) -> str: ...
    def remove(self, job_id: str): ...
    def send(self, recipient: str, message: str, buttons: tuple[tuple[str, str], ...] = ()) -> str: ...


def cli_json(text: str) -> dict:
    """Accept final JSON object after CLI startup logs; reject trailing garbage."""
    candidates = [0] + [match.start() for match in re.finditer(r"(?m)^\s*\{", text)]
    decoder = json.JSONDecoder()
    for start in reversed(candidates):
        chunk = text[start:].lstrip()
        try:
            value, end = decoder.raw_decode(chunk)
        except ValueError:
            continue
        if isinstance(value, dict) and not chunk[end:].strip():
            return value
    raise ValueError("No final CLI JSON object")


def run_json(argv: list[str]) -> dict:
    if shutil.which(argv[0]) is None:
        raise DomainError("OPENCLAW_NOT_INSTALLED", "OpenClaw CLI unavailable; setup: docs/self-hosting.md")
    try:
        env = dict(os.environ)
        if env.get("OPENCLAW_CONFIG_PATH") == "/sandbox/.openclaw/openclaw.json":
            # Cron inherits a URL override, which intentionally disables config auth.
            # Use this sandbox's managed config route, not the inherited override.
            env.pop("OPENCLAW_GATEWAY_URL", None)
        result = subprocess.run(argv, capture_output=True, text=True, timeout=45, check=False, env=env)
        if result.returncode != 0:
            # Classify known failures without exposing child output or credentials.
            failure = "\n".join(line for line in (result.stdout + "\n" + result.stderr).lower().splitlines() if "undici-ehpa" not in line and "envhttpproxyagent is experimental" not in line)
            if "pairing required" in failure or "scope upgrade pending approval" in failure:
                raise DomainError("GATEWAY_PAIRING_REQUIRED", "Gateway CLI device/scope approval required")
            if "econnrefused" in failure or "gateway timeout" in failure:
                raise DomainError("GATEWAY_UNREACHABLE", "Gateway connection unavailable")
            raise DomainError("OPENCLAW_COMMAND_FAILED", "OpenClaw command failed; inspect local runtime status (child output withheld)")
        value = cli_json(result.stdout)
        if not isinstance(value, dict) or value.get("ok") is False:
            raise ValueError("Unexpected CLI response")
        return value
    except DomainError:
        raise
    except (subprocess.TimeoutExpired, OSError, ValueError) as error:
        raise DomainError("OPENCLAW_RESPONSE_UNKNOWN", "OpenClaw response unavailable/unrecognized; operation outcome may be unknown") from error


class OpenClawProvider:
    def __init__(self, profile: str = "gymclaw", *, transport: Callable[[list[str]], dict] = run_json):
        validate_route(profile, "1")
        self.profile = profile
        self.transport = transport

    def call(self, *args: str) -> dict:
        return self.transport(["openclaw", "--profile", self.profile, *args, "--json"])

    def list_jobs(self) -> list[dict]:
        value = self.call("cron", "list", "--all")
        jobs = value.get("jobs")
        if not isinstance(jobs, list) or any(not isinstance(j, dict) or not isinstance(j.get("id"), str) or not isinstance(j.get("name"), str) for j in jobs):
            raise DomainError("OPENCLAW_RESPONSE_UNKNOWN", "Automation list schema unrecognized; nothing reconciled")
        return jobs

    def create(self, spec: AutomationSpec) -> str:
        args = ["cron", "add", "--name", spec.name, "--declaration-key", spec.name,
                "--command-argv", json.dumps(spec.argv), "--command-cwd", spec.cwd,
                "--session", "isolated", "--no-deliver", "--timeout-seconds", "120"]
        for key, value in spec.env:
            args += ["--command-env", f"{key}={value}"]
        if spec.at:
            args += ["--at", spec.at, "--delete-after-run"]
        elif spec.every:
            args += ["--every", spec.every]
        else:
            args += ["--cron", spec.cron, "--tz", spec.timezone or "Europe/Berlin", "--exact"]
        value = self.call(*args)
        job = value.get("job", value)
        if not isinstance(job, dict) or not isinstance(job.get("id"), str) or not job["id"]:
            raise DomainError("OPENCLAW_RESPONSE_UNKNOWN", "Automation creation not confirmed; next sync reconciles stable declaration key")
        return job["id"]

    def remove(self, job_id: str):
        self.call("cron", "remove", job_id)

    def send(self, recipient: str, message: str, buttons: tuple[tuple[str, str], ...] = ()) -> str:
        """`buttons` are (label, callback data) pairs in one row, e.g. ("▶️ Start workout", "gc:begin:1a2b3c4d")."""
        validate_route(self.profile, recipient)
        extra = ["--presentation", json.dumps({"blocks": [{"type": "buttons", "buttons": [{"label": label, "value": value} for label, value in buttons]}]})] if buttons else []
        value = self.call("message", "send", "--channel", "telegram", "--target", recipient, "--message", message, *extra)
        payload = value.get("payload", {})
        receipt = value.get("messageId") or (payload.get("messageId") if isinstance(payload, dict) else None)
        if value.get("action") != "send" or value.get("channel") != "telegram" or value.get("dryRun") is not False or not receipt or value.get("ok") is False or (isinstance(payload, dict) and payload.get("ok") is False):
            raise DomainError("TELEGRAM_DELIVERY_UNKNOWN", "No confirmed Telegram message receipt; do not blindly resend")
        return str(receipt)
