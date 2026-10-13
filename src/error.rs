//! One error type. `Domain` errors carry a stable code the agent and CLI see; the rest are infrastructure.
use std::borrow::Cow;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{message}")]
    Domain { code: Cow<'static, str>, message: String },
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Time(#[from] jiff::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn domain(code: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Error {
        Error::Domain { code: code.into(), message: message.into() }
    }

    /// Stable code for JSON output and turn logs.
    pub fn code(&self) -> &str {
        match self {
            Error::Domain { code, .. } => code,
            Error::Db(_) => "DATABASE_ERROR",
            Error::Json(_) => "INVALID_JSON",
            Error::Time(_) => "INVALID_TIME",
            Error::Http(_) => "NETWORK_ERROR",
            Error::Io(_) => "IO_ERROR",
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"ok": false, "error": {"code": self.code(), "message": self.to_string()}})
    }
}

/// `bail!("CODE", "message {x}")` returns a domain error.
#[macro_export]
macro_rules! bail {
    ($code:expr, $($fmt:tt)+) => {
        return Err($crate::error::Error::domain($code, format!($($fmt)+)))
    };
}
