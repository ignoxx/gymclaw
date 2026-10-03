"""Repo-local autonomous callbacks. No installs or Gateway config edits."""
from dataclasses import asdict
from datetime import datetime, timezone
import os
from pathlib import Path
from types import SimpleNamespace

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.calendar_cli import provider_for, publish_command
from gymclaw.models import CalendarSyncState, NotificationDelivery, RuntimeSettings
from gymclaw.providers.google_calendar import google_provider
from gymclaw.providers.mysports import MySportsProvider
from gymclaw.providers.openclaw import OpenClawProvider, validate_route
from gymclaw.services import crowd, runtime, weekly
from gymclaw.services.calendar import sync_calendar
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import emit, utc
from zoneinfo import ZoneInfo


def register_parser(groups):
    command = groups.add_parser("runtime", add_help=False)
    command.add_argument("operation", choices=["plan", "sync", "fire", "watch", "weekly", "poll-crowd", "deliveries", "deliver", "resolve", "pause", "resume", "revoke-calendar-writes"])
    command.add_argument("--now", type=datetime.fromisoformat)
    command.add_argument("--telegram-id", default=os.environ.get("GYMCLAW_TELEGRAM_USER_ID"))
    command.add_argument("--openclaw-profile", default=os.environ.get("GYMCLAW_OPENCLAW_PROFILE", "gymclaw"))
    command.add_argument("--project-root", type=Path, default=Path.cwd())
    command.add_argument("--template-id")
    command.add_argument("--fixture", type=Path)
    command.add_argument("--calendar-id")
    command.add_argument("--job-id")
    command.add_argument("--delivery-id")
    command.add_argument("--outcome", choices=["sent", "not-sent"])
    command.add_argument("--confirm-sender-stopped", action="store_true")
    command.add_argument("--allow-runtime-changes", action="store_true")
    command.add_argument("--allow-messages", action="store_true")
    command.add_argument("--allow-calendar-writes", action="store_true")
    command.add_argument("--with-crowd-poll", action="store_true")
    command.add_argument("--no-watcher", action="store_true")
    command.add_argument("--json", action="store_true")


def required(value, flag):
    if value is None:
        raise ValueError(f"Missing required {flag}")
    return value


def approved(args):
    if not args.allow_runtime_changes or not args.allow_messages:
        raise DomainError("RUNTIME_NOT_APPROVED", "Live runtime needs --allow-runtime-changes AND --allow-messages")


def callback_settings(engine, args, recipient):
    with Session(engine) as db:
        settings = db.get(RuntimeSettings, 1)
        if settings is None:
            raise DomainError("RUNTIME_NOT_CONFIGURED", "Activate dedicated runtime through runtime sync first")
        if not settings.enabled:
            raise DomainError("RUNTIME_PAUSED", "Runtime paused; callbacks cannot resume it")
        if settings.profile != args.openclaw_profile or settings.recipient != recipient or settings.project_root != str(args.project_root.absolute()):
            raise DomainError("RUNTIME_SCOPE_MISMATCH", "Callback owner/profile/deployment does not match activation")
        return runtime.runtime_authority(db)


def publish_if_enabled(engine, args, authority, *, now):
    if not authority.get("calendar_writes_enabled"):
        return {"published": False, "reason": "calendar_write_authority_not_granted"}
    # Both runtime approvals and persisted separate write authority are required.
    approved(args)
    result = publish_command(engine, SimpleNamespace(fixture=None, allow_writes=True, now=now, calendar_id=args.calendar_id, db_url=None))
    return {"published": result["data"]["remaining_writes"] == 0 and not any(r["status"] == "CONFLICT" for r in result["data"]["results"]), **result["data"]}


def deliver_pending(engine, provider, *, now, recipient):
    with Session(engine) as db:
        pending = list(db.scalars(select(NotificationDelivery.id).where(NotificationDelivery.status == "PENDING",
            NotificationDelivery.runtime_profile == provider.profile, NotificationDelivery.recipient == recipient)))
    return [runtime.deliver(engine, provider, delivery_id, now=now, allow_messages=True) for delivery_id in pending]


