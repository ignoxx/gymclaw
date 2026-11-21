//! Append-only JSONL turn log: enough to replay a turn exactly, trace a root cause across turns and
//! build training sets.
//!
//! One line per event in `<dir>/YYYY-MM-DD.jsonl` (UTC date). Big parts that rarely change (the stable
//! system prompt, the tool specs) are stored once as `<dir>/blobs/<sha>.json` and referenced by hash.
//! Files are owner-only (dir 0700, files 0600): full prompts hold chat, notes and calendar data.
//! `log` never blocks or fails a turn: lines go through a bounded queue to a writer task; when it backs
//! up, new lines are dropped.
//!
//! Events (`ev`), each with `ts`; a turn's events share `run`:
//! - `inbound`: an owner message queued for the agent; `voice`: a voice note's transcript.
//! - `llm_input`: the run stamp: `commit` (baked in at build), `model`, `stableBlob`, `toolsBlob`.
//! - `model_call`: one model step. `messages` are the ones added since the previous step, starting at
//!   index `from` (the first step's stable prompt is `{"blob": sha}`; its "Now" snapshot is inline),
//!   plus the request `settings`, `forced` (`step_cap`/`breaker` when tools were withheld) and the
//!   response `message` (text, tool calls, reasoning) with OpenRouter's `generationId` for cost.
//! - `llm_output`: that step's usage and stop reason.
//! - `tool`: one tool call; `changes` holds the rows it changed and the SQLite changeset with its
//!   `inverse` (base64), which `gymclaw rewind` applies.
//! - `run_end`: outcome, reply, `flags` (`failed`, `deadline`, `step_cap`, `breaker`).
//! - `outbound`: the reply as sent. `label`: the owner's reaction to a message (`good`/`bad`).
//! - `fast`: coach taps and typed sets (no model).
//!
//! Strings are scrubbed of secrets everywhere; only previews are cut.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;
use rusqlite::fallible_streaming_iterator::FallibleStreamingIterator;
use rusqlite::hooks::Action;
use rusqlite::session::{ChangesetIter, ConflictAction, Session};
use rusqlite::types::ValueRef;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::db::Connection;
use crate::error::{Error, Result};

const MAX_PENDING: usize = 1000;

/// Git commit this binary was built from (`GYMCLAW_COMMIT` at build time, see deploy/Dockerfile).
pub const COMMIT: &str = match option_env!("GYMCLAW_COMMIT") {
    Some(commit) => commit,
    None => "dev",
};

static SECRET_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)token|secret|password|passwd|api.?key|authorization|cookie|credential").unwrap());
static SECRET_VALUE: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        Regex::new(r"(?i)\bBearer\s+[\w.~+/=-]+").unwrap(),
        Regex::new(r"\bsk-[\w-]{16,}").unwrap(),
        // Match both bare Telegram bot tokens and `api.telegram.org/bot<id>:<secret>/...` URLs.
        Regex::new(r"(?:\bbot|\b)\d{6,12}:[\w-]{30,}").unwrap(),
        // Private ICS calendar feed bearer URLs (`/calendar/<hex-token>.ics`).
        Regex::new(r"/calendar/[0-9a-fA-F]{16,}\.ics").unwrap(),
    ]
});
// Inline media: data URLs and long unbroken base64 runs are stored as size + hash only.
static BLOB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^data:[^,]*,|^[A-Za-z0-9+/=\r\n]{2000,}$").unwrap());

pub fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))[..12].to_string()
}

/// Redact secrets and cap length; the cut is marked with how much was dropped.
pub fn scrub(text: &str, max: usize) -> String {
    let mut value = text.to_string();
    for pattern in SECRET_VALUE.iter() {
        value = pattern.replace_all(&value, "[redacted]").into_owned();
    }
    let count = value.chars().count();
    if count > max { format!("{}…[+{}]", value.chars().take(max).collect::<String>(), count - max) } else { value }
}

/// JSON-safe preview of tool args: secret keys redacted, strings scrubbed and cut, blobs → {bytes, sha}.
pub fn clean(value: &Value, max: usize, depth: usize) -> Value {
    copy(value, Some(max), depth)
}

/// Full copy for trajectories: like `clean`, but nothing is cut, so the model's exact input survives.
pub fn redact(value: &Value) -> Value {
    copy(value, None, 0)
}

/// `cap` = (string length, nesting depth, 50 items) limits for previews; None copies everything.
fn copy(value: &Value, cap: Option<usize>, depth: usize) -> Value {
    match value {
        Value::String(s) if BLOB.is_match(s) => json!({"bytes": s.len(), "sha": sha(s)}),
        Value::String(s) => Value::String(scrub(s, cap.unwrap_or(usize::MAX))),
        Value::Array(_) | Value::Object(_) if cap.is_some() && depth > 4 => json!("[deep]"),
        Value::Array(items) => Value::Array(
            items.iter().take(if cap.is_some() { 50 } else { usize::MAX }).map(|v| copy(v, cap, depth + 1)).collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (k.clone(), if SECRET_KEY.is_match(k) { json!("[redacted]") } else { copy(v, cap, depth + 1) })
                })
                .collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}

