//! Google Calendar v3 REST + OAuth refresh (port of `google_auth.py` and `google_calendar.py`).
//! No implicit primary calendar.
//!
//! Sync uses stable list params and pages without time filters. Recurring masters and exceptions
//! are expanded locally (`recurrence::occurrences`); syncToken can't be combined with timeMin/timeMax.
//!
//! Credentials are the google-auth `Credentials.to_json()` dict in `calendar_credential.credentials_json`.
//! A refreshed token is written back in that format, so the Python app can still read it.
//! Consent (`gymclaw calendar-auth`) is a paste flow: the owner opens the URL anywhere, approves, and
//! pastes back the loopback URL the browser lands on, so it also works inside the container.
use std::collections::HashSet;

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use super::calendar::{CalendarEvent, CalendarProvider, SyncBatch, already_applied, body_session_id, conflict, parse_event, sync_token_expired};
use super::http::{Body, Method, Request, Response, Transport};
use crate::bail;
use crate::db::Json;
use crate::db::models::{CalendarCredential, CalendarSyncState};
use crate::error::{Error, Result};
use crate::time::Ts;

pub const SCOPES: [&str; 1] = ["https://www.googleapis.com/auth/calendar.events.owned"];
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// google-auth treats a token as expired this long before its expiry.
const REFRESH_THRESHOLD_SECS: i64 = 3 * 60 + 45;

/// Bind the DB to one dedicated calendar and provider; a different one needs a separate DB.
pub fn bind_calendar(db: &Connection, calendar_id: &str, source: &str) -> Result<CalendarSyncState> {
    if calendar_id.is_empty() || calendar_id == "primary" {
        bail!("CALENDAR_ID_REQUIRED", "Specify dedicated calendar ID; primary is not an implicit fallback");
    }
    if let Some(state) = CalendarSyncState::get(db, 1)? {
        if state.calendar_id != calendar_id || state.source != source {
            bail!("CALENDAR_SCOPE_MISMATCH", "DB is bound to another calendar/provider; use a separate DB");
        }
        return Ok(state);
    }
    let state = CalendarSyncState {
        id: 1,
        calendar_id: calendar_id.into(),
        timezone: "Europe/Berlin".into(),
        sync_token: None,
        last_synced_at: None,
        source: source.into(),
    };
    state.insert(db)?;
    Ok(state)
}

