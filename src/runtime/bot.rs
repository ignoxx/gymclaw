//! Inbound routing for the owner DM.
//!
//! - `gc:` taps and typed sets go to the coach on the fast lane (no model, in arrival order).
//! - `btn:<id>` taps, `handled: false` sets and everything else go to the agent. Turns are serialized;
//!   messages that arrive during a turn are collected into the next one.
//! - `/status` answers without the model, `/extensions` lists and turns off what the agent built (no
//!   model). There is no `/new`: the window is the last messages, always current.
//! - Voice notes are transcribed, then routed exactly like typed text.
//! - Reactions on the bot's messages are outcome labels in the turn log; nothing else happens.
//! - Coach steps taken in the Mini App (`coach::APP_VIEW` events) are shown in the chat like taps.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio::task::JoinHandle;

use super::{blocking, in_tx};
use crate::agent::turnlog::{TurnLog, scrub};
use crate::agent::{Agent, Outcome, TurnInput, media};
use crate::db::models::{AgentButton, AgentEvent, ChatMessage, ChatRole, NotificationJob};
use crate::domain::coach;
use crate::domain::set_parser::looks_like_set;
use crate::domain::view::Button;
use crate::error::Result;
use crate::telegram::coach::{Coach, Tapped};
use crate::telegram::{Audio, CallbackQuery, Media, Message, MessageReaction, Telegram, Update};
use crate::time::Ts;

const ACK: &str = "👀";
/// Longest quote of a replied-to message the agent gets (chars); finish cards are long.
const QUOTED_CHARS: usize = 1500;
const SESSION: &str = "telegram:owner";
/// Sent by the bot right before the reply of the turn that finishes setup; the agent's reply then
/// only proposes the first session (`onboarding::step_and_instruction`).
const WELCOME: &str = "**What you can do now**\n\
    • **Just talk to me**, by text or voice note: move sessions, swap exercises, change anything. I remember your preferences and adapt.\n\
    • **Gym** button (bottom-left): your plan, progress charts, live workout tracker and calendar export.\n\
    • **At the gym:** tap the card buttons or type sets (`10x80`). Send a scale photo or a Strong/Hevy CSV anytime; `/help` for the rest.";
/// How often the chat picks up coach steps taken in the Mini App; a log there shows here within this.
const APP_POLL: Duration = Duration::from_millis(700);
/// A Mini App step older than this when the bot gets to it (it was down) only updates the DB: its cards
/// would be out of date by now.
const APP_STALE: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);
/// Quiet time after an album photo before the agent takes its turn: Telegram sends an album's photos as
/// separate messages about a second apart, and answering the first got two contradicting replies.
const ALBUM_WAIT: Duration = Duration::from_millis(1500);

#[derive(Debug, PartialEq)]
pub enum Route {
    Start,
    Help,
    Status,
    Extensions,
    /// Try the coach first.
    Set,
    Agent,
}

/// The "/" menu Telegram shows; `route` handles each. Set on startup.
pub const COMMANDS: &[(&str, &str)] = &[
    ("status", "Bot health and what's running"),
    ("help", "What gymclaw can do and how to use it"),
    ("extensions", "What the agent built; turn things off"),
];

pub fn route(text: &str) -> Route {
    let command = text.split_whitespace().next().unwrap_or("").split('@').next().unwrap_or("");
    match command {
        "/start" => Route::Start,
        "/help" => Route::Help,
        "/status" => Route::Status,
        "/extensions" => Route::Extensions,
        // Cheap pre-check so ordinary chat never pays for a coach lookup. Reps alone ("13 reps") only
        // log on a bodyweight exercise; otherwise the coach returns unhandled and the agent gets it.
        _ if looks_like_set(text) => Route::Set,
        _ => Route::Agent,
    }
}

/// An owner message waiting for the agent.
#[derive(Clone, Debug)]
struct Inbound {
    /// Message that gets 👀 and, if the reply is a reaction, the reaction. None: nothing to react to.
    message_id: Option<i64>,
    key: String,
    images: Vec<Media>,
    /// True for background event batches; false for owner messages and reactions.
    background: bool,
}

enum Fast {
    Tap(Box<CallbackQuery>),
    /// `reply`: the message it replies to, for the agent if the coach hands it over.
    Text {
        message_id: i64,
        text: String,
        reply: Option<Box<Message>>,
    },
}

struct AlbumGuard<'a>(&'a Bot);

impl<'a> AlbumGuard<'a> {
    fn new(bot: &'a Bot) -> Self {
        bot.album_inflight.fetch_add(1, Ordering::AcqRel);
        bot.hold_for_album();
        Self(bot)
    }
}

impl Drop for AlbumGuard<'_> {
    fn drop(&mut self) {
        self.0.album_inflight.fetch_sub(1, Ordering::AcqRel);
        self.0.hold_for_album();
    }
}

pub struct Bot {
    owner: i64,
    tg: Arc<dyn Telegram>,
    coach: Arc<Coach>,
    agent: Agent,
    log: TurnLog,
    /// Wakes the scheduler after anything that may have changed jobs or deliveries.
    wake: Arc<Notify>,
    status: Arc<std::sync::Mutex<super::Health>>,
    last_ignored: std::sync::Mutex<Option<Instant>>,
    inbox: Mutex<Vec<Inbound>>,
    inbox_ready: Notify,
    /// While an album is still arriving, the agent waits until then (`ALBUM_WAIT` after its latest photo).
    album_until: std::sync::Mutex<Option<tokio::time::Instant>>,
    album_inflight: AtomicUsize,
    /// Setup is known to be finished; it never un-finishes, so later turns skip the DB check.
    setup_done: AtomicBool,
    album_change: Notify,
    app_wake: Notify,
    fast: mpsc::UnboundedSender<Fast>,
    stop: watch::Sender<bool>,
    workers: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl Bot {
    /// Start the fast lane, agent worker, and Mini App view relay. Call `shutdown` before dropping
    /// the last `Arc<Bot>` so the workers exit cleanly instead of holding strong references forever.
    pub fn start(
        owner: i64,
        tg: Arc<dyn Telegram>,
        coach: Arc<Coach>,
        agent: Agent,
        log: TurnLog,
        wake: Arc<Notify>,
        status: Arc<std::sync::Mutex<super::Health>>,
    ) -> Arc<Bot> {
        let (fast, mut lane) = mpsc::unbounded_channel();
        let (stop, mut stop_lane) = watch::channel(false);
        let stop_agent = stop.subscribe();
        let stop_views = stop.subscribe();
        let bot = Arc::new(Bot {
            owner,
            tg,
            coach,
            agent,
            log,
            wake,
            status,
            last_ignored: std::sync::Mutex::new(None),
            inbox: Mutex::new(vec![]),
            inbox_ready: Notify::new(),
            album_until: std::sync::Mutex::new(None),
            album_inflight: AtomicUsize::new(0),
            setup_done: AtomicBool::new(false),
            album_change: Notify::new(),
            app_wake: Notify::new(),
            fast,
            stop,
            workers: std::sync::Mutex::new(Vec::new()),
        });
        let worker = bot.clone();
        let h_lane = tokio::spawn(async move {
            loop {
                if *stop_lane.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = stop_lane.changed() => break,
                    job = lane.recv() => {
                        let Some(job) = job else { break };
                        match job {
                            Fast::Tap(q) => worker.tap(*q).await,
                            Fast::Text { message_id, text, reply } => worker.set_text(message_id, text, reply).await,
                        }
                    }
                }
            }
        });
        let worker = bot.clone();
        let h_agent = tokio::spawn(async move { worker.agent_loop(stop_agent).await });
        let worker = bot.clone();
        let h_views = tokio::spawn(async move { worker.app_views(stop_views).await });
        *bot.workers.lock().unwrap_or_else(PoisonError::into_inner) = vec![h_lane, h_agent, h_views];
        bot
    }

    /// Wake the Mini App view relay immediately instead of waiting for the next `APP_POLL` tick.
    pub fn notify_app_views(&self) {
        self.app_wake.notify_one();
    }

