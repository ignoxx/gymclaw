//! Runtime settings from the environment. The only module that reads env vars (clippy
//! `disallowed_methods`). In production a variable reaches the bot only if the `gymclaw` service's
//! `environment` in docker-compose.yaml passes it.
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// The only Telegram user the bot talks to (DM chat id == user id).
    pub owner_id: i64,
    pub telegram_token: String,
    pub openrouter_key: String,
    /// Conversation model.
    pub model: String,
    /// Cheap model for background judgment (event triage).
    pub model_fast: String,
    /// Model that reads photos (scale, plan screenshots).
    pub model_vision: String,
    /// OpenRouter provider pin, e.g. `inference-net/fp4`.
    pub provider: Option<String>,
    /// Dashboard hostname behind Traefik; the Telegram menu button opens it.
    pub dashboard_host: Option<String>,
    /// Where full agent turns are logged, if anywhere.
    pub turn_log_dir: Option<PathBuf>,
}

#[expect(clippy::disallowed_methods, reason = "the one env read")]
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
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
    var("GYMCLAW_TIMEZONE").unwrap_or_else(|| "Europe/Berlin".into())
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
        let model = var("GYMCLAW_MODEL").unwrap_or_else(|| "anthropic/claude-haiku-5.5".into());
        Ok(Config {
            data_dir,
            db_path,
            owner_id,
            telegram_token: need("TELEGRAM_BOT_TOKEN")?,
            openrouter_key: need("OPENROUTER_API_KEY")?,
            model_fast: var("GYMCLAW_MODEL_FAST").unwrap_or_else(|| model.clone()),
            model_vision: var("GYMCLAW_MODEL_VISION").unwrap_or_else(|| model.clone()),
            model,
            provider: var("GYMCLAW_PROVIDER"),
            dashboard_host: var("GYMCLAW_DASHBOARD_HOST"),
            turn_log_dir: var("GYMCLAW_TURN_LOG_DIR").map(PathBuf::from),
        })
    }
}
