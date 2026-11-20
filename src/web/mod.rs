//! Dashboard (`gymclaw dashboard`).
//!
//! Serves the frontend in `web/` (embedded), exercise SVGs, `/api/state` (a fresh snapshot from a
//! read-only connection per request) and `/export.csv` (every logged set, `import::export_csv`). The
//! only writes are `DASHBOARD_TOOLS` (`POST /api/<tool>`), so the owner can edit what the agent
//! remembers and built, and plan targets, and `POST /api/coach`, the live workout's buttons and typed
//! sets (`coach::app_tap`; the bot then shows the step in the chat). Only the allowed Host (or loopback) gets an answer, so DNS
//! rebinding can't read it, and writes need a same-origin JSON request.
pub mod state;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;

use crate::error::Result;
use crate::time::Ts;

const ASSETS: [(&str, &[u8], &str); 7] = [
    ("/", include_bytes!("../../web/index.html"), "text/html"),
    ("/styles.css", include_bytes!("../../web/styles.css"), "text/css"),
    ("/app.js", include_bytes!("../../web/app.js"), "text/javascript"),
    ("/favicon.svg", include_bytes!("../../web/favicon.svg"), "image/svg+xml"),
    // Home screen icon: iOS ignores SVG icons and falls back to a letter without it.
    ("/apple-touch-icon.png", include_bytes!("../../web/apple-touch-icon.png"), "image/png"),
    ("/fonts/BarlowCondensed-SemiBold.ttf", include_bytes!("../../web/fonts/BarlowCondensed-SemiBold.ttf"), "font/ttf"),
    ("/fonts/DM-Sans.ttf", include_bytes!("../../web/fonts/DM-Sans.ttf"), "font/ttf"),
];

/// Tools the dashboard may call: note edits and turning the agent's extensions off, on or back
/// (Memory), plan targets (Exercises) and fixing a logged set (the live workout). They run exactly as
/// the agent's calls do, with the same validation; extension changes are recorded as the owner's.
const DASHBOARD_TOOLS: [&str; 10] = [
    "note_add",
    "note_edit",
    "note_delete",
    "extension_edit",
    "template_edit_exercise",
    "set_edit",
    "body_log",
    "profile_update",
    "runtime_pause",
    "runtime_resume",
];
/// The only exercise fields the dashboard may change. An id, name or picture change would cut an
/// exercise off from its history, so those stay with the agent.
const TARGET_FIELDS: [&str; 5] = ["target_weight", "working_sets", "rep_min", "rep_max", "rest_seconds"];

/// A plan edit that touches only targets: no reordering, no other fields.
fn targets_only(args: &serde_json::Value) -> bool {
    args.get("position").is_none_or(serde_json::Value::is_null)
        && args
            .get("changes")
            .and_then(|c| c.as_object())
            .is_some_and(|c| c.keys().all(|k| TARGET_FIELDS.contains(&k.as_str())))
}

/// Optional Telegram Mini App auth (`TELEGRAM_BOT_TOKEN` + `GYMCLAW_TELEGRAM_USER_ID`).
/// Enforced on every `/api/*` and `/export.csv` request whenever configured (`None` only for local dev).
#[derive(Clone)]
pub struct WebAuth {
    pub bot_token: String,
    pub owner_id: i64,
}

struct App {
    db_path: PathBuf,
    hosts: Vec<String>,
    auth: Option<WebAuth>,
}

