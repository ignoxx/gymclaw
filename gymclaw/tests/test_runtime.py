"""Synthetic scheduler contracts. No installed OpenClaw or live Telegram needed."""
from datetime import datetime, timedelta, timezone
from pathlib import Path
import subprocess

import pytest
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import NotificationDelivery, NotificationJob, PlannedSession
from gymclaw.providers.openclaw import AutomationSpec, OpenClawProvider, run_json, validate_route
from gymclaw.services import runtime, workout
from gymclaw.services.errors import DomainError
from gymclaw.services.set_parser import SetInput
from gymclaw.services.templates import ExerciseSpec, Template, import_template

NOW = datetime(2026, 10, 12, 18, tzinfo=timezone.utc)


class FakeRuntime:
    profile = "gymclaw"

    def __init__(self):
        self.jobs = []
        self.sent = []
        self.created = []
        self.removed = []
        self.fail_send = False
        self.fail_create_after_apply = False

    def list_jobs(self):
        return self.jobs.copy()

    def create(self, spec):
        old = next((row for row in self.jobs if row["name"] == spec.name), None)
        if old:
            return old["id"]
        row = {"id": f"external-{len(self.created)}", "name": spec.name, "enabled": True,
            "payload": {"kind": "command", "argv": list(spec.argv), "cwd": spec.cwd},
            "delivery": {"mode": "none"}, "schedule": {"kind": "at", "at": spec.at} if spec.at else {"kind": "every", "everyMs": 60000}}
        self.jobs.append(row)
        self.created.append(spec)
        if self.fail_create_after_apply:
            raise DomainError("TEST_LOST_RESPONSE", "Lost creation response")
        return row["id"]

    def remove(self, job_id):
        self.removed.append(job_id)
        self.jobs = [row for row in self.jobs if row["id"] != job_id]

    def send(self, recipient, message):
        self.sent.append((recipient, message))
        if self.fail_send:
            raise DomainError("TEST_LOST_SEND", "Lost send response")
        return "message-1"


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'runtime.db'}")
    initialize(engine)
    with Session(engine) as db, db.begin():
        import_template(db, Template(id="short", name="Short", exercises=[ExerciseSpec(id="bench", name="Bench", role="press", primary=True, working_sets=3, target_weight=80, warmup_weight=50, rest_seconds=100)]))
    yield engine
    engine.dispose()


def rest_job(engine):
    with Session(engine) as db, db.begin():
        workout_id = workout.start(db, "short", now=NOW, request_id="start")["data"]["workout_id"]
        workout.log_set(db, workout_id, SetInput(weight=50, reps=8, set_type="WARMUP"), now=NOW, request_id="warmup")
        result = workout.log_set(db, workout_id, SetInput(weight=80, reps=9), now=NOW, request_id="one")["data"]
        return workout_id, result["rest_job"]["id"]


def sync(engine, provider, **kwargs):
    return runtime.sync_automations(engine, provider, now=NOW, recipient="123", project_root=Path.cwd(),
        allow_runtime_changes=True, allow_messages=True, include_watcher=False, **kwargs)


def prepare(engine, job_id):
    with Session(engine) as db, db.begin():
        return runtime.prepare_notification(db, job_id, now=NOW + timedelta(seconds=100), recipient="123", profile="gymclaw")


def test_scheduler_adopts_creation_after_lost_response_and_restart(engine):
    _, job_id = rest_job(engine)
    provider = FakeRuntime()
    provider.fail_create_after_apply = True
    with pytest.raises(DomainError):
        sync(engine, provider)
    with Session(engine) as db:
        assert db.get(NotificationJob, job_id).external_job_id is None
    provider.fail_create_after_apply = False
    result = sync(engine, provider)
    assert result["results"][0]["action"] == "adopted"
    assert len(provider.created) == 1
    with Session(engine) as db:
        assert db.get(NotificationJob, job_id).external_job_id == provider.jobs[0]["id"]
    assert sync(engine, provider)["results"][0]["action"] == "adopted"


