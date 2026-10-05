"""JSON-only CLI adapter. Domain services do not depend on this module."""
import argparse
from datetime import date, datetime, timezone
import json
from pathlib import Path
import sys

from pydantic import ValidationError
from sqlalchemy.exc import SQLAlchemyError
from sqlalchemy.orm import Session

from gymclaw.availability_cli import register_parser as register_availability_parser, availability_command
from gymclaw.coach_cli import register_parser as register_coach_parser, catalog_command, coach_command
from gymclaw.db import make_engine, initialize
from gymclaw.calendar_cli import calendar_command, publish_command
from gymclaw.runtime_cli import register_parser as register_runtime_parser, runtime_command
from gymclaw.session_cli import register_parser as register_session_parser, session_command
from gymclaw.crowd_cli import register_parser as register_crowd_parser, crowd_command
from gymclaw.services.availability import affected_weeks
from gymclaw.services.calendar import get_week
from gymclaw.services.replanning import replan_weeks
from gymclaw.services.profile import get_profile, update_profile
from gymclaw.services.scheduling import PlanningFixture, schedule_week
from gymclaw.services import adaptation, audit, events, notifications, weekly, workout, onboarding
from gymclaw.services.errors import DomainError
from gymclaw.services.set_parser import SetInput, parse_set
from gymclaw.services.templates import Template, get_template, import_template, require_illustrations


class Parser(argparse.ArgumentParser):
    """Bad input becomes a JSON error that carries usage, so the agent can fix the call without exploring.
    `--help` prints plain-text help and exits 0."""
    def error(self, message):
        raise ValueError(f"{message}. {' '.join(self.format_usage().split())}")


def parser():
    root = Parser(description="GymClaw JSON domain tools")
    root.add_argument("--db-url", default=None)
    root.add_argument("--json", action="store_true", help="JSON is always enabled")
    groups = root.add_subparsers(dest="group", required=True, parser_class=Parser)
    register_runtime_parser(groups)
    register_crowd_parser(groups)
    register_availability_parser(groups)
    register_coach_parser(groups)
    register_session_parser(groups)
    db = groups.add_parser("db")
    db.add_argument("operation", choices=["init"])
    profile = groups.add_parser("profile")
    profile.add_argument("operation", choices=["get", "update"])
    profile.add_argument("--data", help="JSON object with profile fields")
    profile.add_argument("--now", type=datetime.fromisoformat)
    setup = groups.add_parser("onboarding")
    setup.add_argument("operation", choices=["status", "answer", "confirm-plan", "finish"])
    setup.add_argument("--answers", type=json.loads, help='JSON object, e.g. {"experience": "2 years"}')
    setup.add_argument("--profile", type=json.loads, help="JSON profile fields backing those answers")
    setup.add_argument("--template-id", action="append", help="Repeat in rotation order")
    setup.add_argument("--fingerprint")
    setup.add_argument("--request-id")
    planning = groups.add_parser("schedule", aliases=["planning"])
    planning.add_argument("operation", choices=["plan-week", "replan"])
    planning.add_argument("--week-start", type=date.fromisoformat, required=True)
    planning.add_argument("--now", type=datetime.fromisoformat)
    planning.add_argument("--fixture", type=Path, help="Explicit demo input JSON, not live data")
    planning.add_argument("--request-id")
    planning.add_argument("--template-id")
    calendar = groups.add_parser("calendar")
    calendar.add_argument("operation", choices=["auth", "sync", "get-week", "plan-week", "replan", "pending-writes", "publish",
        "personal-connect", "personal-sync", "personal-status", "personal-disconnect"])
    calendar.add_argument("--calendar-id")
    calendar.add_argument("--url", help="Read-only personal ICS feed (webcal:// or https://)")
    calendar.add_argument("--url-file", type=Path, help="File holding the feed link; keeps it out of argv/history")
    calendar.add_argument("--client-file", type=Path)
    calendar.add_argument("--week-start", type=date.fromisoformat)
    calendar.add_argument("--template-id")
    calendar.add_argument("--request-id")
    calendar.add_argument("--now", type=datetime.fromisoformat)
    calendar.add_argument("--fixture", type=Path)
    calendar.add_argument("--allow-writes", action="store_true")
    template = groups.add_parser("template")
    template.add_argument("operation", choices=["import", "get"])
    template.add_argument("--file", type=Path)
    template.add_argument("--template-id")
    training = groups.add_parser("workout")
    training.add_argument("operation", choices=["start", "current", "alternatives", "log-set", "rest-complete", "machine-busy", "machine-free", "substitute", "next-exercise", "skip-warmup", "skip-exercise", "finish"])
    training.add_argument("--template-id")
    training.add_argument("--planned-session-id")
    training.add_argument("--workout-id")
    training.add_argument("--exercise-id")
    training.add_argument("--substitute-id")
    training.add_argument("--job-id")
    training.add_argument("--request-id")
    training.add_argument("--now", type=datetime.fromisoformat)
    training.add_argument("--text")
    training.add_argument("--weight", type=float)
    training.add_argument("--reps", type=int)
    training.add_argument("--rir", type=float)
    training.add_argument("--set-type", choices=["WARMUP", "WORKING"], default="WORKING")
    training.add_argument("--reason")
    report = groups.add_parser("audit")
    report.add_argument("operation", choices=["workout", "week"])
    report.add_argument("--workout-id")
    report.add_argument("--week-start", type=date.fromisoformat)
    report.add_argument("--now", type=datetime.fromisoformat)
    inbox = groups.add_parser("events")
    inbox.add_argument("operation", choices=["pending", "ack"])
    inbox.add_argument("--event-id")
    inbox.add_argument("--all", action="store_true", help="Include audit events the code already handled")
    inbox.add_argument("--now", type=datetime.fromisoformat)
    jobs = groups.add_parser("notifications")
    jobs.add_argument("operation", choices=["pending", "due"])
    jobs.add_argument("--now", type=datetime.fromisoformat)
    for command in (db, profile, setup, planning, calendar, template, training, report, inbox, jobs):
        command.add_argument("--json", action="store_true")
    return root


