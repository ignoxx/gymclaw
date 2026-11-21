//! Workout templates: validated on import, stored as JSON, snapshotted when a workout starts.
use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::models::{ExerciseProgression, WeightUnit, WorkoutExercise, WorkoutTemplate};
use crate::db::{Connection, Json};
use crate::domain::illustrations::{Item, catalog, lookup, matching, muscles, suggestions, timed, unknown};
use crate::domain::workout::{format_distance, format_time, g, in_seconds};
use crate::error::{Error, Result};
use crate::time::Ts;

// How an exercise is done and logged (plain comments: the field doc is what the agent reads).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Measure {
    // Sets of reps at a weight.
    #[default]
    Reps,
    // Timed (bike, rower, treadmill, plank): each working set is `minutes` long and logged as a time.
    Time,
    // Distance (rower, carry, sled): each working set is `meters` long and logged as a distance.
    Distance,
}

impl Measure {
    /// How a set of this measure is typed, for error messages.
    pub fn example(self) -> &'static str {
        match self {
            Measure::Reps => "reps × weight",
            Measure::Time => "a time, e.g. 25 min",
            Measure::Distance => "a distance, e.g. 2 km",
        }
    }
}

// What the logged weight of a reps exercise is (plain comments, as for `Measure`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Load {
    // The weight lifted (bar, stack, dumbbells).
    #[default]
    External,
    // Added to bodyweight (dip belt, weighted vest): shown "+10 kg".
    Added,
    // Counterweight that helps (assisted pull-up machine): less is progress.
    Assisted,
}

/// How an exercise's sets read: its load and whether reps count per side. Every place that shows a set
/// or target (coach card, summary, recap, dashboard) formats through this, so they never disagree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetKind {
    pub load: Load,
    pub per_side: bool,
    pub unit: WeightUnit,
}

impl SetKind {
    /// Format weights in `unit` (converting from canonical kg).
    pub fn in_unit(mut self, unit: WeightUnit) -> Self {
        self.unit = unit;
        self
    }

    /// A weight (canonical kg) as a display number with its sign: "80", "+10" (added), "20" (assistance).
    pub fn number(self, weight_kg: f64) -> String {
        let number = g(self.unit.to_display(weight_kg));
        if self.load == Load::Added { format!("+{number}") } else { number }
    }

    /// The unit after `number`: "kg"/"lb", or "kg assist"/"lb assist" for a counterweight.
    pub fn unit(self) -> &'static str {
        match (self.load == Load::Assisted, self.unit) {
            (true, WeightUnit::Kg) => "kg assist",
            (true, WeightUnit::Lb) => "lb assist",
            (false, WeightUnit::Kg) => "kg",
            (false, WeightUnit::Lb) => "lb",
        }
    }

    /// "80 kg", "+10 kg", "20 kg assist".
    pub fn weight(self, weight: f64) -> String {
        format!("{} {}", self.number(weight), self.unit())
    }

    /// Reps (or a range like "8–10"), with "/side" when counted per side.
    pub fn reps(self, reps: &str) -> String {
        if self.per_side { format!("{reps}/side") } else { reps.to_string() }
    }

    /// A target the way the owner says it, reps first: "8–10 × 80 kg", "8/side × +10 kg"; no weight
    /// (bodyweight, unknown) shows reps only: "12–15 reps", "8/side".
    pub fn target(self, rep_min: i64, rep_max: i64, weight: f64) -> String {
        let range = if rep_min == rep_max { rep_min.to_string() } else { format!("{rep_min}–{rep_max}") };
        match (weight != 0.0, self.per_side) {
            (true, _) => format!("{} × {}", self.reps(&range), self.weight(weight)),
            (false, true) => self.reps(&range),
            (false, false) => format!("{range} reps"),
        }
    }

    /// A logged set: "10 × 40 kg", "12 reps", "25 min", "2 km", "2 km in 10 min" (an import can carry
    /// both). Its fields say what it measured.
    pub fn set(self, weight: f64, reps: i64, seconds: Option<i64>, meters: Option<f64>) -> String {
        match (seconds, meters) {
            (Some(seconds), Some(meters)) => format!("{} in {}", format_distance(meters), format_time(seconds)),
            (Some(seconds), None) => self.reps(&format_time(seconds)),
            (None, Some(meters)) => format_distance(meters),
            (None, None) => self.target(reps, reps, weight),
        }
    }

    /// The weight with "better" pointing up: an assisted set with less help ranks higher.
    pub fn signed(self, weight: f64) -> f64 {
        if self.load == Load::Assisted { -weight } else { weight }
    }
}

/// Catalog exercises done on an assisted machine (or with a band): their weight is help, not load.
pub fn default_load(slug: &str) -> Load {
    if slug.contains("assisted") { Load::Assisted } else { Load::External }
}

