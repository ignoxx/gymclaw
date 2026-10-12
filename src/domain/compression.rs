//! Fit a template into a shorter slot by dropping low-priority sets (port of compression.py). Owner: lead.
use serde_json::Value;

use crate::domain::profile::Profile;
use crate::domain::templates::Template;
use crate::error::Result;

/// `{template, sets, budget_seconds, estimated_duration_seconds, reduced}`; WORKOUT_SLOT_TOO_SHORT if nothing fits.
pub fn compress_template(template: &Template, profile: &Profile, budget_seconds: i64) -> Result<Value> {
    let _ = (template, profile, budget_seconds);
    todo!("lead")
}
