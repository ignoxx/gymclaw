"""Owner edits to single sessions. Times without an offset are the owner's local time."""
from datetime import date, datetime, timezone
from zoneinfo import ZoneInfo

from gymclaw.services import session_edits
from gymclaw.services.profile import get_profile


def register_parser(groups):
    command = groups.add_parser("session", help="Move, add, cancel or change one session; the owner's choice wins")
    command.add_argument("operation", choices=["move", "add", "cancel", "set-workout"])
    command.add_argument("--session-id")
    command.add_argument("--template-id")
    command.add_argument("--start", type=datetime.fromisoformat, help="Exact local start YYYY-MM-DDTHH:MM; pins the session")
    command.add_argument("--end", type=datetime.fromisoformat, help="Optional; move keeps the duration, add uses the preferred length")
    command.add_argument("--day", type=date.fromisoformat, help="move without --start: quietest valid slot on this date")
    command.add_argument("--request-id")
    command.add_argument("--now", type=datetime.fromisoformat)
    command.add_argument("--json", action="store_true")


def needed(value, flag):
    if value is None:
        raise ValueError(f"Missing required {flag}")
    return value


def session_command(db, args):
    now = args.now or datetime.now(timezone.utc)
    request_id = needed(args.request_id, "--request-id")
    zone = ZoneInfo(get_profile(db).timezone)
    start, end = (stamp.replace(tzinfo=zone) if stamp and stamp.tzinfo is None else stamp for stamp in (args.start, args.end))
    if args.operation == "move":
        return session_edits.move(db, needed(args.session_id, "--session-id"), start=start, end=end, day=args.day, now=now, request_id=request_id)
    if args.operation == "add":
        return session_edits.add(db, template_id=needed(args.template_id, "--template-id"), start=needed(start, "--start"), end=end, now=now, request_id=request_id)
    if args.operation == "cancel":
        return session_edits.cancel(db, needed(args.session_id, "--session-id"), now=now, request_id=request_id)
    return session_edits.set_template(db, needed(args.session_id, "--session-id"), template_id=needed(args.template_id, "--template-id"), now=now, request_id=request_id)