/// A catalog exercise in `original`'s slot under its own id and picture: its own load, weight unknown
/// (0) until its history or the owner says.
pub fn catalog_slot(original: &ExerciseSpec, item: &Item) -> ExerciseSpec {
    ExerciseSpec {
        id: item.slug.clone(),
        name: item.name.clone(),
        guide_id: Some(item.slug.clone()),
        muscle: None,
        target_weight: 0.0,
        load: default_load(&item.slug),
        per_side: false,
        ..original.clone()
    }
}

/// About half the working weight on the plate steps, never above half: a warm-up's default.
pub fn half_weight(target_weight: f64, increment: f64) -> f64 {
    (target_weight / 2.0 / increment).floor() * increment
}

pub fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExerciseSpec {
    pub id: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub guide_id: Option<String>,
    /// The muscle it trains when there's no guide_id (with one, the catalog's muscle counts): a
    /// catalog muscle, set by the agent, so weekly sets per muscle and swaps don't guess from the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muscle: Option<String>,
    #[serde(default)]
    pub primary: bool,
    #[serde(default = "d3")]
    pub working_sets: i64,
    #[serde(default = "d8")]
    pub rep_min: i64,
    #[serde(default = "d10")]
    pub rep_max: i64,
    pub target_weight: f64,
    #[serde(default = "d2_5")]
    pub increment: f64,
    /// Seconds of rest after each set of this exercise; omit (or null) for the owner's default rest.
    #[serde(default)]
    pub rest_seconds: Option<i64>,
    #[serde(default)]
    pub warmup_weight: Option<f64>,
    #[serde(default = "d8")]
    pub warmup_reps: i64,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub requires_completed: Vec<String>,
    #[serde(default)]
    pub substitutes: Vec<String>,
    #[serde(default)]
    pub measure: Measure,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<f64>,
    /// Distance per working set when measure is "distance".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meters: Option<f64>,
    /// "added": the weight is added to bodyweight (weighted pull-up); "assisted": the weight is the
    /// machine's help, so less is progress.
    #[serde(default, skip_serializing_if = "is_default")]
    pub load: Load,
    /// Reps count per side (single-arm row, split squat).
    #[serde(default, skip_serializing_if = "is_default")]
    pub per_side: bool,
    /// Superset or circuit: exercises with the same group are done back to back in plan order, with
    /// rest after each round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

fn d3() -> i64 {
    3
}
fn d8() -> i64 {
    8
}
fn d10() -> i64 {
    10
}
fn d2_5() -> f64 {
    2.5
}
fn d40() -> i64 {
    40
}
fn d60() -> i64 {
    60
}
fn user() -> String {
    "user".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub exercises: Vec<ExerciseSpec>,
    /// Swap options, with the same fields as an exercise. Keep the saved ones when re-saving, or they are lost.
    #[serde(default)]
    #[schemars(with = "Vec<serde_json::Map<String, serde_json::Value>>")]
    pub alternatives: Vec<ExerciseSpec>,
    #[serde(default = "d40")]
    pub set_duration_seconds: i64,
    #[serde(default = "d60")]
    pub transition_seconds: i64,
    #[serde(default = "user")]
    pub source: String,
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::domain("INVALID_TEMPLATE", message.to_string()))
}

fn identifier(value: &str) -> bool {
    (1..=100).contains(&value.chars().count())
}

