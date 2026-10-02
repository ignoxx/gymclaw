"""Telegram workout cards: a typed set, a button tap or an agent action in; reaction + cards out.

Deterministic and model-free. The `gymclaw-coach` OpenClaw plugin is a thin transport that calls
`coach text|tap|act|card` and renders the result. Button data is `gc:<action>:<exercise-ref>[:args]`
(Telegram allows 64 bytes). The ref is the first 8 characters of the workout exercise ID, so old
buttons stop working once the workout has moved past that exercise.

Result: {"handled", "react", "ack", "cards": [{"text", "photo", "buttons", "rest_until"}]}
- react: emoji for the owner's typed message (text input only)
- ack: short line appended to the tapped message before its buttons are removed
- rest_until: ISO time; the plugin shows a live countdown on that card
"""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import WorkoutExercise, WorkoutSession
from gymclaw.services import adaptation, audit
from gymclaw.services.crowd import record_feedback
from gymclaw.services.errors import DomainError
from gymclaw.services.set_parser import SetInput, parse_set
from gymclaw.services.workout import UNRESOLVED, current, exercises, expected_weight, format_target, log_set, skip_warmup, utc

RATINGS = (("Empty", "EMPTY"), ("Fine", "FINE"), ("Busy", "BUSY"), ("Packed", "PACKED"))


def active_workout(db: Session) -> WorkoutSession | None:
    return db.scalar(select(WorkoutSession).where(WorkoutSession.status != "PLAN_UPDATED").order_by(WorkoutSession.started_at.desc()).limit(1))


def button(text: str, *parts: str) -> dict:
    data = "gc:" + ":".join(parts)
    if len(data.encode()) > 64:
        raise DomainError("CALLBACK_TOO_LONG", "Telegram callback data exceeds 64 bytes")
    return {"text": text, "data": data}


def kg(weight: float) -> str:
    return f"{weight:g} kg"


def card(text: str, buttons: list[list[dict]] | None = None, *, photo: str | None = None, rest_until: str | None = None) -> dict:
    return {"text": text, "photo": photo, "buttons": buttons or [], "rest_until": rest_until}


def exercise_card(db: Session, workout: WorkoutSession, now: datetime, *, photo: bool | None = None) -> dict:
    """Card for whatever the owner should do next. Photo only when an exercise starts (or forced)."""
    data = current(db, workout.id, now=now)
    rows = exercises(db, workout)
    deferred = [e for e in rows if e.status == "DEFERRED"]
    active = data["active_exercise"]
    if active is None:
        waiting = deferred[0]
        ref = waiting.id[:8]
        return card(f"Only **{waiting.config_json['name']}** left. It was taken earlier.",
            [[button("✅ It's free now", "free", ref), button("🔄 Swap", "swap", ref)], [button("⏭ Skip it", "drop", ref)]])
    ref = active["id"][:8]
    head = f"**{active['name']}**" + (f" · {active['primary_muscle']}" if active["primary_muscle"] else "")
    if active["set_type"] == "WARMUP":
        lines = [head, f"Warm-up · {format_target(active)}"]
        buttons = [[button("✅ Warm-up done", "warm", ref), button("Skip warm-up", "nowarm", ref)], [button("🔄 Swap", "swap", ref)]]
    else:
        lines = [head, f"Set {active['set_number']}/{active['working_sets']} · {format_target(active)}"]
        last = active["last_set"]
        quick = (last["weight"], last["reps"]) if last else (active["target_weight"], active["rep_min"]) if active["target_weight"] else None
        buttons = [[button(f"✅ {kg(quick[0])} × {quick[1]}", "log", ref, f"{quick[0]:g}", str(quick[1]))]] if quick else []
        if not quick:
            lines.append("Send weight × reps, e.g. 40x10.")
        buttons.append([button("🔄 Swap", "swap", ref), button("⏭ Next exercise", "next", ref)])
    if deferred:
        buttons.append([button(f"↩ {deferred[0].config_json['name']} free?", "free", deferred[0].id[:8])])
    show_photo = active["new_exercise"] if photo is None else photo
    picture = active["illustration"]["telegram_png"] if show_photo and active["illustration"] else None
    rest = data["rest_job"]["due_at"] if data["rest_job"] else None
    return card("\n".join(lines), buttons, photo=picture, rest_until=rest)