/// RFC 2104 HMAC-SHA256 over `sha2::Sha256` (no extra crate).
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let h = Sha256::digest(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex_str = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex_str, 16).ok()?);
                i += 3;
            }
            b'%' => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Verify Telegram Mini App `initData` (HMAC-SHA256 with `WebAppData` + bot token, auth_date <= 24h,
/// and matching `owner_id` when configured).
pub fn verify_telegram_init_data(init_data: &str, auth: &WebAuth, now: Ts, max_age_secs: i64) -> bool {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut hash_hex: Option<String> = None;
    for part in init_data.split('&').filter(|p| !p.is_empty()) {
        let Some((k, v)) = part.split_once('=') else { return false };
        let (Some(k), Some(v)) = (percent_decode(k), percent_decode(v)) else { return false };
        if k == "hash" {
            hash_hex = Some(v);
        } else {
            pairs.push((k, v));
        }
    }
    let Some(hash_bytes) = hash_hex.and_then(|h| hex::decode(h).ok()) else { return false };
    let auth_date: i64 = match pairs.iter().find(|(k, _)| k == "auth_date").and_then(|(_, v)| v.parse().ok()) {
        Some(d) => d,
        None => return false,
    };
    let age = now.0.as_second() - auth_date;
    if !(-300..=max_age_secs).contains(&age) {
        return false;
    }
    let user_id = pairs
        .iter()
        .find(|(k, _)| k == "user")
        .and_then(|(_, v)| serde_json::from_str::<serde_json::Value>(v).ok())
        .and_then(|u| u["id"].as_i64());
    if user_id != Some(auth.owner_id) {
        return false;
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let check_string = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");
    let secret = hmac_sha256(b"WebAppData", auth.bot_token.as_bytes());
    let expected = hmac_sha256(&secret, check_string.as_bytes());
    ct_eq(&expected, &hash_bytes)
}

fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k == key).then(|| percent_decode(v)).flatten()
    })
}

fn authorized(app: &App, headers: &HeaderMap, query: Option<&str>, allow_query: bool) -> bool {
    let Some(auth) = &app.auth else { return true };
    if let Some(data) = headers.get("x-telegram-init-data").and_then(|v| v.to_str().ok()) {
        return verify_telegram_init_data(data, auth, Ts::now(), 86_400);
    }
    // Query-param auth (CSV download) uses a shorter 5-min window to limit URL log exposure.
    if allow_query && let Some(data) = query_param(query, "init_data") {
        return verify_telegram_init_data(&data, auth, Ts::now(), 300);
    }
    false
}

fn ics_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n")
}

