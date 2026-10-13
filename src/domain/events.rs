//! Durable agent event inbox; acknowledgement never deletes history (port of events.py). Owner: lead.
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::AgentEvent;
use crate::error::Result;
use crate::time::Ts;

/// Events that need the owner's attention. Everything else is an audit record the code already handled
/// (state changes, timers, sends, calendar writes).
pub const ACTIONABLE: [&str; 3] = ["notifications.delivery_unknown", "calendar.write_blocked", "crowd.source_unavailable"];

pub fn pending(db: &Connection, include_all: bool) -> Result<Vec<Value>> {
    let filter = if include_all { String::new() } else { format!("AND type IN ({})", ACTIONABLE.map(|t| format!("'{t}'")).join(", ")) };
    Ok(AgentEvent::select(db, &format!("WHERE handled_at IS NULL {filter} ORDER BY created_at, id"), [])?
        .into_iter()
        .map(|e| json!({"id": e.id, "type": e.r#type, "created_at": e.created_at, "payload": e.payload_json, "correlation_id": e.correlation_id}))
        .collect())
}

pub fn ack(db: &Connection, event_id: &str, now: Ts) -> Result<Value> {
    let Some(mut event) = AgentEvent::get(db, event_id)? else {
        crate::bail!("EVENT_NOT_FOUND", "Unknown event");
    };
    if now < event.created_at {
        crate::bail!("TIME_REVERSED", "Acknowledgement precedes event");
    }
    if event.handled_at.is_none() {
        event.handled_at = Some(now);
        event.update(db)?;
    }
    Ok(json!({"id": event.id, "handled_at": event.handled_at}))
}

/// Whether the latest of a lost/restored event pair is `lost`. Owner alerts dedupe on this, not on
/// `handled_at`: delivering the alert marks its event handled while the outage is still on.
pub fn outage_open(db: &Connection, lost: &str, restored: &str) -> Result<bool> {
    let latest = AgentEvent::first(db, "WHERE type IN (?1, ?2) ORDER BY created_at DESC, id DESC", [lost, restored])?;
    Ok(latest.is_some_and(|e| e.r#type == lost))
}
