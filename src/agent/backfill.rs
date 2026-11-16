//! `gymclaw backfill`: one record per past agent turn, built offline from a local copy of the turn log
//! and the DB, with outcome labels inferred without owner input.
//!
//! Turns the turn log captured exactly (`model_call` with `messages`) are indexed as `source: capture`,
//! `fidelity: exact`; their trajectory stays in the log. Older turns are rebuilt as `source: backfill`
//! from what's left, at a stated fidelity:
//! - `events+chat`: turn-log events (previews) with full texts from `chat_message`;
//! - `events`: turn-log events only;
//! - `chat`: no turn-log events, only the chat (replies carry their `run`).
//!
//! Mutations come from `agent_event` by request ID (`tg-<message id>-<n>`), the release from git
//! history (the newest first-parent commit before the turn: merge time, not deploy time). Exact past
//! system prompts and reasoning are not recoverable.
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{AgentEvent, ChatMessage, ChatRole};
use crate::error::{Error, Result};

/// How soon after a reply the owner's next message counts as a reaction to it.
const FOLLOW_UP: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

static CORRECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\W*(no\b|nope|wrong|that'?s (not|wrong)|not what|i said|i meant|why did you|you didn'?t|actually\b)",
    )
    .unwrap()
});
static UNDO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(undo|revert|put it back|change it back|move it back|cancel that)\b").unwrap());

/// First-parent commits of `repo`'s HEAD, oldest first, as (commit time, sha).
pub fn releases(repo: &Path) -> Result<Vec<(jiff::Timestamp, String)>> {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["log", "--first-parent", "--reverse", "--format=%cI %H"])
        .output()?;
    if !out.status.success() {
        crate::bail!("GIT_FAILED", "{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (at, sha) = line.split_once(' ')?;
            Some((at.parse().ok()?, sha.to_string()))
        })
        .collect())
}

/// One turn being assembled.
#[derive(Default)]
struct Turn {
    ts: Option<String>,
    trigger: Option<Value>,
    model: Option<Value>,
    commit: Option<Value>,
    inbound: Vec<i64>,
    events: Vec<Value>,
    exact: bool,
}

/// Index every turn in `events` (a turn-log dir, `turnlog::read`) and `db`, oldest first.
pub fn build(db: &Connection, events: &[Value], releases: &[(jiff::Timestamp, String)]) -> Result<Vec<Value>> {
    let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    let mut waiting: Vec<i64> = vec![];
    for event in events {
        if event["ev"] == "inbound" {
            waiting.extend(event["msg"].as_str().and_then(|m| m.parse::<i64>().ok()));
            continue;
        }
        let Some(run) = event["run"].as_str() else { continue };
        let turn = turns.entry(run.to_string()).or_insert_with(|| {
            order.push(run.to_string());
            Turn::default()
        });
        if event["ev"] == "llm_input" {
            turn.ts = event["ts"].as_str().map(String::from);
            turn.trigger = Some(event["trigger"].clone());
            turn.model = Some(event["model"].clone());
            turn.commit = event.get("commit").cloned();
            if event["trigger"] == "user" {
                turn.inbound = std::mem::take(&mut waiting);
            }
        }
        turn.exact |= event["ev"] == "model_call" && event.get("messages").is_some();
        turn.events.push(event.clone());
    }

    let chat = ChatMessage::select(db, "ORDER BY id", [])?;
    let mut records = vec![];
    let mut seen = HashSet::new();
    for run in &order {
        let turn = &turns[run];
        seen.insert(run.clone());
        let reply = chat.iter().position(|m| m.role != ChatRole::Owner && m.meta_json["run"] == run.as_str());
        let inbound: Vec<&ChatMessage> = turn
            .inbound
            .iter()
            .filter_map(|id| chat.iter().find(|m| m.role == ChatRole::Owner && m.telegram_message_id == Some(*id)))
            .collect();
        let fidelity = match (turn.exact, reply.is_some() || !inbound.is_empty()) {
            (true, _) => "exact",
            (false, true) => "events+chat",
            (false, false) => "events",
        };
        let ts = turn.ts.clone().or_else(|| turn.events.first().and_then(|e| e["ts"].as_str().map(String::from)));
        let mut record = json!({
            "source": if turn.exact { "capture" } else { "backfill" }, "fidelity": fidelity, "run": run, "ts": ts,
            "trigger": turn.trigger, "model": turn.model,
        });
        record["release"] = match &turn.commit {
            Some(commit) => json!({"commit": commit, "fidelity": "exact"}),
            None => release(releases, ts.as_deref()),
        };
        record["labels"] = json!(labels(&turn.events, &chat, reply, &inbound));
        if !turn.exact {
            fill(db, &mut record, &chat, reply, &inbound)?;
            record["events"] = json!(turn.events);
        }
        records.push(record);
    }

    // Replies from before the turn log (or with their log lost): the chat alone.
    for (at, message) in chat.iter().enumerate() {
        let Some(run) = message.meta_json["run"].as_str() else { continue };
        if message.role == ChatRole::Owner || !seen.insert(run.to_string()) {
            continue;
        }
        let since = chat[..at].iter().rposition(|m| m.role != ChatRole::Owner).map_or(0, |i| i + 1);
        let inbound: Vec<&ChatMessage> = chat[since..at].iter().filter(|m| m.role == ChatRole::Owner).collect();
        let ts = message.at.0.to_string();
        let mut record = json!({"source": "backfill", "fidelity": "chat", "run": run, "ts": ts,
            "trigger": if inbound.is_empty() { "event" } else { "user" }, "release": release(releases, Some(&ts))});
        record["labels"] = json!(labels(&[], &chat, Some(at), &inbound));
        fill(db, &mut record, &chat, Some(at), &inbound)?;
        records.push(record);
    }
    records.sort_by(|a, b| a["ts"].as_str().cmp(&b["ts"].as_str()));
    Ok(records)
}