def test_early_set_cancels_remote_timer_without_touching_foreign_jobs(engine):
    workout_id, old_job = rest_job(engine)
    provider = FakeRuntime()
    sync(engine, provider)
    old_external = provider.jobs[0]["id"]
    provider.jobs.append({"id": "unrelated", "name": "other-agent-reminder"})
    with Session(engine) as db, db.begin():
        workout.log_set(db, workout_id, SetInput(weight=80, reps=9), now=NOW + timedelta(seconds=30), request_id="early")
    result = sync(engine, provider)
    assert old_external in provider.removed
    assert "unrelated" not in provider.removed
    assert len(provider.created) == 2
    assert prepare(engine, old_job)["stale"]


def test_rest_output_survives_restart_and_delivery_retries_do_not_duplicate(engine):
    workout_id, job_id = rest_job(engine)
    result = prepare(engine, job_id)
    assert prepare(engine, job_id) == result
    provider = FakeRuntime()
    delivered = runtime.deliver(engine, provider, result["delivery_id"], now=NOW + timedelta(seconds=100), allow_messages=True)
    assert delivered["status"] == "SENT"
    assert "Rest complete. Bench" in provider.sent[0][1]
    assert len(provider.sent) == 1
    assert runtime.deliver(engine, provider, result["delivery_id"], now=NOW + timedelta(seconds=110), allow_messages=True)["status"] == "SENT"
    assert len(provider.sent) == 1
    with Session(engine) as db:
        assert workout.current(db, workout_id, now=NOW + timedelta(seconds=110))["status"] == "SET_ACTIVE"
        assert len(db.scalars(select(NotificationDelivery)).all()) == 1


def test_lost_send_response_fails_closed_until_operator_resolution(engine):
    _, job_id = rest_job(engine)
    result = prepare(engine, job_id)
    provider = FakeRuntime()
    provider.fail_send = True
    with pytest.raises(DomainError, match="No automatic resend"):
        runtime.deliver(engine, provider, result["delivery_id"], now=NOW + timedelta(seconds=100), allow_messages=True)
    assert runtime.deliver(engine, provider, result["delivery_id"], now=NOW + timedelta(seconds=110), allow_messages=True)["status"] == "UNKNOWN"
    assert len(provider.sent) == 1
    with Session(engine) as db, db.begin():
        runtime.resolve_delivery(db, result["delivery_id"], outcome="sent", now=NOW + timedelta(seconds=120))
    assert runtime.deliver(engine, provider, result["delivery_id"], now=NOW + timedelta(seconds=120), allow_messages=True)["status"] == "SENT"
    assert len(provider.sent) == 1


def test_crash_after_claim_never_auto_resends(engine):
    _, job_id = rest_job(engine)
    result = prepare(engine, job_id)
    with Session(engine) as db, db.begin():
        db.get(NotificationDelivery, result["delivery_id"]).status = "SENDING"
    provider = FakeRuntime()
    assert runtime.deliver(engine, provider, result["delivery_id"], now=NOW + timedelta(minutes=5), allow_messages=True)["status"] == "SENDING"
    assert not provider.sent


def test_new_set_suppresses_stale_pending_delivery(engine):
    workout_id, job_id = rest_job(engine)
    result = prepare(engine, job_id)
    with Session(engine) as db, db.begin():
        workout.log_set(db, workout_id, SetInput(weight=80, reps=9), now=NOW + timedelta(seconds=110), request_id="next")
    provider = FakeRuntime()
    assert runtime.deliver(engine, provider, result["delivery_id"], now=NOW + timedelta(seconds=120), allow_messages=True)["status"] == "CANCELLED"
    assert not provider.sent


