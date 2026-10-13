//! Mid-workout changes: swap, switch, relabel, skip (port of gymclaw/services/adaptation.py). Owner: WP-train.
//!
//! Busy equipment keeps unresolved work durable; substitution needs explicit choice. Swap options come
//! from the template first, then the illustrated catalog (same primary muscle), so every swap still has
//! an image and hits the same muscle group. When the owner names an exercise themselves (`switch_to`,
//! `relabel`), any exercise goes: the plan adapts to what they actually do.
use std::collections::HashSet;

use serde::Serialize;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{ExerciseProgression, ExerciseStatus, SetType, WorkoutExercise, WorkoutSession, WorkoutStatus};
use crate::domain::core::mutate;
use crate::domain::illustrations::{Entry, artwork, catalog, for_exercise, primary_muscle, search, similar};
use crate::domain::templates::{ExerciseSpec, get_template, remember_swap};
use crate::domain::workout::{
    active_exercise, cancel_rest, choose_next, compatible, exercises, guide_id, load_workout, logs, make_exercise, name, snapshot, touch, transition, unresolved, warmup_pending,
    working_count,
};
use crate::error::{Error, Result};
use crate::time::Ts;

fn ensure_running(workout: &WorkoutSession) -> Result<()> {
    if !matches!(workout.status, WorkoutStatus::SetActive | WorkoutStatus::Resting | WorkoutStatus::ExerciseActive) {
        crate::bail!("INVALID_TRANSITION", "Cannot adapt workout in {}", workout.status);
    }
    Ok(())
}

/// A workout exercise by row id, or the active one.
pub fn find_exercise(db: &Connection, workout_id: &str, exercise_id: Option<&str>) -> Result<WorkoutExercise> {
    let Some(exercise_id) = exercise_id else { return active_exercise(db, workout_id) };
    match exercises(db, workout_id)?.into_iter().find(|e| e.id == exercise_id) {
        Some(row) => Ok(row),
        None => crate::bail!("EXERCISE_NOT_FOUND", "Exercise does not belong to workout"),
    }
}

/// Template spec stored on a workout exercise (its config without workout-only keys).
pub fn spec_of(row: &WorkoutExercise) -> Result<ExerciseSpec> {
    const FIELDS: [&str; 16] = [
        "id", "name", "role", "guide_id", "primary", "working_sets", "rep_min", "rep_max", "target_weight", "increment", "rest_seconds", "warmup_weight", "warmup_reps", "priority",
        "requires_completed", "substitutes",
    ];
    let fields: serde_json::Map<String, Value> = row.config_json.as_object().into_iter().flatten().filter(|(k, _)| FIELDS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    serde_json::from_value(Value::Object(fields)).map_err(|e| Error::domain("INVALID_TEMPLATE", format!("Exercise {}: {e}", row.id)))
}

fn substitutes(row: &WorkoutExercise) -> Vec<String> {
    row.config_json.get("substitutes").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
}

pub fn machine_busy(db: &Connection, workout_id: &str, exercise_id: Option<&str>, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.machine_busy", request_id, json!({"workout_id": workout_id, "exercise_id": exercise_id}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        ensure_running(&workout)?;
        let mut exercise = find_exercise(db, workout_id, exercise_id)?;
        if !matches!(exercise.status, ExerciseStatus::Active | ExerciseStatus::Deferred) {
            crate::bail!("INVALID_EXERCISE_STATE", "Only active or deferred equipment can be marked busy");
        }
        let was_active = exercise.status == ExerciseStatus::Active;
        exercise.status = ExerciseStatus::Deferred;
        exercise.deferred_reason = Some("equipment_busy".into());
        exercise.update(db)?;
        workout.waited_for_equipment_count += 1;
        workout.update(db)?;
        if was_active {
            cancel_rest(db, workout_id, now)?;
            transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
            // Stay on the same muscle group while the machine is taken.
            choose_next(db, &mut workout, now, primary_muscle(guide_id(&exercise)).as_deref())?;
        }
        let mut result = touch(db, &mut workout, now)?;
        result["deferred_exercise_id"] = json!(exercise.id);
        result["substitution_options"] = json!(substitutes(&exercise));
        let instruction = if result["active_exercise"].is_null() {
            format!("{} still unresolved. Retry equipment, choose substitution, or explicitly skip.", name(&exercise))
        } else {
            format!("{} deferred. {}. Retry deferred equipment later.", name(&exercise), result["instruction"].as_str().unwrap_or_default())
        };
        result["instruction"] = json!(instruction);
        Ok(result)
    })
}

