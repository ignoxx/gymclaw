//! Telegram + OpenRouter + scheduler + outbox: `gymclaw serve`. Owner: WP-runtime.
use crate::config::Config;
use crate::error::Result;

/// Run the bot: Telegram long-poll, agent turns, coach, scheduler and outbox, until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    let _ = config;
    todo!("WP-runtime")
}
