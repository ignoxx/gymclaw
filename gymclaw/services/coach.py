"""Telegram workout cards: a typed set, a button tap or an agent action in; reaction + cards out.

Deterministic and model-free. The `gymclaw-coach` OpenClaw plugin is a thin transport that calls
`coach text|tap|act|card` and renders the result. Button data is `gc:<action>:<exercise-ref>[:args]`
(Telegram allows 64 bytes). The ref is the first 8 characters of the workout exercise ID, so old
buttons stop working once the workout has moved past that exercise.

Result: {"handled", "react", "ack", "cleanup", "keep", "live_buttons", "restore", "refresh", "cards": [card]}
card: {"text", "photo", "buttons", "rest_until", "kind": "set"|"rest"|"option"}
- react: emoji for the owner's typed message
- ack: short line appended to the previous card before its buttons are removed
- cleanup "delete": remove the previous card and swap options instead of keeping them
- keep + live_buttons: swap menu; the current card's buttons change, options are sent below it
- restore: drop the options and give the current card its buttons back ("Keep")
- refresh: edit the live card in place with the returned card (rest countdown shortened)
- rest cards carry rest_until; the plugin counts down and replaces them with the next set card
"""
from datetime import datetime

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import PlannedSession, WorkoutExercise, WorkoutSession
from gymclaw.services import adaptation, audit
from gymclaw.services.crowd import record_feedback
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import update_profile
from gymclaw.services.set_parser import SetInput, parse_set
from gymclaw.services.templates import ExerciseSpec, remember_swap
from gymclaw.services.workout import UNRESOLVED, current, cut_rest, exercises, expected_weight, format_target, log_set, logs, pending_rest, rest_complete, set_rest, skip_rest, skip_warmup, start, utc

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


def done(reps: int, weight: float) -> str:
    """A set the way the owner says it: reps first, '10 × 40 kg' ('12 reps' for bodyweight)."""
    return f"{reps} × {kg(weight)}" if weight else f"{reps} reps"


def card(text: str, buttons: list[list[dict]] | None = None, *, photo: str | None = None, rest_until: str | None = None, kind: str = "set") -> dict:
    """kind 'rest' is a countdown the plugin deletes when the next set card arrives."""
    return {"text": text, "photo": photo, "buttons": buttons or [], "rest_until": rest_until, "kind": kind}


def exercise_card(db: Session, workout: WorkoutSession, now: datetime) -> dict:
    """Card for whatever the owner should do next. Every set card carries the illustration."""
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
    if data["rest_job"] and not active["new_exercise"]:
        # Between sets there is nothing to log yet: just the countdown and ways to shorten it.
        return card(f"until **{active['name']}** set {active['set_number']}/{active['working_sets']}",
            [[button("−15s", "cut", ref, "15"), button("−30s", "cut", ref, "30"), button("⏭ Skip", "skip", ref)]],
            rest_until=data["rest_job"]["due_at"], kind="rest")
    head = f"**{active['name']}**" + (f" · {active['primary_muscle']}" if active["primary_muscle"] else "")
    if active["set_type"] == "WARMUP":
        lines = [head, f"Warm-up · {format_target(active)}"]
        buttons = [[button("✅ Warm-up done", "warm", ref), button("Skip warm-up", "nowarm", ref)], [button("🔄 Swap", "swap", ref)]]
    else:
        last = active["last_set"]
        quick = (last["weight"], last["reps"]) if last else (active["target_weight"], active["rep_min"]) if active["target_weight"] else None
        if quick is None and active["bodyweight"]:
            # Bodyweight: reps are the whole set, no weight to ask for.
            quick = (0, active["rep_min"])
        reps = format_target(active | {"target_weight": 0}).removesuffix(" reps")
        if quick:
            lines = [head, f"Set {active['set_number']}/{active['working_sets']} · {format_target(active)}"]
            buttons = [[button(f"✅ {done(quick[1], quick[0])}", "log", ref, f"{quick[0]:g}", str(quick[1]))]]
        else:
            # Unknown weight: the gap is the headline and the reply format is impossible to miss.
            guess = guess_set(db, next(e for e in rows if e.id == active["id"]), active["rep_min"])
            example = f"{guess['reps']}x{guess['weight']:g}" if guess else f"{active['rep_min']}x40"
            lines = [head, f"Set {active['set_number']}/{active['working_sets']} · {reps} × **? kg**", f"✍️ **Reply reps × weight:** `{example}`"]
            buttons = []
            if guess:
                lines.append(f"💡 Guess from {guess['source']}")
                buttons.append([button(f"✅ {done(guess['reps'], guess['weight'])}?", "log", ref, f"{guess['weight']:g}", str(guess["reps"]))])
        # Swap only before the first set: once the owner is on the machine it isn't taken.
        # Nothing logged yet means moving on skips the exercise; say so.
        buttons.append(([button("🔄 Swap", "swap", ref)] if active["new_exercise"] else [])
            + [button("⏭ Skip" if active["new_exercise"] else "⏭ Next exercise", "next", ref)])
    if deferred:
        buttons.append([button(f"↩ {deferred[0].config_json['name']} free?", "free", deferred[0].id[:8])])
    picture = active["illustration"]["telegram_png"] if active["illustration"] else None
    return card("\n".join(lines), buttons, photo=picture)