def summary_card(result: dict) -> dict:
    report = result["data"]
    lines = [f"🏁 **{report['template_name']} done** · {report['completed_sets']} sets · {report['actual_duration_seconds'] // 60} min"]
    lines += [f"↑ {e['name']}: next {kg(p['next_weight'])}" for e, p in zip(report["exercises"], report["progressions"]) if p["progressed"]]
    lines.append("How busy was the gym?")
    ref = report["workout_id"][:8]
    return card("\n".join(lines), [[button(label, "crowd", value, ref) for label, value in RATINGS]])


def after_change(db: Session, workout: WorkoutSession, now: datetime, request_id: str, *, photo: bool | None = None) -> list[dict]:
    """Next card, or finish + summary once every exercise is resolved."""
    if workout.status == "WORKOUT_COMPLETE":
        return [summary_card(audit.finish(db, workout.id, now=now, request_id=request_id + ":finish"))]
    return [exercise_card(db, workout, now, photo=photo)]


def handle_text(db: Session, text: str, *, now: datetime, request_id: str) -> dict:
    """Typed '40x10' during a workout logs a working set. Anything else goes to the agent."""
    now = utc(now)
    workout = active_workout(db)
    if workout is None or workout.status not in {"SET_ACTIVE", "RESTING"}:
        return {"handled": False}
    try:
        value = parse_set(text, expected_weight=expected_weight(db, workout.id))
    except DomainError:
        return {"handled": False}
    log_set(db, workout.id, value, now=now, request_id=request_id)
    return {"handled": True, "react": "👍", "ack": None, "cards": after_change(db, workout, now, request_id)}


def handle_tap(db: Session, payload: str, *, now: datetime, request_id: str) -> dict:
    """Button payload without the 'gc:' namespace, e.g. 'log:1a2b3c4d:40:10'."""
    now = utc(now)
    action, *args = payload.split(":")
    if action == "crowd":
        rating, ref = args
        workout = db.scalar(select(WorkoutSession).where(WorkoutSession.id.startswith(ref)))
        if workout is None:
            return {"handled": True, "ack": "⌛ Old button.", "cards": []}
        record_feedback(db, workout.id, rating=rating, now=now, request_id=request_id)
        return {"handled": True, "ack": f"Saved: {rating.title()}. Thanks.", "cards": []}
    workout = active_workout(db)
    if workout is None:
        return {"handled": True, "ack": "⌛ No workout running.", "cards": []}
    if action == "end":
        return end_workout(db, workout, now=now, request_id=request_id)
    ref = args[0] if args else ""
    row = next((e for e in exercises(db, workout) if e.id.startswith(ref)), None) if ref else None
    allowed = {"free": {"DEFERRED"}, "drop": {"ACTIVE", "DEFERRED"}, "swap": {"ACTIVE", "DEFERRED"}, "sub": {"ACTIVE", "DEFERRED"}, "wait": {"ACTIVE", "DEFERRED"}}
    if row is None or row.status not in allowed.get(action, {"ACTIVE"}):
        return {"handled": True, "ack": "⌛ Old button.", "cards": [exercise_card(db, workout, now, photo=False)]}
    name = row.config_json["name"]
    if action == "log":
        weight, reps = float(args[1]), int(args[2])
        log_set(db, workout.id, SetInput(weight=weight, reps=reps), now=now, request_id=request_id)
        return {"handled": True, "ack": f"✅ {kg(weight)} × {reps}", "cards": after_change(db, workout, now, request_id)}
    if action == "warm":
        target = current(db, workout.id, now=now)["active_exercise"]
        log_set(db, workout.id, SetInput(weight=target["target_weight"], reps=target["rep_max"], set_type="WARMUP"), now=now, request_id=request_id)
        return {"handled": True, "ack": "✅ Warm-up", "cards": after_change(db, workout, now, request_id)}
    if action == "nowarm":
        skip_warmup(db, workout.id, now=now, request_id=request_id)
        return {"handled": True, "ack": "Warm-up skipped", "cards": after_change(db, workout, now, request_id, photo=False)}
    if action == "swap":
        return {"handled": True, "ack": None, "keep": True, "cards": swap_cards(db, workout, row)}
    if action == "sub":
        adaptation.substitute(db, workout.id, row.id, args[1], now=now, request_id=request_id)
        return {"handled": True, "ack": "🔄 Swapped", "cards": after_change(db, workout, now, request_id, photo=True)}
    if action == "wait":
        return {"handled": True, "ack": f"⏳ Waiting for {name}.", "cards": []}
    if action == "later":
        adaptation.machine_busy(db, workout.id, exercise_id=row.id, now=now, request_id=request_id)
        return {"handled": True, "ack": f"↪ {name} later.", "cards": after_change(db, workout, now, request_id)}
    if action == "next":
        adaptation.next_exercise(db, workout.id, row.id, now=now, request_id=request_id)
        return {"handled": True, "ack": "⏭ Moved on.", "cards": after_change(db, workout, now, request_id)}
    if action == "free":
        adaptation.machine_free(db, workout.id, row.id, now=now, request_id=request_id)
        return {"handled": True, "ack": f"↩ Back to {name}.", "cards": after_change(db, workout, now, request_id, photo=True)}
    if action == "drop":
        adaptation.skip_exercise(db, workout.id, row.id, reason="owner_skipped", now=now, request_id=request_id)
        return {"handled": True, "ack": f"Skipped {name}.", "cards": after_change(db, workout, now, request_id)}
    raise DomainError("UNKNOWN_ACTION", f"Unknown coach action: {action}")


