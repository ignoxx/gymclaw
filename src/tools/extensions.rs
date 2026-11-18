//! The agent's own layer (`domain::extensions`): playbooks, collections, triggers and the gap log.
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolDef, def};
use crate::domain::extensions::{self as ext, Author, GapSource, Spec};
use crate::error::Result;

pub fn tools() -> Vec<ToolDef> {
    vec![
        def::<ExtensionSave>(),
        def::<ExtensionEdit>(),
        def::<DataAdd>(),
        def::<DataDelete>(),
        def::<CapabilityGapLog>(),
    ]
}

/// Changes from the dashboard and the CLI are the owner's; everything else is the agent's.
fn author(cx: &Ctx) -> Author {
    if cx.request_id.starts_with("dashboard:") || cx.request_id.starts_with("cli:") {
        Author::Owner
    } else {
        Author::Agent
    }
}

fn shown(e: &ext::Extension) -> Value {
    json!({"id": e.id, "kind": e.kind, "name": e.name, "enabled": e.enabled, "version": e.version})
}

#[derive(Deserialize, JsonSchema)]
pub struct ExtensionSave {
    /// Short and stable, e.g. "creatine". Saving the same kind and name again saves a new version.
    pub name: String,
    pub spec: Spec,
}

impl Tool for ExtensionSave {
    const NAME: &'static str = "extension_save";
    const DESCRIPTION: &'static str = "Extend yourself when the owner wants something recurring your tools don't do on their own: \
a playbook (steps for a request that keeps coming back), a collection (your own log) or a trigger (wakes you with an instruction daily/weekly \
at a local time, once at a local date and time, some minutes before each planned session, or after each workout; you decide then, \
and stay silent when nothing needs saying). \
\"Track my creatine and nag me if I forget\" = a creatine collection + a daily 09:00 trigger that asks only if nothing was logged today. \
\"Remind me tomorrow at 9 to buy chalk\" = a once trigger. \"Before leg day, remind me to bring the belt\" = a before_session trigger for that workout. \
Plain preferences stay notes. Every change is a version the owner can roll back.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let saved = ext::save(cx.db, &self.name, self.spec, author(cx), cx.now)?;
        Ok(shown(&saved))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ExtensionEdit {
    pub id: i64,
    /// Turn it off, or back on (only when the owner asks).
    pub enabled: Option<bool>,
    /// Roll back to this earlier version (history: sql_query extension_version).
    pub restore_version: Option<i64>,
}

impl Tool for ExtensionEdit {
    const NAME: &'static str = "extension_edit";
    const DESCRIPTION: &'static str = "Turn one of your extensions off or on, or roll it back to an earlier version.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let author = author(cx);
        let mut current = None;
        if let Some(version) = self.restore_version {
            current = Some(ext::restore(cx.db, self.id, version, author, cx.now)?);
        }
        if let Some(enabled) = self.enabled {
            // The owner may have turned it off; only the owner's own turn can turn it back on.
            if enabled && cx.background() {
                crate::bail!("OWNER_REQUIRED", "Turning an extension back on needs the owner's request");
            }
            current = Some(ext::set_enabled(cx.db, self.id, enabled, author, cx.now)?);
        }
        match current {
            Some(e) => Ok(shown(&e)),
            None => crate::bail!("NOTHING_TO_CHANGE", "Pass enabled or restore_version"),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct DataAdd {
    /// The collection's name.
    pub collection: String,
    /// The row's fields, as the collection's summary describes them; {} when the row itself is the fact.
    #[serde(default)]
    pub data: serde_json::Map<String, Value>,
    /// When it happened, local time; default now.
    pub at: Option<String>,
}

impl Tool for DataAdd {
    const NAME: &'static str = "data_add";
    const DESCRIPTION: &'static str = "Log a row into one of your collections.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let at = match &self.at {
            Some(at) => cx.local(at)?,
            None => cx.now,
        };
        let id = ext::add_data(cx.db, self.collection.trim(), at, Value::Object(self.data), cx.now)?;
        Ok(json!({"id": id}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct DataDelete {
    /// agent_data id.
    pub id: i64,
}

impl Tool for DataDelete {
    const NAME: &'static str = "data_delete";
    const DESCRIPTION: &'static str = "Delete a collection row logged by mistake.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let deleted = cx.db.execute("DELETE FROM agent_data WHERE id = ?1", [self.id])?;
        Ok(json!({"deleted": deleted == 1}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CapabilityGapLog {
    /// What the owner asked for, in their words.
    pub asked: String,
    /// What you tried.
    pub tried: String,
    /// Why no tool could do it.
    pub why: String,
}

impl Tool for CapabilityGapLog {
    const NAME: &'static str = "capability_gap";
    const DESCRIPTION: &'static str = "When no tool or extension can do what the owner asked, log it for the developer, \
then offer the owner the closest thing you can do.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let id = ext::log_gap(cx.db, GapSource::Agent, &self.asked, &self.tried, &self.why, None, cx.now)?;
        Ok(json!({"id": id}))
    }
}