pub fn machine_free(db: &Connection, workout_id: &str, exercise_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.machine_free", request_id, json!({"workout_id": workout_id, "exercise_id": exercise_id}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        ensure_running(&workout)?;
        let mut exercise = find_exercise(db, workout_id, Some(exercise_id))?;
        if exercise.status != ExerciseStatus::Deferred {
            crate::bail!("INVALID_EXERCISE_STATE", "Equipment is not deferred");
        }
        exercise.status = ExerciseStatus::Pending;
        exercise.deferred_reason = None;
        exercise.update(db)?;
        let mut active = exercises(db, workout_id)?.into_iter().find(|e| e.status == ExerciseStatus::Active);
        if let Some(row) = active.as_mut()
            && logs(db, &row.id, None)?.is_empty()
        {
            // Freed machine wins over an exercise that hasn't started yet.
            row.status = ExerciseStatus::Pending;
            row.update(db)?;
            active = None;
        }
        if active.is_none() {
            exercise.status = ExerciseStatus::Active;
            exercise.update(db)?;
            cancel_rest(db, workout_id, now)?;
            transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
            transition(db, &mut workout, WorkoutStatus::ExerciseActive, now)?;
            transition(db, &mut workout, WorkoutStatus::SetActive, now)?;
        }
        touch(db, &mut workout, now)
    })
}

/// Exercises swapped out before any set was logged. They stay swappable: the owner may change their mind.
fn swapped_away(db: &Connection, workout_id: &str) -> Result<Vec<WorkoutExercise>> {
    let mut rows = vec![];
    for e in exercises(db, workout_id)? {
        if e.status == ExerciseStatus::Substituted && working_count(db, &e.id)? == 0 {
            rows.push(e);
        }
    }
    Ok(rows)
}

/// An exercise swapped away earlier in this workout, a template alternative, or a catalog exercise for
/// the same primary muscle (weights from history).
fn replacement_spec(db: &Connection, workout: &WorkoutSession, original: &WorkoutExercise, substitute_id: &str) -> Result<ExerciseSpec> {
    if let Some(back) = swapped_away(db, &workout.id)?.iter().find(|e| e.exercise_id == substitute_id) {
        return spec_of(back);
    }
    if substitutes(original).iter().any(|s| s == substitute_id)
        && let Some(spec) = snapshot(workout)?.alternatives.into_iter().find(|e| e.id == substitute_id)
    {
        return Ok(spec);
    }
    let item = catalog().get(substitute_id);
    let muscle = primary_muscle(guide_id(original));
    let Some(item) = item.filter(|i| muscle.as_deref() == Some(i.primary_muscle.as_str())) else {
        crate::bail!("INVALID_SUBSTITUTION", "Substitute must be a template alternative or a catalog exercise for the same muscle");
    };
    let progression = ExerciseProgression::get(db, substitute_id)?;
    Ok(ExerciseSpec {
        id: substitute_id.into(),
        name: item.name.clone(),
        guide_id: Some(substitute_id.into()),
        working_sets: original.planned_working_sets,
        rep_min: original.rep_min,
        rep_max: original.rep_max,
        target_weight: progression.map_or(0.0, |p| p.next_weight),
        rest_seconds: Some(original.rest_seconds),
        priority: 0,
        requires_completed: vec![],
        substitutes: vec![],
        // role, primary, increment, warm-up from the original.
        ..spec_of(original)?
    })
}

