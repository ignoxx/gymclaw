//! In-process clocks.
//!
//! - Ticks: `watch` every 60 s, `weekly` Sunday ≥ 23:00 profile-local (once per week), `poll_crowd`
//!   every 15 min when enabled, `publish` after ticks when calendar writes are enabled, and the agent's
//!   own triggers (`extensions::fire_due`). Then the outbox sends whatever the ticks queued.
//! - Jobs: one loop sleeps until the next `notification_job` is due (or a wake-up after a write) and
//!   fires it (`Scheduler::fire`). REST jobs fire 30 s late: the coach countdown normally closes them.
//!
//! Nothing runs or sends while `runtime_settings.enabled` is off.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, ToSpan};
use serde_json::json;
use tokio::sync::Notify;

use super::{Health, blocking, in_tx, outbox};
use crate::db::models::{AgentEvent, JobKind, NotificationJob};
use crate::domain::core::emit;
use crate::domain::{profile, ticks};
use crate::error::{Error, Result};
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
    pub crowd_config: Option<PathBuf>,
    pub tg: Arc<dyn Telegram>,
    pub wake: Arc<Notify>,
    pub health: Arc<Mutex<Health>>,
    last_crowd: Mutex<Option<Instant>>,
    failed_jobs: Mutex<HashMap<String, Instant>>,
    rate_limited_until: Mutex<Option<Instant>>,
}

impl Scheduler {
    pub fn new(
        db_path: PathBuf,
        data_dir: PathBuf,
        recipient: String,
        crowd_config: Option<PathBuf>,
        tg: Arc<dyn Telegram>,
        wake: Arc<Notify>,
        health: Arc<Mutex<Health>>,
    ) -> Scheduler {
        Scheduler {
            db_path,
            data_dir,
            recipient,
            crowd_config,
            tg,
            wake,
            health,
            last_crowd: Mutex::new(None),
            failed_jobs: Mutex::new(HashMap::new()),
            rate_limited_until: Mutex::new(None),
        }
    }

    fn rate_limit_remaining(&self) -> Option<Duration> {
        let mut guard = self.rate_limited_until.lock().unwrap_or_else(PoisonError::into_inner);
        let until = (*guard)?;
        let now = Instant::now();
        if until > now {
            Some(until - now)
        } else {
            *guard = None;
            None
        }
    }

