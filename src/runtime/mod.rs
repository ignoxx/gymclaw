//! Telegram + OpenRouter + scheduler + outbox: `gymclaw serve`.
pub mod bot;
pub mod outbox;
pub mod scheduler;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::agent::openrouter::OpenRouter;
use crate::agent::turnlog::TurnLog;
use crate::agent::{Agent, ChatModel};
use crate::config::Config;
use crate::db::Connection;
use crate::db::models::{ChatMessage, ChatRole};
use crate::domain::coach;
use crate::domain::view::Card;
use crate::error::{Error, Result};
use crate::telegram::Telegram;
use crate::telegram::client::Bot as TelegramBot;
use crate::telegram::coach::{Coach, Record, RestOver};
use crate::time::Ts;

/// What `/status` reports.
#[derive(Clone, Debug)]
pub struct Health {
    pub started: Instant,
    pub last_tick: Option<(Ts, String)>,
}

pub fn uptime(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    if mins < 60 {
        format!("{mins}m")
    } else if mins < 48 * 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{}d", mins / 1440)
    }
}

/// Run `f` on a fresh connection in the blocking pool. Domain code is synchronous; a panic becomes an
/// `INTERNAL` error instead of taking the runtime down.
pub async fn blocking<T: Send + 'static>(
    db_path: &Path,
    f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    let path: PathBuf = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut db = crate::db::open(&path)?;
        f(&mut db)
    })
    .await
    .unwrap_or_else(|e| Err(Error::domain("INTERNAL", format!("worker failed: {e}"))))
}

/// `blocking` inside one transaction (`db::in_tx`).
pub async fn in_tx<T: Send + 'static>(
    db_path: &Path,
    f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    blocking(db_path, move |db| crate::db::in_tx(db, f)).await
}

/// Dependencies needed to wire one owner's runtime (`Coach`, `Agent`, `Scheduler`, `Bot`).
pub struct OpenOptions {
    pub owner_id: i64,
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
    pub crowd_config: Option<PathBuf>,
    pub turn_log_dir: Option<PathBuf>,
    pub tg: Arc<dyn Telegram>,
    pub model: Arc<dyn ChatModel>,
    pub chat_model: String,
    pub vision_model: String,
    pub audio_model: String,
}

/// A wired runtime ready to handle updates and run scheduled ticks/jobs.
pub struct Runtime {
    pub bot: Arc<bot::Bot>,
    pub scheduler: Arc<scheduler::Scheduler>,
    pub coach: Arc<Coach>,
    pub wake: Arc<Notify>,
    pub health: Arc<Mutex<Health>>,
}

/// Wire `Coach`, `Agent`, `Scheduler`, and `Bot` without starting polling or scheduler loops.
/// Call `rt.bot.shutdown().await` when tearing down so background workers exit cleanly.
pub fn open(opts: OpenOptions) -> Runtime {
    let db_path = opts.db_path;
    let log = TurnLog::new(opts.turn_log_dir);
    let wake = Arc::new(Notify::new());
    let health = Arc::new(Mutex::new(Health { started: Instant::now(), last_tick: None }));

    let rest_over: RestOver = {
        let (db_path, wake) = (db_path.clone(), wake.clone());
        Arc::new(move || {
            let (db_path, wake) = (db_path.clone(), wake.clone());
            Box::pin(async move {
                match in_tx(&db_path, |db| coach::handle_rest_over(db, Ts::now())).await {
                    Ok(view) => {
                        // Closing the rest here keeps the scheduler's fallback ping silent.
                        wake.notify_one();
                        Some(view)
                    }
                    Err(e) => {
                        tracing::warn!("rest over: {} {e}", e.code());
                        None
                    }
                }
            })
        })
    };
    // Cards the owner sees go into the chat history, so the agent knows what's on screen.
    let record: Record = {
        let db_path = db_path.clone();
        Arc::new(move |card: &Card, text: &str, message_id: i64| {
            let (db_path, text, meta) = (db_path.clone(), text.to_string(), card.meta());
            tokio::spawn(async move {
                let saved = blocking(&db_path, move |db| {
                    ChatMessage::new(Ts::now(), ChatRole::Bot, text, Some(message_id), meta).insert(db)
                })
                .await;
                if let Err(e) = saved {
                    tracing::warn!("chat history write failed: {e}");
                }
            });
        })
    };
    let coach = Coach::new(opts.tg.clone(), opts.owner_id, Arc::new(Ts::now), Some(rest_over), Some(record));
    let agent = Agent {
        db_path: db_path.clone(),
        data_dir: opts.data_dir.clone(),
        recipient: Some(opts.owner_id.to_string()),
        model: opts.model,
        chat_model: opts.chat_model,
        vision_model: opts.vision_model,
        audio_model: opts.audio_model,
        coach: Some(coach.clone()),
        log: log.clone(),
        clock: Arc::new(Ts::now),
    };
    let scheduler = Arc::new(scheduler::Scheduler::new(
        db_path,
        opts.data_dir,
        opts.owner_id.to_string(),
        opts.crowd_config,
        opts.tg.clone(),
        wake.clone(),
        health.clone(),
    ));
    let bot = bot::Bot::start(opts.owner_id, opts.tg, coach.clone(), agent, log, wake.clone(), health.clone());
    Runtime { bot, scheduler, coach, wake, health }
}