fn ics_utc(ts: Ts) -> String {
    ts.0.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// Read-only RFC 5545 calendar feed of non-cancelled planned sessions (past 30 days through future).
fn render_ics(db: &crate::db::Connection, now: Ts) -> Result<String> {
    use crate::db::models::PlannedSession;
    use crate::domain::calendar::workout_name;
    let profile = crate::domain::profile::get_profile(db)?;
    let cutoff = now - jiff::SignedDuration::from_hours(24 * 30);
    let sessions = PlannedSession::select(
        db,
        "WHERE status NOT IN ('CANCELLED', 'SKIPPED', 'MISSED') AND planned_end_at >= ?1 ORDER BY planned_start_at",
        [cutoff],
    )?;
    let stamp = ics_utc(now);
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//gymclaw//Workouts//EN".into(),
        "X-WR-CALNAME:gymclaw".into(),
        "METHOD:PUBLISH".into(),
    ];
    for s in sessions {
        let summary = workout_name(&s);
        let plan_lines = crate::domain::preview::plan_lines(db, &s).unwrap_or_default();
        lines.push("BEGIN:VEVENT".into());
        lines.push(format!("UID:{}@gymclaw", s.id));
        lines.push(format!("DTSTAMP:{stamp}"));
        lines.push(format!("DTSTART:{}", ics_utc(s.planned_start_at)));
        lines.push(format!("DTEND:{}", ics_utc(s.planned_end_at)));
        lines.push(format!("SUMMARY:{}", ics_escape(&summary)));
        if let Some(addr) = profile.gym_address.as_deref().filter(|a| !a.is_empty()) {
            lines.push(format!("LOCATION:{}", ics_escape(addr)));
        }
        if !plan_lines.is_empty() {
            lines.push(format!("DESCRIPTION:{}", ics_escape(&plan_lines.join("\n"))));
        }
        lines.push("END:VEVENT".into());
    }
    lines.push("END:VCALENDAR".into());
    Ok(format!("{}\r\n", lines.join("\r\n")))
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
        // telegram.org: the Mini App SDK (index.html), which Telegram asks apps to load from there.
        "default-src 'self'; style-src 'self'; script-src 'self' https://telegram.org; connect-src 'self'; img-src 'self'; font-src 'self'; frame-ancestors https://web.telegram.org",
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
        if !authorized(&app, headers, request.uri().query(), false) {
            return error(StatusCode::UNAUTHORIZED);
        }
        let Some(tool) = request
            .uri()
            .path()
            .strip_prefix("/api/")
            .filter(|t| *t == "coach" || DASHBOARD_TOOLS.contains(t))
            .map(String::from)
        else {
            return error(StatusCode::NOT_FOUND);
        };
        // A cross-site page can only send JSON after a CORS preflight, which is never answered.
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        let same_origin = headers.get("sec-fetch-site").is_none_or(|v| v == "same-origin");
        if !json || !same_origin {
            return error(StatusCode::FORBIDDEN);
        }
        let Ok(body) = axum::body::to_bytes(request.into_body(), 8192).await else {
            return error(StatusCode::PAYLOAD_TOO_LARGE);
        };
        let Ok(args) = serde_json::from_slice::<serde_json::Value>(&body) else {
            return error(StatusCode::BAD_REQUEST);
        };
        if tool == "template_edit_exercise" && !targets_only(&args) {
            return error(StatusCode::FORBIDDEN);
        }
        // The app fixes a set's numbers; moving, retyping or deleting one stays with the agent.
        if tool == "set_edit" && ["warmup", "exercise", "delete"].iter().any(|k| args.get(k).is_some()) {
            return error(StatusCode::FORBIDDEN);
        }
        // The app logs typed weigh-ins only, never arbitrary filesystem photo paths.
        if tool == "body_log"
            && args
                .get("entries")
                .and_then(|e| e.as_array())
                .is_some_and(|es| es.iter().any(|e| e.get("photo").is_some_and(|v| !v.is_null())))
        {
            return error(StatusCode::FORBIDDEN);
        }
        // The app toggles reminders only; calendar writes stay an explicit chat grant.
        if tool == "runtime_resume" && args.get("calendar_writes").is_some_and(|v| v == true) {
            return error(StatusCode::FORBIDDEN);
        }
        let db_path = app.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let data_dir = db_path.parent().unwrap_or(Path::new("."));
            crate::db::in_tx(&mut crate::db::open(&db_path)?, |db| {
                let request_id = format!("dashboard:{}", crate::db::new_id());
                if tool == "coach" {
                    let view = if let Some(template_id) = args["start_template"].as_str() {
                        crate::domain::coach::app_start_template(db, template_id, Ts::now(), &request_id)?
                    } else {
                        let data = args["data"].as_str().unwrap_or_default();
                        crate::domain::coach::app_tap(db, data, Ts::now(), &request_id)?
                    };
                    return Ok(serde_json::json!({"ack": view.ack}));
                }
                let mut cx = crate::tools::Ctx { db, now: Ts::now(), request_id, data_dir, effects: vec![] };
                let result = crate::tools::call(&mut cx, &tool, args.clone())?;
                if tool == "set_edit" {
                    // The chat saw the old number on the set's card: it gets a note with the fixed one.
                    crate::domain::coach::app_fixed(db, args["set_id"].as_str().unwrap_or_default(), Ts::now())?;
                }
                Ok(result)
            })
        })
        .await;
        return match result {
            Ok(Ok(data)) => reply(StatusCode::OK, serde_json::to_vec(&data).unwrap_or_default(), "application/json"),
            Ok(Err(e @ crate::error::Error::Domain { .. })) => {
                reply(StatusCode::BAD_REQUEST, e.to_json().to_string().into_bytes(), "application/json")
            }
            Ok(Err(e)) => {
                tracing::warn!("dashboard write: {} {e}", e.code());
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
            Err(e) => {
                tracing::warn!("dashboard write panicked: {e}");
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
        };
    }
    if request.method() != Method::GET {
        return error(StatusCode::METHOD_NOT_ALLOWED);
    }
    let path = request.uri().path();
    let query = request.uri().query();
    if let Some(token) = path.strip_prefix("/calendar/").and_then(|p| p.strip_suffix(".ics")) {
        let token = token.to_string();
        let db_path = app.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = crate::db::open_readonly(&db_path)?;
            let Some(expected) = crate::domain::profile::ics_token(&db)? else { return Ok(None) };
            if !ct_eq(expected.as_bytes(), token.as_bytes()) {
                return Ok(None);
            }
            Ok::<_, crate::error::Error>(Some(render_ics(&db, Ts::now())?))
        })
        .await;
        return match result {
            Ok(Ok(Some(ics))) => reply(StatusCode::OK, ics.into_bytes(), "text/calendar"),
            Ok(Ok(None)) => error(StatusCode::NOT_FOUND),
            _ => error(StatusCode::SERVICE_UNAVAILABLE),
        };
    }
    if (path.starts_with("/api/") || path == "/export.csv") && !authorized(&app, headers, query, path == "/export.csv")
    {
        return error(StatusCode::UNAUTHORIZED);
    }
    if path == "/api/live" {
        let db_path = app.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = crate::db::open_readonly(&db_path)?;
            let snapshot = state::live_snapshot(&db, Ts::now())?;
            Ok::<_, crate::error::Error>(serde_json::to_vec(&snapshot)?)
        })
        .await;
        return match result {
            Ok(Ok(body)) => reply(StatusCode::OK, body, "application/json"),
            Ok(Err(e)) => {
                tracing::warn!("dashboard live: {} {e}", e.code());
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
            Err(e) => {
                tracing::warn!("dashboard live panicked: {e}");
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
        };
    }
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
    if path == "/export.csv" {
        let db_path = app.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::domain::import::export_csv(&crate::db::open_readonly(&db_path)?)
        })
        .await;
        return match result {
            Ok(Ok(csv)) => {
                let mut response = reply(StatusCode::OK, csv.into_bytes(), "text/csv");
                let name = HeaderValue::from_static("attachment; filename=\"gymclaw-sets.csv\"");
                response.headers_mut().insert(header::CONTENT_DISPOSITION, name);
                response
            }
            _ => error(StatusCode::SERVICE_UNAVAILABLE),
        };
    }
    if path == "/api/catalog" {
        return reply(StatusCode::OK, serde_json::to_vec(&state::catalog()).unwrap_or_default(), "application/json");
    }
    if let Some(slug) = path.strip_prefix("/illustrations/").and_then(|p| p.strip_suffix(".svg")) {
        return match crate::domain::illustrations::svg_path(slug).map(std::fs::read) {
            Some(Ok(bytes)) => {
                // Without a cache, every dashboard redraw (each 10 s poll that changed the state) downloaded
                // every picture again. A redrawn picture shows within a day.
                let mut response = reply(StatusCode::OK, bytes, "image/svg+xml");
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
                response
            }
            _ => error(StatusCode::NOT_FOUND),
        };
    }
    match ASSETS.iter().find(|(route, _, _)| *route == path) {
        Some((route, embedded, mime)) => reply(StatusCode::OK, asset(route, embedded), mime),
        None => error(StatusCode::NOT_FOUND),
    }
}