/// OAuth secrets live only in a persistent, owner-only SQLite file.
fn private_storage(db: &Connection) -> Result<()> {
    let path = db.path().filter(|p| !p.is_empty() && *p != ":memory:");
    let Some(path) = path else {
        bail!("AUTH_STORAGE_REQUIRED", "OAuth requires a persistent private SQLite DB")
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// The stored google-auth dict. Unknown keys are kept as they are.
#[derive(Clone, Debug)]
pub struct Credentials(Map<String, Value>);

impl Credentials {
    fn text(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    fn has_scopes(&self) -> bool {
        let granted: Vec<&str> = match self.0.get("scopes") {
            Some(Value::String(s)) => s.split(' ').collect(),
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        SCOPES.iter().all(|s| granted.contains(s))
    }

    /// google-auth `expiry`: naive UTC, `…Z`; fractional seconds ignored on read.
    fn expiry(&self) -> Option<Ts> {
        let text = self.text("expiry")?;
        let text = text.trim_end_matches('Z').split('.').next()?;
        Ts::parse(&format!("{text}Z"), None).ok()
    }

    /// google-auth `valid`: a token that isn't within the refresh threshold of expiring. No expiry = never expires.
    fn valid(&self, now: Ts) -> bool {
        self.text("token").is_some() && self.expiry().is_none_or(|e| now < e - jiff::SignedDuration::from_secs(REFRESH_THRESHOLD_SECS))
    }

    /// Refresh-token grant against `token_uri`; updates token/refresh_token/expiry like google-auth.
    fn refresh(&mut self, transport: &dyn Transport, now: Ts) -> Result<()> {
        let fail = |why: &str| Error::domain("CALENDAR_REFRESH_FAILED", why.to_string());
        let refresh_token = self.text("refresh_token").ok_or_else(|| fail("Refresh token missing"))?.to_string();
        let mut form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("client_id".to_string(), self.text("client_id").unwrap_or_default().to_string()),
            ("client_secret".to_string(), self.text("client_secret").unwrap_or_default().to_string()),
            ("refresh_token".to_string(), refresh_token.clone()),
        ];
        let scopes: Vec<String> = match self.0.get("scopes") {
            Some(Value::String(s)) => s.split(' ').map(str::to_string).collect(),
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => vec![],
        };
        if !scopes.is_empty() {
            form.push(("scope".into(), scopes.join(" ")));
        }
        if let Some(rapt) = self.text("rapt_token") {
            form.push(("rapt".into(), rapt.to_string()));
        }
        let mut request = Request::new(Method::Post, self.text("token_uri").unwrap_or(TOKEN_URI), 120);
        request.body = Body::Form(form);
        let response = transport.send(&request)?;
        if response.status != 200 {
            return Err(fail("Token endpoint refused the refresh"));
        }
        let data = response.json()?;
        let token = data
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("No access token in response"))?;
        if let Some(granted) = data.get("scope").and_then(Value::as_str) {
            let granted: HashSet<&str> = granted.split(' ').collect();
            if !scopes.iter().all(|s| granted.contains(s.as_str())) {
                return Err(fail("Not all requested scopes were granted"));
            }
        }
        self.0.insert("token".into(), json!(token));
        let refresh_token = data.get("refresh_token").and_then(Value::as_str).unwrap_or(&refresh_token).to_string();
        self.0.insert("refresh_token".into(), json!(refresh_token));
        match data
            .get("expires_in")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        {
            Some(secs) => {
                // Python `naive_utc.isoformat() + "Z"`.
                let iso = (now + jiff::SignedDuration::from_secs(secs)).iso();
                self.0.insert("expiry".into(), json!(format!("{}Z", iso.trim_end_matches("+00:00"))));
            }
            None => {
                self.0.remove("expiry");
            }
        }
        Ok(())
    }

    fn token(&self) -> &str {
        self.text("token").unwrap_or_default()
    }
}

const AUTH_URI: &str = "https://accounts.google.com/o/oauth2/auth";

/// A Desktop-app OAuth client (`installed` in Google's client JSON) plus this attempt's PKCE verifier and state.
pub struct Consent {
    client_id: String,
    client_secret: String,
    token_uri: String,
    redirect_uri: String,
    verifier: String,
    state: String,
}

impl Consent {
    /// Parse the downloaded client JSON. Only Desktop clients allow the loopback redirect.
    pub fn new(client_json: &[u8]) -> Result<Consent> {
        let client: Value = serde_json::from_slice(client_json).map_err(|_| Error::domain("CALENDAR_AUTH_FAILED", "Client file is not JSON"))?;
        let Some(installed) = client.get("installed") else { bail!("CALENDAR_AUTH_FAILED", "Desktop app OAuth client required") };
        let text = |k: &str| installed.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let random = || uuid::Uuid::new_v4().simple().to_string();
        Ok(Consent {
            client_id: text("client_id"),
            client_secret: text("client_secret"),
            token_uri: Some(text("token_uri")).filter(|t| !t.is_empty()).unwrap_or_else(|| TOKEN_URI.into()),
            // Nothing listens here: the browser shows an error page whose URL the owner pastes back.
            redirect_uri: "http://127.0.0.1:1".into(),
            verifier: format!("{}{}", random(), random()),
            state: random(),
        })
    }

    /// Where the owner approves offline access to `calendar.events.owned`.
    pub fn url(&self) -> String {
        use base64::Engine;
        use sha2::Digest;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(self.verifier.as_bytes()));
        let params = [
            ("response_type", "code"), ("client_id", &self.client_id), ("redirect_uri", &self.redirect_uri), ("scope", SCOPES[0]),
            ("state", &self.state), ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("access_type", "offline"),
            ("prompt", "consent"), ("include_granted_scopes", "false"),
        ];
        let query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={}", quote(v))).collect();
        format!("{AUTH_URI}?{}", query.join("&"))
    }

    /// Exchange the code from the pasted redirect URL and store the credentials (google-auth format).
    /// Errors never echo the URL, code or tokens.
    pub fn finish(&self, db: &Connection, calendar_id: &str, pasted: &str, transport: &dyn Transport, now: Ts) -> Result<Value> {
        bind_calendar(db, calendar_id, "google")?;
        private_storage(db)?;
        let failed = || Error::domain("CALENDAR_AUTH_FAILED", "Google authorization failed. Check desktop client, test-user access and browser consent; retry.");
        let query = pasted.trim().split_once('?').map_or("", |(_, q)| q).split('#').next().unwrap_or_default();
        let param = |key: &str| query.split('&').find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| unquote(v)));
        if param("state").as_deref() != Some(self.state.as_str()) {
            return Err(failed());
        }
        let code = param("code").ok_or_else(failed)?;
        let mut request = Request::new(Method::Post, &self.token_uri, 120);
        request.body = Body::Form(
            [("grant_type", "authorization_code"), ("code", &code), ("client_id", &self.client_id), ("client_secret", &self.client_secret),
                ("redirect_uri", &self.redirect_uri), ("code_verifier", &self.verifier)]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .to_vec(),
        );
        let response = transport.send(&request).map_err(|_| failed())?;
        let data = response.json().ok().filter(|_| response.status == 200).ok_or_else(failed)?;
        let (Some(token), Some(refresh_token)) = (data.get("access_token").and_then(Value::as_str), data.get("refresh_token").and_then(Value::as_str)) else {
            return Err(failed());
        };
        let granted: HashSet<&str> = data.get("scope").and_then(Value::as_str).unwrap_or(SCOPES[0]).split(' ').collect();
        if !SCOPES.iter().all(|s| granted.contains(s)) {
            return Err(failed());
        }
        let mut credentials = json!({"token": token, "refresh_token": refresh_token, "token_uri": self.token_uri, "client_id": self.client_id,
            "client_secret": self.client_secret, "scopes": SCOPES, "universe_domain": "googleapis.com", "account": ""});
        if let Some(secs) = data.get("expires_in").and_then(Value::as_i64) {
            credentials["expiry"] = json!(format!("{}Z", (now + jiff::SignedDuration::from_secs(secs)).iso().trim_end_matches("+00:00")));
        }
        let row = CalendarCredential { id: 1, credentials_json: Json(credentials) };
        if CalendarCredential::get(db, 1)?.is_some() { row.update(db)? } else { row.insert(db)? }
        Ok(json!({"authorized": true, "calendar_id": calendar_id, "scopes": SCOPES, "storage": "private_sqlite", "calendar_events_changed": false}))
    }
}