enum Entry {
    Line { day: String, text: String },
    Blob { sha: String, text: String },
}

#[derive(Clone, Default)]
pub struct TurnLog {
    tx: Option<mpsc::Sender<Entry>>,
}

impl TurnLog {
    /// `None` = disabled (no `GYMCLAW_TURN_LOG_DIR`). Must be called inside the tokio runtime.
    pub fn new(dir: Option<PathBuf>) -> TurnLog {
        let Some(dir) = dir else { return TurnLog::default() };
        let (tx, mut rx) = mpsc::channel::<Entry>(MAX_PENDING);
        tokio::spawn(async move {
            let blobs = dir.join("blobs");
            let _ = create_dir(&dir).await;
            let _ = create_dir(&blobs).await;
            while let Some(entry) = rx.recv().await {
                let _ = match entry {
                    Entry::Line { day, text } => append(&dir.join(format!("{day}.jsonl")), &text).await,
                    Entry::Blob { sha, text } => store(&blobs.join(format!("{sha}.json")), &text).await,
                };
            }
        });
        TurnLog { tx: Some(tx) }
    }

    /// Queue one event; `ts` is added and null fields are dropped.
    pub fn log(&self, event: Value) {
        let Some(tx) = &self.tx else { return };
        let ts = jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let mut line = Map::new();
        line.insert("ts".into(), json!(ts));
        if let Value::Object(fields) = event {
            line.extend(fields.into_iter().filter(|(_, v)| !v.is_null()));
        }
        let _ = tx.try_send(Entry::Line { day: ts[..10].to_string(), text: format!("{}\n", Value::Object(line)) });
    }

    /// Store `value` (redacted) once under its hash and return the hash, which events reference.
    pub fn blob(&self, value: &Value) -> String {
        let text = redact(value).to_string();
        let sha = sha(&text);
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(Entry::Blob { sha: sha.clone(), text });
        }
        sha
    }
}

async fn create_dir(dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

fn options() -> tokio::fs::OpenOptions {
    let mut options = tokio::fs::OpenOptions::new();
    #[cfg(unix)]
    options.mode(0o600);
    options
}

async fn append(file: &Path, line: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    options().create(true).append(true).open(file).await?.write_all(line.as_bytes()).await
}

/// Write a blob atomically unless it's already there (same hash, same content).
async fn store(file: &Path, text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if tokio::fs::try_exists(file).await.unwrap_or(false) {
        return Ok(());
    }
    let tmp = file.with_extension(format!("json.{}.tmp", crate::db::new_id()));
    let write = async {
        let mut f = options().write(true).create_new(true).open(&tmp).await?;
        f.write_all(text.as_bytes()).await?;
        f.flush().await
    }
    .await;
    if let Err(e) = write {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Err(e) = tokio::fs::rename(&tmp, file).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

/// Every event in a turn-log dir (a local copy), oldest file first; unreadable lines are skipped.
pub fn read(dir: &Path) -> Result<Vec<Value>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut events = vec![];
    for file in files {
        let text = std::fs::read_to_string(&file)?;
        events.extend(text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()));
    }
    Ok(events)
}

/// Start recording the rows `conn` changes (SQLite session extension; secret tables are skipped).
/// Read them with `changes` before the transaction commits.
pub fn record(conn: &Connection) -> Result<Session<'_>> {
    let mut session = Session::new(conn)?;
    session.table_filter(Some(|table: &str| !crate::tools::meta::SECRET_TABLES.contains(&table)));
    session.attach(None::<&str>)?;
    Ok(session)
}

/// What a recording changed: `{rows: [{table, op, pk}], changeset, inverse}` (both base64), or None.
pub fn changes(session: &mut Session) -> Result<Option<Value>> {
    let mut changeset = vec![];
    session.changeset_strm(&mut changeset)?;
    if changeset.is_empty() {
        return Ok(None);
    }
    let mut inverse = vec![];
    rusqlite::session::invert_strm(&mut changeset.as_slice(), &mut inverse)?;
    let mut rows = vec![];
    let mut input = changeset.as_slice();
    let reader: &mut dyn Read = &mut input;
    let mut items = ChangesetIter::start_strm(&reader)?;
    while let Some(item) = items.next()? {
        let op = item.op()?;
        let (table, code) = (op.table_name().to_string(), op.code());
        let mut pk = vec![];
        for (column, _) in item.pk()?.iter().enumerate().filter(|(_, flag)| **flag != 0) {
            let value = if code == Action::SQLITE_INSERT { item.new_value(column)? } else { item.old_value(column)? };
            pk.push(match value {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                ValueRef::Blob(b) => json!({"bytes": b.len()}),
            });
        }
        let op = match code {
            Action::SQLITE_INSERT => "insert",
            Action::SQLITE_DELETE => "delete",
            _ => "update",
        };
        rows.push(json!({"table": table, "op": op, "pk": pk}));
    }
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(Some(json!({"rows": rows, "changeset": b64(&changeset), "inverse": b64(&inverse)})))
}

