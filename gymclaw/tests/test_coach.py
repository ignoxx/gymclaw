from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import CrowdFeedback, ExerciseProgression
from gymclaw.services import coach
from gymclaw.services.templates import ExerciseSpec, Template, import_template
from gymclaw.services.workout import start

NOW = datetime(2026, 10, 12, 18, tzinfo=timezone.utc)


@pytest.fixture
def db(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'coach.db'}")
    initialize(engine)
    with Session(engine) as session, session.begin():
        # Plan order puts shoulders between the two chest exercises, like the first real session.
        import_template(session, Template(id="push", name="Push", exercises=[
            ExerciseSpec(id="incline", name="Machine incline press", role="press", guide_id="incline-bench-press", working_sets=2, target_weight=0, rest_seconds=90),
            ExerciseSpec(id="shoulder", name="Shoulder press", role="press", guide_id="machine-shoulder-press", working_sets=2, target_weight=25),
            ExerciseSpec(id="fly", name="Cable fly", role="fly", guide_id="cable-fly", working_sets=2, target_weight=40)]))
        start(session, "push", now=NOW, request_id="start")
        yield session
    engine.dispose()


def at(seconds: int) -> datetime:
    return NOW + timedelta(seconds=seconds)


def buttons(card: dict) -> dict[str, str]:
    return {b["text"]: b["data"].removeprefix("gc:") for row in card["buttons"] for b in row}


def test_typed_set_reacts_then_rest_card_then_fresh_set_card(db):
    first = coach.handle_action(db, "card", now=NOW, request_id="card")["cards"][0]
    # First leg day: no weight known, so no 0 kg warm-up; the card asks for the weight.
    assert first["photo"] and Path(first["photo"]).is_file()
    assert "Set 1/2 · 8–10 × **? kg**" in first["text"] and "✍️ **Reply reps × weight:** `8x40`" in first["text"]
    assert coach.handle_text(db, "how long do I rest?", now=at(5), request_id="chat") == {"handled": False}
    result = coach.handle_text(db, "12x90kg", now=at(10), request_id="msg-1")
    assert result["react"] == "👍" and result["ack"] == "✅ 12 × 90 kg"
    rest = result["cards"][0]
    # Between sets: only a countdown and ways to shorten it, nothing to log yet.
    assert rest["kind"] == "rest" and rest["rest_until"] == at(100).isoformat()
    assert rest["text"] == "until **Machine incline press** set 2/2" and list(buttons(rest)) == ["−15s", "−30s", "⏭ Skip"]
    skipped = coach.handle_tap(db, buttons(rest)["⏭ Skip"], now=at(30), request_id="cb-skip")
    card = skipped["cards"][0]
    assert skipped["cleanup"] == "delete" and card["kind"] == "set" and card["photo"]
    assert "✅ 12 × 90 kg" in buttons(card)
    # Already on the machine: no Swap after the first set, not even from an older card.
    assert "🔄 Swap" not in buttons(card) and "⏭ Next exercise" in buttons(card)
    assert coach.handle_tap(db, buttons(first)["🔄 Swap"], now=at(31), request_id="cb-swap")["ack"] == "Already started Machine incline press: finish it or tap Next."


def test_rest_over_closes_job_once_and_returns_fresh_card(db):
    from gymclaw.models import NotificationJob
    coach.handle_text(db, "10x90", now=at(10), request_id="m1")
    assert coach.handle_rest_over(db, now=at(50))["cards"] == []  # still resting
    over = coach.handle_rest_over(db, now=at(101))
    assert over["rest_over"] and over["cleanup"] == "delete" and over["cards"][0]["kind"] == "set"
    assert db.query(NotificationJob).one().status == "FIRED"  # cron fallback finds nothing to send
    again = coach.handle_rest_over(db, now=at(102))
    assert "rest_over" not in again and again["cards"][0]["kind"] == "set"


def test_finishing_an_exercise_goes_straight_to_the_next(db):
    coach.handle_text(db, "10x90", now=at(10), request_id="m1")
    coach.handle_tap(db, "skip:" + coach.exercise_card(db, coach.active_workout(db), at(11))["buttons"][0][0]["data"].split(":")[2], now=at(12), request_id="cb")
    nxt = coach.handle_text(db, "10x90", now=at(60), request_id="m2")["cards"][0]
    assert nxt["kind"] == "set" and nxt["text"].startswith("**Shoulder press**") and nxt["rest_until"] is None


