//! Events and idempotent mutations, shared by every domain service.
use serde_json::{Value, json};

use crate::db::models::AgentEvent;
use crate::db::{Connection, Json, new_id};
use crate::error::Result;
use crate::time::Ts;

/// Append an audit/agent event.
pub fn emit(db: &Connection, kind: &str, now: Ts, payload: Value) -> Result<AgentEvent> {
    let event = AgentEvent { id: new_id(), r#type: kind.into(), created_at: now, payload_json: Json(payload), handled_at: None, correlation_id: None };
    event.insert(db)?;
    Ok(event)
}

/// Run `action` once per `request_id`. A retry with the same intent returns the saved result; a
/// different intent under the same ID fails. Result: `{data, events, user_message_hint}`, where the
/// hint is `data.instruction` if present.
pub fn mutate(db: &Connection, operation: &str, request_id: &str, intent: Value, now: Ts, action: impl FnOnce() -> Result<Value>) -> Result<Value> {
    if request_id.is_empty() || request_id.len() > 200 {
        crate::bail!("REQUEST_ID_REQUIRED", "Mutation requires request ID (1–200 characters)");
    }
    let request = json!({"operation": operation, "intent": intent});
    if let Some(previous) = AgentEvent::first(db, "WHERE correlation_id = ?1", [request_id])? {
        if previous.payload_json.get("request") != Some(&request) {
            crate::bail!("IDEMPOTENCY_CONFLICT", "Request ID already used with different intent");
        }
        return Ok(previous.payload_json["result"].clone());
    }
    let data = action()?;
    let mut event = emit(db, operation, now, json!({"request": request}))?;
    let hint = data.get("instruction").cloned().unwrap_or(Value::Null);
    let result = json!({"data": data, "events": [{"id": event.id, "type": operation}], "user_message_hint": hint});
    event.correlation_id = Some(request_id.into());
    event.payload_json = Json(json!({"request": request, "result": result}));
    event.update(db)?;
    Ok(result)
}

/// Wrap a read or non-idempotent result in the CLI/tool envelope.
pub fn outcome(data: Value) -> Value {
    let hint = data.get("instruction").cloned().unwrap_or(Value::Null);
    json!({"data": data, "events": [], "user_message_hint": hint})
}
