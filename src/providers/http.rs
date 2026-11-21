//! The one seam between providers and the network. Production uses `Net` (blocking reqwest, honors
//! HTTP(S)_PROXY); tests pass a closure `|req: &Request| -> Result<Response>`.
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    None,
    Json(Value),
    Form(Vec<(String, String)>),
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    pub timeout: Duration,
    pub follow_redirects: bool,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>, timeout_secs: u64) -> Request {
        Request {
            method,
            url: url.into(),
            query: vec![],
            headers: vec![],
            body: Body::None,
            timeout: Duration::from_secs(timeout_secs),
            follow_redirects: true,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(&self) -> serde_json::Result<Value> {
        serde_json::from_slice(&self.body)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Sends one request. Errors are transport failures; HTTP error statuses are normal responses.
pub trait Transport {
    fn send(&self, request: &Request) -> Result<Response>;
}

impl<F: Fn(&Request) -> Result<Response>> Transport for F {
    fn send(&self, request: &Request) -> Result<Response> {
        self(request)
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Real network access over blocking reqwest. Call from blocking code (`spawn_blocking`), never directly on the async runtime.
/// Clients are shared process-wide so ticks reuse connection pools and TLS state instead of rebuilding
/// two blocking runtimes and root cert stores on every call.
#[derive(Clone)]
pub struct Net {
    follow: reqwest::blocking::Client,
    no_follow: reqwest::blocking::Client,
}

impl Net {
    pub fn new() -> Result<Net> {
        static SHARED: OnceLock<Net> = OnceLock::new();
        if let Some(net) = SHARED.get() {
            return Ok(net.clone());
        }
        let net = Net {
            follow: reqwest::blocking::Client::builder().connect_timeout(CONNECT_TIMEOUT).build()?,
            no_follow: reqwest::blocking::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        };
        Ok(SHARED.get_or_init(|| net).clone())
    }
}

impl Transport for Net {
    fn send(&self, request: &Request) -> Result<Response> {
        let client = if request.follow_redirects { &self.follow } else { &self.no_follow };
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut builder = client.request(method, &request.url).timeout(request.timeout);
        if !request.query.is_empty() {
            builder = builder.query(&request.query);
        }
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        builder = match &request.body {
            Body::None => builder,
            Body::Json(value) => builder.json(value),
            Body::Form(fields) => builder.form(fields),
        };
        // Strip URLs from transport errors so secret ICS feed tokens or OAuth params never leak to logs.
        let response = builder.send().map_err(|e| Error::Http(e.without_url()))?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(|e| Error::Http(e.without_url()))?.to_vec();
        Ok(Response { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_errors_strip_secret_urls() {
        let net = Net::new().unwrap();
        let req = Request::new(Method::Get, "http://127.0.0.1:1/calendar?token=super-secret-value", 1);
        let err = net.send(&req).unwrap_err();
        assert_eq!(err.code(), "NETWORK_ERROR");
        assert!(!err.to_string().contains("super-secret-value"), "URL leaked in error: {err}");
    }
}