def test_swap_menu_on_card_wait_later_and_free(db):
    card = coach.exercise_card(db, coach.active_workout(db), NOW)
    swap = coach.handle_tap(db, buttons(card)["🔄 Swap"], now=at(5), request_id="cb-1")
    assert swap["keep"] and len(swap["cards"]) == 3 and all(o["photo"] and o["kind"] == "option" for o in swap["cards"])
    assert all("sub:" in data for o in swap["cards"] for data in buttons(o).values())
    menu = {b["text"]: b["data"].removeprefix("gc:") for row in swap["live_buttons"] for b in row}
    assert set(menu) == {"↩ Keep Machine incline press", "↪ Do it later"}
    assert coach.handle_tap(db, menu["↩ Keep Machine incline press"], now=at(6), request_id="cb-2")["restore"]
    later = coach.handle_tap(db, menu["↪ Do it later"], now=at(7), request_id="cb-3")
    assert later["cleanup"] == "delete"
    card = later["cards"][0]
    # Cable fly (chest) comes before the shoulder press while the chest machine is taken.
    assert card["text"].startswith("**Cable fly** · Chest") and card["photo"]
    back = coach.handle_tap(db, buttons(card)["↩ Machine incline press free?"], now=at(8), request_id="cb-4")["cards"][0]
    assert back["text"].startswith("**Machine incline press**")


def test_swap_is_remembered_in_the_template(db):
    from gymclaw.services.templates import get_template
    card = coach.exercise_card(db, coach.active_workout(db), NOW)
    option = coach.handle_tap(db, buttons(card)["🔄 Swap"], now=at(1), request_id="cb-1")["cards"][0]
    slug = next(iter(buttons(option).values())).split(":")[2]
    swapped = coach.handle_tap(db, next(iter(buttons(option).values())), now=at(2), request_id="cb-2")
    assert swapped["cleanup"] == "delete" and swapped["cards"][0]["photo"] and "Chest" in swapped["cards"][0]["text"]
    template = get_template(db, "push")
    assert template.exercises[0].id == slug and template.exercises[0].working_sets == 2
    assert template.exercises[0].substitutes == ["incline"] and template.alternatives[0].id == "incline"
    # Nothing logged yet: moving on is a skip, and the button says so.
    assert coach.handle_tap(db, buttons(card)["⏭ Skip"], now=at(3), request_id="cb-3")["ack"] == "⌛ Old button."


def test_next_end_crowd_and_baseline_weight(db):
    coach.handle_text(db, "10x90", now=at(10), request_id="m1")
    coach.handle_rest_over(db, now=at(101))
    moved = coach.handle_tap(db, buttons(coach.exercise_card(db, coach.active_workout(db), at(110)))["⏭ Next exercise"], now=at(112), request_id="cb-1")
    assert moved["cards"][0]["text"].startswith("**Shoulder press**")
    done = coach.handle_action(db, "end", now=at(600), request_id="end")["cards"][0]
    assert "🏁 **Push done** · 1 sets · 10 min" in done["text"]
    rating = coach.handle_tap(db, buttons(done)["Busy"], now=at(601), request_id="cb-2")
    assert rating["ack"] == "Saved: Busy. Thanks."
    assert db.query(CrowdFeedback).one().rating == "BUSY"
    # Unknown starting weight becomes the logged baseline for next time.
    assert db.get(ExerciseProgression, "incline").next_weight == 90


def test_idle_workout_is_closed_and_keeps_logged_sets(db):
    from gymclaw.models import WorkoutSession
    from gymclaw.services.weekly import close_stale_workouts
    coach.handle_text(db, "10x90", now=at(10), request_id="m1")
    assert close_stale_workouts(db, now=at(10) + timedelta(hours=2)) == []
    workout = coach.active_workout(db)
    assert close_stale_workouts(db, now=at(10) + timedelta(hours=3, seconds=1)) == [workout.id]
    assert db.get(WorkoutSession, workout.id).status == "PLAN_UPDATED"
    assert coach.active_workout(db) is None
    # A new workout can start again.
    start(db, "push", now=at(10) + timedelta(hours=4), request_id="again")


