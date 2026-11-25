//! Periodic domain work the scheduler runs.
//! `watch`, `weekly` and `poll_crowd` run in one transaction and queue owner messages as PENDING
//! `notification_delivery` rows; the runtime's outbox sends them afterwards.
use serde_json::{Value, json};

use crate::db::Connection;
use crate::domain::{crowd, personal_calendar, weekly};
use crate::error::Result;
use std::path::Path;

use crate::providers::crowd::{from_path, run_command};
use crate::providers::http::Net;
use crate::runtime::outbox::{prepare_event_message, runtime_authority};
use crate::time::Ts;

/// Every 60 s: personal feed refresh, deterministic upkeep, decision events and crowd health alerts.
pub fn watch(db: &Connection, recipient: &str, now: Ts) -> Result<Value> {
    let authority = runtime_authority(db)?;
    personal_calendar::refresh(db, now, false, &Net::new()?, false)?;
    if let Some(template) = &authority.template_id {
        weekly::rolling_plan(db, template, now)?;
    }
    for (event_id, _, message) in weekly::no_show_nudges(db, now)? {
        prepare_event_message(db, &event_id, &message, now, recipient)?;
    }
    if authority.crowd_polling_enabled
        && let Some((event_id, message)) = crowd::polling_alert(db, now)?
    {
        prepare_event_message(db, &event_id, &message, now, recipient)?;
    }
    Ok(json!({}))
}

/// Sunday 23:00 local: refresh the personal feed and draft next week if it is empty. The scheduler
/// then wakes the agent for its review (`weekly::announce`).
pub fn weekly(db: &Connection, now: Ts) -> Result<Value> {
    let authority = runtime_authority(db)?;
    let Some(template) = authority.template_id.clone() else {
        crate::bail!("TEMPLATE_REQUIRED", "No activated template or confirmed split to plan with")
    };
    personal_calendar::refresh(db, now, true, &Net::new()?, false)?;
    weekly::weekly_plan(db, &template, now)
}

/// Every 15 min when crowd polling is enabled: read the gym's live count from `config_path`.
/// A failed read is returned, not raised, so its failure health commits (the stale alert in `watch` tells the owner).
pub fn poll_crowd(db: &Connection, config_path: &Path, _now: Ts) -> Result<Value> {
    let net = Net::new()?;
    let provider = from_path(config_path, &net, &run_command)?;
    // Retrieval time is taken after the response.
    crowd::poll(db, provider.as_ref(), None)
}