def swap_cards(db: Session, workout: WorkoutSession, row: WorkoutExercise) -> list[dict]:
    """One photo card per same-muscle alternative, then a menu to wait or do it later."""
    ref = row.id[:8]
    options = adaptation.alternatives(db, workout.id, row.id)["options"]
    cards = [card(f"**{o['name']}**" + (f" · {o['illustration']['equipment']}" if o["illustration"] else ""),
        [[button(f"Use {o['name']}", "sub", ref, o["substitute_id"])]], photo=o["illustration"]["telegram_png"] if o["illustration"] else None)
        for o in options]
    menu = [[button("⏳ I'll wait", "wait", ref)]]
    if row.status == "ACTIVE" and any(e.status == "PENDING" for e in exercises(db, workout)):
        menu[0].append(button("↪ Do it later", "later", ref))
    text = f"**{row.config_json['name']}** taken?" + ("" if options else " No alternatives for this muscle in the catalog.")
    return cards + [card(text, menu)]


def end_workout(db: Session, workout: WorkoutSession, *, now: datetime, request_id: str) -> dict:
    """Owner is done for today: skip whatever is left, then finish and summarise."""
    for row in exercises(db, workout):
        if row.status in UNRESOLVED:
            adaptation.skip_exercise(db, workout.id, row.id, reason="ended_early", now=now, request_id=f"{request_id}:skip:{row.id}")
    return {"handled": True, "ack": None, "cards": after_change(db, workout, now, request_id)}


def handle_action(db: Session, action: str, *, now: datetime, request_id: str) -> dict:
    """Agent-initiated action on the current exercise ('swap', 'later', 'next', 'end', 'card')."""
    now = utc(now)
    workout = active_workout(db)
    if workout is None:
        raise DomainError("NO_ACTIVE_WORKOUT", "No workout running; start one first")
    if action == "card":
        return {"handled": True, "ack": None, "cards": after_change(db, workout, now, request_id, photo=True)}
    if action == "end":
        return end_workout(db, workout, now=now, request_id=request_id)
    rows = exercises(db, workout)
    row = next((e for e in rows if e.status == "ACTIVE"), None) or next((e for e in rows if e.status == "DEFERRED"), None)
    if row is None:
        raise DomainError("NO_ACTIVE_EXERCISE", "Nothing left to act on; end the workout")
    return handle_tap(db, f"{action}:{row.id[:8]}", now=now, request_id=request_id)
