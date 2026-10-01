from pathlib import Path

from gymclaw.providers.openclaw import AutomationSpec, OpenClawProvider
from gymclaw.services import runtime
from gymclaw.tests.test_runtime import FakeRuntime, NOW, engine


def test_legacy_cron_command_and_managed_config_path():
    calls = []
    def transport(argv):
        calls.append(argv)
        return {"id": "timer", "jobs": []}
    provider = OpenClawProvider(transport=transport)
    provider.list_jobs()
    spec = AutomationSpec(name="test", argv=("/python", "-m", "gymclaw.cli"), cwd="/repo", every="60s", env=(("OPENCLAW_CONFIG_PATH", "/sandbox/.openclaw/openclaw.json"),))
    provider.create(spec)
    assert calls[0][3:5] == ["cron", "list"]
    assert "OPENCLAW_CONFIG_PATH=/sandbox/.openclaw/openclaw.json" in calls[1]
    assert "--no-deliver" in calls[1]


def test_managed_callback_retains_config_without_recreation(engine, monkeypatch):
    monkeypatch.setenv("OPENCLAW_CONFIG_PATH", "/sandbox/.openclaw/openclaw.json")
    provider = FakeRuntime()
    kwargs = dict(now=NOW, recipient="123", project_root=Path.cwd(), allow_runtime_changes=True, allow_messages=True)
    runtime.sync_automations(engine, provider, **kwargs)
    count = len(provider.created)
    assert all(dict(spec.env) == {"OPENCLAW_CONFIG_PATH": "/sandbox/.openclaw/openclaw.json"} for spec in provider.created)
    runtime.sync_automations(engine, provider, **kwargs)
    assert len(provider.created) == count
