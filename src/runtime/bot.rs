//! Inbound routing for the owner DM.
//!
//! - `gc:` taps and typed sets go to the coach on the fast lane (no model, in arrival order).
//! - `btn:<id>` taps, `handled: false` sets and everything else go to the agent. Turns are serialized;
//!   messages that arrive during a turn are collected into the next one.
//! - `/new` starts a fresh chat window, `/status` answers without the model.
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc};

use super::{blocking, in_tx};
use crate::agent::turnlog::{TurnLog, scrub};
use crate::agent::{Agent, Outcome, TurnInput, media};
use crate::db::Json;
use crate::db::models::{AgentButton, ChatMessage, ChatRole, NotificationJob};
use crate::domain::coach;
use crate::domain::view::Button;
use crate::error::Result;
use crate::telegram::coach::{Coach, Tapped};
use crate::telegram::{CallbackQuery, Media, Message, Telegram, Update};
use crate::time::Ts;

/// Cheap pre-check so ordinary chat never pays for a coach lookup. Reps alone ("13 reps") only log on
/// a bodyweight exercise; otherwise the coach returns unhandled and the agent gets the message.
pub static SET_LIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*[x×*]\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*$|^\s*\d+\s*reps?\s*(?:at|@)\s*\d+(?:[.,]\d+)?\s*(?:kg)?\s*$|^\s*\d+\s*(?:reps?|[x×*])?\s*$").unwrap()
});

const ACK: &str = "👀";
const SESSION: &str = "telegram:owner";

#[derive(Debug, PartialEq)]
pub enum Route {
    New,
    Status,
    /// Try the coach first.
    Set,
    Agent,
}

pub fn route(text: &str) -> Route {
    let command = text.split_whitespace().next().unwrap_or("").split('@').next().unwrap_or("");
    match command {
        "/new" | "/reset" => Route::New,
        "/status" => Route::Status,
        _ if SET_LIKE.is_match(text) => Route::Set,
        _ => Route::Agent,
    }
}

/// An owner message waiting for the agent.
#[derive(Clone, Debug)]
struct Inbound {
    /// Message that gets 👀 and, if the reply is a reaction, the reaction.
    message_id: i64,
    key: String,
    images: Vec<Media>,
}

enum Fast {
    Tap(Box<CallbackQuery>),
    Text { message_id: i64, text: String },
}

pub struct Bot {
    pub owner: i64,
    pub tg: Arc<dyn Telegram>,
    pub coach: Arc<Coach>,
    pub agent: Agent,
    pub log: TurnLog,
    /// Wakes the scheduler after anything that may have changed jobs or deliveries.
    pub wake: Arc<Notify>,
    pub status: Arc<Mutex<super::Health>>,
    inbox: Mutex<Vec<Inbound>>,
    inbox_ready: Notify,
    fast: mpsc::UnboundedSender<Fast>,
}

impl Bot {
    /// Start the fast lane and the agent worker.
    pub fn start(owner: i64, tg: Arc<dyn Telegram>, coach: Arc<Coach>, agent: Agent, log: TurnLog, wake: Arc<Notify>, status: Arc<Mutex<super::Health>>) -> Arc<Bot> {
        let (fast, mut lane) = mpsc::unbounded_channel();
        let bot = Arc::new(Bot { owner, tg, coach, agent, log, wake, status, inbox: Mutex::new(vec![]), inbox_ready: Notify::new(), fast });
        let worker = bot.clone();
        tokio::spawn(async move {
            while let Some(job) = lane.recv().await {
                match job {
                    Fast::Tap(q) => worker.tap(*q).await,
                    Fast::Text { message_id, text } => worker.set_text(message_id, text).await,
                }
            }
        });
        let worker = bot.clone();
        tokio::spawn(async move { worker.agent_loop().await });
        bot
    }

    fn db(&self) -> &std::path::Path {
        &self.agent.db_path
    }