def required(value, flag):
    if value is None:
        raise ValueError(f"Missing required {flag}")
    return value


def workout_command(db, args):
    now = args.now or datetime.now(timezone.utc)
    if args.operation == "current":
        # Without --workout-id: the running workout, or null when none is running.
        from gymclaw.services.coach import active_workout
        workout_id = args.workout_id or getattr(active_workout(db), "id", None)
        data = workout.current(db, workout_id, now=now) if workout_id else None
        return {"data": data, "events": [], "user_message_hint": None if data else "No workout running."}
    if args.operation == "alternatives":
        return {"data": adaptation.alternatives(db, required(args.workout_id, "--workout-id"), args.exercise_id), "events": [], "user_message_hint": None}
    request_id = required(args.request_id, "--request-id")
    if args.operation == "start":
        return workout.start(db, required(args.template_id, "--template-id"), now=now, request_id=request_id, planned_session_id=args.planned_session_id)
    if args.operation == "rest-complete":
        return workout.rest_complete(db, required(args.job_id, "--job-id"), now=now, request_id=request_id)
    workout_id = required(args.workout_id, "--workout-id")
    if args.operation == "log-set":
        if args.text is not None:
            if args.weight is not None or args.reps is not None or args.rir is not None:
                raise ValueError("Use --text OR structured set fields, not both")
            value = parse_set(args.text, set_type=args.set_type, expected_weight=workout.expected_weight(db, workout_id))
        else:
            value = SetInput(weight=required(args.weight, "--weight"), reps=required(args.reps, "--reps"), set_type=args.set_type, rir=args.rir)
        return workout.log_set(db, workout_id, value, now=now, request_id=request_id)
    if args.operation == "machine-busy":
        return adaptation.machine_busy(db, workout_id, exercise_id=args.exercise_id, now=now, request_id=request_id)
    if args.operation == "finish":
        return audit.finish(db, workout_id, now=now, request_id=request_id)
    if args.operation == "skip-warmup":
        return workout.skip_warmup(db, workout_id, now=now, request_id=request_id)
    exercise_id = required(args.exercise_id, "--exercise-id")
    if args.operation == "machine-free":
        return adaptation.machine_free(db, workout_id, exercise_id, now=now, request_id=request_id)
    if args.operation == "next-exercise":
        return adaptation.next_exercise(db, workout_id, exercise_id, now=now, request_id=request_id)
    if args.operation == "substitute":
        return adaptation.substitute(db, workout_id, exercise_id, required(args.substitute_id, "--substitute-id"), now=now, request_id=request_id)
    return adaptation.skip_exercise(db, workout_id, exercise_id, reason=required(args.reason, "--reason"), now=now, request_id=request_id)


