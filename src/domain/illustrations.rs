//! Workout Guide first-pose assets and catalog search. Every planned exercise carries a `guide_id`,
//! so every exercise has an image.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// `GYMCLAW_ASSETS_DIR`, else `assets/workout-guide` (or the old Python location) under the working dir.
pub fn asset_root() -> PathBuf {
    if let Ok(dir) = std::env::var("GYMCLAW_ASSETS_DIR") {
        return dir.into();
    }
    ["assets/workout-guide", "gymclaw/assets/workout-guide"].iter().map(PathBuf::from).find(|p| p.join("manifest.json").exists()).unwrap_or_else(|| "assets/workout-guide".into())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub slug: String,
    pub name: String,
    pub exercise_type: String,
    pub equipment: String,
    pub primary_muscle: String,
    #[serde(default)]
    pub secondary_muscles: Vec<String>,
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

/// Slug → item, sorted like the Python dict (manifest order is not needed for any result).
pub static CATALOG: LazyLock<BTreeMap<String, Item>> = LazyLock::new(|| {
    let path = asset_root().join("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("workout guide manifest {}: {e}", path.display()));
    let items: Vec<Item> = serde_json::from_str(&text).expect("valid workout guide manifest");
    items.into_iter().map(|i| (i.slug.clone(), i)).collect()
});

pub fn catalog() -> &'static BTreeMap<String, Item> {
    &CATALOG
}

pub fn entry(item: &Item) -> Entry {
    Entry {
        guide_id: item.slug.clone(),
        name: item.name.clone(),
        equipment: item.equipment.clone(),
        primary_muscle: item.primary_muscle.clone(),
        svg_url: format!("/illustrations/{}.svg", item.slug),
        telegram_png: asset_root().join(&item.slug).join("frame-1.png").to_string_lossy().into(),
    }
}

/// Explicit guide_id, else a unique exact name match. No fuzzy guessing at runtime.
pub fn for_exercise(name: &str, guide_id: Option<&str>) -> Option<Entry> {
    let item = match guide_id {
        Some(id) => catalog().get(id),
        None => {
            let wanted = name.trim().to_lowercase();
            let mut matches = catalog().values().filter(|i| i.name.to_lowercase() == wanted);
            match (matches.next(), matches.next()) {
                (Some(one), None) => Some(one),
                _ => None,
            }
        }
    };
    item.map(entry)
}

pub fn primary_muscle(guide_id: Option<&str>) -> Option<String> {
    guide_id.and_then(|g| catalog().get(g)).map(|i| i.primary_muscle.clone())
}

/// Reps are the whole set (dips, push-ups): no weight to log or ask for.
pub fn bodyweight(guide_id: Option<&str>) -> bool {
    guide_id.and_then(|g| catalog().get(g)).is_some_and(|i| i.exercise_type.starts_with("bodyweight"))
}

/// Gym shorthand the owner types ("DB RDL"), spelled out the way catalog names are.
const ALIASES: &[(&str, &str)] = &[
    ("db", "dumbbell"), ("bb", "barbell"), ("kb", "kettlebell"), ("rdl", "romanian deadlift"), ("sldl", "stiff leg deadlift"),
    ("ohp", "overhead press"), ("ext", "extension"), ("tri", "tricep"), ("bi", "bicep"),
    // A pec deck ("butterfly") is a machine fly.
    ("butterfly", "fly"), ("deck", "fly"),
];
/// Name words that say where an exercise is done, not what the movement is.
const EQUIPMENT_WORDS: &[&str] = &["cable", "dumbbell", "barbell", "machine", "kettlebell", "smith", "band", "plate"];

pub fn words(text: &str) -> HashSet<String> {
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

/// Rank catalog entries by rarity-weighted words shared with the query; filters are case-insensitive exact matches.
pub fn search(query: &str, muscle: Option<&str>, equipment: Option<&str>, limit: usize) -> Vec<Entry> {
    let wanted = words(query);
    let mut results: Vec<(f64, &Item)> = vec![];
    for item in catalog().values() {
        if muscle.is_some_and(|m| item.primary_muscle.to_lowercase() != m.to_lowercase()) || equipment.is_some_and(|e| item.equipment.to_lowercase() != e.to_lowercase()) {
            continue;
        }
        let name = words(&item.name);
        let shared: f64 = wanted.intersection(&name).map(|w| RARITY.get(w).copied().unwrap_or(1.0)).sum();
        let context = words(&format!("{} {}", item.equipment, item.primary_muscle));
        let score = shared + 0.5 * wanted.intersection(&context).count() as f64 - 0.1 * name.difference(&wanted).count() as f64;
        if !wanted.is_empty() && score <= 0.0 {
            continue;
        }
        results.push((score, item));
    }
    results.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    results.into_iter().take(limit).map(|(_, i)| entry(i)).collect()
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
    let base: HashSet<String> = words(&original.name).into_iter().filter(|w| !EQUIPMENT_WORDS.contains(&w.as_str())).collect();
    let taken: HashSet<String> = exclude.iter().chain(std::iter::once(&guide_id.to_string())).filter(|s| catalog().contains_key(*s)).map(|s| artwork(s)).collect();
    let mut options: Vec<((isize, bool, bool, String), Entry)> = vec![];
    for item in catalog().values() {
        if exclude.contains(&item.slug) || taken.contains(&artwork(&item.slug)) || item.is_stretch {
            continue;
        }
        if item.primary_muscle != original.primary_muscle || item.exercise_type != original.exercise_type {
            continue;
        }
        let shared = base.intersection(&words(&item.name)).count() as isize;
        options.push(((-shared, !prefer.contains(&item.slug), item.equipment == original.equipment, item.name.clone()), entry(item)));
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
    let rest: Vec<Entry> = ranked.into_iter().filter(|o| !picked.contains(o)).take(limit.saturating_sub(picked.len())).collect();
    picked.extend(rest);
    picked
}

/// Dashboard SVG route → file.
pub fn svg_path(slug: &str) -> Option<PathBuf> {
    catalog().contains_key(slug).then(|| asset_root().join(slug).join("frame-1.svg"))
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
    fn catalog_assets_and_no_fuzzy_matching() {
        assert!(catalog().keys().all(|slug| svg_path(slug).is_some_and(|p| p.is_file())));
        let pose = for_exercise("Seated Cable Row", None).unwrap();
        assert_eq!((pose.equipment.as_str(), pose.primary_muscle.as_str()), ("Cable", "Back"));
        assert!(std::path::Path::new(&pose.telegram_png).is_file());
        assert!(for_exercise("some pulling machine", None).is_none());
        assert!(for_exercise("anything", Some("../../private")).is_none());
        assert!(svg_path("../README").is_none());
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
        assert!(!slugs(&similar("dumbbell-romanian-deadlift", &set(&["leg-curl"]), &HashSet::new(), 3)).contains(&"lying-leg-curl"));
    }

    #[test]
    fn swap_offers_the_same_movement_on_other_equipment_first() {
        // Cable station taken: a fly elsewhere (pec deck = machine fly), not a press, even one done before.
        let options = similar("cable-fly", &HashSet::new(), &set(&["bench-press"]), 3);
        assert_eq!(slugs(&options)[..2], ["dumbbell-fly", "pec-deck"]);
    }
}
