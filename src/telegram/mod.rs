//! Telegram Bot API client, owner chat and the coach card renderer. Owner: WP-runtime.
//!
//! `Telegram` is the small surface the runtime needs; `client::Bot` implements it over HTTPS and
//! tests use fakes. Message text is the Markdown subset the old OpenClaw channel accepted
//! (`**bold**`, `` `code` ``), converted to Telegram HTML by `markdown::to_html`.
pub mod client;
pub mod coach;
pub mod markdown;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::domain::view::Button;
use crate::error::Result;

/// Inline keyboard rows. Empty = no keyboard (an edit with none removes the buttons).
pub type Keyboard = Vec<Vec<Button>>;

/// Telegram's answer when the message we edit is gone (deleted by the owner or another step).
pub fn is_gone(error: &crate::error::Error) -> bool {
    let text = error.to_string().to_lowercase();
    ["message to edit not found", "message can't be edited", "message_id_invalid", "message to delete not found"].iter().any(|p| text.contains(p))
}

#[async_trait]
pub trait Telegram: Send + Sync {
    /// Send text (or a local photo with `text` as caption). Returns the message id.
    async fn send(&self, chat: i64, text: &str, photo: Option<&Path>, buttons: &Keyboard) -> Result<i64>;
    /// Replace text (or caption, for photo messages) and keyboard.
    async fn edit(&self, chat: i64, message: i64, text: &str, buttons: &Keyboard, caption: bool) -> Result<()>;
    /// Replace only the keyboard (empty removes it).
    async fn edit_buttons(&self, chat: i64, message: i64, buttons: &Keyboard) -> Result<()>;
    async fn remove(&self, chat: i64, message: i64) -> Result<()>;
    /// Set one emoji reaction; `None` clears ours.
    async fn react(&self, chat: i64, message: i64, emoji: Option<&str>) -> Result<()>;
    async fn typing(&self, chat: i64) -> Result<()>;
    async fn answer_callback(&self, callback_id: &str, text: Option<&str>) -> Result<()>;
    /// Long-poll for updates after `offset` (single poller).
    async fn updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Update>>;
    /// Download a file the owner sent.
    async fn download(&self, file_id: &str) -> Result<Vec<u8>>;
}

#[derive(Clone, Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub file_unique_id: String,
    #[serde(default)]
    pub width: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Document {
    pub file_id: String,
    pub file_unique_id: String,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub date: i64,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub photo: Option<Vec<PhotoSize>>,
    pub document: Option<Document>,
}

impl Message {
    /// Text or photo caption.
    pub fn body(&self) -> Option<&str> {
        self.text.as_deref().or(self.caption.as_deref())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub message: Option<Message>,
    pub data: Option<String>,
}

/// An image the owner sent, saved under `data/media` (originals keep EXIF for scale backfill).
#[derive(Clone, Debug)]
pub struct Media {
    pub path: PathBuf,
    pub mime: String,
}
