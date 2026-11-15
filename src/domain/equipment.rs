//! Today's equipment: a workout rebuilt for a hotel or home gym. Each exercise whose equipment isn't
//! there is swapped for the closest same-muscle catalog exercise that fits (`illustrations::fitting`),
//! the way a mid-workout swap is: the replacement keeps the slot (sets, reps, rest, role) under its own
//! id, so its weights come from its own history, and the original stays one swap away as an
//! alternative. Code proposes; the agent reviews, adjusts and saves.
use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use crate::db::models::{ExerciseProgression, GymLocation};
use crate::db::{Connection, Json};
use crate::domain::illustrations::{Item, catalog, closest, fitting, needs, timed};
use crate::domain::templates::{ExerciseSpec, Measure, Template, pick};
use crate::error::Result;
use crate::time::Ts;

/// Equipment every place has: the floor, a wall, a towel, a doorway, a chair.
const ALWAYS: [&str; 5] = ["Bodyweight", "Wall", "Towel", "Doorway", "Chair"];

/// The owner's equipment as catalog names (case-insensitive). An unknown name fails with the list.
pub fn equipment_names(input: &[String]) -> Result<Vec<String>> {
    let mut known: Vec<&str> = catalog().values().map(|i| i.equipment.as_str()).chain(["Pull-up Bar"]).collect();
    known.sort();
    known.dedup();
    let mut names = vec![];
    for wanted in input {
        let Some(name) = known.iter().find(|k| k.eq_ignore_ascii_case(wanted.trim())) else {
            crate::bail!("EQUIPMENT_UNKNOWN", "Unknown equipment '{wanted}'. Catalog equipment: {}", known.join(", "));
        };
        if !names.contains(&name.to_string()) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// Save (or replace) a training place; no equipment deletes it.
pub fn save_location(db: &Connection, name: &str, equipment: &[String], notes: Option<&str>, now: Ts) -> Result<()> {
    let name = name.trim().to_lowercase();
    if let Some(old) = GymLocation::get(db, &name)? {
        old.delete(db)?;
    }
    if !equipment.is_empty() {
        let notes = notes.map(String::from);
        GymLocation { name, equipment_json: Json(equipment_names(equipment)?), notes, updated_at: now }.insert(db)?;
    }
    Ok(())
}

pub fn location(db: &Connection, name: &str) -> Result<GymLocation> {
    match GymLocation::get(db, name.trim().to_lowercase())? {
        Some(place) => Ok(place),
        None => {
            let saved: Vec<String> =
                GymLocation::select(db, "ORDER BY name", [])?.into_iter().map(|l| l.name).collect();
            crate::bail!(
                "LOCATION_UNKNOWN",
                "No saved location '{name}'. Saved: {}. Pass equipment to save it.",
                if saved.is_empty() { "none".into() } else { saved.join(", ") }
            )
        }
    }
}

/// What the owner said for one exercise: keep it, or this catalog guide_id.
pub enum Choice<'a> {
    Keep,
    Use(&'a str),
}

/// `template` rebuilt for `equipment` (catalog names; `ALWAYS` is implied). `choices` override the
/// proposal per exercise (id or name). Returns the rebuilt template and, per exercise, what changed
/// with the other options. An exercise without any catalog match is kept and flagged.
pub fn rebuild(
    db: &Connection,
    template: &Template,
    equipment: &[String],
    choices: &BTreeMap<String, Choice>,
) -> Result<(Template, Vec<Value>)> {
    let fits = |item: &Item| {
        let need = needs(item);
        ALWAYS.contains(&need) || equipment.iter().any(|e| e == need)
    };
    let keys: Vec<[Option<&str>; 3]> =
        template.exercises.iter().map(|s| [Some(s.id.as_str()), Some(s.name.as_str()), None]).collect();
    let mut chosen: Vec<Option<&Choice>> = vec![None; template.exercises.len()];
    for (key, choice) in choices {
        match pick(key, &keys)? {
            Some(i) => chosen[i] = Some(choice),
            None => crate::bail!("EXERCISE_NOT_FOUND", "'{key}' isn't in {}", template.name),
        }
    }
    let mut stmt = db.prepare_cached(
        "SELECT DISTINCT e.exercise_id FROM workout_exercise e JOIN set_log s ON s.workout_exercise_id = e.id",
    )?;
    let history: HashSet<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    // Ids, names and pictures already taken: a replacement must not clash with any of them.
    let mut used: HashSet<String> = template
        .exercises
        .iter()
        .chain(&template.alternatives)
        .flat_map(|s| [s.id.clone(), s.name.to_lowercase()].into_iter().chain(s.guide_id.clone()))
        .collect();
    let (mut exercises, mut alternatives, mut report) = (vec![], template.alternatives.clone(), vec![]);
    let mut renamed: Vec<(String, String)> = vec![];
    for (spec, choice) in template.exercises.iter().zip(chosen) {
        let guide = spec.guide_id.clone().or_else(|| closest(&spec.name).map(|e| e.guide_id));
        let item = guide.as_deref().and_then(|g| catalog().get(g));
        let pictured = |s: &ExerciseSpec| s.guide_id.as_deref().and_then(|g| catalog().get(g));
        // The plan's own alternatives first, as in a mid-workout swap.
        let saved = spec
            .substitutes
            .iter()
            .filter_map(|id| template.alternatives.iter().find(|a| &a.id == id))
            .find(|a| pictured(a).is_some_and(fits));
        let (mut replacement, others): (Option<ExerciseSpec>, Vec<String>) = match (item, choice) {
            (_, Some(Choice::Keep)) => (None, vec![]),
            (_, Some(Choice::Use(slug))) => match catalog().get(*slug) {
                Some(to) => (Some(catalog_spec(db, spec, to)?), vec![]),
                None => crate::bail!("EXERCISE_UNKNOWN", "'{slug}' isn't a catalog guide_id; see catalog_search"),
            },
            (Some(item), None) if !fits(item) => {
                let options: Vec<String> =
                    fitting(&item.slug, fits, &used, &history, 3).into_iter().map(|e| e.guide_id).collect();
                match (saved, options.split_first()) {
                    (Some(alternative), _) => (Some(alternative.clone()), options),
                    (None, Some((first, rest))) => (Some(catalog_spec(db, spec, &catalog()[first])?), rest.to_vec()),
                    (None, None) => (None, vec![]),
                }
            }
            _ => (None, vec![]),
        };
        let Some(new) = replacement.take() else {
            let note = match (item, choice) {
                (None, None) => Some("no catalog match to check its equipment"),
                (Some(i), None) if !fits(i) => Some("nothing fits; keep, skip or pick one"),
                _ => None,
            };
            report.push(json!({"exercise": spec.name, "keep": true, "note": note}));
            exercises.push(spec.clone());
            continue;
        };
        let to = pictured(&new).expect("replacements are catalog exercises");
        used.extend([new.id.clone(), new.name.to_lowercase(), to.slug.clone()]);
        report.push(json!({"exercise": spec.name, "to": {"id": new.id, "name": new.name, "equipment": to.equipment},
            "target_weight": new.target_weight, "other_options": others}));
        renamed.push((spec.id.clone(), new.id.clone()));
        alternatives.retain(|a| a.id != spec.id && a.id != new.id);
        alternatives.push(ExerciseSpec { substitutes: vec![], requires_completed: vec![], ..spec.clone() });
        // The replacement takes the slot: sets, reps, rest, role and order stay the original's.
        exercises.push(ExerciseSpec {
            role: spec.role.clone(),
            primary: spec.primary,
            working_sets: spec.working_sets,
            rep_min: spec.rep_min,
            rep_max: spec.rep_max,
            rest_seconds: spec.rest_seconds,
            priority: spec.priority,
            requires_completed: spec.requires_completed.clone(),
            substitutes: vec![spec.id.clone()],
            ..new
        });
    }
    // Dependencies follow the swapped ids.
    for spec in &mut exercises {
        for required in &mut spec.requires_completed {
            if let Some((_, new)) = renamed.iter().find(|(old, _)| old == required) {
                *required = new.clone();
            }
        }
    }
    let rebuilt = Template { exercises, alternatives, ..template.clone() };
    rebuilt.validate()?;
    Ok((rebuilt, report))
}

/// A catalog exercise in `original`'s slot, under its own id: its weight from its own history
/// (unknown: 0, and the card asks).
fn catalog_spec(db: &Connection, original: &ExerciseSpec, to: &Item) -> Result<ExerciseSpec> {
    let target = ExerciseProgression::get(db, &to.slug)?.map_or(0.0, |p| p.next_weight);
    let is_timed = timed(Some(&to.slug));
    Ok(ExerciseSpec {
        id: to.slug.clone(),
        name: to.name.clone(),
        guide_id: Some(to.slug.clone()),
        target_weight: target,
        // Half the working weight on the plate steps, if the original had a warm-up.
        warmup_weight: original
            .warmup_weight
            .filter(|_| !is_timed)
            .map(|_| (target / 2.0 / original.increment).floor() * original.increment),
        measure: if is_timed { Measure::Time } else { Measure::Reps },
        minutes: if is_timed { original.minutes.or(Some(10.0)) } else { None },
        ..original.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test::db;

    fn template() -> Template {
        serde_json::from_str(include_str!("testdata/template.json")).unwrap()
    }

    fn dumbbells() -> Vec<String> {
        equipment_names(&["dumbbell".into(), "bench".into()]).unwrap()
    }

    #[test]
    fn rebuild_swaps_what_doesnt_fit_and_keeps_the_rest_one_swap_away() {
        let db = db();
        let (rebuilt, report) = rebuild(&db, &template(), &dumbbells(), &BTreeMap::new()).unwrap();
        let ids: Vec<&str> = rebuilt.exercises.iter().map(|e| e.id.as_str()).collect();
        // Barbell bench → dumbbell bench; the pec deck → the plan's own dumbbell fly; raises fit.
        assert_eq!((ids[0], ids[2], ids[3]), ("dumbbell-bench-press", "db-fly", "lateral-raise"));
        assert!(
            rebuilt
                .exercises
                .iter()
                .all(|e| ["Dumbbell", "Bench", "Bodyweight"]
                    .contains(&needs(&catalog()[e.guide_id.as_deref().unwrap()])))
        );
        let bench = &rebuilt.exercises[0];
        assert_eq!(
            (bench.working_sets, bench.primary, bench.substitutes.as_slice()),
            (3, true, ["bench".to_string()].as_slice())
        );
        assert!(rebuilt.spec("bench").is_some(), "the barbell bench is an alternative now");
        assert_eq!(report[3]["keep"], true);
    }

    #[test]
    fn owner_choices_override_the_proposal() {
        let db = db();
        let choices = BTreeMap::from([
            ("Bench Press".to_string(), Choice::Keep),
            ("row".to_string(), Choice::Use("one-arm-dumbbell-row")),
        ]);
        let (rebuilt, _) = rebuild(&db, &template(), &dumbbells(), &choices).unwrap();
        assert_eq!(
            (rebuilt.exercises[0].id.as_str(), rebuilt.exercises[1].id.as_str()),
            ("bench", "one-arm-dumbbell-row")
        );
        let wrong = BTreeMap::from([("squat".to_string(), Choice::Keep)]);
        assert_eq!(rebuild(&db, &template(), &dumbbells(), &wrong).unwrap_err().code(), "EXERCISE_NOT_FOUND");
        assert_eq!(equipment_names(&["laser".into()]).unwrap_err().code(), "EQUIPMENT_UNKNOWN");
    }
}
