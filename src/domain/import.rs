//! History from other apps, imported as finished workouts so next weights, records and trends start
//! from real numbers. Strong and Hevy CSV exports are parsed here; screenshots and pasted logs are
//! read by the agent and arrive as `PastWorkout`s. Both go through `import`. `export_csv` writes
//! everything back out in Strong's format, which Hevy and others import (and so does `import`).
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use jiff::SignedDuration;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::models::{
    ExerciseProgression, ExerciseStatus, SetLog, SetType, WorkoutExercise, WorkoutSession, WorkoutStatus,
};
use crate::db::{Connection, Json, new_id};
use crate::domain::backfill::PastSet;
use crate::domain::core::mutate;
use crate::domain::illustrations::{lookup, matching, suggestions};
use crate::domain::profile::get_profile;
use crate::domain::templates::{ExerciseSpec, Load, default_load, get_template, pick, template_ids};
use crate::domain::workout::in_seconds;
use crate::error::Result;
use crate::time::Ts;

/// A workout the owner did before GymClaw, as the agent read it from a screenshot or pasted text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PastWorkout {
    /// Start, owner-local, e.g. "2026-03-02T18:30".
    pub at: String,
    pub title: Option<String>,
    /// Exercise as the source names it (map it with `exercises`).
    pub sets: Vec<PastSet>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    #[default]
    Kg,
    Lb,
}

