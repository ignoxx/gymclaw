//! Durable agent event inbox; acknowledgement never deletes history.
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::AgentEvent;
use crate::error::Result;
use crate::time::Ts;

/// Event types the agent deals with. Everything else is an audit record the code already handled
/// (state changes, timers, sends). A single failed crowd poll is such a record: the
/// owner hears about a lasting outage from `crowd::polling_alert`. A personal-calendar change is not
/// here: it never moves a session, and a conflict it causes arrives as `planning.attention`.
///
/// Only decisions wake the model. Delivery/permission issues stay open until explicitly resolved;
/// acknowledging a model wake must not imply the owner granted authority. A no-show is sent by the
/// outbox as the session's own message; the owner's answer reaches the agent. `extension.trigger` is a
/// trigger the agent set itself; `agent.reflect` (`extensions::reflect`) asks it to learn from a hard turn,
/// and is never open, so it runs only once the chat is quiet.
const KINDS: [Kind; 10] = [
    Kind { name: "notifications.delivery_unknown", open: true, wake: false, mid_workout: false },
    Kind { name: "notifications.delivery_failed", open: true, wake: false, mid_workout: false },
    Kind { name: "planning.attention", open: true, wake: true, mid_workout: false },
    Kind { name: "planning.replan_required", open: true, wake: true, mid_workout: false },
    Kind { name: "planning.no_show", open: true, wake: false, mid_workout: false },
    Kind { name: "workout.missed", open: true, wake: true, mid_workout: false },
    Kind { name: "planning.weekly_briefing", open: true, wake: true, mid_workout: false },
    Kind { name: "workout.stale", open: true, wake: true, mid_workout: true },
    Kind { name: "extension.trigger", open: true, wake: true, mid_workout: true },
    Kind { name: "agent.reflect", open: false, wake: true, mid_workout: false },
];

struct Kind {
    name: &'static str,
    /// An open issue in every turn's context until handled.
    open: bool,
    /// Wakes the agent for a background turn.
    wake: bool,
    /// Also wakes it while a workout runs. The rest wait for the workout to end: mid-set the owner
    /// hears only about the workout (2026-11-25: a planning warning woke the agent during the
    /// warm-up and it asked about next week's legs).
    mid_workout: bool,
}

/// The types `which` selects, as a SQL `IN (…)` list.
fn types(which: fn(&Kind) -> bool) -> String {
    let names: Vec<String> = KINDS.iter().filter(|k| which(k)).map(|k| format!("'{}'", k.name)).collect();
    format!("({})", names.join(", "))
}

/// Whether events of this type wake the agent.
pub fn wakes(kind: &str) -> bool {
    KINDS.iter().any(|k| k.wake && k.name == kind)
}

/// Held while the owner is mid-workout (`workout::live`): a wake that isn't `mid_workout`.
fn held(k: &Kind) -> bool {
    k.wake && !k.mid_workout
}

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
    let filter = if include_all { String::new() } else { format!("AND type IN {}", types(|k| k.open)) };
    Ok(AgentEvent::select(db, &format!("WHERE handled_at IS NULL {filter} ORDER BY created_at, id"), [])?
        .into_iter()
        .map(listed)
        .collect())
}

