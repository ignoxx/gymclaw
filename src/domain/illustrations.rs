//! Workout Guide first-pose assets and catalog search. An exercise's `guide_id` picks its picture; one
//! without (the owner kept their own name) shows the closest catalog picture meanwhile and is on the
//! `missing` list that `scripts/new-illustration` works through.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub slug: String,
    pub name: String,
    pub exercise_type: String,
    pub equipment: String,
    pub primary_muscle: String,
    #[serde(default)]
    pub is_stretch: bool,
    #[serde(default)]
    pub frames: Vec<serde_json::Value>,
}

/// Catalog entry as tools return it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Entry {
    pub guide_id: String,
    pub name: String,
    pub equipment: String,
    pub primary_muscle: String,
    pub svg_url: String,
    pub telegram_png: String,
}

/// Slug → item, sorted by slug (manifest order is not needed for any result).
static CATALOG: LazyLock<BTreeMap<String, Item>> = LazyLock::new(|| {
    let path = crate::config::assets_dir().join("manifest.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("workout guide manifest {}: {e}", path.display()));
    let items: Vec<Item> = serde_json::from_str(&text).expect("valid workout guide manifest");
    items.into_iter().map(|i| (i.slug.clone(), i)).collect()
});

pub fn catalog() -> &'static BTreeMap<String, Item> {
    &CATALOG
}

fn entry(item: &Item) -> Entry {
    Entry {
        guide_id: item.slug.clone(),
        name: item.name.clone(),
        equipment: item.equipment.clone(),
        primary_muscle: item.primary_muscle.clone(),
        svg_url: format!("/illustrations/{}.svg", item.slug),
        telegram_png: crate::config::assets_dir().join(&item.slug).join("frame-1.png").to_string_lossy().into(),
    }
}

/// The picture of an exercise: its guide_id, else the closest catalog match by name (`closest`).
pub fn for_exercise(name: &str, guide_id: Option<&str>) -> Option<Entry> {
    match guide_id {
        Some(id) => catalog().get(id).map(entry),
        None => closest(name),
    }
}

/// Stand-in picture for an exercise without one of its own: the best catalog match that shares a
/// movement word with the name ("Hip Thrust Machine" → Hip Thrust). A name sharing only equipment
/// ("some pulling machine") gets none: a wrong picture mid-workout is worse than no picture.
pub fn closest(name: &str) -> Option<Entry> {
    let movement: HashSet<String> =
        words(name).into_iter().filter(|w| !EQUIPMENT_WORDS.contains(&w.as_str())).collect();
    rank(name, None, None).into_iter().find(|s| !words(&s.item.name).is_disjoint(&movement)).map(|s| entry(s.item))
}

/// The catalog entry a name from another app means, when it names one exactly: every word is the
/// entry's name or its equipment ("Bench Press (Barbell)", "Lat Pulldown (Cable)", "RDL"), nothing more.
pub fn matching(name: &str) -> Option<Entry> {
    rank(name, None, None).into_iter().next().filter(|s| s.exact).map(|s| entry(s.item))
}

pub fn primary_muscle(guide_id: Option<&str>) -> Option<String> {
    guide_id.and_then(|g| catalog().get(g)).map(|i| i.primary_muscle.clone())
}

/// Every catalog muscle: what an exercise without a picture may name as its `muscle`.
pub fn muscles() -> BTreeSet<&'static str> {
    catalog().values().map(|i| i.primary_muscle.as_str()).collect()
}

/// Reps are the whole set (dips, push-ups): no weight to log or ask for.
pub fn bodyweight(guide_id: Option<&str>) -> bool {
    guide_id.and_then(|g| catalog().get(g)).is_some_and(|i| i.exercise_type.starts_with("bodyweight"))
}

/// Done for time in the catalog (cardio machines, planks).
pub fn timed(guide_id: Option<&str>) -> bool {
    guide_id.and_then(|g| catalog().get(g)).is_some_and(|i| i.exercise_type.ends_with("duration"))
}

