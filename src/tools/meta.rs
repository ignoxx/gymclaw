//! The agent's way past missing verbs: read anything with SQL, change anything with SQL (after a DB
//! snapshot), and keep notes and lessons that go into its own prompt. Owner: lead.
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolDef, def};
use crate::db::models::{AgentNote, NoteKind};
use crate::db::Connection;
use crate::domain::core::emit;
use crate::error::Result;

pub fn tools() -> Vec<ToolDef> {
    vec![def::<SqlQuery>(), def::<SqlWrite>(), def::<NoteAdd>(), def::<NoteEdit>(), def::<NoteDelete>()]
}

/// Tables that hold secrets (OAuth tokens, the private feed URL). Never readable or writable by the model.
const SECRET_TABLES: [&str; 2] = ["calendar_credential", "personal_calendar_state"];
const MAX_ROWS: usize = 200;

/// Deny any access to secret tables for the duration of `f`.
fn guarded<T>(db: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    db.authorizer(Some(|ctx: AuthContext<'_>| {
        let table = match ctx.action {
            AuthAction::Read { table_name, .. } | AuthAction::Update { table_name, .. } | AuthAction::Insert { table_name } | AuthAction::Delete { table_name } => table_name,
            _ => return Authorization::Allow,
        };
        if SECRET_TABLES.contains(&table) { Authorization::Deny } else { Authorization::Allow }
    }))?;
    let result = f();
    db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result
}

fn cell(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef::*;
    match value {
        Null => Value::Null,
        Integer(i) => json!(i),
        Real(f) => json!(f),
        Text(t) => {
            let text = String::from_utf8_lossy(t);
            // JSON columns come back as JSON.
            if text.starts_with('{') || text.starts_with('[') { serde_json::from_str(&text).unwrap_or(Value::String(text.into())) } else { Value::String(text.into()) }
        }
        Blob(b) => json!(format!("<{} bytes>", b.len())),
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SqlQuery {
    /// One read-only SQLite statement. Timestamps are naive UTC text; JSON columns are text.
    /// `SELECT name, sql FROM sqlite_master` shows the schema.
    pub sql: String,
}

impl Tool for SqlQuery {
    const NAME: &'static str = "sql_query";
    const DESCRIPTION: &'static str = "Read anything in the GymClaw database with one SELECT. Use it when no other tool answers the question. At most 200 rows.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        guarded(cx.db, || {
            let mut stmt = cx.db.prepare(&self.sql)?;
            if !stmt.readonly() {
                crate::bail!("NOT_READ_ONLY", "sql_query only reads; use sql_write to change data");
            }
            let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
            let mut rows = stmt.query([])?;
            let mut out = vec![];
            while let Some(row) = rows.next()? {
                if out.len() == MAX_ROWS {
                    return Ok(json!({"columns": columns, "rows": out, "truncated": true}));
                }
                out.push(Value::Array((0..columns.len()).map(|i| row.get_ref(i).map(cell)).collect::<rusqlite::Result<_>>()?));
            }
            Ok(json!({"columns": columns, "rows": out, "truncated": false}))
        })
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SqlWrite {
    /// SQLite statements that change data. Keep the schema's formats: naive UTC timestamps
    /// ("2026-10-14 10:30:00.000000"), uuid4 text ids, JSON text.
    pub sql: String,
    /// What the owner asked for, in a few words (kept with the change).
    pub why: String,
}

impl Tool for SqlWrite {
    const NAME: &'static str = "sql_write";
    const DESCRIPTION: &'static str = "Change data directly when no other tool can do what the owner asked. A DB snapshot is taken first. \
        Check the affected rows with sql_query before and after. Prefer the dedicated tools: they also update reminders and the calendar.";
    const SNAPSHOT: bool = true;

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let before = cx.db.total_changes();
        guarded(cx.db, || Ok(cx.db.execute_batch(&self.sql)?))?;
        let changed = cx.db.total_changes() - before;
        emit(cx.db, "agent.sql_write", cx.now, json!({"sql": self.sql, "why": self.why, "rows_changed": changed}))?;
        Ok(json!({"rows_changed": changed}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct NoteAdd {
    /// `owner`: a lasting preference or fact about the owner. `lesson`: how to do something next time
    /// (e.g. a workaround that worked).
    pub kind: NoteKind,
    /// One short line.
    pub text: String,
}

impl Tool for NoteAdd {
    const NAME: &'static str = "note_add";
    const DESCRIPTION: &'static str = "Remember something for every future conversation (it goes into your instructions).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let note = AgentNote { id: None, kind: self.kind, text: self.text.trim().into(), created_at: cx.now, updated_at: cx.now };
        note.insert(cx.db)?;
        Ok(json!({"id": cx.db.last_insert_rowid()}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct NoteEdit {
    pub id: i64,
    pub text: String,
}

impl Tool for NoteEdit {
    const NAME: &'static str = "note_edit";
    const DESCRIPTION: &'static str = "Rewrite a note (ids are shown with the notes in your instructions).";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let Some(mut note) = AgentNote::get(cx.db, self.id)? else { crate::bail!("NOTE_NOT_FOUND", "No note {}", self.id) };
        note.text = self.text.trim().into();
        note.updated_at = cx.now;
        note.update(cx.db)?;
        Ok(json!({"id": self.id}))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct NoteDelete {
    pub id: i64,
}

impl Tool for NoteDelete {
    const NAME: &'static str = "note_delete";
    const DESCRIPTION: &'static str = "Forget a note that is wrong or outdated.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let deleted = cx.db.execute("DELETE FROM agent_note WHERE id = ?1", [self.id])?;
        Ok(json!({"deleted": deleted == 1}))
    }
}

/// Notes for the prompt, oldest first.
pub fn notes(db: &Connection) -> Result<Vec<AgentNote>> {
    AgentNote::select(db, "ORDER BY kind, id", [])
}

