//! Typed tools: one struct per tool (`#[derive(Deserialize, JsonSchema)]`), shared by the agent and the
//! CLI (`gymclaw call <name> '<json>'`). The model can only call what is registered here.
pub mod exercises;
pub mod extensions;
pub mod meta;
pub mod ops;
pub mod plan;
pub mod progress;
pub mod train;

use std::path::Path;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::db::Connection;
use crate::domain::view::CoachView;
use crate::error::{Error, Result};
use crate::time::Ts;

/// What a tool call runs with. Runs inside one transaction: an error rolls everything back.
pub struct Ctx<'a> {
    pub db: &'a Connection,
    pub now: Ts,
    /// Idempotency key for mutations, derived by the caller (Telegram message id + call index, or CLI).
    pub request_id: String,
    pub data_dir: &'a Path,
    /// Messages the tool wants shown (coach cards); rendered after the transaction commits.
    pub effects: Vec<Effect>,
}

impl Ctx<'_> {
    /// A call from a background turn (an event or trigger wake, request ids `event-…`, see
    /// `runtime::bot`). Background events are observations: no owner permission comes from them.
    pub fn background(&self) -> bool {
        self.request_id.starts_with("event-")
    }

    /// A time argument: owner-local unless the text has an offset.
    pub fn local(&self, text: &str) -> Result<Ts> {
        Ts::parse(text, Some(&crate::domain::profile::get_profile(self.db)?.tz()))
    }
}

/// A `YYYY-MM-DD` date argument.
pub fn date(text: &str) -> Result<jiff::civil::Date> {
    text.trim().parse().map_err(|_| Error::domain("INVALID_DATE", format!("Not a date (YYYY-MM-DD): {text}")))
}

#[derive(Clone, Debug)]
pub enum Effect {
    Coach(CoachView),
}

/// Whether the model is offered a tool. A hidden tool still runs when called by name (from `gymclaw call`,
/// the dashboard, or a model that remembers it); it just stops costing context on every call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listed {
    Always,
    /// A read the state snapshot or `sql_query` already covers.
    Never,
    /// Only until setup is finished; setup never restarts.
    InSetup,
}

pub trait Tool: DeserializeOwned + JsonSchema {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const LISTED: Listed = Listed::Always;
    fn run(self, cx: &mut Ctx) -> Result<Value>;
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub listed: Listed,
    /// JSON schema of the arguments (inlined, no `$ref`s).
    pub schema: fn() -> Value,
    pub call: fn(&mut Ctx, Value) -> Result<Value>,
}

