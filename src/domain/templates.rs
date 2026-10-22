//! Workout templates: validated on import, stored as JSON, snapshotted when a workout starts.
use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::models::{ExerciseProgression, WorkoutTemplate};
use crate::db::{Connection, Json};
use crate::domain::illustrations::{catalog, search};
use crate::error::{Error, Result};
use crate::time::Ts;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExerciseSpec {
    pub id: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub guide_id: Option<String>,
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
    #[serde(default)]
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
        if ![&self.id, &self.name, &self.role].iter().all(|v| identifier(v)) || self.requires_completed.iter().chain(&self.substitutes).any(|v| !identifier(v)) {
            return invalid("IDs, names and roles are 1–100 characters");
        }
        if let Some(guide) = &self.guide_id
            && !catalog().contains_key(guide)
        {
            return invalid(&format!("guide_id must match Workout Guide catalog: {guide}"));
        }
        if self.working_sets <= 0 || self.rep_min <= 0 || self.rep_max <= 0 || self.warmup_reps <= 0 || self.rest_seconds.is_some_and(|r| r <= 0) || self.priority < 0 {
            return invalid("Sets, reps and rest must be positive");
        }
        let weights = [Some(self.target_weight), self.warmup_weight];
        if weights.iter().flatten().any(|w| !w.is_finite() || *w < 0.0) || !self.increment.is_finite() || self.increment <= 0.0 {
            return invalid("Weights must be ≥ 0 and the increment > 0");
        }
        if self.rep_min > self.rep_max {
            return invalid("Rep minimum exceeds maximum");
        }
        Ok(())
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
        let ids: HashSet<&str> = all.iter().map(|s| s.id.as_str()).collect();
        if ids.len() != all.len() {
            return invalid("Exercise IDs must be unique within template");
        }
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
                let ok = self.alternatives.iter().find(|a| &a.id == substitute).is_some_and(|a| a.role == spec.role && substitute != &spec.id);
                if !ok {
                    return invalid("Substitutes must reference same-role alternatives");
                }
            }
        }
        if let Some(first) = self.exercises.iter().find(|s| s.primary)
            && first.warmup_weight.is_none()
        {
            return invalid("First primary exercise requires explicit warmup_weight");
        }
        Ok(())
    }

    pub fn spec(&self, id: &str) -> Option<&ExerciseSpec> {
        self.exercises.iter().chain(&self.alternatives).find(|s| s.id == id)
    }
}

/// Owner templates need a catalog guide_id on every exercise, so every card has an image.
pub fn require_illustrations(template: &Template) -> Result<()> {
    let missing: Vec<&str> = template.exercises.iter().chain(&template.alternatives).filter(|s| s.guide_id.is_none()).map(|s| s.name.as_str()).collect();
    if !missing.is_empty() {
        crate::bail!("ILLUSTRATION_REQUIRED", "Set guide_id (closest match from catalog search) for: {}", missing.join(", "));
    }
    Ok(())
}

pub fn import_template(db: &Connection, template: &Template) -> Result<Value> {
    template.validate()?;
    let definition = serde_json::to_value(template)?;
    let row = WorkoutTemplate { id: template.id.clone(), definition_json: Json(definition.clone()), updated_at: Ts::now() };
    if WorkoutTemplate::get(db, &template.id)?.is_some() { row.update(db)? } else { row.insert(db)? }
    Ok(definition)
}

pub fn get_template(db: &Connection, template_id: &str) -> Result<Template> {
    let Some(row) = WorkoutTemplate::get(db, template_id)? else {
        crate::bail!("TEMPLATE_UNKNOWN", "Unknown workout template: {template_id}. Templates: {}", template_ids(db)?.join(", "));
    };
    serde_json::from_value(row.definition_json.0).map_err(|e| Error::domain("INVALID_TEMPLATE", format!("Stored template {template_id}: {e}")))
}

pub fn template_ids(db: &Connection) -> Result<Vec<String>> {
    Ok(WorkoutTemplate::select(db, "ORDER BY id", [])?.into_iter().map(|t| t.id).collect())
}

/// Owner swapped an exercise mid-workout: the replacement takes its place in the saved template, and
/// the original stays as an alternative so it's one swap away. False if the template can't take it.
pub fn remember_swap(db: &Connection, template_id: &str, original_id: &str, replacement: &ExerciseSpec) -> Result<bool> {
    let Ok(template) = get_template(db, template_id) else { return Ok(false) };
    let Some(index) = template.exercises.iter().position(|e| e.id == original_id) else { return Ok(false) };
    let original = template.exercises[index].clone();
    let new = ExerciseSpec {
        role: original.role.clone(),
        primary: original.primary,
        working_sets: original.working_sets,
        rep_min: original.rep_min,
        rep_max: original.rep_max,
        rest_seconds: original.rest_seconds,
        priority: original.priority,
        requires_completed: original.requires_completed.clone(),
        substitutes: vec![original_id.into()],
        ..replacement.clone()
    };
    let mut alternatives: Vec<ExerciseSpec> = template.alternatives.iter().filter(|a| a.id != replacement.id && a.id != original_id).cloned().collect();
    alternatives.push(ExerciseSpec { substitutes: vec![], requires_completed: vec![], ..original });
    let mut exercises: Vec<ExerciseSpec> = template
        .exercises
        .iter()
        .map(|e| ExerciseSpec {
            requires_completed: e.requires_completed.iter().map(|r| if r == original_id { replacement.id.clone() } else { r.clone() }).collect(),
            substitutes: e.substitutes.iter().filter(|s| **s != replacement.id).cloned().collect(),
            ..e.clone()
        })
        .collect();
    exercises[index] = new;
    let updated = Template { exercises, alternatives, ..template };
    if updated.validate().is_err() {
        return Ok(false);
    }
    import_template(db, &updated)?;
    Ok(true)
}

