//! The one seam between providers and the network. Production uses `Net` (blocking reqwest, honors
//! HTTP(S)_PROXY); tests pass a closure `|req: &Request| -> Result<Response>`.
use std::time::Duration;

use serde_json::Value;

use crate::error::Result;

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

/// Real network access over blocking reqwest. Call from blocking code (`spawn_blocking`), never directly on the async runtime.
pub struct Net {
    follow: reqwest::blocking::Client,
    no_follow: reqwest::blocking::Client,
}

impl Net {
    pub fn new() -> Result<Net> {
        Ok(Net {
            follow: reqwest::blocking::Client::builder().build()?,
            no_follow: reqwest::blocking::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?,
        })
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
        let response = builder.send()?;
        let status = response.status().as_u16();
        Ok(Response {
            status,
            body: response.bytes()?.to_vec(),
        })
    }
}
