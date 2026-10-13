//! Gym crowd sources: the gym's reported active check-in count (port of `crowd.py`, `mysports.py`,
//! `crowd_sources.py`).
//!
//! Config is private JSON at data/crowd-config.json (or GYMCLAW_CROWD_CONFIG), one of:
//!
//!   {"kind": "mysports", "studio_id": "123", "tenant": "my-gym"}
//!   {"kind": "http", "url": "https://…", "headers": {…}, "json_path": "data.0.count"}
//!   {"kind": "http", "url": "https://…", "regex": "(\\d+) people"}
//!   {"kind": "command", "argv": ["python3", "data/crowd-source.py"]}
//!
//! The file holds everything needed to read the count; nothing else configures the source.
//! A file without "kind" is the original MySports shape. See docs/crowd.md.
//! `regex` is Rust `regex` syntax (no lookaround/backreferences); anything else is the same as Python's `re`.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::http::{Method, Request, Transport};
use crate::bail;
use crate::error::{Error, Result};

/// `crowd_observation.source` / `crowd_source_state.source` of every live reading.
pub const SOURCE: &str = "GYM_API";
/// The only metric: the gym's own reported active check-ins. Never converted to a percentage.
pub const METRIC: &str = "reported_active_count";

/// One reading. The count is whole and nonnegative by type; there is no normalized value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrowdReading {
    pub raw_value: u64,
    pub freshness_seconds: Option<u64>,
}

impl CrowdReading {
    pub fn count(raw_value: u64) -> CrowdReading {
        CrowdReading {
            raw_value,
            freshness_seconds: None,
        }
    }
}

/// A source of crowd readings. `provider_id` binds `crowd_source_state`: a different id is a different gym.
pub trait CrowdProvider {
    fn source(&self) -> &'static str {
        SOURCE
    }
    fn provider_id(&self) -> &str;
    /// Synthetic/imported signal, never a live request.
    fn demo(&self) -> bool {
        false
    }
    fn get_reading(&self) -> Result<CrowdReading>;
}

/// Explicit synthetic/imported signal; never labeled a live request.
pub struct FixtureCrowdProvider {
    pub reading: CrowdReading,
}

impl CrowdProvider for FixtureCrowdProvider {
    fn provider_id(&self) -> &str {
        "fixture:GYM_API"
    }
    fn demo(&self) -> bool {
        true
    }
    fn get_reading(&self) -> Result<CrowdReading> {
        Ok(self.reading)
    }
}

fn invalid_response(message: &str) -> Error {
    Error::domain("GYM_API_INVALID_RESPONSE", message.to_string())
}

/// Observed public MySports route, not a documented freshness/occupancy guarantee. Only
/// active-checkin is collected; dated /today percentages and weekly profiles are not headcounts.
pub struct MySportsProvider<'a> {
    provider_id: String,
    url: String,
    tenant: String,
    transport: &'a dyn Transport,
}

impl<'a> MySportsProvider<'a> {
    pub fn new(studio_id: &str, tenant: &str, transport: &'a dyn Transport) -> Result<MySportsProvider<'a>> {
        if !valid_studio(studio_id) || !valid_tenant(tenant) {
            bail!("GYM_API_NOT_CONFIGURED", "Invalid MySports studio ID or tenant");
        }
        Ok(MySportsProvider {
            provider_id: format!("mysports:{tenant}:{studio_id}"),
            url: format!("https://www.mysports.com/nox/public/v1/studios/{studio_id}/utilization/v2/active-checkin"),
            tenant: tenant.into(),
            transport,
        })
    }
}

fn valid_studio(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

fn valid_tenant(tenant: &str) -> bool {
    (1..=100).contains(&tenant.len()) && tenant.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl CrowdProvider for MySportsProvider<'_> {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn get_reading(&self) -> Result<CrowdReading> {
        let invalid = || invalid_response("MySports count unavailable or invalid; response/credentials withheld");
        let mut request = Request::new(Method::Get, &self.url, 10)
            .header("x-tenant", &self.tenant)
            .header("Accept", "application/json");
        request.follow_redirects = false;
        let response = self.transport.send(&request).map_err(|_| invalid())?;
        if response.status != 200 {
            bail!(
                "GYM_API_UNAVAILABLE",
                "MySports public request unavailable (HTTP {}); no new count stored",
                response.status
            );
        }
        // Strictly a nonnegative JSON integer: no strings, floats or booleans.
        let value = response
            .json()
            .map_err(|_| invalid())?
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(invalid)?;
        Ok(CrowdReading::count(value))
    }
}

/// `{"kind": "http", …}`: a JSON or text endpoint.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSource {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Exactly one: dot path into the JSON body ("data.0.count") or a regex whose first group is the count.
    pub json_path: Option<String>,
    pub regex: Option<String>,
}

