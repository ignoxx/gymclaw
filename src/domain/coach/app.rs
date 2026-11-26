//! Mini App coach projection and tap/start handlers (`coach::app_*`).
use serde_json::{Map, Value, json};

use super::{acked, exercise_card, finish_card_edits, guess_set, handle_tap, handled, invalid, next_set, reference};
use crate::db::Connection;
use crate::db::models::{ExerciseStatus, SetType, WorkoutExercise};
use crate::domain::adaptation;
use crate::domain::illustrations::catalog;
use crate::domain::profile::get_profile;
use crate::domain::templates::{Load, Measure};
use crate::domain::view::{Button, Card, CardKind, CoachView};
use crate::domain::workout::{self, active_workout, exercises, format_distance, format_time, guide_id, kind, name};
use crate::error::{Error, Result};
use crate::time::Ts;

/// Event the Mini App leaves for the bot: `payload` is the `CoachView` of a step taken there, which the
/// bot renders in the chat as if the button had been tapped there.
pub const APP_VIEW: &str = "coach.app_view";

/// Coach steps the Mini App offers: the chat card's buttons, its swap options (`sub`, `later`; the app shows
/// them itself, not through the chat's two-step `swap` menu), starting today's session and doing a queued
/// exercise now.
pub fn app_action(data: &str) -> bool {
    let action = data.strip_prefix("gc:").unwrap_or(data).split(':').next().unwrap_or_default();
    [
        "log", "time", "dist", "warm", "nowarm", "skip", "cut", "next", "free", "drop", "sub", "later", "crowd",
        "begin", "now", "end",
    ]
    .contains(&action)
}

/// A set fixed in the Mini App (`set_edit`): a note in the chat, and the chat's cards redrawn from the fixed
/// set: the live card's ✅ suggests the next set from it, a finished workout's summary shows it.
pub fn app_fixed(db: &Connection, set_id: &str, now: Ts) -> Result<()> {
    let Some(set) = crate::db::models::SetLog::get(db, set_id)? else { return Ok(()) };
    let Some(row) = WorkoutExercise::get(db, &set.workout_exercise_id)? else { return Ok(()) };
    let unit = get_profile(db)?.weight_unit;
    let text = format!(
        "✏️ Fixed {}: {}",
        name(&row),
        kind(&row).in_unit(unit).set(set.weight, set.reps, set.seconds, set.meters)
    );
    let note = Card { text: text.clone(), kind: CardKind::Other, ..Card::default() };
    let view = CoachView { ack: Some(text), cards: vec![note], standalone: true, ..handled() };
    crate::domain::core::emit(db, APP_VIEW, now, serde_json::to_value(&view)?)?;
    let workout = &row.workout_session_id;
    let redraw = if active_workout(db)?.is_some_and(|w| &w.id == workout) {
        CoachView { refresh: true, cards: vec![exercise_card(db, workout, now)?], ..handled() }
    } else {
        CoachView { keep: true, edits: finish_card_edits(db, workout, now)?, ..handled() }
    };
    crate::domain::core::emit(db, APP_VIEW, now, serde_json::to_value(&redraw)?)?;
    Ok(())
}

/// The latest working set of a workout, the way the app shows and fixes it: its exercise and how its sets read.
pub fn app_last(db: &Connection, workout_id: &str) -> Result<Value> {
    let Some(set) = crate::db::models::SetLog::first(
        db,
        "WHERE set_type = 'WORKING' AND workout_exercise_id IN (SELECT id FROM workout_exercise WHERE workout_session_id = ?1) ORDER BY logged_at DESC",
        [workout_id],
    )?
    else {
        return Ok(Value::Null);
    };
    let Some(row) = WorkoutExercise::get(db, &set.workout_exercise_id)? else { return Ok(Value::Null) };
    let set_kind = kind(&row).in_unit(get_profile(db)?.weight_unit);
    Ok(json!({"id": set.id, "exercise": name(&row), "workout_exercise_id": row.id,
        "text": set_kind.set(set.weight, set.reps, set.seconds, set.meters),
        "weight": set.weight, "reps": set.reps, "seconds": set.seconds, "meters": set.meters,
        "unit": set_kind.unit(), "added": set_kind.load == Load::Added, "per_side": set_kind.per_side,
        "equipment": guide_id(&row).and_then(|g| catalog().get(g)).map(|i| i.equipment.clone())}))
}

/// Tiered swap options for an exercise not started yet: top picks (swapped-away earlier, saved
/// template alternatives, previously logged same-muscle), same primary muscle, and broader/related
/// muscles, plus "do it later" and "skip".
fn app_swap(db: &Connection, workout_id: &str, row: &WorkoutExercise, rows: &[WorkoutExercise]) -> Result<Value> {
    let r = reference(&row.id);
    let tiers = adaptation::tiered_alternatives(db, workout_id, Some(&row.id))?;
    let map_option = |o: adaptation::SwapOption| {
        json!({
            "id": o.substitute_id,
            "name": o.name,
            "source": o.source,
            "muscle": o.illustration.as_ref().map(|i| i.primary_muscle.clone()),
            "equipment": o.illustration.as_ref().map(|i| i.equipment.clone()),
            "svg_url": o.illustration.map(|i| i.svg_url),
            "data": format!("gc:sub:{r}:{}", o.substitute_id),
        })
    };
    let top: Vec<Value> = tiers.top.into_iter().map(map_option).collect();
    let same: Vec<Value> = tiers.same.into_iter().map(map_option).collect();
    let related: Vec<Value> = tiers.related.into_iter().map(map_option).collect();
    let later = row.status == ExerciseStatus::Active && rows.iter().any(|e| e.status == ExerciseStatus::Pending);
    let skip = if row.status == ExerciseStatus::Deferred { format!("gc:drop:{r}") } else { format!("gc:next:{r}") };
    Ok(json!({
        "ref": r,
        "exercise": name(row),
        "muscle": tiers.muscle,
        "equipment": tiers.equipment,
        "used": tiers.used,
        "top": top,
        "same": same,
        "related": related,
        "later": later.then(|| format!("gc:later:{r}")),
        "skip": skip,
    }))
}

