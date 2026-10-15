//! In-process clocks (replace the OpenClaw cron jobs and the heartbeat session).
//!
//! - Ticks: `watch` every 60 s, `weekly` Sunday ≥ 19:00 profile-local (once per week), `poll_crowd`
//!   every 15 min when enabled, `publish` after ticks when calendar writes are enabled. Then the
//!   outbox sends whatever the ticks queued.
//! - Jobs: one loop sleeps until the next `notification_job` is due (or a wake-up after a write) and
//!   fires it like `runtime fire`. REST jobs fire 30 s late: the coach countdown normally closes them.
//!
//! Nothing runs or sends while `runtime_settings.enabled` is off.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::{SignedDuration, ToSpan};
use serde_json::json;
use tokio::sync::{Mutex, Notify};

use super::{Health, blocking, in_tx, outbox};
use crate::db::models::{AgentEvent, JobKind, NotificationJob};
use crate::domain::core::emit;
use crate::domain::{profile, ticks};
use crate::error::Result;
use crate::telegram::Telegram;
use crate::time::Ts;

const WATCH_EVERY: Duration = Duration::from_secs(60);
const CROWD_EVERY: Duration = Duration::from_secs(15 * 60);
/// Longest sleep of the job loop, so a missed wake-up costs at most this.
const JOB_IDLE: Duration = Duration::from_secs(300);
/// A job whose firing failed is retried after this (no busy loop on a broken job).
const JOB_BACKOFF: Duration = Duration::from_secs(60);
const REST_GRACE: SignedDuration = SignedDuration::from_secs(30);

pub struct Scheduler {
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
    pub recipient: String,
    pub tg: Arc<dyn Telegram>,
    pub wake: Arc<Notify>,
    pub health: Arc<Mutex<Health>>,
}

impl Scheduler {
    /// Send every PENDING delivery (when the runtime is enabled).
    pub async fn pump(&self) {
        let recipient = self.recipient.clone();
        let pending = blocking(&self.db_path, move |db| {
            let enabled = outbox::runtime_authority(db)?.enabled;
            if enabled { outbox::pending(db, &recipient) } else { Ok(vec![]) }
        })
        .await;
        match pending {
            Ok(ids) => {
                for id in ids {
                    if let Err(e) = outbox::deliver(&self.db_path, self.tg.as_ref(), &id, Ts::now()).await {
                        tracing::warn!("delivery {id}: {} {e}", e.code());
                    }
                }
            }
            Err(e) => tracing::warn!("outbox: {e}"),
        }
    }

    /// The periodic loop. Also writes `data/heartbeat` for the container healthcheck.
    pub async fn ticks(self: Arc<Self>) {
        let mut last_crowd: Option<Instant> = None;
        let mut interval = tokio::time::interval(WATCH_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let _ = tokio::fs::write(self.data_dir.join("heartbeat"), Ts::now().iso()).await;
            let result = self.tick(&mut last_crowd).await;
            let summary = match &result {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("error {}", e.code()),
            };
            if let Err(e) = result {
                tracing::warn!("tick: {} {e}", e.code());
            }
            self.health.lock().await.last_tick = Some((Ts::now(), summary));
            self.pump().await;
            self.wake.notify_one();
        }
    }