/// `{"kind": "command", …}`: run from the working directory without a shell. Stdout must be the
/// count, alone or as `{"value": n}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSource {
    pub argv: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MySportsSource {
    pub studio_id: String,
    pub tenant: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceConfig {
    Http(HttpSource),
    Command(CommandSource),
    Mysports(MySportsSource),
}

/// Whole, nonnegative count from a JSON value or text; anything else is invalid.
pub fn count(value: &Value) -> Option<u64> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
        }
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Follow a dot path: numeric keys index lists (negative from the end), other keys index objects.
pub fn dig<'v>(mut body: &'v Value, path: &str) -> Option<&'v Value> {
    for key in path.split('.') {
        body = match body {
            Value::Array(items) => {
                let i: i64 = key.trim().parse().ok()?;
                let i = if i < 0 { items.len() as i64 + i } else { i };
                items.get(usize::try_from(i).ok()?)?
            }
            Value::Object(map) => map.get(key)?,
            _ => return None,
        };
    }
    Some(body)
}

enum Extract {
    JsonPath(String),
    Regex(Regex),
}

/// Reads a count from an HTTP endpoint.
pub struct HttpCountProvider<'a> {
    provider_id: String,
    config: HttpSource,
    extract: Extract,
    transport: &'a dyn Transport,
}

impl<'a> HttpCountProvider<'a> {
    pub fn new(config: HttpSource, transport: &'a dyn Transport) -> Result<HttpCountProvider<'a>> {
        let not_configured = || Error::domain("GYM_API_NOT_CONFIGURED", "No valid crowd source configured; see docs/crowd.md");
        if !config.url.starts_with("https://") {
            return Err(not_configured());
        }
        let extract = match (&config.json_path, &config.regex) {
            (Some(path), None) => Extract::JsonPath(path.clone()),
            (None, Some(pattern)) => {
                let regex = Regex::new(pattern).map_err(|_| not_configured())?;
                if regex.captures_len() < 2 {
                    return Err(not_configured());
                }
                Extract::Regex(regex)
            }
            _ => return Err(not_configured()),
        };
        // Headers and the extraction rule can select another gym at the same URL, so they're part of the identity.
        let dump = json!({"kind": "http", "url": config.url, "headers": config.headers, "json_path": config.json_path, "regex": config.regex});
        let digest = hex::encode(Sha256::digest(super::python_json(&dump).as_bytes()));
        Ok(HttpCountProvider {
            provider_id: format!("http:{}#{}", config.url, &digest[..12]),
            config,
            extract,
            transport,
        })
    }
}

impl CrowdProvider for HttpCountProvider<'_> {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn get_reading(&self) -> Result<CrowdReading> {
        let invalid = || invalid_response("Crowd count unavailable or invalid; response withheld");
        let mut request = Request::new(Method::Get, &self.config.url, 10);
        request.headers = self.config.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        request.follow_redirects = false;
        let response = self.transport.send(&request).map_err(|_| invalid())?;
        if response.status != 200 {
            bail!(
                "GYM_API_UNAVAILABLE",
                "Crowd source unavailable (HTTP {}); no new count stored",
                response.status
            );
        }
        let value = match &self.extract {
            Extract::JsonPath(path) => count(dig(&response.json().map_err(|_| invalid())?, path).ok_or_else(invalid)?),
            Extract::Regex(regex) => regex.captures(&response.text()).and_then(|c| c.get(1)).and_then(|m| count(&json!(m.as_str()))),
        };
        Ok(CrowdReading::count(value.ok_or_else(invalid)?))
    }
}

/// What a finished command printed.
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
}

/// Runs `argv` with a timeout. Injected so tests don't spawn processes.
pub type Runner = dyn Fn(&[String], Duration) -> Result<CommandOutput>;

/// The real runner: no shell, stdout/stderr captured, killed after `timeout`.
pub fn run_command(argv: &[String], timeout: Duration) -> Result<CommandOutput> {
    let (program, args) = argv.split_first().ok_or_else(|| Error::domain("COMMAND_EMPTY", "Empty command"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        text
    });
    let err = std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("COMMAND_TIMEOUT", "Command timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.join().unwrap_or_default();
    let _ = err.join();
    Ok(CommandOutput {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
    })
}

/// Reads a count from a local command.
pub struct CommandCountProvider<'a> {
    provider_id: String,
    config: CommandSource,
    run: &'a Runner,
}

