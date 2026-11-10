//! Telegram + OpenRouter + scheduler + outbox: `gymclaw serve`.
pub mod bot;
pub mod outbox;
pub mod scheduler;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{Mutex, Notify};

use crate::agent::openrouter::OpenRouter;
use crate::agent::turnlog::TurnLog;
use crate::agent::{Agent, ChatModel};
use crate::config::Config;
use crate::db::models::{ChatMessage, ChatRole};
use crate::db::{Connection, Json};
use crate::domain::coach;
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

/// `blocking` inside one transaction, committed when `f` succeeds.
pub async fn in_tx<T: Send + 'static>(
    db_path: &Path,
    f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    blocking(db_path, move |db| {
        let tx = db.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    })
    .await
}

/// Run the bot: Telegram long-poll, agent turns, coach, scheduler and outbox, until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    let db_path = config.db_path.clone();
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
    let log = TurnLog::new(config.turn_log_dir.clone());
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
        Arc::new(move |text: &str, message_id: i64| {
            let (db_path, text) = (db_path.clone(), text.to_string());
            tokio::spawn(async move {
                let saved = blocking(&db_path, move |db| {
                    ChatMessage {
                        id: None,
                        at: Ts::now(),
                        role: ChatRole::Bot,
                        text,
                        telegram_message_id: Some(message_id),
                        meta_json: Json(json!({"kind": "coach"})),
                    }
                    .insert(db)
                })
                .await;
                if let Err(e) = saved {
                    tracing::warn!("chat history write failed: {e}");
                }
            });
        })
    };
    let coach = Coach::new(tg.clone(), config.owner_id, Arc::new(Ts::now), Some(rest_over), Some(record));
    let agent = Agent {
        db_path: db_path.clone(),
        data_dir: config.data_dir.clone(),
        model,
        chat_model: config.model.clone(),
        vision_model: config.model_vision.clone(),
        coach: Some(coach.clone()),
        log: log.clone(),
        clock: Arc::new(Ts::now),
    };
    let scheduler = Arc::new(scheduler::Scheduler {
        db_path: db_path.clone(),
        data_dir: config.data_dir.clone(),
        recipient: config.owner_id.to_string(),
        tg: tg.clone(),
        wake: wake.clone(),
        health: health.clone(),
    });
    let bot = bot::Bot::start(config.owner_id, tg, coach, agent, log, wake, health);

    tracing::info!("gymclaw serving owner {} with {}", config.owner_id, config.model);
    tokio::spawn(scheduler.clone().ticks());
    tokio::spawn(scheduler.jobs());
    tokio::select! {
        _ = bot.poll() => Ok(()),
        _ = shutdown() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
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
