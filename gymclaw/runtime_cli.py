"""Repo-local activation adapter. Nothing installs or configures OpenClaw here."""
from dataclasses import asdict
from datetime import datetime, timezone
import os
from pathlib import Path

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import CalendarSyncState, NotificationDelivery
from gymclaw.providers.google_calendar import google_provider
from gymclaw.providers.openclaw import OpenClawProvider, validate_route
from gymclaw.services import runtime
from gymclaw.services.calendar import sync_calendar
from gymclaw.services.errors import DomainError
from gymclaw.services.workout import utc


def register_parser(groups):
    command = groups.add_parser("runtime", add_help=False)
    command.add_argument("operation", choices=["plan", "sync", "fire", "watch", "deliveries", "deliver", "resolve"])
    command.add_argument("--now", type=datetime.fromisoformat)
    command.add_argument("--telegram-id", default=os.environ.get("GYMCLAW_TELEGRAM_USER_ID"))
    command.add_argument("--openclaw-profile", default=os.environ.get("GYMCLAW_OPENCLAW_PROFILE", "gymclaw"))
    command.add_argument("--project-root", type=Path, default=Path.cwd())
    command.add_argument("--job-id")
    command.add_argument("--delivery-id")
    command.add_argument("--outcome", choices=["sent", "not-sent"])
    command.add_argument("--confirm-sender-stopped", action="store_true")
    command.add_argument("--allow-runtime-changes", action="store_true")
    command.add_argument("--allow-messages", action="store_true")
    command.add_argument("--no-watcher", action="store_true")
    command.add_argument("--json", action="store_true")


def required(value, flag):
    if value is None:
        raise ValueError(f"Missing required {flag}")
    return value


def runtime_command(engine, args) -> dict:
    now = utc(args.now or datetime.now(timezone.utc))
    provider = OpenClawProvider(args.openclaw_profile)
    data = None
    if args.operation == "deliveries":
        with Session(engine) as db:
            data = [runtime.delivery_data(row) for row in db.scalars(select(NotificationDelivery).order_by(NotificationDelivery.created_at))]
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
                data = {"automations": [asdict(spec) for spec in runtime.automation_plan(db, engine, now=now, recipient=recipient,
                    profile=provider.profile, project_root=args.project_root, include_watcher=not args.no_watcher)],
                    "runtime_changed": False, "messages_sent": False, "calendar_writes_enabled": False}
        elif args.operation == "fire":
            with Session(engine) as db, db.begin():
                db.connection().exec_driver_sql("BEGIN IMMEDIATE")
                data = runtime.prepare_notification(db, required(args.job_id, "--job-id"), now=now, recipient=recipient, profile=provider.profile)
            if args.allow_messages and data.get("delivery_id"):
                data["delivery"] = runtime.deliver(engine, provider, data["delivery_id"], now=now, allow_messages=True)
        else:
            if not args.allow_runtime_changes or not args.allow_messages:
                raise DomainError("RUNTIME_NOT_APPROVED", "Live runtime needs --allow-runtime-changes AND --allow-messages")
            if args.operation == "watch":
                # No cached-state replanning if the live read fails. No publication.
                with Session(engine) as db, db.begin():
                    state = db.get(CalendarSyncState, 1)
                    if state is None or state.source != "google":
                        raise DomainError("LIVE_CALENDAR_REQUIRED", "Watcher requires dedicated Google calendar auth; fixtures stay offline")
                    sync_calendar(db, google_provider(db, state.calendar_id), now=now)
            data = runtime.sync_automations(engine, provider, now=now, recipient=recipient, project_root=args.project_root,
                allow_runtime_changes=True, allow_messages=True, include_watcher=not args.no_watcher)
            # Recover crash between domain commit and send. Never replay SENDING/UNKNOWN.
            with Session(engine) as db:
                pending = list(db.scalars(select(NotificationDelivery.id).where(NotificationDelivery.status == "PENDING", NotificationDelivery.runtime_profile == provider.profile, NotificationDelivery.recipient == recipient)))
            data["deliveries"] = [runtime.deliver(engine, provider, delivery_id, now=now, allow_messages=True) for delivery_id in pending]
    return {"data": data, "events": [], "user_message_hint": None}
