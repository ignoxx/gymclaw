//! Fit a template into a shorter slot (port of compression.py): drop lowest-priority volume first and
//! keep the first primary exercise plus its prerequisites. Owner: lead.
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::domain::profile::Profile;
use crate::domain::templates::Template;
use crate::error::Result;

/// `{template, sets, budget_seconds, estimated_duration_seconds, reduced}`; WORKOUT_SLOT_TOO_SHORT if nothing fits.
pub fn compress_template(template: &Template, profile: &Profile, budget_seconds: i64) -> Result<Value> {
    let exercises = &template.exercises;
    let mut counts: HashMap<&str, i64> = exercises.iter().map(|e| (e.id.as_str(), e.working_sets)).collect();
    let first_primary = exercises.iter().find(|e| e.primary).map(|e| e.id.as_str());
    let mut protected: HashSet<&str> = first_primary.into_iter().collect();
    for spec in exercises.iter().rev() {
        if protected.contains(spec.id.as_str()) {
            protected.extend(spec.requires_completed.iter().map(String::as_str));
        }
    }

    let duration = |counts: &HashMap<&str, i64>| {
        let kept: Vec<_> = exercises.iter().filter(|e| counts[e.id.as_str()] > 0).collect();
        let mut seconds = match first_primary {
            Some(id) if counts[id] > 0 => template.set_duration_seconds,
            _ => 0,
        };
        for (index, spec) in kept.iter().enumerate() {
            let rest = spec.rest_seconds.unwrap_or(if spec.primary { profile.default_compound_rest_seconds } else { profile.default_accessory_rest_seconds });
            let sets = counts[spec.id.as_str()];
            seconds += sets * template.set_duration_seconds + (sets - 1).max(0) * rest;
            if index < kept.len() - 1 {
                seconds += rest + template.transition_seconds;
            }
        }
        seconds
    };

    while duration(&counts) > budget_seconds {
        let kept: Vec<(usize, _)> = exercises.iter().enumerate().filter(|(_, e)| counts[e.id.as_str()] > 0).collect();
        let lowest = kept
            .iter()
            .filter(|(_, e)| {
                let count = counts[e.id.as_str()];
                !(protected.contains(e.id.as_str()) && count == 1) && !(kept.len() == 1 && count == 1)
            })
            .min_by_key(|(index, e)| (e.priority, std::cmp::Reverse(*index)));
        let Some((_, lowest)) = lowest else {
            crate::bail!("WORKOUT_SLOT_TOO_SHORT", "Edited slot cannot fit required warm-up and primary movement; event kept, explicit adjustment needed");
        };
        *counts.get_mut(lowest.id.as_str()).expect("known") -= 1;
        // Removing a prerequisite cannot silently create an invalid execution order.
        for spec in exercises {
            if spec.requires_completed.iter().any(|r| counts.get(r.as_str()) == Some(&0)) {
                counts.insert(&spec.id, 0);
            }
        }
    }
    let sets: Map<String, Value> = exercises.iter().map(|e| (e.id.clone(), json!(counts[e.id.as_str()]))).collect();
    let reduced: Vec<Value> = exercises
        .iter()
        .filter(|e| counts[e.id.as_str()] != e.working_sets)
        .map(|e| json!({"exercise_id": e.id, "from_sets": e.working_sets, "to_sets": counts[e.id.as_str()]}))
        .collect();
    Ok(json!({"template": template, "sets": sets, "budget_seconds": budget_seconds, "estimated_duration_seconds": duration(&counts), "reduced": reduced}))
}