pub enum ExerciseEdit<'a> {
    /// ExerciseSpec fields to change; a new target_weight also resets the learned next weight.
    Change(&'a Map<String, Value>),
    /// `exercise` is a catalog guide_id or name; fields override the defaults.
    Add(&'a Map<String, Value>),
    Remove,
}

/// Change one exercise of a saved plan without re-importing it. `exercise` is an ID, guide_id or name;
/// `position` is the 0-based place. Upcoming planned sessions pick the change up via `refresh_plans`.
pub fn edit_exercise(db: &Connection, template_id: &str, exercise: &str, edit: ExerciseEdit, position: Option<usize>) -> Result<Value> {
    let mut definition = serde_json::to_value(get_template(db, template_id)?)?;
    let name = definition["name"].as_str().unwrap_or_default().to_string();
    let rows = definition["exercises"].as_array_mut().expect("exercises array");
    let key = exercise.trim().to_lowercase();
    let matches = |e: &Value| [&e["id"], &e["name"], &e["guide_id"]].iter().any(|v| v.as_str().is_some_and(|s| s.to_lowercase() == key));
    let index = rows.iter().position(matches);
    match edit {
        ExerciseEdit::Add(changes) => {
            let item = catalog().get(&key).or_else(|| catalog().values().find(|i| i.name.to_lowercase() == key));
            let Some(item) = item else {
                let options: Vec<String> = search(exercise, None, None, 5).iter().map(|o| format!("{} ({})", o.guide_id, o.name)).collect();
                let options = if options.is_empty() { "try catalog search".into() } else { options.join(", ") };
                crate::bail!("EXERCISE_UNKNOWN", "No exact match for '{exercise}'. Pass one guide_id: {options}");
            };
            if let Some(i) = index {
                crate::bail!("EXERCISE_EXISTS", "{} is already in {name}; edit it instead", rows[i]["name"].as_str().unwrap_or_default());
            }
            let mut spec = serde_json::json!({"id": item.slug, "name": item.name, "role": "accessory", "guide_id": item.slug, "target_weight": 0});
            spec.as_object_mut().unwrap().extend(changes.clone());
            let at = position.unwrap_or(rows.len()).min(rows.len());
            rows.insert(at, spec);
        }
        ExerciseEdit::Remove => {
            let Some(i) = index else { return not_found(exercise, &name, rows) };
            let removed = rows.remove(i);
            for row in rows.iter_mut() {
                if let Some(required) = row["requires_completed"].as_array_mut() {
                    required.retain(|r| r != &removed["id"]);
                }
            }
        }
        ExerciseEdit::Change(changes) => {
            let Some(i) = index else { return not_found(exercise, &name, rows) };
            rows[i].as_object_mut().unwrap().extend(changes.clone());
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
    let template: Template = serde_json::from_value(definition).map_err(|e| Error::domain("INVALID_TEMPLATE", e.to_string()))?;
    import_template(db, &template)
}

fn not_found<T>(exercise: &str, name: &str, rows: &[Value]) -> Result<T> {
    let names: Vec<&str> = rows.iter().filter_map(|e| e["name"].as_str()).collect();
    crate::bail!("EXERCISE_NOT_FOUND", "'{exercise}' isn't in {name} ({})", names.join(", "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::workout::tests::db;

    fn seed() -> Template {
        serde_json::from_str(&std::fs::read_to_string("config/exercises.seed.json").unwrap()).unwrap()
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

    #[test]
    fn edit_one_exercise_without_reimport() {
        let db = db();
        import_template(&db, &seed()).unwrap();
        let ids = |definition: &Value| definition["exercises"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let edited = edit_exercise(&db, "upper-a", "bench press", ExerciseEdit::Change(&object(json!({"target_weight": 85, "working_sets": 4}))), None).unwrap();
        assert_eq!((edited["exercises"][0]["target_weight"].as_f64(), edited["exercises"][0]["working_sets"].as_i64()), (Some(85.0), Some(4)));
        let added = edit_exercise(&db, "upper-a", "face-pull", ExerciseEdit::Add(&Map::new()), Some(2)).unwrap();
        assert_eq!(ids(&added), ["bench", "row", "face-pull", "pec-deck", "lateral-raise"]);
        let removed = edit_exercise(&db, "upper-a", "Pec Deck", ExerciseEdit::Remove, None).unwrap();
        assert!(!ids(&removed).contains(&"pec-deck".to_string()));
        let missing = edit_exercise(&db, "upper-a", "squat", ExerciseEdit::Change(&Map::new()), None).unwrap_err();
        assert_eq!(missing.code(), "EXERCISE_NOT_FOUND");
        assert!(missing.to_string().contains("Bench Press"), "{missing}");
    }
}