def main(argv=None) -> int:
    engine = None
    try:
        args = parser().parse_args(argv)
        engine = make_engine(args.db_url)
        emitted_events = []
        message_hint = None
        if args.group == "catalog":
            result = catalog_command(args)
            data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
        elif args.group == "db":
            initialize(engine)
            data = {"initialized": True}
        elif args.group == "crowd":
            result = crowd_command(engine, args)
            data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
        elif args.group == "runtime":
            result = runtime_command(engine, args)
            data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
        elif args.group == "calendar" and args.operation == "publish":
            result = publish_command(engine, args)
            data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
        else:
            with Session(engine) as db, db.begin():
                if args.group == "coach":
                    result = coach_command(db, args)
                    data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
                elif args.group == "availability":
                    result = availability_command(db, args)
                    data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
                elif args.group == "session":
                    result = session_command(db, args)
                    data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
                elif args.group == "onboarding":
                    if args.operation == "status":
                        data = onboarding.status(db)
                        message_hint = data["instruction"]
                    else:
                        result = onboarding.update(db, args.operation, now=datetime.now(timezone.utc),
                            request_id=required(args.request_id, "--request-id"), answers=args.answers, profile=args.profile,
                            template_ids=args.template_id, expected_fingerprint=args.fingerprint)
                        data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
                elif args.group == "profile":
                    if args.operation == "update":
                        if args.data is None:
                            raise ValueError("profile update requires --data")
                        changes = json.loads(args.data)
                        if not isinstance(changes, dict):
                            raise ValueError("Profile changes must be JSON object")
                        # Sessions that no longer fit (e.g. a narrower time window) move right away.
                        now = args.now or datetime.now(timezone.utc)
                        data = update_profile(db, changes).model_dump(mode="json") | {"replanning": replan_weeks(db, affected_weeks(db, now), now=now)}
                    else:
                        data = get_profile(db).model_dump(mode="json")
                elif args.group == "calendar":
                    result = calendar_command(db, args)
                    data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
                elif args.group == "template":
                    if args.operation == "import":
                        definition = require_illustrations(Template.model_validate_json(required(args.file, "--file").read_text()))
                        data = import_template(db, definition)
                    else:
                        data = get_template(db, required(args.template_id, "--template-id")).model_dump(mode="json")
                elif args.group == "workout":
                    result = workout_command(db, args)
                    data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
                elif args.group == "audit":
                    data = audit.get_audit(db, required(args.workout_id, "--workout-id")) if args.operation == "workout" else weekly.audit_week(db, required(args.week_start, "--week-start"), now=args.now or datetime.now(timezone.utc))
                elif args.group == "events":
                    data = events.pending(db, include_all=args.all) if args.operation == "pending" else events.ack(db, required(args.event_id, "--event-id"), now=args.now or datetime.now(timezone.utc))
                elif args.group == "notifications":
                    if args.operation == "pending":
                        data = notifications.pending_jobs(db)
                    else:
                        result = notifications.dispatch_due(db, now=args.now or datetime.now(timezone.utc))
                        data, emitted_events, message_hint = result["data"], result["events"], result["user_message_hint"]
                else:
                    now = args.now or datetime.now(timezone.utc)
                    if args.operation == "replan":
                        if args.fixture:
                            raise ValueError("planning replan uses persisted calendar state; import calendar fixtures through calendar sync")
                        data = get_week(db, args.week_start) | replan_weeks(db, {args.week_start}, now=now)
                    else:
                        fixture = PlanningFixture.model_validate_json(args.fixture.read_text()) if args.fixture else PlanningFixture()
                        data = schedule_week(db, args.week_start, now=now, fixture=fixture, request_id=args.request_id, template_id=args.template_id)
        print(json.dumps({"ok": True, "data": data, "events": emitted_events, "user_message_hint": message_hint}, allow_nan=False))
        return 0
    except DomainError as error:
        print(json.dumps({"ok": False, "error": {"code": error.code, "message": str(error)}}))
        return 1
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