def test_moved_plan_suppresses_old_preparation_delivery(engine):
    with Session(engine) as db, db.begin():
        plan = PlannedSession(week_id="2026-10-12", status="COMMITTED", planned_start_at=NOW + timedelta(minutes=35), planned_end_at=NOW + timedelta(minutes=105), prep_start_at=NOW, leave_home_at=NOW + timedelta(minutes=15), expected_finish_at=NOW + timedelta(minutes=105))
        db.add(plan); db.flush()
        job = NotificationJob(kind="GET_READY", planned_session_id=plan.id, due_at=NOW, payload_json={"revision": 1, "instruction": "Get ready."})
        db.add(job); db.flush()
        result = runtime.prepare_notification(db, job.id, now=NOW, recipient="123", profile="gymclaw")
        plan.source_revision += 1
    provider = FakeRuntime()
    assert runtime.deliver(engine, provider, result["delivery_id"], now=NOW, allow_messages=True)["status"] == "CANCELLED"
    assert not provider.sent


def test_approval_gates_run_before_external_calls(engine):
    provider = FakeRuntime()
    with pytest.raises(DomainError, match="requires --allow-runtime-changes"):
        runtime.sync_automations(engine, provider, now=NOW, recipient="123", project_root=Path.cwd())
    _, job_id = rest_job(engine)
    result = prepare(engine, job_id)
    with pytest.raises(DomainError, match="requires --allow-messages"):
        runtime.deliver(engine, provider, result["delivery_id"], now=NOW)
    assert not provider.sent and not provider.created


@pytest.mark.parametrize("profile,recipient", [("default", "123"), ("main", "123"), ("gymclaw", "-100123"), ("gymclaw", "@user"), ("gymclaw", "0")])
def test_single_owner_dedicated_runtime_required(profile, recipient):
    with pytest.raises(DomainError):
        validate_route(profile, recipient)


def test_provider_exact_argv_and_verified_receipt():
    calls = []
    def transport(argv):
        calls.append(argv)
        if "send" in argv:
            return {"action": "send", "channel": "telegram", "dryRun": False, "messageId": "101", "payload": {"ok": True}}
        if "list" in argv:
            return {"jobs": []}
        return {"id": "external-id"}
    provider = OpenClawProvider(transport=transport)
    spec = AutomationSpec(name="timer", argv=("/tmp/my project/.venv/bin/python", "-m", "gymclaw.cli"), cwd="/tmp/my project", at=NOW.isoformat())
    assert provider.create(spec) == "external-id"
    assert calls[0][:3] == ["openclaw", "--profile", "gymclaw"]
    assert "--declaration-key" in calls[0] and "--no-deliver" in calls[0]
    assert "--command-argv" in calls[0] and "--command" not in calls[0]
    assert provider.list_jobs() == []
    assert provider.send("123", "hello; $(not-a-shell)") == "101"
    assert "hello; $(not-a-shell)" in calls[-1]
    with pytest.raises(DomainError, match="No confirmed"):
        OpenClawProvider(transport=lambda _: {"ok": True}).send("123", "hello")


def test_adapter_hides_child_secrets_on_failure(monkeypatch):
    monkeypatch.setattr("shutil.which", lambda _: "/bin/openclaw")
    monkeypatch.setattr("subprocess.run", lambda *a, **k: subprocess.CompletedProcess(a[0], 1, "secret-token", "secret-token"))
    with pytest.raises(DomainError) as error:
        run_json(["openclaw", "--json"])
    assert "secret-token" not in str(error.value)


def test_runtime_cli_offline_plan_and_fire(engine, capsys):
    from gymclaw.cli import main
    _, job_id = rest_job(engine)
    args = ["--db-url", str(engine.url), "runtime"]
    assert main(args + ["plan", "--telegram-id", "123", "--now", NOW.isoformat()]) == 0
    import json
    value = json.loads(capsys.readouterr().out)
    assert value["data"]["messages_sent"] is False
    assert len(value["data"]["automations"]) == 2
    assert main(args + ["fire", "--telegram-id", "123", "--job-id", job_id, "--now", (NOW + timedelta(seconds=100)).isoformat()]) == 0
    value = json.loads(capsys.readouterr().out)
    assert value["data"]["status"] == "PENDING"
    assert main(args + ["sync", "--telegram-id", "123"]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "RUNTIME_NOT_APPROVED"
