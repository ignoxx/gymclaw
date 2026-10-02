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


def test_typed_set_reacts_and_next_card_offers_last_set(db):
    first = coach.handle_action(db, "card", now=NOW, request_id="card")["cards"][0]
    assert first["photo"] and Path(first["photo"]).is_file()
    assert "Set 1/2 · 8–10 reps" in first["text"] and "Send weight × reps" in first["text"]
    assert coach.handle_text(db, "how long do I rest?", now=at(5), request_id="chat") == {"handled": False}
    result = coach.handle_text(db, "12x90kg", now=at(10), request_id="msg-1")
    assert result["react"] == "👍"
    card = result["cards"][0]
    assert card["photo"] is None and card["rest_until"] == at(100).isoformat()
    assert "Set 2/2 · 8–10 reps" in card["text"]
    assert "✅ 90 kg × 12" in buttons(card)


def test_occupied_offers_same_muscle_alternatives_and_stays_on_chest(db):
    swap = coach.handle_tap(db, buttons(coach.exercise_card(db, coach.active_workout(db), NOW))["🔄 Swap"], now=at(5), request_id="cb-1")
    *options, menu = swap["cards"]
    assert len(options) == 2 and all(o["photo"] for o in options)
    assert all("sub:" in data for o in options for data in buttons(o).values())
    assert set(buttons(menu)) == {"⏳ I'll wait", "↪ Do it later"}
    assert coach.handle_tap(db, buttons(menu)["⏳ I'll wait"], now=at(6), request_id="cb-2")["ack"].startswith("⏳")
    later = coach.handle_tap(db, buttons(menu)["↪ Do it later"], now=at(7), request_id="cb-3")
    card = later["cards"][0]
    # Cable fly (chest) comes before the shoulder press while the chest machine is taken.
    assert card["text"].startswith("**Cable fly** · Chest") and card["photo"]
    freed = buttons(card)["↩ Machine incline press free?"]
    back = coach.handle_tap(db, freed, now=at(8), request_id="cb-4")["cards"][0]
    assert back["text"].startswith("**Machine incline press**")


def test_catalog_swap_then_old_buttons_expire(db):
    card = coach.exercise_card(db, coach.active_workout(db), NOW)
    option = coach.handle_tap(db, buttons(card)["🔄 Swap"], now=at(1), request_id="cb-1")["cards"][0]
    swapped = coach.handle_tap(db, next(iter(buttons(option).values())), now=at(2), request_id="cb-2")
    assert swapped["ack"] == "🔄 Swapped" and swapped["cards"][0]["photo"]
    assert "Chest" in swapped["cards"][0]["text"]
    stale = coach.handle_tap(db, buttons(card)["⏭ Next exercise"], now=at(3), request_id="cb-3")
    assert stale["ack"] == "⌛ Old button."


def test_next_end_crowd_and_baseline_weight(db):
    coach.handle_text(db, "90x10", now=at(10), request_id="m1")
    moved = coach.handle_tap(db, buttons(coach.exercise_card(db, coach.active_workout(db), at(11)))["⏭ Next exercise"], now=at(12), request_id="cb-1")
    assert moved["cards"][0]["text"].startswith("**Shoulder press**")
    done = coach.handle_action(db, "end", now=at(600), request_id="end")["cards"][0]
    assert "🏁 **Push done** · 1 sets · 10 min" in done["text"]
    rating = coach.handle_tap(db, buttons(done)["Busy"], now=at(601), request_id="cb-2")
    assert rating["ack"] == "Saved: Busy. Thanks."
    assert db.query(CrowdFeedback).one().rating == "BUSY"
    # Unknown starting weight becomes the logged baseline for next time.
    assert db.get(ExerciseProgression, "incline").next_weight == 90
