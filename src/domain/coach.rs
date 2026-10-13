//! Telegram coach flows: cards, taps and typed sets (port of gymclaw/services/coach.py). Owner: WP-train.
//!
//! Deterministic and model-free: a typed set, a button tap or an agent action in; a `CoachView` out
//! (see `view.rs` for what each flag means to the renderer). Button data is
//! `gc:<action>:<exercise-ref>[:args]` (≤ 64 bytes). The ref is the first 8 characters of the workout
//! exercise ID, so old buttons stop working once the workout has moved past that exercise.
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::db::Connection;
use crate::db::models::{ExerciseStatus, PlannedSession, SetType, WorkoutExercise, WorkoutSession, WorkoutStatus};
use crate::domain::adaptation::{self, spec_of};
use crate::domain::audit;
use crate::domain::crowd::{LABELS, record_feedback};
use crate::domain::illustrations::{bodyweight, catalog};
use crate::domain::profile::update_profile;
use crate::domain::set_parser::{SetInput, parse_set};
use crate::domain::templates::remember_swap;
use crate::domain::view::{Button, Card, CardKind, Cleanup, CoachView};
use crate::domain::workout::{
    self, active_workout, cut_rest, exercises, expected_weight, format_target, g, guide_id, log_set, logs, name, pending_rest, rest_complete, set_rest, skip_rest, skip_warmup, start,
    unresolved,
};
use crate::error::{Error, Result};
use crate::time::Ts;

fn button(text: impl Into<String>, parts: &[&str]) -> Result<Button> {
    let data = format!("gc:{}", parts.join(":"));
    if data.len() > 64 {
        crate::bail!("CALLBACK_TOO_LONG", "Telegram callback data exceeds 64 bytes");
    }
    Ok(Button { text: text.into(), data })
}

fn kg(weight: f64) -> String {
    format!("{} kg", g(weight))
}

/// A set the way the owner says it: reps first, '10 × 40 kg' ('12 reps' for bodyweight).
fn done(reps: i64, weight: f64) -> String {
    if weight != 0.0 { format!("{reps} × {}", kg(weight)) } else { format!("{reps} reps") }
}

fn card(text: String, buttons: Vec<Vec<Button>>) -> Card {
    Card { text, photo: None, buttons, rest_until: None, kind: CardKind::Set }
}

