//! Hand-rolled Bot API client over reqwest. Proxy comes from HTTPS_PROXY (the egress container).
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Keyboard, Telegram, Update, markdown};
use crate::error::{Error, Result};

pub struct Bot {
    http: reqwest::Client,
    base: String,
    files: String,
}

impl Bot {
    pub fn new(token: &str) -> Result<Bot> {
        // Long polls hold the connection for up to 50 s; every other call is short.
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(70)).build()?;
        Ok(Bot { http, base: format!("https://api.telegram.org/bot{token}"), files: format!("https://api.telegram.org/file/bot{token}") })
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, body: Value) -> Result<T> {
        let response = self.http.post(format!("{}/{method}", self.base)).json(&body).send().await.map_err(scrub)?;
        parse(method, response).await
    }

    /// Make the "/" menu exactly `commands` (command, description). OpenClaw left its own menu in the
    /// default and group scopes; group commands go too, the bot is DM only.
    pub async fn set_commands(&self, commands: &[(&str, &str)]) -> Result<()> {
        let commands: Vec<Value> = commands.iter().map(|(command, description)| json!({"command": command, "description": description})).collect();
        let _: bool = self.call("setMyCommands", json!({"commands": commands})).await?;
        let _: bool = self.call("deleteMyCommands", json!({"scope": {"type": "all_group_chats"}})).await?;
        Ok(())
    }

    /// HTML first; if Telegram can't parse the entities, the same text without formatting.
    async fn call_text<T: DeserializeOwned>(&self, method: &str, mut body: Value, text_key: &str, text: &str) -> Result<T> {
        body[text_key] = json!(markdown::to_html(text));
        body["parse_mode"] = json!("HTML");
        match self.call(method, body.clone()).await {
            Err(e) if e.to_string().contains("can't parse entities") => {
                body[text_key] = json!(markdown::plain(text));
                body.as_object_mut().expect("object").remove("parse_mode");
                self.call(method, body).await
            }
            other => other,
        }
    }
}

/// Errors carry Telegram's description; the token never appears (reqwest URLs are stripped).
fn scrub(error: reqwest::Error) -> Error {
    Error::Http(error.without_url())
}

async fn parse<T: DeserializeOwned>(method: &str, response: reqwest::Response) -> Result<T> {
    let body: Value = response.json().await.map_err(scrub)?;
    if body["ok"] == json!(true) {
        return Ok(serde_json::from_value(body["result"].clone())?);
    }
    let description = body["description"].as_str().unwrap_or("unknown error");
    Err(Error::domain("TELEGRAM_ERROR", format!("{method}: {description}")))
}

pub fn markup(buttons: &Keyboard) -> Value {
    json!({"inline_keyboard": buttons.iter().map(|row| row.iter().map(|b| json!({"text": b.text, "callback_data": b.data})).collect::<Vec<_>>()).collect::<Vec<_>>()})
}

#[derive(serde::Deserialize)]
struct Sent {
    message_id: i64,
}

#[async_trait]
impl Telegram for Bot {
    async fn send(&self, chat: i64, text: &str, photo: Option<&Path>, buttons: &Keyboard) -> Result<i64> {
        let reply_markup = if buttons.is_empty() { None } else { Some(markup(buttons)) };
        let sent: Sent = match photo {
            None => {
                let mut body = json!({"chat_id": chat, "link_preview_options": {"is_disabled": true}});
                if let Some(m) = reply_markup {
                    body["reply_markup"] = m;
                }
                self.call_text("sendMessage", body, "text", text).await?
            }
            Some(path) => {
                let bytes = tokio::fs::read(path).await?;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "photo.png".into());
                let form = |html: bool| {
                    let mut form = reqwest::multipart::Form::new()
                        .text("chat_id", chat.to_string())
                        .text("caption", if html { markdown::to_html(text) } else { markdown::plain(text) })
                        .part("photo", reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone()));
                    if html {
                        form = form.text("parse_mode", "HTML");
                    }
                    if let Some(m) = &reply_markup {
                        form = form.text("reply_markup", m.to_string());
                    }
                    form
                };
                let url = format!("{}/sendPhoto", self.base);
                match parse("sendPhoto", self.http.post(&url).multipart(form(true)).send().await.map_err(scrub)?).await {
                    Err(e) if e.to_string().contains("can't parse entities") => parse("sendPhoto", self.http.post(&url).multipart(form(false)).send().await.map_err(scrub)?).await?,
                    other => other?,
                }
            }
        };
        Ok(sent.message_id)
    }

    async fn edit(&self, chat: i64, message: i64, text: &str, buttons: &Keyboard, caption: bool) -> Result<()> {
        let body = json!({"chat_id": chat, "message_id": message, "reply_markup": markup(buttons)});
        let (method, key) = if caption { ("editMessageCaption", "caption") } else { ("editMessageText", "text") };
        match self.call_text::<Value>(method, body.clone(), key, text).await {
            // Unknown message kind (from before a restart): a photo needs a caption edit.
            Err(e) if !caption && e.to_string().contains("no text in the message") => self.call_text::<Value>("editMessageCaption", body, "caption", text).await.map(|_| ()),
            other => other.map(|_| ()),
        }
    }

    async fn edit_buttons(&self, chat: i64, message: i64, buttons: &Keyboard) -> Result<()> {
        self.call::<Value>("editMessageReplyMarkup", json!({"chat_id": chat, "message_id": message, "reply_markup": markup(buttons)})).await.map(|_| ())
    }

    async fn remove(&self, chat: i64, message: i64) -> Result<()> {
        self.call::<Value>("deleteMessage", json!({"chat_id": chat, "message_id": message})).await.map(|_| ())
    }

    async fn react(&self, chat: i64, message: i64, emoji: Option<&str>) -> Result<()> {
        let reaction: Vec<Value> = emoji.map(|e| json!({"type": "emoji", "emoji": e})).into_iter().collect();
        self.call::<Value>("setMessageReaction", json!({"chat_id": chat, "message_id": message, "reaction": reaction})).await.map(|_| ())
    }

    async fn typing(&self, chat: i64) -> Result<()> {
        self.call::<Value>("sendChatAction", json!({"chat_id": chat, "action": "typing"})).await.map(|_| ())
    }

    async fn answer_callback(&self, callback_id: &str, text: Option<&str>) -> Result<()> {
        let mut body = json!({"callback_query_id": callback_id});
        if let Some(t) = text {
            body["text"] = json!(t);
        }
        self.call::<Value>("answerCallbackQuery", body).await.map(|_| ())
    }

    async fn updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Update>> {
        self.call("getUpdates", json!({"offset": offset, "timeout": timeout_secs, "allowed_updates": ["message", "callback_query"]})).await
    }

    async fn download(&self, file_id: &str) -> Result<Vec<u8>> {
        #[derive(serde::Deserialize)]
        struct File {
            file_path: Option<String>,
        }
        let file: File = self.call("getFile", json!({"file_id": file_id})).await?;
        let Some(path) = file.file_path else { crate::bail!("TELEGRAM_ERROR", "getFile: no file path (file too big?)") };
        let response = self.http.get(format!("{}/{path}", self.files)).send().await.map_err(scrub)?.error_for_status().map_err(scrub)?;
        Ok(response.bytes().await.map_err(scrub)?.to_vec())
    }
}