/// An event as `pending` and the wake batch show it.
fn listed(e: AgentEvent) -> Value {
    json!({"id": e.id, "type": e.r#type, "created_at": e.created_at, "payload": e.payload_json})
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

/// Unhandled wake events, oldest first. The weekly review wakes at once; the Sunday recap waits for it
/// (`weekly::release_recap`).
pub fn wake_pending(db: &Connection) -> Result<Vec<Value>> {
    Ok(AgentEvent::select(
        db,
        &format!("WHERE handled_at IS NULL AND type IN {} ORDER BY created_at, id LIMIT 20", types(|k| k.wake)),
        [],
    )?
    .into_iter()
    .map(listed)
    .collect())
}

/// Unhandled actionable events, newest first: the "Open issue" lines of every turn's context, which
/// that turn settles. Mid-workout the held kinds stay out, so a turn then neither raises nor settles them.
pub fn open_issues(db: &Connection, now: Ts) -> Result<Vec<AgentEvent>> {
    let shown = if crate::domain::workout::live(db, now)? { types(|k| k.open && !held(k)) } else { types(|k| k.open) };
    AgentEvent::select(
        db,
        &format!("WHERE handled_at IS NULL AND type IN {shown} ORDER BY created_at DESC, rowid DESC LIMIT 5"),
        [],
    )
}

/// Position in the event log; pass it to `raised_since` after a tool call.
pub fn mark(db: &Connection) -> Result<i64> {
    Ok(db.query_row("SELECT coalesce(max(rowid), 0) FROM agent_event", [], |r| r.get(0))?)
}

/// Unhandled wake events written after `mark`. Inside the transaction that read `mark` these are
/// that transaction's own: no other connection can commit while it holds its read lock.
pub fn raised_since(db: &Connection, mark: i64) -> Result<Vec<AgentEvent>> {
    AgentEvent::select(
        db,
        &format!("WHERE rowid > ?1 AND handled_at IS NULL AND type IN {} ORDER BY rowid", types(|k| k.wake)),
        [mark],
    )
}

/// Freeze a decision batch before calling the model. A retry reuses this batch and its tool request
/// IDs even when new events arrive meanwhile, so a partial failed turn cannot duplicate mutations.
/// Mid-workout (`workout::live`) only `mid_workout` kinds go out: a frozen batch holding any other is
/// retired unsent (its events stay pending, like a batch an owner turn settled) and a filtered one follows.
pub fn wake_batch(db: &Connection, now: Ts) -> Result<Option<AgentEvent>> {
    let live = crate::domain::workout::live(db, now)?;
    let is_held = |e: &Value| KINDS.iter().any(|k| held(k) && e["type"] == k.name);
    if let Some(batch) =
        AgentEvent::first(db, "WHERE type = 'runtime.agent_wake' AND handled_at IS NULL ORDER BY created_at, id", [])?
    {
        if !(live && batch.payload_json["events"].as_array().is_some_and(|es| es.iter().any(is_held))) {
            return Ok(Some(batch));
        }
        ack(db, &batch.id, now)?;
    }
    let mut events = wake_pending(db)?;
    if live {
        events.retain(|e| !is_held(e));
    }
    if events.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::domain::core::emit(db, "runtime.agent_wake", now, json!({"events": events}))?))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    /// 2026-11-25: a planning warning woke the agent a minute into the warm-up and it asked about next
    /// week. Mid-workout only workout-time kinds wake it or show as open issues (so no turn settles
    /// the rest); a batch frozen before the workout is retired unsent; the rest go out once it ends.
    #[test]
    fn a_live_workout_holds_planning_wakes() {
        use serde_json::json;
        let db = crate::db::test::with_short_template();
        let now = crate::time::Ts::now();
        super::request(&db, "planning.attention", json!({"warnings": ["legs not booked"]}), now).unwrap();
        let frozen = super::wake_batch(&db, now).unwrap().unwrap();
        crate::domain::workout::start(&db, Some("short"), None, false, now, "start").unwrap();
        let types = |db: &crate::db::Connection| -> Vec<String> {
            super::open_issues(db, now).unwrap().into_iter().map(|e| e.r#type).collect()
        };
        assert!(types(&db).is_empty(), "an owner turn mid-workout doesn't see it");
        super::request(&db, "extension.trigger", json!({"trigger": "water"}), now).unwrap();
        let batch = super::wake_batch(&db, now).unwrap().unwrap();
        assert_ne!(batch.id, frozen.id, "the frozen planning batch is retired");
        assert_eq!(batch.payload_json["events"].as_array().unwrap().len(), 1);
        assert_eq!(batch.payload_json["events"][0]["type"], "extension.trigger");
        super::ack(&db, &batch.id, now).unwrap();
        super::ack(&db, batch.payload_json["events"][0]["id"].as_str().unwrap(), now).unwrap();
        // Abandoned past the stale mark, the workout no longer holds anything.
        let later = now + crate::domain::weekly::STALE_WORKOUT;
        assert_eq!(super::open_issues(&db, later).unwrap()[0].r#type, "planning.attention");
        crate::domain::coach::discard_workout(&db, None, now, "discard").unwrap();
        assert_eq!(types(&db), ["planning.attention"]);
        let after = super::wake_batch(&db, now).unwrap().unwrap();
        assert_eq!(after.payload_json["events"][0]["type"], "planning.attention");
    }

    /// Every event type a query reads is written somewhere. Habit learning once read
    /// `planning.session_moved` for weeks after its writer was renamed to `planning.session_edited`.
    #[test]
    fn every_event_type_read_is_written() {
        fn sources(dir: &Path, out: &mut String) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    out.push_str(text.split("#[cfg(test)]").next().unwrap());
                }
            }
        }
        let mut code = String::new();
        sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut code);
        let set = |pattern: &str| -> BTreeSet<String> {
            regex::Regex::new(pattern).unwrap().captures_iter(&code).map(|c| c[1].to_string()).collect()
        };
        // Reads are SQL literals ('…'); writers name their type as a Rust string ("…").
        let unwritten: Vec<_> =
            set(r"type = '([a-z_]+\.[a-z_]+)'").difference(&set(r#""([a-z_]+\.[a-z_]+)""#)).cloned().collect();
        assert!(unwritten.is_empty(), "read but never written: {unwritten:?}");
    }
}
