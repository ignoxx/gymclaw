"""Calendar CLI adapter, separate from deterministic domain services."""
import os
from datetime import datetime, timezone

from sqlalchemy.orm import Session

from gymclaw.models import CalendarSyncState, PlannedSession
from gymclaw.providers.fixture_calendar import CalendarFixture, FixtureCalendarProvider
from gymclaw.providers.google_auth import DEFAULT_CLIENT, authenticate, bind_calendar
from gymclaw.providers.google_calendar import google_provider
from gymclaw.services import personal_calendar
from gymclaw.services.calendar import get_week, sync_calendar
from gymclaw.services.calendar_writes import apply_write, pending_writes, queue_session_write
from gymclaw.services.errors import DomainError
from gymclaw.services.replanning import commit_upcoming, move_session, replan_weeks, skip_session
from gymclaw.services.scheduling import schedule_week
from gymclaw.services.workout import utc


def needed(value, flag):
    if value is None:
        raise ValueError(f"Missing required {flag}")
    return value


def provider_for(db, args):
    if args.fixture:
        if args.db_url is None:
            raise DomainError("FIXTURE_DB_REQUIRED", "Calendar fixtures require explicit global --db-url pointing to an isolated demo DB")
        fixture = CalendarFixture.model_validate_json(args.fixture.read_text())
        if args.calendar_id and args.calendar_id != fixture.calendar_id:
            raise DomainError("CALENDAR_SCOPE_MISMATCH", "Fixture calendar ID differs from requested ID")
        return FixtureCalendarProvider.from_fixture(fixture)
    calendar_id = args.calendar_id or os.environ.get("GOOGLE_CALENDAR_ID")
    return google_provider(db, calendar_id)


def calendar_command(db: Session, args) -> dict:
    now = utc(args.now or datetime.now(timezone.utc))
    if args.operation == "auth":
        if args.fixture:
            raise ValueError("OAuth does not accept demo fixtures")
        state = db.get(CalendarSyncState, 1)
        calendar_id = args.calendar_id or os.environ.get("GOOGLE_CALENDAR_ID") or (state.calendar_id if state else None)
        data = authenticate(db, needed(calendar_id, "--calendar-id or GOOGLE_CALENDAR_ID"), client_path=args.client_file or DEFAULT_CLIENT)
        return {"data": data, "events": [], "user_message_hint": None}
    if args.operation.startswith("personal-"):
        # Read-only feed; replanning may queue GymClaw writes but never publishes them.
        if args.operation == "personal-connect":
            url = args.url_file.read_text() if args.url_file else args.url
            data = personal_calendar.connect(db, needed(url, "--url or --url-file"), now=now)
        elif args.operation == "personal-sync":
            data = personal_calendar.refresh(db, now=now, force=True, strict=True)
        elif args.operation == "personal-disconnect":
            data = personal_calendar.disconnect(db, now=now)
        else:
            data = {}
        events, hint = data.pop("events", []), data.pop("user_message_hint", None)
        return {"data": data | {"personal_calendar": personal_calendar.status(db)}, "events": events, "user_message_hint": hint}
    if args.operation == "get-week":
        return {"data": get_week(db, needed(args.week_start, "--week-start")), "events": [], "user_message_hint": None}
    if args.operation == "pending-writes":
        state = db.get(CalendarSyncState, 1)
        return {"data": {"calendar_id": state.calendar_id if state else None, "source": state.source if state else None, "writes": pending_writes(db)}, "events": [], "user_message_hint": None}
    if args.operation == "skip":
        return skip_session(db, needed(args.session_id, "--session-id"), now=now, request_id=needed(args.request_id, "--request-id"))
    if args.operation == "move":
        return move_session(db, needed(args.session_id, "--session-id"), now=now, request_id=needed(args.request_id, "--request-id"), to=args.to, day=args.day, after=args.after)
    provider = provider_for(db, args)
    synced = sync_calendar(db, provider, now=now)
    if args.operation == "sync":
        return synced
    week_start = needed(args.week_start, "--week-start")
    if args.operation == "replan":
        repaired = replan_weeks(db, {week_start}, now=now)
        return {"data": get_week(db, week_start) | repaired, "events": synced["events"], "user_message_hint": synced["user_message_hint"]}
    template_id = needed(args.template_id, "--template-id")
    planned = schedule_week(db, week_start, now=now, template_id=template_id, request_id=needed(args.request_id, "--request-id"))
    commit_upcoming(db, now=now)
    for data in planned["sessions"]:
        session = db.get(PlannedSession, data["id"])
        queue_session_write(db, session, now=now)
    data = get_week(db, week_start) | {"calendar_id": provider.calendar_id, "source": provider.source, "minimum_met": planned["minimum_met"], "shortfall": planned["shortfall"], "writes": pending_writes(db), "remote_events_changed": False}
    return {"data": data, "events": synced["events"], "user_message_hint": synced["user_message_hint"]}


def publish_command(engine, args) -> dict:
    if args.fixture is None and not args.allow_writes:
        raise DomainError("CALENDAR_WRITES_NOT_APPROVED", "Live writes require explicit --allow-writes; inspect pending-writes first")
    now = utc(args.now or datetime.now(timezone.utc))
    fixture_provider = None
    with Session(engine) as db, db.begin():
        provider = provider_for(db, args)
        if provider.source == "fixture":
            bind_calendar(db, provider.calendar_id, provider.source)
            fixture_provider = provider
        else:
            sync_calendar(db, provider, now=now)  # Observe manual edits before any write.
    results, emitted = [], []
    for _ in range(50):
        # Intent was committed before network access. Each remote result commits separately.
        with Session(engine) as db, db.begin():
            pending = pending_writes(db)
            if not pending:
                break
            provider = fixture_provider or provider_for(db, args)
            result = apply_write(db, provider, pending[0]["id"], now=now, allow_writes=args.allow_writes)
            results.append(result)
            if result.get("event"):
                emitted.append(result["event"])
        if result["status"] == "CONFLICT":
            break  # Don't keep changing calendar while a concurrent user edit needs sync.
    with Session(engine) as db:
        remaining = len(pending_writes(db))
        state = db.get(CalendarSyncState, 1)
        calendar_id = state.calendar_id
    return {"data": {"calendar_id": calendar_id, "source": provider.source, "results": results, "remaining_writes": remaining, "remote_events_changed": any(r["status"] == "APPLIED" for r in results)}, "events": emitted, "user_message_hint": "Calendar changed remotely. Sync required; remaining writes not attempted." if any(r["status"] == "CONFLICT" for r in results) else None}
