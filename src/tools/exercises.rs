//! History import from other apps and workouts rebuilt for the equipment at hand.
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ctx, Listed, Tool, ToolDef, def};
use crate::domain::equipment::{self, Choice};
use crate::domain::import::{self, PastWorkout, Unit};
use crate::domain::profile::get_profile;
use crate::domain::templates::{get_template, import_template};
use crate::domain::workout::todays_session;
use crate::error::Result;

pub fn tools() -> Vec<ToolDef> {
    vec![def::<HistoryImport>(), def::<WorkoutRebuild>()]
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryImport {
    /// Path of a Strong/Hevy CSV the owner sent.
    pub file: Option<String>,
    /// Or workouts read from screenshots or text.
    pub workouts: Option<Vec<PastWorkout>>,
    /// Source name → plan exercise id or guide_id; null keeps the owner's name. Matching names can be left out.
    #[serde(default)]
    pub exercises: BTreeMap<String, Option<String>>,
    /// Default kg.
    pub unit: Option<Unit>,
    #[serde(default)]
    pub dry_run: bool,
}

impl Tool for HistoryImport {
    const NAME: &'static str = "history_import";
    const DESCRIPTION: &'static str = "Import past workouts from another app (CSV, screenshots, pasted logs) so weights and trends start from real numbers. \
        Dry run first; ask once about unmatched names (buttons: closest matches + \"keep my name\"). Already logged workouts are skipped.";
    const LISTED: Listed = Listed::Always;

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let zone = get_profile(cx.db)?.tz();
        let history = match (&self.file, &self.workouts) {
            (Some(file), None) => {
                // Only files the bot saved: the parser's errors quote the file.
                let dir = cx.data_dir.join("imports").canonicalize().ok();
                let path = std::path::Path::new(file).canonicalize().ok();
                let Some(path) = path.filter(|p| dir.as_ref().is_some_and(|d| p.starts_with(d))) else {
                    crate::bail!(
                        "IMPORT_FILE",
                        "Pass the path of a file the owner sent ({}/imports/…)",
                        cx.data_dir.display()
                    );
                };
                import::parse_csv(&std::fs::read_to_string(path)?, self.unit, &zone)?
            }
            (None, Some(workouts)) => import::from_chat(workouts, self.unit, &zone)?,
            _ => crate::bail!("ARGUMENT_REQUIRED", "Pass either file or workouts"),
        };
        import::import(cx.db, &history, &self.exercises, self.dry_run, cx.now, &cx.request_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkoutRebuild {
    /// Default: today's session's.
    pub template_id: Option<String>,
    /// Saved place, e.g. "home"; with equipment it is saved (empty list deletes).
    pub location: Option<String>,
    /// Catalog equipment, e.g. ["Dumbbell","Pull-up Bar"]; bodyweight is implied.
    pub equipment: Option<Vec<String>>,
    /// Saved with the location, e.g. "dumbbells up to 24 kg".
    pub notes: Option<String>,
    /// Exercise id → guide_id or "keep".
    #[serde(default)]
    pub swaps: BTreeMap<String, String>,
    /// New template id to save it as.
    pub save_as: Option<String>,
}

impl Tool for WorkoutRebuild {
    const NAME: &'static str = "workout_rebuild";
    const DESCRIPTION: &'static str = "Rebuild a workout for the equipment at hand (hotel, home, travel): exercises that don't fit swap to the closest \
        same-muscle one that does, each with its own history. Returns a proposal; adjust with swaps, save_as, then point the session at it.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let place = match (&self.location, &self.equipment) {
            (Some(name), Some(equipment)) => {
                equipment::save_location(cx.db, name, equipment, self.notes.as_deref(), cx.now)?;
                if equipment.is_empty() {
                    return Ok(json!({"deleted": name}));
                }
                equipment::location(cx.db, name)?
            }
            (Some(name), None) => equipment::location(cx.db, name)?,
            (None, Some(equipment)) => crate::db::models::GymLocation {
                name: "today".into(),
                equipment_json: crate::db::Json(equipment::equipment_names(equipment)?),
                notes: self.notes.clone(),
                updated_at: cx.now,
            },
            (None, None) => crate::bail!("ARGUMENT_REQUIRED", "Pass location or equipment"),
        };
        let template_id = match &self.template_id {
            Some(id) => Some(id.clone()),
            None => todays_session(cx.db, cx.now)?.and_then(|s| s.workout_template_id),
        }
        .ok_or_else(|| crate::error::Error::domain("ARGUMENT_REQUIRED", "No workout today; pass template_id"))?;
        let template = get_template(cx.db, &template_id)?;
        let choices: BTreeMap<String, Choice> = self
            .swaps
            .iter()
            .map(|(k, v)| (k.clone(), if v.eq_ignore_ascii_case("keep") { Choice::Keep } else { Choice::Use(v) }))
            .collect();
        let (mut rebuilt, exercises) = equipment::rebuild(cx.db, &template, &place.equipment_json, &choices)?;
        let mut result = json!({"template_id": template_id, "location": place, "exercises": exercises});
        if let Some(id) = &self.save_as {
            rebuilt.id = id.clone();
            rebuilt.name = format!("{} ({})", template.name, place.name);
            import_template(cx.db, &rebuilt)?;
            result["saved"] = json!(id);
        }
        Ok(result)
    }
}
