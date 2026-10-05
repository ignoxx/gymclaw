"""Body weight CLI: log weigh-ins (one, or a backfill batch), list them, delete a misread one."""
from datetime import datetime, timezone
import json
from pathlib import Path
from zoneinfo import ZoneInfo

from pydantic import TypeAdapter

from gymclaw.services import body
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import utc


def register_parser(groups):
    command = groups.add_parser("body")
    command.add_argument("operation", choices=["log", "list", "delete"])
    command.add_argument("--kg", type=float, help="log: the weight on the scale")
    command.add_argument("--at", help="log: when, e.g. 2026-03-02 or 2026-03-02T07:10:00+01:00 (default: now)")
    command.add_argument("--photo", type=Path, help="log: scale photo sent as a file; its capture date is used")
    command.add_argument("--entries", type=json.loads, help='log: backfill batch, e.g. [{"kg":82.4,"photo":"/path/a.jpg"},{"kg":81.9,"at":"2026-03-02"}]')
    command.add_argument("--entry-id", help="delete: weigh-in to remove")
    command.add_argument("--request-id")
    command.add_argument("--now", type=datetime.fromisoformat)
    command.add_argument("--json", action="store_true")


def body_command(db, args):
    now = utc(args.now or datetime.now(timezone.utc))
    if args.operation == "list":
        zone = ZoneInfo(get_profile(db).timezone)
        rows = [body.entry_data(row, zone) for row in reversed(body.entries(db))]
        return {"data": {"entries": rows, "trend": body.trend(db, now=now)}, "events": [], "user_message_hint": None}
    if not args.request_id:
        raise ValueError("Missing required --request-id")
    if args.operation == "delete":
        if not args.entry_id:
            raise ValueError("Missing required --entry-id")
        return body.delete(db, args.entry_id, now=now, request_id=args.request_id)
    if args.entries is not None:
        if args.kg is not None or args.at or args.photo:
            raise ValueError("Use --entries OR --kg/--at/--photo, not both")
        entries = TypeAdapter(list[body.WeighIn]).validate_python(args.entries)
    else:
        if args.kg is None:
            raise ValueError("Missing required --kg (or --entries)")
        entries = [body.WeighIn(kg=args.kg, at=args.at, photo=args.photo)]
    return body.log(db, entries, now=now, request_id=args.request_id)
