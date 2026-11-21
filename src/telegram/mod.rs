//! Telegram Bot API client, owner chat and the coach card renderer.
//!
//! `Telegram` is the small surface the runtime needs; `client::Bot` implements it over HTTPS and
//! tests use fakes. Message text is the Markdown subset in `markdown`, converted to Telegram HTML
//! by `markdown::to_html`.
pub mod client;
pub mod coach;
pub mod markdown;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::domain::view::Button;
use crate::error::Result;

/// Message effects Telegram plays when a message arrives: for moments worth celebrating, so keep them rare.
pub const EFFECT_CONFETTI: &str = "5046509860389126442";
pub const EFFECT_FIRE: &str = "5104841245755180586";

/// Inline keyboard rows. Empty = no keyboard (an edit with none removes the buttons).
pub type Keyboard = Vec<Vec<Button>>;

/// Telegram's answer when the message we edit is gone (deleted by the owner or another step).
pub fn is_gone(error: &crate::error::Error) -> bool {
    let text = error.to_string().to_lowercase();
    ["message to edit not found", "message can't be edited", "message_id_invalid", "message to delete not found"]
        .iter()
        .any(|p| text.contains(p))
}

/// Telegram's answer to an edit that changes nothing ("message is not modified").
pub fn is_noop(error: &crate::error::Error) -> bool {
    error.to_string().contains("message is not modified")
}

#[async_trait]
pub trait Telegram: Send + Sync {
    /// Send text (or a local photo with `text` as caption), with an optional animated effect
    /// (`EFFECT_*`, private chats only). Returns the message id.
    async fn send(
        &self,
        chat: i64,
        text: &str,
        photo: Option<&Path>,
        buttons: &Keyboard,
        effect: Option<&str>,
    ) -> Result<i64>;
    /// Replace text (or caption, for photo messages) and keyboard.
    async fn edit(&self, chat: i64, message: i64, text: &str, buttons: &Keyboard, caption: bool) -> Result<()>;
    /// Replace only the keyboard (empty removes it).
    async fn edit_buttons(&self, chat: i64, message: i64, buttons: &Keyboard) -> Result<()>;
    async fn remove(&self, chat: i64, message: i64) -> Result<()>;
    /// Pin (silently) or unpin a message.
    async fn pin(&self, chat: i64, message: i64, pinned: bool) -> Result<()>;
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
    pub message_reaction: Option<MessageReaction>,
}

impl Update {
    /// Telegram user id for a private DM update (message, callback query, or reaction), or `None`
    /// for group chats, channel posts, or updates without a matching sender.
    pub fn dm_user_id(&self) -> Option<i64> {
        if let Some(q) = &self.callback_query {
            let chat_ok = q.message.as_ref().is_none_or(|m| m.chat.kind == "private" && m.chat.id == q.from.id);
            return chat_ok.then_some(q.from.id);
        }
        if let Some(r) = &self.message_reaction {
            return r.user.as_ref().filter(|u| r.chat.kind == "private" && r.chat.id == u.id).map(|u| u.id);
        }
        let m = self.message.as_ref()?;
        let from = m.from.as_ref()?;
        (m.chat.kind == "private" && m.chat.id == from.id).then_some(from.id)
    }
}

/// The owner changed their reactions on a message: `new_reaction` is the full set now.
#[derive(Clone, Debug, Deserialize)]
pub struct MessageReaction {
    pub chat: Chat,
    pub message_id: i64,
    pub user: Option<User>,
    #[serde(default)]
    pub new_reaction: Vec<Reaction>,
}

/// One reaction; only emoji ones carry `emoji` (custom emoji and paid stars don't).
#[derive(Clone, Debug, Deserialize)]
pub struct Reaction {
    pub emoji: Option<String>,
}

/// A voice note or an audio file.
#[derive(Clone, Debug, Deserialize)]
pub struct Audio {
    pub file_id: String,
    #[serde(default)]
    pub duration: i64,
    pub mime_type: Option<String>,
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
    pub text: Option<String>,
    pub caption: Option<String>,
    pub photo: Option<Vec<PhotoSize>>,
    pub document: Option<Document>,
    pub voice: Option<Audio>,
    pub audio: Option<Audio>,
    /// The message this one replies to (Telegram "Reply"), as Telegram has it now.
    pub reply_to_message: Option<Box<Message>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: i64, kind: &str) -> Chat {
        Chat { id, kind: kind.into() }
    }

    fn msg(chat_id: i64, kind: &str, from_id: Option<i64>) -> Message {
        Message {
            message_id: 1,
            from: from_id.map(|id| User { id }),
            chat: chat(chat_id, kind),
            text: Some("hi".into()),
            caption: None,
            photo: None,
            document: None,
            voice: None,
            audio: None,
            reply_to_message: None,
        }
    }

    #[test]
    fn dm_user_id_extracts_private_sender_and_ignores_groups() {
        let dm = Update {
            update_id: 1,
            message: Some(msg(42, "private", Some(42))),
            callback_query: None,
            message_reaction: None,
        };
        assert_eq!(dm.dm_user_id(), Some(42));

        let group = Update {
            update_id: 2,
            message: Some(msg(-100, "supergroup", Some(42))),
            callback_query: None,
            message_reaction: None,
        };
        assert_eq!(group.dm_user_id(), None);

        let cb_ok = Update {
            update_id: 3,
            message: None,
            callback_query: Some(CallbackQuery {
                id: "c1".into(),
                from: User { id: 42 },
                message: Some(msg(42, "private", Some(99))),
                data: None,
            }),
            message_reaction: None,
        };
        assert_eq!(cb_ok.dm_user_id(), Some(42));

        let cb_group = Update {
            update_id: 4,
            message: None,
            callback_query: Some(CallbackQuery {
                id: "c2".into(),
                from: User { id: 42 },
                message: Some(msg(-100, "group", Some(99))),
                data: None,
            }),
            message_reaction: None,
        };
        assert_eq!(cb_group.dm_user_id(), None);

        let react = Update {
            update_id: 5,
            message: None,
            callback_query: None,
            message_reaction: Some(MessageReaction {
                chat: chat(42, "private"),
                message_id: 1,
                user: Some(User { id: 42 }),
                new_reaction: vec![],
            }),
        };
        assert_eq!(react.dm_user_id(), Some(42));
    }
}
