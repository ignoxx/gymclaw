//! Outbox: every proactive owner message is a `notification_delivery` row committed before the send
//! (port of gymclaw/services/runtime.py minus the OpenClaw automation sync).
//!
//! PENDING → claim SENDING (committed, no network under the write lock) → send → SENT with the
//! Telegram message id. Any send error → UNKNOWN plus a `notifications.delivery_unknown` event,
//! never retried automatically: Telegram has no idempotent send, so the owner checks and resolves.
use std::path::Path;

use jiff::SignedDuration;
use rusqlite::TransactionBehavior;
use serde::Serialize;
use serde_json::{Value, json};

use super::blocking;
use crate::db::models::{
    AgentEvent, ChatMessage, ChatRole, DeliveryStatus, JobKind, JobStatus, NotificationDelivery, NotificationJob, OnboardingState, PlannedSession,
    RuntimeSettings, SessionStatus, WorkoutSession, WorkoutStatus,
};
use crate::db::{Connection, Json, new_id};
use crate::domain::view::Button;
use crate::domain::{availability, core::emit, workout};
use crate::error::{Error, Result};
use crate::telegram::Telegram;
use crate::time::Ts;

/// `runtime_profile` of every delivery (was the OpenClaw profile name).
pub const PROFILE: &str = "gymclaw";

#[derive(Clone, Debug, Serialize)]
pub struct Authority {
    pub configured: bool,
    pub enabled: bool,
    pub template_id: Option<String>,
    pub calendar_writes_enabled: bool,
    pub crowd_polling_enabled: bool,
}

/// Explicitly activated template, else the first template of the owner's confirmed split.
fn planning_template(db: &Connection, settings: &RuntimeSettings) -> Result<Option<String>> {
    if settings.template_id.is_some() {
        return Ok(settings.template_id.clone());
    }
    Ok(OnboardingState::get(db, 1)?.and_then(|row| row.split_json.0.first().cloned()))
}

/// What the runtime may do: send (enabled), plan with which template, write the calendar, poll the gym.
pub fn runtime_authority(db: &Connection) -> Result<Authority> {
    let Some(settings) = RuntimeSettings::get(db, 1)? else {
        return Ok(Authority { configured: false, enabled: false, template_id: None, calendar_writes_enabled: false, crowd_polling_enabled: false });
    };
    Ok(Authority {
        configured: true,
        enabled: settings.enabled,
        template_id: planning_template(db, &settings)?,
        calendar_writes_enabled: settings.calendar_writes_enabled,
        crowd_polling_enabled: settings.crowd_polling_enabled,
    })
}

/// Why a reminder/rest job (or its queued delivery) should no longer reach the owner.
pub fn stale_reason(db: &Connection, job: &NotificationJob, now: Ts, delivery: Option<&NotificationDelivery>) -> Result<Option<&'static str>> {
    if job.status == JobStatus::Cancelled {
        return Ok(Some("cancelled"));
    }
    if job.kind == JobKind::Rest {
        if !availability::blocks(db, now, now + SignedDuration::from_micros(1))?.is_empty() {
            return Ok(Some("availability_override"));
        }
        let workout = match &job.workout_session_id {
            Some(id) => WorkoutSession::get(db, id)?,
            None => None,
        };
        let Some(workout) = workout.filter(|w| !matches!(w.status, WorkoutStatus::WorkoutComplete | WorkoutStatus::PostAnalysis | WorkoutStatus::PlanUpdated)) else {
            return Ok(Some("workout_finished"));
        };
        if let Some(delivery) = delivery {
            let actual = workout::current(db, &workout.id, now)?;
            let expected = &delivery.result_json["data"];
            let moved = ["status", "active_exercise", "queue", "rest_job"].iter().any(|k| actual.get(*k) != expected.get(*k));
            if workout.last_action_at != job.handled_at || moved {
                return Ok(Some("workout_advanced"));
            }
        }
    } else {
        let planned = match &job.planned_session_id {
            Some(id) => PlannedSession::get(db, id)?,
            None => None,
        };
        let Some(planned) = planned.filter(|p| p.status == SessionStatus::Committed && Some(p.source_revision) == job.payload_json.get("revision").and_then(Value::as_i64)) else {
            return Ok(Some("plan_changed"));
        };
        if availability::session_blocked(db, &planned)? {
            return Ok(Some("availability_override"));
        }
        if now > job.due_at.mins(15) {
            return Ok(Some("reminder_expired"));
        }
    }
    Ok(None)
}