pub fn substitute(db: &Connection, workout_id: &str, exercise_id: &str, substitute_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    let intent = json!({"workout_id": workout_id, "exercise_id": exercise_id, "substitute_id": substitute_id});
    mutate(db, "workout.substituted", request_id, intent, now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        ensure_running(&workout)?;
        let mut original = find_exercise(db, workout_id, Some(exercise_id))?;
        if !matches!(original.status, ExerciseStatus::Active | ExerciseStatus::Deferred) {
            crate::bail!("INVALID_EXERCISE_STATE", "Only active/deferred exercise can be substituted");
        }
        let spec = replacement_spec(db, &workout, &original, substitute_id)?;
        let remaining = original.planned_working_sets - working_count(db, &original.id)?;
        let needs_warmup = warmup_pending(db, &original)?;
        if needs_warmup && spec.warmup_weight.is_none() {
            crate::bail!("WARMUP_REQUIRED", "Primary replacement needs explicit warm-up weight");
        }
        let away: Vec<String> = swapped_away(db, workout_id)?.into_iter().map(|e| e.id).collect();
        if exercises(db, workout_id)?.iter().any(|e| e.exercise_id == substitute_id && !away.contains(&e.id)) {
            crate::bail!("INVALID_SUBSTITUTION", "Replacement exercise already used in this workout");
        }
        let was_active = original.status == ExerciseStatus::Active;
        let mut replacement = make_exercise(db, workout_id, &spec, original.position, needs_warmup, Some(remaining))?;
        replacement.substituted_from_exercise_id = Some(original.exercise_id.clone());
        replacement.rest_seconds = original.rest_seconds;
        // Neither original nor replacement prerequisites may be bypassed.
        let mut required: Vec<String> = original.config_json.get("requires_completed").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
        required.extend(spec.requires_completed.iter().cloned());
        required.sort();
        required.dedup();
        replacement.config_json["requires_completed"] = json!(required);
        replacement.update(db)?;
        original.status = ExerciseStatus::Substituted;
        original.deferred_reason = Some(format!("substituted_with:{substitute_id}"));
        original.update(db)?;
        let rows = exercises(db, workout_id)?;
        if was_active && compatible(&replacement, &rows) {
            replacement.status = ExerciseStatus::Active;
            replacement.update(db)?;
            cancel_rest(db, workout_id, now)?;
            transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
            transition(db, &mut workout, WorkoutStatus::ExerciseActive, now)?;
            transition(db, &mut workout, WorkoutStatus::SetActive, now)?;
        } else if was_active || !rows.iter().any(|e| e.status == ExerciseStatus::Active) {
            cancel_rest(db, workout_id, now)?;
            transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
            choose_next(db, &mut workout, now, None)?;
        }
        let mut result = touch(db, &mut workout, now)?;
        result["replacement_exercise_id"] = json!(replacement.id);
        result["instruction"] = json!(format!("Swapped to {}. {}", spec.name, result["instruction"].as_str().unwrap_or_default()));
        Ok(result)
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct SwapOption {
    pub substitute_id: String,
    pub name: String,
    /// "workout" (swapped away earlier: back to it), "template" or "catalog".
    pub source: &'static str,
    pub illustration: Option<Entry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Alternatives {
    pub exercise_id: String,
    pub name: String,
    pub options: Vec<SwapOption>,
}

/// Read-only swap options: exercises swapped away from this slot first ("back to A"), then saved template
/// alternatives, then same-muscle catalog exercises. Never the exercise being swapped (or the same movement
/// under another name) or one still in the workout.
pub fn alternatives(db: &Connection, workout_id: &str, exercise_id: Option<&str>, limit: usize) -> Result<Alternatives> {
    let workout = load_workout(db, workout_id, None)?;
    let exercise = find_exercise(db, workout_id, exercise_id)?;
    let earlier = swapped_away(db, workout_id)?;
    let rows: Vec<WorkoutExercise> = exercises(db, workout_id)?.into_iter().filter(|e| !earlier.iter().any(|x| x.id == e.id)).collect();
    let mut used: HashSet<String> = rows.iter().flat_map(|e| std::iter::once(e.exercise_id.clone()).chain(guide_id(e).map(String::from))).collect();
    let pictures: HashSet<String> = used.iter().filter(|s| catalog().contains_key(*s)).map(|s| artwork(s)).collect();
    // exercise_id → row, first-seen order (a later duplicate replaces the row, like a dict).
    let mut back: Vec<(String, &WorkoutExercise)> = vec![];
    for e in earlier.iter().filter(|e| e.position == exercise.position && !used.contains(&e.exercise_id)) {
        match back.iter_mut().find(|(k, _)| *k == e.exercise_id) {
            Some(slot) => slot.1 = e,
            None => back.push((e.exercise_id.clone(), e)),
        }
    }
    let mut options: Vec<SwapOption> = back
        .iter()
        .map(|(_, e)| SwapOption { substitute_id: e.exercise_id.clone(), name: name(e).into(), source: "workout", illustration: for_exercise(name(e), guide_id(e)) })
        .collect();
    used.extend(back.iter().filter_map(|(_, e)| guide_id(e).map(String::from)));
    let allowed = substitutes(&exercise);
    for spec in snapshot(&workout)?.alternatives {
        let guide = spec.guide_id.as_deref();
        let taken = guide.is_some_and(|g| used.contains(g) || pictures.contains(&artwork(g)));
        if allowed.contains(&spec.id) && !used.contains(&spec.id) && !taken {
            options.push(SwapOption { substitute_id: spec.id.clone(), name: spec.name.clone(), source: "template", illustration: for_exercise(&spec.name, guide) });
        }
    }
    // Exercises the owner has logged before rank first among catalog options.
    let mut stmt = db.prepare_cached("SELECT DISTINCT e.exercise_id FROM workout_exercise e JOIN set_log s ON s.workout_exercise_id = e.id")?;
    let history: HashSet<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut exclude = used.clone();
    exclude.extend(back.iter().map(|(k, _)| k.clone()));
    exclude.extend(options.iter().filter_map(|o| o.illustration.as_ref().map(|i| i.guide_id.clone())));
    if let Some(guide) = guide_id(&exercise) {
        for item in similar(guide, &exclude, &history, limit) {
            options.push(SwapOption { substitute_id: item.guide_id.clone(), name: item.name.clone(), source: "catalog", illustration: Some(item) });
        }
    }
    options.truncate(limit);
    Ok(Alternatives { exercise_id: exercise.id.clone(), name: name(&exercise).into(), options })
}

/// Owner moves on: completes the active exercise with the sets done so far, or skips it if none.
pub fn next_exercise(db: &Connection, workout_id: &str, exercise_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.exercise_ended", request_id, json!({"workout_id": workout_id, "exercise_id": exercise_id}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        ensure_running(&workout)?;
        let mut exercise = find_exercise(db, workout_id, Some(exercise_id))?;
        if exercise.status != ExerciseStatus::Active {
            crate::bail!("INVALID_EXERCISE_STATE", "Only the active exercise can be ended early");
        }
        let done = working_count(db, &exercise.id)? > 0;
        exercise.status = if done { ExerciseStatus::Completed } else { ExerciseStatus::Skipped };
        exercise.deferred_reason = if done { None } else { Some("owner_skipped".into()) };
        exercise.update(db)?;
        cancel_rest(db, workout_id, now)?;
        transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
        choose_next(db, &mut workout, now, None)?;
        touch(db, &mut workout, now)
    })
}

/// Skip one workout exercise (row id) with a reason. Mutation result envelope.
pub fn skip_exercise(db: &Connection, workout_id: &str, exercise_row_id: &str, reason: &str, now: Ts, request_id: &str) -> Result<Value> {
    if reason.trim().is_empty() {
        crate::bail!("REASON_REQUIRED", "Explicit skip requires reason");
    }
    let intent = json!({"workout_id": workout_id, "exercise_id": exercise_row_id, "reason": reason});
    mutate(db, "workout.exercise_skipped", request_id, intent, now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        ensure_running(&workout)?;
        let mut exercise = find_exercise(db, workout_id, Some(exercise_row_id))?;
        if !unresolved(exercise.status) {
            crate::bail!("INVALID_EXERCISE_STATE", "Exercise is already resolved");
        }
        let was_active = exercise.status == ExerciseStatus::Active;
        exercise.status = ExerciseStatus::Skipped;
        exercise.deferred_reason = Some(reason.into());
        exercise.update(db)?;
        if was_active || !exercises(db, workout_id)?.iter().any(|e| e.status == ExerciseStatus::Active) {
            cancel_rest(db, workout_id, now)?;
            transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
            choose_next(db, &mut workout, now, None)?;
        }
        touch(db, &mut workout, now)
    })
}