/// Gym shorthand the owner types ("DB RDL") and other apps' spellings ("Skullcrusher", "Pullup",
/// "Hip Abductor"), spelled out the way catalog names are.
const ALIASES: &[(&str, &str)] = &[
    ("db", "dumbbell"),
    ("bb", "barbell"),
    ("kb", "kettlebell"),
    ("rdl", "romanian deadlift"),
    ("sldl", "stiff leg deadlift"),
    ("ohp", "overhead press"),
    ("military", "overhead"),
    ("ext", "extension"),
    ("tri", "tricep"),
    ("bi", "bicep"),
    ("skullcrusher", "skull crusher"),
    ("pullup", "pull up"),
    ("chinup", "chin up"),
    ("pushup", "push up"),
    ("situp", "sit up"),
    ("abductor", "abduction"),
    ("adductor", "adduction"),
    // A pec deck ("butterfly") is a machine fly.
    ("butterfly", "fly"),
    ("deck", "fly"),
];
/// Name words that say where an exercise is done, not what the movement is. Other apps put them in
/// brackets: "Bench Press (Barbell)".
const EQUIPMENT_WORDS: &[&str] =
    &["cable", "dumbbell", "barbell", "machine", "kettlebell", "smith", "band", "plate", "bodyweight"];

fn words(text: &str) -> HashSet<String> {
    let lower = text.to_lowercase();
    let spelled: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_lowercase())
        .filter(|w| !w.is_empty())
        .map(|w| ALIASES.iter().find(|(k, _)| *k == w).map_or(w, |(_, v)| *v))
        .collect();
    spelled.join(" ").split_whitespace().map(|w| w.trim_end_matches('s').to_string()).collect()
}

/// Inverse document frequency of name words: 'incline' says more than 'machine'.
static RARITY: LazyLock<HashMap<String, f64>> = LazyLock::new(|| {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in catalog().values() {
        for word in words(&item.name) {
            *counts.entry(word).or_default() += 1;
        }
    }
    let total = catalog().len() as f64;
    counts.into_iter().map(|(w, c)| (w, (total / c as f64).ln())).collect()
});

struct Scored {
    score: f64,
    /// Every query word is the entry's name or equipment, and its name has no other word.
    exact: bool,
    item: &'static Item,
}

/// Catalog entries by rarity-weighted words shared with the query, best first. An exact match
/// ("Squat (Barbell)" → Squat) ranks first; an equipment word the entry lacks ranks it down.
fn rank(query: &str, muscle: Option<&str>, equipment: Option<&str>) -> Vec<Scored> {
    let wanted = words(query);
    let gear: Vec<&String> = wanted.iter().filter(|w| EQUIPMENT_WORDS.contains(&w.as_str())).collect();
    let mut results: Vec<Scored> = vec![];
    for item in catalog().values() {
        if muscle.is_some_and(|m| item.primary_muscle.to_lowercase() != m.to_lowercase())
            || equipment.is_some_and(|e| item.equipment.to_lowercase() != e.to_lowercase())
        {
            continue;
        }
        let name = words(&item.name);
        let shared: f64 = wanted.intersection(&name).map(|w| RARITY.get(w).copied().unwrap_or(1.0)).sum();
        let context = words(&format!("{} {}", item.equipment, item.primary_muscle));
        let has = |w: &String| name.contains(w) || context.contains(w);
        let exact = !wanted.is_empty() && name.is_subset(&wanted) && wanted.iter().all(has);
        let mut score =
            shared + 0.5 * wanted.intersection(&context).count() as f64 - 0.1 * name.difference(&wanted).count() as f64;
        if exact {
            score += 3.0;
        }
        if !gear.is_empty() && !gear.iter().any(|g| has(g)) {
            score -= 2.0;
        }
        if !wanted.is_empty() && score <= 0.0 {
            continue;
        }
        results.push(Scored { score, exact, item });
    }
    results.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.item.name.cmp(&b.item.name)));
    results
}