/// Why a queued proactive message (briefing, nudge, alert) no longer applies.
pub fn event_stale_reason(db: &Connection, row: &NotificationDelivery, now: Ts) -> Result<Option<&'static str>> {
    if now > row.created_at + SignedDuration::from_hours(24) {
        return Ok(Some("event_message_expired"));
    }
    let event = match &row.agent_event_id {
        Some(id) => AgentEvent::get(db, id)?,
        None => None,
    };
    if let Some(event) = event
        && event.r#type == "planning.no_show"
    {
        // Started, skipped, moved or already over: the question no longer applies.
        let planned = match event.payload_json["session_id"].as_str() {
            Some(id) => PlannedSession::get(db, id)?,
            None => None,
        };
        let open = planned.is_some_and(|p| p.status == SessionStatus::Committed && p.planned_end_at > now && Some(p.planned_start_at.iso().as_str()) == event.payload_json["start"].as_str());
        if !open {
            return Ok(Some("session_resolved"));
        }
    }
    Ok(None)
}

/// The delivery as tools and the CLI show it.
pub fn delivery_data(row: &NotificationDelivery) -> Value {
    json!({"id": row.id, "job_id": row.notification_job_id, "status": row.status, "created_at": row.created_at,
        "handled_at": row.handled_at, "external_message_id": row.external_message_id, "event_id": row.agent_event_id})
}

fn acknowledge_delivery_events(db: &Connection, row: &NotificationDelivery, now: Ts) -> Result<()> {
    let Some(event_id) = &row.agent_event_id else { return Ok(()) };
    let related = row.result_json.get("related_events").and_then(Value::as_array).cloned().unwrap_or_default();
    let ids = std::iter::once(event_id.clone()).chain(related.iter().filter_map(|v| v.as_str().map(String::from)));
    for id in ids {
        if let Some(mut event) = AgentEvent::get(db, &id)? {
            event.handled_at = Some(now);
            event.update(db)?;
        }
    }
    Ok(())
}

fn new_delivery(job_id: Option<&str>, event_id: Option<&str>, recipient: &str, message: &str, result: Value, now: Ts) -> NotificationDelivery {
    NotificationDelivery {
        id: new_id(),
        notification_job_id: job_id.map(String::from),
        agent_event_id: event_id.map(String::from),
        recipient: recipient.into(),
        runtime_profile: PROFILE.into(),
        message: message.into(),
        result_json: Json(result),
        status: DeliveryStatus::Pending,
        created_at: now,
        handled_at: None,
        external_message_id: None,
    }
}