/// The Mini App's live card: the chat card's buttons, the swap options the chat would send, what the set
/// logger starts from, all worded as the chat words them (`SetKind`), and "do it now" for what's queued.
pub fn app_live(db: &Connection, workout_id: &str, now: Ts) -> Result<Value> {
    let buttons: Vec<Button> = exercise_card(db, workout_id, now)
        .map(|card| card.buttons.into_iter().flatten().filter(|b| app_action(&b.data)).collect())
        .unwrap_or_default();
    let data = workout::state(db, workout_id, now)?;
    let rows = exercises(db, workout_id)?;
    // Any exercise still to do can be done now instead (a free machine, a different order).
    let queue: Map<String, Value> = rows
        .iter()
        .filter(|e| matches!(e.status, ExerciseStatus::Pending | ExerciseStatus::Deferred))
        .map(|e| (e.id.clone(), json!(format!("gc:now:{}", reference(&e.id)))))
        .collect();
    let Some(active) = data.active_exercise else {
        // Only machines that were taken are left: the chat asks whether one is free now, or offers a swap.
        let waiting = rows.iter().find(|e| e.status == ExerciseStatus::Deferred);
        let swap = waiting.map(|w| app_swap(db, workout_id, w, &rows)).transpose()?;
        return Ok(json!({"buttons": buttons, "waiting": waiting.map(name), "swap": swap, "queue": queue}));
    };
    let r = reference(&active.id);
    let row = rows.iter().find(|e| e.id == active.id).expect("active row");
    let set_kind = active.kind().in_unit(get_profile(db)?.weight_unit);
    // The chat card's suggestion, else a guess from a similar exercise, else nothing (the owner enters it).
    let (weight, reps) = if active.set_type == SetType::Warmup {
        (Some(active.target_weight), active.rep_max)
    } else if let Some((weight, reps)) = next_set(db, row, active.bodyweight)? {
        (Some(weight), reps)
    } else {
        guess_set(db, row, active.rep_min)?.map_or((None, active.rep_min), |(w, reps, _)| (Some(w), reps))
    };
    let target = match active.measure {
        Measure::Time => active.seconds.map_or("time it".into(), |s| set_kind.reps(&format_time(s))),
        Measure::Distance => active.meters.map_or("distance".into(), format_distance),
        Measure::Reps => set_kind.target(active.rep_min, active.rep_max, weight.unwrap_or(0.0)),
    };
    // Before the first set (warm-up included: that's when a taken machine shows), as in the chat.
    let swap = if active.new_exercise { app_swap(db, workout_id, row, &rows)? } else { Value::Null };
    Ok(json!({"ref": r, "buttons": buttons, "target": target, "swap": swap, "queue": queue,
        "equipment": active.illustration.as_ref().map(|i| i.equipment.clone()),
        "logger": {"measure": active.measure, "weight": weight, "reps": reps, "seconds": active.seconds, "meters": active.meters,
            "unit": set_kind.unit(), "added": set_kind.load == Load::Added, "per_side": set_kind.per_side,
            // Reps alone make the set (bodyweight): the weight stays out of the way unless there is some.
            "reps_only": active.bodyweight && weight.unwrap_or(0.0) == 0.0}}))
}

/// Start any saved workout template directly from the Mini App (no 64-byte callback limit).
/// Refuses if a workout is already running (never discards it), and links today's planned session
/// when its template matches so reminders cancel cleanly.
pub fn app_start_template(db: &Connection, template_id: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    if active_workout(db)?.is_some() {
        return Err(Error::domain("COACH_STALE", "Workout is already running."));
    }
    let planned_id = workout::todays_session(db, now)?
        .filter(|s| s.workout_template_id.as_deref() == Some(template_id))
        .filter(|s| workout::startable(db, s, now).unwrap_or(false))
        .map(|s| s.id);
    workout::start(db, Some(template_id), planned_id.as_deref(), true, now, request_id)?;
    let workout = active_workout(db)?.ok_or_else(|| Error::domain("NO_WORKOUT", "Workout did not start"))?;
    let name = workout.template_snapshot["name"].as_str().unwrap_or("Workout").to_string();
    let view = acked(format!("▶️ Started {name}"), vec![exercise_card(db, &workout.id, now)?]);
    crate::domain::core::emit(db, APP_VIEW, now, serde_json::to_value(&view)?)?;
    Ok(view)
}

/// A coach button pressed in the Mini App (a set typed there arrives as `log` with its numbers): the same
/// step as a tap in the chat, plus an `APP_VIEW` event so the chat's card moves on too. A button the
/// workout has moved past fails with `COACH_STALE` instead of acting on whatever is current.
pub fn app_tap(db: &Connection, data: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    if !app_action(data) {
        return Err(invalid("Not a coach button the app offers"));
    }
    let view = handle_tap(db, data, now, request_id)?;
    if let Some(stale) = view.ack.as_deref().filter(|ack| ack.starts_with('⌛')) {
        return Err(Error::domain("COACH_STALE", stale.trim_start_matches("⌛ ").to_string()));
    }
    crate::domain::core::emit(db, APP_VIEW, now, serde_json::to_value(&view)?)?;
    Ok(view)
}