def test_session_preview_uses_current_plan_local_time_and_one_image(db, tmp_path, monkeypatch):
    from gymclaw.models import PlannedSession
    from gymclaw.services import preview
    monkeypatch.setattr(preview, "PREVIEW_ROOT", tmp_path)
    start_at = datetime(2026, 10, 19, 8, 30, tzinfo=timezone.utc)
    db.add(PlannedSession(week_id="2026-10-19", workout_template_id="push", status="TENTATIVE", planned_start_at=start_at,
        planned_end_at=start_at + timedelta(hours=1), prep_start_at=start_at, leave_home_at=start_at, expected_finish_at=start_at + timedelta(hours=1)))
    db.flush()
    card = preview.session_preview(db, now=NOW)
    assert card["text"].splitlines()[0] == "**Push** · Mon 10:30"  # Europe/Berlin, not UTC
    assert "1. Machine incline press · 2 sets of 8–10" in card["text"] and "3. Cable fly · 2 sets of 8–10" in card["text"]
    assert Path(card["photo"]).is_file() and card["photo"].startswith(str(tmp_path))
    assert preview.session_preview(db, now=NOW)["photo"] == card["photo"]  # cached


def test_unknown_weight_offers_a_guess_from_the_same_movement(db):
    coach.handle_text(db, "10x90", now=at(10), request_id="m1")
    coach.handle_action(db, "end", now=at(60), request_id="end")
    # Renamed in the template (same illustration/movement), so no direct history.
    import_template(db, Template(id="push2", name="Push", exercises=[
        ExerciseSpec(id="incline-v2", name="Incline press", role="press", guide_id="incline-bench-press", working_sets=2, target_weight=0)]))
    start(db, "push2", now=at(120), request_id="start-2")
    card = coach.exercise_card(db, coach.active_workout(db), at(121))
    assert "💡 Guess from Machine incline press" in card["text"] and "`8x90`" in card["text"]
    assert "✅ 8 × 90 kg?" in buttons(card)


def test_rest_can_be_cut_by_15_or_30_seconds(db):
    rest = coach.handle_text(db, "10x90", now=at(10), request_id="m1")["cards"][0]
    cut = coach.handle_tap(db, buttons(rest)["−30s"], now=at(20), request_id="cb-1")
    assert cut["refresh"] and cut["cards"][0]["rest_until"] == at(70).isoformat()
    # Cutting past zero ends the rest: the next set card comes right away.
    over = coach.handle_tap(db, buttons(rest)["−30s"], now=at(50), request_id="cb-2")
    assert over["cleanup"] == "delete" and over["cards"][0]["kind"] == "set"


def test_rest_change_applies_now_and_can_become_the_default(db):
    from gymclaw.services.profile import get_profile
    assert coach.handle_action(db, "rest", seconds=60, remember=True, now=at(1), request_id="rest")["keep"]
    rest = coach.handle_text(db, "10x90", now=at(10), request_id="m1")["cards"][0]
    assert rest["rest_until"] == at(70).isoformat()
    assert get_profile(db).default_compound_rest_seconds == get_profile(db).default_accessory_rest_seconds == 60


def test_owner_names_the_exercise_and_the_workout_follows(db):
    from gymclaw.services.errors import DomainError
    from gymclaw.services.templates import get_template
    from gymclaw.services.workout import current
    card = coach.exercise_card(db, coach.active_workout(db), NOW)
    option = coach.handle_tap(db, buttons(card)["🔄 Swap"], now=at(1), request_id="cb-1")["cards"][0]
    coach.handle_tap(db, next(iter(buttons(option).values())), now=at(2), request_id="cb-2")
    # Changed their mind: back to the original, which the swap list no longer offers.
    back = coach.handle_action(db, "switch", exercise="Machine incline press", now=at(3), request_id="sw-1")["cards"][0]
    assert back["text"].startswith("**Machine incline press**")
    assert get_template(db, "push").exercises[0].id == "incline"
    with pytest.raises(DomainError, match="dumbbell-romanian-deadlift"):
        coach.handle_action(db, "switch", exercise="dumbbell rdl", now=at(4), request_id="sw-x")
    # After a set any exercise still goes; the started one keeps its set and counts as done.
    coach.handle_text(db, "10x90", now=at(10), request_id="m1")
    now_on = coach.handle_action(db, "switch", exercise="dumbbell-romanian-deadlift", now=at(20), request_id="sw-2")["cards"][0]
    assert now_on["text"].startswith("**Dumbbell Romanian Deadlift**") and now_on["rest_until"] is None
    queue = [(q["exercise_id"], q["status"]) for q in current(db, coach.active_workout(db).id, now=at(20))["queue"]]
    assert ("incline", "COMPLETED") in queue and ("dumbbell-romanian-deadlift", "ACTIVE") in queue


