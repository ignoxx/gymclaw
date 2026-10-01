"""JSON-only CLI adapter. Domain services do not depend on this module."""
import argparse
from datetime import date, datetime, timezone
import json
from pathlib import Path
import sys

from pydantic import ValidationError
from sqlalchemy.exc import SQLAlchemyError
from sqlalchemy.orm import Session

from gymclaw.db import make_engine, initialize
from gymclaw.services.profile import get_profile, update_profile
from gymclaw.services.scheduling import PlanningFixture, schedule_week


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise ValueError(message)


def parser():
    root = Parser(description="GymClaw JSON domain tools", add_help=False)
    root.add_argument("--db-url", default=None)
    root.add_argument("--json", action="store_true", help="JSON is always enabled")
    groups = root.add_subparsers(dest="group", required=True, parser_class=Parser)
    db = groups.add_parser("db", add_help=False)
    db.add_argument("operation", choices=["init"])
    profile = groups.add_parser("profile", add_help=False)
    profile.add_argument("operation", choices=["get", "update"])
    profile.add_argument("--data", help="JSON object with profile fields")
    planning = groups.add_parser("schedule", aliases=["planning"], add_help=False)
    planning.add_argument("operation", choices=["plan-week"])
    planning.add_argument("--week-start", type=date.fromisoformat, required=True)
    planning.add_argument("--now", type=datetime.fromisoformat)
    planning.add_argument("--fixture", type=Path, help="Explicit demo input JSON, not live data")
    planning.add_argument("--request-id")
    for command in (db, profile, planning):
        command.add_argument("--json", action="store_true")
    return root


def main(argv=None) -> int:
    engine = None
    try:
        args = parser().parse_args(argv)
        engine = make_engine(args.db_url)
        if args.group == "db":
            initialize(engine)
            data = {"initialized": True}
        else:
            with Session(engine) as db, db.begin():
                if args.group == "profile":
                    if args.operation == "update":
                        if args.data is None:
                            raise ValueError("profile update requires --data")
                        changes = json.loads(args.data)
                        if not isinstance(changes, dict):
                            raise ValueError("Profile changes must be JSON object")
                        data = update_profile(db, changes).model_dump(mode="json")
                    else:
                        data = get_profile(db).model_dump(mode="json")
                else:
                    fixture = PlanningFixture.model_validate_json(args.fixture.read_text()) if args.fixture else PlanningFixture()
                    now = args.now or datetime.now(timezone.utc)
                    data = schedule_week(db, args.week_start, now=now, fixture=fixture, request_id=args.request_id)
        print(json.dumps({"ok": True, "data": data, "events": [], "user_message_hint": None}, allow_nan=False))
        return 0
    except (ValueError, ValidationError, OSError) as error:
        print(json.dumps({"ok": False, "error": {"code": "INVALID_INPUT", "message": str(error)}}))
        return 1
    except SQLAlchemyError:
        print(json.dumps({"ok": False, "error": {"code": "DATABASE_ERROR", "message": "Database operation failed; run db init and retry."}}))
        return 1
    finally:
        if engine is not None:
            engine.dispose()


if __name__ == "__main__":
    sys.exit(main())