    /// Signal all background workers to stop and await their exit, releasing their `Arc<Bot>` refs.
    /// Idempotent: calling again after shutdown returns immediately.
    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
        let handles = std::mem::take(&mut *self.workers.lock().unwrap_or_else(PoisonError::into_inner));
        for handle in handles {
            let _ = handle.await;
        }
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
        if update.dm_user_id() != Some(self.owner) {
            if let Some(m) = update.message.as_ref()
                && m.chat.kind == "private"
                && let Some(from) = m.from.as_ref()
            {
                let mut last = self.last_ignored.lock().unwrap_or_else(PoisonError::into_inner);
                if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                    *last = Some(Instant::now());
                    tracing::warn!("ignoring DM from non-owner Telegram user id {}", from.id);
                }
            }
            return;
        }
        if let Some(q) = update.callback_query {
            self.callback(q).await;
        } else if let Some(reaction) = update.message_reaction {
            self.reaction(reaction).await;
        } else if let Some(message) = update.message {
            self.message(message).await;
        }
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
        let message_id = q.message.as_ref().map(|m| m.message_id);
        if let Some(message_id) = message_id {
            let _ = self.tg.edit_buttons(self.owner, message_id, &vec![]).await;
        }
        self.to_agent(
            message_id,
            format!("tg-cb:{}", q.id),
            button.value,
            vec![],
            json!({"kind": "button", "label": button.label}),
        )
        .await;
    }

    async fn message(&self, message: Message) {
        // Hold the agent loop while any album photo is in flight (downloading/saving), then for
        // ALBUM_WAIT after it lands, so slow downloads never split an album across turns.
        let _album_guard = message.media_group_id.is_some().then(|| AlbumGuard::new(self));
        let voice = message.voice.as_ref().or(message.audio.as_ref());
        let text = match voice {
            Some(audio) => match self.transcribe(message.message_id, audio).await {
                Some(text) => text,
                None => return,
            },
            None => message.body().unwrap_or("").to_string(),
        };
        let images = self.media(&message).await;
        let export = self.export(&message).await;
        if images.is_empty() && export.is_none() {
            match route(&text) {
                Route::Start => return self.send_start().await,
                Route::Help => return self.send_help().await,
                Route::Status => return self.send_status().await,
                Route::Extensions => return self.extensions(&text).await,
                Route::Set => {
                    // A typed set stays on the fast lane even as a reply; only a fallback needs the context.
                    let reply = message.reply_to_message;
                    let _ = self.fast.send(Fast::Text { message_id: message.message_id, text, reply });
                    return;
                }
                Route::Agent => {}
            }
        }
        let mut text = text;
        for image in &images {
            text.push_str(&format!("\n[photo saved at {}]", image.path.display()));
        }
        if let Some(export) = &export {
            text.push('\n');
            text.push_str(export);
        } else if message.photo.is_none() && message.document.is_some() && images.is_empty() {
            text.push_str("\n[a file that isn't an image; it wasn't saved]");
        }
        let meta = if voice.is_some() { json!({"kind": "voice"}) } else { json!({}) };
        let text = self.with_reply(text, message.reply_to_message.as_deref()).await;
        let key = format!("tg-{}", message.message_id);
        self.to_agent(Some(message.message_id), key, text.trim().to_string(), images, meta).await;
    }

    fn hold_for_album(&self) {
        *self.album_until.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(tokio::time::Instant::now() + ALBUM_WAIT);
        self.album_change.notify_one();
    }

    /// Wait until no album photo is downloading and `ALBUM_WAIT` has passed since the last one, so
    /// the whole album lands in one turn. Returns `false` if shutdown was signalled.
    async fn album_settled(&self, stop: &mut watch::Receiver<bool>) -> bool {
        loop {
            if *stop.borrow() {
                return false;
            }
            if self.album_inflight.load(Ordering::Acquire) > 0 {
                tokio::select! {
                    biased;
                    _ = stop.changed() => return false,
                    _ = self.album_change.notified() => continue,
                }
            }
            let until = *self.album_until.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(until) = until.filter(|u| *u > tokio::time::Instant::now()) else {
                return true;
            };
            tokio::select! {
                biased;
                _ = stop.changed() => return false,
                _ = self.album_change.notified() => continue,
                _ = tokio::time::sleep_until(until) => {}
            }
        }
    }

    /// Transcribe a voice note. Not echoed to the chat: the owner's message row holds the transcript for
    /// the agent, and the coach's 👍 or the agent's reply shows a mis-hear. None (after telling the
    /// owner) when it can't be read.
    async fn transcribe(&self, message_id: i64, audio: &Audio) -> Option<String> {
        let started = Instant::now();
        let _ = self.tg.react(self.owner, message_id, Some(ACK)).await;
        let model = &self.agent.audio_model;
        let heard = match crate::agent::openrouter::audio_format(audio.mime_type.as_deref()) {
            None => Err(crate::error::Error::domain("AUDIO_FORMAT", "not an audio format models take")),
            Some(format) => match self.tg.download(&audio.file_id).await {
                Ok(bytes) => crate::agent::openrouter::transcribe(&*self.agent.model, model, &bytes, format).await,
                Err(e) => Err(e),
            },
        };
        self.log.log(json!({"ev": "voice", "msg": message_id, "model": model, "seconds": audio.duration,
            "ms": started.elapsed().as_millis() as u64, "ok": heard.is_ok(),
            "error": heard.as_ref().err().map(|e| scrub(&e.to_string(), 500)),
            "text": heard.as_ref().ok().map(|t| scrub(t, 4000))}));
        match heard {
            // 👀 stays: the coach's 👍 or the agent's reply replaces it.
            Ok(text) => Some(text),
            Err(e) => {
                let _ = self.tg.react(self.owner, message_id, None).await;
                tracing::warn!("voice note not transcribed: {} {e}", e.code());
                self.send_bot(
                    &format!("🎙 Couldn't make out that voice note ({}). Could you type it?", e.code()),
                    "voice_failed",
                )
                .await;
                None
            }
        }
    }

    /// The owner's reaction on one of the bot's messages labels the turn that sent it (👍 good,
    /// 👎 bad) in the turn log, for finding bad turns offline. A 👎 on the latest agent reply becomes
    /// an owner message so the agent responds immediately (not subject to daily limits or quiet-wait).
    async fn reaction(&self, reaction: MessageReaction) {
        let message_id = reaction.message_id;
        let sent = blocking(self.db(), move |db| {
            ChatMessage::first(db, "WHERE telegram_message_id = ?1 AND role != 'owner' ORDER BY id DESC", [message_id])
        })
        .await;
        let Ok(Some(sent)) = sent else { return };
        let emoji: Vec<String> = reaction.new_reaction.into_iter().filter_map(|r| r.emoji).collect();
        let label = if emoji.iter().any(|e| e == "👎") {
            Some("bad")
        } else if emoji.iter().any(|e| e == "👍") {
            Some("good")
        } else {
            None
        };
        self.log.log(json!({"ev": "label", "source": "reaction", "run": sent.meta_json.get("run"),
            "kind": sent.meta_json.get("kind"), "msg": message_id, "emoji": emoji, "label": label}));
        if label == Some("bad") && sent.role == ChatRole::Assistant {
            // Only the latest agent reply triggers a response; rating older messages stays silent.
            let is_latest = blocking(self.db(), move |db| {
                let latest = ChatMessage::first(db, "WHERE role = 'assistant' ORDER BY id DESC", []);
                Ok::<bool, crate::error::Error>(latest.ok().flatten().is_some_and(|m| m.id == sent.id))
            })
            .await
            .unwrap_or(false);
            if is_latest {
                let reply_preview = if sent.text.chars().count() > 80 {
                    format!("{}…", sent.text.chars().take(80).collect::<String>())
                } else {
                    sent.text.clone()
                };
                self.to_agent(
                    None,
                    format!("tg-react:{message_id}-{}", Ts::now().0.as_millisecond()),
                    format!("[reacted 👎 to: \"{reply_preview}\"; find what was wrong, fix it if you can, and save a learned note so it won't repeat]"),
                    vec![],
                    json!({"kind": "reaction", "emoji": "👎", "target_msg": message_id}),
                )
                .await;
            }
        }
    }

    /// The owner's text plus what it replies to, as a bracketed line like the other context: the quoted
    /// text and, for a coach card, the workout and sets behind it (`coach::card_context`). The agent
    /// otherwise can't see what "that set" points at (2026-11-16: "dont u see the message i replied to?").
    async fn with_reply(&self, text: String, quoted: Option<&Message>) -> String {
        let Some(quoted) = quoted else { return text };
        let id = quoted.message_id;
        let context = blocking(self.db(), move |db| coach::card_context(db, id)).await.unwrap_or_else(|e| {
            tracing::warn!("reply context lookup failed: {e}");
            None
        });
        let who = if quoted.from.as_ref().is_some_and(|f| f.id == self.owner) { "their own" } else { "a bot" };
        let body = quoted.body().map(|b| b.replace('\n', " / ")).unwrap_or_default();
        let body = match body.char_indices().nth(QUOTED_CHARS) {
            Some((end, _)) => format!("{}…", &body[..end]),
            None if body.is_empty() && quoted.photo.is_some() => "(a photo)".into(),
            None => body,
        };
        let context = context.map(|c| format!("; it is {c}")).unwrap_or_default();
        format!("{text}\n[replying to {who} message: \"{body}\"{context}]").trim().to_string()
    }

    /// Download the largest photo size or an image document.
    async fn media(&self, message: &Message) -> Vec<Media> {
        let file = match (&message.photo, &message.document) {
            (Some(sizes), _) => {
                sizes.iter().max_by_key(|s| s.width).map(|s| (s.file_id.clone(), s.file_unique_id.clone()))
            }
            (None, Some(doc)) if !media::is_csv(doc) => Some((doc.file_id.clone(), doc.file_unique_id.clone())),
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

    /// A CSV document (another app's workout export): saved under `data/imports/`, handed to the agent
    /// as its path plus a summary to import from (`import::summary`).
    async fn export(&self, message: &Message) -> Option<String> {
        let doc = message.document.as_ref().filter(|d| media::is_csv(d))?;
        let saved = match self.tg.download(&doc.file_id).await {
            Ok(bytes) => media::save_import(&self.agent.data_dir, &doc.file_unique_id, &bytes).await,
            Err(e) => Err(e),
        };
        let path = match saved {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!("saving CSV failed: {e}");
                return Some("[a CSV file that couldn't be saved]".into());
            }
        };
        let file = path.clone();
        let summary = blocking(self.db(), move |db| crate::domain::import::summary(db, &file)).await;
        Some(match summary {
            Ok(summary) => format!("[CSV saved at {}]\n{summary}", path.display()),
            Err(e) => format!("[CSV saved at {}; {e}]", path.display()),
        })
    }

    /// Queue an owner message for the agent: 👀, stored in the chat history, collected into the next turn.
    async fn to_agent(&self, message_id: Option<i64>, key: String, text: String, images: Vec<Media>, meta: Value) {
        if let Some(message_id) = message_id {
            let _ = self.tg.react(self.owner, message_id, Some(ACK)).await;
        }
        self.log.log(
            json!({"ev": "inbound", "session": SESSION, "channel": "telegram", "msg": message_id.map(|m| m.to_string()),
            "sentAt": Ts::now().0.as_millisecond(), "chars": text.chars().count(), "text": scrub(&text, 4000)}),
        );
        self.remember(ChatRole::Owner, text, message_id, meta).await;
        self.inbox.lock().await.push(Inbound { message_id, key, images, background: false });
        self.inbox_ready.notify_one();
    }

    async fn send_start(&self) {
        let Ok(st) = blocking(self.db(), |db| crate::domain::onboarding::status(db)).await else { return };
        if st["ready"] == true {
            self.send_bot("You're already set up 💪 Message me anytime, or send `/help` for tips.", "start").await;
            return;
        }
        let total = crate::domain::onboarding::QUESTIONS.len();
        let missing = st["missing"].as_array().map_or(0, Vec::len);
        let next = st["next_question"].as_str();
        let instruction = st["instruction"].as_str().unwrap_or("");
        let fresh = st["answers"].as_object().is_none_or(|m| m.is_empty());
        let text = match st["step"].as_str() {
            Some("INTERVIEW") if fresh => format!(
                "👋 **Hey, I'm gymclaw**, your gym buddy in Telegram. I plan your week around your life and coach you through every set.\n\n\
                 {total} quick questions and we're rolling. First: **what are you training for?**\n\
                 Tap a button, type it, or send a voice note 🎙️, whatever's easiest; mixing goals is fine (\"lose fat, build muscle, better endurance\")."
            ),
            Some("INTERVIEW") => format!("**{}/{total}** {instruction}", total.saturating_sub(missing) + 1),
            Some("PLAN") => {
                "Almost done! Do you already follow a workout plan (send screenshots, a Strong/Hevy CSV, or type it out), or want me to build one for you?".into()
            }
            Some("REVIEW") => {
                "Your plan is ready to review — reply **yes** to finish setup, or tell me what you'd like to change.".into()
            }
            _ => instruction.to_string(),
        };
        let rows: Vec<(String, String, String)> = if next == Some("goal") {
            ["Build muscle", "Get stronger", "Lose fat", "General fitness"]
                .iter()
                .map(|label| (crate::db::new_id()[..8].to_string(), (*label).into(), (*label).into()))
                .collect()
        } else {
            vec![]
        };
        // Two per row: four stacked full-width goals filled the screen.
        let keyboard: Vec<Vec<Button>> = rows
            .chunks(2)
            .map(|pair| {
                pair.iter().map(|(id, label, _)| Button { text: label.clone(), data: format!("btn:{id}") }).collect()
            })
            .collect();
        if let Ok(id) = self.tg.send(self.owner, &text, None, &keyboard, None).await {
            if !rows.is_empty() {
                let _ = blocking(self.db(), move |db| {
                    for (id, label, value) in rows {
                        AgentButton { id, label, value, created_at: Ts::now() }.insert(db)?;
                    }
                    Ok(())
                })
                .await;
            }
            // Recorded as assistant so the agent sees the question it asked when the owner answers.
            self.remember(ChatRole::Assistant, text, Some(id), json!({"kind": "start"})).await;
        }
    }

    async fn send_help(&self) {
        let text = [
            "**gymclaw tips**",
            "• **During a workout:** tap the card button, type a set (`10x80`, `135lb x 8`, `25 min`), or send a voice note.",
            "• **Fix a set:** reply to any exercise card with the right numbers, or tap Fix in the **Gym** app.",
            "• **Body weight:** send a photo of your scale (as a file for old photos to keep the date).",
            "• **History:** send a Strong or Hevy CSV export anytime to import past workouts.",
            "• **Feedback:** react 👎 on my last reply if I got something wrong and I'll fix it.",
            "• **Commands:** `/status` (health), `/extensions` (toggle playbooks/triggers), `/help`.",
        ]
        .join("\n");
        self.send_bot(&text, "help").await;
    }

    async fn send_status(&self) {
        let health = self.status.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let model = self.agent.chat_model.clone();
        let text = blocking(self.db(), move |db| status_text(db, &health, &model))
            .await
            .unwrap_or_else(|e| format!("⚠️ Status unavailable: {}", e.code()));
        self.send_bot(&text, "status").await;
    }

    /// `/extensions [off|on <id>]`: the owner's view of and switch for what the agent built.
    async fn extensions(&self, text: &str) {
        let args = text.split_once(char::is_whitespace).map_or("", |(_, rest)| rest).to_string();
        let reply = in_tx(self.db(), move |db| crate::domain::extensions::owner_switch(db, &args, Ts::now()))
            .await
            .unwrap_or_else(|e| format!("⚠️ {e}"));
        self.send_bot(&reply, "extensions").await;
    }

    /// A message from the bot itself (status, extensions), stored as `bot` in the history.
    async fn send_bot(&self, text: &str, kind: &'static str) {
        let (sent, err) = self.send_chunks(text, &[]).await;
        for (id, chunk) in sent {
            self.remember(ChatRole::Bot, chunk, Some(id), json!({"kind": kind})).await;
        }
        if let Some(e) = err {
            tracing::warn!("send failed: {e}");
        }
    }

    async fn remember(&self, role: ChatRole, text: String, message_id: Option<i64>, meta: Value) {
        let saved =
            blocking(self.db(), move |db| ChatMessage::new(Ts::now(), role, text, message_id, meta).insert(db)).await;
        if let Err(e) = saved {
            tracing::warn!("chat history write failed: {e}");
        }
    }

    async fn tap(&self, q: CallbackQuery) {
        let started = Instant::now();
        let data = q.data.clone().unwrap_or_default();
        let action = data.split(':').nth(1).unwrap_or("").to_string();
        let message_id = q.message.as_ref().map(|m| m.message_id);
        if let Some(id) = message_id
            && self.coach.consumed(id).await
        {
            let _ = self.tg.answer_callback(&q.id, Some("Already done.")).await;
            self.log.log(json!({"ev": "fast", "flow": "tap", "action": action, "msg": message_id,
                "dup": true, "ok": true, "ms": started.elapsed().as_millis() as u64}));
            return;
        }
        let _ = self.tg.answer_callback(&q.id, None).await;
        let request_id = format!("tg-cb:{}", q.id);
        let result = in_tx(self.db(), move |db| coach::handle_tap(db, &data, Ts::now(), &request_id)).await;
        let cli_ms = started.elapsed().as_millis() as u64;
        let error = match result {
            Ok(view)
                if !view.handled
                    || matches!(view.ack.as_deref(), Some("⌛ No workout running." | "⌛ Old button.")) =>
            {
                self.to_agent(message_id, format!("fallback:{}", q.id),
                    format!("Coach button {} could not apply: {}. Card: {}. Inspect current state and resolve the owner's intent; never replay an old set button as a new log.", q.data.as_deref().unwrap_or(""), view.ack.as_deref().unwrap_or("unhandled"), q.message.as_ref().and_then(|m| m.body()).unwrap_or("")),
                    vec![], json!({"kind": "coach_fallback"})).await;
                Some("COACH_UNHANDLED".into())
            }
            Ok(view) => {
                let tapped = Tapped {
                    message_id: message_id.unwrap_or_default(),
                    text: q.message.as_ref().and_then(|m| m.body().map(String::from)),
                };
                if let Err(e) = self.coach.apply(&view, Some(&tapped), None).await {
                    tracing::warn!("coach render failed: {e}");
                    self.to_agent(message_id, format!("render:{}", q.id),
                        format!("Coach button {action} was saved successfully, but rendering failed ({}): {e}. Do not replay it; inspect workout status and resend the card. Card: {}", e.code(), tapped.text.as_deref().unwrap_or("")),
                        vec![], json!({"kind": "coach_fallback"})).await;
                }
                self.wake.notify_one();
                None
            }
            Err(e) => {
                tracing::warn!("coach tap: {} {e}", e.code());
                self.to_agent(message_id, format!("fallback:{}", q.id),
                    format!("Coach button {action} could not be handled ({}): {e}. Card: {}. Resolve the owner's intent with the available tools.", e.code(), q.message.as_ref().and_then(|m| m.body()).unwrap_or("")),
                    vec![], json!({"kind": "coach_fallback"})).await;
                Some(e.code().to_string())
            }
        };
        self.log.log(json!({"ev": "fast", "flow": "tap", "action": action, "msg": message_id, "cliMs": cli_ms,
            "ok": error.is_none(), "error": error, "ms": started.elapsed().as_millis() as u64}));
    }

    async fn set_text(&self, message_id: i64, text: String, reply: Option<Box<Message>>) {
        let started = Instant::now();
        let request_id = format!("tg-msg:{message_id}");
        let input = text.clone();
        let result = in_tx(self.db(), move |db| coach::handle_text(db, &input, Ts::now(), &request_id)).await;
        let cli_ms = started.elapsed().as_millis() as u64;
        let mut entry =
            json!({"ev": "fast", "flow": "text", "text": text, "msg": message_id.to_string(), "cliMs": cli_ms});
        match result {
            Ok(view) if view.handled => {
                if let Err(e) = self.coach.apply(&view, None, Some(message_id)).await {
                    tracing::warn!("coach render failed: {e}");
                    self.to_agent(Some(message_id), format!("render:tg-{message_id}"),
                        format!("Typed set {text} was saved successfully, but rendering failed ({}): {e}. Do not log it again; inspect workout status and resend the card.", e.code()),
                        vec![], json!({"kind": "coach_fallback"})).await;
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
                let text = match &other {
                    Err(e) => format!("{text}\n[Coach fallback: {}: {e}]", e.code()),
                    _ => text,
                };
                let text = self.with_reply(text, reply.as_deref()).await;
                let key = format!("tg-{message_id}");
                self.to_agent(Some(message_id), key, text, vec![], json!({"kind": "coach_fallback"})).await;
            }
        }
        entry["ms"] = json!(started.elapsed().as_millis() as u64);
        self.log.log(entry);
    }

    /// Coach steps taken in the Mini App, shown in the chat as if tapped there: the old card is retired
    /// with the ack, the next card (or rest countdown) is sent. The Mini App runs in its own process
    /// without Telegram access, so it leaves each step as an `APP_VIEW` event; this claims and renders them.
    async fn app_views(&self, mut stop: watch::Receiver<bool>) {
        let mut timer = tokio::time::interval(APP_POLL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                _ = self.app_wake.notified() => {}
                _ = timer.tick() => {}
            }
            if *stop.borrow() {
                break;
            }
            // A plain read first: the write lock is taken only when there is a step to claim.
            // In the order they were written: a fix note, then the card redrawn from it.
            const PENDING: &str = "WHERE type = ?1 AND handled_at IS NULL ORDER BY rowid";
            let waiting =
                blocking(self.db(), |db| Ok(AgentEvent::first(db, PENDING, [coach::APP_VIEW])?.is_some())).await;
            if !matches!(waiting, Ok(true)) {
                continue;
            }
            let claimed = in_tx(self.db(), |db| {
                let now = Ts::now();
                let events = AgentEvent::select(db, PENDING, [coach::APP_VIEW])?;
                for event in &events {
                    AgentEvent { handled_at: Some(now), ..event.clone() }.update(db)?;
                }
                Ok(events)
            })
            .await;
            let events = match claimed {
                Ok(events) => events,
                Err(e) => {
                    tracing::warn!("app coach steps: {} {e}", e.code());
                    continue;
                }
            };
            for event in events {
                let Ok(view) = serde_json::from_value::<crate::domain::view::CoachView>(event.payload_json.0.clone())
                else {
                    tracing::warn!("app coach step {} unreadable", event.id);
                    continue;
                };
                if Ts::now() - event.created_at > APP_STALE {
                    continue;
                }
                if let Err(e) = self.coach.apply(&view, None, None).await {
                    tracing::warn!("app coach step render failed: {e}");
                }
                // The agent reads the chat history: a set logged in the app belongs there like a typed one.
                if let Some(ack) = view.ack.clone() {
                    self.remember(
                        ChatRole::Owner,
                        format!("{ack} (in the app)"),
                        None,
                        json!({"kind": "set", "source": "app"}),
                    )
                    .await;
                }
                self.wake.notify_one();
            }
        }
    }

    async fn agent_loop(&self, mut stop: watch::Receiver<bool>) {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                _ = self.inbox_ready.notified() => {}
                _ = timer.tick() => {}
            }
            if *stop.borrow() {
                break;
            }
            loop {
                if !self.album_settled(&mut stop).await {
                    break;
                }
                let batch = std::mem::take(&mut *self.inbox.lock().await);
                if batch.is_empty() {
                    break;
                }
                self.turn(batch, false).await;
            }
            if *stop.borrow() {
                break;
            }
            if let Err(e) = self.event_turn().await {
                tracing::warn!("agent event wake: {} {e}", e.code());
            }
        }
    }

    /// Background decisions use the same serialized worker as owner chat. Events survive restarts;
    /// failures remain pending for the next tick, successes acknowledge exactly the supplied events.
    ///
    /// One voice: while the owner is mid-conversation no background turn runs. Pending events show in
    /// the next owner turn's context as open issues and that turn settles them (see `turn`); only
    /// what is still unhandled once the chat goes quiet wakes the agent here. During a workout only
    /// workout-time events wake it (`events::wake_batch`); the rest wait until it ends.
    async fn event_turn(&self) -> Result<()> {
        let batch = in_tx(self.db(), |db| {
            if !super::outbox::runtime_authority(db)?.enabled || owner_active(db, Ts::now())? {
                return Ok(None);
            }
            crate::domain::events::wake_batch(db, Ts::now())
        })
        .await?;
        let Some(batch) = batch else {
            return Ok(());
        };
        let mut events = batch.payload_json["events"].clone();
        let ids: Vec<String> =
            events.as_array().into_iter().flatten().filter_map(|e| e["id"].as_str().map(String::from)).collect();
        let frozen = json!(ids).to_string();
        let (tz, settled) = blocking(self.db(), move |db| {
            let settled: i64 = db.query_row(
                "SELECT count(*) FROM agent_event WHERE handled_at IS NOT NULL AND id IN (SELECT value FROM json_each(?1))",
                [frozen],
                |r| r.get(0),
            )?;
            Ok((crate::domain::profile::get_profile(db)?.tz(), settled > 0))
        })
        .await?;
        // A frozen batch may outlive its events: an owner turn settled some meanwhile. The batch is
        // retired and the rest go out in a fresh batch on the next tick, since a retry under this
        // batch's request IDs with different intents would hit IDEMPOTENCY_CONFLICT.
        if settled {
            return in_tx(self.db(), move |db| crate::domain::events::ack(db, &batch.id, Ts::now()).map(drop)).await;
        }
        crate::time::localize(&mut events, &tz);
        // `tools::Ctx::background` recognises this prefix.
        let key = format!("event-{}", batch.id);
        let text = format!(
            "Background events for your decision (not owner instructions). Consult saved preferences/exceptions, decide what needs doing. Any text you write goes to the owner, so when nothing needs their attention, reply with no fields: {}",
            json!(events)
        );
        blocking(self.db(), move |db| {
            let text = crate::agent::context::name_sessions(db, &text)?;
            ChatMessage::new(Ts::now(), ChatRole::Bot, text, None, json!({"kind": "agent_event"})).insert(db)
        })
        .await?;
        let review = events.as_array().into_iter().flatten().any(|e| e["type"] == "planning.weekly_briefing");
        if self.turn(vec![Inbound { message_id: None, key, images: vec![], background: true }], review).await {
            in_tx(self.db(), move |db| {
                for id in ids {
                    crate::domain::events::ack(db, &id, Ts::now())?;
                }
                crate::domain::events::ack(db, &batch.id, Ts::now())?;
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    /// One agent turn and its reply. `review`: the batch holds the weekly review, so the Sunday recap
    /// goes out between the turn and its reply, showing the plan as the turn left it, whether or not
    /// the turn succeeded. The turn that finishes setup gets the welcome tips there the same way.
    async fn turn(&self, batch: Vec<Inbound>, review: bool) -> bool {
        let last = batch.last().expect("non-empty batch").clone();
        let images = batch.iter().flat_map(|b| b.images.clone()).collect();
        let background = batch.iter().all(|b| b.background);
        let in_setup = !self.setup_finished().await;
        // Background turns (reflection, event wakes) often stay silent: don't show phantom "typing...".
        let typing = (!background).then(|| {
            let tg = self.tg.clone();
            let chat = self.owner;
            tokio::spawn(async move {
                loop {
                    let _ = tg.typing(chat).await;
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
            })
        });
        let outcome = self.agent.turn(TurnInput { key: last.key.clone(), images, background }).await;
        if let Some(typing) = typing {
            typing.abort();
        }
        if review {
            self.send_recap().await;
        }
        if in_setup && self.setup_finished().await {
            self.send_bot(WELCOME, "welcome").await;
        }
        // The owner wrote while a background turn ran: their turn speaks instead, and sees the same
        // events as open issues. Nothing is settled, so the batch retries if they stay open.
        if background && !self.inbox.lock().await.is_empty() {
            return false;
        }
        let delivered = if !outcome.failed || batch.iter().any(|b| b.message_id.is_some()) {
            self.answer(&outcome, &batch).await
        } else {
            false
        };
        // What the owner has now been told about never wakes a second turn that asks again.
        let settles = outcome.settles.clone();
        let settled = !outcome.failed
            && delivered
            && in_tx(self.db(), move |db| {
                for id in settles {
                    crate::domain::events::ack(db, &id, Ts::now())?;
                }
                Ok(())
            })
            .await
            .inspect_err(|e| tracing::warn!("seen events not settled: {e}"))
            .is_ok();
        self.wake.notify_one();
        settled
    }

    /// Whether onboarding is done; a read error counts as done so it never triggers the welcome.
    async fn setup_finished(&self) -> bool {
        if self.setup_done.load(Ordering::Relaxed) {
            return true;
        }
        let done = blocking(self.db(), |db| crate::domain::onboarding::finished(db)).await;
        if let Ok(true) = done {
            self.setup_done.store(true, Ordering::Relaxed);
        }
        done.unwrap_or(true)
    }

    /// Queue and send this week's recap (`weekly::release_recap`). A failed send stays in the outbox.
    async fn send_recap(&self) {
        let recipient = self.owner.to_string();
        let queued =
            in_tx(self.db(), move |db| crate::domain::weekly::release_recap(db, &recipient, Ts::now(), true)).await;
        let sent = match queued {
            Ok(Some(id)) => super::outbox::deliver(self.db(), self.tg.as_ref(), &id, Ts::now()).await.map(drop),
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            tracing::warn!("weekly recap: {} {e}", e.code());
        }
    }

    /// Send the turn's one outcome and clear the 👀 acks. False when the owner may not have got it.
    async fn answer(&self, outcome: &Outcome, batch: &[Inbound]) -> bool {
        let reply = &outcome.reply;
        let target = batch.iter().rev().find_map(|b| b.message_id);
        let mut reacted = None;
        let mut delivered = true;
        if let (Some(emoji), Some(message)) = (&reply.react, target) {
            match self.tg.react(self.owner, message, Some(emoji)).await {
                Ok(()) => reacted = Some(message),
                // Not every emoji is an allowed reaction: say it instead.
                Err(_) if reply.message.is_none() => delivered &= self.send_reply(outcome, emoji, None, &[]).await,
                Err(_) => {}
            }
        }
        if reply.message.is_none() && !outcome.actions.is_empty() {
            // Nothing sent, but state changed: later turns still need to know what this one did.
            let text = crate::agent::context::saved(outcome.actions.iter().map(String::as_str));
            let meta = json!({"kind": crate::agent::context::SILENT_ACTIONS, "run": outcome.run_id});
            self.remember(ChatRole::Bot, format!("Silent turn. {text}"), None, meta).await;
        }
        if let Some(message) = &reply.message {
            let buttons: Vec<(String, String)> =
                message.buttons.iter().map(|b| (b.label.clone(), b.value.clone())).collect();
            delivered &= self.send_reply(outcome, &message.text, message.photo.as_deref(), &buttons).await;
        }
        for message in batch.iter().filter_map(|b| b.message_id).filter(|m| Some(*m) != reacted) {
            let _ = self.tg.react(self.owner, message, None).await;
        }
        delivered
    }

    async fn send_reply(
        &self,
        outcome: &Outcome,
        text: &str,
        photo: Option<&std::path::Path>,
        buttons: &[(String, String)],
    ) -> bool {
        let rows: Vec<(String, String, String)> = buttons
            .iter()
            .map(|(label, value)| (crate::db::new_id()[..8].to_string(), label.clone(), value.clone()))
            .collect();
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
        let keyboard: Vec<Vec<Button>> =
            rows.iter().map(|(id, label, _)| vec![Button { text: label.clone(), data: format!("btn:{id}") }]).collect();
        let empty = vec![];
        let mut sent_parts: Vec<(i64, String)> = Vec::new();
        let err = if let Some(photo_path) = photo {
            if crate::telegram::utf16_len(text) <= crate::telegram::CAPTION_LIMIT {
                match self.tg.send(self.owner, text, Some(photo_path), &keyboard, None).await {
                    Ok(id) => {
                        sent_parts.push((id, text.to_string()));
                        None
                    }
                    Err(e) => Some(e),
                }
            } else {
                // Caption exceeds Telegram's 1024-unit limit: send the photo first, then chunked text.
                match self.tg.send(self.owner, "", Some(photo_path), &empty, None).await {
                    Ok(photo_id) => {
                        sent_parts.push((photo_id, "(photo)".into()));
                        let (chunks, err) = self.send_chunks(text, &keyboard).await;
                        sent_parts.extend(chunks);
                        err
                    }
                    Err(e) => Some(e),
                }
            }
        } else {
            let (chunks, err) = self.send_chunks(text, &keyboard).await;
            sent_parts.extend(chunks);
            err
        };
        let last_id = sent_parts.last().map(|(id, _)| *id);
        self.log.log(json!({"ev": "outbound", "session": SESSION, "run": outcome.run_id, "channel": "telegram", "msg": last_id, "photo": photo.is_some(),
            "ok": err.is_none(), "error": err.as_ref().map(|e| scrub(&e.to_string(), 500)), "text": scrub(text, 4000)}));
        // Record every message that actually went out so chat history and reaction lookups stay accurate,
        // even if a later chunk fails partway through.
        let last_part = sent_parts.len().saturating_sub(1);
        for (i, (id, part)) in sent_parts.iter().enumerate() {
            let meta = if i == last_part {
                json!({"kind": if outcome.failed { "failure" } else { "reply" }, "run": outcome.run_id, "buttons": buttons.iter().map(|b| &b.0).collect::<Vec<_>>(), "actions": outcome.actions})
            } else {
                json!({"kind": if outcome.failed { "failure" } else { "reply" }, "run": outcome.run_id})
            };
            self.remember(ChatRole::Assistant, part.clone(), Some(*id), meta).await;
        }
        if let Some(e) = err {
            tracing::warn!("reply not sent: {e}");
            // If at least one chunk reached the owner, treat as delivered so events aren't duplicated.
            !sent_parts.is_empty()
        } else {
            true
        }
    }

    /// Send `text` in Telegram-sized chunks, attaching `keyboard` to the final chunk.
    /// Returns every `(message_id, chunk_text)` that succeeded alongside the first error, if any.
    async fn send_chunks(
        &self,
        text: &str,
        keyboard: &[Vec<Button>],
    ) -> (Vec<(i64, String)>, Option<crate::error::Error>) {
        let chunks = crate::telegram::split_message(text, crate::telegram::TEXT_LIMIT);
        let last_idx = chunks.len().saturating_sub(1);
        let empty = vec![];
        let mut sent = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.into_iter().enumerate() {
            let kb = if i == last_idx { keyboard.to_vec() } else { empty.clone() };
            match self.tg.send(self.owner, chunk, None, &kb, None).await {
                Ok(id) => sent.push((id, chunk.to_string())),
                Err(e) => return (sent, Some(e)),
            }
        }
        (sent, None)
    }
}

/// How long after the owner's last message the chat counts as a live conversation.
const QUIET: jiff::SignedDuration = jiff::SignedDuration::from_mins(5);

/// Whether the owner wrote within `QUIET`: background events then wait for their next turn.
fn owner_active(db: &crate::db::Connection, now: Ts) -> Result<bool> {
    let last = ChatMessage::first(db, "WHERE role = 'owner' ORDER BY id DESC", [])?;
    Ok(last.is_some_and(|m| now - m.at < QUIET))
}

/// `/status`: runtime health in a few lines, no model.
fn status_text(db: &crate::db::Connection, health: &super::Health, model: &str) -> Result<String> {
    let authority = super::outbox::runtime_authority(db)?;
    let tz = crate::domain::profile::get_profile(db)?.tz();
    let count = |s: &str| -> Result<i64> {
        Ok(db.query_row("SELECT count(*) FROM notification_delivery WHERE status = ?1", [s], |r| r.get(0))?)
    };
    let on = |b: bool| if b { "on" } else { "off" };
    let mut lines = vec![
        format!("**gymclaw** · up {}", super::uptime(health.started.elapsed())),
        format!("Runtime {} · crowd polling {}", authority.state(), on(authority.crowd_polling_enabled)),
        format!(
            "Deliveries: {} pending · {} sending · {} unknown",
            count("PENDING")?,
            count("SENDING")?,
            count("UNKNOWN")?
        ),
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

    struct EventModel(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl crate::agent::ChatModel for EventModel {
        async fn complete(
            &self,
            _: &str,
            messages: &[Value],
            _: &[Value],
            _: bool,
        ) -> Result<crate::agent::ModelReply> {
            // Input to answer, not something the bot said.
            assert!(messages.iter().any(|m| m["role"] == "user"
                && m["content"].as_str().is_some_and(
                    |t| t.contains("Background events for your decision") && t.contains("planning.attention")
                )));
            let message = match self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => crate::bail!("MODEL_DOWN", "fixture model outage"),
                1 => {
                    json!({"tool_calls": [{"id": "create", "function": {"name": "session_create", "arguments": json!({"start": "2099-10-14T12:00", "template_id": "fixture"}).to_string()}}]})
                }
                _ => json!({"content": ""}),
            };
            Ok(crate::agent::ModelReply { message, ..Default::default() })
        }
    }

    /// A bot over a fresh DB with one template (`fixture`); runtime authority still off.
    fn fixture(
        model: Arc<dyn crate::agent::ChatModel>,
    ) -> (Bot, crate::db::Connection, std::path::PathBuf, Arc<crate::telegram::coach::tests::FakeTelegram>) {
        let (db_path, db) = crate::db::test::file();
        let dir = db_path.parent().unwrap().to_path_buf();
        let template = serde_json::from_value(json!({"id": "fixture", "name": "Fixture", "exercises": [{"id": "bench", "name": "Bench", "role": "press", "target_weight": 0}]})).unwrap();
        crate::domain::templates::import_template(&db, &template).unwrap();
        let tg = crate::telegram::coach::tests::FakeTelegram::new();
        let coach = Coach::new(tg.clone(), 42, Arc::new(Ts::now), None, None);
        let (fast, _) = mpsc::unbounded_channel();
        let bot = Bot {
            owner: 42,
            tg: tg.clone(),
            coach,
            log: TurnLog::default(),
            wake: Arc::new(Notify::new()),
            status: Arc::new(std::sync::Mutex::new(super::super::Health { started: Instant::now(), last_tick: None })),
            last_ignored: std::sync::Mutex::new(None),
            inbox: Mutex::new(vec![]),
            inbox_ready: Notify::new(),
            album_until: std::sync::Mutex::new(None),
            album_inflight: AtomicUsize::new(0),
            setup_done: AtomicBool::new(false),
            album_change: Notify::new(),
            app_wake: Notify::new(),
            fast,
            stop: watch::channel(false).0,
            workers: std::sync::Mutex::new(Vec::new()),
            agent: Agent {
                db_path,
                data_dir: dir.clone(),
                recipient: Some("1".into()),
                model,
                chat_model: "fixture".into(),
                vision_model: "fixture".into(),
                audio_model: "audio".into(),
                coach: None,
                log: TurnLog::default(),
                clock: Arc::new(Ts::now),
            },
        };
        (bot, db, dir, tg)
    }

    fn enable_runtime(db: &crate::db::Connection) {
        crate::db::test::authority().insert(db).unwrap();
    }

    #[tokio::test]
    async fn background_turn_skips_typing_indicator() {
        struct YieldingSilent;
        #[async_trait::async_trait]
        impl crate::agent::ChatModel for YieldingSilent {
            async fn complete(&self, _: &str, _: &[Value], _: &[Value], _: bool) -> Result<crate::agent::ModelReply> {
                // Yield so any spawned typing task gets polled before the turn returns.
                tokio::task::yield_now().await;
                Ok(crate::agent::ModelReply {
                    message: json!({"role": "assistant", "content": ""}),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                })
            }
        }
        let (bot, db, _, tg) = fixture(Arc::new(YieldingSilent));
        enable_runtime(&db);
        crate::domain::events::request(&db, "planning.attention", json!({"note": "quiet check"}), Ts::now()).unwrap();
        bot.event_turn().await.unwrap();
        assert!(!tg.calls().iter().any(|c| matches!(c, crate::telegram::coach::tests::Call::Typing)));

        // Owner-initiated turns still show the typing indicator.
        bot.turn(
            vec![Inbound { message_id: Some(1), key: "owner-1".into(), images: vec![], background: false }],
            false,
        )
        .await;
        assert!(tg.calls().iter().any(|c| matches!(c, crate::telegram::coach::tests::Call::Typing)));
    }

    #[tokio::test]
    async fn event_wake_obeys_authority_retries_failure_and_acknowledges_success() {
        let model = Arc::new(EventModel(std::sync::atomic::AtomicUsize::new(0)));
        let (bot, db, dir, tg) = fixture(model.clone());
        // Batch identity stays short enough for mutation request IDs regardless of batch size.
        for n in 0..6 {
            crate::domain::events::request(
                &db,
                "planning.attention",
                json!({"warnings": [format!("calendar clash {n}")]}),
                Ts::now(),
            )
            .unwrap();
        }
        bot.event_turn().await.unwrap();
        assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        enable_runtime(&db);
        bot.event_turn().await.unwrap();
        assert_eq!(crate::domain::events::wake_pending(&db).unwrap().len(), 6);
        assert!(!tg.calls().iter().any(|c| matches!(c, crate::telegram::coach::tests::Call::Send(..))));
        let batch = crate::domain::events::wake_batch(&db, Ts::now()).unwrap().unwrap();
        crate::domain::events::request(
            &db,
            "planning.attention",
            json!({"warnings": ["new event during failure"]}),
            Ts::now(),
        )
        .unwrap();
        assert_eq!(crate::domain::events::wake_batch(&db, Ts::now()).unwrap().unwrap().id, batch.id);
        // The retry answers the frozen batch; the newer event was in its context as an open issue, so
        // it is settled too instead of waking another turn.
        bot.event_turn().await.unwrap();
        assert!(crate::domain::events::wake_pending(&db).unwrap().is_empty());
        bot.event_turn().await.unwrap();
        assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 3);
        let sessions = crate::db::models::PlannedSession::select(&db, "", []).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].user_locked);
        // Stale coach buttons and unparsed sets must enter the same agent inbox with context.
        bot.tap(serde_json::from_value(json!({"id": "stale", "from": {"id": 42}, "data": "gc:next:missing", "message": {"message_id": 5, "chat": {"id": 42, "type": "private"}, "text": "Bench card"}})).unwrap()).await;
        bot.set_text(6, "10x40".into(), None).await;
        assert_eq!(bot.inbox.lock().await.len(), 2);
        let fallback =
            ChatMessage::select(&db, "WHERE meta_json ->> 'kind' = 'coach_fallback' ORDER BY id", []).unwrap();
        assert_eq!(fallback.len(), 2);
        assert!(fallback[0].text.contains("Bench card"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Answers every call with plain text and counts the calls.
    struct Chatty(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl crate::agent::ChatModel for Chatty {
        async fn complete(&self, _: &str, _: &[Value], _: &[Value], _: bool) -> Result<crate::agent::ModelReply> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::agent::ModelReply { message: json!({"content": "Which one drops?"}), ..Default::default() })
        }
    }

    /// The owner gets the recap after the review turn, then the agent's take; never the take first.
    #[tokio::test]
    async fn weekly_recap_goes_out_between_the_review_turn_and_its_take() {
        let model = Arc::new(Chatty(std::sync::atomic::AtomicUsize::new(0)));
        let (bot, db, dir, tg) = fixture(model.clone());
        enable_runtime(&db);
        crate::domain::weekly::weekly_plan(&db, "fixture", Ts::now()).unwrap();
        crate::domain::weekly::announce(&db, Ts::now()).unwrap();
        bot.event_turn().await.unwrap();
        let sent: Vec<String> = tg
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                crate::telegram::coach::tests::Call::Send(text, _) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0].starts_with("📅") && sent[1] == "Which one drops?", "{sent:?}");
        assert!(crate::domain::events::wake_pending(&db).unwrap().is_empty());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Finishes setup mid-turn (as `onboarding_finish` would), then replies.
    struct Finisher(std::sync::OnceLock<std::path::PathBuf>);

    #[async_trait::async_trait]
    impl crate::agent::ChatModel for Finisher {
        async fn complete(&self, _: &str, _: &[Value], _: &[Value], _: bool) -> Result<crate::agent::ModelReply> {
            crate::db::open(self.0.get().expect("db path set"))?.execute(
                "INSERT OR REPLACE INTO onboarding_state (id, completed_fingerprint, interview_json, split_json) VALUES (1, 'x', '{}', '[]')",
                [],
            )?;
            Ok(crate::agent::ModelReply {
                message: json!({"content": "First one Mon 9:00, works?"}),
                ..Default::default()
            })
        }
    }

    /// 2026-11-22: the wrap-up was left to the model, which skipped the tips. The bot sends them
    /// between the finishing turn and its reply, once; later turns get none.
    #[tokio::test]
    async fn finishing_setup_sends_tips_before_the_reply() {
        let model = Arc::new(Finisher(std::sync::OnceLock::new()));
        let (bot, db, dir, tg) = fixture(model.clone());
        model.0.set(dir.join("gymclaw.db")).unwrap();
        let texts = || -> Vec<String> {
            tg.calls()
                .into_iter()
                .filter_map(|c| match c {
                    crate::telegram::coach::tests::Call::Send(text, _) => Some(text),
                    _ => None,
                })
                .collect()
        };
        bot.to_agent(Some(7), "tg-7".into(), "looks good".into(), vec![], json!({})).await;
        let batch = std::mem::take(&mut *bot.inbox.lock().await);
        bot.turn(batch, false).await;
        assert_eq!(texts(), [WELCOME, "First one Mon 9:00, works?"]);
        bot.to_agent(Some(8), "tg-8".into(), "thanks".into(), vec![], json!({})).await;
        let batch = std::mem::take(&mut *bot.inbox.lock().await);
        bot.turn(batch, false).await;
        assert_eq!(texts().iter().filter(|t| *t == WELCOME).count(), 1);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// One voice: mid-conversation, a background event waits for the owner's next turn, which sees
    /// it as an open issue and settles it; nothing asks the owner a second time.
    #[tokio::test]
    async fn owner_turn_settles_open_issues_and_background_waits_for_quiet() {
        let model = Arc::new(Chatty(std::sync::atomic::AtomicUsize::new(0)));
        let (bot, db, dir, tg) = fixture(model.clone());
        enable_runtime(&db);
        let calls = || model.0.load(std::sync::atomic::Ordering::SeqCst);
        let attention = |text: &str| {
            crate::domain::events::request(&db, "planning.attention", json!({"warnings": [text]}), Ts::now()).unwrap()
        };

        // The owner just wrote: the event waits.
        bot.to_agent(Some(7), "tg-7".into(), "back to 3 a week".into(), vec![], json!({})).await;
        attention("Week 2026-11-02: 4 sessions saved, target 3");
        bot.event_turn().await.unwrap();
        assert_eq!(calls(), 0);

        // Their turn sees it and settles it; no background turn follows.
        let batch = std::mem::take(&mut *bot.inbox.lock().await);
        assert!(bot.turn(batch, false).await);
        assert_eq!(calls(), 1);
        assert!(crate::domain::events::wake_pending(&db).unwrap().is_empty());

        // Once the chat is quiet, a new event wakes the agent as before.
        db.execute("UPDATE chat_message SET at = '2026-01-01 00:00:00'", []).unwrap();
        attention("Session x: skips a rest day");
        bot.event_turn().await.unwrap();
        assert_eq!(calls(), 2);

        // The owner writes while a background turn thinks: it stays silent and settles nothing.
        attention("Session y: skips a rest day");
        bot.inbox.lock().await.push(Inbound {
            message_id: Some(8),
            key: "tg-8".into(),
            images: vec![],
            background: false,
        });
        let sent = |tg: &crate::telegram::coach::tests::FakeTelegram| {
            tg.calls().iter().filter(|c| matches!(c, crate::telegram::coach::tests::Call::Send(..))).count()
        };
        let before = sent(&tg);
        bot.event_turn().await.unwrap();
        assert_eq!((calls(), sent(&tg)), (3, before));
        assert_eq!(crate::domain::events::wake_pending(&db).unwrap().len(), 1);
        bot.inbox.lock().await.clear();

        // A frozen batch that lost an event to an owner turn is retired, not retried under its old
        // request IDs; the survivor goes out in a fresh batch.
        attention("Session z: skips a rest day");
        let frozen = crate::domain::events::wake_batch(&db, Ts::now()).unwrap().unwrap();
        let first = frozen.payload_json["events"][0]["id"].as_str().unwrap().to_string();
        crate::domain::events::ack(&db, &first, Ts::now()).unwrap();
        bot.event_turn().await.unwrap();
        assert_eq!(calls(), 3);
        let fresh = crate::domain::events::wake_batch(&db, Ts::now()).unwrap().unwrap();
        assert_ne!(fresh.id, frozen.id);
        assert_eq!(fresh.payload_json["events"].as_array().unwrap().len(), 1);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Hears every voice note as the same words and keeps the request it got.
    struct Ears(std::sync::Mutex<Vec<Value>>);

    #[async_trait::async_trait]
    impl crate::agent::ChatModel for Ears {
        async fn complete(
            &self,
            _: &str,
            messages: &[Value],
            _: &[Value],
            _: bool,
        ) -> Result<crate::agent::ModelReply> {
            *self.0.lock().unwrap() = messages.to_vec();
            Ok(crate::agent::ModelReply {
                message: json!({"content": " move tomorrow's workout to the evening\n"}),
                ..Default::default()
            })
        }
    }

    /// A voice note reaches the agent exactly like typed text, without an echo in the chat.
    #[tokio::test]
    async fn voice_note_is_handled_as_text() {
        let ears = Arc::new(Ears(std::sync::Mutex::new(vec![])));
        let (bot, db, dir, tg) = fixture(ears.clone());
        let update = json!({"update_id": 1, "message": {"message_id": 9, "from": {"id": 42}, "chat": {"id": 42, "type": "private"},
            "voice": {"file_id": "f", "file_unique_id": "u", "duration": 3, "mime_type": "audio/ogg"}}});
        bot.handle(serde_json::from_value(update).unwrap()).await;
        let sent = ears.0.lock().unwrap().clone();
        assert_eq!(sent[1]["content"][0]["input_audio"]["format"], "ogg");
        assert!(
            !tg.calls()
                .iter()
                .any(|c| matches!(c, crate::telegram::coach::tests::Call::Send(t, _) if t.starts_with("🎙")))
        );
        let owner = ChatMessage::first(&db, "WHERE role = 'owner'", []).unwrap().unwrap();
        assert_eq!(
            (owner.text.as_str(), owner.meta_json["kind"].as_str()),
            ("move tomorrow's workout to the evening", Some("voice"))
        );
        assert_eq!(bot.inbox.lock().await.len(), 1);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 👍/👎 on a reply labels the turn that sent it; reactions on the owner's own messages don't.
    #[tokio::test]
    async fn reaction_labels_the_turn_that_sent_the_message() {
        let (mut bot, db, dir, _) = fixture(Arc::new(Chatty(std::sync::atomic::AtomicUsize::new(0))));
        let turns = dir.join("turns");
        bot.log = TurnLog::new(Some(turns.clone()));
        for (role, id, meta) in
            [(ChatRole::Assistant, 20, json!({"kind": "reply", "run": "r1"})), (ChatRole::Owner, 21, json!({}))]
        {
            ChatMessage::new(Ts::now(), role, "x".into(), Some(id), meta).insert(&db).unwrap();
        }
        for message in [20, 21] {
            let update = json!({"update_id": 2, "message_reaction": {"chat": {"id": 42, "type": "private"}, "message_id": message,
                "user": {"id": 42}, "date": 0, "old_reaction": [], "new_reaction": [{"type": "emoji", "emoji": "👎"}]}});
            bot.handle(serde_json::from_value(update).unwrap()).await;
        }
        let mut all = vec![];
        // The label and the routed message are logged separately: wait for both, not just the first line.
        for _ in 0..100 {
            all = crate::agent::turnlog::read(&turns).unwrap_or_default();
            if all.iter().any(|e| e["ev"] == "label") && all.iter().any(|e| e["ev"] == "inbound") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let labels: Vec<_> = all.iter().filter(|e| e["ev"] == "label").collect();
        assert_eq!(labels.len(), 1);
        assert_eq!((labels[0]["run"].as_str(), labels[0]["label"].as_str()), (Some("r1"), Some("bad")));
        // The 👎 on the latest assistant reply is also routed as a message to the agent.
        let inbound: Vec<_> = all.iter().filter(|e| e["ev"] == "inbound").collect();
        assert_eq!(inbound.len(), 1);
        assert!(inbound[0]["text"].as_str().unwrap().contains("reacted 👎"));
        assert_eq!(bot.inbox.lock().await.len(), 1, "👎 on the latest reply reaches the agent");
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 2026-11-16: the owner replied to a dips card "update that set, i did 12 reps but typed 8"; the
    /// agent asked "Which exercise was it?". The reply now carries the card, its workout and set ids.
    #[tokio::test]
    async fn reply_to_a_card_reaches_the_agent_with_its_sets_and_typed_sets_stay_fast() {
        let (bot, db, dir, _) = fixture(Arc::new(Chatty(std::sync::atomic::AtomicUsize::new(0))));
        crate::domain::workout::start(&db, Some("fixture"), None, false, Ts::now(), "start").unwrap();
        let workout = crate::domain::workout::active_workout(&db).unwrap().unwrap();
        let card = coach::exercise_card(&db, &workout.id, Ts::now()).unwrap();
        ChatMessage::new(Ts::now(), ChatRole::Bot, card.text.clone(), Some(300), card.meta()).insert(&db).unwrap();
        coach::handle_text(&db, "8x40", Ts::now(), "m1").unwrap();
        let set = crate::db::models::SetLog::first(&db, "", []).unwrap().unwrap();
        let message = |id: i64, text: &str| -> Message {
            serde_json::from_value(json!({"message_id": id, "from": {"id": 42}, "chat": {"id": 42, "type": "private"}, "text": text,
                "reply_to_message": {"message_id": 300, "from": {"id": 7}, "chat": {"id": 42, "type": "private"}, "text": "Bench\nSet 1/3 · 8–10 × ? kg"}}))
            .unwrap()
        };

        bot.message(message(9, "12x40")).await;
        assert!(bot.inbox.lock().await.is_empty(), "a typed set goes to the coach, reply or not");

        bot.message(message(10, "possible to update that set, i did 12 reps")).await;
        let stored = ChatMessage::first(&db, "WHERE telegram_message_id = 10", []).unwrap().unwrap();
        assert_eq!(
            stored.text,
            format!(
                "possible to update that set, i did 12 reps\n[replying to a bot message: \"Bench / Set 1/3 · 8–10 × ? kg\"; it is the **Bench** card of workout {} (Fixture, {}); its sets now: 1. 8 × 40 kg (set_id {})]",
                workout.id,
                workout
                    .started_at
                    .unwrap()
                    .local(&crate::domain::profile::get_profile(&db).unwrap().tz())
                    .strftime("%a %-d %b"),
                set.id
            )
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn set_like_routing() {
        for text in [
            "80x9",
            "12x40kg",
            "90kgx10 reps",
            "9 reps at 80",
            "22,5 × 8",
            "13 reps",
            "13x",
            "13",
            "+10x8",
            "8/side",
            "25 min",
            "1:30",
            "2 km",
            "40 m",
        ] {
            assert_eq!(route(text), Route::Set, "{text}");
        }
        for text in ["how long do I rest?", "it's occupied", "80x9 then 80x10", "x"] {
            assert_eq!(route(text), Route::Agent, "{text}");
        }
        assert_eq!(route("/new"), Route::Agent);
        assert_eq!(route("/status@gymclaw_bot"), Route::Status);
        assert_eq!(route("/start"), Route::Start);
        assert_eq!(route("/help"), Route::Help);
        // The menu only offers commands the bot handles.
        assert!(
            COMMANDS
                .iter()
                .all(|(c, _)| matches!(route(&format!("/{c}")), Route::Status | Route::Help | Route::Extensions))
        );
    }

    #[tokio::test]
    async fn long_reply_is_split_with_buttons_on_last_chunk() {
        let long = format!("{}\n\n{}", "a".repeat(3000), "b".repeat(2500));
        let (bot, _db, _dir, tg) = fixture(Arc::new(EventModel(std::sync::atomic::AtomicUsize::new(0))));
        let outcome = Outcome {
            run_id: "r-long".into(),
            reply: crate::agent::Reply::default(),
            failed: false,
            actions: vec![],
            settles: vec![],
        };
        let buttons = vec![("Go".into(), "Go now".into())];
        assert!(bot.send_reply(&outcome, &long, None, &buttons).await);
        let sends: Vec<_> = tg
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                crate::telegram::coach::tests::Call::Send(text, kb) => Some((text, kb)),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 2);
        assert!(sends[0].1.is_empty());
        assert_eq!(sends[1].1.len(), 1);
        assert!(crate::telegram::utf16_len(&sends[0].0) <= crate::telegram::TEXT_LIMIT);
        assert!(crate::telegram::utf16_len(&sends[1].0) <= crate::telegram::TEXT_LIMIT);
        let stored = ChatMessage::select(&_db, "WHERE role = 'assistant' ORDER BY id", []).unwrap();
        assert_eq!(stored.len(), 2);
    }

    /// 2026-11-21: four screenshots of a plan, sent as one album, got two contradicting replies: the
    /// agent answered photo one before the rest arrived. The album is one turn, each picture once.
    #[tokio::test]
    async fn album_photos_reach_the_agent_in_one_turn() {
        struct Pictures(std::sync::Mutex<Vec<usize>>);
        #[async_trait::async_trait]
        impl crate::agent::ChatModel for Pictures {
            async fn complete(
                &self,
                _: &str,
                messages: &[Value],
                _: &[Value],
                _: bool,
            ) -> Result<crate::agent::ModelReply> {
                let parts = messages.iter().filter_map(|m| m["content"].as_array()).flatten();
                self.0.lock().unwrap().push(parts.filter(|p| p["type"] == "image_url").count());
                Ok(crate::agent::ModelReply {
                    message: json!({"role": "assistant", "content": "Got the plan."}),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                })
            }
        }
        let model = Arc::new(Pictures(std::sync::Mutex::new(vec![])));
        let (bot, _db, dir, _) = fixture(model.clone());
        let bot = Arc::new(bot);
        let (worker, stop) = (bot.clone(), bot.stop.subscribe());
        let looping = tokio::spawn(async move { worker.agent_loop(stop).await });
        let photo = |id: i64, file: &str| -> Message {
            serde_json::from_value(json!({"message_id": id, "from": {"id": 42}, "chat": {"id": 42, "type": "private"},
                "media_group_id": "album", "photo": [{"file_id": file, "file_unique_id": format!("u{id}"), "width": 800}]}))
            .unwrap()
        };
        // The fourth is the first screenshot again (same bytes, new Telegram file).
        for (id, file) in [(1, "GIF89a-legs"), (2, "GIF89a-push"), (3, "GIF89a-pull"), (4, "GIF89a-legs")] {
            bot.message(photo(id, file)).await;
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while model.0.lock().unwrap().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(ALBUM_WAIT).await;
        assert_eq!(*model.0.lock().unwrap(), [3]);
        let _ = bot.stop.send(true);
        looping.await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn start_before_setup_records_question_in_chat_history() {
        let model = Arc::new(EventModel(std::sync::atomic::AtomicUsize::new(0)));
        let (bot, db, dir, tg) = fixture(model.clone());
        bot.send_start().await;
        assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let msg = ChatMessage::first(&db, "WHERE role = 'assistant'", []).unwrap().unwrap();
        assert!(msg.text.contains("voice note") && msg.meta_json["kind"] == "start", "{}", msg.text);
        let buttons: i64 = db.query_row("SELECT count(*) FROM agent_button", [], |r| r.get(0)).unwrap();
        assert_eq!(buttons, 4);
        let grid: Vec<usize> = tg
            .calls()
            .into_iter()
            .find_map(|c| match c {
                crate::telegram::coach::tests::Call::Send(_, kb) => Some(kb.iter().map(Vec::len).collect()),
                _ => None,
            })
            .unwrap();
        assert_eq!(grid, [2, 2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn double_tap_ignores_retired_buttons_but_allows_repeated_rest_cuts() {
        use crate::db::models::{PlannedSession, SessionStatus, SetLog};
        use crate::domain::templates::{Template, import_template};
        use crate::domain::workout::tests::spec;
        use crate::telegram::coach::tests::Call;

        let model = Arc::new(EventModel(std::sync::atomic::AtomicUsize::new(0)));
        let (bot, db, dir, tg) = fixture(model);
        let template = Template {
            id: "pull".into(),
            name: "Pull".into(),
            exercises: vec![spec(
                json!({"id": "row", "name": "Seated cable row", "role": "pull", "primary": false, "working_sets": 2, "target_weight": 80, "rest_seconds": 90}),
            )],
            alternatives: vec![],
            set_duration_seconds: 40,
            transition_seconds: 60,
            source: "user".into(),
        };
        import_template(&db, &template).unwrap();
        let now = Ts::now();
        let plan = PlannedSession {
            planned_start_at: now,
            planned_end_at: now.mins(60),
            prep_start_at: now.mins(-30),
            leave_home_at: now.mins(-15),
            status: SessionStatus::Committed,
            ..PlannedSession::new("2026-W47".into(), Some("pull".into()), false)
        };
        plan.insert(&db).unwrap();

        let cb = |id: &str, msg: i64, data: String, text: &str| -> CallbackQuery {
            serde_json::from_value(json!({
                "id": id,
                "from": {"id": 42},
                "data": data,
                "message": {"message_id": msg, "from": {"id": 1}, "chat": {"id": 42, "type": "private"}, "text": text}
            }))
            .unwrap()
        };

        // 1. Double-tap Start on reminder msg 50 → only one card sent.
        let begin_data = coach::begin_data(&plan.id);
        bot.tap(cb("c1", 50, begin_data.clone(), "⏰ Pull planned")).await;
        bot.tap(cb("c2", 50, begin_data, "⏰ Pull planned")).await;
        let sends = || tg.calls().into_iter().filter(|c| matches!(c, Call::Send(..))).count();
        assert_eq!(sends(), 1, "second Start tap must not send a duplicate card");

        // 2. Double-tap ✅ set on live card msg 100 → only one set_log row.
        let log_data = tg
            .calls()
            .into_iter()
            .find_map(|c| match c {
                Call::Send(_, kb) => kb.into_iter().flatten().find(|b| b.data.starts_with("gc:log:")).map(|b| b.data),
                _ => None,
            })
            .expect("log button on set card");
        bot.tap(cb("c3", 100, log_data.clone(), "Seated cable row")).await;
        bot.tap(cb("c4", 100, log_data, "Seated cable row")).await;
        let sets: i64 = db.query_row("SELECT count(*) FROM set_log", [], |r| r.get(0)).unwrap();
        assert_eq!(sets, 1, "second set tap must not double-log");

        // 3. Double-tap -30s on rest card msg 101 → both cuts apply.
        let cut_data = tg
            .calls()
            .into_iter()
            .rev()
            .find_map(|c| match c {
                Call::Send(_, kb) => kb.into_iter().flatten().find(|b| b.data.starts_with("gc:cut:")).map(|b| b.data),
                _ => None,
            })
            .expect("cut button on rest card");
        let before_due = SetLog::first(&db, "ORDER BY logged_at DESC", []).unwrap().unwrap().rest_due_at.unwrap();
        bot.tap(cb("c5", 101, cut_data.clone(), "Rest")).await;
        bot.tap(cb("c6", 101, cut_data, "Rest")).await;
        let after_due = SetLog::first(&db, "ORDER BY logged_at DESC", []).unwrap().unwrap().rest_due_at.unwrap();
        assert!((before_due - after_due).as_secs() >= 55, "both -30s cuts should apply: {before_due} -> {after_due}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
