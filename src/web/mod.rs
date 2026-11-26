//! Dashboard (`gymclaw dashboard`).
//!
//! Serves the frontend in `web/` (embedded, minified & pre-compressed in release builds with
//! content-hashed immutable URLs and ETags), exercise SVGs, `/api/state` (full snapshot),
//! `/api/today` (lightweight cold-boot snapshot), `/api/live` (3 s mid-workout poll),
//! `/api/catalog` and `/export.csv` (every logged set, `import::export_csv`). The
//! only writes are `DASHBOARD_TOOLS` (`POST /api/<tool>`), so the owner can edit what the agent
//! remembers and built, and plan targets, and `POST /api/coach`, the live workout's buttons and typed
//! sets (`coach::app_tap`; the bot then shows the step in the chat). Only the allowed Host (or loopback) gets an answer, so DNS
//! rebinding can't read it, and writes need a same-origin JSON request.
pub mod state;

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::time::Ts;

const ASSETS: [(&str, &[u8], &str); 7] = [
    ("/", include_bytes!("../../web/index.html"), "text/html"),
    ("/styles.css", include_bytes!("../../web/styles.css"), "text/css"),
    ("/app.js", include_bytes!("../../web/app.js"), "text/javascript"),
    ("/favicon.svg", include_bytes!("../../web/favicon.svg"), "image/svg+xml"),
    // Home screen icon: iOS ignores SVG icons and falls back to a letter without it.
    ("/apple-touch-icon.png", include_bytes!("../../web/apple-touch-icon.png"), "image/png"),
    (
        "/fonts/BarlowCondensed-SemiBold.woff2",
        include_bytes!("../../web/fonts/BarlowCondensed-SemiBold.woff2"),
        "font/woff2",
    ),
    ("/fonts/DM-Sans.woff2", include_bytes!("../../web/fonts/DM-Sans.woff2"), "font/woff2"),
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
/// The only profile fields the Settings tab edits directly.
const PROFILE_FIELDS: [&str; 3] = ["weight_unit", "rest_seconds", "timezone"];

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
    data_dir: PathBuf,
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

/// Verify Telegram Mini App `initData` (HMAC-SHA256 with `WebAppData` + bot token, auth_date within
/// `max_age_secs`) and return the verified Telegram user id.
pub fn verified_telegram_user_id(init_data: &str, bot_token: &str, now: Ts, max_age_secs: i64) -> Option<i64> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut hash_hex: Option<String> = None;
    for part in init_data.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=')?;
        let (k, v) = (percent_decode(k)?, percent_decode(v)?);
        if k == "hash" {
            hash_hex = Some(v);
        } else {
            pairs.push((k, v));
        }
    }
    let hash_bytes = hash_hex.and_then(|h| hex::decode(h).ok())?;
    let auth_date: i64 = pairs.iter().find(|(k, _)| k == "auth_date").and_then(|(_, v)| v.parse().ok())?;
    let age = now.0.as_second() - auth_date;
    if !(-300..=max_age_secs).contains(&age) {
        return None;
    }
    let user_id = pairs
        .iter()
        .find(|(k, _)| k == "user")
        .and_then(|(_, v)| serde_json::from_str::<serde_json::Value>(v).ok())
        .and_then(|u| u["id"].as_i64())?;
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let check_string = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");
    let secret = hmac_sha256(b"WebAppData", bot_token.as_bytes());
    let expected = hmac_sha256(&secret, check_string.as_bytes());
    ct_eq(&expected, &hash_bytes).then_some(user_id)
}

/// Verify Telegram Mini App `initData` against `auth.bot_token` and `auth.owner_id`.
pub fn verify_telegram_init_data(init_data: &str, auth: &WebAuth, now: Ts, max_age_secs: i64) -> bool {
    verified_telegram_user_id(init_data, &auth.bot_token, now, max_age_secs) == Some(auth.owner_id)
}

fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k == key).then(|| percent_decode(v)).flatten()
    })
}

