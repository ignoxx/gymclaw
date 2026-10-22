//! Typed tools: one struct per tool (`#[derive(Deserialize, JsonSchema)]`), shared by the agent and the
//! CLI (`gymclaw call <name> '<json>'`). The model can only call what is registered here.
pub mod meta;
pub mod ops;
pub mod plan;
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

#[derive(Clone, Debug)]
pub enum Effect {
    Coach(CoachView),
}

pub trait Tool: DeserializeOwned + JsonSchema {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    fn run(self, cx: &mut Ctx) -> Result<Value>;
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON schema of the arguments (inlined, no `$ref`s).
    pub schema: fn() -> Value,
    pub call: fn(&mut Ctx, Value) -> Result<Value>,
}

pub fn def<T: Tool>() -> ToolDef {
    ToolDef {
        name: T::NAME,
        description: T::DESCRIPTION,
        schema: || {
            let settings = schemars::generate::SchemaSettings::draft07().with(|s| s.inline_subschemas = true);
            let mut schema = settings.into_generator().into_root_schema_for::<T>().to_value();
            if let Some(object) = schema.as_object_mut() {
                object.remove("$schema");
                object.remove("title");
            }
            schema
        },
        call: |cx, args| {
            let args = if args.is_null() { Value::Object(Default::default()) } else { args };
            let parsed: T = serde_json::from_value(args).map_err(|e| Error::domain("INVALID_ARGUMENTS", format!("{}: {e}", T::NAME)))?;
            parsed.run(cx)
        },
    }
}

pub fn all() -> Vec<ToolDef> {
    [plan::tools(), train::tools(), meta::tools(), ops::tools()].into_iter().flatten().collect()
}

pub fn call(cx: &mut Ctx, name: &str, args: Value) -> Result<Value> {
    let tools = all();
    let Some(tool) = tools.iter().find(|t| t.name == name) else {
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        crate::bail!("UNKNOWN_TOOL", "No tool {name}. Tools: {}", names.join(", "));
    };
    (tool.call)(cx, args).map_err(|e| match e.code() {
        // Wrong arguments: show how the tool is called, so the model fixes the call instead of repeating it.
        "INVALID_ARGUMENTS" | "ARGUMENT_REQUIRED" | "TEMPLATE_REQUIRED" | "INVALID_DATE" | "INVALID_TIME" | "INVALID_JSON" => {
            Error::domain(e.code().to_string(), format!("{e}\nUsage of {name}:\n{}", usage(&(tool.schema)(), 0)))
        }
        _ => e,
    })
}

/// The argument list of a tool schema, one line per field, nested objects indented:
/// `- start (string, required): Exact start…`.
fn usage(schema: &Value, depth: usize) -> String {
    let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let Some(properties) = schema["properties"].as_object() else { return format!("{}(no arguments)", "  ".repeat(depth)) };
    let mut lines = Vec::new();
    for (field, spec) in properties {
        let need = if required.contains(&field.as_str()) { "required" } else { "optional" };
        let description = spec["description"].as_str().map(|d| format!(": {}", d.replace('\n', " "))).unwrap_or_default();
        lines.push(format!("{}- {field} ({}, {need}){description}", "  ".repeat(depth), kind(spec)));
        if spec.get("properties").is_some() {
            lines.push(usage(spec, depth + 1));
        }
    }
    lines.join("\n")
}

/// Short type of one schema field: `string`, `integer`, or `one of "A"|"B"` for enums.
fn kind(spec: &Value) -> String {
    let variants: Vec<&Value> = [&spec["anyOf"], &spec["oneOf"]].into_iter().filter_map(Value::as_array).flatten().collect();
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
