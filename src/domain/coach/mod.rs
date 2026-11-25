//! Telegram coach flows: cards, taps and typed sets.
//!
//! Deterministic and model-free: a typed set, a button tap or an agent action in; a `CoachView` out
//! (see `view.rs` for what each flag means to the renderer). Button data is
//! `gc:<action>:<exercise-ref>[:args]` (≤ 64 bytes). The ref is the first 8 characters of the workout
//! exercise ID, so old buttons stop working once the workout has moved past that exercise.
mod app;
pub use app::*;

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::db::Connection;
use crate::db::models::{
    ChatMessage, ExerciseStatus, PlannedSession, SetType, WorkoutExercise, WorkoutSession, WorkoutStatus,
};
use crate::domain::adaptation::{self, spec_of};
use crate::domain::audit;
use crate::domain::core::mutate;
use crate::domain::crowd::{LABELS, record_feedback};
use crate::domain::highlights::{self, Change};
use crate::domain::illustrations::{catalog, timed};
use crate::domain::profile::{get_profile, update_profile};
use crate::domain::set_parser::{SetInput, parse_block, parse_set};
use crate::domain::templates::{Load, Measure, SetKind, remember_swap};
use crate::domain::view::{Button, Card, CardKind, CoachView, Edit};
use crate::domain::workout::{
    self, active_workout, cut_rest, exercises, format_distance, format_time, g, guide_id, kind, log_set, logs, measure,
    name, pending_rest, rest_complete, set_rest, skip_rest, skip_warmup, start, unresolved, working_count,
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

fn card(text: String, buttons: Vec<Vec<Button>>) -> Card {
    Card { text, buttons, ..Card::default() }
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
    CoachView { delete: true, cards, ..handled() }
}

/// `workout::next_working_set`, or bodyweight reps (no weight to ask for). The set card, its quick
/// button and the rest countdown all show this, so they never disagree.
fn next_set(db: &Connection, row: &WorkoutExercise, bodyweight: bool) -> Result<Option<(f64, i64)>> {
    Ok(workout::next_working_set(db, row)?.or(bodyweight.then_some((0.0, row.rep_min))))
}

/// Card for whatever the owner should do next. Every set card carries the illustration.
pub fn exercise_card(db: &Connection, workout_id: &str, now: Ts) -> Result<Card> {
    let unit = get_profile(db)?.weight_unit;
    let data = workout::state(db, workout_id, now)?;
    let rows = exercises(db, workout_id)?;
    let deferred: Vec<&WorkoutExercise> = rows.iter().filter(|e| e.status == ExerciseStatus::Deferred).collect();
    let Some(active) = data.active_exercise else {
        let Some(waiting) = deferred.first() else {
            crate::bail!("NO_ACTIVE_EXERCISE", "Nothing left to act on; end the workout")
        };
        let r = reference(&waiting.id);
        return Ok(card(
            format!("Only **{}** left. It was taken earlier.", name(waiting)),
            vec![
                vec![button("✅ It's free now", &["free", r])?, button("🔄 Swap", &["swap", r])?],
                vec![button("⏭ Skip it", &["drop", r])?],
            ],
        ));
    };
    let r = reference(&active.id);
    let row = rows.iter().find(|e| e.id == active.id).expect("active row");
    if let Some(rest) = data.rest_job {
        // Resting there is nothing to log yet: just the countdown and ways to shorten it.
        let upcoming = if active.new_exercise {
            format!("next: **{}**", active.name)
        } else {
            format!("**{}** set {}/{}", active.name, active.set_number, active.working_sets)
        };
        let weight = if active.set_type == SetType::Warmup {
            active.target_weight
        } else {
            next_set(db, row, active.bodyweight)?.map_or(0.0, |(w, _)| w)
        };
        let target = if active.measure == Measure::Time {
            active.seconds.map_or("timed".into(), format_time)
        } else if active.measure == Measure::Distance {
            active.meters.map_or("distance".into(), format_distance)
        } else if weight > 0.0 {
            active.kind().in_unit(unit).weight(weight)
        } else if active.bodyweight {
            "Bodyweight".into()
        } else {
            format!("? {}", unit.label())
        };
        // Shorter or longer: a cut by a negative number adds time (a hard set needs more rest).
        return Ok(Card {
            text: format!("until {upcoming} · {target}"),
            photo: None,
            buttons: vec![
                vec![
                    button("−30s", &["cut", r, "30"])?,
                    button("−15s", &["cut", r, "15"])?,
                    button("+15s", &["cut", r, "-15"])?,
                    button("+30s", &["cut", r, "-30"])?,
                ],
                vec![button("⏭ Skip", &["skip", r])?],
            ],
            rest_until: Some(rest.due_at),
            kind: CardKind::Rest,
            effect: None,
            workout_id: Some(workout_id.into()),
            exercise_id: Some(active.id.clone()),
        });
    }
    let head =
        format!("**{}**{}", active.name, active.primary_muscle.as_ref().map(|m| format!(" · {m}")).unwrap_or_default());
    let set_kind = active.kind().in_unit(unit);
    let (lines, mut buttons) = if active.set_type == SetType::Warmup {
        (
            vec![head, format!("Warm-up · {}", set_kind.target(active.rep_min, active.rep_max, active.target_weight))],
            vec![
                vec![button("✅ Warm-up done", &["warm", r])?, button("Skip warm-up", &["nowarm", r])?],
                vec![button("🔄 Swap", &["swap", r])?],
            ],
        )
    } else {
        let set_line = format!("Set {}/{}", active.set_number, active.working_sets);
        let (mut lines, mut buttons) = (vec![head], vec![]);
        if active.measure == Measure::Time {
            // One tap logs the planned time; any other time is typed.
            let step = if active.working_sets > 1 { format!("{set_line} · ") } else { String::new() };
            if let Some(seconds) = active.seconds {
                let time = set_kind.reps(&format_time(seconds));
                lines.push(format!("{step}⏱ {time}"));
                lines.push("✍️ Other time? Reply it, e.g. `30 min`".into());
                buttons.push(vec![button(format!("✅ {time}"), &["time", r, &seconds.to_string()])?]);
            } else {
                lines.push(format!("{step}⏱ **? min**"));
                lines.push("✍️ **Reply the time:** `25 min`".into());
            }
        } else if active.measure == Measure::Distance {
            // Same as a time: one tap logs the planned distance.
            let step = if active.working_sets > 1 { format!("{set_line} · ") } else { String::new() };
            if let Some(meters) = active.meters {
                lines.push(format!("{step}📏 {}", format_distance(meters)));
                lines.push("✍️ Other distance? Reply it, e.g. `2.5 km`".into());
                let label = format!("✅ {}", format_distance(meters));
                buttons.push(vec![button(label, &["dist", r, &g(meters)])?]);
            } else {
                lines.push(format!("{step}📏 **? m**"));
                lines.push("✍️ **Reply the distance:** `2 km`".into());
            }
        } else if let Some((weight, reps)) = next_set(db, row, active.bodyweight)? {
            lines.push(format!("{set_line} · {}", set_kind.target(active.rep_min, active.rep_max, weight)));
            let label = format!("✅ {}", set_kind.set(weight, reps, None, None));
            buttons.push(vec![button(label, &["log", r, &g(weight), &reps.to_string()])?]);
        } else {
            // Unknown weight: the gap is the headline and the reply format is impossible to miss.
            let guess = guess_set(db, row, active.rep_min)?;
            let example = guess.as_ref().map_or(
                format!("{}x{}", active.rep_min, if unit == crate::db::models::WeightUnit::Lb { 90 } else { 40 }),
                |(w, reps, _)| format!("{reps}x{}", g(unit.to_display(*w))),
            );
            let reps = set_kind.target(active.rep_min, active.rep_max, 0.0);
            lines.push(format!("{set_line} · {} × **? {}**", reps.trim_end_matches(" reps"), set_kind.unit()));
            lines.push(format!("✍️ **Reply reps × weight:** `{example}`"));
            if let Some((weight, reps, source)) = guess {
                lines.push(format!("💡 Guess from {source}"));
                let label = format!("✅ {}?", set_kind.set(weight, reps, None, None));
                buttons.push(vec![button(label, &["log", r, &g(weight), &reps.to_string()])?]);
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
    Ok(Card {
        text: lines.join("\n"),
        photo: active.illustration.map(|i| i.telegram_png),
        buttons,
        rest_until: None,
        kind: CardKind::Set,
        effect: None,
        workout_id: Some(workout_id.into()),
        exercise_id: Some(active.id.clone()),
    })
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
    let recent: Vec<(f64, crate::db::Json<Value>)> =
        stmt.query_map([&row.exercise_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for same in [true, false] {
        for (weight, config) in &recent {
            let Some(other) = config.get("guide_id").and_then(Value::as_str).and_then(|g| catalog().get(g)) else {
                continue;
            };
            let hit = if same {
                other.slug == item.slug
            } else {
                other.primary_muscle == item.primary_muscle && other.equipment == item.equipment
            };
            if hit {
                return Ok(Some((
                    *weight,
                    reps,
                    config.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                )));
            }
        }
    }
    Ok(None)
}

/// "6.2 t" (kg) or "13.7k lb" (lb) from canonical kilograms.
fn tonnes(kg: f64, unit: crate::db::models::WeightUnit) -> String {
    match unit {
        crate::db::models::WeightUnit::Kg => format!("{:.1} t", kg / 1000.0),
        crate::db::models::WeightUnit::Lb => format!("{:.1}k lb", unit.to_display(kg) / 1000.0),
    }
}

/// One exercise's working sets for the details quote: "Bench 85 kg: 8 · 8 · 7" when the weight held,
/// else "Bench 8 × 85 · 7 × 82.5 kg"; "Dips 15 · 12 reps" for bodyweight; "Cycling 25 min" when timed,
/// "Rower 2 km" for distance. Added and assisted weights read "+10 kg" and "20 kg assist".
fn sets_line(name: &str, set_kind: SetKind, sets: &[Value]) -> String {
    let blocks: Vec<String> = sets
        .iter()
        .filter(|s| s["seconds"].is_i64() || s["meters"].is_f64())
        .map(|s| set_kind.set(0.0, 0, s["seconds"].as_i64(), s["meters"].as_f64()))
        .collect();
    if !blocks.is_empty() {
        return format!("{name} {}", blocks.join(" · "));
    }
    let pairs: Vec<(f64, i64)> =
        sets.iter().map(|s| (s["weight"].as_f64().unwrap_or(0.0), s["reps"].as_i64().unwrap_or(0))).collect();
    let reps =
        |pairs: &[(f64, i64)]| pairs.iter().map(|(_, r)| set_kind.reps(&r.to_string())).collect::<Vec<_>>().join(" · ");
    match pairs.first() {
        Some((w, _)) if pairs.iter().all(|(x, _)| x == w) && *w == 0.0 => {
            format!("{name} {}{}", reps(&pairs), if set_kind.per_side { "" } else { " reps" })
        }
        Some((w, _)) if pairs.iter().all(|(x, _)| x == w) => {
            format!("{name} {}: {}", set_kind.weight(*w), reps(&pairs))
        }
        _ => format!(
            "{name} {} {}",
            pairs
                .iter()
                .map(|(w, r)| format!("{} × {}", set_kind.reps(&r.to_string()), set_kind.number(*w)))
                .collect::<Vec<_>>()
                .join(" · "),
            set_kind.unit()
        ),
    }
}

/// "Legs Thu 18:00": the next planned session.
fn next_session(db: &Connection, now: Ts, tz: &jiff::tz::TimeZone) -> Result<Option<String>> {
    let next = PlannedSession::first(
        db,
        "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_start_at > ?1 ORDER BY planned_start_at",
        [now],
    )?;
    Ok(next.map(|s| {
        let name = s.plan("template").and_then(|t| t.get("name")).and_then(Value::as_str).map(String::from);
        format!(
            "{} {}",
            name.or(s.workout_template_id.clone()).unwrap_or_else(|| "Workout".into()),
            s.planned_start_at.local(tz).strftime("%a %H:%M")
        )
    }))
}

/// The wrap-up card: totals, pace and volume against the last session of the same workout, every
/// lift that beat earlier sessions (🏆 for records, with confetti), next targets, where the week
/// stands, every set in a collapsed quote, then the crowd question.
fn summary_card(db: &Connection, report: &Value, now: Ts) -> Result<Card> {
    let workout_id = report["workout_id"].as_str().unwrap_or_default();
    let h = highlights::workout(db, workout_id)?;
    let profile = crate::domain::profile::get_profile(db)?;
    let tz = profile.tz();
    let volume = if h.volume > 0.0 { format!(" · {}", tonnes(h.volume, profile.weight_unit)) } else { String::new() };
    let sets = match h.sets {
        // Only timed work (a bike session): its minutes are the next lines, not "0 sets".
        0 if !h.timed.is_empty() => String::new(),
        1 => " · 1 set".to_string(),
        n => format!(" · {n} sets"),
    };
    let mut lines = vec![format!("🏁 **{} done** · {} min{sets}{volume}", h.name, h.minutes)];
    if let Some((minutes, volume)) = h.last {
        let mut versus = Vec::new();
        if minutes.abs_diff(h.minutes) >= 3 {
            versus.push(if h.minutes < minutes {
                format!("⚡ {} min faster", minutes - h.minutes)
            } else {
                format!("{} min slower", h.minutes - minutes)
            });
        }
        let change = if volume > 0.0 { ((h.volume / volume - 1.0) * 100.0).round() as i64 } else { 0 };
        if change != 0 {
            versus.push(if change > 0 { format!("📈 +{change}% volume") } else { format!("📉 {change}% volume") });
        }
        if !versus.is_empty() {
            lines.push(format!("{} vs last {}", versus.join(" · "), h.name));
        }
    }
    for (name, seconds) in &h.timed {
        lines.push(format!("⏱ **{name}** {}", format_time(*seconds)));
    }
    for (name, meters) in &h.distances {
        lines.push(format!("📏 **{name}** {}", format_distance(*meters)));
    }
    for lift in &h.lifts {
        let set = lift.kind.set(lift.weight, lift.reps, None, None);
        let (heaviest, heavier) =
            if lift.kind.load == Load::Assisted { ("least help ever", "less help") } else { ("heaviest ever", "was") };
        lines.push(match lift.change {
            Change::WeightRecord(was) => {
                format!("🏆 **{} {set}** · {heaviest} (was {})", lift.name, lift.kind.number(was))
            }
            Change::RepRecord(was) if lift.weight == 0.0 => {
                format!("🏆 **{} {set}** · most ever (was {was})", lift.name)
            }
            Change::RepRecord(was) => {
                format!("🏆 **{} {set}** · most reps at {} (was {was})", lift.name, lift.kind.weight(lift.weight))
            }
            Change::Heavier(was) if lift.kind.load == Load::Assisted => {
                format!("↑ {} {set} ({heavier}, was {})", lift.name, lift.kind.number(was))
            }
            Change::Heavier(was) => format!("↑ {} {set} ({heavier} {})", lift.name, lift.kind.number(was)),
            Change::MoreReps(was) => format!("↑ {} {set} (+{} reps)", lift.name, lift.reps - was),
        });
    }
    let exercises = report["exercises"].as_array().cloned().unwrap_or_default();
    let progressions = report["progressions"].as_array().cloned().unwrap_or_default();
    // How each exercise's sets read, by its workout row.
    let kinds: Vec<SetKind> = exercises
        .iter()
        .map(|e| {
            Ok(WorkoutExercise::get(db, e["id"].as_str().unwrap_or_default())?
                .map(|r| kind(&r).in_unit(profile.weight_unit))
                .unwrap_or_default())
        })
        .collect::<Result<_>>()?;
    let targets: Vec<String> = exercises
        .iter()
        .zip(&progressions)
        .zip(&kinds)
        .filter(|((_, p), _)| p["progressed"].as_bool() == Some(true))
        .map(|((e, p), k)| {
            format!("{} {}", e["name"].as_str().unwrap_or_default(), k.weight(p["next_weight"].as_f64().unwrap_or(0.0)))
        })
        .collect();
    if !targets.is_empty() {
        lines.push(format!("🎯 Next time: {}", targets.join(", ")));
    }
    // The week the workout started in: one that runs past Sunday midnight still counts for that week.
    let week = highlights::finished_in(db, crate::domain::calendar::week_of(h.started_at, &tz), &tz)?.len() as i64;
    let target = profile.weekly_target_sessions;
    let mut standing = if target > 0 && week >= target {
        format!("🗓 Week done: **{week}/{target}** ✅")
    } else {
        format!("🗓 This week **{week}/{target}**")
    };
    if let Some(next) = next_session(db, now, &tz)? {
        standing += &format!(" · next: {next}");
    }
    lines.push(standing);
    for (e, set_kind) in exercises.iter().zip(kinds) {
        let name = e["name"].as_str().unwrap_or_default();
        let sets = e["sets"].as_array().cloned().unwrap_or_default();
        if e["status"] == "SKIPPED" && sets.is_empty() {
            lines.push(format!(">> ~~{name}~~ skipped"));
        } else if !sets.is_empty() {
            lines.push(format!(">> {}", sets_line(name, set_kind, &sets)));
        }
    }
    let buttons = if crate::domain::crowd::should_ask_feedback(db)? {
        lines.push("How busy was the gym?".into());
        vec![crowd_buttons(workout_id)?]
    } else {
        vec![]
    };
    let effect = h.lifts.iter().any(|l| l.record()).then(|| crate::telegram::EFFECT_CONFETTI.to_string());
    Ok(Card {
        effect,
        kind: CardKind::Finish,
        workout_id: Some(workout_id.to_string()),
        ..card(lines.join("\n"), buttons)
    })
}

/// The ack a crowd answer leaves under the finish card.
/// The button data that starts a planned session (`begin`), as the reminder's Start button carries it.
pub fn begin_data(session_id: &str) -> String {
    format!("gc:begin:{}", reference(session_id))
}

/// "How busy was the gym?" after a workout: one button per rating (the finish card and the Mini App).
pub fn crowd_buttons(workout_id: &str) -> Result<Vec<Button>> {
    let r = reference(workout_id);
    LABELS.iter().map(|(value, label)| button(*label, &["crowd", value, r])).collect()
}

fn crowd_ack(rating: &str) -> String {
    let label = LABELS.iter().find(|(value, _)| value.eq_ignore_ascii_case(rating)).map_or(rating, |(_, label)| label);
    format!("Saved: {label}. Thanks.")
}

/// A finished workout's finish card, re-rendered from its sets as they are now, as an edit of the
/// message it went out as (empty while it runs, or when no finish card was recorded for it). The
/// stored chat text follows, so the agent's view of the chat never shows the old sets.
/// Called after anything changes a finished workout's sets: `set_edit`, a set added later, relabel.
pub fn finish_card_edits(db: &Connection, workout_id: &str, now: Ts) -> Result<Vec<Edit>> {
    let workout = workout::load_workout(db, workout_id, None)?;
    if workout.status != WorkoutStatus::PlanUpdated {
        return Ok(vec![]);
    }
    let Some(mut sent) = ChatMessage::first(
        db,
        "WHERE meta_json ->> 'card' = 'finish' AND meta_json ->> 'workout_id' = ?1
         AND telegram_message_id IS NOT NULL AND meta_json ->> 'removed' IS NULL ORDER BY id DESC",
        [workout_id],
    )?
    else {
        return Ok(vec![]);
    };
    // As of the finish, so "next:" and the week count read as they did then.
    let mut card = summary_card(db, &audit::report(db, workout_id)?, workout.completed_at.unwrap_or(now))?;
    card.effect = None;
    if let Some(rating) = &workout.crowd_feedback {
        // Answered already: the card looks the way the answer left it.
        card.text = format!("{}\n{}", card.text, crowd_ack(rating));
        card.buttons = vec![];
    }
    sent.text = card.text.clone();
    sent.update(db)?;
    Ok(vec![Edit { message_id: sent.telegram_message_id.expect("checked in the query"), card }])
}

/// For an owner's Telegram reply: what the coach card they replied to is about, e.g. "the **Dips**
/// card of workout 3f2a… (Push, Mon 16 Nov); its sets now: 1. 8 × 8 kg (set_id …)". None when the
/// message isn't a recorded card linked to a workout.
pub fn card_context(db: &Connection, telegram_message_id: i64) -> Result<Option<String>> {
    let Some(sent) = ChatMessage::first(
        db,
        "WHERE telegram_message_id = ?1 AND role != 'owner' ORDER BY id DESC",
        [telegram_message_id],
    )?
    else {
        return Ok(None);
    };
    let meta = &sent.meta_json.0;
    let Some(workout) = meta["workout_id"].as_str().map(|id| WorkoutSession::get(db, id)).transpose()?.flatten() else {
        return Ok(None);
    };
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let when = workout.started_at.map(|t| format!(", {}", t.local(&tz).strftime("%a %-d %b"))).unwrap_or_default();
    let about =
        format!("workout {} ({}{when})", workout.id, workout.template_snapshot["name"].as_str().unwrap_or("Workout"));
    let exercise = meta["exercise_id"].as_str().map(|id| WorkoutExercise::get(db, id)).transpose()?.flatten();
    let Some(exercise) = exercise else {
        let card = if meta["card"] == "finish" { "finish card" } else { "card" };
        return Ok(Some(format!("the {card} of {about}")));
    };
    let shown = kind(&exercise).in_unit(profile.weight_unit);
    let sets: Vec<String> = logs(db, &exercise.id, None)?
        .iter()
        .map(|s| {
            let set = shown.set(s.weight, s.reps, s.seconds, s.meters);
            let warmup = if s.set_type == SetType::Warmup { "warm-up " } else { "" };
            format!("{}. {warmup}{set} (set_id {})", s.set_number, s.id)
        })
        .collect();
    let sets = if sets.is_empty() { "none logged".to_string() } else { sets.join(", ") };
    Ok(Some(format!("the **{}** card of {about}; its sets now: {sets}", name(&exercise))))
}

/// Next card, or finish + summary once every exercise is resolved.
fn after_change(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Vec<Card>> {
    let workout = workout::load_workout(db, workout_id, None)?;
    if workout.status == WorkoutStatus::WorkoutComplete {
        let finished = audit::finish(db, workout_id, now, &format!("{request_id}:finish"))?;
        return Ok(vec![summary_card(db, &finished["data"], now)?]);
    }
    Ok(vec![exercise_card(db, workout_id, now)?])
}

/// The chat after sets changed outside the coach's flow (`workout::settle` ran): the current card again,
/// the next exercise's card when the coach moved on, or the finish card when that was the last.
/// Nothing once the workout is finished.
pub fn after_fix(db: &Connection, workout_id: &str, now: Ts, request_id: &str) -> Result<Option<CoachView>> {
    if workout::load_workout(db, workout_id, None)?.status == WorkoutStatus::PlanUpdated {
        return Ok(None);
    }
    Ok(Some(acked("✏️ Sets fixed", after_change(db, workout_id, now, request_id)?)))
}

/// Typed text during a workout ("10x40", "25 min", "2 km"). `handled: false` when it isn't a set; the agent gets it.
pub fn handle_text(db: &Connection, text: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    let unit = get_profile(db)?.weight_unit;
    Ok(log_text(db, text, None, unit, now, request_id)?.0)
}

/// `handle_text` for the agent's `workout log`. `warmup`: `Some` is the agent's word for it (the owner
/// said so: 2026-11-18 "warm-up done with 45 kg" became working set 1); `None` (typed by the owner)
/// reads a set lighter than the working target, typed while the warm-up is pending, as the warm-up
/// (2026-11-25: "12x80kg" under an 8 × 55 kg warm-up card became working set 1 of 110 kg).
/// Also returns the logged set as `log_set` reports it (its exercise, type and number) and whether it
/// skipped a pending warm-up, so the agent's reply names where the set really went.
pub fn log_text(
    db: &Connection,
    text: &str,
    warmup: Option<bool>,
    default_unit: crate::db::models::WeightUnit,
    now: Ts,
    request_id: &str,
) -> Result<(CoachView, Value)> {
    let Some(workout) =
        active_workout(db)?.filter(|w| matches!(w.status, WorkoutStatus::SetActive | WorkoutStatus::Resting))
    else {
        return Ok((CoachView::default(), Value::Null));
    };
    let active = exercises(db, &workout.id)?.into_iter().find(|e| e.status == ExerciseStatus::Active);
    let unit = get_profile(db)?.weight_unit;
    let set_kind = active.as_ref().map(|r| kind(r).in_unit(unit)).unwrap_or_default();
    let value = match &active {
        // A time or distance counts on an exercise done for one, or a catalog cardio machine still stored
        // as reps; until its first set either one goes (it decides): on a lift, "5 min" is talk, not a set.
        Some(row) => {
            let measured = measure(row);
            let fresh = working_count(db, &row.id)? == 0;
            match parse_block(text, measured) {
                Some(value) if (measured != Measure::Reps || timed(guide_id(row))) && fresh => Some(value),
                Some(value) if value.measure() == measured => Some(value),
                _ if measured != Measure::Reps => return Ok((CoachView::default(), Value::Null)),
                _ => None,
            }
        }
        None => None,
    };
    let value = match value {
        Some(value) => value,
        None => {
            let reps_alone = active.as_ref().is_some_and(workout::reps_alone);
            // The weight the card offers decides an unclear order.
            let expected = active.as_ref().map(|row| workout::next_working_set(db, row)).transpose()?.flatten();
            match parse_set(text, expected.map(|(weight, _)| weight), reps_alone, default_unit) {
                Ok(value) => value,
                Err(Error::Domain { .. }) => return Ok((CoachView::default(), Value::Null)),
                Err(other) => return Err(other),
            }
        }
    };
    let warmup = match (warmup, &active) {
        (Some(said), _) => said,
        (None, Some(row)) => {
            value.weight > 0.0 && value.weight < row.target_weight && workout::warmup_pending(db, row)?
        }
        (None, None) => false,
    };
    let value = if warmup { SetInput { set_type: SetType::Warmup, ..value } } else { value };
    let result = log_set(db, &workout.id, &value, now, request_id)?;
    let skipped = result["data"]["warmup_skipped"] == true;
    let what = if value.set_type == SetType::Warmup { "Warm-up " } else { "" };
    let mut ack = format!("✅ {what}{}", set_kind.set(value.weight, value.reps, value.seconds, value.meters));
    if skipped {
        ack.push_str(" · warm-up skipped");
    }
    let logged = json!({"logged": result["data"]["logged_set"], "warmup_skipped": skipped});
    Ok((CoachView { react: Some("👍".into()), ..acked(ack, after_change(db, &workout.id, now, request_id)?) }, logged))
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
        return Ok(acked(crowd_ack(rating), vec![]));
    }
    if action == "begin" {
        return begin(db, args.first().copied().unwrap_or_default(), now, request_id);
    }
    let Some(workout) = active_workout(db)? else { return Ok(acked("⌛ No workout running.", vec![])) };
    if action == "end" {
        if let Some(r) = args.first().copied().filter(|r| !r.is_empty())
            && !workout.id.starts_with(r)
        {
            return Ok(acked("⌛ Old button.", vec![]));
        }
        return end_workout(db, &workout, now, request_id);
    }
    let r = args.first().copied().unwrap_or_default();
    let row = if r.is_empty() { None } else { exercises(db, &workout.id)?.into_iter().find(|e| e.id.starts_with(r)) };
    let allowed: &[ExerciseStatus] = match action {
        "free" => &[ExerciseStatus::Deferred],
        "now" => &[ExerciseStatus::Pending, ExerciseStatus::Deferred],
        "drop" | "swap" | "sub" | "wait" => &[ExerciseStatus::Active, ExerciseStatus::Deferred],
        _ => &[ExerciseStatus::Active],
    };
    let Some(row) = row.filter(|row| allowed.contains(&row.status)) else {
        return Ok(acked("⌛ Old button.", vec![exercise_card(db, &workout.id, now)?]));
    };
    let id = workout.id.as_str();
    let arg = |i: usize| args.get(i).copied().ok_or_else(|| invalid("Button is missing an argument"));
    let next = |ack: Option<String>| -> Result<CoachView> {
        Ok(CoachView { ack, cards: after_change(db, id, now, request_id)?, ..handled() })
    };
    let set_kind = kind(&row).in_unit(get_profile(db)?.weight_unit);
    match action {
        "log" => {
            let weight: f64 = arg(1)?.parse().map_err(|_| invalid("Bad weight"))?;
            let reps: i64 = arg(2)?.parse().map_err(|_| invalid("Bad reps"))?;
            log_set(db, id, &SetInput::reps(weight, reps, SetType::Working), now, request_id)?;
            next(Some(format!("✅ {}", set_kind.set(weight, reps, None, None))))
        }
        "time" => {
            let seconds: i64 = arg(1)?.parse().map_err(|_| invalid("Bad seconds"))?;
            log_set(db, id, &SetInput::timed(seconds), now, request_id)?;
            next(Some(format!("✅ {}", set_kind.set(0.0, 0, Some(seconds), None))))
        }
        "dist" => {
            let meters: f64 = arg(1)?.parse().map_err(|_| invalid("Bad distance"))?;
            log_set(db, id, &SetInput::distance(meters), now, request_id)?;
            next(Some(format!("✅ {}", format_distance(meters))))
        }
        "warm" => {
            let target = workout::state(db, id, now)?
                .active_exercise
                .ok_or_else(|| Error::domain("NO_ACTIVE_EXERCISE", "No active exercise"))?;
            log_set(db, id, &SetInput::reps(target.target_weight, target.rep_max, SetType::Warmup), now, request_id)?;
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
        "swap" | "sub"
            if row.status == ExerciseStatus::Active && !logs(db, &row.id, Some(SetType::Working))?.is_empty() =>
        {
            // A set done means the owner is on that machine: finish it there. (A machine taken mid-way is
            // deferred first, and its remaining sets can then be swapped.) Naming an exercise in chat still works.
            Ok(acked(
                format!("Already started {}: finish it or tap Next.", name(&row)),
                vec![exercise_card(db, id, now)?],
            ))
        }
        "swap" => {
            // The card's own buttons become wait/later; options are sent below it. No extra menu message.
            let mut menu = vec![button(format!("↩ Keep {}", name(&row)), &["wait", r])?];
            if row.status == ExerciseStatus::Active
                && exercises(db, id)?.iter().any(|e| e.status == ExerciseStatus::Pending)
            {
                menu.push(button("↪ Do it later", &["later", r])?);
            }
            Ok(CoachView { keep: true, live_buttons: Some(vec![menu]), cards: swap_cards(db, id, &row)?, ..handled() })
        }
        "sub" => {
            let result = adaptation::substitute(db, id, &row.id, arg(1)?, now, request_id)?;
            let replacement_id = result["data"]["replacement_exercise_id"].as_str().unwrap_or_default();
            if let (Some(replacement), Some(template_id)) =
                (WorkoutExercise::get(db, replacement_id)?, &workout.template_id)
            {
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
        "now" => {
            // Out of order: the current exercise keeps its sets (or goes back in the queue if not started).
            adaptation::switch_to(db, id, &row.exercise_id, false, now, request_id)?;
            next(Some(format!("↪ Now {}.", name(&row))))
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
    let options = adaptation::alternatives(db, workout_id, Some(&row.id), 3)?;
    if options.is_empty() {
        return Ok(vec![Card {
            kind: CardKind::Option,
            ..card("No alternatives for this muscle in the catalog.".into(), vec![])
        }]);
    }
    options
        .into_iter()
        .map(|o| {
            let label =
                if o.source == "workout" { format!("↩ Back to {}", o.name) } else { format!("Use {}", o.name) };
            Ok(Card {
                text: format!(
                    "**{}**{}",
                    o.name,
                    o.illustration.as_ref().map(|i| format!(" · {}", i.equipment)).unwrap_or_default()
                ),
                photo: o.illustration.map(|i| i.telegram_png),
                buttons: vec![vec![button(label, &["sub", r, &o.substitute_id])?]],
                kind: CardKind::Option,
                ..Card::default()
            })
        })
        .collect()
}

/// End early: skip what's left, finish, summary card.
pub fn end_workout(db: &Connection, workout: &WorkoutSession, now: Ts, request_id: &str) -> Result<CoachView> {
    for row in exercises(db, &workout.id)? {
        if unresolved(row.status) {
            adaptation::skip_exercise(
                db,
                &workout.id,
                &row.id,
                "ended_early",
                now,
                &format!("{request_id}:skip:{}", row.id),
            )?;
        }
    }
    Ok(CoachView { cards: after_change(db, &workout.id, now, request_id)?, ..handled() })
}

/// Delete a workout with nothing logged (the running one, or `workout_id`) and reopen its session. The
/// live card is retired only when it belongs to that workout. A retry returns the first result.
pub fn discard_workout(db: &Connection, workout_id: Option<&str>, now: Ts, request_id: &str) -> Result<CoachView> {
    let running = active_workout(db)?.map(|w| w.id);
    let result = mutate(db, "workout.discarded", request_id, json!({"workout_id": workout_id}), now, || {
        let Some(id) = workout_id.map(String::from).or_else(|| running.clone()) else {
            crate::bail!("NO_WORKOUT", "No workout running; pass workout_id to discard a finished one");
        };
        let name = workout::load_workout(db, &id, None)?.template_snapshot["name"].clone();
        let session = workout::discard(db, &id, now)?;
        Ok(json!({"workout_id": id, "name": name, "session": session.map(|s| json!({"id": s.id, "status": s.status}))}))
    })?;
    let data = &result["data"];
    let ack = format!("🗑 {} discarded", data["name"].as_str().unwrap_or("Workout"));
    let keep = running.as_deref() != data["workout_id"].as_str();
    Ok(CoachView { ack: Some(ack), keep, ..handled() })
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
    Ok(deleted(after_change(db, &workout.id, now, &request_id)?))
}

/// The `workout` tool's verbs that act on the running workout (documented on `WorkoutAction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Card,
    Swap,
    Later,
    Next,
    End,
    Switch,
    Rest,
}

/// Owner says a logged exercise was really `exercise`. `workout_id` picks any past workout; default
/// is the running workout, else the latest. A running workout gets its next card.
pub fn relabel(
    db: &Connection,
    workout_id: Option<&str>,
    exercise: &str,
    which: Option<&str>,
    now: Ts,
    request_id: &str,
) -> Result<CoachView> {
    let workout = match workout_id {
        Some(id) => WorkoutSession::get(db, id)?,
        None => match active_workout(db)? {
            Some(w) => Some(w),
            None => WorkoutSession::first(db, "ORDER BY started_at DESC", [])?,
        },
    };
    let Some(workout) = workout else { crate::bail!("NO_WORKOUT", "No workout to correct") };
    let result = adaptation::relabel(db, &workout.id, exercise, which, now, request_id)?;
    let label = result["data"]["name"]
        .as_str()
        .or_else(|| result["data"]["active_exercise"]["name"].as_str())
        .unwrap_or(exercise);
    if result["data"]["unchanged"] == true {
        let msg = if workout.status == WorkoutStatus::PlanUpdated {
            format!("Already logged as {label}.")
        } else {
            format!("Already doing {label}.")
        };
        return Ok(CoachView { keep: true, ..acked(msg, vec![]) });
    }
    let ack = format!("✏️ Saved as {label}.");
    if workout.status != WorkoutStatus::PlanUpdated {
        return Ok(acked(ack, after_change(db, &workout.id, now, request_id)?));
    }
    Ok(CoachView { edits: finish_card_edits(db, &workout.id, now)?, ..acked(ack, vec![]) })
}

/// Run a `workout` tool verb. The tool has already checked that Switch has `exercise` and Rest has
/// `seconds`; `remember` makes Rest's seconds the owner's default too.
#[allow(clippy::too_many_arguments)]
pub fn handle_action(
    db: &Connection,
    action: Action,
    exercise: Option<&str>,
    _which: Option<&str>,
    seconds: Option<i64>,
    remember: bool,
    now: Ts,
    request_id: &str,
) -> Result<CoachView> {
    let seconds = seconds.unwrap_or_default();
    if action == Action::Rest && remember {
        update_profile(db, &Map::from_iter([("rest_seconds".to_string(), json!(seconds))]))?;
    }
    let Some(workout) = active_workout(db)? else {
        if action == Action::Rest && remember {
            return Ok(acked(format!("Rest is {seconds}s from now on."), vec![]));
        }
        // Ending what just finished is done, not an error: the last set already ended it and sent the
        // finish card (prod 2026-11-19: a failed end read as "nothing wrapped up" and the reply asked
        // the crowd question a second time).
        if action == Action::End && WorkoutSession::first(db, "WHERE completed_at > ?1", [now.mins(-60)])?.is_some() {
            return Ok(acked("Already finished: its finish card is the wrap-up, crowd question included.", vec![]));
        }
        crate::bail!("NO_ACTIVE_WORKOUT", "No workout running; start one first");
    };
    let tap = match action {
        Action::Card => return Ok(CoachView { cards: after_change(db, &workout.id, now, request_id)?, ..handled() }),
        Action::End => return end_workout(db, &workout, now, request_id),
        Action::Rest => {
            set_rest(db, &workout.id, seconds, now, request_id)?;
            // The live card stays; the next rest uses the new time.
            return Ok(CoachView { keep: true, ..handled() });
        }
        Action::Switch => {
            adaptation::switch_to(db, &workout.id, exercise.unwrap_or_default(), true, now, request_id)?;
            return Ok(deleted(after_change(db, &workout.id, now, request_id)?));
        }
        Action::Swap => "swap",
        Action::Later => "later",
        Action::Next => "next",
    };
    let rows = exercises(db, &workout.id)?;
    let Some(row) = rows
        .iter()
        .find(|e| e.status == ExerciseStatus::Active)
        .or_else(|| rows.iter().find(|e| e.status == ExerciseStatus::Deferred))
    else {
        crate::bail!("NO_ACTIVE_EXERCISE", "Nothing left to act on; end the workout");
    };
    handle_tap(db, &format!("{tap}:{}", reference(&row.id)), now, request_id)
}

/// 'Start workout' on the session reminder: start that planned session's workout and send its first card.
fn begin(db: &Connection, r: &str, now: Ts, request_id: &str) -> Result<CoachView> {
    if let Some(running) = active_workout(db)? {
        return Ok(acked("Already running.", vec![exercise_card(db, &running.id, now)?]));
    }
    let planned = match PlannedSession::first(db, "WHERE substr(id, 1, length(?1)) = ?1", [r])? {
        Some(p) if workout::startable(db, &p, now)? => Some(p),
        _ => None,
    };
    let Some((planned, template_id)) = planned.and_then(|p| p.workout_template_id.clone().map(|t| (p, t))) else {
        return Ok(acked("⌛ This session can't be started anymore.", vec![]));
    };
    let started = start(db, Some(&template_id), Some(&planned.id), true, now, request_id)?;
    let workout_id = started["data"]["workout_id"].as_str().unwrap_or_default();
    Ok(acked("▶️ Started.", vec![exercise_card(db, workout_id, now)?]))
}

/// Session preview as a standalone coach message (the live card stays).
pub fn preview(db: &Connection, now: Ts, planned_session_id: Option<&str>, data_dir: &Path) -> Result<CoachView> {
    let card = crate::domain::preview::session_preview(db, now, planned_session_id, data_dir)?;
    Ok(CoachView { standalone: true, cards: vec![card], ..handled() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{AgentEvent, ExerciseProgression, JobStatus, NotificationJob};
    use crate::db::test::db;
    use crate::domain::templates::{Template, get_template, import_template};
    use crate::domain::workout::tests::{at, spec};

    fn kg(weight: f64) -> audit::SetChange {
        audit::SetChange { weight: Some(weight), ..Default::default() }
    }

    /// Plan order puts shoulders between the two chest exercises, like the first real session.
    fn push(db: &Connection) {
        let template = Template {
            id: "push".into(),
            name: "Push".into(),
            exercises: vec![
                spec(
                    json!({"id": "incline", "name": "Machine incline press", "role": "press", "guide_id": "incline-bench-press", "working_sets": 2, "target_weight": 0, "rest_seconds": 90}),
                ),
                spec(
                    json!({"id": "shoulder", "name": "Shoulder press", "role": "press", "guide_id": "machine-shoulder-press", "working_sets": 2, "target_weight": 25}),
                ),
                spec(
                    json!({"id": "fly", "name": "Cable fly", "role": "fly", "guide_id": "cable-fly", "working_sets": 2, "target_weight": 40}),
                ),
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
        buttons(card)
            .into_iter()
            .find(|(t, _)| t == text)
            .unwrap_or_else(|| panic!("no button {text}: {:?}", buttons(card)))
            .1
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
        assert!(
            first.text.contains("Set 1/2 · 8–10 × **? kg**")
                && first.text.contains("✍️ **Reply reps × weight:** `8x40`"),
            "{}",
            first.text
        );
        assert!(!handle_text(&db, "how long do I rest?", at(5), "chat").unwrap().handled);
        let result = handle_text(&db, "12x90kg", at(10), "msg-1").unwrap();
        assert_eq!((result.react.as_deref(), result.ack.as_deref()), (Some("👍"), Some("✅ 12 × 90 kg")));
        let rest = &result.cards[0];
        assert_eq!((rest.kind, rest.rest_until), (CardKind::Rest, Some(at(100))));
        assert_eq!(rest.text, "until **Machine incline press** set 2/2 · 90 kg");
        assert_eq!(
            buttons(rest).iter().map(|b| b.0.as_str()).collect::<Vec<_>>(),
            ["−30s", "−15s", "+15s", "+30s", "⏭ Skip"]
        );
        // +30s pushes the end back; the countdown card is refreshed in place.
        let longer = handle_tap(&db, &data(rest, "+30s"), at(20), "cb-more").unwrap();
        assert_eq!((longer.refresh, longer.cards[0].rest_until), (true, Some(at(130))));
        let skipped = handle_tap(&db, &data(rest, "⏭ Skip"), at(30), "cb-skip").unwrap();
        let card = &skipped.cards[0];
        assert!(skipped.delete && card.kind == CardKind::Set);
        data(card, "✅ 12 × 90 kg");
        // Already on the machine: no Swap after the first set, not even from an older card.
        assert!(!buttons(card).iter().any(|b| b.0 == "🔄 Swap"));
        let swap = handle_tap(&db, &data(&first, "🔄 Swap"), at(31), "cb-swap").unwrap();
        assert_eq!(swap.ack.as_deref(), Some("Already started Machine incline press: finish it or tap Next."));
    }

    /// A set typed in the Mini App logs like a chat tap and leaves the view for the bot to render; a button
    /// from a card the workout has moved past, or one the app doesn't offer, changes nothing.
    #[test]
    fn app_tap_logs_and_leaves_the_view_for_the_chat() {
        let db = db();
        push(&db);
        let r = &data(&live(&db, at(0)), "🔄 Swap")["swap:".len()..];
        // No weight known yet: the logger starts empty, and the swap options are there before the first set.
        let card = app_live(&db, &active_workout(&db).unwrap().unwrap().id, at(0)).unwrap();
        assert_eq!((card["logger"]["weight"].clone(), card["swap"].is_object()), (Value::Null, true));
        let view = app_tap(&db, &format!("gc:log:{r}:90:12"), at(10), "app-1").unwrap();
        assert_eq!(view.ack.as_deref(), Some("✅ 12 × 90 kg"));
        let events = AgentEvent::select(&db, "WHERE type = ?1", [APP_VIEW]).unwrap();
        let sent: CoachView = serde_json::from_value(events[0].payload_json.0.clone()).unwrap();
        assert_eq!((events.len(), sent), (1, view));
        assert_eq!(app_tap(&db, &format!("gc:wait:{r}"), at(11), "app-2").unwrap_err().code(), "INVALID_INPUT");
        // The next set starts from the one just logged.
        let card = app_live(&db, &active_workout(&db).unwrap().unwrap().id, at(11)).unwrap();
        assert_eq!((card["logger"]["weight"].as_f64(), card["logger"]["reps"].as_i64()), (Some(90.0), Some(12)));
        // Moved on to the next exercise in the chat: the app's old card can't log onto it.
        handle_tap(&db, &format!("gc:next:{r}"), at(12), "cb-next").unwrap();
        assert_eq!(app_tap(&db, &format!("gc:log:{r}:90:12"), at(13), "app-3").unwrap_err().code(), "COACH_STALE");
        assert_eq!(AgentEvent::select(&db, "WHERE type = ?1", [APP_VIEW]).unwrap().len(), 1);
    }

    /// "Do it now" on a queued exercise switches to it; fixing a set redraws the chat's live card from it.
    #[test]
    fn app_now_switches_and_a_fix_redraws_the_chat_card() {
        let db = db();
        push(&db);
        let workout = active_workout(&db).unwrap().unwrap().id;
        let queued = app_live(&db, &workout, at(0)).unwrap()["queue"].as_object().unwrap().clone();
        let shoulder = exercises(&db, &workout).unwrap().into_iter().find(|e| name(e) == "Shoulder press").unwrap();
        app_tap(&db, queued[&shoulder.id].as_str().unwrap(), at(5), "app-now").unwrap();
        assert_eq!(app_live(&db, &workout, at(6)).unwrap()["target"], "8–10 × 25 kg");
        // Done out of order, it keeps its own rest (only a replacement takes over the slot's).
        assert_eq!(WorkoutExercise::get(&db, &shoulder.id).unwrap().unwrap().rest_seconds, shoulder.rest_seconds);
        let r = &data(&live(&db, at(6)), "🔄 Swap")["swap:".len()..];
        app_tap(&db, &format!("gc:log:{r}:25:10"), at(10), "app-log").unwrap();
        let set = app_last(&db, &workout).unwrap()["id"].as_str().unwrap().to_string();
        crate::domain::audit::edit_set(&db, &set, kg(30.0), at(20), "fix").unwrap();
        app_fixed(&db, &set, at(20)).unwrap();
        let views: Vec<CoachView> = AgentEvent::select(&db, "WHERE type = ?1 ORDER BY rowid", [APP_VIEW])
            .unwrap()
            .into_iter()
            .map(|e| serde_json::from_value(e.payload_json.0).unwrap())
            .collect();
        let (note, redraw) = (&views[views.len() - 2], &views[views.len() - 1]);
        assert_eq!(note.ack.as_deref(), Some("✏️ Fixed Shoulder press: 10 × 30 kg"));
        assert!(redraw.refresh && redraw.cards[0].text.contains("30 kg"), "{:?}", redraw.cards);
    }

    #[test]
    fn rest_over_closes_job_once_and_finishing_an_exercise_rests_before_the_next() {
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        assert!(handle_rest_over(&db, at(50)).unwrap().cards.is_empty());
        let over = handle_rest_over(&db, at(101)).unwrap();
        assert!(over.delete && over.cards[0].kind == CardKind::Set);
        assert_eq!(NotificationJob::select(&db, "", []).unwrap()[0].status, JobStatus::Fired);
        // Already closed: the countdown is still replaced, and the job fired once.
        assert!(handle_rest_over(&db, at(102)).unwrap().delete);
        assert_eq!(NotificationJob::select(&db, "", []).unwrap().len(), 1);
        let rest = handle_text(&db, "10x90", at(110), "m2").unwrap().cards.remove(0);
        // The finished exercise's rest (90 s), and the countdown says what comes next.
        assert_eq!((rest.text.as_str(), rest.rest_until), ("until next: **Shoulder press** · 25 kg", Some(at(200))));
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
        assert!(
            swap.keep
                && swap.cards.len() == 3
                && swap.cards.iter().all(|o| o.photo.is_some() && o.kind == CardKind::Option)
        );
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
        assert!(handle_tap(&db, &sub, at(10), "cb-6").unwrap().delete);
        let template = get_template(&db, "push").unwrap();
        assert_eq!((template.exercises[0].id.as_str(), template.exercises[0].working_sets), (slug.as_str(), 2));
        assert_eq!(
            (template.exercises[0].substitutes.clone(), template.alternatives[0].id.as_str()),
            (vec!["incline".to_string()], "incline")
        );
        // Changed their mind: the swapped-away exercise is offered first.
        let again = handle_tap(&db, &data(&live(&db, at(11)), "🔄 Swap"), at(11), "cb-7").unwrap();
        let back = handle_tap(&db, &data(&again.cards[0], "↩ Back to Machine incline press"), at(12), "cb-8").unwrap();
        assert!(back.cards[0].text.starts_with("**Machine incline press**"));
    }

    #[test]
    fn summary_shows_pace_and_lifts_that_went_up() {
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        let first = handle_action(&db, Action::End, None, None, None, false, at(600), "end-1").unwrap().cards.remove(0);
        // First time doing anything: nothing to compare against.
        assert!(
            !first.text.contains("vs last") && !first.text.contains('🏆') && first.effect.is_none(),
            "{}",
            first.text
        );
        start(&db, Some("push"), None, false, at(1000), "start-2").unwrap();
        handle_text(&db, "10x95", at(1010), "m2").unwrap();
        let second =
            handle_action(&db, Action::End, None, None, None, false, at(1240), "end-2").unwrap().cards.remove(0);
        assert!(second.text.contains("\n⚡ 6 min faster · 📈 +6% volume vs last Push\n"), "{}", second.text);
        assert!(
            second.text.contains("\n🏆 **Machine incline press 10 × 95 kg** · heaviest ever (was 90)\n"),
            "{}",
            second.text
        );
        assert_eq!(second.effect.as_deref(), Some(crate::telegram::EFFECT_CONFETTI));
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
        assert!(summary.text.starts_with("🏁 **Push done** · 10 min · 1 set · 0.9 t\n"), "{}", summary.text);
        assert!(
            summary.text.contains("\n>> Machine incline press 90 kg: 10\n>> ~~Shoulder press~~ skipped\n"),
            "{}",
            summary.text
        );
        assert_eq!(
            buttons(&summary).iter().map(|b| b.0.as_str()).collect::<Vec<_>>(),
            ["Empty", "A few", "Busy", "Packed"]
        );
        // Unknown starting weight becomes the logged baseline for next time.
        assert_eq!(ExerciseProgression::get(&db, "incline").unwrap().unwrap().next_weight, 90.0);
        let fixed =
            relabel(&db, None, "incline-dumbbell-press", Some("Machine incline press"), at(700), "fix").unwrap();
        assert_eq!((fixed.ack.as_deref(), fixed.cards.len()), (Some("✏️ Saved as Incline Dumbbell Press."), 0));
        assert!(ExerciseProgression::get(&db, "incline").unwrap().is_none());
        assert_eq!(ExerciseProgression::get(&db, "incline-dumbbell-press").unwrap().unwrap().next_weight, 90.0);
        assert_eq!(get_template(&db, "push").unwrap().exercises[0].id, "incline-dumbbell-press");
        // Renamed movement with no history of its own: the card guesses from the same artwork.
        let template = Template {
            id: "push2".into(),
            exercises: vec![spec(
                json!({"id": "incline-v2", "name": "Incline press", "role": "press", "guide_id": "incline-dumbbell-press", "working_sets": 2, "target_weight": 0}),
            )],
            ..get_template(&db, "push").unwrap()
        };
        import_template(&db, &Template { alternatives: vec![], ..template }).unwrap();
        start(&db, Some("push2"), None, false, at(800), "start-2").unwrap();
        let card = live(&db, at(801));
        assert!(
            card.text.contains("💡 Guess from Incline Dumbbell Press") && card.text.contains("`8x90`"),
            "{}",
            card.text
        );
        data(&card, "✅ 8 × 90 kg?");
    }

    #[test]
    fn card_button_and_typed_sets_agree_on_the_weight() {
        let db = db();
        push(&db);
        // Shoulder press, target 25 kg. Within a workout the last set leads.
        handle_tap(&db, &data(&live(&db, at(0)), "⏭ Skip"), at(1), "cb-skip").unwrap();
        assert!(live(&db, at(2)).text.contains("Set 1/2 · 8–10 × 25 kg"));
        handle_text(&db, "10x8kg", at(10), "m1").unwrap();
        handle_rest_over(&db, at(101)).unwrap();
        let second = live(&db, at(110));
        assert!(second.text.contains("Set 2/2 · 8–10 × 8 kg"), "{}", second.text);
        data(&second, "✅ 10 × 8 kg");
        handle_text(&db, "10x8kg", at(120), "m2").unwrap();
        handle_action(&db, Action::End, None, None, None, false, at(600), "end").unwrap();
        // Next time the target leads, not last time's lighter sets (single >50% drop is guarded), and a typed set reads against it.
        start(&db, Some("push"), None, false, at(1000), "start-2").unwrap();
        handle_tap(&db, &data(&live(&db, at(1001)), "⏭ Skip"), at(1002), "cb-skip-2").unwrap();
        let first = live(&db, at(1003));
        assert!(first.text.contains("Set 1/2 · 8–10 × 25 kg"), "{}", first.text);
        data(&first, "✅ 8 × 25 kg");
        assert_eq!(handle_text(&db, "8x25", at(1010), "m3").unwrap().ack.as_deref(), Some("✅ 8 × 25 kg"));
    }

    /// Prod 2026-11-19: the agent logged the last set and called end in the same step. The set
    /// finished the workout; an end right after must say so, not claim nothing ran.
    #[test]
    fn ending_a_workout_that_just_finished_is_not_an_error() {
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        handle_rest_over(&db, at(101)).unwrap();
        handle_tap(&db, &data(&live(&db, at(110)), "⏭ Next exercise"), at(112), "cb-1").unwrap();
        handle_action(&db, Action::End, None, None, None, false, at(600), "end").unwrap();
        let again = handle_action(&db, Action::End, None, None, None, false, at(610), "end-again").unwrap();
        assert!(again.cards.is_empty() && again.ack.unwrap().starts_with("Already finished"));
        let later = handle_action(&db, Action::End, None, None, None, false, at(600 + 3601), "end-later");
        assert_eq!(later.unwrap_err().code(), "NO_ACTIVE_WORKOUT");
    }

    #[test]
    fn corrected_set_recomputes_next_weight() {
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        handle_rest_over(&db, at(101)).unwrap();
        handle_tap(&db, &data(&live(&db, at(110)), "⏭ Next exercise"), at(112), "cb-1").unwrap();
        handle_action(&db, Action::End, None, None, None, false, at(600), "end").unwrap();
        let set = crate::db::models::SetLog::first(&db, "", []).unwrap().unwrap();
        let fixed = crate::domain::audit::edit_set(&db, &set.id, kg(95.0), at(700), "fix").unwrap();
        assert_eq!(
            (fixed["data"]["before"]["weight"].as_f64(), fixed["data"]["next_weight"].as_f64()),
            (Some(90.0), Some(95.0))
        );
        assert_eq!(ExerciseProgression::get(&db, "incline").unwrap().unwrap().next_weight, 95.0);
    }

    /// The card as the runtime records it once Telegram gave it `message_id`.
    fn sent(db: &Connection, card: &Card, message_id: i64) {
        ChatMessage {
            id: None,
            at: at(0),
            role: crate::db::models::ChatRole::Bot,
            text: card.text.clone(),
            telegram_message_id: Some(message_id),
            meta_json: crate::db::Json(card.meta()),
        }
        .insert(db)
        .unwrap();
    }

    /// 2026-11-16: the owner fixed a dips set the agent corrected, then asked for the cards already sent
    /// to match; the finish card kept the typo.
    #[test]
    fn corrected_set_rewrites_the_finish_card_it_went_out_as() {
        let db = db();
        push(&db);
        handle_text(&db, "8x90", at(10), "m1").unwrap();
        handle_rest_over(&db, at(101)).unwrap();
        let workout_id = active_workout(&db).unwrap().unwrap().id;
        assert!(finish_card_edits(&db, &workout_id, at(105)).unwrap().is_empty(), "nothing to edit while it runs");
        handle_tap(&db, &data(&live(&db, at(110)), "⏭ Next exercise"), at(112), "cb-1").unwrap();
        let summary = handle_action(&db, Action::End, None, None, None, false, at(600), "end").unwrap().cards.remove(0);
        assert!(summary.text.contains("\n>> Machine incline press 90 kg: 8\n"), "{}", summary.text);
        sent(&db, &summary, 500);
        let set = crate::db::models::SetLog::first(&db, "", []).unwrap().unwrap();
        // The agent only calls set_edit; the card edit comes with it.
        let mut cx = crate::tools::Ctx {
            db: &db,
            now: at(700),
            request_id: "fix".into(),
            data_dir: Path::new("/tmp"),
            recipient: None,
            effects: vec![],
        };
        let fixed = crate::tools::call(&mut cx, "set_edit", json!({"set_id": set.id, "reps": 12})).unwrap();
        assert_eq!(fixed["finish_card"], "edited in place; the owner sees it — react, don't restate it");
        let [crate::tools::Effect::Coach(CoachView { edits, .. })] = &cx.effects[..] else {
            panic!("{:?}", cx.effects)
        };
        let [Edit { message_id: 500, card }] = &edits[..] else { panic!("{edits:?}") };
        assert!(card.text.contains("\n>> Machine incline press 90 kg: 12\n"), "{}", card.text);
        assert_eq!(card.buttons, summary.buttons, "crowd question still open");
        let stored = ChatMessage::first(&db, "WHERE telegram_message_id = 500", []).unwrap().unwrap();
        assert_eq!(stored.text, card.text, "the agent's view of the chat shows the corrected sets");

        // Answered since: the card keeps the answer and has no buttons, as the tap left it.
        // Home trainer (0 commute, no gym address) does not get asked how busy the gym was.
        let home = json!({"commute_to_gym_minutes": 0, "gym_address": null}).as_object().unwrap().clone();
        crate::domain::profile::update_profile(&db, &home).unwrap();
        let home_card = summary_card(&db, &audit::report(&db, &workout_id).unwrap(), at(710)).unwrap();
        assert!(!home_card.text.contains("How busy was the gym?"), "{}", home_card.text);
        assert!(home_card.buttons.is_empty());
        let gym = json!({"commute_to_gym_minutes": 20}).as_object().unwrap().clone();
        crate::domain::profile::update_profile(&db, &gym).unwrap();

        db.execute("UPDATE workout_session SET crowd_feedback = 'BUSY'", []).unwrap();
        let card = finish_card_edits(&db, &workout_id, at(710)).unwrap().remove(0).card;
        assert!(card.text.ends_with("How busy was the gym?\nSaved: Busy. Thanks."), "{}", card.text);
        assert!(card.buttons.is_empty());
    }

    #[test]
    fn replied_card_names_its_exercise_and_sets() {
        let db = db();
        push(&db);
        let card = live(&db, at(0));
        sent(&db, &card, 400);
        handle_text(&db, "8x90", at(10), "m1").unwrap();
        let set = crate::db::models::SetLog::first(&db, "", []).unwrap().unwrap();
        let context = card_context(&db, 400).unwrap().unwrap();
        assert!(context.starts_with("the **Machine incline press** card of workout "), "{context}");
        assert!(context.ends_with(&format!("its sets now: 1. 8 × 90 kg (set_id {})", set.id)), "{context}");
        assert_eq!(card_context(&db, 401).unwrap(), None, "not a recorded card");
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
        assert!(over.delete && over.cards[0].kind == CardKind::Set);
        let profile = crate::domain::profile::get_profile(&db).unwrap();
        assert_eq!(profile.rest_seconds, 60);
    }

    #[test]
    fn rest_previews_warmup_unknown_and_bodyweight_targets() {
        for (guide, weight, warmup, expected) in [
            ("bench-press", 80.0, true, "40 kg"),
            ("bench-press", 0.0, false, "? kg"),
            ("crunch", 0.0, false, "Bodyweight"),
        ] {
            let db = db();
            push(&db);
            let workout = active_workout(&db).unwrap().unwrap();
            let mut upcoming = exercises(&db, &workout.id).unwrap().remove(1);
            upcoming.target_weight = weight;
            upcoming.config_json["guide_id"] = json!(guide);
            upcoming.config_json["warmup_required"] = json!(warmup);
            upcoming.update(&db).unwrap();
            handle_text(&db, "10x90", at(10), "m1").unwrap();
            let rest = handle_text(&db, "10x90", at(20), "m2").unwrap().cards.remove(0);
            assert_eq!(rest.text, format!("until next: **Shoulder press** · {expected}"));
        }
    }

    fn plan(db: &Connection, exercises: Vec<Value>) {
        let template = Template {
            id: "plan".into(),
            name: "Plan".into(),
            exercises: exercises.into_iter().map(spec).collect(),
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(db, &template).unwrap();
        start(db, Some("plan"), None, false, at(0), "start").unwrap();
    }

    #[test]
    fn a_superset_alternates_and_rests_after_the_round() {
        let db = db();
        plan(
            &db,
            vec![
                json!({"id": "row", "name": "Cable row", "role": "row", "guide_id": "single-arm-cable-row", "working_sets": 2, "target_weight": 50, "group": "A"}),
                json!({"id": "press", "name": "Shoulder press", "role": "press", "guide_id": "machine-shoulder-press", "working_sets": 2, "target_weight": 30, "group": "A"}),
                json!({"id": "fly", "name": "Cable fly", "role": "fly", "guide_id": "cable-fly", "working_sets": 2, "target_weight": 20}),
            ],
        );
        // Row, straight to press with no rest, then rest before the second round's row.
        let next = handle_text(&db, "10x50", at(10), "m1").unwrap().cards.remove(0);
        assert_eq!((next.kind, next.rest_until), (CardKind::Set, None), "{}", next.text);
        assert!(next.text.starts_with("**Shoulder press**"), "{}", next.text);
        let rest = handle_text(&db, "10x30", at(20), "m2").unwrap().cards.remove(0);
        assert_eq!(rest.kind, CardKind::Rest);
        assert!(rest.text.contains("**Cable row** set 2/2"), "{}", rest.text);
        handle_rest_over(&db, at(200)).unwrap();
        handle_text(&db, "10x50", at(210), "m3").unwrap();
        // The group's last set leads to the next exercise after rest, like any finished exercise.
        let after = handle_text(&db, "10x30", at(220), "m4").unwrap().cards.remove(0);
        assert_eq!(after.kind, CardKind::Rest);
        assert!(after.text.contains("**Cable fly**"), "{}", after.text);
    }

    #[test]
    fn an_assisted_machine_progresses_to_less_help() {
        let db = db();
        plan(
            &db,
            vec![
                json!({"id": "chin", "name": "Assisted chin-up", "role": "pull", "working_sets": 2, "rep_max": 10, "target_weight": 30, "load": "assisted"}),
            ],
        );
        handle_text(&db, "10x30", at(10), "m1").unwrap();
        handle_rest_over(&db, at(200)).unwrap();
        // The last set finishes the workout, which sets the next target.
        handle_text(&db, "10x30", at(210), "m2").unwrap();
        assert_eq!(ExerciseProgression::get(&db, "chin").unwrap().unwrap().next_weight, 27.5);
    }

    /// Progressed to no help: the next workout offers 0 kg assist, not last time's help.
    #[test]
    fn an_assisted_machine_down_to_no_help_offers_none() {
        let db = db();
        plan(
            &db,
            vec![
                json!({"id": "chin", "name": "Assisted chin-up", "role": "pull", "working_sets": 1, "rep_max": 10, "target_weight": 2.5, "load": "assisted"}),
            ],
        );
        handle_text(&db, "10x2.5", at(10), "m1").unwrap();
        assert_eq!(ExerciseProgression::get(&db, "chin").unwrap().unwrap().next_weight, 0.0);
        start(&db, Some("plan"), None, false, at(4000), "again").unwrap();
        let workout = active_workout(&db).unwrap().unwrap();
        let chin = exercises(&db, &workout.id).unwrap().remove(0);
        assert_eq!(crate::domain::workout::next_working_set(&db, &chin).unwrap(), Some((0.0, chin.rep_min)));
    }

    #[test]
    fn exercise_rest_overrides_the_default_rest() {
        use crate::domain::{compression::compress_template, profile::get_profile};
        let db = db();
        push(&db);
        discard_workout(&db, None, at(0), "discard").unwrap();
        handle_action(&db, Action::Rest, None, None, Some(120), true, at(1), "rest").unwrap();
        let mut template = get_template(&db, "push").unwrap();
        template.exercises[0].rest_seconds = Some(180);
        template.exercises[0].primary = true;
        template.exercises[0].target_weight = 80.0;
        template.exercises[0].warmup_weight = Some(40.0);
        template.exercises[1].rest_seconds = Some(60);
        import_template(&db, &template).unwrap();
        let result = start(&db, Some("push"), None, false, at(2), "restart").unwrap();
        let workout = active_workout(&db).unwrap().unwrap();
        let rests: Vec<i64> = exercises(&db, &workout.id).unwrap().iter().map(|row| row.rest_seconds).collect();
        assert_eq!(rests[..3], [180, 60, 120], "an override wins; no override is the owner's one rest");
        let planned = compress_template(&template, &get_profile(&db).unwrap(), 3600).unwrap();
        let eta = Ts::parse(result["data"]["eta"].as_str().unwrap(), None).unwrap();
        assert_eq!(planned["estimated_duration_seconds"].as_i64().unwrap() as f64, (eta - at(2)).as_secs_f64());
        let rest = handle_text(&db, "10x90", at(10), "m1").unwrap().cards.remove(0);
        assert_eq!(rest.rest_until, Some(at(190)));
    }

    #[test]
    fn owner_names_the_exercise_and_the_workout_follows() {
        let db = db();
        push(&db);
        let option = handle_tap(&db, &data(&live(&db, at(0)), "🔄 Swap"), at(1), "cb-1").unwrap().cards.remove(0);
        handle_tap(&db, &buttons(&option)[0].1, at(2), "cb-2").unwrap();
        let back = handle_action(&db, Action::Switch, Some("Machine incline press"), None, None, false, at(3), "sw-1")
            .unwrap();
        assert!(back.cards[0].text.starts_with("**Machine incline press**"));
        assert_eq!(get_template(&db, "push").unwrap().exercises[0].id, "incline");
        let unknown =
            handle_action(&db, Action::Switch, Some("dumbbell rdl"), None, None, false, at(4), "sw-x").unwrap_err();
        assert!(unknown.to_string().contains("dumbbell-romanian-deadlift"), "{unknown}");
        // After a set any exercise still goes; the started one keeps its set and counts as done.
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        let now_on =
            handle_action(&db, Action::Switch, Some("dumbbell-romanian-deadlift"), None, None, false, at(20), "sw-2")
                .unwrap()
                .cards
                .remove(0);
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
            exercises: vec![spec(
                json!({"id": "crunch", "name": "Crunch", "role": "core", "guide_id": "crunch", "working_sets": 1, "rep_min": 12, "rep_max": 15, "target_weight": 0}),
            )],
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

    /// The owner's cardio day: one 25-minute ride on the bike.
    fn cardio(db: &Connection, cycling: Value) {
        let template = Template {
            id: "cardio".into(),
            name: "Cardio".into(),
            exercises: vec![spec(cycling)],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(db, &template).unwrap();
    }

    #[test]
    fn a_timed_exercise_is_logged_in_minutes_and_counts_as_done() {
        let db = db();
        cardio(
            &db,
            json!({"id": "cycling", "name": "Cycling", "role": "conditioning", "guide_id": "cycling", "working_sets": 1, "target_weight": 0, "measure": "time", "minutes": 25}),
        );
        let started = start(&db, Some("cardio"), None, false, at(0), "start").unwrap();
        assert_eq!(started["data"]["eta"], json!(at(1500)), "the ride's 25 min, not one 40 s set");
        let card = live(&db, at(1));
        assert!(card.text.contains("⏱ 25 min") && !card.text.contains("reps"), "{}", card.text);
        data(&card, "✅ 25 min");
        assert!(!handle_text(&db, "10x40", at(5), "m0").unwrap().handled, "reps on a ride go to the agent");
        let done = handle_text(&db, "27m", at(1700), "m1").unwrap();
        assert_eq!(done.ack.as_deref(), Some("✅ 27 min"));
        let summary = &done.cards[0].text;
        assert!(summary.starts_with("🏁 **Cardio done** · 28 min\n⏱ **Cycling** 27 min\n"), "{summary}");
        assert!(summary.contains("\n>> Cycling 27 min\n") && !summary.contains("sets"), "{summary}");
        let row = &exercises(&db, &WorkoutSession::first(&db, "", []).unwrap().unwrap().id).unwrap()[0];
        assert_eq!(row.status, ExerciseStatus::Completed);
        assert!(ExerciseProgression::get(&db, "cycling").unwrap().is_none(), "no next weight for a ride");
    }

    /// 2026-11-12: Cycling was stored as 3 × 8–10 at 0 kg; "25 min" couldn't be logged and ending the
    /// workout marked it skipped. A catalog cardio machine still stored as reps takes a time as its
    /// first set; a lift doesn't take one at all.
    #[test]
    fn a_cardio_machine_stored_as_reps_takes_a_time() {
        let db = db();
        cardio(
            &db,
            json!({"id": "cycling", "name": "Cycling", "role": "conditioning", "guide_id": "cycling", "working_sets": 3, "target_weight": 0}),
        );
        start(&db, Some("cardio"), None, false, at(0), "start").unwrap();
        let logged = handle_action(&db, Action::Card, None, None, None, false, at(1), "card").unwrap();
        assert!(logged.cards[0].text.contains("Set 1/3"), "{}", logged.cards[0].text);
        let done = handle_text(&db, "25 min", at(1500), "m1").unwrap();
        assert!(done.cards[0].text.contains("⏱ **Cycling** 25 min"), "{}", done.cards[0].text);
        let lifting = crate::db::test::db();
        push(&lifting);
        assert!(!handle_text(&lifting, "5 min", at(5), "talk").unwrap().handled);
    }

    /// The fix for the 2026-11-12 workout: the agent adds the ride to the finished workout, and the skipped
    /// exercise counts as done, in minutes. (Ending with nothing logged at all now fails instead.)
    #[test]
    fn a_skipped_ride_is_added_to_the_finished_workout() {
        let db = db();
        let crunch = spec(
            json!({"id": "crunch", "name": "Crunch", "role": "core", "guide_id": "cable-crunch", "working_sets": 1, "target_weight": 40}),
        );
        let cycling = spec(
            json!({"id": "cycling", "name": "Cycling", "role": "conditioning", "guide_id": "cycling", "working_sets": 3, "target_weight": 0}),
        );
        let template = Template {
            id: "cardio".into(),
            name: "Cardio".into(),
            exercises: vec![crunch, cycling],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(&db, &template).unwrap();
        start(&db, Some("cardio"), None, false, at(0), "start").unwrap();
        handle_text(&db, "12x40", at(60), "crunch").unwrap();
        let ended = handle_action(&db, Action::End, None, None, None, false, at(1800), "end").unwrap();
        assert!(ended.cards[0].text.contains("~~Cycling~~ skipped"), "{}", ended.cards[0].text);
        let id = WorkoutSession::first(&db, "", []).unwrap().unwrap().id;
        let fixed =
            audit::add_set(&db, &id, "cycling", "25 min", SetType::Working, at(7200), "fix").unwrap()["data"].clone();
        assert_eq!(
            (fixed["status_before"].as_str(), fixed["status"].as_str(), fixed["seconds"].as_i64()),
            (Some("SKIPPED"), Some("COMPLETED"), Some(1500))
        );
        let row = &exercises(&db, &id).unwrap()[1];
        assert_eq!((measure(row), row.deferred_reason.as_deref()), (Measure::Time, None));
        let h = highlights::workout(&db, &id).unwrap();
        assert_eq!((h.timed, h.sets), (vec![("Cycling".to_string(), 1500)], 1));
        // Logged during the workout, so later history keeps its order.
        assert_eq!(logs(&db, &row.id, None).unwrap()[0].logged_at, at(1800));
        // A second, different kind of set on it is refused; a wrong time is corrected instead.
        let mixed = audit::add_set(&db, &id, "cycling", "10x40", SetType::Working, at(7201), "mixed").unwrap_err();
        assert!(mixed.to_string().contains("logged as a time"), "{mixed}");
        let set_id = fixed["set_id"].as_str().unwrap();
        let corrected = audit::edit_set(
            &db,
            set_id,
            audit::SetChange { seconds: Some(1560), ..Default::default() },
            at(7202),
            "fix-time",
        )
        .unwrap();
        assert_eq!(corrected["data"]["after"]["seconds"], 1560);
        assert_eq!(audit::edit_set(&db, set_id, kg(40.0), at(7203), "kg").unwrap_err().code(), "INVALID_SET");
    }

    fn planned(
        db: &Connection,
        template: &str,
        start: Ts,
        status: crate::db::models::SessionStatus,
    ) -> crate::db::models::PlannedSession {
        let mut plan = crate::db::models::PlannedSession::new("2026-10-12".into(), Some(template.into()), false);
        plan.status = status;
        (plan.planned_start_at, plan.planned_end_at, plan.prep_start_at, plan.leave_home_at) =
            (start, start.mins(60), start.mins(-30), start.mins(-15));
        plan.insert(db).unwrap();
        plan
    }

    /// 2026-11-13: "I'll do push", Push started, then "sorry, I meant pull". The empty Push must not count
    /// as trained: starting Pull replaces it on the same session, and ending an empty workout discards it.
    #[test]
    fn an_empty_workout_never_counts_as_trained() {
        use crate::db::models::{PlannedSession, SessionStatus};
        let db = db();
        push(&db);
        discard_workout(&db, None, at(60), "discard-first").unwrap();
        import_template(
            &db,
            &Template { id: "pull".into(), name: "Pull".into(), ..get_template(&db, "push").unwrap() },
        )
        .unwrap();
        let plan = planned(&db, "push", at(0), SessionStatus::Committed);
        start(&db, None, Some(&plan.id), false, at(120), "start-push").unwrap();
        start(&db, Some("pull"), None, false, at(180), "start-pull").unwrap();
        let running = active_workout(&db).unwrap().unwrap();
        assert_eq!(
            (running.template_id.as_deref(), running.planned_session_id.as_deref()),
            (Some("pull"), Some(plan.id.as_str()))
        );
        assert_eq!(WorkoutSession::select(&db, "", []).unwrap().len(), 1);
        // Nothing logged can't end as done; discarded after its window, the session is missed as if never started.
        let end = handle_action(&db, Action::End, None, None, None, false, at(3601), "end-2").unwrap_err();
        assert_eq!(end.code(), "NOTHING_LOGGED");
        let discarded = discard_workout(&db, None, at(3601), "discard").unwrap();
        assert_eq!(discarded.ack.as_deref(), Some("🗑 Pull discarded"));
        // A retried tap or tool call returns the first result, not NO_WORKOUT.
        assert_eq!(discard_workout(&db, None, at(3602), "discard").unwrap().ack, discarded.ack);
        assert!(WorkoutSession::select(&db, "", []).unwrap().is_empty());
        assert_eq!(PlannedSession::get(&db, &plan.id).unwrap().unwrap().status, SessionStatus::Missed);
        // A workout with a set keeps blocking a new start.
        start(&db, Some("pull"), Some(&plan.id), false, at(3700), "start-3").unwrap();
        handle_text(&db, "10x90", at(3710), "m1").unwrap();
        assert_eq!(
            start(&db, Some("push"), None, false, at(3800), "start-4").unwrap_err().code(),
            "WORKOUT_ALREADY_ACTIVE"
        );
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
        assert_eq!(
            handle_tap(&db, &data(&card, "⏭ Skip"), at(3), "cb-3").unwrap().ack.as_deref(),
            Some("⌛ Old button.")
        );
    }

    #[test]
    fn idle_workout_flags_agent_and_keeps_logged_sets() {
        use crate::domain::weekly::detect_stale_workouts;
        let db = db();
        push(&db);
        handle_text(&db, "10x90", at(10), "m1").unwrap();
        assert!(detect_stale_workouts(&db, at(10 + 2 * 3600)).unwrap().is_empty());
        let id = active_workout(&db).unwrap().unwrap().id;
        assert_eq!(detect_stale_workouts(&db, at(10 + 3 * 3600 + 1)).unwrap(), vec![id.clone()]);
        assert_ne!(
            crate::db::models::WorkoutSession::get(&db, &id).unwrap().unwrap().status,
            WorkoutStatus::PlanUpdated
        );
        assert_eq!(workout::logs(&db, &workout::exercises(&db, &id).unwrap()[0].id, None).unwrap().len(), 1);
        assert!(active_workout(&db).unwrap().is_some());
        assert_eq!(crate::domain::events::wake_pending(&db).unwrap().len(), 1);
        detect_stale_workouts(&db, at(10 + 4 * 3600)).unwrap();
        assert_eq!(crate::domain::events::wake_pending(&db).unwrap().len(), 1);
    }

    #[test]
    fn session_preview_uses_current_plan_local_time_and_one_image() {
        let db = db();
        push(&db);
        let dir = std::env::temp_dir().join(format!("gymclaw-preview-{}", crate::db::new_id()));
        planned(
            &db,
            "push",
            Ts::parse("2026-10-19T08:30:00+00:00", None).unwrap(),
            crate::db::models::SessionStatus::Tentative,
        );
        let card = crate::domain::preview::session_preview(&db, at(0), None, &dir).unwrap();
        assert_eq!(card.text.lines().next(), Some("**Push** · Mon 10:30")); // Europe/Berlin, not UTC
        assert!(
            card.text.contains("1. Machine incline press · 2 sets of 8–10")
                && card.text.contains("3. Cable fly · 2 sets of 8–10"),
            "{}",
            card.text
        );
        let photo = card.photo.clone().unwrap();
        assert_eq!(card.kind, CardKind::Preview);
        assert!(std::path::Path::new(&photo).is_file() && photo.starts_with(dir.to_str().unwrap()));
        assert_eq!(crate::domain::preview::session_preview(&db, at(0), None, &dir).unwrap().photo, card.photo); // cached
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_button_starts_once_without_a_blind_warmup() {
        let db = db();
        push(&db);
        discard_workout(&db, None, at(1), "discard").unwrap();
        let legs = Template {
            id: "legs".into(),
            name: "Legs".into(),
            exercises: vec![spec(
                json!({"id": "press", "name": "Leg press", "role": "legs", "guide_id": "leg-press", "primary": true, "working_sets": 1, "target_weight": 0, "warmup_weight": 0}),
            )],
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
        assert_eq!(
            crate::db::models::PlannedSession::get(&db, &plan.id).unwrap().unwrap().status,
            crate::db::models::SessionStatus::Started
        );
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
        discard_workout(&db, None, at(60), "discard").unwrap();
        let tomorrow = at(86400);
        let plan = planned(&db, "push", tomorrow, crate::db::models::SessionStatus::Committed);
        begin(&db, &plan.id[..8], tomorrow, "begin").unwrap();
        assert_eq!(evidence(&db, tomorrow, &tz).unwrap(), vec![(20.0 * 60.0, 1.0)]); // 18:00 UTC = 20:00 Berlin
    }

    /// Legs as on 2026-11-18: leg press with a warm-up, then two more exercises, two sets each.
    fn legs(db: &Connection) -> String {
        let template = Template {
            id: "legs".into(),
            name: "Legs".into(),
            exercises: vec![
                spec(
                    json!({"id": "press", "name": "Horizontal leg press", "role": "press", "primary": true, "muscle": "Quads", "working_sets": 2, "target_weight": 85, "warmup_weight": 42.5}),
                ),
                spec(
                    json!({"id": "ext", "name": "Leg extension", "role": "accessory", "muscle": "Quads", "working_sets": 2, "target_weight": 75}),
                ),
                spec(
                    json!({"id": "curl", "name": "Lying leg curl", "role": "accessory", "muscle": "Hamstrings", "working_sets": 2, "target_weight": 60}),
                ),
            ],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(db, &template).unwrap();
        start(db, Some("legs"), None, false, at(0), "start").unwrap();
        active_workout(db).unwrap().unwrap().id
    }

    fn agent(db: &Connection, now: Ts, tool: &str, args: Value) -> Result<Value> {
        let request_id = format!("{tool}-{}", now.0.as_second());
        let mut cx =
            crate::tools::Ctx { db, now, request_id, data_dir: Path::new("/tmp"), recipient: None, effects: vec![] };
        crate::tools::call(&mut cx, tool, args)
    }

    /// Each exercise's sets as "W45x8" / "90x8", by exercise name.
    fn sets(db: &Connection, workout_id: &str) -> Vec<(String, Vec<String>)> {
        exercises(db, workout_id)
            .unwrap()
            .iter()
            .map(|e| {
                let sets = logs(db, &e.id, None).unwrap();
                let mut sets: Vec<_> = sets.iter().map(|s| (s.set_type == SetType::Working, s.set_number, s)).collect();
                sets.sort_by_key(|(working, n, _)| (*working, *n));
                let text = |s: &crate::db::models::SetLog| {
                    let w = if s.set_type == SetType::Warmup { "W" } else { "" };
                    format!("{w}{}x{}", s.weight, s.reps)
                };
                (name(e).to_string(), sets.into_iter().map(|(_, _, s)| text(s)).collect())
            })
            .collect()
    }

    /// 2026-11-18: "warm-up done with 45kg" was logged as working set 1, leg press counted itself done one
    /// set early, and every set after it landed one exercise late while the agent said it was fine. Now the
    /// agent can log the warm-up as one, a set goes on the exercise named, and the reply learns where each
    /// set landed.
    #[test]
    fn a_typed_warmup_and_named_sets_land_where_the_owner_said() {
        let db = db();
        let id = legs(&db);
        let warm = agent(
            &db,
            at(10),
            "workout",
            json!({"action": "log", "text": "8x45", "which": "Horizontal leg press", "warmup": true}),
        );
        assert_eq!(warm.unwrap()["logged"]["set_type"], "WARMUP");
        handle_text(&db, "8x90", at(100), "s1").unwrap();
        // The agent names leg press while the coach is still on it: same as the card.
        let second =
            agent(&db, at(200), "workout", json!({"action": "log", "text": "8x95", "which": "Horizontal leg press"}));
        assert_eq!(second.unwrap()["logged"]["exercise"], "Horizontal leg press");
        // The coach moved on to leg extension; an extra leg press set still goes on leg press.
        let extra =
            agent(&db, at(300), "workout", json!({"action": "log", "text": "6x95", "which": "Horizontal leg press"}))
                .unwrap();
        assert_eq!(extra["data"]["exercise"], "Horizontal leg press");
        assert_eq!(workout::active_exercise(&db, &id).unwrap().exercise_id, "ext", "the coach stays where it is");
        // A set for an exercise not started yet goes on it, and the coach finds it done.
        for (i, text) in ["8x60", "7x60"].iter().enumerate() {
            agent(
                &db,
                at(400 + i as i64),
                "workout",
                json!({"action": "log", "text": text, "which": "Lying leg curl"}),
            )
            .unwrap();
        }
        handle_text(&db, "10x75", at(500), "e1").unwrap();
        handle_text(&db, "8x85", at(600), "e2").unwrap();
        assert_eq!(
            sets(&db, &id),
            vec![
                ("Horizontal leg press".into(), vec!["W45x8".into(), "90x8".into(), "95x8".into(), "95x6".into()]),
                ("Leg extension".into(), vec!["75x10".into(), "85x8".into()]),
                ("Lying leg curl".into(), vec!["60x8".into(), "60x7".into()]),
            ]
        );
        assert_eq!(
            workout::load_workout(&db, &id, None).unwrap().status,
            WorkoutStatus::PlanUpdated,
            "the coach finished it"
        );
    }

    /// 2026-11-25: "12x80kg" typed under the warm-up card became working set 1 of a 110 kg press. A set
    /// lighter than the working target, typed while the warm-up is pending, is the warm-up; one at the
    /// working weight still skips it, and so does the agent's explicit log.
    #[test]
    fn a_light_set_typed_on_the_warmup_card_is_the_warmup() {
        let typed = db();
        let id = legs(&typed);
        assert!(handle_text(&typed, "12x60", at(10), "w").unwrap().ack.unwrap().starts_with("✅ Warm-up"));
        handle_text(&typed, "8x85", at(100), "s1").unwrap();
        assert_eq!(sets(&typed, &id)[0].1, vec!["W60x12".to_string(), "85x8".into()]);

        let heavy = db();
        legs(&heavy);
        assert!(handle_text(&heavy, "8x85", at(10), "s1").unwrap().ack.unwrap().ends_with("· warm-up skipped"));

        let said = db();
        let id = legs(&said);
        agent(&said, at(10), "workout", json!({"action": "log", "text": "8x45"})).unwrap();
        assert_eq!(sets(&said, &id)[0].1, vec!["45x8".to_string()], "the agent said working");
    }

    /// A mid-workout fix that leaves the coach where it was still redraws the live card: the rest
    /// countdown after a set re-marked as the warm-up no longer points at "set 2/2".
    #[test]
    fn a_live_fix_redraws_the_current_card() {
        let db = db();
        legs(&db);
        agent(&db, at(10), "workout", json!({"action": "log", "text": "8x45"})).unwrap();
        assert!(live(&db, at(11)).text.contains("set 2/2"));
        let fixed =
            agent(&db, at(20), "set_edit", json!({"which": "Horizontal leg press", "set": 1, "warmup": true})).unwrap();
        assert_eq!(fixed["data"]["coach_moved"], false);
        assert!(fixed["card"].as_str().unwrap().contains("set 1/2"), "{fixed}");
    }

    /// Today's mess fixed after the fact with set_edit alone: a warm-up logged as working, sets on
    /// the wrong exercise, a set that never happened. The rest renumber in the order done.
    #[test]
    fn any_logged_set_can_be_retyped_moved_or_deleted() {
        let db = db();
        let id = legs(&db);
        let unsaid = agent(&db, at(10), "workout", json!({"action": "log", "text": "8x45"})).unwrap();
        assert_eq!(unsaid["warmup_skipped"], true, "the agent hears the warm-up was skipped");
        for (i, text) in ["8x90", "8x95", "10x75", "8x85", "8x85"].iter().enumerate() {
            handle_text(&db, text, at(100 * (i as i64 + 1)), &format!("s{i}")).unwrap();
        }
        let set = |weight: f64, exercise: &str| -> String {
            db.query_row(
                "SELECT s.id FROM set_log s JOIN workout_exercise e ON e.id = s.workout_exercise_id WHERE s.weight = ?1 AND e.exercise_id = ?2 ORDER BY s.logged_at LIMIT 1",
                rusqlite::params![weight, exercise],
                |r| r.get(0),
            )
            .unwrap()
        };
        let (warmup, ninety_five, eighty_five) = (set(45.0, "press"), set(95.0, "ext"), set(85.0, "curl"));
        agent(&db, at(700), "set_edit", json!({"set_id": warmup, "warmup": true})).unwrap();
        agent(&db, at(701), "set_edit", json!({"set_id": ninety_five, "exercise": "Horizontal leg press"})).unwrap();
        let moved =
            agent(&db, at(702), "set_edit", json!({"set_id": eighty_five, "exercise": "leg extension"})).unwrap();
        assert_eq!(moved["data"]["after"]["set_number"], 2);
        let doubled = set(85.0, "curl");
        agent(&db, at(703), "set_edit", json!({"set_id": doubled, "delete": true})).unwrap();
        assert_eq!(
            sets(&db, &id),
            vec![
                ("Horizontal leg press".into(), vec!["W45x8".into(), "90x8".into(), "95x8".into()]),
                ("Leg extension".into(), vec!["75x10".into(), "85x8".into()]),
                ("Lying leg curl".into(), vec![]),
            ]
        );
        let both = agent(&db, at(704), "set_edit", json!({"set_id": warmup, "delete": true, "reps": 5})).unwrap_err();
        assert_eq!(both.code(), "INVALID_ARGUMENT");
    }

    /// The council's live case: sets fixed mid-workout put exercise status right. A working set that skips
    /// the warm-up says so; a set moved onto the current exercise finishes it and the coach moves on with a
    /// new card; an exercise finished early stays finished (the coach doesn't go back) but still takes a
    /// set named for it.
    #[test]
    fn live_fixes_keep_the_coach_in_step() {
        let db = db();
        let id = legs(&db);
        let first = agent(&db, at(10), "workout", json!({"action": "log", "text": "8x45"})).unwrap();
        assert_eq!(
            (first["warmup_skipped"].as_bool(), first["logged"]["exercise"].as_str()),
            (Some(true), Some("Horizontal leg press"))
        );
        assert!(first["ack"].as_str().unwrap().ends_with("· warm-up skipped"), "{first}");
        handle_text(&db, "8x90", at(100), "s1").unwrap();
        let warmup = logs(&db, &exercises(&db, &id).unwrap()[0].id, None).unwrap()[0].id.clone();
        agent(&db, at(150), "set_edit", json!({"set_id": warmup, "warmup": true})).unwrap();
        agent(&db, at(160), "workout", json!({"action": "log", "text": "8x95", "which": "Horizontal leg press"}))
            .unwrap();
        handle_text(&db, "10x75", at(200), "e1").unwrap();
        agent(&db, at(300), "workout", json!({"action": "log", "text": "8x85", "which": "Lying leg curl"})).unwrap();
        let curl = logs(&db, &exercises(&db, &id).unwrap()[2].id, None).unwrap()[0].id.clone();
        let moved = agent(&db, at(310), "set_edit", json!({"set_id": curl, "exercise": "Leg extension"})).unwrap();
        assert_eq!(moved["data"]["coach_moved"], true);
        assert!(moved["card"].as_str().unwrap().contains("Lying leg curl"), "{moved}");
        assert_eq!(workout::active_exercise(&db, &id).unwrap().exercise_id, "curl");
        handle_text(&db, "4x70", at(400), "c1").unwrap();
        handle_text(&db, "7x60", at(500), "c2").unwrap();
        assert_eq!(
            sets(&db, &id),
            vec![
                ("Horizontal leg press".into(), vec!["W45x8".into(), "90x8".into(), "95x8".into()]),
                ("Leg extension".into(), vec!["75x10".into(), "85x8".into()]),
                ("Lying leg curl".into(), vec!["70x4".into(), "60x7".into()]),
            ]
        );
        assert_eq!(workout::load_workout(&db, &id, None).unwrap().status, WorkoutStatus::PlanUpdated);
    }

    #[test]
    fn bodyweight_exercise_steps_down_to_reps_only_card_and_weighted_never_drops_to_zero() {
        let db = db();
        let pull = Template {
            id: "pull".into(),
            name: "Pull".into(),
            exercises: vec![
                spec(
                    json!({"id": "back-extension", "name": "Back extension", "role": "back", "guide_id": "back-extension", "working_sets": 2, "target_weight": 40, "rep_min": 8, "rep_max": 12}),
                ),
                spec(
                    json!({"id": "row", "name": "Seated cable row", "role": "pull", "guide_id": "seated-cable-row", "working_sets": 2, "target_weight": 80, "rep_min": 8, "rep_max": 12}),
                ),
            ],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(&db, &pull).unwrap();

        // Outing 1: bodyweight on back-extension (first >50% drop holds 40), and 0 kg on weighted row.
        start(&db, Some("pull"), None, false, at(0), "s1").unwrap();
        handle_text(&db, "12 reps (no weight)", at(10), "b1a").unwrap();
        handle_text(&db, "12 bw", at(100), "b1b").unwrap();
        handle_text(&db, "12 bw", at(200), "r1a").unwrap();
        handle_text(&db, "12 bw", at(300), "r1b").unwrap();
        assert_eq!(ExerciseProgression::get(&db, "back-extension").unwrap().unwrap().next_weight, 40.0);
        assert_eq!(ExerciseProgression::get(&db, "row").unwrap().unwrap().next_weight, 80.0);

        // Outing 2: second consecutive bodyweight outing -> back-extension steps down to 0 (bodyweight),
        // while weighted row NEVER drops to 0 (which would mean unknown weight).
        start(&db, Some("pull"), None, false, at(1000), "s2").unwrap();
        handle_text(&db, "12 reps (no weight)", at(1010), "b2a").unwrap();
        handle_text(&db, "12 bw", at(1100), "b2b").unwrap();
        handle_text(&db, "12 bw", at(1200), "r2a").unwrap();
        handle_text(&db, "12 bw", at(1300), "r2b").unwrap();
        assert_eq!(ExerciseProgression::get(&db, "back-extension").unwrap().unwrap().next_weight, 0.0);
        assert_eq!(ExerciseProgression::get(&db, "row").unwrap().unwrap().next_weight, 80.0);

        // Outing 3: back-extension card shows Bodyweight (8–12 reps), NOT 40 kg and NOT "? kg" / guess.
        // Also verify mild deload (80 -> 75) followed by a 10x8 typo holds 75 instead of dropping to 8.
        start(&db, Some("pull"), None, false, at(2000), "s3").unwrap();
        let card = live(&db, at(2001));
        assert!(card.text.contains("8–12 reps"), "expected bodyweight card, got: {}", card.text);
        assert!(!card.text.contains("40 kg") && !card.text.contains("? kg"), "{}", card.text);
        handle_text(&db, "12", at(2010), "b3a").unwrap();
        handle_text(&db, "12", at(2100), "b3b").unwrap();
        handle_text(&db, "10x75", at(2200), "r3a").unwrap();
        handle_text(&db, "10x75", at(2300), "r3b").unwrap();
        assert_eq!(ExerciseProgression::get(&db, "row").unwrap().unwrap().next_weight, 75.0);

        start(&db, Some("pull"), None, false, at(3000), "s4").unwrap();
        handle_text(&db, "12", at(3010), "b4a").unwrap();
        handle_text(&db, "12", at(3100), "b4b").unwrap();
        handle_text(&db, "10x8", at(3200), "r4a").unwrap();
        handle_text(&db, "10x8", at(3300), "r4b").unwrap();
        assert_eq!(ExerciseProgression::get(&db, "row").unwrap().unwrap().next_weight, 75.0);
    }
}
