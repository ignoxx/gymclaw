"""Small OpenClaw CLI adapter; never installs or edits Gateway config.

Contract researched against official CLI docs/source. Live acceptance still
requires the locally installed version. Transport injection keeps tests offline.
"""
from collections.abc import Callable
from dataclasses import dataclass
import json
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

    def __post_init__(self):
        if (self.at is None) == (self.every is None):
            raise ValueError("Automation requires exactly one schedule")


class AutomationProvider(Protocol):
    profile: str

    def list_jobs(self) -> list[dict]: ...
    def create(self, spec: AutomationSpec) -> str: ...
    def remove(self, job_id: str): ...
    def send(self, recipient: str, message: str) -> str: ...


def run_json(argv: list[str]) -> dict:
    if shutil.which(argv[0]) is None:
        raise DomainError("OPENCLAW_NOT_INSTALLED", "OpenClaw CLI unavailable; activation instructions: openclaw/README.md")
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=45, check=False)
        if result.returncode != 0:
            raise DomainError("OPENCLAW_COMMAND_FAILED", "OpenClaw command failed; inspect local runtime status (child output withheld)")
        value = json.loads(result.stdout)
        if not isinstance(value, dict) or value.get("ok") is False:
            raise ValueError("Unexpected CLI response")
        return value
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
        value = self.call("automations", "list", "--all")
        jobs = value.get("jobs")
        if not isinstance(jobs, list) or any(not isinstance(j, dict) or not isinstance(j.get("id"), str) or not isinstance(j.get("name"), str) for j in jobs):
            raise DomainError("OPENCLAW_RESPONSE_UNKNOWN", "Automation list schema unrecognized; nothing reconciled")
        return jobs

    def create(self, spec: AutomationSpec) -> str:
        args = ["automations", "add", "--name", spec.name, "--declaration-key", spec.name,
                "--command-argv", json.dumps(spec.argv), "--command-cwd", spec.cwd,
                "--session", "isolated", "--no-deliver", "--timeout-seconds", "120"]
        if spec.at:
            args += ["--at", spec.at, "--delete-after-run"]
        else:
            args += ["--every", spec.every]
        value = self.call(*args)
        if not isinstance(value.get("id"), str) or not value["id"]:
            raise DomainError("OPENCLAW_RESPONSE_UNKNOWN", "Automation creation not confirmed; next sync reconciles stable declaration key")
        return value["id"]

    def remove(self, job_id: str):
        self.call("automations", "remove", job_id)

    def send(self, recipient: str, message: str) -> str:
        validate_route(self.profile, recipient)
        value = self.call("message", "send", "--channel", "telegram", "--target", recipient, "--message", message)
        payload = value.get("payload", {})
        receipt = value.get("messageId") or (payload.get("messageId") if isinstance(payload, dict) else None)
        if value.get("action") != "send" or value.get("channel") != "telegram" or value.get("dryRun") is not False or not receipt or value.get("ok") is False or (isinstance(payload, dict) and payload.get("ok") is False):
            raise DomainError("TELEGRAM_DELIVERY_UNKNOWN", "No confirmed Telegram message receipt; do not blindly resend")
        return str(receipt)
