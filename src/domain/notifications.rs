//! Session reminder jobs (port of gymclaw/services/notifications.py). Owner: lead (planning).
use crate::db::Connection;
use crate::error::Result;
use crate::time::Ts;

/// Cancel a planned session's pending reminders.
pub fn cancel_session_jobs(db: &Connection, session_id: &str, now: Ts) -> Result<()> {
    let _ = (db, session_id, now);
    todo!("lead")
}
