//! The conversational agent: one turn = context from the DB, a tool loop over OpenRouter, and exactly
//! one outcome (reply text with optional buttons, a reaction, or silence). Owner: WP-runtime.
//!
//! Tools are `tools::all()` plus `reply`, the turn's final answer. Each tool call runs in its own DB
//! transaction on the blocking pool; errors go back to
//! the model as `Error::to_json()`. Coach views a tool produced are rendered after the commit.
pub mod context;
pub mod failure;
pub mod media;
pub mod openrouter;
pub mod turnlog;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::runtime::blocking;
use crate::telegram::Media;
use crate::telegram::coach::Coach;
use crate::time::Ts;
use crate::tools::{self, Effect};
use failure::{FailureState, diagnose, format_failure};
use turnlog::{TurnLog, clean, scrub, sha};

/// Tool calls per turn before the model must answer.
pub const STEP_CAP: usize = 12;
/// Wall time for a whole turn.
pub const DEADLINE: Duration = Duration::from_secs(300);
/// Longest tool result the model gets back (chars); more is cut with a marker.
const MAX_RESULT: usize = 12_000;
const REPLY: &str = "reply";

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    pub cache_read: Option<u64>,
}

/// One assistant message from the model.
#[derive(Clone, Debug, Default)]
pub struct ModelReply {
    /// OpenAI-format assistant message (`content`, `tool_calls`).
    pub message: Value,
    pub finish_reason: Option<String>,
    pub usage: Usage,
    pub provider: Option<String>,
    pub ms: u128,
    pub ttfb_ms: u128,
    pub req_bytes: usize,
    pub res_bytes: usize,
}

#[async_trait]
pub trait ChatModel: Send + Sync {
    /// `force_text`: no more tool calls, answer now.
    async fn complete(&self, model: &str, messages: &[Value], tools: &[Value], force_text: bool) -> Result<ModelReply>;
}

#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema)]
pub struct ReplyButton {
    /// Short label, e.g. "13:00".
    pub label: String,
    /// What the owner "says" by tapping it, as a plain request: "Move Push to 13:00".
    pub value: String,
}

/// The `reply` tool: how a turn ends.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema)]
pub struct Reply {
    /// The message (Markdown: **bold**, `code`). Omit to send no text.
    pub text: Option<String>,
    /// Choices as buttons, one row each (at most 8).
    #[serde(default)]
    pub buttons: Vec<ReplyButton>,
    /// One emoji reaction on the owner's message instead of (or with) text, e.g. 👍 or 👌.
    pub react: Option<String>,
}

/// How a turn ended. A failure is a reply with the sanitized line.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub reply: Reply,
    pub failed: bool,
    pub run_id: String,
}

/// What a turn answers.
pub struct TurnInput {
    /// Idempotency prefix for this turn's tool calls: `tg-<message id>` or `tg-cb:<callback id>`.
    pub key: String,
    /// Photos/documents from this turn (owner messages are already in `chat_message`).
    pub images: Vec<Media>,
}

pub struct Agent {
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
    pub model: Arc<dyn ChatModel>,
    pub chat_model: String,
    pub vision_model: String,
    pub coach: Option<Arc<Coach>>,
    pub log: TurnLog,
    /// Time source (tests pin it).
    pub clock: Arc<dyn Fn() -> Ts + Send + Sync>,
}

/// Tool definitions in OpenAI format, `reply` last.
pub fn tool_specs() -> Vec<Value> {
    let mut specs: Vec<Value> = tools::all()
        .iter()
        .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": (t.schema)()}}))
        .collect();
    let settings = schemars::generate::SchemaSettings::draft07().with(|s| s.inline_subschemas = true);
    let mut schema = settings.into_generator().into_root_schema_for::<Reply>().to_value();
    if let Some(o) = schema.as_object_mut() {
        o.remove("$schema");
        o.remove("title");
    }
    specs.push(json!({"type": "function", "function": {"name": REPLY, "description": "Finish the turn: what the owner gets. Call it once, last. With no fields the owner gets nothing.", "parameters": schema}}));
    specs
}

fn cut(text: String) -> String {
    if text.len() <= MAX_RESULT {
        return text;
    }
    let mut end = MAX_RESULT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[cut {} chars; narrow the query]", &text[..end], text.len() - end)
}

/// "NO_REPLY" and empty text mean silence.
fn as_reply(text: Option<&str>) -> Reply {
    let text = text.map(str::trim).filter(|t| !t.is_empty() && *t != "NO_REPLY");
    Reply { text: text.map(String::from), ..Reply::default() }
}