/// Advance a due job and queue its exact message in the same transaction (was `runtime fire`).
pub fn prepare_notification(db: &Connection, job_id: &str, now: Ts, recipient: &str) -> Result<Value> {
    let Some(mut job) = NotificationJob::get(db, job_id)? else { crate::bail!("JOB_NOT_FOUND", "Unknown notification job") };
    if let Some(existing) = NotificationDelivery::first(db, "WHERE notification_job_id = ?1", [job_id])? {
        if existing.recipient != recipient || existing.runtime_profile != PROFILE {
            crate::bail!("DELIVERY_ROUTE_MISMATCH", "Stored delivery belongs to a different owner/runtime");
        }
        return Ok(json!({"delivery_id": existing.id, "status": existing.status, "result": existing.result_json}));
    }
    if job.status != JobStatus::Pending {
        return Ok(json!({"stale": true, "status": job.status}));
    }
    if now < job.due_at {
        crate::bail!("NOTIFICATION_NOT_DUE", "Notification has not reached due_at");
    }
    if let Some(reason) = stale_reason(db, &job, now, None)? {
        job.status = JobStatus::Cancelled;
        job.handled_at = Some(now);
        job.update(db)?;
        emit(db, "notifications.suppressed", now, json!({"job_id": job.id, "reason": reason}))?;
        return Ok(json!({"stale": true, "reason": reason}));
    }
    let result = if job.kind == JobKind::Rest {
        workout::rest_complete(db, job_id, now, &format!("rest-due:{job_id}"))?
    } else {
        job.status = JobStatus::Fired;
        job.handled_at = Some(now);
        job.update(db)?;
        let event = emit(db, "notifications.due", now, json!({"job_id": job.id, "session_id": job.planned_session_id}))?;
        json!({"data": {"job_id": job.id}, "events": [{"id": event.id, "type": event.r#type}], "user_message_hint": job.payload_json["instruction"]})
    };
    let Some(message) = result["user_message_hint"].as_str().filter(|m| !m.is_empty()).map(String::from) else {
        return Ok(json!({"stale": true, "result": result}));
    };
    let delivery = new_delivery(Some(job_id), None, recipient, &message, result.clone(), now);
    delivery.insert(db)?;
    Ok(json!({"delivery_id": delivery.id, "status": delivery.status, "result": result}))
}

/// Freeze a proactive message (briefing, nudge, alert): one delivery per durable event. A newer
/// calendar briefing supersedes any still-pending one.
pub fn prepare_event_message(db: &Connection, event_id: &str, message: &str, now: Ts, recipient: &str, related_events: &[String]) -> Result<Value> {
    let Some(source) = AgentEvent::get(db, event_id)? else { crate::bail!("EVENT_NOT_FOUND", "Unknown proactive message event") };
    if let Some(existing) = NotificationDelivery::first(db, "WHERE agent_event_id = ?1", [event_id])? {
        if existing.recipient != recipient || existing.runtime_profile != PROFILE {
            crate::bail!("DELIVERY_ROUTE_MISMATCH", "Event delivery belongs to another owner/runtime");
        }
        return Ok(delivery_data(&existing));
    }
    if source.r#type == "calendar.update_briefing" {
        let older = NotificationDelivery::select(
            db,
            "WHERE status = 'PENDING' AND runtime_profile = ?1 AND recipient = ?2 AND agent_event_id IN (SELECT id FROM agent_event WHERE type = 'calendar.update_briefing')",
            [PROFILE, recipient],
        )?;
        for mut old in older {
            old.status = DeliveryStatus::Cancelled;
            old.handled_at = Some(now);
            old.update(db)?;
            acknowledge_delivery_events(db, &old, now)?;
            emit(db, "notifications.superseded", now, json!({"delivery_id": old.id, "new_event_id": event_id}))?;
        }
    }
    let row = new_delivery(None, Some(event_id), recipient, message, json!({"related_events": related_events}), now);
    row.insert(db)?;
    Ok(delivery_data(&row))
}

/// Ids of PENDING deliveries for the owner, oldest first.
pub fn pending(db: &Connection, recipient: &str) -> Result<Vec<String>> {
    Ok(NotificationDelivery::select(db, "WHERE status = 'PENDING' AND runtime_profile = ?1 AND recipient = ?2 ORDER BY created_at, id", [PROFILE, recipient])?
        .into_iter()
        .map(|d| d.id)
        .collect())
}

enum Claim {
    Done(Value),
    Send { chat: i64, message: String, buttons: Vec<Vec<Button>> },
}

/// Check route, authority and staleness, then commit SENDING. Nothing goes out unless this says so.
fn claim(db: &mut Connection, delivery_id: &str, now: Ts) -> Result<Claim> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(mut row) = NotificationDelivery::get(&tx, delivery_id)? else { crate::bail!("DELIVERY_NOT_FOUND", "Unknown notification delivery") };
    if row.runtime_profile != PROFILE {
        crate::bail!("DELIVERY_ROUTE_MISMATCH", "Delivery belongs to another runtime profile");
    }
    if let Some(settings) = RuntimeSettings::get(&tx, 1)?
        && (!settings.enabled || settings.profile != PROFILE || settings.recipient != row.recipient)
    {
        crate::bail!("RUNTIME_PAUSED", "Delivery authority paused or owner/profile mismatch");
    }
    if row.status != DeliveryStatus::Pending {
        return Ok(Claim::Done(delivery_data(&row)));
    }
    let job = match &row.notification_job_id {
        Some(id) => NotificationJob::get(&tx, id)?,
        None => None,
    };
    let reason = match &job {
        Some(job) => stale_reason(&tx, job, now, Some(&row))?,
        None => event_stale_reason(&tx, &row, now)?,
    };
    if let Some(reason) = reason {
        row.status = DeliveryStatus::Cancelled;
        row.handled_at = Some(now);
        row.update(&tx)?;
        if let Some(event_id) = &row.agent_event_id
            && let Some(mut event) = AgentEvent::get(&tx, event_id)?
        {
            event.handled_at = Some(now);
            event.update(&tx)?;
        }
        emit(&tx, "notifications.delivery_suppressed", now, json!({"delivery_id": row.id, "reason": reason}))?;
        tx.commit()?;
        return Ok(Claim::Done(delivery_data(&row)));
    }
    row.status = DeliveryStatus::Sending;
    row.handled_at = Some(now);
    row.update(&tx)?;
    // The session reminder starts the workout in one tap (handled by the coach, no model).
    let buttons = match &job {
        Some(job) if job.kind == JobKind::SessionStart => {
            let id: String = job.planned_session_id.as_deref().unwrap_or_default().chars().take(8).collect();
            vec![vec![Button { text: "▶️ Start workout".into(), data: format!("gc:begin:{id}") }]]
        }
        _ => vec![],
    };
    let chat = row.recipient.parse().map_err(|_| Error::domain("DELIVERY_ROUTE_MISMATCH", "Recipient is not a Telegram user id"))?;
    let message = row.message.clone();
    tx.commit()?;
    Ok(Claim::Send { chat, message, buttons })
}

/// Send one PENDING delivery. Returns its data; a failed send leaves it UNKNOWN and returns
/// TELEGRAM_DELIVERY_UNKNOWN.
pub async fn deliver(db_path: &Path, tg: &dyn Telegram, delivery_id: &str, now: Ts) -> Result<Value> {
    let id = delivery_id.to_string();
    let (chat, message, buttons) = match blocking(db_path, move |db| claim(db, &id, now)).await? {
        Claim::Done(data) => return Ok(data),
        Claim::Send { chat, message, buttons } => (chat, message, buttons),
    };
    let id = delivery_id.to_string();
    match tg.send(chat, &message, None, &buttons).await {
        Err(error) => {
            tracing::warn!("delivery {delivery_id} outcome unknown: {error}");
            blocking(db_path, move |db| {
                let tx = db.transaction()?;
                tx.execute("UPDATE notification_delivery SET status = 'UNKNOWN' WHERE id = ?1 AND status = 'SENDING'", [&id])?;
                emit(&tx, "notifications.delivery_unknown", now, json!({"delivery_id": id, "instruction": "Check Telegram before explicitly resolving; do not auto-retry."}))?;
                Ok(tx.commit()?)
            })
            .await?;
            crate::bail!("TELEGRAM_DELIVERY_UNKNOWN", "Delivery outcome unknown; inspect Telegram, then resolve it. No automatic resend.")
        }
        Ok(message_id) => {
            blocking(db_path, move |db| {
                let tx = db.transaction()?;
                tx.execute("UPDATE notification_delivery SET status = 'SENT', external_message_id = ?2 WHERE id = ?1 AND status = 'SENDING'", [&id, &message_id.to_string()])?;
                let row = NotificationDelivery::get(&tx, &id)?.expect("claimed row");
                acknowledge_delivery_events(&tx, &row, now)?;
                emit(&tx, "notifications.sent", now, json!({"delivery_id": id, "message_id": message_id.to_string()}))?;
                let meta = json!({"kind": "delivery", "delivery_id": id});
                ChatMessage { id: None, at: now, role: ChatRole::Bot, text: row.message.clone(), telegram_message_id: Some(message_id), meta_json: Json(meta) }.insert(&tx)?;
                tx.commit()?;
                Ok(delivery_data(&row))
            })
            .await
        }
    }
}

/// The owner's answer about an ambiguous delivery: `sent` closes it, `not-sent` queues it again.
pub fn resolve_delivery(db: &Connection, delivery_id: &str, sent: bool, now: Ts) -> Result<Value> {
    let Some(mut row) = NotificationDelivery::get(db, delivery_id)? else { crate::bail!("DELIVERY_NOT_FOUND", "Unknown notification delivery") };
    if !matches!(row.status, DeliveryStatus::Unknown | DeliveryStatus::Sending) {
        crate::bail!("INVALID_DELIVERY_RESOLUTION", "Resolve only ambiguous delivery with sent or not-sent");
    }
    row.status = if sent { DeliveryStatus::Sent } else { DeliveryStatus::Pending };
    row.handled_at = Some(now);
    row.update(db)?;
    if sent {
        acknowledge_delivery_events(db, &row, now)?;
    }
    let outcome = if sent { "sent" } else { "not-sent" };
    emit(db, "notifications.delivery_resolved", now, json!({"delivery_id": row.id, "outcome": outcome, "source": "operator_confirmation"}))?;
    Ok(delivery_data(&row))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::telegram::coach::tests::FakeTelegram;

    fn fresh() -> (tempdir::Dir, Connection) {
        let dir = tempdir::Dir::new();
        let mut db = crate::db::open(&dir.db()).unwrap();
        crate::db::migrate(&mut db).unwrap();
        (dir, db)
    }

    /// A throwaway directory under the system temp dir.
    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                let path = std::env::temp_dir().join(format!("gymclaw-test-{}", crate::db::new_id()));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }
            pub fn db(&self) -> std::path::PathBuf {
                self.0.join("gymclaw.db")
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn now() -> Ts {
        Ts::parse("2026-10-12T10:00:00Z", None).unwrap()
    }

    fn briefing(db: &Connection, at: Ts) -> String {
        let event = emit(db, "calendar.update_briefing", at, json!({})).unwrap();
        prepare_event_message(db, &event.id, "Moved Push to 14:30.", at, "42", &[]).unwrap()["id"].as_str().unwrap().into()
    }

    #[test]
    fn newer_briefing_supersedes_pending_one_and_old_messages_expire() {
        let (_dir, db) = fresh();
        let first = briefing(&db, now());
        let second = briefing(&db, now());
        assert_eq!(NotificationDelivery::get(&db, &first).unwrap().unwrap().status, DeliveryStatus::Cancelled);
        assert_eq!(pending(&db, "42").unwrap(), vec![second.clone()]);
        let row = NotificationDelivery::get(&db, &second).unwrap().unwrap();
        assert_eq!(event_stale_reason(&db, &row, now().mins(24 * 60 + 1)).unwrap(), Some("event_message_expired"));
        assert_eq!(event_stale_reason(&db, &row, now().mins(60)).unwrap(), None);
    }

    #[tokio::test]
    async fn failed_send_is_unknown_and_never_resent() {
        let (dir, db) = fresh();
        let id = briefing(&db, now());
        drop(db);
        let tg = Arc::new(FakeTelegram { fail_send: true, ..Arc::into_inner(FakeTelegram::new()).unwrap() });
        let error = deliver(&dir.db(), tg.as_ref(), &id, now()).await.unwrap_err();
        assert_eq!(error.code(), "TELEGRAM_DELIVERY_UNKNOWN");
        let db = crate::db::open(&dir.db()).unwrap();
        assert_eq!(NotificationDelivery::get(&db, &id).unwrap().unwrap().status, DeliveryStatus::Unknown);
        assert!(AgentEvent::first(&db, "WHERE type = 'notifications.delivery_unknown'", []).unwrap().is_some());
        // A second attempt only reports the state; it does not send again.
        let ok = FakeTelegram::new();
        let data = deliver(&dir.db(), ok.as_ref(), &id, now()).await.unwrap();
        assert_eq!(data["status"], "UNKNOWN");
        assert!(ok.calls().is_empty());
        // The owner says it never arrived: queued again, then sent once.
        resolve_delivery(&db, &id, false, now()).unwrap();
        let data = deliver(&dir.db(), ok.as_ref(), &id, now()).await.unwrap();
        assert_eq!((data["status"].as_str(), data["external_message_id"].as_str()), (Some("SENT"), Some("100")));
        assert_eq!(ok.calls().len(), 1);
    }

    #[tokio::test]
    async fn paused_runtime_sends_nothing() {
        let (dir, db) = fresh();
        db.execute("INSERT INTO runtime_settings VALUES (1, 'gymclaw', '42', '', '', NULL, 0, 0, 0)", []).unwrap();
        let id = briefing(&db, now());
        let tg = FakeTelegram::new();
        assert_eq!(deliver(&dir.db(), tg.as_ref(), &id, now()).await.unwrap_err().code(), "RUNTIME_PAUSED");
        assert!(tg.calls().is_empty());
        assert_eq!(NotificationDelivery::get(&db, &id).unwrap().unwrap().status, DeliveryStatus::Pending);
    }
}
