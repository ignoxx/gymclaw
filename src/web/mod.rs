//! Dashboard (`gymclaw dashboard`). Owner: WP-runtime.
//!
//! Serves the frontend in `web/` (embedded), exercise SVGs and `/api/state`, a fresh snapshot from a
//! read-only connection per request. The only writes are the agent's notes (`POST /api/note_*`), so
//! the owner can edit what it remembers. Only the allowed Host (or loopback) gets an answer, so DNS
//! rebinding can't read it, and writes need a same-origin JSON request.
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

/// Tools the dashboard may call: the owner edits the notes and lessons in the agent's prompt.
const NOTE_TOOLS: [&str; 3] = ["note_add", "note_edit", "note_delete"];

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
    if request.method() == Method::POST {
        let Some(tool) = request.uri().path().strip_prefix("/api/").filter(|t| NOTE_TOOLS.contains(t)).map(String::from) else {
            return error(StatusCode::NOT_FOUND);
        };
        // A cross-site page can only send JSON after a CORS preflight, which is never answered.
        let json = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
        let same_origin = headers.get("sec-fetch-site").is_none_or(|v| v == "same-origin");
        if !json || !same_origin {
            return error(StatusCode::FORBIDDEN);
        }
        let Ok(body) = axum::body::to_bytes(request.into_body(), 8192).await else { return error(StatusCode::PAYLOAD_TOO_LARGE) };
        let Ok(args) = serde_json::from_slice::<serde_json::Value>(&body) else { return error(StatusCode::BAD_REQUEST) };
        let db_path = app.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = crate::db::open(&db_path)?;
            let dir = db_path.parent().unwrap_or(std::path::Path::new("."));
            let mut cx = crate::tools::Ctx { db: &db, now: Ts::now(), request_id: format!("dashboard:{}", crate::db::new_id()), data_dir: dir, effects: vec![] };
            crate::tools::call(&mut cx, &tool, args)
        })
        .await;
        return match result {
            Ok(Ok(data)) => reply(StatusCode::OK, serde_json::to_vec(&data).unwrap_or_default(), "application/json"),
            Ok(Err(e @ crate::error::Error::Domain { .. })) => reply(StatusCode::BAD_REQUEST, e.to_json().to_string().into_bytes(), "application/json"),
            Ok(Err(e)) => {
                tracing::warn!("dashboard note write: {} {e}", e.code());
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
            Err(e) => {
                tracing::warn!("dashboard note write panicked: {e}");
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
        };
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

/// Serve the dashboard: reads from a read-only connection, note edits through a writable one.
/// Requests whose Host isn't `allowed_host` are rejected.
pub async fn dashboard(db_path: PathBuf, addr: SocketAddr, allowed_host: String) -> Result<()> {
    let port = addr.port();
    let hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}"), allowed_host];
    let app = Router::new().fallback(handle).with_state(Arc::new(App { db_path, hosts }));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("GymClaw dashboard: http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> Arc<App> {
        let path = std::env::temp_dir().join(format!("gymclaw-dash-{}.db", crate::db::new_id()));
        let mut db = crate::db::open(&path).unwrap();
        crate::db::migrate(&mut db).unwrap();
        Arc::new(App { db_path: path, hosts: vec!["gym.example".into()] })
    }

    async fn post(app: &Arc<App>, path: &str, body: &str, site: &str) -> Response {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, "gym.example")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", site)
            .body(Body::from(body.to_string()))
            .unwrap();
        handle(State(app.clone()), request).await
    }

    async fn get(app: &Arc<App>, method: Method, path: &str, host: &str) -> Response {
        let request = Request::builder().method(method).uri(path).header(header::HOST, host).body(Body::empty()).unwrap();
        handle(State(app.clone()), request).await
    }

    #[tokio::test]
    async fn serves_state_only_to_allowed_hosts_and_only_public_assets() {
        let app = app();
        let state = get(&app, Method::GET, "/api/state", "gym.example").await;
        assert_eq!(state.status(), StatusCode::OK);
        assert_eq!(get(&app, Method::GET, "/api/state", "evil.example").await.status(), StatusCode::FORBIDDEN);
        let index = get(&app, Method::GET, "/", "gym.example").await;
        assert_eq!(index.status(), StatusCode::OK);
        assert!(index.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("frame-ancestors 'none'"));
        for path in ["/../README.md", "/data/private.sqlite", "/unknown", "/illustrations/../../README.svg"] {
            assert_eq!(get(&app, Method::GET, path, "gym.example").await.status(), StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(get(&app, Method::POST, "/api/state", "gym.example").await.status(), StatusCode::NOT_FOUND);
        let _ = std::fs::remove_file(&app.db_path);
    }

    #[tokio::test]
    async fn edits_notes_only_from_same_origin() {
        let app = app();
        let add = r#"{"kind":"owner","text":"Weighs in on Sundays"}"#;
        assert_eq!(post(&app, "/api/note_add", add, "cross-site").await.status(), StatusCode::FORBIDDEN);
        assert_eq!(post(&app, "/api/sql_query", r#"{"sql":"SELECT 1"}"#, "same-origin").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(post(&app, "/api/note_add", add, "same-origin").await.status(), StatusCode::OK);
        assert_eq!(post(&app, "/api/note_edit", r#"{"id":99,"text":"x"}"#, "same-origin").await.status(), StatusCode::BAD_REQUEST);
        let db = crate::db::open_readonly(&app.db_path).unwrap();
        assert_eq!(crate::tools::meta::notes(&db).unwrap()[0].text, "Weighs in on Sundays");
        let _ = std::fs::remove_file(&app.db_path);
    }
}