impl Agent {
    /// Run one tool call in its own transaction. Returns the JSON the model sees and whether it succeeded.
    async fn tool(&self, name: &str, args: Value, request_id: String) -> (Value, bool) {
        let now = (self.clock)();
        let data_dir = self.data_dir.clone();
        let name_owned = name.to_string();
        let result = blocking(&self.db_path, move |db| {
            let tx = db.transaction()?;
            let mut cx = tools::Ctx { db: &tx, now, request_id, data_dir: &data_dir, effects: vec![] };
            let value = tools::call(&mut cx, &name_owned, args)?;
            let effects = std::mem::take(&mut cx.effects);
            tx.commit()?;
            Ok((value, effects))
        })
        .await;
        match result {
            Ok((value, effects)) => {
                for Effect::Coach(view) in effects {
                    if let Some(coach) = &self.coach
                        && let Err(e) = coach.apply(&view, None, None).await
                    {
                        tracing::warn!("coach card from {name} not shown: {e}");
                    }
                }
                (value, true)
            }
            Err(e) => (e.to_json(), false),
        }
    }

    /// One turn over the chat window. Never fails: problems become a sanitized reply.
    pub async fn turn(&self, input: TurnInput) -> Outcome {
        let run_id = crate::db::new_id();
        let started = Instant::now();
        let mut state = FailureState::default();
        let mut calls_log = vec![];
        let result = tokio::time::timeout(DEADLINE, self.run(&input, &run_id, &mut state, &mut calls_log)).await;
        let (reply, failed, error) = match result {
            Ok(Ok(reply)) => (reply, false, None),
            Ok(Err(e)) => {
                state.error = format!("{}: {e}", e.code());
                (Reply { text: Some(format_failure(&state)), ..Reply::default() }, true, Some(state.error.clone()))
            }
            Err(_) => {
                state.error = "turn deadline exceeded".into();
                (Reply { text: Some(format_failure(&state)), ..Reply::default() }, true, Some(state.error.clone()))
            }
        };
        let text = reply.text.clone().unwrap_or_default();
        self.log.log(json!({
            "ev": "run_end", "run": run_id, "session": "telegram:owner", "trigger": "user", "ok": !failed,
            "error": error.map(|e| scrub(&e, 500)), "diagnosis": failed.then(|| diagnose(&state)), "ms": started.elapsed().as_millis() as u64, "calls": calls_log,
            "noReply": (reply.text.is_none() && reply.react.is_none()).then_some(true), "reply": scrub(&text, 4000),
        }));
        Outcome { reply, failed, run_id }
    }