/// Percent-decode a query value (`+` is a space).
fn unquote(text: &str) -> String {
    let mut out = Vec::new();
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hex: String = bytes.by_ref().take(2).map(char::from).collect();
                out.push(u8::from_str_radix(&hex, 16).unwrap_or(b'?'));
            }
            b => out.push(b),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Stored credentials, refreshed (and written back) when expired. Fails with CALENDAR_AUTH_REQUIRED.
pub fn load_credentials(db: &Connection, transport: &dyn Transport) -> Result<Credentials> {
    let Some(mut row) = CalendarCredential::get(db, 1)? else {
        bail!("CALENDAR_AUTH_REQUIRED", "Run `gymclaw calendar-auth` before Google sync")
    };
    private_storage(db)?;
    let expired = || Error::domain("CALENDAR_AUTH_REQUIRED", "Google credentials expired or invalid; run `gymclaw calendar-auth` again");
    let mut credentials = match &row.credentials_json.0 {
        Value::Object(map) => Credentials(map.clone()),
        _ => return Err(expired()),
    };
    if ["refresh_token", "client_id", "client_secret"].iter().any(|k| !credentials.0.contains_key(*k)) || !credentials.has_scopes() {
        return Err(expired());
    }
    if !credentials.valid(Ts::now()) {
        credentials.refresh(transport, Ts::now()).map_err(|_| expired())?;
        row.credentials_json = Json(Value::Object(credentials.0.clone()));
        row.update(db)?;
    }
    Ok(credentials)
}