/// Search the catalog (see `rank`); filters are case-insensitive exact matches.
pub fn search(query: &str, muscle: Option<&str>, equipment: Option<&str>, limit: usize) -> Vec<Entry> {
    rank(query, muscle, equipment).into_iter().take(limit).map(|s| entry(s.item)).collect()
}

/// The catalog item a guide_id or exact name (any case) names.
pub fn lookup(key: &str) -> Option<&'static Item> {
    let key = key.trim().to_lowercase();
    catalog().get(&key).or_else(|| catalog().values().find(|i| i.name.to_lowercase() == key))
}

/// The top 3 search matches' guide_ids, for "closest: …" hints.
pub fn suggestions(name: &str) -> Vec<String> {
    search(name, None, None, 3).into_iter().map(|e| e.guide_id).collect()
}

/// EXERCISE_UNKNOWN for a name `lookup` doesn't know, with the best search matches to pick from.
pub fn unknown(query: &str) -> crate::error::Error {
    let options: Vec<String> =
        search(query, None, None, 5).iter().map(|o| format!("{} ({})", o.guide_id, o.name)).collect();
    let options = if options.is_empty() { "try catalog search".into() } else { options.join(", ") };
    crate::error::Error::domain(
        "EXERCISE_UNKNOWN",
        format!("No exact match for '{query}'. Pass one guide_id: {options}"),
    )
}

/// Identity of an entry's picture. A few catalog entries are the same movement under two names with
/// the same art (Leg Curl / Lying Leg Curl); offering one for the other isn't a swap.
pub fn artwork(slug: &str) -> String {
    catalog()
        .get(slug)
        .and_then(|i| i.frames.first())
        .and_then(|f| f.pointer("/attribution/source/url"))
        .and_then(|u| u.as_str())
        .map_or_else(|| slug.to_string(), str::to_string)
}

/// Same primary muscle and exercise type, no stretches, never the same artwork as the original or an
/// excluded entry. Shared movement words rank first, then `prefer` slugs (e.g. owner history), then
/// different equipment (the original may be occupied).
pub fn similar(guide_id: &str, exclude: &HashSet<String>, prefer: &HashSet<String>, limit: usize) -> Vec<Entry> {
    let Some(original) = catalog().get(guide_id) else { return vec![] };
    let base: HashSet<String> =
        words(&original.name).into_iter().filter(|w| !EQUIPMENT_WORDS.contains(&w.as_str())).collect();
    let taken: HashSet<String> = exclude
        .iter()
        .chain(std::iter::once(&guide_id.to_string()))
        .filter(|s| catalog().contains_key(*s))
        .map(|s| artwork(s))
        .collect();
    let mut options: Vec<((isize, bool, bool, String), Entry)> = vec![];
    for item in catalog().values() {
        if exclude.contains(&item.slug) || taken.contains(&artwork(&item.slug)) || item.is_stretch {
            continue;
        }
        if item.primary_muscle != original.primary_muscle || item.exercise_type != original.exercise_type {
            continue;
        }
        let shared = base.intersection(&words(&item.name)).count() as isize;
        options.push((
            (-shared, !prefer.contains(&item.slug), item.equipment == original.equipment, item.name.clone()),
            entry(item),
        ));
    }
    options.sort_by(|a, b| a.0.cmp(&b.0));
    let ranked: Vec<Entry> = options.into_iter().map(|(_, e)| e).collect();
    // Near-duplicates exist ("Rear Delt Fly" vs "Bent-Over Rear Delt Raise"): different equipment first.
    let mut picked: Vec<Entry> = vec![];
    for option in &ranked {
        if picked.len() < limit && !picked.iter().any(|p| p.equipment == option.equipment) {
            picked.push(option.clone());
        }
    }
    let rest: Vec<Entry> =
        ranked.into_iter().filter(|o| !picked.contains(o)).take(limit.saturating_sub(picked.len())).collect();
    picked.extend(rest);
    picked
}

