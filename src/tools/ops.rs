//! Runtime operations: deliveries, pause/resume, runtime status.
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ctx, Listed, Tool, ToolDef, def};
use crate::db::models::{NotificationDelivery, NotificationJob, RuntimeSettings};
use crate::error::Result;
use crate::runtime::outbox;

pub fn tools() -> Vec<ToolDef> {
    vec![
        def::<Delivery>(),
        def::<Runtime>(),
        def::<Deliveries>(),
        def::<DeliveryResolve>(),
        def::<RuntimeStatus>(),
        def::<RuntimePause>(),
        def::<RuntimeResume>(),
    ]
}

fn settings(cx: &Ctx) -> Result<RuntimeSettings> {
    match RuntimeSettings::get(cx.db, 1)? {
        Some(s) => Ok(s),
        None => crate::bail!("RUNTIME_NOT_CONFIGURED", "No runtime authority configured"),
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Deliveries {
    /// Only this status; default all.
    pub status: Option<crate::db::models::DeliveryStatus>,
}

impl Tool for Deliveries {
    const LISTED: Listed = Listed::Never;
    const NAME: &'static str = "deliveries";
    const DESCRIPTION: &'static str = "Proactive messages (reminders, briefings, alerts) and whether they reached Telegram, newest last (at most 30).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let rows = match self.status {
            Some(status) => {
                NotificationDelivery::select(cx.db, "WHERE status = ?1 ORDER BY created_at DESC LIMIT 30", [status])?
            }
            None => NotificationDelivery::select(cx.db, "ORDER BY created_at DESC LIMIT 30", [])?,
        };
        Ok(Value::Array(
            rows.iter()
                .rev()
                .map(|r| {
                    let mut data = outbox::delivery_data(r);
                    data["message"] = json!(r.message);
                    data
                })
                .collect(),
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct DeliveryResolve {
    pub delivery_id: String,
    /// What the owner saw in Telegram: `sent` (it arrived) or `not-sent` (it didn't; it is queued again).
    #[schemars(regex(pattern = r"^(sent|not-sent)$"))]
    pub outcome: String,
}

impl Tool for DeliveryResolve {
    const LISTED: Listed = Listed::Never;
    const NAME: &'static str = "delivery_resolve";
    const DESCRIPTION: &'static str = "Settle an UNKNOWN delivery after the owner checked Telegram.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let sent = match self.outcome.as_str() {
            "sent" => true,
            "not-sent" => false,
            _ => crate::bail!("INVALID_DELIVERY_RESOLUTION", "Resolve only ambiguous delivery with sent or not-sent"),
        };
        outbox::resolve_delivery(cx.db, &self.delivery_id, sent, cx.now)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RuntimeStatus {}

impl Tool for RuntimeStatus {
    const NAME: &'static str = "runtime_status";
    const LISTED: Listed = Listed::Never;
    const DESCRIPTION: &'static str =
        "Whether reminders and briefings are on, crowd-polling authority, open deliveries and the next reminder.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let authority = outbox::runtime_authority(cx.db)?;
        let count = |status: &str| -> Result<i64> {
            Ok(cx
                .db
                .query_row("SELECT count(*) FROM notification_delivery WHERE status = ?1", [status], |r| r.get(0))?)
        };
        let next = NotificationJob::first(cx.db, "WHERE status = 'PENDING' ORDER BY due_at", [])?;
        Ok(json!({
            "authority": authority,
            "deliveries": {"pending": count("PENDING")?, "sending": count("SENDING")?, "unknown": count("UNKNOWN")?},
            "next_job": next.map(|j| json!({"id": j.id, "kind": j.kind, "due_at": j.due_at})),
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RuntimePause {}

impl Tool for RuntimePause {
    const LISTED: Listed = Listed::Never;
    const NAME: &'static str = "runtime_pause";
    const DESCRIPTION: &'static str =
        "Stop all reminders, briefings and planning ticks until the owner explicitly resumes.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let mut settings = settings(cx)?;
        settings.enabled = false;
        settings.update(cx.db)?;
        Ok(serde_json::to_value(outbox::runtime_authority(cx.db)?)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RuntimeResume {
    /// True only when the owner asked for this in their own words in this conversation.
    pub owner_asked: bool,
    /// Turn polling the gym's live check-in count on (true) or off (false); omit to leave unchanged.
    #[serde(default)]
    pub crowd_polling: Option<bool>,
}

impl Tool for RuntimeResume {
    const LISTED: Listed = Listed::Never;
    const NAME: &'static str = "runtime_resume";
    const DESCRIPTION: &'static str = "Turn reminders, briefings and planning ticks back on (or toggle crowd polling). Needs the owner's explicit request.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        if !self.owner_asked || cx.background() {
            crate::bail!("RUNTIME_NOT_APPROVED", "Resuming needs the owner's explicit request; ask them first");
        }
        let mut settings = match RuntimeSettings::get(cx.db, 1)? {
            Some(s) => s,
            None => {
                let Some(recipient) = cx.recipient.as_deref() else {
                    crate::bail!("RUNTIME_NOT_CONFIGURED", "No delivery recipient configured")
                };
                let row = RuntimeSettings {
                    id: 1,
                    recipient: recipient.to_string(),
                    enabled: true,
                    calendar_writes_enabled: false,
                    crowd_polling_enabled: false,
                };
                row.insert(cx.db)?;
                row
            }
        };
        settings.enabled = true;
        if let Some(poll) = self.crowd_polling {
            settings.crowd_polling_enabled = poll;
        }
        settings.update(cx.db)?;
        Ok(serde_json::to_value(outbox::runtime_authority(cx.db)?)?)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Delivery {
    List(Deliveries),
    Resolve(DeliveryResolve),
}

impl Tool for Delivery {
    const NAME: &'static str = "delivery";
    const DESCRIPTION: &'static str =
        "Inspect proactive Telegram sends or settle an UNKNOWN delivery after the owner checked Telegram.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        match self {
            Delivery::List(t) => t.run(cx),
            Delivery::Resolve(t) => t.run(cx),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Runtime {
    Pause(RuntimePause),
    Resume(RuntimeResume),
}

impl Tool for Runtime {
    const NAME: &'static str = "runtime";
    const DESCRIPTION: &'static str =
        "Pause reminders/planning ticks, or resume them (and toggle crowd polling with owner_asked: true).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        match self {
            Runtime::Pause(t) => t.run(cx),
            Runtime::Resume(t) => t.run(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Ts;

    fn call(db: &crate::db::Connection, tool: &str, args: Value) -> Result<Value> {
        let dir = std::env::temp_dir();
        let mut cx = Ctx {
            db,
            now: Ts::parse("2026-10-11T17:00:00+00:00", None).unwrap(),
            request_id: "r".into(),
            data_dir: &dir,
            recipient: Some("99".into()),
            effects: vec![],
        };
        crate::tools::call(&mut cx, tool, args)
    }

    /// Pause, then resume only on the owner's own request.
    #[test]
    fn resume_needs_the_owner() {
        let db = crate::db::test::db();
        crate::db::test::authority().insert(&db).unwrap();
        assert_eq!(call(&db, "runtime_pause", json!({})).unwrap()["enabled"], false);
        assert_eq!(
            call(&db, "runtime_resume", json!({"owner_asked": false})).unwrap_err().code(),
            "RUNTIME_NOT_APPROVED"
        );
        let resumed = call(&db, "runtime_resume", json!({"owner_asked": true, "crowd_polling": true})).unwrap();
        assert_eq!(
            (resumed["enabled"].as_bool(), resumed["crowd_polling_enabled"].as_bool()),
            (Some(true), Some(true))
        );
        // Omitting crowd_polling leaves it on; passing false turns it off.
        assert_eq!(call(&db, "runtime_resume", json!({"owner_asked": true})).unwrap()["crowd_polling_enabled"], true);
        assert_eq!(
            call(&db, "runtime_resume", json!({"owner_asked": true, "crowd_polling": false})).unwrap()["crowd_polling_enabled"],
            false
        );

        // Fresh DB without RuntimeSettings initializes from cx.recipient without reading env vars.
        let fresh = crate::db::test::db();
        let created = call(&fresh, "runtime_resume", json!({"owner_asked": true})).unwrap();
        assert_eq!(created["enabled"], true);
        assert_eq!(RuntimeSettings::get(&fresh, 1).unwrap().unwrap().recipient, "99");
    }
}
