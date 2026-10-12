//! Runtime settings from the environment (same variables the Python/OpenClaw deploy used).
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
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
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
        let owner_id = need("GYMCLAW_TELEGRAM_USER_ID")?.parse().map_err(|_| "GYMCLAW_TELEGRAM_USER_ID must be a numeric Telegram user id")?;
        let model = var("GYMCLAW_MODEL").unwrap_or_else(|| "z-ai/glm-5.3-flash".into());
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
        })
    }
}