    async fn run(&self, input: &TurnInput, run_id: &str, state: &mut FailureState, calls_log: &mut Vec<Value>) -> Result<Reply> {
        let now = (self.clock)();
        let (system, history) = blocking(&self.db_path, move |db| Ok((context::system(db, now)?, context::history(db)?))).await?;
        let mut messages = vec![json!({"role": "system", "content": system})];
        let history_chars = Value::Array(history.clone()).to_string().len();
        let history_len = history.len();
        messages.extend(history);
        let images = media::attach(&mut messages, &input.images).await;
        let model = if images > 0 { &self.vision_model } else { &self.chat_model };
        let specs = tool_specs();
        let prompt = messages.iter().rev().find(|m| m["role"] == "user").map(|m| media::text_of(&m["content"])).unwrap_or_default();
        self.log.log(json!({
            "ev": "llm_input", "run": run_id, "session": "telegram:owner", "trigger": "user", "provider": "openrouter", "model": model,
            "history": history_len, "historyChars": history_chars, "systemChars": system.chars().count(), "systemSha": sha(&system),
            "tools": specs.len(), "toolsChars": Value::Array(specs.clone()).to_string().len(), "images": (images > 0).then_some(images),
            "chars": prompt.chars().count(), "prompt": scrub(&prompt, 4000),
        }));

        let mut steps = 0usize;
        let mut index = 0usize;
        let mut seen: HashMap<String, u32> = HashMap::new();
        // Set once the model loops on one call: the next turn must answer in text.
        let mut looping = false;
        loop {
            let force_text = steps >= STEP_CAP || looping;
            state.observed_input = true;
            let call_started = Instant::now();
            let reply = match self.model.complete(model, &messages, &specs, force_text).await {
                Ok(r) => r,
                Err(e) => {
                    self.log.log(json!({"ev": "model_call", "run": run_id, "call": calls_log.len(), "provider": "openrouter", "model": model,
                        "ms": call_started.elapsed().as_millis() as u64, "outcome": "error", "error": scrub(&e.to_string(), 500)}));
                    return Err(e);
                }
            };
            state.stop = reply.finish_reason.clone();
            state.output = reply.usage.output;
            state.reasoning = reply.usage.reasoning;
            let tool_calls = reply.message["tool_calls"].as_array().cloned().unwrap_or_default();
            self.log.log(json!({"ev": "model_call", "run": run_id, "call": calls_log.len(), "provider": reply.provider, "model": model,
                "ms": reply.ms as u64, "ttfbMs": reply.ttfb_ms as u64, "outcome": "completed", "reqBytes": reply.req_bytes, "resBytes": reply.res_bytes}));
            let usage = json!({"input": reply.usage.input, "output": reply.usage.output, "reasoning": reply.usage.reasoning, "cacheRead": reply.usage.cache_read});
            self.log.log(json!({"ev": "llm_output", "run": run_id, "model": model, "usage": usage, "stop": reply.finish_reason}));
            let mut call = usage.clone();
            call["stop"] = json!(reply.finish_reason);
            call["toolCalls"] = json!(tool_calls.len());
            calls_log.push(call);

            let content = reply.message["content"].as_str();
            if tool_calls.is_empty() || force_text {
                if reply.finish_reason.as_deref() == Some("length") && content.is_none_or(|c| c.trim().is_empty()) {
                    crate::bail!("OUTPUT_LIMIT", "model stopped at the output limit");
                }
                if force_text && content.is_none_or(|c| c.trim().is_empty()) {
                    crate::bail!("STUCK", "step limit reached without an answer");
                }
                return Ok(as_reply(content));
            }
            // Text next to tool calls is narration ("Moving it now…"): it never reaches the owner.
            messages.push(json!({"role": "assistant", "content": content, "tool_calls": tool_calls}));
            let mut finished: Option<Reply> = None;
            for tc in &tool_calls {
                let id = tc["id"].as_str().unwrap_or_default().to_string();
                let name = tc.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string();
                let raw = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
                let args: std::result::Result<Value, _> = if raw.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str(raw) };
                let started = Instant::now();
                let (result, ok) = match (name.as_str(), args) {
                    (_, Err(e)) => (Error::domain("INVALID_JSON", format!("arguments are not JSON: {e}")).to_json(), false),
                    (REPLY, Ok(args)) => match serde_json::from_value::<Reply>(args) {
                        Ok(reply) => {
                            finished = Some(reply);
                            (json!({"ok": true}), true)
                        }
                        Err(e) => (Error::domain("INVALID_ARGUMENTS", format!("reply: {e}")).to_json(), false),
                    },
                    (_, Ok(args)) => {
                        let key = format!("{name} {args}");
                        let repeats = seen.entry(key).or_default();
                        *repeats += 1;
                        if *repeats >= 2 {
                            looping |= *repeats >= 3;
                            let message = "Same call with the same arguments as before; the result won't change. Use the earlier result, try something else, or reply.";
                            (Error::domain("REPEATED_CALL", message).to_json(), false)
                        } else {
                            steps += 1;
                            index += 1;
                            state.tool_calls += 1;
                            self.tool(&name, args, format!("{}-{index}", input.key)).await
                        }
                    }
                };
                self.log_tool(run_id, &id, &name, &serde_json::from_str(raw).unwrap_or(Value::Null), &result, ok, started);
                messages.push(json!({"role": "tool", "tool_call_id": id, "content": cut(result.to_string())}));
            }
            if let Some(mut reply) = finished {
                reply.text = as_reply(reply.text.as_deref()).text;
                reply.buttons.truncate(8);
                return Ok(reply);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn log_tool(&self, run: &str, call: &str, name: &str, args: &Value, result: &Value, ok: bool, started: Instant) {
        let text = result.to_string();
        let error = result.pointer("/error/code").and_then(Value::as_str).map(|code| format!("{code}: {}", result.pointer("/error/message").and_then(Value::as_str).unwrap_or("")));
        self.log.log(json!({
            "ev": "tool", "run": run, "call": call, "name": name, "ms": started.elapsed().as_millis() as u64, "ok": ok,
            "error": error.map(|e| scrub(&e, 500)), "args": clean(args, 1000, 0),
            "result": {"chars": text.len(), "sha": sha(&text), "preview": scrub(&text, 1000)},
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::db::models::{AgentNote, ChatMessage, ChatRole};
    use crate::db::Json;

    /// Plays back scripted assistant messages and records what it was sent.
    struct Script {
        replies: Mutex<Vec<Value>>,
        seen: Mutex<Vec<(usize, bool)>>,
    }

    #[async_trait]
    impl ChatModel for Script {
        async fn complete(&self, _: &str, messages: &[Value], _: &[Value], force_text: bool) -> Result<ModelReply> {
            self.seen.lock().unwrap().push((messages.len(), force_text));
            let mut replies = self.replies.lock().unwrap();
            let message = if replies.is_empty() { json!({"content": "fallback"}) } else { replies.remove(0) };
            Ok(ModelReply { message, finish_reason: Some("stop".into()), ..ModelReply::default() })
        }
    }

    fn call(name: &str, args: Value) -> Value {
        json!({"content": "Working on it…", "tool_calls": [{"id": format!("c-{name}"), "type": "function", "function": {"name": name, "arguments": args.to_string()}}]})
    }

    fn scripted(replies: Vec<Value>) -> (Agent, Arc<Script>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("gymclaw-agent-{}", crate::db::new_id()));
        let db_path = dir.join("gymclaw.db");
        let mut db = crate::db::open(&db_path).unwrap();
        crate::db::migrate(&mut db).unwrap();
        let now = Ts::parse("2026-10-12T10:00:00Z", None).unwrap();
        ChatMessage { id: None, at: now, role: ChatRole::Owner, text: "remember I hate lunges".into(), telegram_message_id: Some(5), meta_json: Json(json!({})) }.insert(&db).unwrap();
        let script = Arc::new(Script { replies: Mutex::new(replies), seen: Mutex::new(vec![]) });
        let agent = Agent {
            db_path, data_dir: dir.clone(), model: script.clone(), chat_model: "m".into(), vision_model: "v".into(), coach: None,
            log: TurnLog::default(), clock: Arc::new(move || now),
        };
        (agent, script, dir)
    }

    fn input() -> TurnInput {
        TurnInput { key: "tg-5".into(), images: vec![] }
    }

    #[tokio::test]
    async fn tool_then_one_reply_with_buttons_and_narration_dropped() {
        let reply = call("reply", json!({"text": "Noted.", "buttons": [{"label": "Undo", "value": "Forget the lunges note"}]}));
        let (agent, script, dir) = scripted(vec![call("note_add", json!({"kind": "owner", "text": "Hates lunges"})), reply]);
        let outcome = agent.turn(input()).await;
        assert!(!outcome.failed);
        assert_eq!(outcome.reply.text.as_deref(), Some("Noted."));
        assert_eq!(outcome.reply.buttons[0].value, "Forget the lunges note");
        let db = crate::db::open(&dir.join("gymclaw.db")).unwrap();
        assert_eq!(AgentNote::select(&db, "", []).unwrap()[0].text, "Hates lunges");
        // The note is in the next turn's prompt.
        assert!(context::system(&db, Ts::now()).unwrap().contains("- [1] Hates lunges"));
        assert_eq!(script.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn plain_text_is_the_reply_and_no_reply_is_silence() {
        let (agent, _, _) = scripted(vec![json!({"content": "  **Mon** Pull  "})]);
        assert_eq!(agent.turn(input()).await.reply.text.as_deref(), Some("**Mon** Pull"));
        let (agent, _, _) = scripted(vec![json!({"content": "NO_REPLY"})]);
        assert_eq!(agent.turn(input()).await.reply, Reply::default());
        let (agent, _, _) = scripted(vec![call("reply", json!({"react": "👍"}))]);
        assert_eq!(agent.turn(input()).await.reply.react.as_deref(), Some("👍"));
    }

    #[tokio::test]
    async fn tool_errors_go_back_to_the_model() {
        let (agent, script, _) = scripted(vec![call("sql_query", json!({"sql": "DELETE FROM agent_note"})), json!({"content": "Done."})]);
        let outcome = agent.turn(input()).await;
        assert_eq!(outcome.reply.text.as_deref(), Some("Done."));
        assert_eq!(script.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn identical_calls_are_blocked_then_the_model_must_answer() {
        let same = || call("sql_query", json!({"sql": "SELECT 1"}));
        let (agent, script, _) = scripted(vec![same(), same(), same(), json!({"content": "Couldn't do that."})]);
        let outcome = agent.turn(input()).await;
        assert!(!outcome.failed);
        assert_eq!(outcome.reply.text.as_deref(), Some("Couldn't do that."));
        assert_eq!(script.seen.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn step_cap_forces_an_answer() {
        let calls: Vec<Value> = (0..20).map(|i| call("sql_query", json!({"sql": format!("SELECT {i}")}))).collect();
        let (agent, script, _) = scripted(calls);
        let outcome = agent.turn(input()).await;
        let seen = script.seen.lock().unwrap();
        assert_eq!(seen.len(), STEP_CAP + 1);
        assert!(seen.last().unwrap().1, "the last call forbids tools");
        // The script's 13th message still has tool calls; with tools forbidden its text is the answer.
        assert_eq!(outcome.reply.text.as_deref(), Some("Working on it…"));
    }
}