/// Equipment an entry needs. The catalog files pull-ups, chin-ups and hanging raises under
/// Bodyweight; they need a bar.
pub fn needs(item: &Item) -> &str {
    let name = words(&item.name);
    if item.equipment == "Bodyweight" && ["pull", "chin", "hanging"].iter().any(|w| name.contains(*w)) {
        "Pull-up Bar"
    } else {
        &item.equipment
    }
}

/// Same-muscle replacements for `guide_id` that `fits` allows (a hotel gym's equipment), best first:
/// shared movement words, then the same kind of exercise (weights for weights), then `prefer` (owner
/// history). Timed stays timed; no stretches; never the same artwork or an `exclude`d entry.
pub fn fitting(
    guide_id: &str,
    fits: impl Fn(&Item) -> bool,
    exclude: &HashSet<String>,
    prefer: &HashSet<String>,
    limit: usize,
) -> Vec<Entry> {
    let Some(original) = catalog().get(guide_id) else { return vec![] };
    let base: HashSet<String> =
        words(&original.name).into_iter().filter(|w| !EQUIPMENT_WORDS.contains(&w.as_str())).collect();
    let picture = artwork(guide_id);
    let mut options: Vec<&Item> = catalog()
        .values()
        .filter(|i| i.primary_muscle == original.primary_muscle && !i.is_stretch && fits(i))
        .filter(|i| timed(Some(&i.slug)) == timed(Some(guide_id)))
        .filter(|i| !exclude.contains(&i.slug) && artwork(&i.slug) != picture)
        .collect();
    options.sort_by_cached_key(|i| {
        let shared = base.intersection(&words(&i.name)).count() as isize;
        (-shared, i.exercise_type != original.exercise_type, !prefer.contains(&i.slug), i.name.clone())
    });
    options.into_iter().take(limit).map(entry).collect()
}

/// An exercise with no picture of its own, for `gymclaw illustrations missing`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Missing {
    pub exercise_id: String,
    pub name: String,
    /// In a saved plan (template exercise or alternative).
    pub in_plan: bool,
    pub logged_sets: i64,
    /// The catalog picture shown meanwhile (`closest`).
    pub stand_in: Option<String>,
}

/// Exercises with no picture of their own in the saved plans and logged workouts: the work list for
/// `scripts/new-illustration`, plan exercises first, then by logged sets. Not missing: a logged
/// exercise whose plan entry (same id) has a picture (early workouts predate guide_ids), a name that
/// is a catalog name in other words ("Triceps pushdown"), and the demo plan.
pub fn missing(db: &crate::db::Connection) -> crate::error::Result<Vec<Missing>> {
    let mut pictured: HashSet<String> = catalog().keys().cloned().collect();
    let mut found: BTreeMap<String, Missing> = BTreeMap::new();
    let named: HashSet<Vec<String>> = catalog().values().map(|i| sorted_words(&i.name)).collect();
    let mut add = |id: &str, name: &str, in_plan: bool, sets: i64| {
        if named.contains(&sorted_words(name)) {
            return;
        }
        let row = found.entry(id.to_string()).or_insert_with(|| Missing {
            exercise_id: id.into(),
            name: name.into(),
            in_plan,
            logged_sets: 0,
            stand_in: closest(name).map(|e| e.guide_id),
        });
        row.in_plan |= in_plan;
        row.logged_sets += sets;
    };
    let demo = "WHERE coalesce(json_extract(definition_json, '$.source'), '') != 'demo_fixture'";
    for template in crate::db::models::WorkoutTemplate::select(db, demo, [])? {
        let list = |k: &str| template.definition_json.get(k).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for spec in list("exercises").into_iter().chain(list("alternatives")) {
            let (Some(id), name) = (spec["id"].as_str(), spec["name"].as_str().unwrap_or_default()) else { continue };
            match spec["guide_id"].as_str() {
                Some(_) => {
                    pictured.insert(id.into());
                }
                None => add(id, name, true, 0),
            }
        }
    }
    let mut stmt = db.prepare(
        "SELECT e.exercise_id, json_extract(e.config_json, '$.name'), json_extract(e.config_json, '$.guide_id'), count(s.id)
         FROM workout_exercise e JOIN set_log s ON s.workout_exercise_id = e.id GROUP BY e.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (id, name, guide, sets) = row?;
        if guide.is_none() && !pictured.contains(&id) {
            add(&id, name.as_deref().unwrap_or(&id), false, sets);
        }
    }
    let mut list: Vec<Missing> = found.into_values().collect();
    list.sort_by_key(|m| (!m.in_plan, -m.logged_sets));
    Ok(list)
}

