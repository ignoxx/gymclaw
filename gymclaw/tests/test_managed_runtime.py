from pathlib import Path
import subprocess
import pytest

from gymclaw.services.errors import DomainError

from gymclaw.providers.openclaw import AutomationSpec, OpenClawProvider, run_json, cli_json
from gymclaw.services import runtime
from gymclaw.tests.test_runtime import FakeRuntime, NOW, engine


@pytest.mark.parametrize("prefix", ["", "[plugins] Loaded\n", "\x1b[32mStartup\x1b[0m\n"])
def test_cli_startup_logs_do_not_hide_final_json(prefix):
    assert cli_json(prefix + '{\n "payload": {"messageId": "test-receipt"}\n}\n') == {"payload": {"messageId": "test-receipt"}}


@pytest.mark.parametrize("output", ['{"jobs":[]}\ntrailing garbage', '[{"jobs":[]}]', 'no JSON'])
def test_cli_json_rejects_partial_or_unstructured_output(output):
    with pytest.raises(ValueError):
        cli_json(output)


def test_cli_pairing_failure_is_actionable_without_child_output(monkeypatch):
    monkeypatch.setattr("gymclaw.providers.openclaw.shutil.which", lambda _: "/openclaw")
    monkeypatch.setattr("gymclaw.providers.openclaw.subprocess.run", lambda *a, **k: subprocess.CompletedProcess(a, 1, "", "scope upgrade pending approval; private-output-sentinel"))
    with pytest.raises(DomainError) as caught:
        run_json(["openclaw", "cron", "add"])
    assert caught.value.code == "GATEWAY_PAIRING_REQUIRED"
    assert "private-output-sentinel" not in str(caught.value)


def test_cli_failure_keeps_error_class_without_child_output(monkeypatch):
    monkeypatch.setattr("gymclaw.providers.openclaw.shutil.which", lambda _: "/openclaw")
    monkeypatch.setattr("gymclaw.providers.openclaw.subprocess.run", lambda *a, **k: subprocess.CompletedProcess(a, 1, "", "private-output-sentinel"))
    with pytest.raises(DomainError) as caught:
        run_json(["openclaw", "cron", "list", "--json"])
    assert caught.value.code == "OPENCLAW_COMMAND_FAILED"
    assert "private-output-sentinel" not in str(caught.value)


@pytest.mark.parametrize("wrapped", [False, True])
def test_legacy_cron_command_and_managed_config_path(wrapped):
    calls = []
    def transport(argv):
        calls.append(argv)
        if wrapped and "add" in argv:
            return {"created": False, "updated": False, "job": {"id": "timer"}}
        return {"id": "timer", "jobs": []}
    provider = OpenClawProvider(transport=transport)
    provider.list_jobs()
    spec = AutomationSpec(name="test", argv=("/python", "-m", "gymclaw.cli"), cwd="/repo", every="60s", env=(("OPENCLAW_CONFIG_PATH", "/state/openclaw.json"),))
    assert provider.create(spec) == "timer"
    assert calls[0][3:5] == ["cron", "list"]
    assert "OPENCLAW_CONFIG_PATH=/state/openclaw.json" in calls[1]
    assert "--no-deliver" in calls[1]


def test_managed_callback_retains_config_without_recreation(engine, monkeypatch):
    monkeypatch.setenv("OPENCLAW_CONFIG_PATH", "/state/openclaw.json")
    provider = FakeRuntime()
    kwargs = dict(now=NOW, recipient="123", project_root=Path.cwd(), allow_runtime_changes=True, allow_messages=True)
    runtime.sync_automations(engine, provider, **kwargs)
    count = len(provider.created)
    assert all(dict(spec.env) == {"OPENCLAW_CONFIG_PATH": "/state/openclaw.json"} for spec in provider.created)
    runtime.sync_automations(engine, provider, **kwargs)
    assert len(provider.created) == count