/// Release builds serve the copy embedded at compile time; debug builds read `web/` from disk, so an
/// edit shows on refresh. Only the routes in `ASSETS` are served either way.
fn asset(route: &str, embedded: &[u8]) -> Vec<u8> {
    if cfg!(debug_assertions) {
        let file = if route == "/" { "index.html" } else { &route[1..] };
        if let Ok(bytes) = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("web").join(file)) {
            return bytes;
        }
    }
    embedded.to_vec()
}

/// Serve the dashboard: reads from a read-only connection, `DASHBOARD_TOOLS` writes through a writable one.
/// Requests whose Host isn't `allowed_host` are rejected.
pub async fn dashboard(db_path: PathBuf, addr: SocketAddr, allowed_host: String, auth: Option<WebAuth>) -> Result<()> {
    let port = addr.port();
    let hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}"), allowed_host];
    let app = Router::new().fallback(handle).with_state(Arc::new(App { db_path, hosts, auth }));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("gymclaw dashboard: http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> Arc<App> {
        let (path, _) = crate::db::test::file();
        Arc::new(App { db_path: path, hosts: vec!["gym.example".into()], auth: None })
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
        let request =
            Request::builder().method(method).uri(path).header(header::HOST, host).body(Body::empty()).unwrap();
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
        assert!(
            index.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors https://web.telegram.org")
        );
        let picture = get(&app, Method::GET, "/illustrations/bench-press.svg", "gym.example").await;
        assert_eq!(picture.headers()[header::CACHE_CONTROL], "private, max-age=86400");
        assert_eq!(index.headers()[header::CACHE_CONTROL], "no-store");
        for path in ["/../README.md", "/data/private.sqlite", "/unknown", "/illustrations/../../README.svg"] {
            assert_eq!(get(&app, Method::GET, path, "gym.example").await.status(), StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(get(&app, Method::POST, "/api/state", "gym.example").await.status(), StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(app.db_path.parent().unwrap());
    }

    #[tokio::test]
    async fn edits_notes_and_plan_targets_only_from_same_origin() {
        let app = app();
        let add = r#"{"kind":"owner","text":"Weighs in on Sundays"}"#;
        assert_eq!(post(&app, "/api/note_add", add, "cross-site").await.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            post(&app, "/api/sql_query", r#"{"sql":"SELECT 1"}"#, "same-origin").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(
                &app,
                "/api/template_remove_exercise",
                r#"{"template_id":"legs","exercise":"Leg press"}"#,
                "same-origin"
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(post(&app, "/api/note_add", add, "same-origin").await.status(), StatusCode::OK);
        assert_eq!(
            post(&app, "/api/note_edit", r#"{"id":99,"text":"x"}"#, "same-origin").await.status(),
            StatusCode::BAD_REQUEST
        );
        let db = crate::db::open_readonly(&app.db_path).unwrap();
        assert_eq!(crate::tools::meta::notes(&db).unwrap()[0].text, "Weighs in on Sundays");
        let _ = std::fs::remove_dir_all(app.db_path.parent().unwrap());
    }

    #[tokio::test]
    async fn edits_only_the_targets_of_the_named_variant() {
        let app = app();
        let press = |id: &str, name: &str, weight: f64| serde_json::json!({"id": id, "name": name, "role": "legs", "guide_id": "leg-press", "target_weight": weight});
        let legs = serde_json::json!({"id": "legs", "name": "Legs", "source": "user", "exercises": [press("leg-press", "Leg press", 85.0), press("leg-press-45", "Leg press (45°)", 140.0)]});
        let template: crate::domain::templates::Template = serde_json::from_value(legs).unwrap();
        crate::domain::templates::import_template(&crate::db::open(&app.db_path).unwrap(), &template).unwrap();
        let edit =
            |changes: &str| format!(r#"{{"template_id":"legs","exercise":"Leg press (45°)","changes":{changes}}}"#);
        for other in [r#"{"id":"x"}"#, r#"{"guide_id":null}"#, r#"{"name":"Squat"}"#] {
            assert_eq!(
                post(&app, "/api/template_edit_exercise", &edit(other), "same-origin").await.status(),
                StatusCode::FORBIDDEN,
                "{other}"
            );
        }
        let reorder = r#"{"template_id":"legs","exercise":"Leg press (45°)","changes":{},"position":0}"#;
        assert_eq!(
            post(&app, "/api/template_edit_exercise", reorder, "same-origin").await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post(
                &app,
                "/api/template_edit_exercise",
                &edit(r#"{"target_weight":150,"rest_seconds":null}"#),
                "same-origin"
            )
            .await
            .status(),
            StatusCode::OK
        );
        let stored =
            crate::domain::templates::get_template(&crate::db::open_readonly(&app.db_path).unwrap(), "legs").unwrap();
        let weights: Vec<f64> = stored.exercises.iter().map(|e| e.target_weight).collect();
        assert_eq!(weights, [85.0, 150.0]);

        // A write that fails midway (here refreshing a planned session, after the plan was saved)
        // leaves nothing behind.
        let db = crate::db::open(&app.db_path).unwrap();
        crate::db::models::PlannedSession::new("2026-11-16".into(), Some("legs".into()), false).insert(&db).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail BEFORE UPDATE ON planned_session BEGIN SELECT RAISE(ABORT, 'disk full'); END",
        )
        .unwrap();
        let failed = post(&app, "/api/template_edit_exercise", &edit(r#"{"target_weight":160}"#), "same-origin").await;
        let body = axum::body::to_bytes(failed.into_body(), 8192).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("disk full"));
        assert_eq!(crate::domain::templates::get_template(&db, "legs").unwrap().exercises[1].target_weight, 150.0);
        let _ = std::fs::remove_dir_all(app.db_path.parent().unwrap());
    }

    fn sign_init_data(bot_token: &str, user_id: i64, auth_date: i64) -> String {
        let user = format!(r#"{{"id":{user_id}}}"#);
        let check = format!("auth_date={auth_date}\nuser={user}");
        let secret = hmac_sha256(b"WebAppData", bot_token.as_bytes());
        let hash = hex::encode(hmac_sha256(&secret, check.as_bytes()));
        let encoded_user = user.replace('{', "%7B").replace('}', "%7D").replace('"', "%22").replace(':', "%3A");
        format!("auth_date={auth_date}&user={encoded_user}&hash={hash}")
    }

    #[tokio::test]
    async fn telegram_init_data_auth_and_ics_export() {
        let (path, db) = crate::db::test::file();
        let _ = crate::domain::profile::get_profile(&db).unwrap();
        let token = crate::domain::profile::ics_token(&db).unwrap().unwrap();
        let mut session = crate::db::models::PlannedSession::new("2026-11-16".into(), None, false);
        session.planned_start_at = Ts::now();
        session.planned_end_at = Ts::now().mins(60);
        session.merge_plan(serde_json::json!({"template": {"name": "Push"}}));
        session.insert(&db).unwrap();

        let auth = WebAuth { bot_token: "123:secret".into(), owner_id: 42 };
        let app = Arc::new(App { db_path: path.clone(), hosts: vec!["gym.example".into()], auth: Some(auth) });
        // Without initData, /api/state is 401.
        assert_eq!(get(&app, Method::GET, "/api/state", "gym.example").await.status(), StatusCode::UNAUTHORIZED);
        // With valid initData for owner 42, /api/state is 200.
        let valid = sign_init_data("123:secret", 42, Ts::now().0.as_second());
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header(header::HOST, "gym.example")
            .header("x-telegram-init-data", &valid)
            .body(Body::empty())
            .unwrap();
        assert_eq!(handle(State(app.clone()), req).await.status(), StatusCode::OK);
        // Wrong user id is 401.
        let wrong_user = sign_init_data("123:secret", 99, Ts::now().0.as_second());
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header(header::HOST, "gym.example")
            .header("x-telegram-init-data", &wrong_user)
            .body(Body::empty())
            .unwrap();
        assert_eq!(handle(State(app.clone()), req).await.status(), StatusCode::UNAUTHORIZED);
        // Loopback Host header cannot bypass auth when WebAuth is configured.
        let app_lb = Arc::new(App {
            db_path: path.clone(),
            hosts: vec!["127.0.0.1:8766".into(), "gym.example".into()],
            auth: Some(WebAuth { bot_token: "123:secret".into(), owner_id: 42 }),
        });
        assert_eq!(get(&app_lb, Method::GET, "/api/state", "127.0.0.1:8766").await.status(), StatusCode::UNAUTHORIZED);
        // Expired and tampered initData are rejected.
        let expired = sign_init_data("123:secret", 42, Ts::now().0.as_second() - 90_000);
        assert!(!verify_telegram_init_data(&expired, app.auth.as_ref().unwrap(), Ts::now(), 86_400));
        let tampered = valid.replace("42", "43");
        assert!(!verify_telegram_init_data(&tampered, app.auth.as_ref().unwrap(), Ts::now(), 86_400));
        // ICS export works without initData for the right token, and 404s for a wrong token.
        assert_eq!(get(&app, Method::GET, "/calendar/wrong.ics", "gym.example").await.status(), StatusCode::NOT_FOUND);
        let ics = get(&app, Method::GET, &format!("/calendar/{token}.ics"), "gym.example").await;
        assert_eq!(ics.status(), StatusCode::OK);
        let body = String::from_utf8(axum::body::to_bytes(ics.into_body(), 65536).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("BEGIN:VCALENDAR") && body.contains("SUMMARY:Push"), "{body}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn live_endpoint_and_dashboard_guards() {
        let app = app();
        // /api/live returns lightweight snapshot without history or crowd series.
        let live = get(&app, Method::GET, "/api/live", "gym.example").await;
        assert_eq!(live.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(live.into_body(), 16384).await.unwrap()).unwrap();
        assert!(body.get("workout").is_some() && body.get("history").is_none() && body.get("crowd").is_none());
        // body_log forbids photo paths; typed kg works.
        assert_eq!(
            post(&app, "/api/body_log", r#"{"entries":[{"kg":80,"photo":"/etc/passwd"}]}"#, "same-origin")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post(&app, "/api/body_log", r#"{"entries":[{"kg":80.5}]}"#, "same-origin").await.status(),
            StatusCode::OK
        );
        // runtime_resume forbids granting calendar_writes from dashboard.
        assert_eq!(
            post(&app, "/api/runtime_resume", r#"{"owner_asked":true,"calendar_writes":true}"#, "same-origin")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let _ = std::fs::remove_dir_all(app.db_path.parent().unwrap());
    }
}