/// The release that served a turn at `ts`: the newest commit before it.
fn release(releases: &[(jiff::Timestamp, String)], ts: Option<&str>) -> Value {
    let Some(at) = ts.and_then(|t| t.parse::<jiff::Timestamp>().ok()) else { return Value::Null };
    match releases.iter().rev().find(|(time, _)| *time <= at) {
        Some((_, sha)) => json!({"commit": sha, "fidelity": "merge-time"}),
        None => Value::Null,
    }
}

/// Full texts from the chat and the mutations the turn's tool calls made.
fn fill(
    db: &Connection,
    record: &mut Value,
    chat: &[ChatMessage],
    reply: Option<usize>,
    inbound: &[&ChatMessage],
) -> Result<()> {
    record["inbound"] =
        json!(inbound.iter().map(|m| json!({"msg": m.telegram_message_id, "text": m.text})).collect::<Vec<_>>());
    if let Some(reply) = reply.map(|i| &chat[i]) {
        record["reply"] = json!(reply.text);
        record["actions"] = reply.meta_json["actions"].clone();
    }
    // The runtime keys a turn's tool calls by its newest owner message: `tg-<id>-<n>`.
    if let Some(id) = inbound.last().and_then(|m| m.telegram_message_id) {
        let mutations =
            AgentEvent::select(db, "WHERE correlation_id LIKE ?1 ORDER BY created_at", [format!("tg-{id}-%")])?;
        record["mutations"] = json!(
            mutations
                .iter()
                .map(|e| json!({"request_id": e.correlation_id, "at": e.created_at.0.to_string(),
                    "request": e.payload_json["request"], "result": e.payload_json["result"]}))
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

/// Outcome labels: failures and withheld tools from the log, the owner's reactions, and what the
/// owner said next (a correction, an undo, or the same ask again) within `FOLLOW_UP` of the reply.
fn labels(events: &[Value], chat: &[ChatMessage], reply: Option<usize>, inbound: &[&ChatMessage]) -> Vec<Value> {
    let mut out = vec![];
    for event in events {
        match event["ev"].as_str() {
            Some("run_end") => {
                let flags: Vec<String> = match event["flags"].as_array() {
                    Some(flags) => flags.iter().filter_map(|f| f.as_str().map(String::from)).collect(),
                    // Older lines: no flags, only ok and the error.
                    None if event["ok"] == false => {
                        let error = event["error"].as_str().unwrap_or_default();
                        vec![
                            if error.contains("deadline") {
                                "deadline"
                            } else if error.starts_with("STUCK") {
                                "step_cap"
                            } else {
                                "failed"
                            }
                            .into(),
                        ]
                    }
                    None => vec![],
                };
                out.extend(flags.into_iter().map(|f| json!({"label": f, "source": "log"})));
            }
            Some("label") if event["label"].is_string() => {
                out.push(json!({"label": event["label"], "source": "reaction"}))
            }
            _ => {}
        }
    }
    let Some(reply) = reply else { return out };
    let Some(next) = chat[reply + 1..].iter().find(|m| m.role == ChatRole::Owner) else { return out };
    if next.at.0.duration_since(chat[reply].at.0) > FOLLOW_UP {
        return out;
    }
    if UNDO.is_match(&next.text) {
        out.push(json!({"label": "undone", "source": "inferred", "by": next.text}));
    } else if CORRECTION.is_match(&next.text) {
        out.push(json!({"label": "corrected", "source": "inferred", "by": next.text}));
    } else if inbound.last().is_some_and(|ask| similar(&ask.text, &next.text)) {
        out.push(json!({"label": "repeat_ask", "source": "inferred", "by": next.text}));
    }
    out
}

/// Same request in other words: most words shared (Jaccard ≥ 0.7, at least two words each).
fn similar(a: &str, b: &str) -> bool {
    let words = |text: &str| -> HashSet<String> {
        text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
    };
    let (a, b) = (words(a), words(b));
    if a.len() < 2 || b.len() < 2 {
        return false;
    }
    a.intersection(&b).count() as f64 / a.union(&b).count() as f64 >= 0.7
}

/// Write `records` as JSONL, owner-only: they hold the chat.
pub fn write(out: &Path, records: &[Value]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(out).map_err(|e| Error::domain("WRITE_FAILED", format!("{}: {e}", out.display())))?;
    for record in records {
        writeln!(file, "{record}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Json;
    use crate::time::Ts;

    fn chat(db: &Connection, role: ChatRole, at: &str, text: &str, msg: Option<i64>, meta: Value) {
        ChatMessage {
            id: None,
            at: Ts::parse(at, None).unwrap(),
            role,
            text: text.into(),
            telegram_message_id: msg,
            meta_json: Json(meta),
        }
        .insert(db)
        .unwrap();
    }

    /// An old logged turn gets the chat's full texts, its mutations and an inferred correction; an exact
    /// capture is only indexed; a reply with no log at all is rebuilt from the chat.
    #[test]
    fn rebuilds_old_turns_and_marks_their_fidelity() {
        let db = crate::db::test::db();
        chat(&db, ChatRole::Owner, "2026-10-01T10:00:00Z", "Move Pull to the evening, after work", Some(5), json!({}));
        chat(
            &db,
            ChatRole::Assistant,
            "2026-10-01T10:00:30Z",
            "Pull is at 18:00.",
            Some(6),
            json!({"kind": "reply", "run": "old"}),
        );
        chat(&db, ChatRole::Owner, "2026-10-01T10:02:00Z", "no, 19:00", Some(7), json!({}));
        chat(&db, ChatRole::Owner, "2026-09-01T10:00:00Z", "hi", Some(1), json!({}));
        chat(
            &db,
            ChatRole::Assistant,
            "2026-09-01T10:00:05Z",
            "Hi!",
            Some(2),
            json!({"kind": "reply", "run": "chat-only"}),
        );
        db.execute(
            "INSERT INTO agent_event (id, type, created_at, payload_json, correlation_id) VALUES ('e1', 'session_update', '2026-10-01 10:00:20', '{\"request\": {\"operation\": \"session_update\"}, \"result\": {}}', 'tg-5-1')",
            [],
        )
        .unwrap();
        let events = vec![
            json!({"ts": "2026-10-01T10:00:01.000Z", "ev": "inbound", "msg": "5", "text": "Move Pull…"}),
            json!({"ts": "2026-10-01T10:00:02.000Z", "ev": "llm_input", "run": "old", "trigger": "user", "model": "m"}),
            json!({"ts": "2026-10-01T10:00:29.000Z", "ev": "run_end", "run": "old", "ok": true}),
            json!({"ts": "2026-11-01T10:00:02.000Z", "ev": "llm_input", "run": "new", "trigger": "event", "model": "m", "commit": "abc"}),
            json!({"ts": "2026-11-01T10:00:03.000Z", "ev": "model_call", "run": "new", "messages": []}),
            json!({"ts": "2026-11-01T10:00:04.000Z", "ev": "run_end", "run": "new", "ok": false, "flags": ["step_cap", "failed"]}),
        ];
        let releases = [("2026-09-30T00:00:00Z".parse().unwrap(), "r1".to_string())];
        let records = build(&db, &events, &releases).unwrap();
        let runs: Vec<(&str, &str, &str)> = records
            .iter()
            .map(|r| (r["run"].as_str().unwrap(), r["source"].as_str().unwrap(), r["fidelity"].as_str().unwrap()))
            .collect();
        assert_eq!(
            runs,
            [("chat-only", "backfill", "chat"), ("old", "backfill", "events+chat"), ("new", "capture", "exact")]
        );
        let old = &records[1];
        assert_eq!(old["inbound"][0]["text"], "Move Pull to the evening, after work");
        assert_eq!(old["mutations"][0]["request_id"], "tg-5-1");
        assert_eq!(old["release"], json!({"commit": "r1", "fidelity": "merge-time"}));
        assert_eq!(old["labels"], json!([{"label": "corrected", "source": "inferred", "by": "no, 19:00"}]));
        assert!(records[0]["release"].is_null(), "before the first release");
        assert_eq!(records[2]["labels"].as_array().unwrap().len(), 2);
        assert!(records[2].get("events").is_none(), "an exact capture stays in the turn log");
    }

    #[test]
    fn repeat_asks_and_undos() {
        assert!(similar("move pull to the evening", "Move Pull to the evening please"));
        assert!(!similar("move pull to the evening", "thanks"));
        assert!(UNDO.is_match("ok undo that") && !CORRECTION.is_match("nothing else"));
    }
}
