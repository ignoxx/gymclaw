//! Append-only JSONL turn log, same events and privacy rules as the old `turn-log` plugin
//! (`openclaw/plugins/turn-log/README.md`), so `summarize.mjs` keeps working.
//!
//! One line per event in `<dir>/YYYY-MM-DD.jsonl` (UTC date). `log` never blocks or fails a turn:
//! lines go through a bounded queue to a writer task; when it backs up, new lines are dropped.
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

const MAX_PENDING: usize = 1000;

static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)token|secret|password|passwd|api.?key|authorization|cookie|credential").unwrap());
static SECRET_VALUE: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r"(?i)\bBearer\s+[\w.~+/=-]+").unwrap(),
        Regex::new(r"\bsk-[\w-]{16,}").unwrap(),
        Regex::new(r"\b\d{6,12}:[\w-]{30,}").unwrap(),
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
    // JS counts UTF-16 units; chars are close enough for a cap.
    let count = value.chars().count();
    if count > max { format!("{}…[+{}]", value.chars().take(max).collect::<String>(), count - max) } else { value }
}

/// JSON-safe copy of tool args: secret keys redacted, strings scrubbed, blobs → {bytes, sha}.
pub fn clean(value: &Value, max: usize, depth: usize) -> Value {
    match value {
        Value::String(s) if BLOB.is_match(s) => json!({"bytes": s.len(), "sha": sha(s)}),
        Value::String(s) => Value::String(scrub(s, max)),
        Value::Array(_) | Value::Object(_) if depth > 4 => json!("[deep]"),
        Value::Array(items) => Value::Array(items.iter().take(50).map(|v| clean(v, max, depth + 1)).collect()),
        Value::Object(map) => Value::Object(
            map.iter().map(|(k, v)| (k.clone(), if SECRET_KEY.is_match(k) { json!("[redacted]") } else { clean(v, max, depth + 1) })).collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}

#[derive(Clone, Default)]
pub struct TurnLog {
    tx: Option<mpsc::Sender<(String, String)>>,
}

impl TurnLog {
    /// `None` = disabled (no `GYMCLAW_TURN_LOG_DIR`). Must be called inside the tokio runtime.
    pub fn new(dir: Option<PathBuf>) -> TurnLog {
        let Some(dir) = dir else { return TurnLog::default() };
        let (tx, mut rx) = mpsc::channel::<(String, String)>(MAX_PENDING);
        tokio::spawn(async move {
            let _ = create_dir(&dir).await;
            while let Some((day, line)) = rx.recv().await {
                let file = dir.join(format!("{day}.jsonl"));
                let _ = append(&file, &line).await;
            }
        });
        TurnLog { tx: Some(tx) }
    }

    /// Queue one event; `ts` is added. Null fields are dropped (JSON.stringify drops undefined).
    pub fn log(&self, event: Value) {
        let Some(tx) = &self.tx else { return };
        let ts = jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let mut line = Map::new();
        line.insert("ts".into(), json!(ts));
        if let Value::Object(fields) = event {
            line.extend(fields.into_iter().filter(|(_, v)| !v.is_null()));
        }
        let _ = tx.try_send((ts[..10].to_string(), format!("{}\n", Value::Object(line))));
    }
}

async fn create_dir(dir: &std::path::Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

async fn append(file: &std::path::Path, line: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(file).await?.write_all(line.as_bytes()).await
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
    }
}