/// Owner's exercise → (planned row still to do, its spec) or (None, spec to add). Exact ID, guide ID or
/// name (case-insensitive) from this workout, its template, then the catalog. Anything else fails with
/// catalog suggestions, so the agent picks one instead of guessing.
fn resolve(db: &Connection, workout: &WorkoutSession, query: &str, current: Option<&WorkoutExercise>) -> Result<(Option<WorkoutExercise>, ExerciseSpec)> {
    let key = query.trim().to_lowercase();
    let same = |names: &[Option<&str>]| names.iter().flatten().any(|n| n.to_lowercase() == key);
    let rows = exercises(db, &workout.id)?;
    for row in &rows {
        if matches!(row.status, ExerciseStatus::Pending | ExerciseStatus::Deferred) && same(&[Some(&row.exercise_id), Some(name(row)), guide_id(row)]) {
            return Ok((Some(row.clone()), spec_of(row)?));
        }
    }
    let template = snapshot(workout)?;
    let mut known: Vec<ExerciseSpec> = template.exercises.into_iter().chain(template.alternatives).collect();
    for row in &rows {
        known.push(spec_of(row)?);
    }
    if let Some(spec) = known.into_iter().find(|s| same(&[Some(&s.id), Some(&s.name), s.guide_id.as_deref()])) {
        return Ok((None, spec));
    }
    let item = catalog().get(&key).or_else(|| catalog().values().find(|i| i.name.to_lowercase() == key));
    let Some(item) = item else {
        let options: Vec<String> = search(query, None, None, 5).iter().map(|o| format!("{} ({})", o.guide_id, o.name)).collect();
        let options = if options.is_empty() { "try catalog search".into() } else { options.join(", ") };
        crate::bail!("EXERCISE_UNKNOWN", "No exact match for '{query}'. Pass one guide_id: {options}");
    };
    let base = |k: &str| current.and_then(|c| c.config_json.get(k).cloned());
    let spec = json!({
        "id": item.slug, "name": item.name, "role": base("role").unwrap_or(json!("accessory")), "guide_id": item.slug,
        "working_sets": current.map_or(3, |c| c.planned_working_sets), "rep_min": base("rep_min").unwrap_or(json!(8)),
        "rep_max": base("rep_max").unwrap_or(json!(12)), "target_weight": 0, "increment": base("increment").unwrap_or(json!(2.5)),
    });
    Ok((None, serde_json::from_value(spec)?))
}