fn authorized(auth: Option<&WebAuth>, headers: &HeaderMap, query: Option<&str>, allow_query: bool) -> bool {
    let Some(auth) = auth else { return true };
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

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| {
        v.split(',').any(|part| {
            let mut segs = part.split(';');
            if !segs.next().is_some_and(|enc| enc.trim().eq_ignore_ascii_case("gzip")) {
                return false;
            }
            for param in segs {
                if let Some((k, val)) = param.split_once('=')
                    && k.trim().eq_ignore_ascii_case("q")
                {
                    return val.trim().parse::<f32>().is_ok_and(|q| q > 0.0);
                }
            }
            true
        })
    })
}

fn is_compressible(mime: &str) -> bool {
    mime.starts_with("text/") || mime == "application/json" || mime == "image/svg+xml"
}

fn gzip_bytes(bytes: &[u8], level: Compression) -> Option<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::with_capacity(bytes.len() / 3), level);
    enc.write_all(bytes).ok()?;
    enc.finish().ok()
}

/// Strip `/* … */` block comments and leading/trailing line whitespace from CSS.
fn minify_css(input: &str) -> String {
    let mut no_comments = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("/*") {
        no_comments.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    no_comments.push_str(rest);
    no_comments.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// Strip `<!-- … -->` comments and leading/trailing line whitespace from HTML.
fn minify_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// Strip full-line `//` comments, top-level `/* … */` comment blocks, and line indentation from JS
/// without touching strings or regex literals. Assumes `web/app.js` has no multi-line template
/// literals whose leading whitespace or `//`-prefixed lines are significant.
fn minify_js(input: &str) -> String {
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in input.lines() {
        let line = raw.trim();
        if in_block {
            if let Some(end) = line.find("*/") {
                in_block = false;
                let tail = line[end + 2..].trim();
                if !tail.is_empty() && !tail.starts_with("//") {
                    out.push(tail);
                }
            }
            continue;
        }
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(after_open) = line.strip_prefix("/*") {
            if let Some(end) = after_open.find("*/") {
                let tail = after_open[end + 2..].trim();
                if !tail.is_empty() && !tail.starts_with("//") {
                    out.push(tail);
                }
            } else {
                in_block = true;
            }
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

fn short_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(&digest[..6])
}

fn weak_etag(tag: &str) -> String {
    format!("W/\"{tag}\"")
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let bare = etag.strip_prefix("W/").unwrap_or(etag);
    inm.split(',').any(|item| {
        let item = item.trim();
        item == "*" || item == etag || item.strip_prefix("W/").unwrap_or(item) == bare
    })
}

fn set_security_headers(headers: &mut HeaderMap) {
    let set = |h: &mut HeaderMap, k, v: &str| {
        h.insert(k, HeaderValue::from_str(v).expect("header value"));
    };
    set(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(headers, header::HeaderName::from_static("cross-origin-resource-policy"), "same-origin");
    set(
        headers,
        header::CONTENT_SECURITY_POLICY,
        // telegram.org: the Mini App SDK (index.html), which Telegram asks apps to load from there.
        // data: in img-src allows tiny inline blurred exercise placeholders (LQIP) with no extra requests.
        "default-src 'self'; style-src 'self'; script-src 'self' https://telegram.org; connect-src 'self'; img-src 'self' data:; font-src 'self'; frame-ancestors https://web.telegram.org",
    );
}

fn reply(status: StatusCode, body: Vec<u8>, mime: &str) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    let content_type = if is_compressible(mime) { format!("{mime}; charset=utf-8") } else { mime.to_string() };
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).expect("content-type"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_security_headers(headers);
    response
}

fn reply_cached(
    req_headers: &HeaderMap,
    body: Vec<u8>,
    precompressed: Option<&[u8]>,
    mime: &str,
    cache_control: &'static str,
    etag_seed: Option<&[u8]>,
) -> Response {
    let etag = weak_etag(&short_hash(etag_seed.unwrap_or(&body)));
    if etag_matches(req_headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
        headers.insert(header::ETAG, HeaderValue::from_str(&etag).expect("etag"));
        if is_compressible(mime) {
            headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        }
        set_security_headers(headers);
        return response;
    }
    let want_gz = is_compressible(mime) && body.len() >= 256 && accepts_gzip(req_headers);
    let (payload, encoded) = if want_gz {
        if let Some(gz) = precompressed {
            (gz.to_vec(), true)
        } else if let Some(gz) = gzip_bytes(&body, Compression::default()) {
            (gz, true)
        } else {
            (body, false)
        }
    } else {
        (body, false)
    };
    let mut response = reply(StatusCode::OK, payload, mime);
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    headers.insert(header::ETAG, HeaderValue::from_str(&etag).expect("etag"));
    if is_compressible(mime) {
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    if encoded {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    response
}

/// Strip the volatile `"captured_at":"…"` timestamp when hashing API JSON so unchanged polls
/// return 0-byte `304 Not Modified` instead of resending the whole snapshot.
fn stable_json_hash(snapshot: &serde_json::Value) -> Vec<u8> {
    let mut clone = snapshot.clone();
    if let Some(obj) = clone.as_object_mut() {
        obj.remove("captured_at");
    }
    serde_json::to_vec(&clone).unwrap_or_default()
}

fn error(status: StatusCode) -> Response {
    reply(status, status.canonical_reason().unwrap_or("").as_bytes().to_vec(), "text/plain")
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    handle_request(app.db_path.clone(), app.data_dir.clone(), &app.hosts, app.auth.as_ref(), request).await
}

/// Handle one dashboard HTTP request against the given DB and data directory.
pub async fn handle_request(
    db_path: PathBuf,
    data_dir: PathBuf,
    hosts: &[String],
    auth: Option<&WebAuth>,
    request: Request,
) -> Response {
    if let Some(response) = serve_public(hosts, &request) {
        return response;
    }
    let headers = request.headers();
    if request.method() == Method::POST {
        if !authorized(auth, headers, request.uri().query(), false) {
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
        // The app logs one typed weigh-in (`kg` only), never photo paths or backfill batches.
        if tool == "body_log" {
            let valid = args.get("entries").and_then(|e| e.as_array()).is_some_and(|es| {
                es.len() == 1 && es[0].as_object().is_some_and(|o| o.len() == 1 && o.contains_key("kg"))
            });
            if !valid {
                return error(StatusCode::FORBIDDEN);
            }
        }
        // Settings edits only weight_unit and rest_seconds; schedule constraints stay with the agent.
        if tool == "profile_update" {
            let valid = args
                .get("changes")
                .and_then(|c| c.as_object())
                .is_some_and(|c| !c.is_empty() && c.keys().all(|k| PROFILE_FIELDS.contains(&k.as_str())));
            if !valid {
                return error(StatusCode::FORBIDDEN);
            }
        }
        // The app toggles reminders only; crowd polling stays with the agent.
        if tool == "runtime_resume" && args.get("crowd_polling").is_some_and(|v| !v.is_null()) {
            return error(StatusCode::FORBIDDEN);
        }
        let recipient = auth.map(|a| a.owner_id.to_string());
        let result = tokio::task::spawn_blocking(move || {
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
                let mut cx = crate::tools::Ctx {
                    db,
                    now: Ts::now(),
                    request_id,
                    data_dir: &data_dir,
                    recipient: recipient.clone(),
                    effects: vec![],
                };
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
    if (path.starts_with("/api/") || path == "/export.csv") && !authorized(auth, headers, query, path == "/export.csv")
    {
        return error(StatusCode::UNAUTHORIZED);
    }
    if path == "/api/live" {
        let req_headers = headers.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = crate::db::open_readonly(&db_path)?;
            let snapshot = state::live_snapshot(&db, Ts::now())?;
            let seed = stable_json_hash(&snapshot);
            Ok::<_, crate::error::Error>((serde_json::to_vec(&snapshot)?, seed))
        })
        .await;
        return match result {
            Ok(Ok((body, seed))) => {
                reply_cached(&req_headers, body, None, "application/json", "private, no-cache", Some(&seed))
            }
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
    if path == "/api/today" {
        let req_headers = headers.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = crate::db::open_readonly(&db_path)?;
            let snapshot = state::today_snapshot(&db, Ts::now())?;
            let seed = stable_json_hash(&snapshot);
            Ok::<_, crate::error::Error>((serde_json::to_vec(&snapshot)?, seed))
        })
        .await;
        return match result {
            Ok(Ok((body, seed))) => {
                reply_cached(&req_headers, body, None, "application/json", "private, no-cache", Some(&seed))
            }
            Ok(Err(e)) => {
                tracing::warn!("dashboard today: {} {e}", e.code());
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
            Err(e) => {
                tracing::warn!("dashboard today panicked: {e}");
                error(StatusCode::SERVICE_UNAVAILABLE)
            }
        };
    }
    if path == "/api/state" {
        let req_headers = headers.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = crate::db::open_readonly(&db_path)?;
            let snapshot = state::snapshot(&db, Ts::now())?;
            let seed = stable_json_hash(&snapshot);
            Ok::<_, crate::error::Error>((serde_json::to_vec(&snapshot)?, seed))
        })
        .await;
        return match result {
            Ok(Ok((body, seed))) => {
                reply_cached(&req_headers, body, None, "application/json", "private, no-cache", Some(&seed))
            }
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
        static CATALOG_BYTES: LazyLock<(Vec<u8>, Option<Vec<u8>>)> = LazyLock::new(|| {
            let body = serde_json::to_vec(&state::catalog()).unwrap_or_default();
            let gz = gzip_bytes(&body, Compression::best());
            (body, gz)
        });
        let (body, gz) = &*CATALOG_BYTES;
        return reply_cached(headers, body.clone(), gz.as_deref(), "application/json", "private, max-age=86400", None);
    }
    error(StatusCode::NOT_FOUND)
}

fn host_allowed(hosts: &[String], headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let cross_site = headers.get("sec-fetch-site").is_some_and(|v| v == "cross-site");
    hosts.iter().any(|h| h == host) && !cross_site
}

struct PreparedAsset {
    body: Vec<u8>,
    gzip: Option<Vec<u8>>,
    hash: String,
    mime: &'static str,
}

fn build_assets() -> HashMap<&'static str, PreparedAsset> {
    let mut raw: HashMap<&'static str, (Vec<u8>, &'static str)> =
        ASSETS.iter().map(|(route, embedded, mime)| (*route, (asset(route, embedded), *mime))).collect();

    // Hash leaf assets first so CSS can version font URLs and HTML can version CSS/JS/icon URLs.
    // Skip minification in debug builds so DevTools line numbers match `web/` on disk.
    let minify = !cfg!(debug_assertions);
    let font_barlow_hash = short_hash(&raw["/fonts/BarlowCondensed-SemiBold.woff2"].0);
    let font_dm_hash = short_hash(&raw["/fonts/DM-Sans.woff2"].0);
    if let Some((css_bytes, _)) = raw.get_mut("/styles.css")
        && let Ok(css) = String::from_utf8(css_bytes.clone())
    {
        let base = if minify { minify_css(&css) } else { css };
        let versioned = base
            .replace(
                "/fonts/BarlowCondensed-SemiBold.woff2",
                &format!("/fonts/BarlowCondensed-SemiBold.woff2?v={font_barlow_hash}"),
            )
            .replace("/fonts/DM-Sans.woff2", &format!("/fonts/DM-Sans.woff2?v={font_dm_hash}"));
        *css_bytes = versioned.into_bytes();
    }
    if minify
        && let Some((js_bytes, _)) = raw.get_mut("/app.js")
        && let Ok(js) = String::from_utf8(js_bytes.clone())
    {
        *js_bytes = minify_js(&js).into_bytes();
    }

    let css_hash = short_hash(&raw["/styles.css"].0);
    let js_hash = short_hash(&raw["/app.js"].0);
    let fav_hash = short_hash(&raw["/favicon.svg"].0);
    let icon_hash = short_hash(&raw["/apple-touch-icon.png"].0);
    if let Some((html_bytes, _)) = raw.get_mut("/")
        && let Ok(html) = String::from_utf8(html_bytes.clone())
    {
        let base = if minify { minify_html(&html) } else { html };
        let versioned = base
            .replace("href=\"/styles.css\"", &format!("href=\"/styles.css?v={css_hash}\""))
            .replace("src=\"/app.js\"", &format!("src=\"/app.js?v={js_hash}\""))
            .replace("href=\"/favicon.svg\"", &format!("href=\"/favicon.svg?v={fav_hash}\""))
            .replace("href=\"/apple-touch-icon.png\"", &format!("href=\"/apple-touch-icon.png?v={icon_hash}\""));
        *html_bytes = versioned.into_bytes();
    }

    let level = if minify { Compression::best() } else { Compression::fast() };
    raw.into_iter()
        .map(|(route, (body, mime))| {
            let hash = short_hash(&body);
            let gzip = (is_compressible(mime) && body.len() >= 256).then(|| gzip_bytes(&body, level)).flatten();
            (route, PreparedAsset { body, gzip, hash, mime })
        })
        .collect()
}

static RELEASE_ASSETS: LazyLock<HashMap<&'static str, PreparedAsset>> = LazyLock::new(build_assets);

fn public_response(path: &str, query: Option<&str>, req_headers: &HeaderMap) -> Option<Response> {
    if let Some(slug) = path.strip_prefix("/illustrations/").and_then(|p| p.strip_suffix(".svg")) {
        return Some(match crate::domain::illustrations::svg_path(slug).map(std::fs::read) {
            Some(Ok(bytes)) => {
                // Without a cache, every dashboard redraw (each 10 s poll that changed the state) downloaded
                // every picture again. A redrawn picture shows within a day.
                reply_cached(req_headers, bytes, None, "image/svg+xml", "private, max-age=86400", None)
            }
            _ => error(StatusCode::NOT_FOUND),
        });
    }
    let serve_prepared = |a: &PreparedAsset| {
        let cc = if path != "/" && query_param(query, "v").as_deref() == Some(a.hash.as_str()) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        reply_cached(req_headers, a.body.clone(), a.gzip.as_deref(), a.mime, cc, None)
    };
    if cfg!(debug_assertions) {
        let map = build_assets();
        map.get(path).map(serve_prepared)
    } else {
        RELEASE_ASSETS.get(path).map(serve_prepared)
    }
}

/// Serve static dashboard assets (HTML/CSS/JS/fonts and `/illustrations/*.svg`) without requiring
/// a database path, after enforcing the Host and `sec-fetch-site` checks. Returns `None` for
/// non-static or non-GET routes so the caller can forward them to `handle_request`.
pub fn serve_public(hosts: &[String], request: &Request) -> Option<Response> {
    if !host_allowed(hosts, request.headers()) {
        return Some(error(StatusCode::FORBIDDEN));
    }
    if request.method() != Method::GET {
        return None;
    }
    public_response(request.uri().path(), request.uri().query(), request.headers())
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

/// Build the dashboard axum `Router` without binding a TCP listener.
/// Requests whose `Host` header isn't in `hosts` are rejected.
pub fn router(db_path: PathBuf, data_dir: PathBuf, hosts: Vec<String>, auth: Option<WebAuth>) -> Router {
    Router::new().fallback(handle).with_state(Arc::new(App { db_path, data_dir, hosts, auth }))
}

/// Serve the dashboard: reads from a read-only connection, `DASHBOARD_TOOLS` writes through a writable one.
/// Requests whose Host isn't `allowed_host` are rejected.
pub async fn dashboard(
    db_path: PathBuf,
    data_dir: PathBuf,
    addr: SocketAddr,
    allowed_host: String,
    auth: Option<WebAuth>,
) -> Result<()> {
    let port = addr.port();
    let hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}"), allowed_host];
    let app = router(db_path, data_dir, hosts, auth);
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
        let data_dir = path.parent().unwrap().to_path_buf();
        Arc::new(App { db_path: path, data_dir, hosts: vec!["gym.example".into()], auth: None })
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
        assert_eq!(index.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(index.headers().contains_key(header::ETAG));
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
        let data_dir = path.parent().unwrap().to_path_buf();
        let app = Arc::new(App {
            db_path: path.clone(),
            data_dir: data_dir.clone(),
            hosts: vec!["gym.example".into()],
            auth: Some(auth),
        });
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
            data_dir,
            hosts: vec!["127.0.0.1:8766".into(), "gym.example".into()],
            auth: Some(WebAuth { bot_token: "123:secret".into(), owner_id: 42 }),
        });
        assert_eq!(get(&app_lb, Method::GET, "/api/state", "127.0.0.1:8766").await.status(), StatusCode::UNAUTHORIZED);
        // Expired and tampered initData are rejected.
        let expired = sign_init_data("123:secret", 42, Ts::now().0.as_second() - 90_000);
        assert!(!verify_telegram_init_data(&expired, app.auth.as_ref().unwrap(), Ts::now(), 86_400));
        let tampered = valid.replace("42", "43");
        assert!(!verify_telegram_init_data(&tampered, app.auth.as_ref().unwrap(), Ts::now(), 86_400));
        assert_eq!(verified_telegram_user_id(&valid, "123:secret", Ts::now(), 86_400), Some(42));
        assert_eq!(verified_telegram_user_id(&tampered, "123:secret", Ts::now(), 86_400), None);
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
        // runtime_resume forbids toggling crowd polling from dashboard.
        assert_eq!(
            post(&app, "/api/runtime_resume", r#"{"owner_asked":true,"crowd_polling":true}"#, "same-origin")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        // profile_update allows weight_unit/rest_seconds/timezone and forbids other fields.
        assert_eq!(
            post(&app, "/api/profile_update", r#"{"changes":{"weekly_target_sessions":5}}"#, "same-origin")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post(&app, "/api/profile_update", r#"{"changes":{"timezone":"America/New_York"}}"#, "same-origin")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            post(&app, "/api/profile_update", r#"{"changes":{"weight_unit":"lb"}}"#, "same-origin").await.status(),
            StatusCode::OK
        );
        let _ = std::fs::remove_dir_all(app.db_path.parent().unwrap());
    }

    #[tokio::test]
    async fn compression_etags_immutable_assets_and_today_endpoint() {
        let app = app();
        // Index rewrites asset URLs with ?v=<hash>, compresses with gzip, and returns 304 on matching ETag.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(header::HOST, "gym.example")
            .header(header::ACCEPT_ENCODING, "gzip, deflate")
            .body(Body::empty())
            .unwrap();
        let res = handle(State(app.clone()), req).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(res.headers()[header::VARY], "Accept-Encoding");
        let etag = res.headers()[header::ETAG].to_str().unwrap().to_string();

        let reval = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(header::HOST, "gym.example")
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        let res304 = handle(State(app.clone()), reval).await;
        assert_eq!(res304.status(), StatusCode::NOT_MODIFIED);

        // Uncompressed index contains versioned URLs for all four static assets, each served immutable,
        // and CSS contains versioned URLs for both WOFF2 fonts.
        let raw_index = get(&app, Method::GET, "/", "gym.example").await;
        let html =
            String::from_utf8(axum::body::to_bytes(raw_index.into_body(), 65536).await.unwrap().to_vec()).unwrap();
        for prefix in ["/styles.css?v=", "/app.js?v=", "/favicon.svg?v=", "/apple-touch-icon.png?v="] {
            let start = html.find(prefix).unwrap_or_else(|| panic!("missing versioned {prefix}"));
            let url = &html[start..start + html[start..].find('"').unwrap()];
            let asset_res = get(&app, Method::GET, url, "gym.example").await;
            assert_eq!(asset_res.status(), StatusCode::OK, "{url}");
            assert_eq!(asset_res.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable", "{url}");
            if prefix == "/styles.css?v=" {
                assert!(
                    asset_res.headers()[header::CONTENT_SECURITY_POLICY]
                        .to_str()
                        .unwrap()
                        .contains("img-src 'self' data:")
                );
                let css = String::from_utf8(axum::body::to_bytes(asset_res.into_body(), 65536).await.unwrap().to_vec())
                    .unwrap();
                for font in ["/fonts/BarlowCondensed-SemiBold.woff2?v=", "/fonts/DM-Sans.woff2?v="] {
                    let fstart = css.find(font).unwrap_or_else(|| panic!("missing versioned {font}"));
                    let furl = &css[fstart..fstart + css[fstart..].find('\'').unwrap()];
                    let fres = get(&app, Method::GET, furl, "gym.example").await;
                    assert_eq!(fres.status(), StatusCode::OK, "{furl}");
                    assert_eq!(fres.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable", "{furl}");
                }
            }
        }

        // /api/today returns lightweight Today payload without history/insights, and supports 304.
        let today = get(&app, Method::GET, "/api/today", "gym.example").await;
        assert_eq!(today.status(), StatusCode::OK);
        let today_etag = today.headers()[header::ETAG].to_str().unwrap().to_string();
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(today.into_body(), 65536).await.unwrap()).unwrap();
        assert!(body.get("days").is_some() && body.get("crowd").is_some() && body.get("previews").is_some());
        assert!(body.get("history").is_none() && body.get("insights").is_none());

        let today_reval = Request::builder()
            .method(Method::GET)
            .uri("/api/today")
            .header(header::HOST, "gym.example")
            .header(header::IF_NONE_MATCH, &today_etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(handle(State(app.clone()), today_reval).await.status(), StatusCode::NOT_MODIFIED);

        let _ = std::fs::remove_dir_all(app.db_path.parent().unwrap());
    }

    #[test]
    fn accepts_gzip_etag_and_minifiers_handle_edge_cases() {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br, gzip;q=0"));
        assert!(!accepts_gzip(&h));
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("deflate, gzip;q=0.5"));
        assert!(accepts_gzip(&h));

        let mut et = HeaderMap::new();
        et.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"abc\", W/\"def\""));
        assert!(etag_matches(&et, "W/\"abc\""));
        assert!(etag_matches(&et, "W/\"def\""));
        assert!(!etag_matches(&et, "W/\"xyz\""));
        et.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(etag_matches(&et, "W/\"anything\""));

        assert_eq!(minify_css("body { color: red; } /* unclosed"), "body { color: red; }");
        assert_eq!(minify_html("<div>ok</div><!-- unclosed"), "<div>ok</div>");
        assert_eq!(
            minify_js("  // comment\n/* block\ncomment */ const x = 1;\n  const y = 2;"),
            "const x = 1;\nconst y = 2;"
        );
    }

    #[tokio::test]
    async fn handle_request_serves_without_router_and_takes_explicit_data_dir() {
        let (db_path, _) = crate::db::test::file();
        let data_dir = std::env::temp_dir().join(format!("gymclaw-datadir-{}", crate::db::new_id()));
        let hosts = vec!["gym.example".to_string()];
        let request = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header(header::HOST, "gym.example")
            .body(Body::empty())
            .unwrap();
        let response = handle_request(db_path, data_dir, &hosts, None, request).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn serve_public_handles_assets_and_defers_db_routes() {
        let hosts = vec!["gym.example".to_string()];
        let req = |uri: &str, host: &str| {
            Request::builder().method(Method::GET).uri(uri).header(header::HOST, host).body(Body::empty()).unwrap()
        };
        assert_eq!(serve_public(&hosts, &req("/", "gym.example")).unwrap().status(), StatusCode::OK);
        assert_eq!(serve_public(&hosts, &req("/styles.css", "gym.example")).unwrap().status(), StatusCode::OK);
        assert_eq!(serve_public(&hosts, &req("/", "evil.example")).unwrap().status(), StatusCode::FORBIDDEN);
        assert!(serve_public(&hosts, &req("/api/catalog", "gym.example")).is_none());
        assert!(serve_public(&hosts, &req("/api/state", "gym.example")).is_none());
    }
}
