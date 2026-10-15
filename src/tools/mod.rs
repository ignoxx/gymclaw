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
    (tool.call)(cx, args)
}
