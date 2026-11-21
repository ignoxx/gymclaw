//! Runtime settings from the environment. The only module that reads env vars (clippy
//! `disallowed_methods`). In production a variable reaches the bot only if the `gymclaw` service's
//! `environment` in docker-compose.yaml passes it. Unset and empty are the same. Every variable is
//! listed in docs/self-hosting.md; keep that table in step.
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// The only Telegram user the bot talks to (DM chat id == user id).
    pub owner_id: i64,
    pub telegram_token: String,
    pub openrouter_key: String,
    /// Conversation model.
    pub model: String,
    /// Model that reads photos (scale, plan screenshots).
    pub model_vision: String,
    /// Model that transcribes voice notes; needs audio input (the chat model's default has none).
    pub model_audio: String,
    /// OpenRouter provider pin, e.g. `inference-net/fp4`.
    pub provider: Option<String>,
    /// Public HTTPS hostname of the dashboard; the Telegram menu button opens it.
    pub dashboard_host: Option<String>,
    /// Where full agent turns are logged, if anywhere.
    pub turn_log_dir: Option<PathBuf>,
    /// Crowd source config JSON, if crowd polling is configured for this instance.
    pub crowd_config: Option<PathBuf>,
}

#[expect(clippy::disallowed_methods, reason = "the one env read")]
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The chat model (`GYMCLAW_MODEL`), for `Config` and `gymclaw replay`.
pub fn model() -> String {
    var("GYMCLAW_MODEL").unwrap_or_else(|| "anthropic/claude-haiku-5.5".into())
}

/// The OpenRouter provider pin (`GYMCLAW_PROVIDER`), if any.
pub fn provider() -> Option<String> {
    var("GYMCLAW_PROVIDER")
}

/// Optional Telegram bot token (for `dashboard` Mini App auth and `doctor`).
pub fn telegram_token() -> Option<String> {
    var("TELEGRAM_BOT_TOKEN")
}

/// Optional OpenRouter API key (for `doctor`).
pub fn openrouter_key() -> Option<String> {
    var("OPENROUTER_API_KEY")
}

/// The owner's Telegram user id, for code that runs without a full `Config` (tools, CLI).
pub fn owner_id() -> Result<i64, String> {
    var("GYMCLAW_TELEGRAM_USER_ID")
        .ok_or("GYMCLAW_TELEGRAM_USER_ID is required")?
        .parse()
        .map_err(|_| "GYMCLAW_TELEGRAM_USER_ID must be a numeric Telegram user id".into())
}

/// Timezone a new profile starts with.
pub fn timezone_seed() -> String {
    var("GYMCLAW_TIMEZONE").map(|v| v.trim().to_string()).unwrap_or_else(|| "Europe/Berlin".into())
}

/// Workout Guide assets, relative to the working dir unless set.
pub fn assets_dir() -> PathBuf {
    var("GYMCLAW_ASSETS_DIR").unwrap_or_else(|| "assets/workout-guide".into()).into()
}

/// Crowd source config file.
pub fn crowd_config() -> PathBuf {
    var("GYMCLAW_CROWD_CONFIG").unwrap_or_else(|| "data/crowd-config.json".into()).into()
}

impl Config {
    /// Paths only; for CLI commands that don't talk to Telegram or a model.
    pub fn paths() -> (PathBuf, PathBuf) {
        let data_dir: PathBuf = var("GYMCLAW_DATA_DIR").unwrap_or_else(|| "data".into()).into();
        let db_path = var("GYMCLAW_DB").map(PathBuf::from).unwrap_or_else(|| data_dir.join("gymclaw.db"));
        (data_dir, db_path)
    }

    pub fn from_env() -> Result<Config, String> {
        let (data_dir, db_path) = Self::paths();
        let need = |name: &str| var(name).ok_or_else(|| format!("{name} is required"));
        let owner_id = owner_id()?;
        let model = model();
        Ok(Config {
            data_dir,
            db_path,
            owner_id,
            telegram_token: need("TELEGRAM_BOT_TOKEN")?,
            openrouter_key: need("OPENROUTER_API_KEY")?,
            model_vision: var("GYMCLAW_MODEL_VISION").unwrap_or_else(|| model.clone()),
            model_audio: var("GYMCLAW_MODEL_AUDIO").unwrap_or_else(|| "google/gemini-3.5-flash-lite".into()),
            model,
            provider: provider(),
            dashboard_host: var("GYMCLAW_DASHBOARD_HOST"),
            turn_log_dir: var("GYMCLAW_TURN_LOG_DIR").map(PathBuf::from),
            crowd_config: Some(crowd_config()),
        })
    }
}

#[cfg(test)]
mod tests {
    /// `config.rs` promises every env var is listed in `docs/self-hosting.md`; keep them in step
    /// (including clap `env = "..."` args in `src/main.rs`).
    #[test]
    fn every_env_var_is_documented() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let config = std::fs::read_to_string(root.join("src/config.rs")).unwrap();
        let main = std::fs::read_to_string(root.join("src/main.rs")).unwrap();
        let doc = std::fs::read_to_string(root.join("docs/self-hosting.md")).unwrap();
        let cfg_re = regex::Regex::new(r#"(?:var|need)\("([A-Z0-9_]+)"\)"#).unwrap();
        let clap_re = regex::Regex::new(r#"env\s*=\s*"([A-Z0-9_]+)""#).unwrap();
        let vars: std::collections::BTreeSet<String> =
            cfg_re.captures_iter(&config).chain(clap_re.captures_iter(&main)).map(|c| c[1].to_string()).collect();
        let missing: Vec<_> = vars.into_iter().filter(|v| !doc.contains(&format!("`{v}`"))).collect();
        assert!(missing.is_empty(), "env vars missing from docs/self-hosting.md: {missing:?}");
    }
}
