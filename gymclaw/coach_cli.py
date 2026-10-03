"""`coach` (Telegram cards for the gymclaw-coach plugin) and `catalog` (illustrated exercise search)."""
from datetime import datetime, timezone

from gymclaw.models import RuntimeSettings
from gymclaw.services import coach, illustrations, workout


def register_parser(groups):
    command = groups.add_parser("coach", add_help=False)
    command.add_argument("operation", choices=["text", "tap", "act", "start", "card", "rest-over", "preview"])
    command.add_argument("--text")
    command.add_argument("--data", help="Callback payload without the gc: namespace")
    command.add_argument("--action", choices=["card", "swap", "later", "next", "end"])
    command.add_argument("--template-id")
    command.add_argument("--planned-session-id")
    command.add_argument("--request-id")
    command.add_argument("--now", type=datetime.fromisoformat)
    command.add_argument("--json", action="store_true")
    search = groups.add_parser("catalog", add_help=False)
    search.add_argument("operation", choices=["search"])
    search.add_argument("--query", default="")
    search.add_argument("--muscle")
    search.add_argument("--equipment")
    search.add_argument("--limit", type=int, default=8)
    search.add_argument("--json", action="store_true")


def needed(value, flag):
    if value is None:
        raise ValueError(f"Missing required {flag}")
    return value


def catalog_command(args):
    results = illustrations.search(args.query, muscle=args.muscle, equipment=args.equipment, limit=args.limit)
    return {"data": [{k: r[k] for k in ("guide_id", "name", "equipment", "primary_muscle")} for r in results], "events": [], "user_message_hint": None}


def coach_command(db, args):
    now = args.now or datetime.now(timezone.utc)
    # Serialise taps and typed sets arriving at the same moment.
    db.connection().exec_driver_sql("BEGIN IMMEDIATE")
    if args.operation == "card":
        result = coach.handle_action(db, "card", now=now, request_id="card")
    elif args.operation == "rest-over":
        result = coach.handle_rest_over(db, now=now)
    elif args.operation == "preview":
        from gymclaw.services.preview import session_preview
        # Standalone: sent as its own message, never replaces the live workout card.
        result = {"handled": True, "standalone": True, "cards": [session_preview(db, now=now, planned_session_id=args.planned_session_id)]}
    else:
        request_id = needed(args.request_id, "--request-id")
        if args.operation == "text":
            result = coach.handle_text(db, needed(args.text, "--text"), now=now, request_id=request_id)
        elif args.operation == "tap":
            result = coach.handle_tap(db, needed(args.data, "--data"), now=now, request_id=request_id)
        elif args.operation == "act":
            result = coach.handle_action(db, needed(args.action, "--action"), now=now, request_id=request_id)
        else:
            workout.start(db, needed(args.template_id, "--template-id"), now=now, request_id=request_id, planned_session_id=args.planned_session_id)
            result = coach.handle_action(db, "card", now=now, request_id=request_id)
    settings = db.get(RuntimeSettings, 1)
    # The plugin sends cards to the single verified owner chat.
    return {"data": result | {"chat_id": settings.recipient if settings else None}, "events": [], "user_message_hint": None}
