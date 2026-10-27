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
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use regex::Regex;
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

/// A message to the owner. Buttons hang under its text, so there are no buttons without text.
#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema)]
pub struct Message {
    /// What the owner reads (Markdown: **bold**, `code`). Introduces the buttons when there are any.
    pub text: String,
    /// Choices as buttons under the text, one row each (at most 8).
    #[serde(default)]
    pub buttons: Vec<ReplyButton>,
}

/// The `reply` tool: how a turn ends. Unknown fields are an error, so a misplaced `text` or
/// `buttons` goes back to the model instead of becoming silence.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    /// The message to send. Omit to send none.
    pub message: Option<Message>,
    /// One emoji reaction on the owner's message instead of (or with) a message, e.g. 👍 or 👌.
    pub react: Option<String>,
}

impl Reply {
    fn text(text: String) -> Self {
        Reply { message: Some(Message { text, buttons: vec![] }), react: None }
    }
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
    /// Woken by background events, not by the owner: notes to itself ("nothing to report") are dropped.
    pub background: bool,
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

/// Start of tool-call markup models write as text: `<tool_call>`, `<|tool_call|>`, `<function_call>`,
/// `<tool_calls>`, `<function=workout>`.
static CALL_MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<\|?(tool_calls?|function_calls?)\b|<function=").unwrap());

/// A tool call written as text instead of made (markup, or the whole message as a `{"name", "arguments"}`
/// object). Returns the text before it, or None when there is none.
fn written_tool_call(text: &str) -> Option<&str> {
    if let Some(found) = CALL_MARKUP.find(text) {
        return Some(text[..found.start()].trim());
    }
    let json: Value = serde_json::from_str(text.trim()).ok()?;
    (json.get("name").is_some() && (json.get("arguments").is_some() || json.get("parameters").is_some())).then_some("")
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

/// History prefix the model sometimes copies onto its own reply ("[Tue 14:22, sent automatically] …").
static ECHOED_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[[^\]\n]*sent automatically\]\s*").unwrap());

/// The whole reply is a note that there's nothing to say: "No reply needed.", "Nothing to report",
/// "(background events handled — nothing to report)".
static NOTHING_TO_SAY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\(?([^()]*\s[—–-]\s*)?(heartbeat_ok|no (reply|response|message|action) (is )?(needed|required|necessary)|nothing (to (report|say|do|add)|(needs|requires) (saying|attention|action)))[.!]?\)?$").unwrap()
});

/// The text worth sending, if any: "NO_REPLY" and text without a word in it ("", "...", "…") mean
/// silence. On a background turn so does a note to itself ("No reply needed."); to the owner the same
/// words can be a real answer.
fn clean_text(text: &str, background: bool) -> Option<String> {
    let text = ECHOED_PREFIX.replace(text.trim(), "").trim().to_string();
    let wordless = text.chars().all(|c| c.is_whitespace() || c.is_ascii_punctuation() || c == '…');
    (!wordless && text != "NO_REPLY" && !(background && NOTHING_TO_SAY.is_match(&text))).then_some(text)
}