def guess_set(db: Session, row: WorkoutExercise, reps: int) -> dict | None:
    """Starting weight for an exercise with no history: the same movement under another name first
    (renamed/swapped template entries), else your latest similar exercise (same muscle and equipment)."""
    from gymclaw.models import SetLog
    from gymclaw.services.illustrations import catalog
    guide = row.config_json.get("guide_id")
    item = catalog().get(guide) if guide else None
    if item is None:
        return None
    recent = db.execute(select(SetLog, WorkoutExercise).join(WorkoutExercise).where(SetLog.set_type == "WORKING",
        WorkoutExercise.exercise_id != row.exercise_id).order_by(SetLog.logged_at.desc()).limit(300)).all()
    for match in ("same", "similar"):
        for log, other in recent:
            other_item = catalog().get(other.config_json.get("guide_id") or "")
            if other_item is None:
                continue
            if (match == "same" and other_item["slug"] == item["slug"]) or (match == "similar" and
                    other_item["primaryMuscle"] == item["primaryMuscle"] and other_item["equipment"] == item["equipment"]):
                return {"weight": log.weight, "reps": reps, "source": other.config_json["name"]}
    return None


def summary_card(result: dict) -> dict:
    report = result["data"]
    lines = [f"🏁 **{report['template_name']} done** · {report['completed_sets']} sets · {report['actual_duration_seconds'] // 60} min"]
    lines += [f"↑ {e['name']}: next {kg(p['next_weight'])}" for e, p in zip(report["exercises"], report["progressions"]) if p["progressed"]]
    lines.append("How busy was the gym?")
    ref = report["workout_id"][:8]
    return card("\n".join(lines), [[button(label, "crowd", value, ref) for label, value in RATINGS]])


def after_change(db: Session, workout: WorkoutSession, now: datetime, request_id: str) -> list[dict]:
    """Next card, or finish + summary once every exercise is resolved."""
    if workout.status == "WORKOUT_COMPLETE":
        return [summary_card(audit.finish(db, workout.id, now=now, request_id=request_id + ":finish"))]
    return [exercise_card(db, workout, now)]