    async fn tick(&self, last_crowd: &mut Option<Instant>) -> Result<()> {
        let authority = blocking(&self.db_path, |db| outbox::runtime_authority(db)).await?;
        if !authority.enabled {
            return Ok(());
        }
        let recipient = self.recipient.clone();
        let mut first_error = None;
        let mut note = |r: Result<serde_json::Value>, what: &str| {
            if let Err(e) = r {
                tracing::warn!("{what}: {} {e}", e.code());
                first_error.get_or_insert(e);
            }
        };
        let r = recipient.clone();
        note(in_tx(&self.db_path, move |db| ticks::watch(db, &r, Ts::now())).await, "watch");
        let r = recipient.clone();
        // Commits per step (plan, publish, briefing), so no outer transaction.
        note(blocking(&self.db_path, move |db| weekly_if_due(db, &r, Ts::now())).await, "weekly");
        if authority.crowd_polling_enabled && last_crowd.is_none_or(|t| t.elapsed() >= CROWD_EVERY) {
            *last_crowd = Some(Instant::now());
            note(in_tx(&self.db_path, |db| ticks::poll_crowd(db, Ts::now())).await, "crowd poll");
        }
        if authority.calendar_writes_enabled {
            // Network per write; it commits each write itself, so no outer transaction.
            note(blocking(&self.db_path, |db| ticks::publish(db, Ts::now())).await, "publish");
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Fire due notification jobs, forever.
    pub async fn jobs(self: Arc<Self>) {
        let mut failed: HashMap<String, Instant> = HashMap::new();
        loop {
            let jobs = blocking(&self.db_path, |db| {
                let enabled = outbox::runtime_authority(db)?.enabled;
                if enabled { NotificationJob::select(db, "WHERE status = 'PENDING' ORDER BY due_at, id", []) } else { Ok(vec![]) }
            })
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("jobs: {e}");
                vec![]
            });
            failed.retain(|_, at| at.elapsed() < JOB_BACKOFF);
            let now = Ts::now();
            let mut next: Option<Ts> = None;
            for job in &jobs {
                if failed.contains_key(&job.id) {
                    continue;
                }
                let at = fire_at(job);
                if at > now {
                    next = Some(next.map_or(at, |n| n.min(at)));
                    continue;
                }
                if let Err(e) = self.fire(&job.id).await {
                    tracing::warn!("job {}: {} {e}", job.id, e.code());
                    failed.insert(job.id.clone(), Instant::now());
                }
            }
            let mut sleep = next.map(|n| Duration::try_from(n - Ts::now()).unwrap_or_default()).unwrap_or(JOB_IDLE).min(JOB_IDLE);
            if !failed.is_empty() {
                sleep = sleep.min(JOB_BACKOFF);
            }
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    /// `runtime fire`: advance the job and queue its message, then send it.
    async fn fire(&self, job_id: &str) -> Result<()> {
        let id = job_id.to_string();
        let recipient = self.recipient.clone();
        let data = blocking(&self.db_path, move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let data = outbox::prepare_notification(&tx, &id, Ts::now(), &recipient)?;
            tx.commit()?;
            Ok(data)
        })
        .await?;
        if let Some(delivery) = data["delivery_id"].as_str() {
            outbox::deliver(&self.db_path, self.tg.as_ref(), delivery, Ts::now()).await?;
        }
        Ok(())
    }
}

/// When the job loop fires a job.
pub fn fire_at(job: &NotificationJob) -> Ts {
    if job.kind == JobKind::Rest { job.due_at + REST_GRACE } else { job.due_at }
}

/// Run `ticks::weekly` once per week, on Sunday from 19:00 profile-local (a restart after 19:00
/// still catches up the same evening), publish calendar writes, then queue the briefing.
pub fn weekly_if_due(db: &crate::db::Connection, recipient: &str, now: Ts) -> Result<serde_json::Value> {
    let tz = profile::get_profile(db)?.tz();
    let local = now.local(&tz);
    if local.weekday() != jiff::civil::Weekday::Sunday || local.hour() < 19 {
        return Ok(json!({"due": false}));
    }
    let week = local.date().checked_add(1.day())?.to_string();
    let done = AgentEvent::first(db, "WHERE type = 'runtime.weekly_ran' AND payload_json ->> 'week_start' = ?1", [&week])?;
    if done.is_some() {
        return Ok(json!({"due": false}));
    }
    // The week is marked done only with its briefing; a crash before that replans on the next tick,
    // which is idempotent per week (request id + briefing correlation key).
    let tx = db.unchecked_transaction()?;
    let mut data = ticks::weekly(&tx, now)?;
    tx.commit()?;
    // Publish before briefing so the briefing reports what actually reached the calendar.
    let published = outbox::runtime_authority(db)?.calendar_writes_enabled
        && match ticks::publish(db, now) {
            Ok(result) => result["published"] == true,
            Err(e) => {
                tracing::warn!("weekly publish: {} {e}", e.code());
                false
            }
        };
    let tx = db.unchecked_transaction()?;
    ticks::weekly_briefing(&tx, recipient, &mut data, published, now)?;
    emit(&tx, "runtime.weekly_ran", now, json!({"week_start": week}))?;
    tx.commit()?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_waits_for_sunday_evening() {
        let mut db = crate::db::open(&std::env::temp_dir().join(format!("gymclaw-weekly-{}.db", crate::db::new_id()))).unwrap();
        crate::db::migrate(&mut db).unwrap();
        // Saturday 20:00 and Sunday 18:59 Berlin: not yet (ticks::weekly isn't even called).
        for at in ["2026-10-17T18:00:00Z", "2026-10-18T16:59:00Z"] {
            assert_eq!(weekly_if_due(&db, "42", Ts::parse(at, None).unwrap()).unwrap(), json!({"due": false}));
        }
        // Already ran for the week starting Monday 19 Oct: not again.
        emit(&db, "runtime.weekly_ran", Ts::now(), json!({"week_start": "2026-10-19"})).unwrap();
        assert_eq!(weekly_if_due(&db, "42", Ts::parse("2026-10-18T17:30:00Z", None).unwrap()).unwrap(), json!({"due": false}));
    }
}
