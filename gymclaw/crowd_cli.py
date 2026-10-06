"""Crowd JSON adapter. Live reads are public; no session cookie/token needed."""
from datetime import datetime, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

from sqlalchemy.orm import Session

from gymclaw.models import CalendarSyncState
from gymclaw.providers.crowd import CrowdReading, FixtureCrowdProvider
from gymclaw.providers import crowd_sources
from gymclaw.services import crowd
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import utc


def register_parser(groups):
    command = groups.add_parser("crowd")
    command.add_argument("operation", choices=["poll", "test", "predict", "record-feedback", "get-source-health"])
    command.add_argument("--fixture", type=Path)
    command.add_argument("--during-gym-hours", action="store_true")
    command.add_argument("--now", type=datetime.fromisoformat)
    command.add_argument("--at", type=datetime.fromisoformat)
    command.add_argument("--observed-at", type=datetime.fromisoformat)
    command.add_argument("--workout-id")
    command.add_argument("--rating", type=str.upper, choices=list(crowd.RATINGS))
    command.add_argument("--waited-count", type=int, default=0)
    command.add_argument("--notes")
    command.add_argument("--request-id")
    command.add_argument("--json", action="store_true")


def needed(value, flag):
    if value is None:
        raise ValueError(f"Missing required {flag}")
    return value


def crowd_command(engine, args) -> dict:
    if args.fixture and args.operation != "poll":
        raise ValueError("Crowd fixtures apply only to poll")
    if args.operation == "test":
        # One live read of the configured source, nothing stored: for setting up a new gym.
        provider = crowd_sources.from_environment()
        reading = provider.get_reading()
        return {"data": {"provider_id": provider.provider_id, "raw_value": int(reading.raw_value), "count_stored": False},
            "events": [], "user_message_hint": None}
    if args.operation == "poll":
        if args.during_gym_hours:
            with Session(engine) as db, db.begin():
                profile = get_profile(db)
                local = utc(args.now or datetime.now(timezone.utc)).astimezone(ZoneInfo(profile.timezone)).time().replace(tzinfo=None)
                if not profile.earliest_workout_start <= local < profile.latest_workout_finish:
                    return {"data": {"available": True, "skipped": "outside_configured_gym_hours"}, "events": [], "user_message_hint": None}
        if args.fixture:
            if args.db_url is None:
                raise DomainError("FIXTURE_DB_REQUIRED", "Crowd fixtures require explicit global --db-url for isolated demo DB")
            provider = FixtureCrowdProvider(CrowdReading.model_validate_json(args.fixture.read_text()))
            with Session(engine) as db:
                state = db.get(CalendarSyncState, 1)
                if state and state.source != "fixture":
                    raise DomainError("FIXTURE_DB_REQUIRED", "Do not add synthetic observations to live Google calendar DB")
        else:
            if args.now:
                raise DomainError("LIVE_TIME_REQUIRED", "Live crowd reads use actual retrieval time; --now requires fixture")
            provider = crowd_sources.from_environment()
        with Session(engine) as db, db.begin():
            result = crowd.poll(db, provider, now=args.now)
        # Persist failure health before returning nonzero; do not fabricate a count.
        if not result["data"]["available"]:
            raise DomainError(result["data"]["error_code"], "Crowd source unavailable; last model/history preserved, failed retrieval recorded")
        return result
    now = utc(args.now or datetime.now(timezone.utc))
    with Session(engine) as db, db.begin():
        if args.operation == "record-feedback":
            db.connection().exec_driver_sql("BEGIN IMMEDIATE")
            return crowd.record_feedback(db, needed(args.workout_id, "--workout-id"), rating=needed(args.rating, "--rating"),
                now=now, observed_at=args.observed_at, waited_count=args.waited_count, notes=args.notes,
                request_id=needed(args.request_id, "--request-id"))
        data = crowd.source_health(db, now=now) if args.operation == "get-source-health" else crowd.CrowdModel(db, now=now).predict(needed(args.at, "--at"))
        return {"data": data, "events": [], "user_message_hint": None}