    fn note_rate_limit(&self, err: &Error) {
        if err.telegram_status() == Some(429) {
            let secs = err.retry_after().unwrap_or(JOB_BACKOFF.as_secs()).max(1);
            let until = Instant::now() + Duration::from_secs(secs);
            let mut guard = self.rate_limited_until.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.is_none_or(|prev| until > prev) {
                *guard = Some(until);
            }
        }
    }
    /// Send every PENDING delivery (when the runtime is enabled), after closing UNKNOWN ones that no
    /// longer apply.
    pub async fn pump(&self) {
        if self.rate_limit_remaining().is_some() {
            return;
        }
        let recipient = self.recipient.clone();
        let pending = blocking(&self.db_path, move |db| {
            if !outbox::runtime_authority(db)?.enabled {
                return Ok(vec![]);
            }
            crate::db::in_tx(db, |tx| outbox::settle_unknown(tx, &recipient, Ts::now()))?;
            outbox::pending(db, &recipient)
        })
        .await;
        match pending {
            Ok(ids) => {
                for id in ids {
                    if let Err(e) = outbox::deliver(&self.db_path, self.tg.as_ref(), &id, Ts::now()).await {
                        tracing::warn!("delivery {id}: {} {e}", e.code());
                        self.note_rate_limit(&e);
                        if matches!(e.telegram_status(), Some(429 | 401 | 404)) {
                            return;
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("outbox: {e}"),
        }
        if let Ok(chat) = self.recipient.parse()
            && let Err(e) = outbox::settle_session_messages(&self.db_path, self.tg.as_ref(), chat, Ts::now()).await
        {
            tracing::warn!("settle session messages: {e}");
        }
    }

    /// Run one tick pass, record its health summary, drain the outbox, and wake the job loop.
    pub async fn tick_once(&self) -> Result<()> {
        let result = self.tick().await;
        let summary = match &result {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("error {}", e.code()),
        };
        if let Err(e) = &result {
            tracing::warn!("tick: {} {e}", e.code());
        }
        self.health.lock().unwrap_or_else(PoisonError::into_inner).last_tick = Some((Ts::now(), summary));
        self.pump().await;
        self.wake.notify_one();
        result
    }

    /// The periodic loop. Also writes `data/heartbeat` for the container healthcheck.
    pub async fn ticks(self: Arc<Self>) {
        let mut interval = tokio::time::interval(WATCH_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let _ = tokio::fs::write(self.data_dir.join("heartbeat"), Ts::now().iso()).await;
            let _ = self.tick_once().await;
        }
    }

    async fn tick(&self) -> Result<()> {
        let authority = blocking(&self.db_path, |db| outbox::runtime_authority(db)).await?;
        if !authority.enabled {
            return Ok(());
        }
        let recipient = self.recipient.clone();
        let mut first_error = None;
        let r = recipient.clone();
        // Calendar and crowd ticks fetch over the network inside their transaction (`network_tx`).
        let watch = blocking(&self.db_path, move |db| {
            let tx = crate::db::network_tx(db)?;
            let data = ticks::watch(&tx, &r, Ts::now())?;
            tx.commit()?;
            Ok(data)
        });
        note(&mut first_error, "watch", watch.await);
        // The agent's own triggers queue wake events; the agent worker picks them up within a minute.
        let triggers = in_tx(&self.db_path, |db| crate::domain::extensions::fire_due(db, Ts::now()));
        note(&mut first_error, "triggers", triggers.await);
        // Commits per step (plan, publish, review event), so no outer transaction.
        note(&mut first_error, "weekly", blocking(&self.db_path, |db| weekly_if_due(db, Ts::now())).await);
        // The bot sends the recap as soon as the agent's review turn ends; this covers a review that
        // never ran. `pump` sends it after the ticks.
        let r = recipient.clone();
        let recap = in_tx(&self.db_path, move |db| crate::domain::weekly::release_recap(db, &r, Ts::now(), false));
        note(&mut first_error, "recap", recap.await);
        if authority.crowd_polling_enabled
            && let Some(crowd_path) = self.crowd_config.clone()
        {
            let due = {
                let mut last = self.last_crowd.lock().unwrap_or_else(PoisonError::into_inner);
                if last.is_none_or(|t| t.elapsed() >= CROWD_EVERY) {
                    *last = Some(Instant::now());
                    true
                } else {
                    false
                }
            };
            if due {
                let poll = blocking(&self.db_path, move |db| {
                    let tx = crate::db::network_tx(db)?;
                    let data = ticks::poll_crowd(&tx, &crowd_path, Ts::now())?;
                    tx.commit()?;
                    Ok(data)
                });
                note(&mut first_error, "crowd poll", poll.await);
            }
        }
        if authority.calendar_writes_enabled {
            // Network per write; it commits each write itself, so no outer transaction.
            note(&mut first_error, "publish", blocking(&self.db_path, |db| ticks::publish(db, Ts::now())).await);
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Fire all currently due notification jobs and return how long to sleep until the next one.
    pub async fn fire_due(&self) -> Duration {
        if let Some(wait) = self.rate_limit_remaining() {
            return wait.min(JOB_IDLE);
        }
        let jobs = blocking(&self.db_path, |db| {
            let enabled = outbox::runtime_authority(db)?.enabled;
            if enabled {
                NotificationJob::select(db, "WHERE status = 'PENDING' ORDER BY due_at, id", [])
            } else {
                Ok(vec![])
            }
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("jobs: {e}");
            vec![]
        });
        let now = Ts::now();
        let mut next: Option<Ts> = None;
        let mut due = Vec::new();
        {
            let mut failed = self.failed_jobs.lock().unwrap_or_else(PoisonError::into_inner);
            failed.retain(|_, at| at.elapsed() < JOB_BACKOFF);
            for job in jobs {
                if failed.contains_key(&job.id) {
                    continue;
                }
                let at = fire_at(&job);
                if at > now {
                    next = Some(next.map_or(at, |n| n.min(at)));
                    continue;
                }
                due.push(job.id);
            }
        }
        for job_id in due {
            if let Err(e) = self.fire(&job_id).await {
                tracing::warn!("job {job_id}: {} {e}", e.code());
                self.note_rate_limit(&e);
                if matches!(e.telegram_status(), Some(429 | 401 | 404)) {
                    break;
                }
                self.failed_jobs.lock().unwrap_or_else(PoisonError::into_inner).insert(job_id, Instant::now());
            }
        }
        if let Some(wait) = self.rate_limit_remaining() {
            return wait.min(JOB_IDLE);
        }
        let mut sleep =
            next.map(|n| Duration::try_from(n - Ts::now()).unwrap_or_default()).unwrap_or(JOB_IDLE).min(JOB_IDLE);
        if !self.failed_jobs.lock().unwrap_or_else(PoisonError::into_inner).is_empty() {
            sleep = sleep.min(JOB_BACKOFF);
        }
        sleep
    }

    /// Fire due notification jobs, forever.
    pub async fn jobs(self: Arc<Self>) {
        loop {
            let sleep = self.fire_due().await;
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    /// Advance the job and queue its message (`outbox::prepare_notification`), then send it.
    async fn fire(&self, job_id: &str) -> Result<()> {
        let id = job_id.to_string();
        let recipient = self.recipient.clone();
        let delivery = in_tx(&self.db_path, move |db| outbox::prepare_notification(db, &id, Ts::now(), &recipient));
        if let Some(delivery) = delivery.await? {
            outbox::deliver(&self.db_path, self.tg.as_ref(), &delivery, Ts::now()).await?;
        }
        Ok(())
    }
}

/// Log a failed tick step and keep the first error for the tick's result.
fn note<T>(first_error: &mut Option<Error>, what: &str, result: Result<T>) {
    if let Err(e) = result {
        tracing::warn!("{what}: {} {e}", e.code());
        first_error.get_or_insert(e);
    }
}

/// When the job loop fires a job.
fn fire_at(job: &NotificationJob) -> Ts {
    if job.kind == JobKind::Rest { job.due_at + REST_GRACE } else { job.due_at }
}

/// Run `ticks::weekly` once per week, on Sunday from 23:00 profile-local (late, so a Sunday evening
/// workout makes the recap; a restart before midnight still catches up), publish calendar writes, then
/// wake the agent for its weekly review (`weekly::announce`); the recap follows it (`weekly::release_recap`).
/// True when it ran.
pub fn weekly_if_due(db: &crate::db::Connection, now: Ts) -> Result<bool> {
    let tz = profile::get_profile(db)?.tz();
    let local = now.local(&tz);
    if local.weekday() != jiff::civil::Weekday::Sunday || local.hour() < 23 {
        return Ok(false);
    }
    let week = local.date().checked_add(1.day())?.to_string();
    let done =
        AgentEvent::first(db, "WHERE type = 'runtime.weekly_ran' AND payload_json ->> 'week_start' = ?1", [&week])?;
    if done.is_some() {
        return Ok(false);
    }
    // Drafting and the agent event are idempotent per week, including across crashes.
    let tx = crate::db::network_tx(db)?;
    ticks::weekly(&tx, now)?;
    tx.commit()?;
    // Publish before recording the weekly run; the agent reads the actual outbox state.
    let writes = outbox::runtime_authority(db)?.calendar_writes_enabled;
    let published = writes
        && match ticks::publish(db, now) {
            Ok(result) => result["published"] == true,
            Err(e) => {
                tracing::warn!("weekly publish: {} {e}", e.code());
                false
            }
        };
    let tx = db.unchecked_transaction()?;
    crate::domain::weekly::announce(&tx, now, writes && !published)?;
    emit(&tx, "runtime.weekly_ran", now, json!({"week_start": week}))?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_waits_for_sunday_night() {
        let db = crate::db::test::db();
        // Saturday 20:00 and Sunday 22:59 Berlin: not yet (ticks::weekly isn't even called).
        for at in ["2026-10-17T18:00:00Z", "2026-10-18T20:59:00Z"] {
            assert!(!weekly_if_due(&db, Ts::parse(at, None).unwrap()).unwrap());
        }
        // Already ran for the week starting Monday 19 Oct: not again.
        emit(&db, "runtime.weekly_ran", Ts::now(), json!({"week_start": "2026-10-19"})).unwrap();
        assert!(!weekly_if_due(&db, Ts::parse("2026-10-18T21:30:00Z", None).unwrap()).unwrap());
    }

    #[tokio::test]
    async fn tick_once_updates_health_and_pumps_outbox() {
        let (path, db) = crate::db::test::file();
        crate::db::test::authority().insert(&db).unwrap();
        let ev = emit(&db, "calendar.update_briefing", Ts::now(), json!({})).unwrap();
        outbox::prepare_event_message(&db, &ev.id, "Moved Push to 14:30.", Ts::now(), "42").unwrap();
        drop(db);

        let tg = crate::telegram::coach::tests::FakeTelegram::new();
        let health = Arc::new(Mutex::new(Health { started: Instant::now(), last_tick: None }));
        let sched = Scheduler::new(
            path.clone(),
            path.parent().unwrap().to_path_buf(),
            "42".into(),
            None,
            tg.clone(),
            Arc::new(Notify::new()),
            health.clone(),
        );
        sched.tick_once().await.unwrap();
        assert!(health.lock().unwrap().last_tick.is_some());
        assert_eq!(tg.calls().len(), 1);
    }

    #[tokio::test]
    async fn fire_due_returns_next_sleep_and_backs_off_failed_jobs() {
        let (path, db) = crate::db::test::file();
        crate::db::test::authority().insert(&db).unwrap();
        let now = Ts::now();
        let mut session = crate::db::models::PlannedSession::new("2026-W42".into(), None, false);
        session.id = "sess-1".into();
        session.status = crate::db::models::SessionStatus::Committed;
        session.source_revision = 1;
        session.planned_start_at = now.mins(10);
        session.planned_end_at = now.mins(70);
        session.insert(&db).unwrap();
        let future = NotificationJob {
            id: "job-future".into(),
            kind: JobKind::SessionStart,
            planned_session_id: Some("sess-1".into()),
            workout_session_id: None,
            set_log_id: None,
            due_at: now + SignedDuration::from_secs(90),
            payload_json: crate::db::Json(json!({})),
            status: crate::db::models::JobStatus::Pending,
            handled_at: None,
        };
        future.insert(&db).unwrap();
        drop(db);

        let tg = crate::telegram::coach::tests::FakeTelegram::new();
        let sched = Scheduler::new(
            path.clone(),
            path.parent().unwrap().to_path_buf(),
            "42".into(),
            None,
            tg,
            Arc::new(Notify::new()),
            Arc::new(Mutex::new(Health { started: Instant::now(), last_tick: None })),
        );
        let sleep = sched.fire_due().await;
        assert!(sleep <= Duration::from_secs(90) && sleep >= Duration::from_secs(80), "got {sleep:?}");

        // A failing due job records backoff and caps sleep at JOB_BACKOFF.
        let db = crate::db::open(&path).unwrap();
        let due_job = NotificationJob {
            id: "job-due".into(),
            kind: JobKind::SessionStart,
            planned_session_id: Some("sess-1".into()),
            workout_session_id: None,
            set_log_id: None,
            due_at: now - SignedDuration::from_secs(5),
            payload_json: crate::db::Json(json!({"revision": 1, "instruction": "Time to lift"})),
            status: crate::db::models::JobStatus::Pending,
            handled_at: None,
        };
        due_job.insert(&db).unwrap();
        drop(db);

        let failing_tg = Arc::new(crate::telegram::coach::tests::FakeTelegram {
            fail_send: true,
            ..Arc::into_inner(crate::telegram::coach::tests::FakeTelegram::new()).unwrap()
        });
        let sched_fail = Scheduler::new(
            path,
            PathBuf::from("/tmp"),
            "42".into(),
            None,
            failing_tg,
            Arc::new(Notify::new()),
            Arc::new(Mutex::new(Health { started: Instant::now(), last_tick: None })),
        );
        let sleep = sched_fail.fire_due().await;
        assert!(sleep <= JOB_BACKOFF, "expected backoff cap, got {sleep:?}");
        assert!(sched_fail.failed_jobs.lock().unwrap().contains_key("job-due"));
    }
}