impl<'a> CommandCountProvider<'a> {
    pub fn new(config: CommandSource, run: &'a Runner) -> Result<CommandCountProvider<'a>> {
        if config.argv.is_empty() || !(1..=120).contains(&config.timeout_seconds) {
            bail!("GYM_API_NOT_CONFIGURED", "No valid crowd source configured; see docs/crowd.md");
        }
        Ok(CommandCountProvider {
            provider_id: format!("command:{}", config.argv.join(" ")),
            config,
            run,
        })
    }
}

impl CrowdProvider for CommandCountProvider<'_> {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn get_reading(&self) -> Result<CrowdReading> {
        let done = (self.run)(&self.config.argv, Duration::from_secs(self.config.timeout_seconds))
            .map_err(|_| Error::domain("GYM_API_UNAVAILABLE", "Crowd source command failed to run or timed out"))?;
        if done.status != 0 {
            bail!("GYM_API_UNAVAILABLE", "Crowd source command exited {}; output withheld", done.status);
        }
        let out = done.stdout.trim();
        let value = if out.starts_with('{') {
            serde_json::from_str::<Value>(out).ok().and_then(|v| v.get("value").and_then(count))
        } else {
            count(&json!(out))
        };
        Ok(CrowdReading::count(
            value.ok_or_else(|| invalid_response("Crowd source command printed no valid count"))?,
        ))
    }
}

/// The configured crowd provider, or GYM_API_NOT_CONFIGURED. Config values never appear in errors.
/// Production: `from_environment(&Net::new()?, &run_command)`.
pub fn from_environment<'a>(transport: &'a dyn Transport, run: &'a Runner) -> Result<Box<dyn CrowdProvider + 'a>> {
    let path = std::env::var_os("GYMCLAW_CROWD_CONFIG").map_or_else(|| PathBuf::from("data/crowd-config.json"), PathBuf::from);
    let not_configured = || Error::domain("GYM_API_NOT_CONFIGURED", "No valid crowd source configured; see docs/crowd.md");
    let values: Value = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(not_configured)?;
    from_config(values, transport, run)
}

