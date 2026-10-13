//! Read-only dashboard (`gymclaw dashboard`). Owner: WP-runtime.
//!
//! Serves the frontend in `web/` (embedded), exercise SVGs and `/api/state`, a fresh snapshot from a
//! read-only connection per request. Only the allowed Host (or loopback) gets an answer, so DNS
//! rebinding can't read it.
pub mod state;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;

use crate::error::Result;
use crate::time::Ts;

const ASSETS: [(&str, &[u8], &str); 6] = [
    ("/", include_bytes!("../../web/index.html"), "text/html"),
    ("/styles.css", include_bytes!("../../web/styles.css"), "text/css"),
    ("/app.js", include_bytes!("../../web/app.js"), "text/javascript"),
    ("/favicon.svg", include_bytes!("../../web/favicon.svg"), "image/svg+xml"),
    ("/fonts/BarlowCondensed-SemiBold.ttf", include_bytes!("../../web/fonts/BarlowCondensed-SemiBold.ttf"), "font/ttf"),
    ("/fonts/DM-Sans.ttf", include_bytes!("../../web/fonts/DM-Sans.ttf"), "font/ttf"),
];

struct App {
    db_path: PathBuf,
    hosts: Vec<String>,
}

fn reply(status: StatusCode, body: Vec<u8>, mime: &str) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    let set = |h: &mut HeaderMap, k, v: &str| {
        h.insert(k, HeaderValue::from_str(v).expect("header value"));
    };
    set(headers, header::CONTENT_TYPE, &format!("{mime}; charset=utf-8"));
    set(headers, header::CACHE_CONTROL, "no-store");
    set(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(headers, header::HeaderName::from_static("cross-origin-resource-policy"), "same-origin");
    set(
        headers,
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self'; frame-ancestors 'none'",
    );
    response
}

fn error(status: StatusCode) -> Response {
    reply(status, status.canonical_reason().unwrap_or("").as_bytes().to_vec(), "text/plain")
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    let headers = request.headers();
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let cross_site = headers.get("sec-fetch-site").is_some_and(|v| v == "cross-site");
    if !app.hosts.iter().any(|h| h == host) || cross_site {
        return error(StatusCode::FORBIDDEN);
    }
    if request.method() != Method::GET {
        return error(StatusCode::METHOD_NOT_ALLOWED);
    }
    let path = request.uri().path();
    if path == "/api/state" {
        let db_path = app.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = crate::db::open_readonly(&db_path)?;
            let snapshot = state::snapshot(&db, Ts::now())?;
            Ok::<_, crate::error::Error>(serde_json::to_vec(&snapshot)?)
        })
        .await;
        return match result {
            Ok(Ok(body)) => reply(StatusCode::OK, body, "application/json"),
            Ok(Err(e)) => {
                tracing::warn!("dashboard state: {} {e}", e.code());
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
            Err(e) => {
                tracing::warn!("dashboard state panicked: {e}");
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
        };
    }
    if let Some(slug) = path.strip_prefix("/illustrations/").and_then(|p| p.strip_suffix(".svg")) {
        return match crate::domain::illustrations::svg_path(slug).map(std::fs::read) {
            Some(Ok(bytes)) => reply(StatusCode::OK, bytes, "image/svg+xml"),
            _ => error(StatusCode::NOT_FOUND),
        };
    }
    match ASSETS.iter().find(|(route, _, _)| *route == path) {
        Some((_, body, mime)) => reply(StatusCode::OK, body.to_vec(), mime),
        None => error(StatusCode::NOT_FOUND),
    }
}

/// Serve the dashboard from a read-only DB. Requests whose Host isn't `allowed_host` are rejected.
pub async fn dashboard(db_path: PathBuf, addr: SocketAddr, allowed_host: String) -> Result<()> {
    let port = addr.port();
    let hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}"), allowed_host];
    let app = Router::new().fallback(handle).with_state(Arc::new(App { db_path, hosts }));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("GymClaw dashboard: http://{addr}, read-only DB state");
    axum::serve(listener, app).await?;
    Ok(())
}
