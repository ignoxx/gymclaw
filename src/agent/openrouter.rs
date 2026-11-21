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
    let mut body = json!({"model": model, "messages": messages, "max_tokens": MAX_TOKENS, "reasoning": {"effort": "minimal"}, "usage": {"include": true}});
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

/// Max elapsed time on attempt 1 that still qualifies for a retry, keeping total wall time well
/// inside the turn `DEADLINE` (300 s).
const MAX_RETRY_ELAPSED: Duration = Duration::from_secs(30);
const MAX_RETRY_AFTER_SECS: u64 = 5;

/// Retry delay for pre-generation transient HTTP errors (429, 500, 502, 503, 504, 529).
/// Never retries HTTP 200 inline errors (generation already started; retrying could double-bill),
/// connection/timeout errors, or `Retry-After` waits longer than 5 s.
fn retry_delay(http_status: u16, retry_after: Option<&str>) -> Option<Duration> {
    if !matches!(http_status, 429 | 500 | 502 | 503 | 504 | 529) {
        return None;
    }
    let secs = match retry_after {
        None => 1,
        Some(raw) => {
            let parsed = raw.trim().parse::<u64>().ok()?;
            if parsed > MAX_RETRY_AFTER_SECS {
                return None;
            }
            parsed.max(1)
        }
    };
    Some(Duration::from_secs(secs))
}

#[async_trait]
impl ChatModel for OpenRouter {
    async fn complete(&self, model: &str, messages: &[Value], tools: &[Value], force_text: bool) -> Result<ModelReply> {
        let started = Instant::now();
        let request = body(model, messages, tools, force_text, self.provider.as_deref());
        let req_bytes = request.to_string().len();
        let mut attempt = 0u8;
        let (ttfb_ms, res_bytes, parsed) = loop {
            let attempt_started = Instant::now();
            let response = self
                .http
                .post("https://openrouter.ai/api/v1/chat/completions")
                .bearer_auth(&self.key)
                .header("X-Title", "gymclaw")
                .json(&request)
                .send()
                .await
                .map_err(|e| Error::domain("MODEL_ERROR", format!("connection: {}", e.without_url())))?;
            let ttfb_ms = attempt_started.elapsed().as_millis();
            let status = response.status();
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(String::from);
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
                if attempt == 0
                    && started.elapsed() < MAX_RETRY_ELAPSED
                    && let Some(delay) = retry_delay(status.as_u16(), retry_after.as_deref())
                {
                    attempt += 1;
                    tracing::warn!("openrouter transient error ({code} {message}); retrying in {}s", delay.as_secs());
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(Error::domain("MODEL_ERROR", format!("{code} {message}")));
            }
            break (ttfb_ms, res_bytes, parsed);
        };
        let choice = parse_choice(&parsed)?;
        let usage = &parsed["usage"];
        // The request minus what the trajectory already stores (messages, tool specs).
        let mut settings = request;
        if let Some(map) = settings.as_object_mut() {
            map.remove("messages");
            map.remove("tools");
        }
        Ok(ModelReply {
            message: choice["message"].clone(),
            finish_reason: choice["finish_reason"].as_str().map(String::from),
            usage: parse_usage(usage),
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

fn parse_usage(usage: &Value) -> Usage {
    let cached = usage.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input: usage["prompt_tokens"].as_u64().map(|p| p.saturating_sub(cached)),
        output: usage["completion_tokens"].as_u64(),
        reasoning: usage.pointer("/completion_tokens_details/reasoning_tokens").and_then(Value::as_u64),
        cache_read: Some(cached),
        cost: usage["cost"].as_f64(),
    }
}

/// Extract `choices[0]`, surfacing OpenRouter mid-generation errors (`finish_reason == "error"`
/// or a non-null `choice.error`) as `MODEL_ERROR` instead of an empty silent reply.
fn parse_choice(parsed: &Value) -> Result<Value> {
    let choice = parsed
        .pointer("/choices/0")
        .cloned()
        .ok_or_else(|| Error::domain("MODEL_ERROR", "upstream returned no choices"))?;
    let is_error =
        choice["finish_reason"].as_str() == Some("error") || choice.get("error").is_some_and(|e| !e.is_null());
    if is_error {
        let message = choice.pointer("/error/message").and_then(Value::as_str).unwrap_or("mid-generation error");
        let code = choice.pointer("/error/code").map(|c| c.to_string()).unwrap_or_else(|| "upstream".into());
        return Err(Error::domain("MODEL_ERROR", format!("{code} {message}")));
    }
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_usage_extracts_tokens_and_exact_cost() {
        let u = parse_usage(&json!({
            "prompt_tokens": 1200,
            "completion_tokens": 180,
            "prompt_tokens_details": {"cached_tokens": 800},
            "completion_tokens_details": {"reasoning_tokens": 40},
            "cost": 0.00125
        }));
        assert_eq!(u.input, Some(400));
        assert_eq!(u.output, Some(180));
        assert_eq!(u.cache_read, Some(800));
        assert_eq!(u.reasoning, Some(40));
        assert_eq!(u.cost, Some(0.00125));

        let without_cost = parse_usage(&json!({"prompt_tokens": 100, "completion_tokens": 20}));
        assert_eq!(without_cost.cost, None);

        let req = body("anthropic/claude-haiku-5.5", &[], &[], false, None);
        assert_eq!(req["usage"]["include"], true);
    }

    #[test]
    fn retry_delay_retries_only_pre_generation_transient_http_errors() {
        assert_eq!(retry_delay(502, None), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(529, None), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(429, Some("3")), Some(Duration::from_secs(3)));
        assert_eq!(retry_delay(429, Some("0")), Some(Duration::from_secs(1)));
        // Long or non-numeric Retry-After: skip retry instead of hammering the limit.
        assert_eq!(retry_delay(429, Some("60")), None);
        assert_eq!(retry_delay(429, Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        // HTTP 200 inline errors (already generating) and 4xx client errors never retry.
        assert_eq!(retry_delay(200, None), None);
        assert_eq!(retry_delay(400, None), None);
        assert_eq!(retry_delay(401, None), None);
        assert_eq!(retry_delay(402, None), None);
        assert_eq!(retry_delay(403, None), None);
    }

    #[test]
    fn parse_choice_surfaces_mid_generation_errors() {
        let ok = parse_choice(&json!({
            "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "hi"}}]
        }))
        .unwrap();
        assert_eq!(ok["message"]["content"], "hi");

        let err = parse_choice(&json!({
            "choices": [{
                "finish_reason": "error",
                "error": {"code": 502, "message": "Provider disconnected"},
                "message": {"role": "assistant", "content": ""}
            }]
        }))
        .unwrap_err();
        assert_eq!(err.code(), "MODEL_ERROR");
        assert!(err.to_string().contains("502") && err.to_string().contains("Provider disconnected"));

        let no_detail = parse_choice(&json!({
            "choices": [{"finish_reason": "error", "message": {"role": "assistant", "content": ""}}]
        }))
        .unwrap_err();
        assert_eq!(no_detail.code(), "MODEL_ERROR");
        assert!(no_detail.to_string().contains("upstream"));

        let missing = parse_choice(&json!({"choices": []})).unwrap_err();
        assert_eq!(missing.code(), "MODEL_ERROR");
    }
}