impl ExerciseSpec {
    fn validate(&self) -> Result<()> {
        if ![&self.id, &self.name, &self.role].iter().all(|v| identifier(v))
            || self.requires_completed.iter().chain(&self.substitutes).chain(&self.group).any(|v| !identifier(v))
        {
            return invalid("IDs, names, roles and groups are 1–100 characters");
        }
        if let Some(guide) = &self.guide_id
            && !catalog().contains_key(guide)
        {
            return invalid(&format!("guide_id must match Workout Guide catalog: {guide}"));
        }
        if let Some(muscle) = &self.muscle
            && !muscles().contains(muscle.as_str())
        {
            return invalid(&format!(
                "muscle must be one of: {}",
                muscles().into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        if self.working_sets <= 0
            || self.rep_min <= 0
            || self.rep_max <= 0
            || self.warmup_reps <= 0
            || self.rest_seconds.is_some_and(|r| r <= 0)
            || self.priority < 0
        {
            return invalid("Sets, reps and rest must be positive");
        }
        let weights = [Some(self.target_weight), self.warmup_weight];
        if weights.iter().flatten().any(|w| !w.is_finite() || *w < 0.0)
            || !self.increment.is_finite()
            || self.increment <= 0.0
        {
            return invalid("Weights must be ≥ 0 and the increment > 0");
        }
        if self.rep_min > self.rep_max {
            return invalid("Rep minimum exceeds maximum");
        }
        // Each block measure has its own per-set target, and only that one.
        let targets = [
            (Measure::Time, self.minutes, "minutes", "time", "timed"),
            (Measure::Distance, self.meters, "meters", "distance", "done for distance"),
        ];
        for (measure, value, field, label, done) in targets {
            match (self.measure == measure, value) {
                (true, Some(v)) if v.is_finite() && v > 0.0 => {}
                (true, _) => return invalid(&format!("{} is {done}: pass {field} (per set) > 0", self.name)),
                (false, Some(_)) => return invalid(&format!("{}: {field} is only for measure \"{label}\"", self.name)),
                (false, None) => {}
            }
        }
        Ok(())
    }

    /// Length of one working set of a timed exercise.
    pub fn set_seconds(&self) -> Option<i64> {
        self.minutes.filter(|_| self.measure == Measure::Time).map(in_seconds)
    }

    pub fn kind(&self) -> SetKind {
        SetKind { load: self.load, per_side: self.per_side, unit: WeightUnit::Kg }
    }
}

impl Template {
    pub fn validate(&self) -> Result<()> {
        if !identifier(&self.id) || !identifier(&self.name) {
            return invalid("Template id and name are 1–100 characters");
        }
        if self.exercises.is_empty() {
            return invalid("A template needs at least one exercise");
        }
        if self.set_duration_seconds <= 0 || self.transition_seconds < 0 {
            return invalid("Set duration must be positive");
        }
        let all: Vec<&ExerciseSpec> = self.exercises.iter().chain(&self.alternatives).collect();
        for spec in &all {
            spec.validate()?;
        }
        // Names are how the agent picks between variants that share a picture (see `pick`). The error
        // names both clashing exercises (often an alternative) so the caller can rename or remove one.
        let kind = |i: usize| if i < self.exercises.len() { "exercise" } else { "alternative" };
        let unique = |field: &str, key: fn(&ExerciseSpec) -> String| -> Result<()> {
            for (i, a) in all.iter().enumerate() {
                if let Some(j) = all[..i].iter().position(|b| key(b) == key(a)) {
                    let b = all[j];
                    return invalid(&format!(
                        "Exercise {field}s must be unique within template: {} '{}' ({}) and {} '{}' ({}) clash; rename or remove one",
                        kind(j),
                        b.name,
                        b.id,
                        kind(i),
                        a.name,
                        a.id
                    ));
                }
            }
            Ok(())
        };
        unique("ID", |s| s.id.clone())?;
        unique("name", |s| s.name.trim().to_lowercase())?;
        let mut seen: HashSet<&str> = HashSet::new();
        for spec in &self.exercises {
            if !spec.requires_completed.iter().all(|r| seen.contains(r.as_str())) {
                return invalid("Dependencies must reference earlier planned exercises");
            }
            seen.insert(&spec.id);
        }
        for alternative in &self.alternatives {
            if !alternative.requires_completed.iter().all(|r| seen.contains(r.as_str())) {
                return invalid("Alternative dependencies must reference planned exercises");
            }
        }
        for spec in &all {
            for substitute in &spec.substitutes {
                let ok = self
                    .alternatives
                    .iter()
                    .find(|a| &a.id == substitute)
                    .is_some_and(|a| a.role == spec.role && substitute != &spec.id);
                if !ok {
                    return invalid("Substitutes must reference same-role alternatives");
                }
            }
        }
        if let Some(first) = self.warmup_exercise()
            && first.warmup_weight.is_none()
        {
            return invalid("First primary exercise requires explicit warmup_weight");
        }
        Ok(())
    }

    /// The first primary lift: it gets the warm-up set. Timed exercises never do.
    pub fn warmup_exercise(&self) -> Option<&ExerciseSpec> {
        self.exercises.iter().find(|s| s.primary && s.measure == Measure::Reps)
    }

    pub fn spec(&self, id: &str) -> Option<&ExerciseSpec> {
        self.exercises.iter().chain(&self.alternatives).find(|s| s.id == id)
    }
}

/// Warning for exercises saved without a guide_id, with their closest catalog matches. They are
/// allowed: the card shows the closest picture meanwhile and the exercise is on the missing-pictures
/// list. The agent asks the owner once whether one of the matches is what they meant.
pub fn missing_pictures(template: &Template) -> Option<String> {
    let missing: Vec<String> = template
        .exercises
        .iter()
        .chain(&template.alternatives)
        .filter(|s| s.guide_id.is_none() && s.muscle.is_none())
        .map(|s| {
            let options = suggestions(&s.name);
            format!("{} (closest: {})", s.name, if options.is_empty() { "none".into() } else { options.join(", ") })
        })
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "Saved without a picture of their own: {}. Unless the owner already chose their own name, ask once with buttons: the closest matches plus \"keep my name\"; for a match set guide_id with template (action edit_exercise) (keep id and name); otherwise set its muscle.",
            missing.join("; ")
        )
    })
}