/// Bearer auth for API calls: refreshes before a call when expired and after a 401 (twice at most,
/// like google-auth's `AuthorizedSession`), persisting each new token.
struct Session<'a> {
    credentials: Credentials,
    db: &'a Connection,
}

/// Google Calendar for one dedicated calendar. Without a session (tests), requests go out unauthenticated.
pub struct GoogleCalendarProvider<'a> {
    calendar_id: String,
    pub timezone: String,
    url: String,
    transport: &'a dyn Transport,
    session: Option<Session<'a>>,
}

/// Percent-encode everything but RFC 3986 unreserved characters (Python `quote(s, safe="")`).
fn quote(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The live provider for the DB's bound calendar (or `calendar_id`, which is then bound).
pub fn google_provider<'a>(db: &'a Connection, calendar_id: Option<&str>, transport: &'a dyn Transport) -> Result<GoogleCalendarProvider<'a>> {
    let calendar_id = match calendar_id {
        Some(id) => id.to_string(),
        None => match CalendarSyncState::get(db, 1)? {
            Some(state) => state.calendar_id,
            None => bail!("CALENDAR_ID_REQUIRED", "Authenticate dedicated calendar first"),
        },
    };
    let state = bind_calendar(db, &calendar_id, "google")?;
    let credentials = load_credentials(db, transport)?;
    let mut provider = GoogleCalendarProvider::new(&calendar_id, transport, &state.timezone)?;
    provider.session = Some(Session { credentials, db });
    Ok(provider)
}

impl<'a> GoogleCalendarProvider<'a> {
    pub fn new(calendar_id: &str, transport: &'a dyn Transport, timezone: &str) -> Result<GoogleCalendarProvider<'a>> {
        if calendar_id.is_empty() || calendar_id == "primary" {
            bail!("CALENDAR_ID_REQUIRED", "Dedicated calendar ID required");
        }
        Ok(GoogleCalendarProvider {
            calendar_id: calendar_id.into(),
            timezone: timezone.into(),
            url: format!("https://www.googleapis.com/calendar/v3/calendars/{}/events", quote(calendar_id)),
            transport,
            session: None,
        })
    }

    /// One API call. Transport and refresh failures never expose URLs, tokens or credentials.
    fn request(&mut self, mut request: Request) -> Result<Response> {
        let unavailable = || Error::domain("CALENDAR_UNAVAILABLE", "Calendar request failed; sync/retry before further changes.");
        let mut refreshes = 0;
        loop {
            if let Some(session) = &mut self.session {
                if !session.credentials.valid(Ts::now()) {
                    session.refresh(self.transport).map_err(|_| unavailable())?;
                }
                request.headers.retain(|(k, _)| k != "Authorization");
                request
                    .headers
                    .push(("Authorization".into(), format!("Bearer {}", session.credentials.token())));
            }
            let response = self.transport.send(&request).map_err(|_| unavailable())?;
            match &mut self.session {
                Some(session) if response.status == 401 && refreshes < 2 => {
                    refreshes += 1;
                    session.refresh(self.transport).map_err(|_| unavailable())?;
                }
                _ => return Ok(response),
            }
        }
    }

    fn event_request(&self, method: Method, event_id: Option<&str>) -> Request {
        let url = match event_id {
            Some(id) => format!("{}/{}", self.url, quote(id)),
            None => self.url.clone(),
        };
        Request::new(method, url, 30)
    }

    fn check(response: &Response) -> Result<()> {
        match response.status {
            412 => Err(conflict()),
            401 => bail!("CALENDAR_AUTH_REQUIRED", "Google authorization expired; run `gymclaw calendar-auth`"),
            s if s >= 400 => bail!("CALENDAR_UNAVAILABLE", "Google Calendar request failed (HTTP {s}); retry later"),
            _ => Ok(()),
        }
    }

    fn parse(&self, response: &Response) -> Result<CalendarEvent> {
        let body = response
            .json()
            .map_err(|_| Error::domain("CALENDAR_INVALID_RESPONSE", "Google returned invalid JSON"))?;
        parse_event(&body, &self.timezone)
    }

    fn owned(event: &CalendarEvent, session_id: Option<&str>) -> Result<()> {
        if !event.managed || session_id.is_none_or(str::is_empty) || event.session_id.as_deref() != session_id {
            bail!("CALENDAR_NOT_OWNED", "Refusing to modify event without matching GymClaw ownership");
        }
        Ok(())
    }
}

impl Session<'_> {
    fn refresh(&mut self, transport: &dyn Transport) -> Result<()> {
        self.credentials.refresh(transport, Ts::now())?;
        let row = CalendarCredential {
            id: 1,
            credentials_json: Json(Value::Object(self.credentials.0.clone())),
        };
        row.update(self.db)
    }
}

impl CalendarProvider for GoogleCalendarProvider<'_> {
    fn calendar_id(&self) -> &str {
        &self.calendar_id
    }

    fn source(&self) -> &'static str {
        "google"
    }

    fn incremental_sync(&mut self, sync_token: Option<&str>) -> Result<SyncBatch> {
        let mut query = vec![
            ("showDeleted".to_string(), "true".to_string()),
            ("singleEvents".into(), "false".into()),
            ("maxResults".into(), "2500".into()),
        ];
        if let Some(token) = sync_token.filter(|t| !t.is_empty()) {
            query.push(("syncToken".into(), token.into()));
        }
        let mut events = Vec::new();
        let mut seen_pages = HashSet::new();
        loop {
            let mut request = self.event_request(Method::Get, None);
            request.query = query.clone();
            let response = self.request(request)?;
            if response.status == 410 {
                return Err(sync_token_expired());
            }
            Self::check(&response)?;
            let raw = response
                .json()
                .map_err(|_| Error::domain("CALENDAR_INVALID_RESPONSE", "Google returned invalid JSON"))?;
            if let Some(tz) = raw.get("timeZone").and_then(Value::as_str) {
                self.timezone = tz.to_string();
            }
            for item in raw.get("items").and_then(Value::as_array).into_iter().flatten() {
                events.push(parse_event(item, &self.timezone)?);
            }
            match raw.get("nextPageToken").and_then(Value::as_str).filter(|p| !p.is_empty()) {
                None => {
                    let Some(token) = raw.get("nextSyncToken").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
                        bail!("CALENDAR_INVALID_RESPONSE", "Google sync response missing nextSyncToken");
                    };
                    return Ok(SyncBatch {
                        events,
                        next_sync_token: token.into(),
                        timezone: self.timezone.clone(),
                        full: sync_token.is_none(),
                    });
                }
                Some(page) => {
                    if !seen_pages.insert(page.to_string()) {
                        bail!("CALENDAR_INVALID_RESPONSE", "Google sync repeated page token");
                    }
                    query.retain(|(k, _)| k != "pageToken");
                    query.push(("pageToken".into(), page.into()));
                }
            }
        }
    }

    fn get_event(&mut self, event_id: &str) -> Result<Option<CalendarEvent>> {
        let response = self.request(self.event_request(Method::Get, Some(event_id)))?;
        if matches!(response.status, 404 | 410) {
            return Ok(None);
        }
        Self::check(&response)?;
        self.parse(&response).map(Some)
    }

    fn create_gym_event(&mut self, body: &Value) -> Result<CalendarEvent> {
        let intended = parse_event(body, &self.timezone)?;
        Self::owned(&intended, intended.session_id.as_deref())?;
        let mut request = self.event_request(Method::Post, None);
        request.body = Body::Json(body.clone());
        let response = self.request(request)?;
        if response.status == 409 {
            let existing = match self.get_event(&intended.id)? {
                Some(event) if !event.cancelled() => event,
                _ => bail!("CALENDAR_EVENT_DELETED", "Deleted event ID will not be recreated"),
            };
            Self::owned(&existing, intended.session_id.as_deref())?;
            return Ok(existing);
        }
        Self::check(&response)?;
        self.parse(&response)
    }

    fn update_gym_event(&mut self, event_id: &str, body: &Value, etag: &str) -> Result<CalendarEvent> {
        let event = match self.get_event(event_id)? {
            Some(event) if !event.cancelled() => event,
            _ => bail!("CALENDAR_EVENT_DELETED", "Event was deleted; sync before updating"),
        };
        Self::owned(&event, body_session_id(body))?;
        if event.etag.as_deref() != Some(etag) {
            if already_applied(&event, body, &self.timezone)? {
                return Ok(event); // Response lost after successful patch: acknowledge, don't duplicate.
            }
            return Err(conflict());
        }
        let mut patch = body.clone();
        if let Some(map) = patch.as_object_mut() {
            map.remove("id");
        }
        let mut request = self.event_request(Method::Patch, Some(event_id)).header("If-Match", etag);
        request.body = Body::Json(patch);
        let response = self.request(request)?;
        Self::check(&response)?;
        self.parse(&response)
    }

    fn delete_gym_event(&mut self, event_id: &str, etag: &str, session_id: &str) -> Result<()> {
        let event = match self.get_event(event_id)? {
            Some(event) if !event.cancelled() => event,
            _ => return Ok(()),
        };
        Self::owned(&event, Some(session_id))?;
        if event.etag.as_deref() != Some(etag) {
            return Err(conflict());
        }
        let response = self.request(self.event_request(Method::Delete, Some(event_id)).header("If-Match", etag))?;
        if !matches!(response.status, 404 | 410) {
            Self::check(&response)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    fn raw_event(session_id: &str, changes: Value) -> Value {
        let mut event = json!({"id": "owned", "etag": "\"v1\"", "summary": "Gym", "start": {"dateTime": "2026-10-14T20:00:00+02:00"},
            "end": {"dateTime": "2026-10-14T21:10:00+02:00"},
            "extendedProperties": {"private": {"gymclawManaged": "true", "gymclawSessionId": session_id, "gymclawPlanRevision": "1"}}});
        for (k, v) in changes.as_object().unwrap() {
            event[k] = v.clone();
        }
        event
    }

    /// Scripted responses; records each request.
    struct Script {
        responses: RefCell<VecDeque<Result<Response>>>,
        calls: RefCell<Vec<Request>>,
    }

    impl Script {
        fn new(responses: Vec<Result<Response>>) -> Script {
            Script {
                responses: RefCell::new(responses.into()),
                calls: RefCell::new(vec![]),
            }
        }
    }

    impl Transport for Script {
        fn send(&self, request: &Request) -> Result<Response> {
            self.calls.borrow_mut().push(request.clone());
            self.responses.borrow_mut().pop_front().expect("scripted response")
        }
    }

    fn ok(status: u16, body: Value) -> Result<Response> {
        Ok(Response {
            status,
            body: body.to_string().into_bytes(),
        })
    }

    #[test]
    fn pagination_keeps_sync_params_and_quotes_calendar() {
        let script = Script::new(vec![
            ok(
                200,
                json!({"timeZone": "Europe/Berlin", "items": [raw_event("s", json!({}))], "nextPageToken": "page"}),
            ),
            ok(200, json!({"items": [], "nextSyncToken": "new"})),
        ]);
        let mut provider = GoogleCalendarProvider::new("only-this@group.calendar.google.com", &script, "Europe/Berlin").unwrap();
        let batch = provider.incremental_sync(Some("old")).unwrap();
        assert!(batch.events.len() == 1 && batch.next_sync_token == "new" && !batch.full);
        let calls = script.calls.borrow();
        let pairs = |r: &Request| r.query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>();
        assert_eq!(pairs(&calls[0]), ["showDeleted=true", "singleEvents=false", "maxResults=2500", "syncToken=old"]);
        assert_eq!(
            pairs(&calls[1]),
            ["showDeleted=true", "singleEvents=false", "maxResults=2500", "syncToken=old", "pageToken=page"]
        );
        assert!(calls.iter().all(|c| c.url.contains("only-this%40group.calendar.google.com")));
    }

    #[test]
    fn expired_token_and_safe_errors() {
        let script = Script::new(vec![ok(410, json!({}))]);
        let error = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin")
            .unwrap()
            .incremental_sync(Some("old"))
            .unwrap_err();
        assert_eq!(error.code(), "CALENDAR_SYNC_TOKEN_EXPIRED");
        let script = Script::new(vec![Err(Error::domain("X", "SECRET_URL_AND_TOKEN"))]);
        let error = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin")
            .unwrap()
            .incremental_sync(None)
            .unwrap_err();
        assert!(error.code() == "CALENDAR_UNAVAILABLE" && !error.to_string().contains("SECRET"));
    }

    #[test]
    fn create_retry_verifies_ownership() {
        let script = Script::new(vec![ok(409, json!({})), ok(200, raw_event("s", json!({})))]);
        let mut provider = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin").unwrap();
        assert_eq!(provider.create_gym_event(&raw_event("s", json!({}))).unwrap().id, "owned");
        let script = Script::new(vec![ok(409, json!({})), ok(200, raw_event("other", json!({})))]);
        let mut provider = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin").unwrap();
        assert_eq!(provider.create_gym_event(&raw_event("s", json!({}))).unwrap_err().code(), "CALENDAR_NOT_OWNED");
    }

    #[test]
    fn update_uses_if_match_and_detects_conflicts() {
        let script = Script::new(vec![ok(200, raw_event("s", json!({}))), ok(200, raw_event("s", json!({"etag": "\"v2\""})))]);
        let mut provider = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin").unwrap();
        assert_eq!(
            provider
                .update_gym_event("owned", &raw_event("s", json!({})), "\"v1\"")
                .unwrap()
                .etag
                .as_deref(),
            Some("\"v2\"")
        );
        let patch = &script.calls.borrow()[1];
        assert!(patch.method == Method::Patch && patch.header_value("If-Match") == Some("\"v1\""));
        assert!(matches!(&patch.body, Body::Json(b) if b.get("id").is_none()));

        // Remote moved it: conflict. Remote already has our revision and times: acknowledged.
        let moved = raw_event("s", json!({"etag": "\"v2\"", "start": {"dateTime": "2026-10-14T20:30:00+02:00"}}));
        let script = Script::new(vec![ok(200, moved)]);
        let mut provider = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin").unwrap();
        assert_eq!(
            provider.update_gym_event("owned", &raw_event("s", json!({})), "\"v1\"").unwrap_err().code(),
            "CALENDAR_WRITE_CONFLICT"
        );
        let script = Script::new(vec![ok(200, raw_event("s", json!({"etag": "\"v2\""})))]);
        let mut provider = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin").unwrap();
        assert!(provider.update_gym_event("owned", &raw_event("s", json!({})), "\"v1\"").is_ok());
        assert_eq!(script.calls.borrow().len(), 1);
    }

    #[test]
    fn refresh_writes_google_auth_format_back() {
        let dir = std::env::temp_dir().join(format!("gymclaw-oauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("private.db");
        let _ = std::fs::remove_file(&path);
        let mut db = crate::db::open(&path).unwrap();
        crate::db::migrate(&mut db).unwrap();
        let stored = json!({"token": "old", "refresh_token": "r1", "token_uri": "https://oauth2.googleapis.com/token", "client_id": "c",
            "client_secret": "s", "scopes": SCOPES, "universe_domain": "googleapis.com", "account": "", "expiry": "2020-01-01T00:00:00Z"});
        CalendarCredential {
            id: 1,
            credentials_json: Json(stored),
        }
        .insert(&db)
        .unwrap();
        bind_calendar(&db, "dedicated", "google").unwrap();
        let script = Script::new(vec![
            ok(
                200,
                json!({"access_token": "fresh", "expires_in": 3599, "scope": SCOPES[0], "token_type": "Bearer"}),
            ),
            ok(200, json!({"items": [], "nextSyncToken": "t"})),
        ]);
        let mut provider = google_provider(&db, None, &script).unwrap();
        provider.incremental_sync(None).unwrap();
        let calls = script.calls.borrow();
        assert!(matches!(&calls[0].body, Body::Form(f) if f.contains(&("refresh_token".into(), "r1".into()))));
        assert_eq!(calls[1].header_value("Authorization"), Some("Bearer fresh"));
        let saved = CalendarCredential::get(&db, 1).unwrap().unwrap().credentials_json.0;
        assert_eq!(
            (saved["token"].as_str(), saved["refresh_token"].as_str(), saved["universe_domain"].as_str()),
            (Some("fresh"), Some("r1"), Some("googleapis.com"))
        );
        assert!(Credentials(saved.as_object().unwrap().clone()).valid(Ts::now()));
        assert_eq!(bind_calendar(&db, "different", "google").unwrap_err().code(), "CALENDAR_SCOPE_MISMATCH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_guard_refuses_events_gymclaw_does_not_own() {
        let script = Script::new(vec![ok(200, raw_event("other", json!({})))]);
        let mut provider = GoogleCalendarProvider::new("calendar", &script, "Europe/Berlin").unwrap();
        let error = provider.delete_gym_event("owned", "\"v1\"", "s").unwrap_err();
        assert!(error.to_string().contains("ownership"), "{error}");
        assert_eq!(script.calls.borrow().len(), 1); // only the GET, no DELETE
    }

    #[test]
    fn consent_paste_stores_loadable_credentials() {
        let dir = std::env::temp_dir().join(format!("gymclaw-consent-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = crate::db::open(&dir.join("gymclaw.db")).unwrap();
        crate::db::migrate(&mut db).unwrap();
        let consent = Consent::new(br#"{"installed": {"client_id": "cid", "client_secret": "cs", "token_uri": "https://oauth2.googleapis.com/token"}}"#).unwrap();
        let url = consent.url();
        assert!(url.contains("code_challenge_method=S256") && url.contains("access_type=offline") && !url.contains(&consent.verifier));
        let now = Ts::now();
        let script = Script::new(vec![ok(200, json!({"access_token": "a1", "refresh_token": "r1", "expires_in": 3599, "scope": SCOPES[0]}))]);
        let wrong_state = format!("http://127.0.0.1:1/?state=other&code=c%2F1&scope={}", quote(SCOPES[0]));
        assert_eq!(consent.finish(&db, "cal@group", &wrong_state, &script, now).unwrap_err().code(), "CALENDAR_AUTH_FAILED");
        let pasted = format!("http://127.0.0.1:1/?state={}&code=4%2F0Ab&scope={}", consent.state, quote(SCOPES[0]));
        assert_eq!(consent.finish(&db, "cal@group", &pasted, &script, now).unwrap()["authorized"], true);
        let Body::Form(form) = &script.calls.borrow()[0].body else { panic!("form body") };
        assert!(form.contains(&("code".into(), "4/0Ab".into())) && form.contains(&("code_verifier".into(), consent.verifier.clone())));
        // Fresh token: loading needs no network.
        assert_eq!(load_credentials(&db, &Script::new(vec![])).unwrap().token(), "a1");
        std::fs::remove_dir_all(dir).ok();
    }
}