/// Owner says what they're doing now. Any exercise, any time: the current one is finished with the
/// sets done so far, replaced if not started (and, if `remember`, the template keeps the replacement),
/// or put back in the queue if the owner just wants another planned exercise first.
pub fn switch_to(db: &Connection, workout_id: &str, query: &str, remember: bool, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.switched", request_id, json!({"workout_id": workout_id, "to": query}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        ensure_running(&workout)?;
        let mut current = exercises(db, workout_id)?.into_iter().find(|e| e.status == ExerciseStatus::Active);
        let (row, spec) = resolve(db, &workout, query, current.as_ref())?;
        let mut replaced: Option<String> = None;
        if current.as_ref().is_some_and(|c| c.exercise_id == spec.id) {
            let mut result = touch(db, &mut workout, now)?;
            result["replaced_exercise_id"] = Value::Null;
            return Ok(result);
        }
        let started = match &current {
            Some(c) => working_count(db, &c.id)? > 0,
            None => false,
        };
        if let Some(c) = current.as_mut() {
            if started {
                c.status = ExerciseStatus::Completed;
            } else if row.is_none() {
                c.status = ExerciseStatus::Substituted;
                c.deferred_reason = Some(format!("substituted_with:{}", spec.id));
                replaced = Some(c.exercise_id.clone());
            } else {
                c.status = ExerciseStatus::Pending;
            }
            c.update(db)?;
        }
        let mut row = match row {
            Some(row) => row,
            None => {
                let position = match &current {
                    Some(c) => c.position,
                    None => exercises(db, workout_id)?.iter().map(|e| e.position).max().unwrap_or(-1) + 1,
                };
                let sets = current.as_ref().filter(|_| !started).map(|c| c.planned_working_sets);
                let mut row = make_exercise(db, workout_id, &spec, position, false, sets)?;
                if replaced.is_some() {
                    row.substituted_from_exercise_id = current.as_ref().map(|c| c.exercise_id.clone());
                }
                row
            }
        };
        if let Some(c) = &current {
            row.rest_seconds = c.rest_seconds;
        }
        row.status = ExerciseStatus::Active;
        row.deferred_reason = None;
        row.update(db)?;
        cancel_rest(db, workout_id, now)?;
        transition(db, &mut workout, WorkoutStatus::NextExercise, now)?;
        transition(db, &mut workout, WorkoutStatus::ExerciseActive, now)?;
        transition(db, &mut workout, WorkoutStatus::SetActive, now)?;
        if remember
            && let (Some(original), Some(template_id)) = (&replaced, &workout.template_id)
        {
            remember_swap(db, template_id, original, &spec)?;
        }
        let mut result = touch(db, &mut workout, now)?;
        result["replaced_exercise_id"] = json!(replaced);
        Ok(result)
    })
}