    /// Long-poll Telegram forever (single poller).
    pub async fn poll(self: Arc<Self>) {
        let mut offset = 0;
        loop {
            match self.tg.updates(offset, 50).await {
                Ok(updates) => {
                    for update in updates {
                        offset = offset.max(update.update_id + 1);
                        self.handle(update).await;
                    }
                }
                Err(e) => {
                    tracing::warn!("getUpdates failed: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Owner DMs only; everything else is dropped.
    pub async fn handle(&self, update: Update) {
        if let Some(q) = update.callback_query {
            let chat_ok = q.message.as_ref().is_none_or(|m| m.chat.id == self.owner);
            if q.from.id == self.owner && chat_ok {
                self.callback(q).await;
            }
            return;
        }
        let Some(message) = update.message else { return };
        if message.chat.kind != "private" || message.chat.id != self.owner || message.from.as_ref().is_none_or(|f| f.id != self.owner) {
            return;
        }
        self.message(message).await;
    }

    async fn callback(&self, q: CallbackQuery) {
        let data = q.data.clone().unwrap_or_default();
        if data.starts_with("gc:") {
            let _ = self.fast.send(Fast::Tap(Box::new(q)));
            return;
        }
        let Some(id) = data.strip_prefix("btn:").map(String::from) else {
            let _ = self.tg.answer_callback(&q.id, Some("⌛ Old button.")).await;
            return;
        };
        let button = blocking(self.db(), move |db| AgentButton::get(db, &id)).await.ok().flatten();
        let Some(button) = button else {
            let _ = self.tg.answer_callback(&q.id, Some("⌛ Old button.")).await;
            return;
        };
        let _ = self.tg.answer_callback(&q.id, None).await;
        // The choice is made: the buttons go, the owner's pick shows as their message in the history.
        let message_id = q.message.as_ref().map(|m| m.message_id).unwrap_or_default();
        if message_id != 0 {
            let _ = self.tg.edit_buttons(self.owner, message_id, &vec![]).await;
        }
        self.to_agent(message_id, format!("tg-cb:{}", q.id), button.value, vec![], json!({"kind": "button", "label": button.label})).await;
    }

    async fn message(&self, message: Message) {
        let text = message.body().unwrap_or("").to_string();
        let images = self.media(&message).await;
        if images.is_empty() {
            match route(&text) {
                Route::New => return self.new_window(message.message_id).await,
                Route::Status => return self.send_status().await,
                Route::Set => {
                    let _ = self.fast.send(Fast::Text { message_id: message.message_id, text });
                    return;
                }
                Route::Agent => {}
            }
        }
        let mut text = text;
        for image in &images {
            text.push_str(&format!("\n[photo saved at {}]", image.path.display()));
        }
        if message.photo.is_none() && message.document.is_some() && images.is_empty() {
            text.push_str("\n[a file that isn't an image; it wasn't saved]");
        }
        self.to_agent(message.message_id, format!("tg-{}", message.message_id), text.trim().to_string(), images, json!({})).await;
    }

    /// Download the largest photo size or an image document.
    async fn media(&self, message: &Message) -> Vec<Media> {
        let file = match (&message.photo, &message.document) {
            (Some(sizes), _) => sizes.iter().max_by_key(|s| s.width).map(|s| (s.file_id.clone(), s.file_unique_id.clone())),
            (None, Some(doc)) => Some((doc.file_id.clone(), doc.file_unique_id.clone())),
            _ => None,
        };
        let Some((file_id, unique)) = file else { return vec![] };
        match self.tg.download(&file_id).await {
            Ok(bytes) => match media::save(&self.agent.data_dir, &unique, &bytes).await {
                Ok(Some(m)) => vec![m],
                Ok(None) => vec![],
                Err(e) => {
                    tracing::warn!("saving media failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("download failed: {e}");
                vec![]
            }
        }
    }

    /// Queue an owner message for the agent: 👀, stored in the chat history, collected into the next turn.
    async fn to_agent(&self, message_id: i64, key: String, text: String, images: Vec<Media>, meta: Value) {
        if message_id != 0 {
            let _ = self.tg.react(self.owner, message_id, Some(ACK)).await;
        }
        self.log.log(json!({"ev": "inbound", "session": SESSION, "channel": "telegram", "msg": message_id.to_string(),
            "sentAt": Ts::now().0.as_millisecond(), "chars": text.chars().count(), "text": scrub(&text, 4000)}));
        let stored = text.clone();
        let saved = blocking(self.db(), move |db| {
            ChatMessage { id: None, at: Ts::now(), role: ChatRole::Owner, text: stored, telegram_message_id: (message_id != 0).then_some(message_id), meta_json: Json(meta) }.insert(db)
        })
        .await;
        if let Err(e) = saved {
            tracing::warn!("chat history write failed: {e}");
        }
        self.inbox.lock().await.push(Inbound { message_id, key, images });
        self.inbox_ready.notify_one();
    }

    async fn new_window(&self, message_id: i64) {
        let saved = blocking(self.db(), |db| {
            ChatMessage { id: None, at: Ts::now(), role: ChatRole::Bot, text: "/new".into(), telegram_message_id: None, meta_json: Json(json!({"kind": "boundary"})) }.insert(db)
        })
        .await;
        let emoji = if saved.is_ok() { "👍" } else { "🤷" };
        let _ = self.tg.react(self.owner, message_id, Some(emoji)).await;
    }

    async fn send_status(&self) {
        let health = self.status.lock().await.clone();
        let model = self.agent.chat_model.clone();
        let text = blocking(self.db(), move |db| status_text(db, &health, &model)).await.unwrap_or_else(|e| format!("⚠️ Status unavailable: {}", e.code()));
        self.send_bot(&text, "status").await;
    }

    /// A message from the bot itself (status), stored as `bot` in the history.
    async fn send_bot(&self, text: &str, kind: &'static str) {
        match self.tg.send(self.owner, text, None, &vec![]).await {
            Ok(id) => self.remember(ChatRole::Bot, text.to_string(), Some(id), json!({"kind": kind})).await,
            Err(e) => tracing::warn!("send failed: {e}"),
        }
    }

    async fn remember(&self, role: ChatRole, text: String, message_id: Option<i64>, meta: Value) {
        let saved = blocking(self.db(), move |db| ChatMessage { id: None, at: Ts::now(), role, text, telegram_message_id: message_id, meta_json: Json(meta) }.insert(db)).await;
        if let Err(e) = saved {
            tracing::warn!("chat history write failed: {e}");
        }
    }

    async fn tap(&self, q: CallbackQuery) {
        let _ = self.tg.answer_callback(&q.id, None).await;
        let started = Instant::now();
        let data = q.data.clone().unwrap_or_default();
        let action = data.split(':').nth(1).unwrap_or("").to_string();
        let message_id = q.message.as_ref().map(|m| m.message_id).unwrap_or_default();
        let request_id = format!("tg-cb:{}", q.id);
        let result = in_tx(self.db(), move |db| coach::handle_tap(db, &data, Ts::now(), &request_id)).await;
        let cli_ms = started.elapsed().as_millis() as u64;
        let error = match result {
            Ok(view) => {
                let tapped = Tapped { message_id, text: q.message.as_ref().and_then(|m| m.body().map(String::from)) };
                if let Err(e) = self.coach.apply(&view, Some(&tapped), None).await {
                    tracing::warn!("coach render failed: {e}");
                }
                self.wake.notify_one();
                None
            }
            Err(e) => {
                tracing::warn!("coach tap: {} {e}", e.code());
                self.coach.warn(&e.to_string()).await;
                Some(e.code().to_string())
            }
        };
        self.log.log(json!({"ev": "fast", "flow": "tap", "action": action, "msg": message_id, "cliMs": cli_ms,
            "ok": error.is_none(), "error": error, "ms": started.elapsed().as_millis() as u64}));
    }

    async fn set_text(&self, message_id: i64, text: String) {
        let started = Instant::now();
        let request_id = format!("tg-msg:{message_id}");
        let input = text.clone();
        let result = in_tx(self.db(), move |db| coach::handle_text(db, &input, Ts::now(), &request_id)).await;
        let cli_ms = started.elapsed().as_millis() as u64;
        let mut entry = json!({"ev": "fast", "flow": "text", "text": text, "msg": message_id.to_string(), "cliMs": cli_ms});
        match result {
            Ok(view) if view.handled => {
                if let Err(e) = self.coach.apply(&view, None, Some(message_id)).await {
                    tracing::warn!("coach render failed: {e}");
                }
                self.remember(ChatRole::Owner, text, Some(message_id), json!({"kind": "set"})).await;
                self.wake.notify_one();
                entry["handled"] = json!(true);
                entry["ok"] = json!(true);
            }
            other => {
                // Not a set after all (or no workout to log into): the agent explains.
                if let Err(e) = &other {
                    tracing::info!("typed set not handled: {}", e.code());
                    entry["error"] = json!(e.code());
                }
                entry["handled"] = json!(false);
                self.to_agent(message_id, format!("tg-{message_id}"), text, vec![], json!({})).await;
            }
        }
        entry["ms"] = json!(started.elapsed().as_millis() as u64);
        self.log.log(entry);
    }

    async fn agent_loop(&self) {
        loop {
            self.inbox_ready.notified().await;
            loop {
                let batch = std::mem::take(&mut *self.inbox.lock().await);
                if batch.is_empty() {
                    break;
                }
                self.turn(batch).await;
            }
        }
    }

    async fn turn(&self, batch: Vec<Inbound>) {
        let typing = {
            let tg = self.tg.clone();
            let chat = self.owner;
            tokio::spawn(async move {
                loop {
                    let _ = tg.typing(chat).await;
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
            })
        };
        let last = batch.last().expect("non-empty batch").clone();
        let images = batch.iter().flat_map(|b| b.images.clone()).collect();
        let outcome = self.agent.turn(TurnInput { key: last.key.clone(), images }).await;
        typing.abort();
        self.answer(&outcome, &batch).await;
        self.wake.notify_one();
    }

    /// Send the turn's one outcome and clear the 👀 acks.
    async fn answer(&self, outcome: &Outcome, batch: &[Inbound]) {
        let reply = &outcome.reply;
        let target = batch.iter().rev().find(|b| b.message_id != 0).map(|b| b.message_id);
        let mut reacted = None;
        if let (Some(emoji), Some(message)) = (&reply.react, target) {
            match self.tg.react(self.owner, message, Some(emoji)).await {
                Ok(()) => reacted = Some(message),
                // Not every emoji is an allowed reaction: say it instead.
                Err(_) if reply.text.is_none() => self.send_reply(outcome, emoji, &[]).await,
                Err(_) => {}
            }
        }
        if let Some(text) = &reply.text {
            let buttons: Vec<(String, String)> = reply.buttons.iter().map(|b| (b.label.clone(), b.value.clone())).collect();
            self.send_reply(outcome, text, &buttons).await;
        }
        for b in batch.iter().filter(|b| b.message_id != 0 && Some(b.message_id) != reacted) {
            let _ = self.tg.react(self.owner, b.message_id, None).await;
        }
    }

    async fn send_reply(&self, outcome: &Outcome, text: &str, buttons: &[(String, String)]) {
        let rows: Vec<(String, String, String)> = buttons.iter().map(|(label, value)| (crate::db::new_id()[..8].to_string(), label.clone(), value.clone())).collect();
        if !rows.is_empty() {
            let stored = rows.clone();
            let saved = blocking(self.db(), move |db| {
                for (id, label, value) in stored {
                    AgentButton { id, label, value, created_at: Ts::now() }.insert(db)?;
                }
                Ok(())
            })
            .await;
            if let Err(e) = saved {
                tracing::warn!("agent buttons not stored: {e}");
            }
        }
        let keyboard: Vec<Vec<Button>> = rows.iter().map(|(id, label, _)| vec![Button { text: label.clone(), data: format!("btn:{id}") }]).collect();
        let sent = self.tg.send(self.owner, text, None, &keyboard).await;
        self.log.log(json!({"ev": "outbound", "session": SESSION, "run": outcome.run_id, "channel": "telegram", "msg": sent.as_ref().ok(),
            "ok": sent.is_ok(), "error": sent.as_ref().err().map(|e| scrub(&e.to_string(), 500)), "text": scrub(text, 4000)}));
        match sent {
            Ok(id) => {
                let meta = json!({"kind": if outcome.failed { "failure" } else { "reply" }, "run": outcome.run_id, "buttons": buttons.iter().map(|b| &b.0).collect::<Vec<_>>()});
                self.remember(ChatRole::Assistant, text.to_string(), Some(id), meta).await;
            }
            Err(e) => tracing::warn!("reply not sent: {e}"),
        }
    }
}

/// `/status`: runtime health in a few lines, no model.
fn status_text(db: &crate::db::Connection, health: &super::Health, model: &str) -> Result<String> {
    let authority = super::outbox::runtime_authority(db)?;
    let tz = crate::domain::profile::get_profile(db)?.tz();
    let count = |s: &str| -> Result<i64> { Ok(db.query_row("SELECT count(*) FROM notification_delivery WHERE status = ?1", [s], |r| r.get(0))?) };
    let on = |b: bool| if b { "on" } else { "off" };
    let mut lines = vec![
        format!("**GymClaw** · up {}", super::uptime(health.started.elapsed())),
        format!(
            "Runtime {} · calendar writes {} · crowd polling {}",
            if authority.enabled { "on" } else if authority.configured { "paused" } else { "not set up" },
            on(authority.calendar_writes_enabled),
            on(authority.crowd_polling_enabled)
        ),
        format!("Deliveries: {} pending · {} sending · {} unknown", count("PENDING")?, count("SENDING")?, count("UNKNOWN")?),
    ];
    if let Some(job) = NotificationJob::first(db, "WHERE status = 'PENDING' ORDER BY due_at", [])? {
        lines.push(format!("Next: {} {}", job.kind, job.due_at.local(&tz).strftime("%a %H:%M")));
    }
    if let Some((at, result)) = &health.last_tick {
        lines.push(format!("Last tick {}: {result}", at.local(&tz).strftime("%H:%M")));
    }
    lines.push(format!("Model: `{model}`"));
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_like_routing() {
        for text in ["80x9", "12x40kg", "90kgx10 reps", "9 reps at 80", "22,5 × 8", "13 reps", "13x", "13"] {
            assert_eq!(route(text), Route::Set, "{text}");
        }
        for text in ["how long do I rest?", "it's occupied", "80x9 then 80x10", "x"] {
            assert_eq!(route(text), Route::Agent, "{text}");
        }
        assert_eq!(route("/new"), Route::New);
        assert_eq!(route("/status@gymclaw_bot"), Route::Status);
    }
}