fn reference(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn handled() -> CoachView {
    CoachView { handled: true, ..CoachView::default() }
}

fn acked(ack: impl Into<String>, cards: Vec<Card>) -> CoachView {
    CoachView { ack: Some(ack.into()), cards, ..handled() }
}

fn deleted(cards: Vec<Card>) -> CoachView {
    CoachView { cleanup: Some(Cleanup::Delete), cards, ..handled() }
}

/// Card for whatever the owner should do next. Every set card carries the illustration.
pub fn exercise_card(db: &Connection, workout_id: &str, now: Ts) -> Result<Card> {
    let data = workout::state(db, workout_id, now)?;
    let rows = exercises(db, workout_id)?;
    let deferred: Vec<&WorkoutExercise> = rows.iter().filter(|e| e.status == ExerciseStatus::Deferred).collect();
    let Some(active) = data.active_exercise else {
        let Some(waiting) = deferred.first() else { crate::bail!("NO_ACTIVE_EXERCISE", "Nothing left to act on; end the workout") };
        let r = reference(&waiting.id);
        return Ok(card(
            format!("Only **{}** left. It was taken earlier.", name(waiting)),
            vec![vec![button("✅ It's free now", &["free", r])?, button("🔄 Swap", &["swap", r])?], vec![button("⏭ Skip it", &["drop", r])?]],
        ));
    };
    let r = reference(&active.id);
    if let Some(rest) = data.rest_job {
        // Resting there is nothing to log yet: just the countdown and ways to shorten it.
        let upcoming = if active.new_exercise { format!("next: **{}**", active.name) } else { format!("**{}** set {}/{}", active.name, active.set_number, active.working_sets) };
        return Ok(Card {
            text: format!("until {upcoming}"),
            photo: None,
            buttons: vec![vec![button("−15s", &["cut", r, "15"])?, button("−30s", &["cut", r, "30"])?, button("⏭ Skip", &["skip", r])?]],
            rest_until: Some(rest.due_at),
            kind: CardKind::Rest,
        });
    }
    let head = format!("**{}**{}", active.name, active.primary_muscle.as_ref().map(|m| format!(" · {m}")).unwrap_or_default());
    let target = format_target(active.rep_min, active.rep_max, active.target_weight);
    let (lines, mut buttons) = if active.set_type == SetType::Warmup {
        (
            vec![head, format!("Warm-up · {target}")],
            vec![vec![button("✅ Warm-up done", &["warm", r])?, button("Skip warm-up", &["nowarm", r])?], vec![button("🔄 Swap", &["swap", r])?]],
        )
    } else {
        let quick = match (&active.last_set, active.target_weight) {
            (Some(last), _) => Some((last.weight, last.reps)),
            (None, w) if w != 0.0 => Some((w, active.rep_min)),
            // Bodyweight: reps are the whole set, no weight to ask for.
            _ if active.bodyweight => Some((0.0, active.rep_min)),
            _ => None,
        };
        let set_line = format!("Set {}/{}", active.set_number, active.working_sets);
        let (mut lines, mut buttons) = (vec![head], vec![]);
        if let Some((weight, reps)) = quick {
            lines.push(format!("{set_line} · {target}"));
            buttons.push(vec![button(format!("✅ {}", done(reps, weight)), &["log", r, &g(weight), &reps.to_string()])?]);
        } else {
            // Unknown weight: the gap is the headline and the reply format is impossible to miss.
            let row = rows.iter().find(|e| e.id == active.id).expect("active row");
            let guess = guess_set(db, row, active.rep_min)?;
            let example = guess.as_ref().map_or(format!("{}x40", active.rep_min), |(w, reps, _)| format!("{reps}x{}", g(*w)));
            let reps = format_target(active.rep_min, active.rep_max, 0.0);
            lines.push(format!("{set_line} · {} × **? kg**", reps.trim_end_matches(" reps")));
            lines.push(format!("✍️ **Reply reps × weight:** `{example}`"));
            if let Some((weight, reps, source)) = guess {
                lines.push(format!("💡 Guess from {source}"));
                buttons.push(vec![button(format!("✅ {}?", done(reps, weight)), &["log", r, &g(weight), &reps.to_string()])?]);
            }
        }
        // Swap only before the first set: once the owner is on the machine it isn't taken.
        // Nothing logged yet means moving on skips the exercise; say so.
        let mut row = if active.new_exercise { vec![button("🔄 Swap", &["swap", r])?] } else { vec![] };
        row.push(button(if active.new_exercise { "⏭ Skip" } else { "⏭ Next exercise" }, &["next", r])?);
        buttons.push(row);
        (lines, buttons)
    };
    if let Some(first) = deferred.first() {
        buttons.push(vec![button(format!("↩ {} free?", name(first)), &["free", reference(&first.id)])?]);
    }
    Ok(Card { text: lines.join("\n"), photo: active.illustration.map(|i| i.telegram_png), buttons, rest_until: None, kind: CardKind::Set })
}

/// Starting weight for an exercise with no history: the same movement under another name first
/// (renamed/swapped template entries), else your latest similar exercise (same muscle and equipment).
/// (weight, reps, source exercise name).
fn guess_set(db: &Connection, row: &WorkoutExercise, reps: i64) -> Result<Option<(f64, i64, String)>> {
    let Some(item) = guide_id(row).and_then(|g| catalog().get(g)) else { return Ok(None) };
    let mut stmt = db.prepare_cached(
        "SELECT s.weight, e.config_json FROM set_log s JOIN workout_exercise e ON e.id = s.workout_exercise_id \
         WHERE s.set_type = 'WORKING' AND e.exercise_id != ?1 ORDER BY s.logged_at DESC LIMIT 300",
    )?;
    let recent: Vec<(f64, crate::db::Json<Value>)> = stmt.query_map([&row.exercise_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for same in [true, false] {
        for (weight, config) in &recent {
            let Some(other) = config.get("guide_id").and_then(Value::as_str).and_then(|g| catalog().get(g)) else { continue };
            let hit = if same { other.slug == item.slug } else { other.primary_muscle == item.primary_muscle && other.equipment == item.equipment };
            if hit {
                return Ok(Some((*weight, reps, config.get("name").and_then(Value::as_str).unwrap_or_default().to_string())));
            }
        }
    }
    Ok(None)
}

fn summary_card(result: &Value) -> Result<Card> {
    let report = &result["data"];
    let minutes = report["actual_duration_seconds"].as_i64().unwrap_or(0).div_euclid(60);
    let mut lines = vec![format!("🏁 **{} done** · {} sets · {minutes} min", report["template_name"].as_str().unwrap_or_default(), report["completed_sets"])];
    let exercises = report["exercises"].as_array().cloned().unwrap_or_default();
    let progressions = report["progressions"].as_array().cloned().unwrap_or_default();
    for (e, p) in exercises.iter().zip(&progressions) {
        if p["progressed"].as_bool() == Some(true) {
            lines.push(format!("↑ {}: next {}", e["name"].as_str().unwrap_or_default(), kg(p["next_weight"].as_f64().unwrap_or(0.0))));
        }
    }
    lines.push("How busy was the gym?".into());
    let r = reference(report["workout_id"].as_str().unwrap_or_default()).to_string();
    let buttons = LABELS.iter().map(|(value, label)| button(*label, &["crowd", value, &r])).collect::<Result<Vec<_>>>()?;
    Ok(card(lines.join("\n"), vec![buttons]))
}

/// Next card, or finish + summary once every exercise is resolved.
fn after_change(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Vec<Card>> {
    let workout = workout::load_workout(db, workout_id, None)?;
    if workout.status == WorkoutStatus::WorkoutComplete {
        return Ok(vec![summary_card(&audit::finish(db, workout_id, now, &format!("{request_id}:finish"))?)?]);
    }
    Ok(vec![exercise_card(db, workout_id, now)?])
}

/// Typed text during a workout ("10x40"). `handled: false` when it isn't a set; the agent gets it.
pub fn handle_text(db: &Connection, text: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    let Some(workout) = active_workout(db)?.filter(|w| matches!(w.status, WorkoutStatus::SetActive | WorkoutStatus::Resting)) else {
        return Ok(CoachView::default());
    };
    let active = exercises(db, &workout.id)?.into_iter().find(|e| e.status == ExerciseStatus::Active);
    let is_bodyweight = active.as_ref().is_some_and(|a| bodyweight(guide_id(a)));
    let parsed = expected_weight(db, &workout.id).and_then(|expected| parse_set(text, SetType::Working, expected, is_bodyweight));
    let value = match parsed {
        Ok(value) => value,
        Err(Error::Domain { .. }) => return Ok(CoachView::default()),
        Err(other) => return Err(other),
    };
    log_set(db, &workout.id, &value, now, request_id)?;
    Ok(CoachView { react: Some("👍".into()), ..acked(format!("✅ {}", done(value.reps, value.weight)), after_change(db, &workout.id, now, request_id)?) })
}

fn invalid(message: &str) -> Error {
    Error::domain("INVALID_INPUT", message.to_string())
}

/// A `gc:<action>:<ref>` button tap (the `gc:` prefix is optional).
pub fn handle_tap(db: &Connection, payload: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    let payload = payload.strip_prefix("gc:").unwrap_or(payload);
    let mut parts = payload.split(':');
    let action = parts.next().unwrap_or_default();
    let args: Vec<&str> = parts.collect();
    if action == "crowd" {
        let [rating, r] = args[..] else { return Err(invalid("crowd needs rating and workout ref")) };
        let Some(workout) = WorkoutSession::first(db, "WHERE substr(id, 1, length(?1)) = ?1", [r])? else {
            return Ok(acked("⌛ Old button.", vec![]));
        };
        record_feedback(db, &workout.id, rating, now, request_id)?;
        let label = LABELS.iter().find(|(value, _)| *value == rating).map_or(rating, |(_, label)| label);
        return Ok(acked(format!("Saved: {label}. Thanks."), vec![]));
    }
    if action == "begin" {
        return begin(db, args.first().copied().unwrap_or_default(), now, request_id);
    }
    let Some(workout) = active_workout(db)? else { return Ok(acked("⌛ No workout running.", vec![])) };
    if action == "end" {
        return end_workout(db, &workout, now, request_id);
    }
    let r = args.first().copied().unwrap_or_default();
    let row = if r.is_empty() { None } else { exercises(db, &workout.id)?.into_iter().find(|e| e.id.starts_with(r)) };
    let allowed: &[ExerciseStatus] = match action {
        "free" => &[ExerciseStatus::Deferred],
        "drop" | "swap" | "sub" | "wait" => &[ExerciseStatus::Active, ExerciseStatus::Deferred],
        _ => &[ExerciseStatus::Active],
    };
    let Some(row) = row.filter(|row| allowed.contains(&row.status)) else {
        return Ok(acked("⌛ Old button.", vec![exercise_card(db, &workout.id, now)?]));
    };
    let id = workout.id.as_str();
    let arg = |i: usize| args.get(i).copied().ok_or_else(|| invalid("Button is missing an argument"));
    let next = |ack: Option<String>| -> Result<CoachView> { Ok(CoachView { ack, cards: after_change(db, id, now, request_id)?, ..handled() }) };
    match action {
        "log" => {
            let weight: f64 = arg(1)?.parse().map_err(|_| invalid("Bad weight"))?;
            let reps: i64 = arg(2)?.parse().map_err(|_| invalid("Bad reps"))?;
            log_set(db, id, &SetInput { weight, reps, set_type: SetType::Working, rir: None }, now, request_id)?;
            next(Some(format!("✅ {}", done(reps, weight))))
        }
        "warm" => {
            let target = workout::state(db, id, now)?.active_exercise.ok_or_else(|| Error::domain("NO_ACTIVE_EXERCISE", "No active exercise"))?;
            log_set(db, id, &SetInput { weight: target.target_weight, reps: target.rep_max, set_type: SetType::Warmup, rir: None }, now, request_id)?;
            next(Some("✅ Warm-up".into()))
        }
        "nowarm" => {
            skip_warmup(db, id, now, request_id)?;
            next(Some("Warm-up skipped".into()))
        }
        "skip" => {
            skip_rest(db, id, now, request_id)?;
            Ok(deleted(after_change(db, id, now, request_id)?))
        }
        "cut" => {
            let seconds: i64 = arg(1)?.parse().map_err(|_| invalid("Bad seconds"))?;
            cut_rest(db, id, seconds, now, request_id)?;
            if pending_rest(db, id)?.is_none() {
                return Ok(deleted(after_change(db, id, now, request_id)?));
            }
            // Same countdown message, new time: no new notification on the phone.
            Ok(CoachView { refresh: true, cards: vec![exercise_card(db, id, now)?], ..handled() })
        }
        "swap" | "sub" if row.status == ExerciseStatus::Active && !logs(db, &row.id, Some(SetType::Working))?.is_empty() => {
            // A set done means the owner is on that machine: finish it there. (A machine taken mid-way is
            // deferred first, and its remaining sets can then be swapped.) Naming an exercise in chat still works.
            Ok(acked(format!("Already started {}: finish it or tap Next.", name(&row)), vec![exercise_card(db, id, now)?]))
        }
        "swap" => {
            // The card's own buttons become wait/later; options are sent below it. No extra menu message.
            let mut menu = vec![button(format!("↩ Keep {}", name(&row)), &["wait", r])?];
            if row.status == ExerciseStatus::Active && exercises(db, id)?.iter().any(|e| e.status == ExerciseStatus::Pending) {
                menu.push(button("↪ Do it later", &["later", r])?);
            }
            Ok(CoachView { keep: true, live_buttons: Some(vec![menu]), cards: swap_cards(db, id, &row)?, ..handled() })
        }
        "sub" => {
            let result = adaptation::substitute(db, id, &row.id, arg(1)?, now, request_id)?;
            let replacement_id = result["data"]["replacement_exercise_id"].as_str().unwrap_or_default();
            if let (Some(replacement), Some(template_id)) = (WorkoutExercise::get(db, replacement_id)?, &workout.template_id) {
                remember_swap(db, template_id, &row.exercise_id, &spec_of(&replacement)?)?;
            }
            Ok(deleted(after_change(db, id, now, request_id)?))
        }
        "wait" => Ok(CoachView { restore: true, ..handled() }),
        "later" => {
            adaptation::machine_busy(db, id, Some(&row.id), now, request_id)?;
            Ok(deleted(after_change(db, id, now, request_id)?))
        }
        "next" => {
            adaptation::next_exercise(db, id, &row.id, now, request_id)?;
            next(Some("⏭ Moved on.".into()))
        }
        "free" => {
            adaptation::machine_free(db, id, &row.id, now, request_id)?;
            next(Some(format!("↩ Back to {}.", name(&row))))
        }
        "drop" => {
            adaptation::skip_exercise(db, id, &row.id, "owner_skipped", now, request_id)?;
            next(Some(format!("Skipped {}.", name(&row))))
        }
        other => crate::bail!("UNKNOWN_ACTION", "Unknown coach action: {other}"),
    }
}

/// One photo card per same-muscle alternative, each with a single 'Use' button.
fn swap_cards(db: &Connection, workout_id: &str, row: &WorkoutExercise) -> Result<Vec<Card>> {
    let r = reference(&row.id);
    let options = adaptation::alternatives(db, workout_id, Some(&row.id), 3)?.options;
    if options.is_empty() {
        return Ok(vec![Card { kind: CardKind::Option, ..card("No alternatives for this muscle in the catalog.".into(), vec![]) }]);
    }
    options
        .into_iter()
        .map(|o| {
            let label = if o.source == "workout" { format!("↩ Back to {}", o.name) } else { format!("Use {}", o.name) };
            Ok(Card {
                text: format!("**{}**{}", o.name, o.illustration.as_ref().map(|i| format!(" · {}", i.equipment)).unwrap_or_default()),
                photo: o.illustration.map(|i| i.telegram_png),
                buttons: vec![vec![button(label, &["sub", r, &o.substitute_id])?]],
                rest_until: None,
                kind: CardKind::Option,
            })
        })
        .collect()
}

/// End early: skip what's left, finish, summary card.
pub fn end_workout(db: &Connection, workout: &WorkoutSession, now: Ts, request_id: &str) -> Result<CoachView> {
    for row in exercises(db, &workout.id)? {
        if unresolved(row.status) {
            adaptation::skip_exercise(db, &workout.id, &row.id, "ended_early", now, &format!("{request_id}:skip:{}", row.id))?;
        }
    }
    Ok(CoachView { cards: after_change(db, &workout.id, now, request_id)?, ..handled() })
}

/// The live countdown reached rest_until: close the rest and show the next set card.
pub fn handle_rest_over(db: &Connection, now: Ts) -> Result<CoachView> {
    let Some(workout) = active_workout(db)? else { return Ok(handled()) };
    let Some(job) = pending_rest(db, &workout.id)? else {
        // Rest already closed (skip, typed set, or the fallback ping): still replace the countdown.
        return Ok(deleted(after_change(db, &workout.id, now, "rest-over")?));
    };
    if job.due_at > now {
        return Ok(handled());
    }
    let request_id = format!("rest-due:{}", job.id);
    rest_complete(db, &job.id, now, &request_id)?;
    Ok(CoachView { rest_over: true, ..deleted(after_change(db, &workout.id, now, &request_id)?) })
}

/// Agent-initiated coach actions (the old `gymclaw_workout` tool's coach verbs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Resend the current card.
    Card,
    /// Show up to 3 same-muscle alternatives as buttons (only before the first set).
    Swap,
    /// Do the current exercise later.
    Later,
    /// Move on to the next exercise.
    Next,
    /// End early, save, show the summary.
    End,
    /// The owner is doing `exercise` now.
    Switch,
    /// `exercise` is what the owner really did (`which`: the logged one).
    Relabel,
    /// Seconds between sets for this workout (`remember`: also the default).
    Rest,
}

fn needed<T>(value: Option<T>, name: &str) -> Result<T> {
    value.ok_or_else(|| Error::domain("ARGUMENT_REQUIRED", format!("Missing {name}")))
}

/// Agent-initiated action: 'card', 'swap', 'later', 'next', 'end' on the current exercise;
/// 'switch' (exercise: do this one now), 'relabel' (exercise: what was really done, which: the logged one;
/// also after the workout), 'rest' (seconds for this workout; remember: also the default from now on).
#[allow(clippy::too_many_arguments)]
pub fn handle_action(db: &Connection, action: Action, exercise: Option<&str>, which: Option<&str>, seconds: Option<i64>, remember: bool, now: Ts, request_id: &str) -> Result<CoachView> {
    let seconds = if action == Action::Rest { Some(needed(seconds, "seconds")?) } else { seconds };
    if action == Action::Rest && remember {
        let changes: Map<String, Value> = [("default_compound_rest_seconds", seconds), ("default_accessory_rest_seconds", seconds)].into_iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        update_profile(db, &changes)?;
    }
    let running = active_workout(db)?;
    if action == Action::Relabel {
        let workout = match running {
            Some(w) => w,
            None => match WorkoutSession::first(db, "ORDER BY started_at DESC", [])? {
                Some(w) => w,
                None => crate::bail!("NO_WORKOUT", "No workout to correct"),
            },
        };
        let exercise = needed(exercise, "exercise")?;
        let result = adaptation::relabel(db, &workout.id, exercise, which, now, request_id)?;
        let ack = format!("✏️ Saved as {}.", result["data"]["name"].as_str().unwrap_or(exercise));
        let cards = if workout.status != WorkoutStatus::PlanUpdated { after_change(db, &workout.id, now, request_id)? } else { vec![] };
        return Ok(acked(ack, cards));
    }
    let Some(workout) = running else {
        if action == Action::Rest && remember {
            return Ok(acked(format!("Rest is {}s from now on.", seconds.unwrap_or_default()), vec![]));
        }
        crate::bail!("NO_ACTIVE_WORKOUT", "No workout running; start one first");
    };
    let tap = match action {
        Action::Card => return Ok(CoachView { cards: after_change(db, &workout.id, now, request_id)?, ..handled() }),
        Action::End => return end_workout(db, &workout, now, request_id),
        Action::Rest => {
            set_rest(db, &workout.id, seconds.unwrap_or_default(), now, request_id)?;
            // The live card stays; the next rest uses the new time.
            return Ok(CoachView { keep: true, ..handled() });
        }
        Action::Switch => {
            adaptation::switch_to(db, &workout.id, needed(exercise, "exercise")?, true, now, request_id)?;
            return Ok(deleted(after_change(db, &workout.id, now, request_id)?));
        }
        Action::Swap => "swap",
        Action::Later => "later",
        Action::Next => "next",
        Action::Relabel => unreachable!("handled above"),
    };
    let rows = exercises(db, &workout.id)?;
    let Some(row) = rows.iter().find(|e| e.status == ExerciseStatus::Active).or_else(|| rows.iter().find(|e| e.status == ExerciseStatus::Deferred)) else {
        crate::bail!("NO_ACTIVE_EXERCISE", "Nothing left to act on; end the workout");
    };
    handle_tap(db, &format!("{tap}:{}", reference(&row.id)), now, request_id)
}

/// 'Start workout' on the session reminder: start that planned session's workout and send its first card.
pub fn begin(db: &Connection, r: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    if let Some(running) = active_workout(db)? {
        return Ok(acked("Already running.", vec![exercise_card(db, &running.id, now)?]));
    }
    let planned = PlannedSession::first(db, "WHERE substr(id, 1, length(?1)) = ?1", [r])?;
    let Some((planned, template_id)) = planned.and_then(|p| p.workout_template_id.clone().filter(|_| p.status.is_open()).map(|t| (p, t))) else {
        return Ok(acked("⌛ This session can't be started anymore.", vec![]));
    };
    start(db, Some(&template_id), Some(&planned.id), true, now, request_id)?;
    let running = active_workout(db)?.ok_or_else(|| Error::domain("NO_ACTIVE_WORKOUT", "Workout did not start"))?;
    Ok(acked("▶️ Started.", vec![exercise_card(db, &running.id, now)?]))
}

/// Session preview as a standalone coach message (the live card stays).
pub fn preview(db: &Connection, now: Ts, planned_session_id: Option<&str>, data_dir: &Path) -> Result<CoachView> {
    let card = crate::domain::preview::session_preview(db, now, planned_session_id, data_dir)?;
    Ok(CoachView { standalone: true, cards: vec![card], ..handled() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{ExerciseProgression, JobStatus, NotificationJob};
    use crate::domain::templates::{Template, get_template, import_template};
    use crate::domain::workout::tests::{at, db, spec};

    /// Plan order puts shoulders between the two chest exercises, like the first real session.
    fn push(db: &Connection) {
        let template = Template {
            id: "push".into(),
            name: "Push".into(),
            exercises: vec![
                spec(json!({"id": "incline", "name": "Machine incline press", "role": "press", "guide_id": "incline-bench-press", "working_sets": 2, "target_weight": 0, "rest_seconds": 90})),
                spec(json!({"id": "shoulder", "name": "Shoulder press", "role": "press", "guide_id": "machine-shoulder-press", "working_sets": 2, "target_weight": 25})),
                spec(json!({"id": "fly", "name": "Cable fly", "role": "fly", "guide_id": "cable-fly", "working_sets": 2, "target_weight": 40})),
            ],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(db, &template).unwrap();
        start(db, Some("push"), None, false, at(0), "start").unwrap();
    }

    fn buttons(card: &Card) -> Vec<(String, String)> {
        card.buttons.iter().flatten().map(|b| (b.text.clone(), b.data.trim_start_matches("gc:").to_string())).collect()
    }

    fn data(card: &Card, text: &str) -> String {
        buttons(card).into_iter().find(|(t, _)| t == text).unwrap_or_else(|| panic!("no button {text}: {:?}", buttons(card))).1
    }

    fn live(db: &Connection, now: Ts) -> Card {
        exercise_card(db, &active_workout(db).unwrap().unwrap().id, now).unwrap()
    }

    #[test]
    fn typed_set_then_rest_card_then_fresh_set_card() {
        let db = db();
        push(&db);
        let first = live(&db, at(0));
        // No weight known: no 0 kg warm-up; the card asks for the weight.
        assert!(first.photo.is_some());
        assert!(first.text.contains("Set 1/2 · 8–10 × **? kg**") && first.text.contains("✍️ **Reply reps × weight:** `8x40`"), "{}", first.text);
        assert!(!handle_text(&db, "how long do I rest?", at(5), "chat").unwrap().handled);
        let result = handle_text(&db, "12x90kg", at(10), "msg-1").unwrap();
        assert_eq!((result.react.as_deref(), result.ack.as_deref()), (Some("👍"), Some("✅ 12 × 90 kg")));
        let rest = &result.cards[0];
        assert_eq!((rest.kind, rest.rest_until), (CardKind::Rest, Some(at(100))));
        assert_eq!(rest.text, "until **Machine incline press** set 2/2");
        assert_eq!(buttons(rest).iter().map(|b| b.0.as_str()).collect::<Vec<_>>(), ["−15s", "−30s", "⏭ Skip"]);
        let skipped = handle_tap(&db, &data(rest, "⏭ Skip"), at(30), "cb-skip").unwrap();
        let card = &skipped.cards[0];
        assert_eq!((skipped.cleanup, card.kind), (Some(Cleanup::Delete), CardKind::Set));
        data(card, "✅ 12 × 90 kg");
        // Already on the machine: no Swap after the first set, not even from an older card.
        assert!(!buttons(card).iter().any(|b| b.0 == "🔄 Swap"));
        let swap = handle_tap(&db, &data(&first, "🔄 Swap"), at(31), "cb-swap").unwrap();
        assert_eq!(swap.ack.as_deref(), Some("Already started Machine incline press: finish it or tap Next."));
    }

    #[test]
    fn rest_over_closes_job_once_and_finishing_an_exercise_rests_before_the_next() {
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        assert!(handle_rest_over(&db, at(50)).unwrap().cards.is_empty());
        let over = handle_rest_over(&db, at(101)).unwrap();
        assert!(over.rest_over && over.cleanup == Some(Cleanup::Delete) && over.cards[0].kind == CardKind::Set);
        assert_eq!(NotificationJob::select(&db, "", []).unwrap()[0].status, JobStatus::Fired);
        assert!(!handle_rest_over(&db, at(102)).unwrap().rest_over);
        let rest = handle_text(&db, "10x90", at(110), "m2").unwrap().cards.remove(0);
        // The finished exercise's rest (90 s), and the countdown says what comes next.
        assert_eq!((rest.text.as_str(), rest.rest_until), ("until next: **Shoulder press**", Some(at(200))));
        let next = handle_rest_over(&db, at(201)).unwrap().cards.remove(0);
        assert!(next.text.starts_with("**Shoulder press**"));
        data(&next, "🔄 Swap");
    }

    #[test]
    fn swap_menu_later_free_and_remembered_swap() {
        let db = db();
        push(&db);
        let card = live(&db, at(0));
        let swap = handle_tap(&db, &data(&card, "🔄 Swap"), at(5), "cb-1").unwrap();
        assert!(swap.keep && swap.cards.len() == 3 && swap.cards.iter().all(|o| o.photo.is_some() && o.kind == CardKind::Option));
        let menu = Card { buttons: swap.live_buttons.clone().unwrap(), ..Card::default() };
        assert!(handle_tap(&db, &data(&menu, "↩ Keep Machine incline press"), at(6), "cb-2").unwrap().restore);
        let later = handle_tap(&db, &data(&menu, "↪ Do it later"), at(7), "cb-3").unwrap();
        // Cable fly (chest) comes before the shoulder press while the chest machine is taken.
        assert!(later.cards[0].text.starts_with("**Cable fly** · Chest"), "{}", later.cards[0].text);
        let back = handle_tap(&db, &data(&later.cards[0], "↩ Machine incline press free?"), at(8), "cb-4").unwrap();
        assert!(back.cards[0].text.starts_with("**Machine incline press**"));
        // Swap it for the first option: the template remembers.
        let option = handle_tap(&db, &data(&back.cards[0], "🔄 Swap"), at(9), "cb-5").unwrap().cards.remove(0);
        let sub = buttons(&option)[0].1.clone();
        let slug = sub.split(':').nth(2).unwrap().to_string();
        assert_eq!(handle_tap(&db, &sub, at(10), "cb-6").unwrap().cleanup, Some(Cleanup::Delete));
        let template = get_template(&db, "push").unwrap();
        assert_eq!((template.exercises[0].id.as_str(), template.exercises[0].working_sets), (slug.as_str(), 2));
        assert_eq!((template.exercises[0].substitutes.clone(), template.alternatives[0].id.as_str()), (vec!["incline".to_string()], "incline"));
        // Changed their mind: the swapped-away exercise is offered first.
        let again = handle_tap(&db, &data(&live(&db, at(11)), "🔄 Swap"), at(11), "cb-7").unwrap();
        let back = handle_tap(&db, &data(&again.cards[0], "↩ Back to Machine incline press"), at(12), "cb-8").unwrap();
        assert!(back.cards[0].text.starts_with("**Machine incline press**"));
    }

    #[test]
    fn next_end_and_baseline_weight_then_relabel() {
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        handle_rest_over(&db, at(101)).unwrap();
        let moved = handle_tap(&db, &data(&live(&db, at(110)), "⏭ Next exercise"), at(112), "cb-1").unwrap();
        assert!(moved.cards[0].text.starts_with("**Shoulder press**"));
        let summary = handle_action(&db, Action::End, None, None, None, false, at(600), "end").unwrap().cards.remove(0);
        assert!(summary.text.contains("🏁 **Push done** · 1 sets · 10 min"), "{}", summary.text);
        assert_eq!(buttons(&summary).iter().map(|b| b.0.as_str()).collect::<Vec<_>>(), ["Empty", "A few", "Busy", "Packed"]);
        // Unknown starting weight becomes the logged baseline for next time.
        assert_eq!(ExerciseProgression::get(&db, "incline").unwrap().unwrap().next_weight, 90.0);
        let fixed = handle_action(&db, Action::Relabel, Some("incline-dumbbell-press"), Some("Machine incline press"), None, false, at(700), "fix").unwrap();
        assert_eq!((fixed.ack.as_deref(), fixed.cards.len()), (Some("✏️ Saved as Incline Dumbbell Press."), 0));
        assert!(ExerciseProgression::get(&db, "incline").unwrap().is_none());
        assert_eq!(ExerciseProgression::get(&db, "incline-dumbbell-press").unwrap().unwrap().next_weight, 90.0);
        assert_eq!(get_template(&db, "push").unwrap().exercises[0].id, "incline-dumbbell-press");
        // Renamed movement with no history of its own: the card guesses from the same artwork.
        let template = Template { id: "push2".into(), exercises: vec![spec(json!({"id": "incline-v2", "name": "Incline press", "role": "press", "guide_id": "incline-dumbbell-press", "working_sets": 2, "target_weight": 0}))], ..get_template(&db, "push").unwrap() };
        import_template(&db, &Template { alternatives: vec![], ..template }).unwrap();
        start(&db, Some("push2"), None, false, at(800), "start-2").unwrap();
        let card = live(&db, at(801));
        assert!(card.text.contains("💡 Guess from Incline Dumbbell Press") && card.text.contains("`8x90`"), "{}", card.text);
        data(&card, "✅ 8 × 90 kg?");
    }

    #[test]
    fn cut_rest_and_rest_change() {
        let db = db();
        push(&db);
        assert!(handle_action(&db, Action::Rest, None, None, Some(60), true, at(1), "rest").unwrap().keep);
        let rest = handle_text(&db, "10x90", at(10), "m1").unwrap().cards.remove(0);
        assert_eq!(rest.rest_until, Some(at(70)));
        let cut = handle_tap(&db, &data(&rest, "−30s"), at(20), "cb-1").unwrap();
        assert!(cut.refresh && cut.cards[0].rest_until == Some(at(40)));
        // Cutting past zero ends the rest: the next set card comes right away.
        let over = handle_tap(&db, &data(&rest, "−30s"), at(30), "cb-2").unwrap();
        assert!(over.cleanup == Some(Cleanup::Delete) && over.cards[0].kind == CardKind::Set);
        let profile = crate::domain::profile::get_profile(&db).unwrap();
        assert_eq!((profile.default_compound_rest_seconds, profile.default_accessory_rest_seconds), (60, 60));
    }

    #[test]
    fn owner_names_the_exercise_and_the_workout_follows() {
        let db = db();
        push(&db);
        let option = handle_tap(&db, &data(&live(&db, at(0)), "🔄 Swap"), at(1), "cb-1").unwrap().cards.remove(0);
        handle_tap(&db, &buttons(&option)[0].1, at(2), "cb-2").unwrap();
        let back = handle_action(&db, Action::Switch, Some("Machine incline press"), None, None, false, at(3), "sw-1").unwrap();
        assert!(back.cards[0].text.starts_with("**Machine incline press**"));
        assert_eq!(get_template(&db, "push").unwrap().exercises[0].id, "incline");
        let unknown = handle_action(&db, Action::Switch, Some("dumbbell rdl"), None, None, false, at(4), "sw-x").unwrap_err();
        assert!(unknown.to_string().contains("dumbbell-romanian-deadlift"), "{unknown}");
        // After a set any exercise still goes; the started one keeps its set and counts as done.
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        let now_on = handle_action(&db, Action::Switch, Some("dumbbell-romanian-deadlift"), None, None, false, at(20), "sw-2").unwrap().cards.remove(0);
        assert!(now_on.text.starts_with("**Dumbbell Romanian Deadlift**") && now_on.rest_until.is_none());
        let queue = workout::state(&db, &active_workout(&db).unwrap().unwrap().id, at(20)).unwrap().queue;
        let has = |id: &str, status| queue.iter().any(|q| q.exercise_id == id && q.status == status);
        assert!(has("incline", ExerciseStatus::Completed) && has("dumbbell-romanian-deadlift", ExerciseStatus::Active));
    }

    #[test]
    fn bodyweight_reps_alone_and_no_rest_after_the_last_set() {
        let db = db();
        let template = Template {
            id: "core".into(),
            name: "Core".into(),
            exercises: vec![spec(json!({"id": "crunch", "name": "Crunch", "role": "core", "guide_id": "crunch", "working_sets": 1, "rep_min": 12, "rep_max": 15, "target_weight": 0}))],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(&db, &template).unwrap();
        start(&db, Some("core"), None, false, at(0), "start").unwrap();
        let card = live(&db, at(1));
        assert!(card.text.contains("Set 1/1 · 12–15 reps"), "{}", card.text);
        data(&card, "✅ 12 reps");
        let done = handle_text(&db, "13 reps", at(90), "m2").unwrap();
        assert_eq!(done.ack.as_deref(), Some("✅ 13 reps"));
        assert!(done.cards.iter().all(|c| c.rest_until.is_none()) && done.cards[0].text.starts_with("🏁"));
    }

    fn planned(db: &Connection, template: &str, start: Ts, status: crate::db::models::SessionStatus) -> crate::db::models::PlannedSession {
        let mut plan = crate::db::models::PlannedSession::new("2026-10-12".into(), Some(template.into()), false);
        plan.status = status;
        (plan.planned_start_at, plan.planned_end_at, plan.prep_start_at, plan.leave_home_at, plan.expected_finish_at) = (start, start.mins(60), start.mins(-30), start.mins(-15), start.mins(60));
        plan.insert(db).unwrap();
        plan
    }

    #[test]
    fn a_few_rating_is_stored_as_fine() {
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        let done = handle_action(&db, Action::End, None, None, None, false, at(600), "end").unwrap().cards.remove(0);
        let rating = handle_tap(&db, &data(&done, "A few"), at(601), "cb-2").unwrap();
        assert_eq!(rating.ack.as_deref(), Some("Saved: A few. Thanks."));
        let stored: String = db.query_row("SELECT rating FROM crowd_feedback", [], |r| r.get(0)).unwrap();
        assert_eq!(stored, "FINE");
    }

    #[test]
    fn skip_before_any_set_on_a_swapped_card_is_an_old_button() {
        let db = db();
        push(&db);
        let card = live(&db, at(0));
        let option = handle_tap(&db, &data(&card, "🔄 Swap"), at(1), "cb-1").unwrap().cards.remove(0);
        handle_tap(&db, &buttons(&option)[0].1, at(2), "cb-2").unwrap();
        assert_eq!(handle_tap(&db, &data(&card, "⏭ Skip"), at(3), "cb-3").unwrap().ack.as_deref(), Some("⌛ Old button."));
    }

    #[test]
    fn idle_workout_is_closed_and_keeps_logged_sets() {
        use crate::domain::weekly::close_stale_workouts;
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        assert!(close_stale_workouts(&db, at(10 + 2 * 3600)).unwrap().is_empty());
        let id = active_workout(&db).unwrap().unwrap().id;
        assert_eq!(close_stale_workouts(&db, at(10 + 3 * 3600 + 1)).unwrap(), vec![id.clone()]);
        assert_eq!(crate::db::models::WorkoutSession::get(&db, &id).unwrap().unwrap().status, WorkoutStatus::PlanUpdated);
        assert_eq!(workout::logs(&db, &workout::exercises(&db, &id).unwrap()[0].id, None).unwrap().len(), 1);
        assert!(active_workout(&db).unwrap().is_none());
        start(&db, Some("push"), None, false, at(10 + 4 * 3600), "again").unwrap();
    }

    #[test]
    fn session_preview_uses_current_plan_local_time_and_one_image() {
        let db = db();
        push(&db);
        let dir = std::env::temp_dir().join(format!("gymclaw-preview-{}", crate::db::new_id()));
        planned(&db, "push", Ts::parse("2026-10-19T08:30:00+00:00", None).unwrap(), crate::db::models::SessionStatus::Tentative);
        let card = crate::domain::preview::session_preview(&db, at(0), None, &dir).unwrap();
        assert_eq!(card.text.lines().next(), Some("**Push** · Mon 10:30")); // Europe/Berlin, not UTC
        assert!(card.text.contains("1. Machine incline press · 2 sets of 8–10") && card.text.contains("3. Cable fly · 2 sets of 8–10"), "{}", card.text);
        let photo = card.photo.clone().unwrap();
        assert!(std::path::Path::new(&photo).is_file() && photo.starts_with(dir.to_str().unwrap()));
        assert_eq!(crate::domain::preview::session_preview(&db, at(0), None, &dir).unwrap().photo, card.photo); // cached
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_button_starts_once_without_a_blind_warmup() {
        let db = db();
        push(&db);
        handle_action(&db, Action::End, None, None, None, false, at(1), "end").unwrap();
        let legs = Template {
            id: "legs".into(),
            name: "Legs".into(),
            exercises: vec![spec(json!({"id": "press", "name": "Leg press", "role": "legs", "guide_id": "leg-press", "primary": true, "working_sets": 1, "target_weight": 0, "warmup_weight": 0}))],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(&db, &legs).unwrap();
        let plan = planned(&db, "legs", at(60), crate::db::models::SessionStatus::Committed);
        let tap = format!("begin:{}", &plan.id[..8]);
        let started = handle_tap(&db, &tap, at(70), "cb-begin").unwrap();
        assert_eq!(started.ack.as_deref(), Some("▶️ Started."));
        assert_eq!(crate::db::models::PlannedSession::get(&db, &plan.id).unwrap().unwrap().status, crate::db::models::SessionStatus::Started);
        // Unknown weight: straight to the working set, no "8 × 0 kg" warm-up.
        assert!(started.cards[0].text.contains("**? kg**"), "{}", started.cards[0].text);
        assert_eq!(handle_tap(&db, &tap, at(71), "cb-again").unwrap().ack.as_deref(), Some("Already running."));
    }

    #[test]
    fn only_button_starts_count_as_gym_arrivals_for_habits() {
        use crate::domain::habits::evidence;
        let db = db();
        push(&db);
        let tz = crate::time::zone("Europe/Berlin").unwrap();
        assert!(evidence(&db, at(60), &tz).unwrap().is_empty()); // started from chat
        handle_action(&db, Action::End, None, None, None, false, at(60), "end").unwrap();
        let tomorrow = at(86400);
        let plan = planned(&db, "push", tomorrow, crate::db::models::SessionStatus::Committed);
        begin(&db, &plan.id[..8], tomorrow, "begin").unwrap();
        assert_eq!(evidence(&db, tomorrow, &tz).unwrap(), vec![(20.0 * 60.0, 1.0)]); // 18:00 UTC = 20:00 Berlin
    }
}