fn sorted_words(name: &str) -> Vec<String> {
    let mut list: Vec<String> = words(name).into_iter().collect();
    list.sort();
    list
}

/// Dashboard SVG route → file.
pub fn svg_path(slug: &str) -> Option<PathBuf> {
    catalog().contains_key(slug).then(|| crate::config::assets_dir().join(slug).join("frame-1.svg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slugs(options: &[Entry]) -> Vec<&str> {
        options.iter().map(|o| o.guide_id.as_str()).collect()
    }

    fn set(slugs: &[&str]) -> HashSet<String> {
        slugs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn catalog_assets_and_stand_in_pictures() {
        assert!(catalog().keys().all(|slug| svg_path(slug).is_some_and(|p| p.is_file())));
        let pose = for_exercise("Seated Cable Row", None).unwrap();
        assert_eq!((pose.equipment.as_str(), pose.primary_muscle.as_str()), ("Cable", "Back"));
        assert!(std::path::Path::new(&pose.telegram_png).is_file());
        // No guide_id: the closest picture of the same movement, never one that only shares equipment.
        assert_eq!(for_exercise("Hip Thrust Machine", None).unwrap().guide_id, "smith-machine-hip-thrust");
        assert!(for_exercise("some pulling machine", None).is_none());
        assert!(for_exercise("anything", Some("../../private")).is_none());
        assert!(svg_path("../README").is_none());
    }

    #[test]
    fn other_apps_names_match_the_catalog() {
        // Strong/Hevy put the equipment in brackets; gym shorthand and compound words are spelled out.
        let cases = [
            ("Bench Press (Barbell)", Some("bench-press")),
            ("Squat (Barbell)", Some("squat")),
            ("Deadlift (Barbell)", Some("deadlift")),
            ("Lat Pulldown (Cable)", Some("lat-pulldown")),
            ("Pull Up", Some("pull-up")),
            ("RDL", Some("romanian-deadlift")),
            ("OHP", Some("overhead-press")),
            ("Skullcrusher (Barbell)", Some("skull-crusher")),
            ("Hip Abductor (Machine)", Some("hip-abduction-machine")),
            ("Incline Bench Press (Dumbbell)", None),
        ];
        for (name, slug) in cases {
            assert_eq!(matching(name).map(|e| e.guide_id).as_deref(), slug, "{name}");
        }
        // Not exact, but the bracketed equipment wins over a better word match on other equipment.
        assert_eq!(search("Incline Bench Press (Dumbbell)", None, None, 1)[0].guide_id, "incline-dumbbell-press");
    }

    #[test]
    fn missing_lists_plan_and_logged_exercises_without_a_picture() {
        let db = crate::db::test::db();
        let plan = serde_json::json!({"id": "legs", "name": "Legs", "exercises": [
            {"id": "squat", "name": "Squat", "role": "squat", "guide_id": "squat", "primary": true, "target_weight": 100, "warmup_weight": 50},
            {"id": "hip-thrust-machine", "name": "Hip Thrust Machine", "role": "glutes", "target_weight": 60}]});
        crate::domain::templates::import_template(&db, &serde_json::from_value(plan).unwrap()).unwrap();
        let history = |exercise: &str, config: &str| {
            db.execute_batch(&format!(
                "INSERT INTO workout_session (id, status, waited_for_equipment_count) VALUES ('w-{exercise}', 'PLAN_UPDATED', 0);
                 INSERT INTO workout_exercise (id, workout_session_id, exercise_id, position, status, planned_working_sets, rep_min, rep_max, target_weight, rest_seconds, config_json)
                 VALUES ('e-{exercise}', 'w-{exercise}', '{exercise}', 0, 'COMPLETED', 1, 8, 8, 0, 90, '{config}');
                 INSERT INTO set_log (id, workout_exercise_id, set_number, set_type, weight, reps, logged_at) VALUES ('s-{exercise}', 'e-{exercise}', 1, 'WORKING', 50, 8, '2026-11-01 18:00:00');"
            ))
            .unwrap();
        };
        // An early workout without guide_ids: its plan entry has a picture now, so it isn't missing.
        history("squat", r#"{"name": "Squat"}"#);
        history("old-press", r#"{"name": "Old Press"}"#);
        // Named exactly like a catalog picture (word order aside): the picture is found by name.
        history("seated-row", r#"{"name": "Cable Row Seated"}"#);
        let missing = missing(&db).unwrap();
        let rows: Vec<(&str, bool, i64)> =
            missing.iter().map(|m| (m.exercise_id.as_str(), m.in_plan, m.logged_sets)).collect();
        assert_eq!(rows, [("hip-thrust-machine", true, 0), ("old-press", false, 1)]);
        assert_eq!(missing[0].stand_in.as_deref(), Some("smith-machine-hip-thrust"));
    }

    #[test]
    fn fitting_swaps_keep_the_muscle_within_the_equipment() {
        let dumbbells = |i: &Item| ["Dumbbell", "Bodyweight"].contains(&needs(i));
        let options = fitting("bench-press", dumbbells, &HashSet::new(), &HashSet::new(), 3);
        assert_eq!(options[0].guide_id, "dumbbell-bench-press");
        assert!(
            options
                .iter()
                .all(|o| o.primary_muscle == "Chest" && ["Dumbbell", "Bodyweight"].contains(&o.equipment.as_str()))
        );
        // A pull-up needs a bar even though the catalog calls it bodyweight.
        let bodyweight = |i: &Item| needs(i) == "Bodyweight";
        assert!(
            !slugs(&fitting("lat-pulldown", bodyweight, &HashSet::new(), &HashSet::new(), 10)).contains(&"pull-up")
        );
    }

    #[test]
    fn swap_alternatives_differ_in_equipment() {
        // "Rear Delt Fly" and "Bent-Over Rear Delt Raise" are the same dumbbell movement; offer one of them.
        let options = similar("cable-rear-delt-fly", &HashSet::new(), &HashSet::new(), 3);
        assert_eq!(options.len(), 3);
        assert_ne!(options[0].equipment, options[1].equipment);
    }

    #[test]
    fn swap_never_offers_the_same_movement_under_another_name() {
        // Leg Curl and Lying Leg Curl share one picture: swapping one for the other isn't a swap.
        assert!(!slugs(&similar("lying-leg-curl", &HashSet::new(), &HashSet::new(), 3)).contains(&"leg-curl"));
        assert!(
            !slugs(&similar("dumbbell-romanian-deadlift", &set(&["leg-curl"]), &HashSet::new(), 3))
                .contains(&"lying-leg-curl")
        );
    }

    #[test]
    fn swap_offers_the_same_movement_on_other_equipment_first() {
        // Cable station taken: a fly elsewhere (pec deck = machine fly), not a press, even one done before.
        let options = similar("cable-fly", &HashSet::new(), &set(&["bench-press"]), 3);
        assert_eq!(slugs(&options)[..2], ["dumbbell-fly", "pec-deck"]);
    }
}