/// Undo a run's tool calls on `conn` (a DB copy) from their logged inverses, newest first, in one
/// transaction. A row changed since (by a later turn or a tap) aborts it all: rewind later runs first.
/// Returns how many tool calls were undone.
pub fn rewind(conn: &mut Connection, events: &[Value], run: &str) -> Result<usize> {
    if !events.iter().any(|e| e["run"] == run) {
        crate::bail!("RUN_NOT_FOUND", "run {run} is not in the turn log");
    }
    let inverses: Vec<Vec<u8>> = events
        .iter()
        .filter(|e| e["ev"] == "tool" && e["run"] == run)
        .filter_map(|e| e.pointer("/changes/inverse").and_then(Value::as_str))
        .map(|b| base64::engine::general_purpose::STANDARD.decode(b))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::domain("INVALID_LOG", format!("inverse changeset: {e}")))?;
    let tx = conn.transaction()?;
    for inverse in inverses.iter().rev() {
        tx.apply_strm(&mut inverse.as_slice(), None::<fn(&str) -> bool>, |_, _| ConflictAction::SQLITE_CHANGESET_ABORT)
            .map_err(|e| Error::domain("REWIND_CONFLICT", format!("a row changed after run {run}: {e}")))?;
    }
    tx.commit()?;
    Ok(inverses.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_and_blobs() {
        let args = json!({"api_key": "x", "text": "Bearer abc.def sk-1234567890abcdefXYZ", "photo": format!("data:image/jpeg;base64,{}", "A".repeat(10))});
        let cleaned = clean(&args, 1000, 0);
        assert_eq!(cleaned["api_key"], "[redacted]");
        assert_eq!(cleaned["text"], "[redacted] [redacted]");
        assert_eq!(cleaned["photo"]["bytes"], 33);
        assert_eq!(scrub("abcdef", 4), "abcd…[+2]");
        // Trajectories keep deep, long tool-call arguments whole; secrets still go.
        let deep = json!([{"tool_calls": [{"function": {"arguments": format!("{} sk-1234567890abcdefXYZ", "x".repeat(2000))}}]}]);
        let kept = redact(&deep)[0]["tool_calls"][0]["function"]["arguments"].as_str().unwrap().to_string();
        assert!(kept.starts_with(&"x".repeat(2000)) && kept.ends_with("[redacted]"), "{kept}");
        let bot_url = "https://api.telegram.org/bot123456789:AAHxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/sendMessage";
        let scrubbed_bot = scrub(bot_url, 200);
        assert!(!scrubbed_bot.contains("AAHxxx"), "bot token leaked: {scrubbed_bot}");
        let ics_url = "https://gym.example/calendar/0123456789abcdef0123456789abcdef.ics";
        let scrubbed_ics = scrub(ics_url, 200);
        assert!(!scrubbed_ics.contains("0123456789abcdef"), "ics token leaked: {scrubbed_ics}");
    }

    /// A tool call's recorded inverse restores the DB exactly; OAuth tokens never enter the changeset.
    #[test]
    fn changes_rewind_and_skip_secrets() {
        let (path, mut db) = crate::db::test::file();
        db.execute("INSERT INTO agent_note (kind, text, created_at, updated_at) VALUES ('owner', 'keep', '2026-01-01', '2026-01-01')", [])
            .unwrap();
        let tx = db.transaction().unwrap();
        let mut session = record(&tx).unwrap();
        tx.execute("UPDATE agent_note SET text = 'changed'", []).unwrap();
        tx.execute("INSERT INTO agent_note (kind, text, created_at, updated_at) VALUES ('owner', 'new', '2026-01-02', '2026-01-02')", [])
            .unwrap();
        tx.execute("INSERT INTO calendar_credential (id, credentials_json) VALUES (1, '{\"token\": \"t\"}')", [])
            .unwrap();
        let changed = changes(&mut session).unwrap().unwrap();
        drop(session);
        tx.commit().unwrap();
        assert_eq!(
            changed["rows"],
            json!([{"table": "agent_note", "op": "update", "pk": [1]}, {"table": "agent_note", "op": "insert", "pk": [2]}])
        );
        let events = [json!({"ev": "tool", "run": "r1", "changes": changed})];
        assert_eq!(rewind(&mut db, &events, "r1").unwrap(), 1);
        let notes: Vec<String> = db
            .prepare("SELECT text FROM agent_note")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(notes, ["keep"]);
        // A row changed since the run: the rewind refuses instead of clobbering it.
        db.execute("UPDATE agent_note SET text = 'later'", []).unwrap();
        let again = [json!({"ev": "tool", "run": "r1", "changes": changed})];
        assert_eq!(rewind(&mut db, &again, "r1").unwrap_err().code(), "REWIND_CONFLICT");
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
