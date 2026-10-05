"""Local availability intent. Date end is explicitly inclusive with --through."""
from datetime import date, datetime, time, timedelta, timezone
from zoneinfo import ZoneInfo

from gymclaw.services import availability
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import utc


def register_parser(groups):
    command = groups.add_parser("availability")
    command.add_argument("operation", choices=["add", "list", "remove"])
    command.add_argument("--kind", choices=["TRAVEL", "SICK", "UNAVAILABLE"], default="UNAVAILABLE")
    command.add_argument("--start", type=datetime.fromisoformat)
    command.add_argument("--from-date", type=date.fromisoformat)
    command.add_argument("--end", type=datetime.fromisoformat)
    command.add_argument("--through", type=date.fromisoformat)
    command.add_argument("--block-id")
    command.add_argument("--request-id")
    command.add_argument("--cancel-locked", action="store_true")
    command.add_argument("--now", type=datetime.fromisoformat)
    command.add_argument("--json", action="store_true")


def availability_command(db, args):
    now = utc(args.now or datetime.now(timezone.utc))
    if args.operation == "list":
        return {"data": {"blocks": availability.list_blocks(db, now=now)}, "events": [], "user_message_hint": None}
    if not args.request_id:
        raise ValueError("Missing required --request-id")
    if args.operation == "remove":
        if not args.block_id:
            raise ValueError("Missing required --block-id")
        return availability.retract(db, args.block_id, now=now, request_id=args.request_id)
    if args.start and args.from_date or args.end and args.through:
        raise ValueError("Use either --start/--from-date and either --end/--through")
    zone = ZoneInfo(get_profile(db).timezone)
    start = args.start or (datetime.combine(args.from_date, time.min, zone) if args.from_date else now)
    end = args.end or (datetime.combine(args.through + timedelta(days=1), time.min, zone) if args.through else None)
    if end is None:
        raise ValueError("Missing required --end or inclusive --through")
    return availability.add(db, start=start, end=end, kind=args.kind, now=now, request_id=args.request_id, cancel_locked=args.cancel_locked)