def test_relabel_fixes_what_was_logged_even_after_the_workout(db):
    from gymclaw.services.templates import get_template
    coach.handle_text(db, "10x90", now=at(10), request_id="m1")
    coach.handle_action(db, "next", now=at(20), request_id="next")
    coach.handle_action(db, "end", now=at(60), request_id="end")
    assert db.get(ExerciseProgression, "incline").next_weight == 90
    fixed = coach.handle_action(db, "relabel", exercise="incline-dumbbell-press", which="Machine incline press", now=at(120), request_id="fix")
    assert fixed["ack"] == "✏️ Saved as Incline Dumbbell Press." and fixed["cards"] == []
    assert db.get(ExerciseProgression, "incline") is None
    assert db.get(ExerciseProgression, "incline-dumbbell-press").next_weight == 90
    assert get_template(db, "push").exercises[0].id == "incline-dumbbell-press"


def test_start_button_bodyweight_sets_and_no_blind_warmup(db):
    from gymclaw.models import PlannedSession
    coach.handle_action(db, "end", now=at(1), request_id="end")
    import_template(db, Template(id="legs", name="Legs", exercises=[
        ExerciseSpec(id="press", name="Leg press", role="legs", guide_id="leg-press", primary=True, working_sets=1, target_weight=0, warmup_weight=0),
        ExerciseSpec(id="crunch", name="Crunch", role="core", guide_id="crunch", working_sets=1, rep_min=12, rep_max=15, target_weight=0)]))
    planned = PlannedSession(week_id="2026-10-12", workout_template_id="legs", status="COMMITTED", planned_start_at=at(60),
        planned_end_at=at(3660), prep_start_at=at(0), leave_home_at=at(30), expected_finish_at=at(3660))
    db.add(planned)
    db.flush()
    started = coach.handle_tap(db, f"begin:{planned.id[:8]}", now=at(70), request_id="cb-begin")
    assert started["ack"] == "▶️ Started." and planned.status == "STARTED"
    # Unknown weight: straight to the working set, no "8 × 0 kg" warm-up.
    assert "**? kg**" in started["cards"][0]["text"]
    assert coach.handle_tap(db, f"begin:{planned.id[:8]}", now=at(71), request_id="cb-again")["ack"] == "Already running."
    crunch = coach.handle_text(db, "10x80", now=at(80), request_id="m1")["cards"][0]
    assert "Set 1/1 · 12–15 reps" in crunch["text"] and "✅ 12 reps" in buttons(crunch)
    assert coach.handle_tap(db, buttons(crunch)["✅ 12 reps"], now=at(90), request_id="cb-crunch")["ack"] == "✅ 12 reps"


def test_swapped_away_exercise_comes_back_first(db):
    card = coach.exercise_card(db, coach.active_workout(db), NOW)
    option = coach.handle_tap(db, buttons(card)["🔄 Swap"], now=at(1), request_id="cb-1")["cards"][0]
    swapped = coach.handle_tap(db, next(iter(buttons(option).values())), now=at(2), request_id="cb-2")["cards"][0]
    # Changed their mind on B: A is offered again, first.
    again = coach.handle_tap(db, buttons(swapped)["🔄 Swap"], now=at(3), request_id="cb-3")["cards"]
    assert "↩ Back to Machine incline press" in buttons(again[0])
    back = coach.handle_tap(db, buttons(again[0])["↩ Back to Machine incline press"], now=at(4), request_id="cb-4")["cards"][0]
    assert back["text"].startswith("**Machine incline press**")
