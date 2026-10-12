//! Periodic domain work the scheduler runs (was `runtime watch|weekly|poll-crowd`). Owner: lead.
//! Each runs in one transaction and queues owner messages as PENDING `notification_delivery` rows;
//! the runtime's outbox sends them afterwards.
use serde_json::Value;

use crate::db::Connection;
use crate::error::Result;
use crate::time::Ts;

/// Every 60 s: calendar sync, personal feed, rolling replan, no-show nudges, crowd alert, plan-change briefing.
pub fn watch(db: &Connection, recipient: &str, now: Ts) -> Result<Value> {
    let _ = (db, recipient, now);
    todo!("lead")
}

/// Sunday 19:00 local: plan next week, audit last week, queue the weekly briefing.
pub fn weekly(db: &Connection, recipient: &str, now: Ts) -> Result<Value> {
    let _ = (db, recipient, now);
    todo!("lead")
}

/// Every 15 min when crowd polling is enabled: read the gym's live count.
pub fn poll_crowd(db: &Connection, now: Ts) -> Result<Value> {
    let _ = (db, now);
    todo!("lead")
}

/// Push PENDING calendar writes to Google (when calendar writes are enabled). Network per write.
pub fn publish(db: &Connection, now: Ts) -> Result<Value> {
    let _ = (db, now);
    todo!("lead")
}
