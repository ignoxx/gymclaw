//! Telegram + OpenRouter + scheduler + outbox: `gymclaw serve`. Owner: WP-runtime.
pub mod outbox;

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::db::Connection;
use crate::error::{Error, Result};

/// Run `f` on a fresh connection in the blocking pool. Domain code is synchronous; a panic (e.g. a
/// `todo!()` contract) becomes an `INTERNAL` error instead of taking the runtime down.
pub async fn blocking<T: Send + 'static>(db_path: &Path, f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static) -> Result<T> {
    let path: PathBuf = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut db = crate::db::open(&path)?;
        f(&mut db)
    })
    .await
    .unwrap_or_else(|e| Err(Error::domain("INTERNAL", format!("worker failed: {e}"))))
}

/// Run the bot: Telegram long-poll, agent turns, coach, scheduler and outbox, until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    let _ = config;
    todo!("WP-runtime")
}