def runtime_command(engine, args, *, now_override: datetime | None = None) -> dict:
    if args.now and not args.fixture and (args.allow_messages or args.allow_runtime_changes) and args.operation in {"sync", "watch", "weekly", "fire", "deliver"}:
        raise DomainError("LIVE_TIME_REQUIRED", "Activated runtime uses real time; --now is local-only/fixture preview")
    now = utc(now_override or args.now or datetime.now(timezone.utc))
    provider = OpenClawProvider(args.openclaw_profile)
    if args.fixture and (args.operation != "weekly" or args.allow_messages or args.allow_runtime_changes or args.allow_calendar_writes):
        raise DomainError("FIXTURE_SCOPE_REQUIRED", "Runtime calendar fixture is weekly local-only preview; no runtime/messages/calendar writes")
    if args.allow_calendar_writes and args.operation != "sync":
        raise DomainError("WRITE_AUTHORITY_REQUIRES_SYNC", "Grant continuing calendar-write authority only through explicit runtime sync")
    if args.operation == "deliveries":
        with Session(engine) as db:
            data = [runtime.delivery_data(row) for row in db.scalars(select(NotificationDelivery).order_by(NotificationDelivery.created_at))]
    elif args.operation in {"pause", "resume", "revoke-calendar-writes"}:
        if args.operation == "resume":
            approved(args)
        with Session(engine) as db, db.begin():
            settings = db.get(RuntimeSettings, 1)
            if settings is None:
                raise DomainError("RUNTIME_NOT_CONFIGURED", "No local runtime authority configured")
            if settings.profile != provider.profile:
                raise DomainError("RUNTIME_SCOPE_MISMATCH", "Runtime belongs to another profile")
            if args.operation == "revoke-calendar-writes":
                settings.calendar_writes_enabled = False
            else:
                settings.enabled = args.operation == "resume"
                if not settings.enabled:
                    settings.calendar_writes_enabled = False
            data = runtime.runtime_authority(db)
    elif args.operation == "resolve":
        if not args.confirm_sender_stopped:
            raise DomainError("SENDER_STOP_REQUIRED", "Verify original sender process stopped; then pass --confirm-sender-stopped")
        with Session(engine) as db, db.begin():
            data = runtime.resolve_delivery(db, required(args.delivery_id, "--delivery-id"), outcome=required(args.outcome, "--outcome"), now=now)
    elif args.operation == "deliver":
        data = runtime.deliver(engine, provider, required(args.delivery_id, "--delivery-id"), now=now, allow_messages=args.allow_messages)
    else:
        recipient = required(args.telegram_id, "--telegram-id or GYMCLAW_TELEGRAM_USER_ID")
        validate_route(provider.profile, recipient)
        if args.operation == "plan":
            with Session(engine) as db:
                authority = runtime.runtime_authority(db)
                data = {"automations": [asdict(spec) for spec in runtime.automation_plan(db, engine, now=now, recipient=recipient,
                    profile=provider.profile, project_root=args.project_root, include_watcher=not args.no_watcher)],
                    "runtime_changed": False, "messages_sent": False, **authority}
        elif args.operation == "fire":
            if args.allow_messages:
                callback_settings(engine, args, recipient)
            with Session(engine) as db, db.begin():
                db.connection().exec_driver_sql("BEGIN IMMEDIATE")
                data = runtime.prepare_notification(db, required(args.job_id, "--job-id"), now=now, recipient=recipient, profile=provider.profile)
            if args.allow_messages and data.get("delivery_id"):
                data["delivery"] = runtime.deliver(engine, provider, data["delivery_id"], now=now, allow_messages=True)
        elif args.operation == "poll-crowd":
            authority = callback_settings(engine, args, recipient)
            if not authority["crowd_polling_enabled"]:
                raise DomainError("CROWD_POLLING_NOT_APPROVED", "Periodic crowd reads not activated")
            if args.now:
                raise DomainError("LIVE_TIME_REQUIRED", "Runtime crowd polling uses actual retrieval time")
            # Around the clock: the whole day's curve matters, not just training hours.
            with Session(engine) as db, db.begin():
                result = crowd.poll(db, MySportsProvider.from_environment())
            if not result["data"]["available"]:
                raise DomainError(result["data"]["error_code"], "Crowd source unavailable; failure health recorded")
            return result
        elif args.operation == "weekly":
            live_activation = args.allow_messages or args.allow_runtime_changes
            if live_activation:
                authority = callback_settings(engine, args, recipient)
            else:
                with Session(engine) as db:
                    authority = runtime.runtime_authority(db) | {"calendar_writes_enabled": False}
            if live_activation:
                approved(args)
            template_id = args.template_id or authority.get("template_id")
            with Session(engine) as db, db.begin():
                calendar = provider_for(db, args)
                sync_calendar(db, calendar, now=now)
                data = weekly.weekly_plan(db, required(template_id, "--template-id or activated template"), now=now)
            publication = publish_if_enabled(engine, args, authority, now=now) if live_activation else {"published": False}
            with Session(engine) as db, db.begin():
                text = weekly.briefing(db, datetime.fromisoformat(data["week_start"]).date(), data["audit"], published=publication["published"])
                delivery = runtime.prepare_event_message(db, data["event_id"], message=text, now=now, recipient=recipient, profile=provider.profile) if live_activation else {"queued": False, "reason": "local_preview"}
            data |= {"publication": publication, "briefing": text, "delivery": delivery, "source": calendar.source}
            if live_activation:
                data["runtime"] = runtime.sync_automations(engine, provider, now=now, recipient=recipient, project_root=args.project_root,
                    allow_runtime_changes=True, allow_messages=True, configure=False)
                data["deliveries"] = deliver_pending(engine, provider, now=now, recipient=recipient)
        else:
            approved(args)
            if args.operation == "sync":
                with Session(engine) as db:
                    state = db.get(CalendarSyncState, 1)
                    if state and state.source != "google":
                        raise DomainError("LIVE_CALENDAR_REQUIRED", "Do not activate fixture calendar DB for live runtime")
                data = runtime.sync_automations(engine, provider, now=now, recipient=recipient, project_root=args.project_root,
                    allow_runtime_changes=True, allow_messages=True, include_watcher=not args.no_watcher,
                    template_id=args.template_id, allow_calendar_writes=args.allow_calendar_writes, crowd_polling=args.with_crowd_poll)
            else:
                authority = callback_settings(engine, args, recipient)
                with Session(engine) as db, db.begin():
                    state = db.get(CalendarSyncState, 1)
                    if state is None or state.source != "google":
                        raise DomainError("LIVE_CALENDAR_REQUIRED", "Watcher requires dedicated Google calendar auth")
                    synced = sync_calendar(db, google_provider(db, state.calendar_id), now=now)
                    rolling = weekly.rolling_plan(db, authority["template_id"], now=now) if authority.get("template_id") else {}
                    if authority["crowd_polling_enabled"]:
                        alert = crowd.polling_alert(db, now=now)
                        if alert:
                            runtime.prepare_event_message(db, alert["event_id"], message=alert["message"], now=now, recipient=recipient, profile=provider.profile)
                    hint = synced["user_message_hint"]
                    if not hint and any(rolling.get(key) for key in ("changed", "created", "missed")):
                        hint = "Training plan updated. Recovery/calendar constraints kept."
                    if hint:
                        if not authority["calendar_writes_enabled"]:
                            hint += " Local plan; calendar publication pending approval."
                        event = emit(db, "calendar.update_briefing", now, {"related_events": [e["id"] for e in synced["events"]]})
                        runtime.prepare_event_message(db, event.id, message=hint, now=now, recipient=recipient, profile=provider.profile,
                            related_events=tuple(e["id"] for e in synced["events"]))
                publication = publish_if_enabled(engine, args, authority, now=now)
                data = runtime.sync_automations(engine, provider, now=now, recipient=recipient, project_root=args.project_root,
                    allow_runtime_changes=True, allow_messages=True, include_watcher=not args.no_watcher, configure=False)
                data |= {"publication": publication, "rolling": rolling}
            data["deliveries"] = deliver_pending(engine, provider, now=now, recipient=recipient)
    return {"data": data, "events": [], "user_message_hint": None}