/// The provider a crowd-config JSON value describes (see the module docs).
pub fn from_config<'a>(values: Value, transport: &'a dyn Transport, run: &'a Runner) -> Result<Box<dyn CrowdProvider + 'a>> {
    let not_configured = || Error::domain("GYM_API_NOT_CONFIGURED", "No valid crowd source configured; see docs/crowd.md");
    let Value::Object(mut values) = values else { return Err(not_configured()) };
    if !values.contains_key("kind") {
        // Original MySports file: only the two identifiers count.
        values = [
            ("kind", json!("mysports")),
            ("studio_id", values.get("studio_id").cloned().unwrap_or(Value::Null)),
            ("tenant", values.get("tenant").cloned().unwrap_or(Value::Null)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    }
    let config: SourceConfig = serde_json::from_value(Value::Object(values)).map_err(|_| not_configured())?;
    let provider: Box<dyn CrowdProvider + 'a> = match config {
        SourceConfig::Mysports(c) => Box::new(MySportsProvider::new(&c.studio_id, &c.tenant, transport).map_err(|_| not_configured())?),
        SourceConfig::Http(c) => Box::new(HttpCountProvider::new(c, transport).map_err(|_| not_configured())?),
        SourceConfig::Command(c) => Box::new(CommandCountProvider::new(c, run).map_err(|_| not_configured())?),
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::providers::http::Response;

    fn body(value: Value) -> impl Fn(&Request) -> Result<Response> {
        move |_: &Request| {
            Ok(Response {
                status: 200,
                body: value.to_string().into_bytes(),
            })
        }
    }

    #[test]
    fn mysports_accepts_only_whole_nonnegative_counts() {
        let seen = RefCell::new(None);
        let transport = |r: &Request| {
            *seen.borrow_mut() = Some(r.clone());
            Ok(Response {
                status: 200,
                body: br#"{"value": 7}"#.to_vec(),
            })
        };
        let reading = MySportsProvider::new("1234567890", "fixture-tenant", &transport)
            .unwrap()
            .get_reading()
            .unwrap();
        assert_eq!(reading, CrowdReading::count(7));
        let request = seen.borrow().clone().unwrap();
        assert!(request.url.ends_with("/1234567890/utilization/v2/active-checkin") && !request.follow_redirects);
        assert_eq!(request.header_value("x-tenant"), Some("fixture-tenant"));
        for bad in [
            json!({}),
            json!({"value": -1}),
            json!({"value": true}),
            json!({"value": "7"}),
            json!({"value": 7.5}),
            json!({"value": null}),
        ] {
            let transport = body(bad);
            let error = MySportsProvider::new("1234567890", "fixture-tenant", &transport)
                .unwrap()
                .get_reading()
                .unwrap_err();
            assert_eq!(error.code(), "GYM_API_INVALID_RESPONSE");
        }
    }

    #[test]
    fn http_source_extracts_and_identity_matches_python() {
        let config = |extra: Value| -> HttpSource {
            let mut base = json!({"url": "https://gym.example/live", "headers": {"x-tenant": "gym-b", "b": "2 ü"}});
            for (k, v) in extra.as_object().unwrap() {
                base[k] = v.clone();
            }
            serde_json::from_value(base).unwrap()
        };
        let transport = body(json!({"data": [{"count": 12}]}));
        let source = HttpCountProvider::new(config(json!({"json_path": "data.0.count"})), &transport).unwrap();
        assert_eq!(source.get_reading().unwrap().raw_value, 12);
        // Python: HttpCountProvider(HttpSource(...)).provider_id for the same config.
        assert_eq!(source.provider_id(), "http:https://gym.example/live#dcdf56b39c45");

        let transport = |_: &Request| {
            Ok(Response {
                status: 200,
                body: b"<b>12 people</b> training now".to_vec(),
            })
        };
        assert_eq!(
            HttpCountProvider::new(config(json!({"regex": r"(\d+) people"})), &transport)
                .unwrap()
                .get_reading()
                .unwrap()
                .raw_value,
            12
        );
        assert!(HttpCountProvider::new(config(json!({"regex": r"\d+"})), &transport).is_err());
        assert!(HttpCountProvider::new(config(json!({"regex": r"(\d+)", "json_path": "n"})), &transport).is_err());
    }

    #[test]
    fn config_shapes() {
        let transport = body(json!({"value": 9}));
        let run = |_: &[String], _: Duration| Ok(CommandOutput { status: 0, stdout: "1".into() });
        let id = |config: Value| {
            from_config(config, &transport, &run)
                .map(|p| p.provider_id().to_string())
                .map_err(|e| e.code().to_string())
        };
        // Legacy file (no kind, extra field) keeps the identity the live DB is bound to.
        assert_eq!(
            id(json!({"studio_id": "1234567890", "tenant": "fixture-tenant", "note": "my gym"})).unwrap(),
            "mysports:fixture-tenant:1234567890"
        );
        assert_eq!(
            id(json!({"kind": "mysports", "studio_id": "1234567890", "tenant": "fixture-tenant"})).unwrap(),
            "mysports:fixture-tenant:1234567890"
        );
        assert_eq!(
            id(json!({"kind": "command", "argv": ["python3", "data/crowd-source.py"]})).unwrap(),
            "command:python3 data/crowd-source.py"
        );
        for bad in [
            json!({"studio_id": "private-invalid-value", "tenant": "fixture-tenant"}),
            json!({"kind": "mysports", "studio_id": "1", "tenant": "t", "note": "x"}),
            json!({"kind": "http", "url": "http://plain.example", "json_path": "n"}),
            json!({"kind": "http", "url": "https://gym.example", "json_path": "n", "regex": "(\\d+)"}),
            json!({"kind": "http", "url": "https://gym.example", "regex": "\\d+"}),
            json!({"kind": "command", "argv": []}),
            json!({"kind": "scrape-it-somehow"}),
            json!([1]),
        ] {
            assert_eq!(id(bad).unwrap_err(), "GYM_API_NOT_CONFIGURED");
        }
    }

    #[test]
    fn command_source_reads_count() {
        for (stdout, expected) in [("12\n", Some(12)), (r#"{"value": 12}"#, Some(12)), ("busy", None), ("-3", None)] {
            let run = move |_: &[String], _: Duration| {
                Ok(CommandOutput {
                    status: 0,
                    stdout: stdout.into(),
                })
            };
            let source = CommandCountProvider::new(
                CommandSource {
                    argv: vec!["python3".into()],
                    timeout_seconds: 30,
                },
                &run,
            )
            .unwrap();
            match expected {
                Some(n) => assert_eq!(source.get_reading().unwrap().raw_value, n),
                None => assert_eq!(source.get_reading().unwrap_err().code(), "GYM_API_INVALID_RESPONSE"),
            }
        }
    }

    #[test]
    fn http_identity_covers_headers_not_key_order() {
        let transport = body(json!({"n": 1}));
        let id = |config: Value| HttpCountProvider::new(serde_json::from_value(config).unwrap(), &transport).unwrap().provider_id().to_string();
        let a = id(json!({"url": "https://gym.example/live", "headers": {"x-tenant": "gym-a"}, "json_path": "n"}));
        let b = id(json!({"url": "https://gym.example/live", "headers": {"x-tenant": "gym-b"}, "json_path": "n"}));
        let first = id(json!({"url": "https://gym.example/live", "headers": {"b": "2", "x-tenant": "gym-b"}, "json_path": "n"}));
        let reordered = id(json!({"json_path": "n", "headers": {"x-tenant": "gym-b", "b": "2"}, "url": "https://gym.example/live"}));
        assert!(a != b && first == reordered);
    }
}