/// Owner did a different exercise than the one logged ("that was dumbbell RDL, not single-leg").
/// Keeps the sets, renames the exercise, moves this workout's progression to it and puts it in the
/// template where the logged one was. Works on finished workouts too. `which` picks the logged
/// exercise (ID, guide ID or name); default is the latest exercise with sets.
pub fn relabel(db: &Connection, workout_id: &str, query: &str, which: Option<&str>, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "workout.relabeled", request_id, json!({"workout_id": workout_id, "to": query, "which": which}), now, || {
        let mut workout = load_workout(db, workout_id, Some(now))?;
        let mut rows = vec![];
        for e in exercises(db, workout_id)? {
            if let Some(last) = logs(db, &e.id, Some(SetType::Working))?.pop() {
                rows.push((e, last.logged_at));
            }
        }
        let key = which.unwrap_or_default().trim().to_lowercase();
        let row = if key.is_empty() {
            // Latest last set; the first one on ties.
            rows.into_iter().reduce(|best, next| if next.1 > best.1 { next } else { best }).map(|(e, _)| e)
        } else {
            rows.into_iter().map(|(e, _)| e).find(|e| {
                [e.id.chars().take(8).collect::<String>(), e.exercise_id.to_lowercase(), name(e).to_lowercase(), guide_id(e).unwrap_or_default().to_lowercase()].contains(&key)
            })
        };
        let Some(mut row) = row else {
            crate::bail!("EXERCISE_NOT_FOUND", "No logged exercise matches; pass which= with its name");
        };
        let (_, spec) = resolve(db, &workout, query, Some(&row))?;
        let old = row.exercise_id.clone();
        if spec.id == old {
            return Ok(json!({"workout_id": workout.id, "exercise_id": row.id, "from": old, "to": old}));
        }
        row.exercise_id = spec.id.clone();
        if let Some(config) = row.config_json.as_object_mut() {
            config.insert("id".into(), json!(spec.id));
            config.insert("name".into(), json!(spec.name));
            config.insert("guide_id".into(), json!(spec.guide_id));
        }
        row.update(db)?;
        if let Some(progression) = ExerciseProgression::get(db, &old)?.filter(|p| p.source_workout_id == workout.id) {
            let moved = ExerciseProgression { exercise_id: spec.id.clone(), next_weight: progression.next_weight, updated_at: now, source_workout_id: workout.id.clone() };
            if ExerciseProgression::get(db, &spec.id)?.is_some() { moved.update(db)? } else { moved.insert(db)? }
            progression.delete(db)?;
        }
        if let Some(template_id) = &workout.template_id
            && get_template(db, template_id)?.exercises.iter().any(|e| e.id == old)
        {
            remember_swap(db, template_id, &old, &spec)?;
        }
        if workout.status != WorkoutStatus::PlanUpdated {
            workout.last_action_at = Some(now);
            workout.update(db)?;
        }
        Ok(json!({"workout_id": workout.id, "exercise_id": row.id, "from": old, "to": spec.id, "name": spec.name}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{ExerciseProgression, NotificationJob};
    use crate::domain::audit::{finish, get_audit};
    use crate::domain::set_parser::SetInput;
    use crate::domain::templates::{Template, get_template, import_template};
    use crate::domain::workout::tests::{at, db, spec};
    use crate::domain::workout::{current, log_set, start};

    fn adapt(db: &Connection) {
        let template = Template {
            id: "adapt".into(),
            name: "Adapt".into(),
            exercises: vec![
                spec(json!({"id": "bench", "name": "Bench", "role": "press", "primary": true, "working_sets": 1, "target_weight": 80, "warmup_weight": 50})),
                spec(json!({"id": "fly", "name": "Fly", "role": "fly", "working_sets": 2, "target_weight": 50, "rep_max": 12, "substitutes": ["db-fly"]})),
                spec(json!({"id": "raise", "name": "Raise", "role": "delts", "working_sets": 1, "target_weight": 10, "increment": 1})),
            ],
            alternatives: vec![spec(json!({"id": "db-fly", "name": "DB Fly", "role": "fly", "working_sets": 2, "target_weight": 12, "rep_max": 12, "increment": 1}))],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(db, &template).unwrap();
    }

    fn set(weight: f64, reps: i64) -> SetInput {
        SetInput { weight, reps, set_type: SetType::Working, rir: None }
    }

    fn bench_done(db: &Connection) -> String {
        adapt(db);
        let id = start(db, Some("adapt"), None, false, at(0), "start").unwrap()["data"]["workout_id"].as_str().unwrap().to_string();
        log_set(db, &id, &SetInput { set_type: SetType::Warmup, ..set(50.0, 8) }, at(0), "warmup").unwrap();
        log_set(db, &id, &set(80.0, 10), at(40), "bench").unwrap();
        id
    }

    fn progression(db: &Connection, id: &str) -> Option<f64> {
        ExerciseProgression::get(db, id).unwrap().map(|p| p.next_weight)
    }

    #[test]
    fn reorder_retry_deferred_and_audit() {
        let db = db();
        let id = bench_done(&db);
        let result = machine_busy(&db, &id, None, at(60), "busy").unwrap();
        let deferred = result["data"]["deferred_exercise_id"].as_str().unwrap().to_string();
        assert_eq!(result["data"]["active_exercise"]["exercise_id"], "raise");
        assert_eq!(result["data"]["substitution_options"], json!(["db-fly"]));
        assert_eq!(WorkoutExercise::get(&db, &deferred).unwrap().unwrap().status, ExerciseStatus::Deferred);
        // Finishing an exercise starts the next one without a rest timer.
        assert!(NotificationJob::first(&db, "WHERE status = 'PENDING'", []).unwrap().is_none());
        log_set(&db, &id, &set(10.0, 10), at(100), "raise").unwrap();
        assert_eq!(finish(&db, &id, at(110), "bad-finish").unwrap_err().code(), "UNRESOLVED_EXERCISES");
        let result = machine_free(&db, &id, &deferred, at(120), "free").unwrap();
        assert_eq!(result["data"]["active_exercise"]["exercise_id"], "fly");
        for n in 0..2 {
            log_set(&db, &id, &set(50.0, 12), at(160 + 120 * n), &format!("fly-{n}")).unwrap();
        }
        let result = finish(&db, &id, at(300), "finish").unwrap();
        let data = &result["data"];
        assert_eq!((data["planned_sets"].as_i64(), data["completed_sets"].as_i64(), data["warmup_sets"].as_i64()), (Some(4), Some(4), Some(1)));
        assert_eq!((data["equipment_waits"].as_i64(), data["progression_count"].as_i64()), (Some(1), Some(3)));
        assert_eq!((progression(&db, "bench"), progression(&db, "fly")), (Some(82.5), Some(52.5)));
        assert_eq!(finish(&db, &id, at(310), "finish").unwrap(), result);
        assert_eq!(get_audit(&db, &id).unwrap(), result["data"]);
        let second = start(&db, Some("adapt"), None, false, at(2 * 86400), "next").unwrap();
        assert_eq!(second["data"]["active_exercise"]["target_weight"], 50.0); // warm-up unchanged
        let row = second["data"]["active_exercise"]["id"].as_str().unwrap();
        assert_eq!(WorkoutExercise::get(&db, row).unwrap().unwrap().target_weight, 82.5);
        let replans: i64 = db.query_row("SELECT count(*) FROM agent_event WHERE type = 'planning.replan_required'", [], |r| r.get(0)).unwrap();
        assert_eq!(replans, 1);
    }

    #[test]
    fn substitute_only_remaining_volume_and_hold_partial_target() {
        let db = db();
        let id = bench_done(&db);
        let fly = log_set(&db, &id, &set(50.0, 12), at(100), "fly-1").unwrap()["data"]["active_exercise"]["id"].as_str().unwrap().to_string();
        assert_eq!(machine_busy(&db, &id, None, at(120), "busy").unwrap()["data"]["active_exercise"]["exercise_id"], "raise");
        let replacement = substitute(&db, &id, &fly, "db-fly", at(130), "swap").unwrap()["data"]["replacement_exercise_id"].as_str().unwrap().to_string();
        assert_eq!(WorkoutExercise::get(&db, &replacement).unwrap().unwrap().planned_working_sets, 1);
        log_set(&db, &id, &set(10.0, 10), at(160), "raise").unwrap();
        assert_eq!(current(&db, &id, at(160)).unwrap()["active_exercise"]["exercise_id"], "db-fly");
        log_set(&db, &id, &set(12.0, 12), at(200), "db-fly").unwrap();
        let audit = finish(&db, &id, at(210), "finish").unwrap()["data"].clone();
        assert_eq!((audit["planned_sets"].as_i64(), audit["completed_sets"].as_i64(), audit["substitutions"].as_i64()), (Some(4), Some(4), Some(1)));
        assert_eq!((progression(&db, "db-fly"), progression(&db, "fly")), (Some(12.0), None));
    }

    #[test]
    fn dependency_blocks_unsafe_reordering_and_explicit_skips() {
        let db = db();
        let a = spec(json!({"id": "a", "name": "A", "role": "press", "target_weight": 10, "working_sets": 1}));
        let b = spec(json!({"id": "b", "name": "B", "role": "press", "target_weight": 10, "working_sets": 1, "requires_completed": ["a"]}));
        import_template(&db, &Template { id: "dependent".into(), name: "Dependent".into(), exercises: vec![a, b], alternatives: vec![], set_duration_seconds: 40, transition_seconds: 60, source: "user".into() }).unwrap();
        let id = start(&db, Some("dependent"), None, false, at(0), "dependent").unwrap()["data"]["workout_id"].as_str().unwrap().to_string();
        let blocked = machine_busy(&db, &id, None, at(0), "busy").unwrap()["data"].clone();
        assert!(blocked["active_exercise"].is_null());
        let queue = blocked["queue"].as_array().unwrap().clone();
        let skipped = skip_exercise(&db, &id, queue[0]["id"].as_str().unwrap(), "equipment unavailable", at(0), "skip-a").unwrap();
        assert!(skipped["data"]["active_exercise"].is_null());
        skip_exercise(&db, &id, queue[1]["id"].as_str().unwrap(), "dependency skipped", at(0), "skip-b").unwrap();
        let result = finish(&db, &id, at(0), "finish").unwrap()["data"].clone();
        assert_eq!((result["skips"].as_i64(), result["completed_sets"].as_i64()), (Some(2), Some(0)));
        assert!(ExerciseProgression::select(&db, "", []).unwrap().is_empty());
    }

    #[test]
    fn eta_includes_deferred_volume_and_elapsed_time() {
        let db = db();
        let id = bench_done(&db);
        let blocked = machine_busy(&db, &id, None, at(60), "busy").unwrap()["data"].clone();
        assert!(blocked["queue"].as_array().unwrap().iter().any(|e| e["exercise_id"] == "fly" && e["remaining_sets"] == 2));
        // Cancelling rest may improve ETA; unresolved sets still count, not disappear.
        let eta = Ts::parse(blocked["eta"].as_str().unwrap(), None).unwrap();
        assert!((eta - at(60)).as_secs() >= 3 * 40);
        let later = Ts::parse(current(&db, &id, at(120)).unwrap()["eta"].as_str().unwrap(), None).unwrap();
        assert!(later > eta);
    }

    #[test]
    fn replacement_prerequisites_are_preserved() {
        let db = db();
        adapt(&db);
        let mut template = get_template(&db, "adapt").unwrap();
        template.alternatives[0].requires_completed = vec!["raise".into()];
        import_template(&db, &template).unwrap();
        let id = start(&db, Some("adapt"), None, false, at(0), "start").unwrap()["data"]["workout_id"].as_str().unwrap().to_string();
        log_set(&db, &id, &SetInput { set_type: SetType::Warmup, ..set(50.0, 8) }, at(0), "warmup").unwrap();
        log_set(&db, &id, &set(80.0, 10), at(40), "bench").unwrap();
        let fly = current(&db, &id, at(40)).unwrap()["active_exercise"]["id"].as_str().unwrap().to_string();
        let result = substitute(&db, &id, &fly, "db-fly", at(50), "swap").unwrap()["data"].clone();
        assert_eq!(result["active_exercise"]["exercise_id"], "raise");
        let replacement = WorkoutExercise::get(&db, result["replacement_exercise_id"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(replacement.status, ExerciseStatus::Pending);
        assert_eq!(replacement.config_json["requires_completed"], json!(["raise"]));
    }

    #[test]
    fn invalid_substitution_preserves_state() {
        let db = db();
        let id = bench_done(&db);
        let fly = current(&db, &id, at(40)).unwrap()["active_exercise"]["id"].as_str().unwrap().to_string();
        let tx = db.unchecked_transaction().unwrap();
        let error = substitute(&tx, &id, &fly, "bench", at(50), "bad").unwrap_err();
        assert!(error.to_string().contains("template alternative or a catalog exercise"), "{error}");
        drop(tx);
        assert_eq!(WorkoutExercise::get(&db, &fly).unwrap().unwrap().status, ExerciseStatus::Active);
    }
}