/// Run the bot: Telegram long-poll, agent turns, coach, scheduler and outbox, until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    let telegram = TelegramBot::new(&config.telegram_token)?;
    if let Err(e) = telegram.set_commands(bot::COMMANDS).await {
        tracing::warn!("telegram command menu not updated: {e}");
    }
    let dashboard_url = config.dashboard_host.as_ref().map(|host| format!("https://{host}"));
    if let Err(e) = telegram.set_menu_button(dashboard_url.as_deref()).await {
        tracing::warn!("telegram menu button not updated: {e}");
    }
    let tg: Arc<dyn Telegram> = Arc::new(telegram);
    let model: Arc<dyn ChatModel> = Arc::new(OpenRouter::new(&config.openrouter_key, config.provider.clone())?);
    let rt = open(OpenOptions {
        owner_id: config.owner_id,
        db_path: config.db_path,
        data_dir: config.data_dir,
        crowd_config: config.crowd_config,
        turn_log_dir: config.turn_log_dir,
        tg,
        model,
        chat_model: config.model.clone(),
        vision_model: config.model_vision,
        audio_model: config.model_audio,
    });

    tracing::info!("gymclaw serving owner {} with {}", config.owner_id, config.model);
    let ticks = tokio::spawn(rt.scheduler.clone().ticks());
    let jobs = tokio::spawn(rt.scheduler.clone().jobs());
    tokio::select! {
        _ = rt.bot.clone().poll() => {}
        _ = shutdown() => {
            tracing::info!("shutting down");
        }
    }
    ticks.abort();
    jobs.abort();
    if tokio::time::timeout(Duration::from_secs(15), rt.bot.shutdown()).await.is_err() {
        tracing::warn!("timed out waiting for bot workers to stop");
    }
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ModelReply, Usage};

    struct DummyModel;

    #[async_trait::async_trait]
    impl ChatModel for DummyModel {
        async fn complete(
            &self,
            _: &str,
            _: &[serde_json::Value],
            _: &[serde_json::Value],
            _: bool,
        ) -> Result<ModelReply> {
            Ok(ModelReply {
                message: serde_json::json!({"role": "assistant", "content": ""}),
                finish_reason: Some("stop".into()),
                usage: Usage::default(),
                provider: None,
                generation_id: None,
                settings: serde_json::Value::Null,
                ms: 0,
                ttfb_ms: 0,
                req_bytes: 0,
                res_bytes: 0,
            })
        }
    }

    #[tokio::test]
    async fn open_and_shutdown_release_all_worker_refs() {
        let (db_path, _db) = crate::db::test::file();
        let data_dir = db_path.parent().unwrap().to_path_buf();
        let rt = open(OpenOptions {
            owner_id: 42,
            db_path,
            data_dir: data_dir.clone(),
            crowd_config: None,
            turn_log_dir: Some(data_dir.join("turns")),
            tg: crate::telegram::coach::tests::FakeTelegram::new(),
            model: Arc::new(DummyModel),
            chat_model: "test-chat".into(),
            vision_model: "test-vision".into(),
            audio_model: "test-audio".into(),
        });
        assert_eq!(Arc::strong_count(&rt.bot), 4);
        rt.bot.shutdown().await;
        assert_eq!(Arc::strong_count(&rt.bot), 1);
        // Second shutdown is a safe no-op.
        rt.bot.shutdown().await;
        assert_eq!(Arc::strong_count(&rt.bot), 1);
    }
}