def handle_text(db: Session, text: str, *, now: datetime, request_id: str) -> dict:
    """Typed '10x40' during a workout logs a working set. Anything else goes to the agent."""
    now = utc(now)
    workout = active_workout(db)
    if workout is None or workout.status not in {"SET_ACTIVE", "RESTING"}:
        return {"handled": False}
    try:
        value = parse_set(text, expected_weight=expected_weight(db, workout.id))
    except DomainError:
        return {"handled": False}
    log_set(db, workout.id, value, now=now, request_id=request_id)
    return {"handled": True, "react": "👍", "ack": f"✅ {done(value.reps, value.weight)}", "cards": after_change(db, workout, now, request_id)}


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
    if action == "begin":
        return begin(db, args[0], now=now, request_id=request_id)
    workout = active_workout(db)
    if workout is None:
        return {"handled": True, "ack": "⌛ No workout running.", "cards": []}
    if action == "end":
        return end_workout(db, workout, now=now, request_id=request_id)
    ref = args[0] if args else ""
    row = next((e for e in exercises(db, workout) if e.id.startswith(ref)), None) if ref else None
    allowed = {"free": {"DEFERRED"}, "drop": {"ACTIVE", "DEFERRED"}, "swap": {"ACTIVE", "DEFERRED"}, "sub": {"ACTIVE", "DEFERRED"}, "wait": {"ACTIVE", "DEFERRED"}}
    if row is None or row.status not in allowed.get(action, {"ACTIVE"}):
        return {"handled": True, "ack": "⌛ Old button.", "cards": [exercise_card(db, workout, now)]}
    name = row.config_json["name"]
    if action == "log":
        weight, reps = float(args[1]), int(args[2])
        log_set(db, workout.id, SetInput(weight=weight, reps=reps), now=now, request_id=request_id)
        return {"handled": True, "ack": f"✅ {done(reps, weight)}", "cards": after_change(db, workout, now, request_id)}
    if action == "warm":
        target = current(db, workout.id, now=now)["active_exercise"]
        log_set(db, workout.id, SetInput(weight=target["target_weight"], reps=target["rep_max"], set_type="WARMUP"), now=now, request_id=request_id)
        return {"handled": True, "ack": "✅ Warm-up", "cards": after_change(db, workout, now, request_id)}
    if action == "nowarm":
        skip_warmup(db, workout.id, now=now, request_id=request_id)
        return {"handled": True, "ack": "Warm-up skipped", "cards": after_change(db, workout, now, request_id)}
    if action == "skip":
        skip_rest(db, workout.id, now=now, request_id=request_id)
        return {"handled": True, "cleanup": "delete", "cards": after_change(db, workout, now, request_id)}
    if action == "cut":
        cut_rest(db, workout.id, int(args[1]), now=now, request_id=request_id)
        if pending_rest(db, workout) is None:
            return {"handled": True, "cleanup": "delete", "cards": after_change(db, workout, now, request_id)}
        # Same countdown message, new time: no new notification on the phone.
        return {"handled": True, "refresh": True, "cards": [exercise_card(db, workout, now)]}
    if action in {"swap", "sub"} and row.status == "ACTIVE" and logs(db, row, "WORKING"):
        # A set done means the owner is on that machine: finish it there. (A machine taken mid-way is
        # deferred first, and its remaining sets can then be swapped.) Naming an exercise in chat still works.
        return {"handled": True, "ack": f"Already started {name}: finish it or tap Next.", "cards": [exercise_card(db, workout, now)]}
    if action == "swap":
        # The card's own buttons become wait/later; options are sent below it. No extra menu message.
        menu = [button(f"↩ Keep {row.config_json['name']}", "wait", ref)]
        if row.status == "ACTIVE" and any(e.status == "PENDING" for e in exercises(db, workout)):
            menu.append(button("↪ Do it later", "later", ref))
        return {"handled": True, "keep": True, "live_buttons": [menu], "cards": swap_cards(db, workout, row)}
    if action == "sub":
        result = adaptation.substitute(db, workout.id, row.id, args[1], now=now, request_id=request_id)
        replacement = db.get(WorkoutExercise, result["data"]["replacement_exercise_id"])
        spec = ExerciseSpec.model_validate({k: v for k, v in replacement.config_json.items() if k in ExerciseSpec.model_fields})
        remember_swap(db, workout.template_id, row.exercise_id, spec)
        return {"handled": True, "cleanup": "delete", "cards": after_change(db, workout, now, request_id)}
    if action == "wait":
        return {"handled": True, "restore": True, "cards": []}
    if action == "later":
        adaptation.machine_busy(db, workout.id, exercise_id=row.id, now=now, request_id=request_id)
        return {"handled": True, "cleanup": "delete", "cards": after_change(db, workout, now, request_id)}
    if action == "next":
        adaptation.next_exercise(db, workout.id, row.id, now=now, request_id=request_id)
        return {"handled": True, "ack": "⏭ Moved on.", "cards": after_change(db, workout, now, request_id)}
    if action == "free":
        adaptation.machine_free(db, workout.id, row.id, now=now, request_id=request_id)
        return {"handled": True, "ack": f"↩ Back to {name}.", "cards": after_change(db, workout, now, request_id)}
    if action == "drop":
        adaptation.skip_exercise(db, workout.id, row.id, reason="owner_skipped", now=now, request_id=request_id)
        return {"handled": True, "ack": f"Skipped {name}.", "cards": after_change(db, workout, now, request_id)}
    raise DomainError("UNKNOWN_ACTION", f"Unknown coach action: {action}")


def swap_cards(db: Session, workout: WorkoutSession, row: WorkoutExercise) -> list[dict]:
    """One photo card per same-muscle alternative, each with a single 'Use' button."""
    ref = row.id[:8]
    options = adaptation.alternatives(db, workout.id, row.id)["options"]
    if not options:
        return [card("No alternatives for this muscle in the catalog.", kind="option")]
    return [card(f"**{o['name']}**" + (f" · {o['illustration']['equipment']}" if o["illustration"] else ""),
        [[button(f"↩ Back to {o['name']}" if o["source"] == "workout" else f"Use {o['name']}", "sub", ref, o["substitute_id"])]], photo=o["illustration"]["telegram_png"] if o["illustration"] else None, kind="option")
        for o in options]


def end_workout(db: Session, workout: WorkoutSession, *, now: datetime, request_id: str) -> dict:
    """Owner is done for today: skip whatever is left, then finish and summarise."""
    for row in exercises(db, workout):
        if row.status in UNRESOLVED:
            adaptation.skip_exercise(db, workout.id, row.id, reason="ended_early", now=now, request_id=f"{request_id}:skip:{row.id}")
    return {"handled": True, "ack": None, "cards": after_change(db, workout, now, request_id)}