/// Inlined JSON schema of a tool's arguments. The struct's own doc comment is dropped: it is written
/// for developers, and the tool description already tells the model what the call is for.
/// Number formats (`int64`, `double`) and `default: null` are dropped too: serde checks those, and they
/// cost context on every call.
pub fn schema<T: JsonSchema>() -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("format");
                if object.get("default").is_some_and(Value::is_null) {
                    object.remove("default");
                }
                for (key, child) in object.iter_mut() {
                    if key == "properties" {
                        // Field names, not keywords: a field called `format` stays.
                        child.as_object_mut().into_iter().flat_map(|p| p.values_mut()).for_each(strip);
                    } else {
                        strip(child);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let settings = schemars::generate::SchemaSettings::draft07().with(|s| s.inline_subschemas = true);
    let mut schema = settings.into_generator().into_root_schema_for::<T>().to_value();
    if let Some(object) = schema.as_object_mut() {
        for key in ["$schema", "title", "description"] {
            object.remove(key);
        }
    }
    strip(&mut schema);
    // Flatten root tagged-enum `oneOf` into a standard `type: "object"` schema so Anthropic/OpenRouter
    // accept it and `guide()` prints its fields.
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array).cloned() {
        let mut actions: Vec<Value> = Vec::new();
        let mut props = serde_json::Map::new();
        for variant in variants {
            if let Some(vp) = variant.get("properties").and_then(Value::as_object) {
                for (k, v) in vp {
                    if k == "action" {
                        if let Some(c) = v.get("const") {
                            actions.push(c.clone());
                        } else if let Some(es) = v.get("enum").and_then(Value::as_array) {
                            actions.extend(es.iter().cloned());
                        }
                    } else {
                        props.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }
        if !actions.is_empty() {
            props.insert("action".into(), serde_json::json!({"type": "string", "enum": actions}));
            schema = serde_json::json!({
                "type": "object",
                "required": ["action"],
                "properties": props,
            });
        }
    }
    schema
}

pub fn def<T: Tool>() -> ToolDef {
    ToolDef {
        name: T::NAME,
        description: T::DESCRIPTION,
        listed: T::LISTED,
        schema: schema::<T>,
        call: |cx, args| {
            let args = if args.is_null() { Value::Object(Default::default()) } else { args };
            let parsed: T = serde_json::from_value(args)
                .map_err(|e| Error::domain("INVALID_ARGUMENTS", format!("{}: {e}", T::NAME)))?;
            parsed.run(cx)
        },
    }
}

pub fn all() -> Vec<ToolDef> {
    [
        plan::tools(),
        train::tools(),
        progress::tools(),
        exercises::tools(),
        meta::tools(),
        ops::tools(),
        extensions::tools(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn call(cx: &mut Ctx, name: &str, args: Value) -> Result<Value> {
    let tools = all();
    let Some(tool) = tools.iter().find(|t| t.name == name) else {
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        crate::bail!("UNKNOWN_TOOL", "No tool {name}. Tools: {}", names.join(", "));
    };
    (tool.call)(cx, args).map_err(|e| guide(name, e))
}

/// Turns a failed call's error into one the model can act on: what to do next, plus the tool's
/// usage so it can fix the call or pick another option instead of repeating it. Every tool error,
/// from argument parsing to domain rules, passes through here.
pub fn guide(name: &str, e: Error) -> Error {
    let code = e.code().to_string();
    // `*_UNAVAILABLE` is reserved for outside services that are down.
    let system =
        matches!(code.as_str(), "DATABASE_ERROR" | "NETWORK_ERROR" | "IO_ERROR" | "INTERNAL" | "TELEGRAM_ERROR")
            || code.ends_with("_UNAVAILABLE")
            || code.starts_with("CALENDAR_AUTH");
    let next = if system {
        "A system problem, not a wrong call: don't retry it this turn. Tell the owner what didn't happen.".to_string()
    } else {
        // The exercise error already lists the names to pass; an id lookup would lead back to the same key.
        let lookup = if code.ends_with("_NOT_FOUND") || code == "SESSION_AMBIGUOUS" {
            "Look the id up (sql_query, get_week) instead of guessing it. "
        } else {
            ""
        };
        let usage = all()
            .iter()
            .find(|t| t.name == name)
            .map(|t| format!("\nUsage of {name}:\n{}", usage(&(t.schema)(), 0)))
            .unwrap_or_default();
        format!("{lookup}Fix the call, use another tool, or ask the owner; don't repeat it unchanged.{usage}")
    };
    Error::domain(code, format!("{e}\n{next}"))
}

/// The argument list of a tool schema, one line per field, nested objects indented:
/// `- start (string, required): Exact start…`.
fn usage(schema: &Value, depth: usize) -> String {
    let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let Some(properties) = schema["properties"].as_object() else {
        return format!("{}(no arguments)", "  ".repeat(depth));
    };
    let mut lines = Vec::new();
    for (field, spec) in properties {
        let need = if required.contains(&field.as_str()) { "required" } else { "optional" };
        let description =
            spec["description"].as_str().map(|d| format!(": {}", d.replace('\n', " "))).unwrap_or_default();
        lines.push(format!("{}- {field} ({}, {need}){description}", "  ".repeat(depth), kind(spec)));
        if spec.get("properties").is_some() {
            lines.push(usage(spec, depth + 1));
        }
    }
    lines.join("\n")
}

/// Short type of one schema field: `string`, `integer`, or `one of "A"|"B"` for enums.
fn kind(spec: &Value) -> String {
    let variants: Vec<&Value> =
        [&spec["anyOf"], &spec["oneOf"]].into_iter().filter_map(Value::as_array).flatten().collect();
    let values: Vec<String> = std::iter::once(spec)
        .chain(variants.iter().copied())
        .flat_map(|v| v["enum"].as_array().cloned().unwrap_or_default().into_iter().chain(v.get("const").cloned()))
        .filter(|v| !v.is_null())
        .map(|v| v.to_string())
        .collect();
    if !values.is_empty() {
        return format!("one of {}", values.join("|"));
    }
    let types: Vec<&str> = std::iter::once(spec)
        .chain(variants)
        .flat_map(|v| match &v["type"] {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        })
        .filter(|t| *t != "null")
        .collect();
    if types.is_empty() { "any".into() } else { types.join("|") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_what_to_do_next() {
        let wrong = guide("session_update", Error::domain("SESSION_NOT_FOUND", "Unknown session")).to_string();
        assert!(wrong.contains("Look the id up") && wrong.contains("- session_id (string, required)"), "{wrong}");
        // Retrying or rewriting the call can't fix a system problem, so no usage there.
        let down = guide("crowd_forecast", Error::domain("GYM_API_UNAVAILABLE", "No source")).to_string();
        assert!(down.contains("don't retry") && !down.contains("Usage"), "{down}");
        // An owner's availability block is theirs to lift, not an outage.
        let blocked = guide("workout", Error::domain("AVAILABILITY_BLOCKED", "Block")).to_string();
        assert!(!blocked.contains("don't retry"), "{blocked}");
    }

    /// Docs drift: every backticked `src/…`, `docs/…`, `scripts/…` or `deploy/…` path and every relative
    /// Markdown link in the docs exists, and every `gymclaw call <tool>` names a registered tool.
    #[test]
    fn docs_name_real_paths_and_tools() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let docs = ["README.md", "AGENTS.md", "CODING_STANDARDS.md", "SECURITY.md", "deploy/README.md"]
            .map(|f| root.join(f))
            .into_iter()
            .chain(std::fs::read_dir(root.join("docs")).unwrap().map(|e| e.unwrap().path()));
        let path = regex::Regex::new(r"`((?:src|docs|scripts|deploy)/[^`\s*<…]*)").unwrap();
        let link = regex::Regex::new(r"\]\(([^)#\s]+)(?:#[^)]*)?\)").unwrap();
        let call = regex::Regex::new(r"gymclaw call ([a-z_]+)").unwrap();
        let tools: Vec<&str> = all().iter().map(|t| t.name).collect();
        let mut stale = vec![];
        for doc in docs {
            let text = std::fs::read_to_string(&doc).unwrap();
            let name = doc.strip_prefix(root).unwrap().display().to_string();
            for found in path.captures_iter(&text) {
                let target = found[1].trim_end_matches([':', '.', ',', ')']);
                if !root.join(target).exists() {
                    stale.push(format!("{name}: `{target}` doesn't exist"));
                }
            }
            for found in link.captures_iter(&text).filter(|l| !l[1].contains(':')) {
                if !doc.parent().unwrap().join(&found[1]).exists() {
                    stale.push(format!("{name}: link to {} is broken", &found[1]));
                }
            }
            for found in call.captures_iter(&text) {
                if !tools.contains(&&found[1]) {
                    stale.push(format!("{name}: `gymclaw call {}` isn't a tool", &found[1]));
                }
            }
        }
        assert!(stale.is_empty(), "fix these docs:\n{}", stale.join("\n"));
    }

    /// Every offered tool must have a root `type: "object"` schema (no top-level `oneOf` that
    /// Anthropic rejects), and every consolidated action tool must deserialize and dispatch cleanly.
    #[test]
    fn offered_tools_have_object_schemas_and_consolidated_dispatch_works() {
        for setup_done in [false, true] {
            for spec in crate::agent::tool_specs(setup_done) {
                let name = spec["function"]["name"].as_str().unwrap();
                let params = &spec["function"]["parameters"];
                assert_eq!(params["type"], "object", "{name} schema must be type: object, got {params}");
                assert!(params.get("oneOf").is_none(), "{name} has top-level oneOf: {params}");
            }
        }
        let offered: Vec<&str> = crate::agent::tool_specs(true)
            .into_iter()
            .filter_map(|s| s["function"]["name"].as_str().map(String::from))
            .collect::<Vec<_>>()
            .leak()
            .iter()
            .map(String::as_str)
            .collect();
        for required in [
            "note",
            "delivery",
            "runtime",
            "extension",
            "data",
            "availability",
            "crowd",
            "personal_calendar",
            "template",
            "body",
        ] {
            assert!(offered.contains(&required), "missing consolidated tool {required}");
        }
        // Smoke-test serde tag dispatch on structs that live behind consolidated enums.
        let db = crate::db::test::with_short_template();
        let dir = std::env::temp_dir();
        let mut cx = Ctx { db: &db, now: Ts::now(), request_id: "t1".into(), data_dir: &dir, effects: vec![] };
        assert!(call(&mut cx, "template", serde_json::json!({"action": "get", "template_id": "short"})).is_ok());
        assert!(call(&mut cx, "body", serde_json::json!({"action": "log", "entries": [{"kg": 80.0}]})).is_ok());
        assert!(call(&mut cx, "note", serde_json::json!({"action": "add", "kind": "owner", "text": "test"})).is_ok());
        assert!(call(&mut cx, "crowd", serde_json::json!({"action": "status"})).is_ok());
        assert!(call(&mut cx, "personal_calendar", serde_json::json!({"action": "status"})).is_ok());
    }
}