impl Agent {
    /// Run one tool call in its own transaction. Returns the JSON the model sees, whether it succeeded
    /// and whether it changed the database.
    async fn tool(&self, name: &str, args: Value, request_id: String) -> (Value, bool, bool) {
        let now = (self.clock)();
        let data_dir = self.data_dir.clone();
        let name_owned = name.to_string();
        let result = blocking(&self.db_path, move |db| {
            let tx = db.transaction()?;
            let before = tx.total_changes();
            let mut cx = tools::Ctx { db: &tx, now, request_id, data_dir: &data_dir, effects: vec![] };
            let value = tools::call(&mut cx, &name_owned, args)?;
            let effects = std::mem::take(&mut cx.effects);
            let wrote = tx.total_changes() > before;
            tx.commit()?;
            Ok((value, effects, wrote))
        })
        .await;
        match result {
            Ok((value, effects, wrote)) => {
                for Effect::Coach(view) in effects {
                    if let Some(coach) = &self.coach
                        && let Err(e) = coach.apply(&view, None, None).await
                    {
                        tracing::warn!("coach card from {name} not shown: {e}");
                    }
                }
                (value, true, wrote)
            }
            Err(e) => (e.to_json(), false, false),
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
                (Reply::text(format_failure(&state)), true, Some(state.error.clone()))
            }
            Err(_) => {
                state.error = "turn deadline exceeded".into();
                (Reply::text(format_failure(&state)), true, Some(state.error.clone()))
            }
        };
        let text = reply.message.as_ref().map(|m| m.text.as_str()).unwrap_or_default();
        self.log.log(json!({
            "ev": "run_end", "run": run_id, "session": "telegram:owner", "trigger": "user", "ok": !failed,
            "error": error.map(|e| scrub(&e, 500)), "diagnosis": failed.then(|| diagnose(&state)), "ms": started.elapsed().as_millis() as u64, "calls": calls_log,
            "noReply": (reply.message.is_none() && reply.react.is_none()).then_some(true), "reply": scrub(text, 4000),
        }));
        Outcome { reply, failed, run_id }
    }

    async fn run(&self, input: &TurnInput, run_id: &str, state: &mut FailureState, calls_log: &mut Vec<Value>) -> Result<Reply> {
        let now = (self.clock)();
        let (system, history, tz) =
            blocking(&self.db_path, move |db| Ok((context::system(db, now)?, context::history(db)?, crate::domain::profile::get_profile(db)?.tz()))).await?;
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
        // Calls made this turn: how often, and the error of the first run if it failed.
        let mut seen: HashMap<String, (u32, Option<String>)> = HashMap::new();
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
                let written = content.and_then(written_tool_call);
                if written.is_some() && !force_text {
                    // It meant to call a tool: nothing ran, so say so and let it call properly.
                    steps += 1;
                    messages.push(json!({"role": "assistant", "content": content}));
                    messages.push(json!({"role": "user", "content": "[system] That tool call was written as text, so it did not run and the owner would have seen it. Call tools as function calls, or answer the owner."}));
                    continue;
                }
                let content = written.or(content);
                if force_text && content.is_none_or(|c| c.trim().is_empty()) {
                    crate::bail!("STUCK", "step limit reached without an answer");
                }
                return Ok(content.and_then(|c| clean_text(c, input.background)).map_or_else(Reply::default, Reply::text));
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
                let (mut result, ok) = match (name.as_str(), args) {
                    (_, Err(e)) => (tools::guide(&name, Error::domain("INVALID_JSON", format!("arguments are not JSON: {e}"))).to_json(), false),
                    (REPLY, Ok(args)) => match serde_json::from_value::<Reply>(args) {
                        // The call it meant didn't run; the owner must not get its markup or a silent reply.
                        Ok(reply) if reply.message.as_ref().and_then(|m| written_tool_call(&m.text)).is_some() => {
                            // Counts as a step, so a model that keeps doing it is forced to answer in text.
                            steps += 1;
                            (Error::domain("WRITTEN_TOOL_CALL", "The reply text contains a tool call written as text; it did not run. Make it as a function call, then reply.").to_json(), false)
                        }
                        Ok(reply) => {
                            finished = Some(reply);
                            (json!({"ok": true}), true)
                        }
                        Err(e) => (Error::domain("INVALID_ARGUMENTS", format!("reply: {e}")).to_json(), false),
                    },
                    (_, Ok(args)) => {
                        let key = format!("{name} {args}");
                        let (repeats, failed) = seen.entry(key.clone()).or_default();
                        *repeats += 1;
                        if *repeats >= 2 {
                            looping |= *repeats >= 3;
                            let message = match failed {
                                Some(error) => format!("This exact call already failed and will fail again: {error}\nChange the arguments as described, or tell the owner what's blocking."),
                                None => "Same call with the same arguments as before; the result won't change. Use the earlier result, try something else, or reply.".into(),
                            };
                            (Error::domain("REPEATED_CALL", message).to_json(), false)
                        } else {
                            steps += 1;
                            index += 1;
                            state.tool_calls += 1;
                            let (result, ok, wrote) = self.tool(&name, args, format!("{}-{index}", input.key)).await;
                            if !ok && let Some(entry) = seen.get_mut(&key) {
                                entry.1 = result.pointer("/error/message").and_then(Value::as_str).map(str::to_string);
                            }
                            // State changed, so other earlier calls may now give a different answer.
                            if wrote {
                                seen.retain(|k, _| *k == key);
                            }
                            (result, ok)
                        }
                    }
                };
                crate::time::localize(&mut result, &tz);
                self.log_tool(run_id, &id, &name, &serde_json::from_str(raw).unwrap_or(Value::Null), &result, ok, started);
                messages.push(json!({"role": "tool", "tool_call_id": id, "content": cut(result.to_string())}));
            }
            if let Some(mut reply) = finished {
                // Silence wins over buttons: "NO_REPLY" with buttons sends nothing.
                reply.message = reply.message.and_then(|mut m| {
                    m.text = clean_text(&m.text, input.background)?;
                    m.buttons.truncate(8);
                    Some(m)
                });
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
        /// Messages of the latest call.
        last: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl ChatModel for Script {
        async fn complete(&self, _: &str, messages: &[Value], _: &[Value], force_text: bool) -> Result<ModelReply> {
            self.seen.lock().unwrap().push((messages.len(), force_text));
            *self.last.lock().unwrap() = messages.to_vec();
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
        let script = Arc::new(Script { replies: Mutex::new(replies), seen: Mutex::new(vec![]), last: Mutex::new(vec![]) });
        let agent = Agent {
            db_path, data_dir: dir.clone(), model: script.clone(), chat_model: "m".into(), vision_model: "v".into(), coach: None,
            log: TurnLog::default(), clock: Arc::new(move || now),
        };
        (agent, script, dir)
    }

    fn input() -> TurnInput {
        TurnInput { key: "tg-5".into(), images: vec![], background: false }
    }

    fn background() -> TurnInput {
        TurnInput { key: "event-1".into(), images: vec![], background: true }
    }

    #[tokio::test]
    async fn tool_then_one_reply_with_buttons_and_narration_dropped() {
        let reply = call("reply", json!({"message": {"text": "Noted.", "buttons": [{"label": "Undo", "value": "Forget the lunges note"}]}}));
        let (agent, script, dir) = scripted(vec![call("note_add", json!({"kind": "owner", "text": "Hates lunges"})), reply]);
        let outcome = agent.turn(input()).await;
        assert!(!outcome.failed);
        let message = outcome.reply.message.unwrap();
        assert_eq!(message.text, "Noted.");
        assert_eq!(message.buttons[0].value, "Forget the lunges note");
        let db = crate::db::open(&dir.join("gymclaw.db")).unwrap();
        assert_eq!(AgentNote::select(&db, "", []).unwrap()[0].text, "Hates lunges");
        // The note is in the next turn's prompt.
        assert!(context::system(&db, Ts::now()).unwrap().contains("- [1] Hates lunges"));
        assert_eq!(script.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn plain_text_is_the_reply_and_no_reply_is_silence() {
        let (agent, _, _) = scripted(vec![json!({"content": "  **Mon** Pull  "})]);
        assert_eq!(agent.turn(input()).await.reply.message.map(|m| m.text).as_deref(), Some("**Mon** Pull"));
        for silence in ["NO_REPLY", "...", "…"] {
            let (agent, _, _) = scripted(vec![json!({"content": silence})]);
            assert_eq!(agent.turn(input()).await.reply, Reply::default(), "{silence}");
        }
        // An echoed history prefix is never part of the message.
        let (agent, _, _) = scripted(vec![json!({"content": "[Tue 14:22, sent automatically] Pull moved to **13:30**."})]);
        assert_eq!(agent.turn(input()).await.reply.message.map(|m| m.text).as_deref(), Some("Pull moved to **13:30**."));
        // After background events, notes to itself are silence; to the owner the same words can be answers.
        let notes = ["[Tue 14:22, sent automatically] (background events handled — nothing to report)", "No reply needed.", "Nothing to report", "nothing needs attention."];
        for note in notes {
            let (agent, _, _) = scripted(vec![json!({"content": note})]);
            assert_eq!(agent.turn(background()).await.reply, Reply::default(), "{note}");
        }
        for answer in ["No action needed.", "(Pull moved to 13:30)"] {
            let (agent, _, _) = scripted(vec![json!({"content": answer})]);
            assert_eq!(agent.turn(input()).await.reply.message.map(|m| m.text).as_deref(), Some(answer));
        }
        for message in ["Nothing to report on the crowd yet: no data for Fridays.", "(Pull moved to 13:30)", "Wed clash — move Pull?"] {
            let (agent, _, _) = scripted(vec![json!({"content": message})]);
            assert_eq!(agent.turn(background()).await.reply.message.map(|m| m.text).as_deref(), Some(message));
        }
        let (agent, _, _) = scripted(vec![call("reply", json!({"react": "👍"}))]);
        assert_eq!(agent.turn(input()).await.reply.react.as_deref(), Some("👍"));
    }

    #[tokio::test]
    async fn tool_errors_go_back_to_the_model() {
        let (agent, script, _) = scripted(vec![call("sql_query", json!({"sql": "DELETE FROM agent_note"})), json!({"content": "Done."})]);
        let outcome = agent.turn(input()).await;
        assert_eq!(outcome.reply.message.map(|m| m.text).as_deref(), Some("Done."));
        assert_eq!(script.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn identical_calls_are_blocked_then_the_model_must_answer() {
        let same = || call("sql_query", json!({"sql": "SELECT 1"}));
        let (agent, script, _) = scripted(vec![same(), same(), same(), json!({"content": "Couldn't do that."})]);
        let outcome = agent.turn(input()).await;
        assert!(!outcome.failed);
        assert_eq!(outcome.reply.message.map(|m| m.text).as_deref(), Some("Couldn't do that."));
        assert_eq!(script.seen.lock().unwrap().len(), 4);
    }

    /// Prod 2026-10-22: a call missing required fields was repeated until STUCK. Both the error and
    /// its repeat must say how to call the tool.
    #[tokio::test]
    async fn wrong_arguments_come_back_with_usage_even_when_repeated() {
        let wrong = || call("session_create", json!({"fields": {"end": "2099-10-25T10:00"}}));
        let (agent, script, _) = scripted(vec![wrong(), wrong(), json!({"content": "Which workout?"})]);
        assert_eq!(agent.turn(input()).await.reply.message.map(|m| m.text).as_deref(), Some("Which workout?"));
        let last = script.last.lock().unwrap();
        let errors: Vec<&str> = last.iter().filter(|m| m["role"] == "tool").filter_map(|m| m["content"].as_str()).collect();
        assert!(errors[0].contains("start (string, required)"), "{}", errors[0]);
        assert!(errors[1].contains("REPEATED_CALL") && errors[1].contains("already failed") && errors[1].contains("template_id (string, required)"), "{}", errors[1]);
    }

    #[tokio::test]
    async fn a_write_lets_earlier_calls_run_again() {
        let check = || call("sql_query", json!({"sql": "SELECT count(*) AS n FROM agent_note"}));
        let (agent, script, _) = scripted(vec![check(), call("note_add", json!({"kind": "owner", "text": "Hates lunges"})), check(), json!({"content": "Saved."})]);
        assert_eq!(agent.turn(input()).await.reply.message.map(|m| m.text).as_deref(), Some("Saved."));
        let last = script.last.lock().unwrap();
        let rerun = last.last().unwrap()["content"].as_str().unwrap();
        assert!(rerun.contains("[[1]]"), "{rerun}");
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
        assert_eq!(outcome.reply.message.map(|m| m.text).as_deref(), Some("Working on it…"));
    }

    #[tokio::test]
    async fn tool_call_written_as_text_never_reaches_the_owner() {
        let written = || json!({"content": "<tool_call>workout<arg_key>action</arg_key><arg_value>start</arg_value></tool_call>"});
        let (agent, script, _) = scripted(vec![written(), json!({"content": "Started."})]);
        assert_eq!(agent.turn(input()).await.reply.message.map(|m| m.text).as_deref(), Some("Started."));
        assert_eq!(script.seen.lock().unwrap().len(), 2);
        // Forced to answer, it can't retry: an honest failure beats leaking the markup.
        let calls: Vec<Value> = (0..STEP_CAP).map(|i| call("sql_query", json!({"sql": format!("SELECT {i}")}))).chain([written()]).collect();
        let (agent, _, _) = scripted(calls);
        let outcome = agent.turn(input()).await;
        assert!(outcome.failed && !outcome.reply.message.unwrap().text.contains("<tool_call>"));
    }

    #[test]
    fn written_tool_calls_in_other_formats() {
        for text in ["<|tool_call|>workout", "Starting.\n<function_call>workout</function_call>", "<function=workout>{}</function>", r#"{"name": "workout", "arguments": {"action": "start"}}"#] {
            assert!(written_tool_call(text).is_some(), "{text}");
        }
        for text in ["Use the function of the hips.", r#"{"name": "Bench"}"#, "**Pull** at 13:30"] {
            assert_eq!(written_tool_call(text), None, "{text}");
        }
    }

    #[tokio::test]
    async fn buttons_outside_a_message_go_back_to_the_model() {
        let buttons = json!([{"label": "10:30", "value": "Move Sunday Pull to 10:30"}]);
        let (agent, script, _) = scripted(vec![call("reply", json!({"buttons": buttons})), call("reply", json!({"message": {"text": "Pick one:", "buttons": buttons}}))]);
        let message = agent.turn(input()).await.reply.message.unwrap();
        assert_eq!((message.text.as_str(), message.buttons.len()), ("Pick one:", 1));
        assert_eq!(script.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reply_tool_with_a_written_call_goes_back_to_the_model() {
        let written = || call("reply", json!({"message": {"text": "<tool_call>workout</tool_call>"}}));
        let (agent, script, _) = scripted(vec![written(), json!({"content": "Started."})]);
        assert_eq!(agent.turn(input()).await.reply.message.map(|m| m.text).as_deref(), Some("Started."));
        assert_eq!(script.seen.lock().unwrap().len(), 2);
        // Doing it over and over still ends at the step cap with a forced text answer.
        let (agent, script, _) = scripted((0..20).map(|_| written()).collect());
        agent.turn(input()).await;
        let seen = script.seen.lock().unwrap();
        assert!(seen.len() == STEP_CAP + 1 && seen.last().unwrap().1, "{seen:?}");
    }
}
