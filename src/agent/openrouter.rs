//! OpenRouter chat completions (OpenAI format) with tools.
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{ChatModel, ModelReply, Usage};
use crate::error::{Error, Result};

/// Models reason even at "minimal", and a cut-off answer is worse than a slow one.
pub const MAX_TOKENS: u32 = 16384;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub struct OpenRouter {
    http: reqwest::Client,
    key: String,
    /// Provider tried first (`GYMCLAW_PROVIDER`, e.g. `inference-net/fp4`); `auto` or None = OpenRouter's routing.
    provider: Option<String>,
}

impl OpenRouter {
    pub fn new(key: &str, provider: Option<String>) -> Result<OpenRouter> {
        let http =
            reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(REQUEST_TIMEOUT).build()?;
        Ok(OpenRouter { http, key: key.into(), provider: provider.filter(|p| p != "auto") })
    }
}

/// Request body. `force_text` asks for a plain answer with no further tool calls.
pub fn body(model: &str, messages: &[Value], tools: &[Value], force_text: bool, provider: Option<&str>) -> Value {
    // Some models (GLM) can't turn reasoning off (`enabled: false` is a 400); minimal keeps it to a few hundred tokens.
    let mut body =
        json!({"model": model, "messages": messages, "max_tokens": MAX_TOKENS, "reasoning": {"effort": "minimal"}});
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!(if force_text { "none" } else { "auto" });
    }
    if let Some(p) = provider {
        body["provider"] = json!({"order": [p], "allow_fallbacks": true});
    }
    // Claude caches only what's marked: the system prompt's stable first part for an hour (owner turns
    // are often further apart than 5 minutes), and the whole request for the turn's next step.
    if model.starts_with("anthropic/") {
        if let Some(stable) = body.pointer_mut("/messages/0/content/0") {
            stable["cache_control"] = json!({"type": "ephemeral", "ttl": "1h"});
        }
        body["cache_control"] = json!({"type": "ephemeral"});
    }
    body
}

#[async_trait]
impl ChatModel for OpenRouter {
    async fn complete(&self, model: &str, messages: &[Value], tools: &[Value], force_text: bool) -> Result<ModelReply> {
        let started = Instant::now();
        let request = body(model, messages, tools, force_text, self.provider.as_deref());
        let req_bytes = request.to_string().len();
        let response = self
            .http
            .post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(&self.key)
            .header("X-Title", "gymclaw")
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::domain("MODEL_ERROR", format!("connection: {}", e.without_url())))?;
        let ttfb_ms = started.elapsed().as_millis();
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| Error::domain("MODEL_ERROR", format!("connection: {}", e.without_url())))?;
        let res_bytes = text.len();
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() || parsed.get("error").is_some_and(|e| !e.is_null()) {
            let message = parsed.pointer("/error/message").and_then(Value::as_str).unwrap_or("no details");
            let code =
                parsed.pointer("/error/code").map(|c| c.to_string()).unwrap_or_else(|| status.as_u16().to_string());
            return Err(Error::domain("MODEL_ERROR", format!("{code} {message}")));
        }
        let choice = parsed
            .pointer("/choices/0")
            .cloned()
            .ok_or_else(|| Error::domain("MODEL_ERROR", "upstream returned no choices"))?;
        let usage = &parsed["usage"];
        // The request minus what the trajectory already stores (messages, tool specs).
        let mut settings = request;
        if let Some(map) = settings.as_object_mut() {
            map.remove("messages");
            map.remove("tools");
        }
        let cached = usage.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0);
        Ok(ModelReply {
            message: choice["message"].clone(),
            finish_reason: choice["finish_reason"].as_str().map(String::from),
            usage: Usage {
                input: usage["prompt_tokens"].as_u64().map(|p| p.saturating_sub(cached)),
                output: usage["completion_tokens"].as_u64(),
                reasoning: usage.pointer("/completion_tokens_details/reasoning_tokens").and_then(Value::as_u64),
                cache_read: Some(cached),
            },
            provider: parsed["provider"].as_str().map(String::from),
            generation_id: parsed["id"].as_str().map(String::from),
            settings,
            ms: started.elapsed().as_millis(),
            ttfb_ms,
            req_bytes,
            res_bytes,
        })
    }
}

/// What the transcriber is told. Sets come out in a form the coach's parser reads ("10 reps at 80").
const TRANSCRIBE: &str = "Transcribe this voice note from a gym app user, word for word, in the language spoken. \
Write numbers as digits. If it is only a logged set like \"ten at eighty\" or \"ten reps, eighty kilos\", write it as \
\"10 reps at 80\". Output only the transcript; if nothing is said, output nothing.";

/// Audio format name for OpenRouter's `input_audio` from a Telegram MIME type (voice notes are
/// `audio/ogg`, Opus). None = not a format models take.
pub fn audio_format(mime: Option<&str>) -> Option<&'static str> {
    match mime.unwrap_or("audio/ogg") {
        "audio/ogg" | "audio/opus" => Some("ogg"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => Some("m4a"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/aac" => Some("aac"),
        "audio/flac" => Some("flac"),
        _ => None,
    }
}

/// Transcribe a voice note with an audio-input model (`GYMCLAW_MODEL_AUDIO`), sent as-is: no
/// re-encoding. Empty speech is an error, so silence never reaches the agent.
pub async fn transcribe(chat: &dyn ChatModel, model: &str, audio: &[u8], format: &str) -> Result<String> {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(audio);
    let messages = [
        json!({"role": "system", "content": TRANSCRIBE}),
        json!({"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": data, "format": format}}]}),
    ];
    let reply = chat.complete(model, &messages, &[], false).await?;
    let text = reply.message["content"].as_str().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        crate::bail!("NOTHING_HEARD", "no speech in the voice note");
    }
    Ok(text)
}
