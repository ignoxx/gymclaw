//! Read-only dashboard (`gymclaw dashboard`). Owner: WP-runtime.
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::error::Result;

/// Serve the dashboard from a read-only DB. Requests whose Host isn't `allowed_host` are rejected.
pub async fn dashboard(db_path: PathBuf, addr: SocketAddr, allowed_host: String) -> Result<()> {
    let _ = (db_path, addr, allowed_host);
    todo!("WP-runtime")
}
