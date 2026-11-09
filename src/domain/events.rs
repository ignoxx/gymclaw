//! Durable agent event inbox; acknowledgement never deletes history (port of events.py). Owner: lead.
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::AgentEvent;
use crate::error::Result;
use crate::time::Ts;

/// Events that need the owner's attention. Everything else is an audit record the code already handled
/// (state changes, timers, sends, calendar writes). A single failed crowd poll is such a record: the
/// owner hears about a lasting outage from `crowd::polling_alert`.
/// A personal-calendar change is not here: it never moves a session, and a conflict it causes arrives as
/// `planning.attention`, so waking the agent for the change itself only invites an empty reply.
pub const ACTIONABLE: [&str; 8] = [
    "notifications.delivery_unknown", "calendar.write_blocked",
    "planning.attention", "planning.replan_required", "planning.no_show", "workout.missed",
    "planning.weekly_briefing", "workout.stale",
];

/// One event per distinct situation, including across restarts and acknowledgement. A quiet timer
/// never creates a new model turn for the same unchanged conflict.
pub fn request(db: &Connection, kind: &str, payload: Value, now: Ts) -> Result<()> {
    use sha2::{Digest, Sha256};
    let key = format!("agent:{kind}:{}", hex::encode(Sha256::digest(payload.to_string())));
    if AgentEvent::first(db, "WHERE correlation_id = ?1", [&key])?.is_none() {
        let mut event = crate::domain::core::emit(db, kind, now, payload)?;
        event.correlation_id = Some(key);
        event.update(db)?;
    }
    Ok(())
}

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

/// Only decisions wake the model. Delivery/permission issues stay in the inbox until explicitly
/// resolved; acknowledging a model wake must not imply the owner granted authority. A no-show is
/// sent by the outbox as the session's own message; the owner's answer reaches the agent.
pub const WAKE: [&str; 5] = [
    "planning.attention", "planning.replan_required", "workout.missed",
    "planning.weekly_briefing", "workout.stale",
];

pub fn wake_pending(db: &Connection) -> Result<Vec<Value>> {
    Ok(pending(db, false)?.into_iter().filter(|e| e["type"].as_str().is_some_and(|t| WAKE.contains(&t))).take(20).collect())
}

/// Unhandled actionable events, newest first: the "Open issue" lines of every turn's context.
pub fn open_issues(db: &Connection) -> Result<Vec<AgentEvent>> {
    let marks = ACTIONABLE.map(|t| format!("'{t}'")).join(", ");
    AgentEvent::select(db, &format!("WHERE handled_at IS NULL AND type IN ({marks}) ORDER BY created_at DESC, rowid DESC LIMIT 5"), [])
}

/// Position in the event log; pass it to `raised_since` after a tool call.
pub fn mark(db: &Connection) -> Result<i64> {
    Ok(db.query_row("SELECT coalesce(max(rowid), 0) FROM agent_event", [], |r| r.get(0))?)
}

/// Unhandled wake events written after `mark`. Inside the transaction that read `mark` these are
/// that transaction's own: no other connection can commit while it holds its read lock.
pub fn raised_since(db: &Connection, mark: i64) -> Result<Vec<AgentEvent>> {
    let types = WAKE.map(|t| format!("'{t}'")).join(", ");
    AgentEvent::select(db, &format!("WHERE rowid > ?1 AND handled_at IS NULL AND type IN ({types}) ORDER BY rowid"), [mark])
}

/// Freeze a decision batch before calling the model. A retry reuses this batch and its tool request
/// IDs even when new events arrive meanwhile, so a partial failed turn cannot duplicate mutations.
pub fn wake_batch(db: &Connection, now: Ts) -> Result<Option<AgentEvent>> {
    if let Some(batch) = AgentEvent::first(db, "WHERE type = 'runtime.agent_wake' AND handled_at IS NULL ORDER BY created_at, id", [])? {
        return Ok(Some(batch));
    }
    let events = wake_pending(db)?;
    if events.is_empty() { return Ok(None); }
    Ok(Some(crate::domain::core::emit(db, "runtime.agent_wake", now, json!({"events": events}))?))
}