/// The catalog picture an exercise's name means, so a plan read off another app's screenshots saves
/// with pictures without a catalog search per exercise: its exact catalog name or the way other apps
/// write it ("Bench Press (Barbell)", `matching`). A guide_id the catalog doesn't know is read as a
/// name too, else dropped, so the exercise saves as the owner's own instead of failing the import
/// (`missing_pictures` then asks). A muscle without a guide_id is an owner's own exercise: left alone.
/// Normalize common informal muscle names (e.g. "Abs" -> "Core", "Pecs" -> "Chest") to catalog
/// muscles; drop unknown ones so an import never fails over a synonym.
fn normalize_muscle(spec: &mut ExerciseSpec) {
    let Some(raw) = spec.muscle.as_deref() else { return };
    let valid = muscles();
    if let Some(exact) = valid.iter().find(|m| m.eq_ignore_ascii_case(raw.trim())) {
        spec.muscle = Some((*exact).to_string());
        return;
    }
    let mapped = match raw.trim().to_ascii_lowercase().as_str() {
        "abs" | "abdominals" | "obliques" | "midsection" => Some("Core"),
        "pecs" | "pectorals" => Some("Chest"),
        "lats" | "upper back" | "lower back" | "traps" | "rhomboids" => Some("Back"),
        "delts" | "deltoids" => Some("Shoulders"),
        "quads" | "quadriceps" | "hamstrings" | "glutes" | "calves" | "adductors" | "abductors" => Some("Legs"),
        "arms" | "bicep" | "tricep" | "forearms" => Some("Arms"),
        _ => None,
    };
    if let Some(m) = mapped.filter(|m| valid.iter().any(|v| v == m)) {
        spec.muscle = Some(m.to_string());
    }
}

fn picture(spec: &mut ExerciseSpec) {
    normalize_muscle(spec);
    let known = spec.guide_id.as_deref().is_some_and(|g| catalog().contains_key(g));
    if known || (spec.guide_id.is_none() && spec.muscle.is_some()) {
        return;
    }
    let named = |key: &str| lookup(key).map(|i| i.slug.clone()).or_else(|| matching(key).map(|e| e.guide_id));
    spec.guide_id = spec.guide_id.as_deref().and_then(named).or_else(|| named(&spec.name));
}

/// Save a whole template (new or replacing its id), pictures matched (`picture`). Returns what was saved.
pub fn import_template(db: &Connection, template: &Template) -> Result<Value> {
    let mut template = template.clone();
    template.exercises.iter_mut().chain(template.alternatives.iter_mut()).for_each(picture);
    template.validate()?;
    let definition = serde_json::to_value(&template)?;
    let row =
        WorkoutTemplate { id: template.id.clone(), definition_json: Json(definition.clone()), updated_at: Ts::now() };
    if WorkoutTemplate::get(db, &template.id)?.is_some() {
        row.update(db)?
    } else {
        row.insert(db)?
    }
    Ok(definition)
}

pub fn get_template(db: &Connection, template_id: &str) -> Result<Template> {
    let Some(row) = WorkoutTemplate::get(db, template_id)? else {
        crate::bail!(
            "TEMPLATE_UNKNOWN",
            "Unknown workout template: {template_id}. Templates: {}",
            template_ids(db)?.join(", ")
        );
    };
    serde_json::from_value(row.definition_json.0)
        .map_err(|e| Error::domain("INVALID_TEMPLATE", format!("Stored template {template_id}: {e}")))
}

pub fn template_ids(db: &Connection) -> Result<Vec<String>> {
    Ok(WorkoutTemplate::select(db, "ORDER BY id", [])?.into_iter().map(|t| t.id).collect())
}

/// Owner swapped an exercise mid-workout: the replacement takes its place in the saved template, and
/// the original stays as an alternative so it's one swap away. False if the template can't take it.
pub fn remember_swap(
    db: &Connection,
    template_id: &str,
    original_id: &str,
    replacement: &ExerciseSpec,
) -> Result<bool> {
    let Ok(template) = get_template(db, template_id) else { return Ok(false) };
    let Some(index) = template.exercises.iter().position(|e| e.id == original_id) else { return Ok(false) };
    let updated = swapped(&template, index, replacement);
    if updated.validate().is_err() {
        return Ok(false);
    }
    import_template(db, &updated)?;
    Ok(true)
}