def handle_rest_over(db: Session, *, now: datetime) -> dict:
    """Countdown reached zero: close the rest job (so the cron fallback ping stays silent) and return a
    fresh card with buttons, which notifies the owner. Nothing to do if the rest already ended."""
    now = utc(now)
    workout = active_workout(db)
    if workout is None:
        return {"handled": True, "cards": []}
    job = pending_rest(db, workout)
    if job is None:
        # Rest already closed (skip, typed set, or cron fallback): still replace the countdown.
        return {"handled": True, "cleanup": "delete", "cards": after_change(db, workout, now, "rest-over")}
    if job.due_at > now:
        return {"handled": True, "cards": []}
    rest_complete(db, job.id, now=now, request_id=f"rest-due:{job.id}")
    return {"handled": True, "rest_over": True, "cleanup": "delete", "cards": after_change(db, workout, now, f"rest-due:{job.id}")}


def handle_action(db: Session, action: str, *, now: datetime, request_id: str, exercise: str | None = None,
                  which: str | None = None, seconds: int | None = None, remember: bool = False) -> dict:
    """Agent-initiated action: 'card', 'swap', 'later', 'next', 'end' on the current exercise;
    'switch' (exercise: do this one now), 'relabel' (exercise: what was really done, which: the logged one;
    also after the workout), 'rest' (seconds for this workout; remember: also the default from now on)."""
    now = utc(now)
    if action == "rest":
        seconds = needed(seconds, "seconds")
    if action == "rest" and remember:
        update_profile(db, {"default_compound_rest_seconds": seconds, "default_accessory_rest_seconds": seconds})
    workout = active_workout(db)
    if action == "relabel":
        workout = workout or db.scalar(select(WorkoutSession).order_by(WorkoutSession.started_at.desc()).limit(1))
        if workout is None:
            raise DomainError("NO_WORKOUT", "No workout to correct")
        result = adaptation.relabel(db, workout.id, needed(exercise, "exercise"), which=which, now=now, request_id=request_id)["data"]
        ack = f"✏️ Saved as {result.get('name', exercise)}."
        return {"handled": True, "ack": ack, "cards": after_change(db, workout, now, request_id) if workout.status != "PLAN_UPDATED" else []}
    if workout is None:
        if action == "rest" and remember:
            return {"handled": True, "ack": f"Rest is {seconds}s from now on.", "cards": []}
        raise DomainError("NO_ACTIVE_WORKOUT", "No workout running; start one first")
    if action == "card":
        return {"handled": True, "ack": None, "cards": after_change(db, workout, now, request_id)}
    if action == "end":
        return end_workout(db, workout, now=now, request_id=request_id)
    if action == "rest":
        set_rest(db, workout.id, seconds, now=now, request_id=request_id)
        # The live card stays; the next rest uses the new time.
        return {"handled": True, "ack": None, "keep": True, "cards": []}
    if action == "switch":
        adaptation.switch_to(db, workout.id, needed(exercise, "exercise"), now=now, request_id=request_id)
        return {"handled": True, "cleanup": "delete", "cards": after_change(db, workout, now, request_id)}
    rows = exercises(db, workout)
    row = next((e for e in rows if e.status == "ACTIVE"), None) or next((e for e in rows if e.status == "DEFERRED"), None)
    if row is None:
        raise DomainError("NO_ACTIVE_EXERCISE", "Nothing left to act on; end the workout")
    return handle_tap(db, f"{action}:{row.id[:8]}", now=now, request_id=request_id)


def begin(db: Session, ref: str, *, now: datetime, request_id: str) -> dict:
    """'Start workout' on the session reminder: start that planned session's workout and send its first card."""
    running = active_workout(db)
    if running is not None:
        return {"handled": True, "ack": "Already running.", "cards": [exercise_card(db, running, now)]}
    planned = db.scalar(select(PlannedSession).where(PlannedSession.id.startswith(ref)))
    if planned is None or planned.status not in {"TENTATIVE", "COMMITTED"} or not planned.workout_template_id:
        return {"handled": True, "ack": "⌛ This session can't be started anymore.", "cards": []}
    start(db, planned.workout_template_id, now=now, request_id=request_id, planned_session_id=planned.id)
    return {"handled": True, "ack": "▶️ Started.", "cards": [exercise_card(db, active_workout(db), now)]}


def needed(value, name: str):
    if value is None:
        raise DomainError("ARGUMENT_REQUIRED", f"Missing {name}")
    return value
