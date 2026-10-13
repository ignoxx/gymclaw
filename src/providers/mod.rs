//! External services: Google Calendar + OAuth, ICS feed, gym crowd sources, photos. Owner: WP-providers.
//!
//! Everything here is synchronous (blocking HTTP via `http::Net`); domain code calls providers inside
//! a DB transaction, and the async runtime reaches them through `spawn_blocking`.
pub mod calendar;
pub mod crowd;
pub mod fixture_calendar;
pub mod google;
pub mod http;
pub mod ics;
pub mod recurrence;

use serde_json::Value;

/// Python `json.dumps(value, sort_keys=True)`: `", "` / `": "` separators, non-ASCII escaped.
/// Used where Python hashed JSON into an identity that is stored in the DB.
pub(crate) fn python_json(value: &Value) -> String {
    let mut out = String::new();
    write_python_json(value, &mut out);
    out
}

fn write_python_json(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{08}' => out.push_str("\\b"),
                    '\u{0c}' => out.push_str("\\f"),
                    c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                        let mut units = [0u16; 2];
                        for unit in c.encode_utf16(&mut units) {
                            out.push_str(&format!("\\u{unit:04x}"));
                        }
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_python_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_python_json(&Value::String(key.clone()), out);
                out.push_str(": ");
                write_python_json(&map[key], out);
            }
            out.push('}');
        }
        Value::Null => out.push_str("null"),
        other => out.push_str(&other.to_string()),
    }
}