/// `replacement` in exercise `index`'s slot (sets, reps, rest, role, order and superset group stay the
/// slot's), the original kept as an alternative one swap away, dependencies following the new id. A
/// first primary lift keeps a warm-up: half the new weight on the plate steps.
pub fn swapped(template: &Template, index: usize, replacement: &ExerciseSpec) -> Template {
    let original = template.exercises[index].clone();
    let original_id = original.id.as_str();
    let warmup = |r: &ExerciseSpec| {
        r.warmup_weight.or_else(|| {
            original
                .warmup_weight
                .filter(|_| r.measure == Measure::Reps)
                .map(|_| half_weight(r.target_weight, r.increment))
        })
    };
    let new = ExerciseSpec {
        warmup_weight: warmup(replacement),
        role: original.role.clone(),
        primary: original.primary,
        working_sets: original.working_sets,
        rep_min: original.rep_min,
        rep_max: original.rep_max,
        rest_seconds: original.rest_seconds,
        priority: original.priority,
        requires_completed: original.requires_completed.clone(),
        substitutes: vec![original_id.into()],
        group: original.group.clone(),
        ..replacement.clone()
    };
    let mut alternatives: Vec<ExerciseSpec> =
        template.alternatives.iter().filter(|a| a.id != replacement.id && a.id != original_id).cloned().collect();
    alternatives.push(ExerciseSpec { substitutes: vec![], requires_completed: vec![], ..original.clone() });
    let mut exercises: Vec<ExerciseSpec> = template
        .exercises
        .iter()
        .map(|e| ExerciseSpec {
            requires_completed: e
                .requires_completed
                .iter()
                .map(|r| if r == original_id { replacement.id.clone() } else { r.clone() })
                .collect(),
            substitutes: e.substitutes.iter().filter(|s| **s != replacement.id).cloned().collect(),
            ..e.clone()
        })
        .collect();
    exercises[index] = new;
    Template { exercises, alternatives, ..template.clone() }
}