impl Unit {
    fn kg(self, weight: f64) -> f64 {
        match self {
            Unit::Kg => weight,
            Unit::Lb => (weight * 0.453_592_37 * 100.0).round() / 100.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Strong,
    Hevy,
    /// Workouts the agent passed.
    Chat,
}

#[derive(Clone, Debug, PartialEq)]
struct Set {
    exercise: String,
    kind: SetType,
    /// kg
    weight: f64,
    reps: i64,
    seconds: Option<i64>,
    meters: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workout {
    at: Ts,
    title: String,
    seconds: Option<i64>,
    sets: Vec<Set>,
}

/// Parsed workouts in the source's order, weights in kg.
#[derive(Clone, Debug, PartialEq)]
pub struct History {
    pub format: Format,
    pub workouts: Vec<Workout>,
    /// Strong doesn't say whether its weights are kg or lb.
    unit_unknown: bool,
}

/// RFC 4180 records: quoted fields may hold the delimiter, doubled quotes and newlines. The delimiter
/// is ',' or ';' (Strong has written both), whichever the header line has more of.
fn records(text: &str) -> Vec<Vec<String>> {
    let text = text.trim_start_matches('\u{feff}');
    let header = text.lines().next().unwrap_or_default();
    let delimiter = if header.matches(';').count() > header.matches(',').count() { ';' } else { ',' };
    let (mut rows, mut row, mut field, mut quoted) = (vec![], vec![], String::new(), false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            '\r' if !quoted => {}
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c if c == delimiter && !quoted => row.push(std::mem::take(&mut field)),
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|r| r.iter().any(|f| !f.trim().is_empty()));
    rows
}

/// "1h 5m", "45m", "30s" → seconds.
fn duration(text: &str) -> Option<i64> {
    let mut total = 0;
    for token in text.split_whitespace() {
        let (number, unit) = token.split_at(token.find(|c: char| !c.is_ascii_digit())?);
        let n: i64 = number.parse().ok()?;
        total += n * match unit {
            "h" => 3600,
            "m" | "min" => 60,
            "s" => 1,
            _ => return None,
        };
    }
    (total > 0).then_some(total)
}

fn number(text: &str) -> f64 {
    text.trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(0.0)
}

fn local(text: &str, formats: &[&str], zone: &TimeZone) -> Result<Ts> {
    for format in formats {
        if let Ok(at) = DateTime::strptime(format, text.trim()) {
            return Ok(Ts(at.to_zoned(zone.clone())?.timestamp()));
        }
    }
    crate::bail!("IMPORT_INVALID", "Unreadable date '{text}'")
}

/// A Strong or Hevy CSV export. `unit` overrides the file's weight unit (Strong doesn't record it).
pub fn parse_csv(text: &str, unit: Option<Unit>, zone: &TimeZone) -> Result<History> {
    let rows = records(text);
    let Some((header, body)) = rows.split_first() else { crate::bail!("IMPORT_EMPTY", "The file is empty") };
    let col = |name: &str| header.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let field = |row: &[String], i: Option<usize>| i.and_then(|i| row.get(i)).map_or("", |f| f.trim()).to_string();
    let mut workouts: Vec<Workout> = vec![];
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut add = |key: (String, String), workout: Workout, set: Option<Set>| {
        let i = *index.entry(key).or_insert_with(|| {
            workouts.push(workout);
            workouts.len() - 1
        });
        workouts[i].sets.extend(set);
    };
    let (format, unit_unknown) =
        if let (Some(date), Some(exercise), Some(order)) = (col("Date"), col("Exercise Name"), col("Set Order")) {
            let (title, length, weight, reps, seconds, distance) =
                (col("Workout Name"), col("Duration"), col("Weight"), col("Reps"), col("Seconds"), col("Distance"));
            let unit_column = col("Weight Unit");
            for row in body {
                let order = field(row, Some(order));
                let at = local(&field(row, Some(date)), &["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"], zone)?;
                let workout =
                    Workout { at, title: field(row, title), seconds: duration(&field(row, length)), sets: vec![] };
                let file_unit = match field(row, unit_column).to_lowercase().as_str() {
                    "lbs" | "lb" => Unit::Lb,
                    _ => Unit::Kg,
                };
                // Strong writes rest timers as rows of their own.
                let set = (!order.eq_ignore_ascii_case("rest timer")).then(|| Set {
                    exercise: field(row, Some(exercise)),
                    kind: if order.eq_ignore_ascii_case("w") { SetType::Warmup } else { SetType::Working },
                    weight: unit.unwrap_or(file_unit).kg(number(&field(row, weight))),
                    reps: number(&field(row, reps)).round() as i64,
                    seconds: Some(number(&field(row, seconds)).round() as i64).filter(|s| *s > 0),
                    // Strong writes distance in km (our export does too).
                    meters: Some(number(&field(row, distance)) * 1000.0).filter(|m| *m > 0.0),
                });
                add((field(row, Some(date)), workout.title.clone()), workout, set);
            }
            (Format::Strong, unit.is_none() && unit_column.is_none())
        } else if let (Some(start), Some(exercise)) = (col("start_time"), col("exercise_title")) {
            let (title, end, kind, reps, seconds) =
                (col("title"), col("end_time"), col("set_type"), col("reps"), col("duration_seconds"));
            let (weight, file_unit) = match (col("weight_kg"), col("weight_lbs")) {
                (Some(kg), _) => (Some(kg), Unit::Kg),
                (None, lbs) => (lbs, Unit::Lb),
            };
            let (distance, per_unit) = match (col("distance_km"), col("distance_miles")) {
                (Some(km), _) => (Some(km), 1000.0),
                (None, miles) => (miles, 1609.344),
            };
            const HEVY: [&str; 2] = ["%d %b %Y, %H:%M", "%Y-%m-%d %H:%M:%S"];
            for row in body {
                let at = local(&field(row, Some(start)), &HEVY, zone)?;
                let finished = local(&field(row, end), &HEVY, zone).ok();
                let workout = Workout {
                    at,
                    title: field(row, title),
                    seconds: finished.map(|f| (f - at).as_secs()).filter(|s| *s > 0),
                    sets: vec![],
                };
                let set = Set {
                    exercise: field(row, Some(exercise)),
                    kind: if field(row, kind) == "warmup" { SetType::Warmup } else { SetType::Working },
                    weight: unit.unwrap_or(file_unit).kg(number(&field(row, weight))),
                    reps: number(&field(row, reps)).round() as i64,
                    seconds: Some(number(&field(row, seconds)).round() as i64).filter(|s| *s > 0),
                    meters: Some(number(&field(row, distance)) * per_unit).filter(|m| *m > 0.0),
                };
                add((field(row, Some(start)), workout.title.clone()), workout, Some(set));
            }
            (Format::Hevy, false)
        } else {
            crate::bail!(
                "IMPORT_FORMAT",
                "Not a Strong or Hevy export (columns: {}). Read the file and pass its workouts instead.",
                header.join(", ")
            );
        };
    Ok(History { format, workouts: workouts.into_iter().map(clean).collect(), unit_unknown })
}

/// Workouts the agent read from screenshots or text.
pub fn from_chat(workouts: &[PastWorkout], unit: Option<Unit>, zone: &TimeZone) -> Result<History> {
    let unit = unit.unwrap_or_default();
    let mut parsed = vec![];
    for w in workouts {
        let sets = w
            .sets
            .iter()
            .map(|s| Set {
                exercise: s.exercise.clone(),
                kind: SetType::Working,
                weight: unit.kg(s.weight),
                reps: s.reps,
                seconds: s.minutes.map(in_seconds),
                meters: s.meters,
            })
            .collect();
        let title = w.title.clone().unwrap_or_else(|| "Workout".into());
        parsed.push(clean(Workout { at: Ts::parse(&w.at, Some(zone))?, title, seconds: None, sets }));
    }
    Ok(History { format: Format::Chat, workouts: parsed, unit_unknown: false })
}

/// Drops sets with nothing done (no reps, time or distance) and nameless exercises.
fn clean(mut workout: Workout) -> Workout {
    workout.sets.retain(|s| !s.exercise.is_empty() && (s.reps > 0 || s.seconds.is_some() || s.meters.is_some()));
    workout
}

/// Where an imported exercise name goes: a plan exercise, a catalog exercise, or the owner's own
/// name without a picture.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub guide_id: Option<String>,
    /// How the plan exercise (or catalog entry) reads its weight and reps; imported rows keep it.
    #[serde(skip)]
    load: Load,
    #[serde(skip)]
    per_side: bool,
    #[serde(skip)]
    muscle: Option<String>,
}

impl Target {
    fn from_spec(spec: &ExerciseSpec) -> Target {
        Target {
            id: spec.id.clone(),
            name: spec.name.clone(),
            guide_id: spec.guide_id.clone(),
            load: spec.load,
            per_side: spec.per_side,
            muscle: spec.muscle.clone(),
        }
    }

    fn catalog(slug: &str, name: &str) -> Target {
        Target {
            id: slug.into(),
            name: name.into(),
            guide_id: Some(slug.into()),
            load: default_load(slug),
            per_side: false,
            muscle: None,
        }
    }

    /// The owner's own name, kept: no picture until one is drawn.
    fn own(name: &str) -> Target {
        let slug: String = name.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        let id = slug.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
        Target { id, name: name.trim().into(), guide_id: None, load: Load::External, per_side: false, muscle: None }
    }
}

/// Every exercise and alternative of the saved plans.
fn plan_specs(db: &Connection) -> Result<Vec<ExerciseSpec>> {
    let mut specs: Vec<ExerciseSpec> = vec![];
    for id in template_ids(db)? {
        let template = get_template(db, &id)?;
        for spec in template.exercises.into_iter().chain(template.alternatives) {
            if !specs.iter().any(|s| s.id == spec.id) {
                specs.push(spec);
            }
        }
    }
    Ok(specs)
}

fn keys(specs: &[ExerciseSpec]) -> Vec<[Option<&str>; 3]> {
    specs.iter().map(|s| [Some(s.id.as_str()), Some(s.name.as_str()), s.guide_id.as_deref()]).collect()
}

/// The agent's choice: a plan exercise (id, name or guide_id), else a catalog guide_id or exact name.
fn chosen(specs: &[ExerciseSpec], to: &str) -> Result<Target> {
    if let Some(i) = pick(to, &keys(specs))? {
        return Ok(Target::from_spec(&specs[i]));
    }
    match lookup(to) {
        Some(item) => Ok(Target::catalog(&item.slug, &item.name)),
        None => crate::bail!("EXERCISE_UNKNOWN", "'{to}' is neither a plan exercise nor a catalog guide_id"),
    }
}

/// A name that resolves without asking: a plan exercise's name, or exactly one catalog entry
/// (`matching`). A catalog entry a plan exercise pictures becomes that exercise, so history joins it.
fn automatic(specs: &[ExerciseSpec], name: &str) -> Option<Target> {
    if let Ok(Some(i)) = pick(name, &specs.iter().map(|s| [None, Some(s.name.as_str()), None]).collect::<Vec<_>>()) {
        return Some(Target::from_spec(&specs[i]));
    }
    let entry = matching(name)?;
    let users: Vec<&ExerciseSpec> = specs.iter().filter(|s| s.guide_id.as_deref() == Some(&entry.guide_id)).collect();
    match users[..] {
        [] => Some(Target::catalog(&entry.guide_id, &entry.name)),
        [one] => Some(Target::from_spec(one)),
        _ => None,
    }
}

/// Exercise names with set counts, in the order first seen.
fn names(history: &History) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = vec![];
    for set in history.workouts.iter().flat_map(|w| &w.sets) {
        match counts.iter_mut().find(|(n, _)| *n == set.exercise) {
            Some((_, c)) => *c += 1,
            None => counts.push((set.exercise.clone(), 1)),
        }
    }
    counts
}

/// Short summary of a saved export for the agent: format, size, and every exercise name with where it
/// would go. Handed over with the owner's message when they send a CSV.
pub fn summary(db: &Connection, path: &Path) -> Result<String> {
    let zone = get_profile(db)?.tz();
    let history = parse_csv(&std::fs::read_to_string(path)?, None, &zone)?;
    let specs = plan_specs(db)?;
    let (Some(first), Some(last)) = (history.workouts.first(), history.workouts.last()) else {
        crate::bail!("IMPORT_EMPTY", "No workouts in the file");
    };
    let day = |at: Ts| at.local(&zone).date().to_string();
    let (from, to) = (day(first.at.min(last.at)), day(first.at.max(last.at)));
    let mut lines = vec![format!(
        "{:?} export: {} workouts, {from} – {to}. Exercises (sets → match):",
        history.format,
        history.workouts.len()
    )];
    let mut names = names(&history);
    names.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (name, sets) in &names {
        let to = match automatic(&specs, name) {
            Some(t) => format!("{} ({})", t.id, t.name),
            None => format!("? closest: {}", suggestions(name).join(", ")),
        };
        lines.push(format!("- {name} ({sets}) → {to}"));
    }
    if history.unit_unknown {
        lines.push("Weights are read as kg; Strong doesn't say. Pass unit \"lb\" if the owner logs pounds.".into());
    }
    lines.push("Import with history_import (dry_run first); names marked ? need the owner's choice.".into());
    Ok(lines.join("\n"))
}

/// Within this long of an already logged workout, an imported one is the same workout.
const SAME_WORKOUT: SignedDuration = SignedDuration::from_hours(1);

/// Import `history`: every workout with no logged workout within an hour of its start becomes a
/// finished workout, and each exercise's next weight comes from its newest imported session unless
/// a newer workout set it. `exercises` maps source names to a plan exercise or catalog entry (null
/// keeps the owner's name, without a picture); names it leaves out must resolve on their own. A dry
/// run writes nothing and says where every name would go. Importing the same file again adds nothing.
pub fn import(
    db: &Connection,
    history: &History,
    exercises: &BTreeMap<String, Option<String>>,
    dry_run: bool,
    now: Ts,
    request_id: &str,
) -> Result<Value> {
    // A misread date ("Mar 2" taken as next March) would land a finished workout in the future and
    // override every newer next weight.
    if let Some(w) = history.workouts.iter().find(|w| w.at >= now) {
        crate::bail!("WORKOUT_NOT_PAST", "'{}' at {} hasn't happened yet; check its date", w.title, w.at);
    }
    let specs = plan_specs(db)?;
    let mut targets: HashMap<String, Target> = HashMap::new();
    let (mut report, mut unresolved) = (vec![], vec![]);
    for (name, sets) in names(history) {
        let mapped = exercises.iter().find(|(k, _)| k.trim().eq_ignore_ascii_case(name.trim())).map(|(_, v)| v);
        let target = match mapped {
            Some(Some(to)) => Some(chosen(&specs, to)?),
            Some(None) => Some(Target::own(&name)),
            None => automatic(&specs, &name),
        };
        match &target {
            Some(t) => report.push(json!({"name": name, "sets": sets, "to": t})),
            None => {
                report.push(json!({"name": name, "sets": sets, "to": null, "closest": suggestions(&name)}));
                unresolved.push(format!("{name} (closest: {})", suggestions(&name).join(", ")));
            }
        }
        if let Some(t) = target {
            targets.insert(name, t);
        }
    }
    let logged: Vec<Ts> = WorkoutSession::select(db, "WHERE started_at IS NOT NULL", [])?
        .into_iter()
        .filter_map(|w| w.started_at)
        .collect();
    let fresh: Vec<&Workout> = history
        .workouts
        .iter()
        .filter(|w| w.sets.iter().any(|s| s.kind == SetType::Working))
        .filter(|w| !logged.iter().any(|at| (*at - w.at).abs() < SAME_WORKOUT))
        .collect();
    let skipped = history.workouts.len() - fresh.len();
    if dry_run {
        let range = fresh.iter().map(|w| w.at).min().zip(fresh.iter().map(|w| w.at).max());
        return Ok(json!({
            "format": history.format, "workouts": history.workouts.len(), "new": fresh.len(), "already_logged_or_empty": skipped,
            "range": range, "exercises": report,
            "weights": if history.unit_unknown { "read as kg; pass unit \"lb\" if the owner logs pounds" } else { "kg" },
        }));
    }
    if !unresolved.is_empty() {
        crate::bail!(
            "IMPORT_UNMAPPED",
            "Map these names in exercises (a plan exercise or guide_id, or null to keep the owner's name): {}",
            unresolved.join("; ")
        );
    }
    let first = fresh.iter().map(|w| w.at).min();
    let intent =
        json!({"format": history.format, "workouts": history.workouts.len(), "first": first, "exercises": exercises});
    mutate(db, "history.imported", request_id, intent, now, || {
        let rest = get_profile(db)?.rest_seconds;
        let source = format!("{:?}", history.format).to_lowercase();
        // exercise id → (start of its newest imported workout, that workout, heaviest working set there).
        let mut newest: HashMap<String, (Ts, String, f64)> = HashMap::new();
        let mut sets = 0;
        for workout in &fresh {
            let (id, tops) = write(db, workout, &targets, &source, rest)?;
            sets += workout.sets.len();
            for (exercise, top) in tops {
                if newest.get(&exercise).is_none_or(|(at, _, _)| *at < workout.at) {
                    newest.insert(exercise, (workout.at, id.clone(), top));
                }
            }
        }
        let mut next_weights = BTreeMap::new();
        for (exercise, (at, workout_id, top)) in newest {
            let existing = ExerciseProgression::get(db, &exercise)?;
            let newer = match &existing {
                Some(p) => {
                    WorkoutSession::get(db, &p.source_workout_id)?.and_then(|w| w.started_at).is_some_and(|s| s > at)
                }
                None => false,
            };
            if newer {
                continue;
            }
            let row = ExerciseProgression {
                exercise_id: exercise.clone(),
                next_weight: top,
                updated_at: now,
                source_workout_id: workout_id,
            };
            if existing.is_some() {
                row.update(db)?
            } else {
                row.insert(db)?
            }
            next_weights.insert(exercise, top);
        }
        Ok(
            json!({"imported": fresh.len(), "already_logged_or_empty": skipped, "sets": sets, "next_weights": next_weights}),
        )
    })
}

/// One finished workout with its exercises and sets, spread evenly over its length. Returns the
/// workout id and each weighted exercise's heaviest working set.
fn write(
    db: &Connection,
    workout: &Workout,
    targets: &HashMap<String, Target>,
    source: &str,
    rest: i64,
) -> Result<(String, Vec<(String, f64)>)> {
    // Sets grouped by target exercise, in the order first done.
    let mut groups: Vec<(&Target, Vec<&Set>)> = vec![];
    for set in &workout.sets {
        let target = &targets[&set.exercise];
        match groups.iter_mut().find(|(t, _)| t.id == target.id) {
            Some((_, sets)) => sets.push(set),
            None => groups.push((target, vec![set])),
        }
    }
    groups.retain(|(_, sets)| sets.iter().any(|s| s.kind == SetType::Working));
    let length = workout.seconds.unwrap_or(workout.sets.len() as i64 * 180);
    let end = workout.at + SignedDuration::from_secs(length);
    let step = SignedDuration::from_secs(length / (workout.sets.len() as i64 + 1));
    let mut specs = vec![];
    let mut tops = vec![];
    for (target, sets) in &groups {
        let working: Vec<&&Set> = sets.iter().filter(|s| s.kind == SetType::Working).collect();
        // A rower row can carry both a time and a distance; distance is what the set measured.
        let distance = working.iter().all(|s| s.meters.is_some() && s.reps == 0);
        let timed = !distance && working.iter().all(|s| s.seconds.is_some() && s.reps == 0);
        let reps = working.iter().map(|s| s.reps.max(1));
        // The best set: the heaviest, or on an assisted machine the least help.
        let top = match target.load {
            Load::Assisted => working.iter().map(|s| s.weight).fold(f64::INFINITY, f64::min),
            _ => working.iter().map(|s| s.weight).fold(0.0, f64::max),
        };
        let mut spec = json!({
            "id": target.id, "name": target.name, "role": "imported", "guide_id": target.guide_id,
            "working_sets": working.len(), "rep_min": reps.clone().min().unwrap_or(1), "rep_max": reps.max().unwrap_or(1),
            "target_weight": top, "measure": if distance { "distance" } else if timed { "time" } else { "reps" },
            "load": target.load, "per_side": target.per_side, "muscle": target.muscle,
        });
        if distance {
            spec["meters"] = json!(working[0].meters);
        } else if timed {
            spec["minutes"] = json!(working[0].seconds.unwrap_or(60) as f64 / 60.0);
        } else if top > 0.0 {
            tops.push((target.id.clone(), top));
        }
        specs.push(spec);
    }
    let session = WorkoutSession {
        id: new_id(),
        planned_session_id: None,
        template_id: None,
        template_snapshot: Json(json!({"id": "imported", "name": workout.title, "exercises": specs, "source": source})),
        last_action_at: Some(end),
        started_at: Some(workout.at),
        started_by_button: false,
        completed_at: Some(end),
        status: WorkoutStatus::PlanUpdated,
        initial_eta: None,
        final_eta: None,
        actual_duration_seconds: Some(length),
        crowd_feedback: None,
        waited_for_equipment_count: 0,
    };
    session.insert(db)?;
    // Each set's time in the workout, by its place in the source.
    let when = |set: &Set| {
        let i = workout.sets.iter().position(|s| std::ptr::eq(s, set)).unwrap_or(0) as i32;
        workout.at + step * (i + 1)
    };
    for (position, ((target, sets), spec)) in groups.iter().zip(&specs).enumerate() {
        let mut config = spec.clone();
        config["warmup_required"] = json!(false);
        let row = WorkoutExercise {
            id: new_id(),
            workout_session_id: session.id.clone(),
            exercise_id: target.id.clone(),
            position: position as i64,
            status: ExerciseStatus::Completed,
            planned_working_sets: spec["working_sets"].as_i64().unwrap_or(1),
            rep_min: spec["rep_min"].as_i64().unwrap_or(1),
            rep_max: spec["rep_max"].as_i64().unwrap_or(1),
            target_weight: spec["target_weight"].as_f64().unwrap_or(0.0),
            rest_seconds: rest,
            substituted_from_exercise_id: None,
            deferred_reason: None,
            config_json: Json(config),
        };
        row.insert(db)?;
        let mut numbers: HashMap<SetType, i64> = HashMap::new();
        for (i, set) in sets.iter().enumerate() {
            let number = numbers.entry(set.kind).or_default();
            *number += 1;
            let at = when(set);
            // Rest fills the gap to the next set, so imported spacing never counts as set duration.
            let next = sets.get(i + 1).map(|s| when(s));
            SetLog {
                id: new_id(),
                workout_exercise_id: row.id.clone(),
                set_number: *number,
                set_type: set.kind,
                weight: set.weight,
                reps: set.reps,
                logged_at: at,
                rest_due_at: next,
                seconds: set.seconds.filter(|_| set.reps == 0),
                meters: set.meters.filter(|_| set.reps == 0),
            }
            .insert(db)?;
        }
    }
    Ok((session.id, tops))
}

/// Every logged set of every finished workout as a Strong-format CSV (kg), oldest first.
pub fn export_csv(db: &Connection) -> Result<String> {
    let zone = get_profile(db)?.tz();
    let quote = |text: &str| {
        if text.contains([',', '"', '\n']) { format!("\"{}\"", text.replace('"', "\"\"")) } else { text.to_string() }
    };
    let mut out = String::from(
        "Date,Workout Name,Duration,Exercise Name,Set Order,Weight,Reps,Distance,Seconds,Notes,Workout Notes,RPE\n",
    );
    let workouts =
        WorkoutSession::select(db, "WHERE status = 'PLAN_UPDATED' AND started_at IS NOT NULL ORDER BY started_at", [])?;
    for workout in workouts {
        let at = workout.started_at.expect("filtered");
        let date = at.local(&zone).strftime("%Y-%m-%d %H:%M:%S").to_string();
        let title = workout.template_snapshot["name"].as_str().unwrap_or("Workout").to_string();
        let minutes = workout.actual_duration_seconds.unwrap_or(0) / 60;
        let length = if minutes >= 60 { format!("{}h {}m", minutes / 60, minutes % 60) } else { format!("{minutes}m") };
        for exercise in WorkoutExercise::select(db, "WHERE workout_session_id = ?1 ORDER BY position", [&workout.id])? {
            let name = exercise.config_json["name"].as_str().unwrap_or(&exercise.exercise_id).to_string();
            for set in
                SetLog::select(db, "WHERE workout_exercise_id = ?1 ORDER BY logged_at, set_number", [&exercise.id])?
            {
                let order = if set.set_type == SetType::Warmup { "W".to_string() } else { set.set_number.to_string() };
                out.push_str(&format!(
                    "{date},{},{length},{},{order},{},{},{},{},,,\n",
                    quote(&title),
                    quote(&name),
                    set.weight,
                    set.reps,
                    set.meters.map_or(0.0, |m| m / 1000.0),
                    set.seconds.unwrap_or(0)
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test::db;
    use crate::domain::templates::{Template, import_template};

    const STRONG: &str = "Date;Workout Name;Duration;Exercise Name;Set Order;Weight;Reps;Distance;Seconds;Notes;Workout Notes;RPE
\"2026-03-02 18:00:00\";\"Push, heavy\";\"1h 5m\";\"Bench Press (Barbell)\";\"W\";\"40\";\"10\";\"0\";\"0\";\"\";\"\";\"\"
\"2026-03-02 18:00:00\";\"Push, heavy\";\"1h 5m\";\"Bench Press (Barbell)\";\"1\";\"80\";\"8\";\"0\";\"0\";\"\";\"\";\"\"
\"2026-03-02 18:00:00\";\"Push, heavy\";\"1h 5m\";\"Bench Press (Barbell)\";\"Rest Timer\";\"0\";\"0\";\"0\";\"90\";\"\";\"\";\"\"
\"2026-03-02 18:00:00\";\"Push, heavy\";\"1h 5m\";\"Bench Press (Barbell)\";\"2\";\"82.5\";\"8\";\"0\";\"0\";\"\";\"\";\"\"
\"2026-03-02 18:00:00\";\"Push, heavy\";\"1h 5m\";\"Cable Thing\";\"1\";\"30\";\"12\";\"0\";\"0\";\"\";\"\";\"\"
\"2026-03-05 07:30:00\";\"Push, heavy\";\"45m\";\"Bench Press (Barbell)\";\"1\";\"85\";\"6\";\"0\";\"0\";\"\";\"\";\"\"
\"2026-03-05 07:30:00\";\"Push, heavy\";\"45m\";\"Rowing (Machine)\";\"1\";\"0\";\"0\";\"2\";\"600\";\"\";\"\";\"\"
";

    const HEVY: &str = "\"title\",\"start_time\",\"end_time\",\"description\",\"exercise_title\",\"superset_id\",\"exercise_notes\",\"set_index\",\"set_type\",\"weight_lbs\",\"reps\",\"distance_miles\",\"duration_seconds\",\"rpe\"
\"Pull\",\"22 Dec 2025, 08:00\",\"22 Dec 2025, 08:37\",\"\",\"Lat Pulldown (Cable)\",,\"\",0,\"warmup\",50,12,,0,
\"Pull\",\"22 Dec 2025, 08:00\",\"22 Dec 2025, 08:37\",\"\",\"Lat Pulldown (Cable)\",,\"\",1,\"normal\",100,10,,0,8.5
";

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    fn now() -> Ts {
        Ts::parse("2026-11-15T12:00:00+00:00", None).unwrap()
    }

    #[test]
    fn strong_and_hevy_exports_parse_to_sets_in_kg() {
        let strong = parse_csv(STRONG, None, &utc()).unwrap();
        assert_eq!((strong.format, strong.workouts.len(), strong.unit_unknown), (Format::Strong, 2, true));
        let push = &strong.workouts[0];
        assert_eq!((push.title.as_str(), push.seconds), ("Push, heavy", Some(3900)));
        // The rest timer row is gone; the warm-up stays a warm-up.
        assert_eq!(push.sets.len(), 4);
        assert_eq!((push.sets[0].kind, push.sets[1].weight), (SetType::Warmup, 80.0));
        let row = &strong.workouts[1].sets[1];
        assert_eq!((row.seconds, row.meters), (Some(600), Some(2000.0)), "Strong's distance is km");
        let hevy = parse_csv(HEVY, None, &utc()).unwrap();
        let set = &hevy.workouts[0].sets[1];
        assert_eq!(
            (hevy.workouts[0].seconds, set.kind, set.weight, set.reps),
            (Some(2220), SetType::Working, 45.36, 10)
        );
        assert_eq!(parse_csv("a,b\n1,2\n", None, &utc()).unwrap_err().code(), "IMPORT_FORMAT");
    }

    fn plan(db: &Connection) {
        let template: Template = serde_json::from_value(json!({"id": "push", "name": "Push", "exercises": [
            {"id": "bench", "name": "Bench", "role": "press", "guide_id": "bench-press", "primary": true, "target_weight": 60, "warmup_weight": 30}]}))
        .unwrap();
        import_template(db, &template).unwrap();
    }

    #[test]
    fn import_joins_plan_history_once_and_sets_next_weights() {
        let db = db();
        plan(&db);
        let history = parse_csv(STRONG, None, &utc()).unwrap();
        // Names that don't resolve stop a real import and say what to map.
        let dry = import(&db, &history, &BTreeMap::new(), true, now(), "r1").unwrap();
        assert_eq!(dry["exercises"][0]["to"]["id"], "bench", "bracketed catalog name joins the plan exercise");
        assert_eq!(dry["exercises"][1]["to"], Value::Null);
        assert_eq!(WorkoutSession::select(&db, "", []).unwrap().len(), 0);
        let unmapped = import(&db, &history, &BTreeMap::new(), false, now(), "r2").unwrap_err();
        assert!(unmapped.to_string().contains("Cable Thing"), "{unmapped}");
        let map = BTreeMap::from([
            ("cable thing".to_string(), None),
            ("Rowing (Machine)".to_string(), Some("rowing".to_string())),
        ]);
        let done = import(&db, &history, &map, false, now(), "r3").unwrap();
        assert_eq!((done["data"]["imported"].as_i64(), done["data"]["sets"].as_i64()), (Some(2), Some(6)));
        // The newest session's heaviest set is the plan's next weight; the timed row has none.
        assert_eq!(ExerciseProgression::get(&db, "bench").unwrap().unwrap().next_weight, 85.0);
        assert!(ExerciseProgression::get(&db, "rowing").unwrap().is_none());
        let own = WorkoutExercise::first(&db, "WHERE exercise_id = 'cable-thing'", []).unwrap().unwrap();
        assert_eq!(
            (own.config_json["name"].as_str(), own.config_json["guide_id"].as_str()),
            (Some("Cable Thing"), None)
        );
        // Same file again (new request): nothing new.
        let again = import(&db, &history, &map, false, now(), "r4").unwrap();
        assert_eq!(
            (again["data"]["imported"].as_i64(), again["data"]["already_logged_or_empty"].as_i64()),
            (Some(0), Some(2))
        );
        assert_eq!(SetLog::select(&db, "", []).unwrap().len(), 6);
        let early = Ts::parse("2025-01-01T00:00:00+00:00", None).unwrap();
        assert_eq!(import(&db, &history, &map, true, early, "r5").unwrap_err().code(), "WORKOUT_NOT_PAST");
    }

    /// An assisted machine keeps its load: the next weight is the least help, not the most.
    #[test]
    fn assisted_import_keeps_its_load() {
        let db = db();
        let csv = HEVY.lines().next().unwrap().to_string()
            + "\n\"Pull\",\"22 Dec 2025, 08:00\",\"22 Dec 2025, 08:37\",\"\",\"Assisted Pull-up\",,\"\",0,\"normal\",40,8,,0,\n\"Pull\",\"22 Dec 2025, 08:00\",\"22 Dec 2025, 08:37\",\"\",\"Assisted Pull-up\",,\"\",1,\"normal\",30,8,,0,\n";
        let history = parse_csv(&csv, None, &utc()).unwrap();
        import(&db, &history, &BTreeMap::new(), false, now(), "r1").unwrap();
        let row = WorkoutExercise::first(&db, "WHERE exercise_id = 'assisted-pull-up'", []).unwrap().unwrap();
        assert_eq!(row.config_json["load"], "assisted");
        let next = ExerciseProgression::get(&db, "assisted-pull-up").unwrap().unwrap().next_weight;
        assert!((next - 30.0 * 0.45359237).abs() < 0.01, "{next}");
    }

    #[test]
    fn summary_says_where_each_name_goes() {
        let db = db();
        plan(&db);
        let path = std::env::temp_dir().join(format!("gymclaw-strong-{}.csv", new_id()));
        std::fs::write(&path, STRONG).unwrap();
        let text = summary(&db, &path).unwrap();
        assert!(text.starts_with("Strong export: 2 workouts, 2026-03-02 – 2026-03-05"), "{text}");
        assert!(text.contains("- Bench Press (Barbell) (4) → bench (Bench)"), "{text}");
        assert!(text.contains("- Cable Thing (1) → ? closest:"), "{text}");
        assert!(text.contains("unit \"lb\""), "{text}");
    }

    #[test]
    fn export_round_trips_through_import() {
        let db = db();
        plan(&db);
        let map = BTreeMap::from([
            ("cable thing".to_string(), None),
            ("Rowing (Machine)".to_string(), Some("rowing".to_string())),
        ]);
        import(&db, &parse_csv(STRONG, None, &utc()).unwrap(), &map, false, now(), "r1").unwrap();
        let csv = export_csv(&db).unwrap();
        // The export writes the owner's local time.
        let back = parse_csv(&csv, None, &get_profile(&db).unwrap().tz()).unwrap();
        let original = parse_csv(STRONG, None, &utc()).unwrap();
        let sets = |h: &History| {
            h.workouts
                .iter()
                .flat_map(|w| w.sets.iter().map(|s| (s.kind, s.weight, s.reps, s.seconds, s.meters)))
                .collect::<Vec<_>>()
        };
        assert_eq!(sets(&back), sets(&original));
        assert_eq!(
            back.workouts.iter().map(|w| (w.at, w.seconds)).collect::<Vec<_>>(),
            original.workouts.iter().map(|w| (w.at, w.seconds)).collect::<Vec<_>>()
        );
    }
}
