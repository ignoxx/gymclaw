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
    assert first["photo"] and Path(first["photo"]).is_file()
    assert "Set 1/2 · 8–10 × **? kg**" in first["text"] and "✍️ **Reply reps × weight:** `8x40`" in first["text"]
    assert coach.handle_text(db, "how long do I rest?", now=at(5), request_id="chat") == {"handled": False}
    result = coach.handle_text(db, "12x90kg", now=at(10), request_id="msg-1")
    assert result["react"] == "👍" and result["ack"] == "✅ 12 × 90 kg"
    rest = result["cards"][0]
    # Between sets: only a countdown and Skip, nothing to log yet.
    assert rest["kind"] == "rest" and rest["rest_until"] == at(100).isoformat()
    assert rest["text"] == "until **Machine incline press** set 2/2" and set(buttons(rest)) == {"⏭ Skip"}
    skipped = coach.handle_tap(db, buttons(rest)["⏭ Skip"], now=at(30), request_id="cb-skip")
    card = skipped["cards"][0]
    assert skipped["cleanup"] == "delete" and card["kind"] == "set" and card["photo"]
    assert "✅ 12 × 90 kg" in buttons(card)
    # Already on the machine: no Swap after the first set.
    assert "🔄 Swap" not in buttons(card) and "⏭ Next exercise" in buttons(card)


def test_rest_over_closes_job_once_and_returns_fresh_card(db):
    from gymclaw.models import NotificationJob
    coach.handle_text(db, "90x10", now=at(10), request_id="m1")
    assert coach.handle_rest_over(db, now=at(50))["cards"] == []  # still resting
    over = coach.handle_rest_over(db, now=at(101))
    assert over["rest_over"] and over["cleanup"] == "delete" and over["cards"][0]["kind"] == "set"
    assert db.query(NotificationJob).one().status == "FIRED"  # cron fallback finds nothing to send
    again = coach.handle_rest_over(db, now=at(102))
    assert "rest_over" not in again and again["cards"][0]["kind"] == "set"


def test_finishing_an_exercise_goes_straight_to_the_next(db):
    coach.handle_text(db, "90x10", now=at(10), request_id="m1")
    coach.handle_tap(db, "skip:" + coach.exercise_card(db, coach.active_workout(db), at(11))["buttons"][0][0]["data"].split(":")[2], now=at(12), request_id="cb")
    nxt = coach.handle_text(db, "90x10", now=at(60), request_id="m2")["cards"][0]
    assert nxt["kind"] == "set" and nxt["text"].startswith("**Shoulder press**") and nxt["rest_until"] is None


def test_swap_menu_on_card_wait_later_and_free(db):
    card = coach.exercise_card(db, coach.active_workout(db), NOW)
    swap = coach.handle_tap(db, buttons(card)["🔄 Swap"], now=at(5), request_id="cb-1")
    assert swap["keep"] and len(swap["cards"]) == 2 and all(o["photo"] and o["kind"] == "option" for o in swap["cards"])
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
    coach.handle_text(db, "90x10", now=at(10), request_id="m1")
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
    coach.handle_text(db, "90x10", now=at(10), request_id="m1")
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
    coach.handle_text(db, "90x10", now=at(10), request_id="m1")
    coach.handle_action(db, "end", now=at(60), request_id="end")
    # Renamed in the template (same illustration/movement), so no direct history.
    import_template(db, Template(id="push2", name="Push", exercises=[
        ExerciseSpec(id="incline-v2", name="Incline press", role="press", guide_id="incline-bench-press", working_sets=2, target_weight=0)]))
    start(db, "push2", now=at(120), request_id="start-2")
    card = coach.exercise_card(db, coach.active_workout(db), at(121))
    assert "💡 Guess from Machine incline press" in card["text"] and "`8x90`" in card["text"]
    assert "✅ 8 × 90 kg?" in buttons(card)