pub enum ExerciseEdit<'a> {
    /// ExerciseSpec fields to change; a new target_weight also resets the learned next weight.
    Change(&'a Map<String, Value>),
    /// `exercise` is a catalog guide_id or name; fields override the defaults.
    Add(&'a Map<String, Value>),
    Remove,
}

/// Index of the exercise `key` names by id, name or guide_id (case-insensitive), from `[id, name,
/// guide_id]` candidates. Matches on several ids fail, even when one is an exact id: variants share a
/// guide_id, and the first variant's id often equals it, so a guess would edit or log the wrong one.
/// Names are unique per template, so the names in the error always resolve.
pub fn pick(key: &str, candidates: &[[Option<&str>; 3]]) -> Result<Option<usize>> {
    let key = key.trim().to_lowercase();
    let hits: Vec<usize> =
        (0..candidates.len()).filter(|&i| candidates[i].iter().flatten().any(|v| v.to_lowercase() == key)).collect();
    let mut ids: Vec<Option<&str>> = hits.iter().map(|&i| candidates[i][0]).collect();
    ids.sort();
    ids.dedup();
    if ids.len() > 1 {
        let mut names: Vec<&str> = hits.iter().filter_map(|&i| candidates[i][1]).collect();
        names.sort();
        names.dedup();
        crate::bail!(
            "EXERCISE_AMBIGUOUS",
            "'{key}' matches several exercises; pass one of these names: {}",
            names.join(", ")
        );
    }
    Ok(hits.first().copied())
}

/// Change one exercise of a saved plan without re-importing it. `exercise` is an ID, guide_id or name;
/// `position` is the 0-based place. Upcoming planned sessions pick the change up via `refresh_plans`.
pub fn edit_exercise(
    db: &Connection,
    template_id: &str,
    exercise: &str,
    edit: ExerciseEdit,
    position: Option<usize>,
) -> Result<Value> {
    let mut definition = serde_json::to_value(get_template(db, template_id)?)?;
    let name = definition["name"].as_str().unwrap_or_default().to_string();
    let key = exercise.trim().to_lowercase();
    let find = |rows: &[Value]| {
        pick(
            &key,
            &rows.iter().map(|e| [e["id"].as_str(), e["name"].as_str(), e["guide_id"].as_str()]).collect::<Vec<_>>(),
        )
    };
    // Change and Remove reach alternatives too, so a clashing alternative can be renamed or dropped.
    let planned = definition["exercises"].as_array().expect("exercises array");
    let list =
        if matches!(edit, ExerciseEdit::Add(_)) || find(planned)?.is_some() { "exercises" } else { "alternatives" };
    let names: Vec<String> = planned.iter().filter_map(|e| e["name"].as_str().map(String::from)).collect();
    let not_found = || -> Result<Value> {
        crate::bail!("EXERCISE_NOT_FOUND", "'{exercise}' isn't in {name} ({})", names.join(", "))
    };
    let rows = definition[list].as_array_mut().expect("exercise list");
    match edit {
        ExerciseEdit::Add(changes) => {
            let mut spec = match lookup(exercise) {
                Some(item) => {
                    let mut spec = serde_json::json!({"id": item.slug, "name": item.name, "role": "accessory", "guide_id": item.slug, "target_weight": 0});
                    if timed(Some(&item.slug)) {
                        // Cardio machines and holds are done for time unless the fields say otherwise.
                        spec["measure"] = serde_json::json!(Measure::Time);
                        spec["working_sets"] = serde_json::json!(1);
                    }
                    if default_load(&item.slug) == Load::Assisted {
                        spec["load"] = serde_json::json!(Load::Assisted);
                    }
                    spec
                }
                // The owner's own exercise, kept under their name without a picture (#101); the agent
                // names its muscle, since nothing else knows it.
                None if changes.contains_key("id") && changes.contains_key("muscle") => {
                    serde_json::json!({"name": exercise.trim(), "role": "accessory", "target_weight": 0})
                }
                None => crate::bail!(
                    "EXERCISE_UNKNOWN",
                    "{}; or, to keep the owner's name without a picture, pass fields.id and fields.muscle",
                    unknown(exercise)
                ),
            };
            spec.as_object_mut().unwrap().extend(changes.clone());
            // Exercises are told apart by id; the guide_id only picks the picture, so two machine
            // variants can share one.
            let same = |a: &Value, b: &Value| {
                a.as_str().zip(b.as_str()).is_some_and(|(a, b)| a.trim().eq_ignore_ascii_case(b.trim()))
            };
            if let Some(existing) = rows.iter().find(|e| same(&e["id"], &spec["id"]) || same(&e["name"], &spec["name"]))
            {
                let existing = existing["name"].as_str().unwrap_or_default();
                crate::bail!(
                    "EXERCISE_EXISTS",
                    "{existing} is already in {name}; edit it, or add a variant with its own id and name in fields"
                );
            }
            let at = position.unwrap_or(rows.len()).min(rows.len());
            rows.insert(at, spec);
        }
        ExerciseEdit::Remove => {
            let Some(i) = find(rows)? else { return not_found() };
            let removed = rows.remove(i);
            for row in rows.iter_mut() {
                if let Some(required) = row["requires_completed"].as_array_mut() {
                    required.retain(|r| r != &removed["id"]);
                }
            }
        }
        ExerciseEdit::Change(changes) => {
            let Some(i) = find(rows)? else { return not_found() };
            let row = rows[i].as_object_mut().unwrap();
            row.extend(changes.clone());
            // A new measure drops the other measure's target (back to reps: the time goes with it).
            if let Some(measure) = changes.get("measure") {
                for (field, owner) in [("minutes", Measure::Time), ("meters", Measure::Distance)] {
                    if *measure != serde_json::json!(owner) && !changes.contains_key(field) {
                        row.remove(field);
                    }
                }
            }
            let id = rows[i]["id"].as_str().unwrap_or_default().to_string();
            if let Some(at) = position {
                let row = rows.remove(i);
                rows.insert(at.min(rows.len()), row);
            }
            if let Some(weight) = changes.get("target_weight").and_then(Value::as_f64)
                && let Some(mut progression) = ExerciseProgression::get(db, &id)?
            {
                progression.next_weight = weight;
                progression.update(db)?;
            }
        }
    }
    let template: Template =
        serde_json::from_value(definition).map_err(|e| Error::domain("INVALID_TEMPLATE", e.to_string()))?;
    import_template(db, &template)
}

/// Warning when an edit or import drops exercise ids that have logged history. History and the
/// learned next weight are keyed by exercise id, so a new id starts from nothing.
pub fn orphaned_history(db: &Connection, before: &Template, after: &Template) -> Result<Option<String>> {
    let mut orphaned = vec![];
    for spec in before.exercises.iter().chain(&before.alternatives) {
        if after.spec(&spec.id).is_none() {
            let logged = WorkoutExercise::select(db, "WHERE exercise_id = ?1", [&spec.id])?.len();
            if logged > 0 {
                orphaned.push(format!("{} ({}, {logged} logged)", spec.name, spec.id));
            }
        }
    }
    Ok((!orphaned.is_empty()).then(|| {
        format!(
            "No longer in the plan, with history left behind: {}. History and next weight follow the exercise id, so to only rename or change the picture keep the id. If the owner always did the new exercise, move each past workout with workout relabel (workout_id, which).",
            orphaned.join(", ")
        )
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::test::db;

    fn seed() -> Template {
        serde_json::from_str(include_str!("testdata/template.json")).unwrap()
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn template_roundtrip_and_validation() {
        let db = db();
        let template = seed();
        import_template(&db, &template).unwrap();
        assert_eq!(get_template(&db, &template.id).unwrap(), template);
        let raw = serde_json::to_value(&template).unwrap();
        for change in ["warmup", "dependency", "role", "duplicate", "guide"] {
            let mut modified = raw.clone();
            match change {
                "warmup" => modified["exercises"][0]["warmup_weight"] = Value::Null,
                "dependency" => modified["exercises"][0]["requires_completed"] = json!(["pec-deck"]),
                "role" => modified["alternatives"][0]["role"] = json!("press"),
                "duplicate" => modified["alternatives"][0]["id"] = raw["exercises"][0]["id"].clone(),
                _ => modified["exercises"][0]["guide_id"] = json!("../wrong"),
            }
            let modified: Template = serde_json::from_value(modified).unwrap();
            assert_eq!(modified.validate().unwrap_err().code(), "INVALID_TEMPLATE", "{change}");
        }
    }

    /// 2026-11-21: a plan read off screenshots took a catalog search per exercise before it could be saved.
    #[test]
    fn import_matches_catalog_pictures_by_name_and_keeps_unknown_exercises() {
        let db = db();
        let exercise = |name: &str, extra: Value| {
            let mut spec =
                json!({"id": name.to_lowercase().replace(' ', "-"), "name": name, "role": name, "target_weight": 20});
            spec.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            spec
        };
        let template: Template = serde_json::from_value(json!({"id": "legs", "name": "Legs", "exercises": [
            exercise("Leg Press", json!({})),
            exercise("Lat Pulldown (Cable)", json!({})),
            exercise("Hack Squat", json!({"guide_id": "leg-press-ish"})),
            exercise("Grandma's Glute Thing", json!({})),
            exercise("Leg Extension", json!({"muscle": "Quads"})),
        ]}))
        .unwrap();
        let saved: Template = serde_json::from_value(import_template(&db, &template).unwrap()).unwrap();
        let pictures: Vec<Option<&str>> = saved.exercises.iter().map(|e| e.guide_id.as_deref()).collect();
        assert_eq!(pictures, [Some("leg-press"), Some("lat-pulldown"), Some("hack-squat"), None, None]);
        assert_eq!(get_template(&db, "legs").unwrap(), saved);
        let mp = missing_pictures(&saved).unwrap();
        assert!(mp.contains("Grandma's Glute Thing") && !mp.contains("Leg Extension"));
        // Informal muscle aliases ("Abs") normalize to catalog muscles instead of failing import.
        let abs_tpl: Template = serde_json::from_value(json!({"id": "core", "name": "Core", "exercises": [
            exercise("Ab Machine", json!({"muscle": "Abs"})),
        ]}))
        .unwrap();
        let saved_abs: Template = serde_json::from_value(import_template(&db, &abs_tpl).unwrap()).unwrap();
        assert_eq!(saved_abs.exercises[0].muscle.as_deref(), Some("Core"));
        assert!(missing_pictures(&saved_abs).is_none());
    }

    #[test]
    fn edit_one_exercise_without_reimport() {
        let db = db();
        import_template(&db, &seed()).unwrap();
        let ids = |definition: &Value| {
            definition["exercises"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let edited = edit_exercise(
            &db,
            "upper-a",
            "bench press",
            ExerciseEdit::Change(&object(json!({"target_weight": 85, "working_sets": 4}))),
            None,
        )
        .unwrap();
        assert_eq!(
            (edited["exercises"][0]["target_weight"].as_f64(), edited["exercises"][0]["working_sets"].as_i64()),
            (Some(85.0), Some(4))
        );
        let added = edit_exercise(&db, "upper-a", "face-pull", ExerciseEdit::Add(&Map::new()), Some(2)).unwrap();
        assert_eq!(ids(&added), ["bench", "row", "face-pull", "pec-deck", "lateral-raise"]);
        let again = edit_exercise(&db, "upper-a", "face-pull", ExerciseEdit::Add(&Map::new()), None).unwrap_err();
        assert_eq!(again.code(), "EXERCISE_EXISTS");
        // A second machine with the same picture is its own exercise.
        let variant = object(json!({"id": "bench-machine", "name": "Machine bench"}));
        let added = edit_exercise(&db, "upper-a", "bench-press", ExerciseEdit::Add(&variant), None).unwrap();
        assert_eq!(added["exercises"][5]["guide_id"], added["exercises"][0]["guide_id"]);
        let unnamed =
            edit_exercise(&db, "upper-a", "bench-press", ExerciseEdit::Add(&object(json!({"id": "bench-2"}))), None)
                .unwrap_err();
        assert_eq!(unnamed.code(), "EXERCISE_EXISTS", "a second 'Bench Press' could never be told apart");
        let shared = edit_exercise(&db, "upper-a", "bench-press", ExerciseEdit::Remove, None).unwrap_err();
        assert_eq!(shared.code(), "EXERCISE_AMBIGUOUS");
        let named = edit_exercise(
            &db,
            "upper-a",
            "Machine bench",
            ExerciseEdit::Change(&object(json!({"target_weight": 60}))),
            None,
        )
        .unwrap();
        assert_eq!(
            (named["exercises"][0]["target_weight"].as_f64(), named["exercises"][5]["target_weight"].as_f64()),
            (Some(85.0), Some(60.0))
        );
        // No catalog match: an error with the closest matches, or the owner's name kept without a picture.
        let unknown = edit_exercise(&db, "upper-a", "Zottman curl", ExerciseEdit::Add(&Map::new()), None).unwrap_err();
        assert_eq!(unknown.code(), "EXERCISE_UNKNOWN");
        let unnamed = object(json!({"id": "zottman"}));
        let unnamed = edit_exercise(&db, "upper-a", "Zottman curl", ExerciseEdit::Add(&unnamed), None).unwrap_err();
        assert!(unnamed.to_string().contains("fields.muscle"), "{unnamed}");
        let wrong = object(json!({"id": "zottman", "muscle": "Forearm-ish"}));
        assert!(edit_exercise(&db, "upper-a", "Zottman curl", ExerciseEdit::Add(&wrong), None).is_err());
        let own = object(json!({"id": "zottman", "muscle": "Biceps"}));
        let kept = edit_exercise(&db, "upper-a", "Zottman curl", ExerciseEdit::Add(&own), None).unwrap();
        let zottman = kept["exercises"].as_array().unwrap().last().unwrap();
        assert_eq!(
            (zottman["name"].as_str(), zottman.get("guide_id"), zottman["muscle"].as_str()),
            (Some("Zottman curl"), Some(&Value::Null), Some("Biceps"))
        );
        edit_exercise(&db, "upper-a", "zottman", ExerciseEdit::Remove, None).unwrap();
        let removed = edit_exercise(&db, "upper-a", "Pec Deck", ExerciseEdit::Remove, None).unwrap();
        assert!(!ids(&removed).contains(&"pec-deck".to_string()));
        let missing = edit_exercise(&db, "upper-a", "squat", ExerciseEdit::Change(&Map::new()), None).unwrap_err();
        assert_eq!(missing.code(), "EXERCISE_NOT_FOUND");
        assert!(missing.to_string().contains("Bench Press"), "{missing}");
        // A name taken by an alternative says which one, and the alternative itself can be renamed.
        let rename = object(json!({"name": "dumbbell fly"}));
        let clash = edit_exercise(&db, "upper-a", "row", ExerciseEdit::Change(&rename), None).unwrap_err();
        assert!(clash.to_string().contains("alternative 'Dumbbell Fly' (db-fly)"), "{clash}");
        let alt =
            edit_exercise(&db, "upper-a", "db-fly", ExerciseEdit::Change(&object(json!({"name": "Old fly"}))), None);
        assert_eq!(alt.unwrap()["alternatives"][0]["name"], "Old fly");
        edit_exercise(&db, "upper-a", "row", ExerciseEdit::Change(&rename), None).unwrap();
    }

    #[test]
    fn timed_exercises_need_minutes_and_lose_them_going_back_to_reps() {
        let db = db();
        import_template(&db, &seed()).unwrap();
        // A catalog cardio machine is timed by default; it can't be saved without its time.
        let bare = edit_exercise(&db, "upper-a", "cycling", ExerciseEdit::Add(&Map::new()), None).unwrap_err();
        assert!(bare.to_string().contains("Cycling is timed: pass minutes"), "{bare}");
        let added =
            edit_exercise(&db, "upper-a", "cycling", ExerciseEdit::Add(&object(json!({"minutes": 20}))), None).unwrap();
        let bike = &added["exercises"][4];
        assert_eq!(
            (bike["measure"].as_str(), bike["minutes"].as_f64(), bike["working_sets"].as_i64()),
            (Some("time"), Some(20.0), Some(1))
        );
        assert_eq!(get_template(&db, "upper-a").unwrap().exercises[4].set_seconds(), Some(1200));
        // Minutes on a lift is a mistake, not a silent no-op.
        let lift = edit_exercise(&db, "upper-a", "row", ExerciseEdit::Change(&object(json!({"minutes": 5}))), None);
        assert_eq!(lift.unwrap_err().code(), "INVALID_TEMPLATE");
        let back =
            edit_exercise(&db, "upper-a", "cycling", ExerciseEdit::Change(&object(json!({"measure": "reps"}))), None)
                .unwrap();
        assert!(back["exercises"][4].get("minutes").is_none() && back["exercises"][4]["measure"] == "reps");
    }
}
