//! Read-only SQL for open-ended questions, plus notes/lessons included in every agent prompt.
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolDef, def};
use crate::db::models::{AgentNote, NoteKind};
use crate::db::Connection;
use crate::error::Result;

pub fn tools() -> Vec<ToolDef> {
    vec![def::<SqlQuery>(), def::<NoteAdd>(), def::<NoteEdit>(), def::<NoteDelete>()]
}

/// Tables holding OAuth tokens and the private feed URL stay outside model access.
const SECRET_TABLES: [&str; 2] = ["calendar_credential", "personal_calendar_state"];
const MAX_ROWS: usize = 200;

/// Read-only pragmas the model may use to inspect the schema.
const SCHEMA_PRAGMAS: [&str; 5] = ["table_info", "table_xinfo", "index_list", "index_info", "foreign_key_list"];

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
    const DESCRIPTION: &'static str = "Read anything in the GymClaw database with one SELECT. Use it for history or analysis. Schema: planned_session (planned_start_at, planned_end_at, status, workout_template_id, user_locked), workout_session, workout_exercise, set_log, workout_template, user_profile, agent_note and agent_event. JOIN on *_id; inspect sqlite_master or PRAGMA table_info for columns. UTC text timestamps, JSON text columns. At most 200 rows. OAuth credentials and private feed URLs are excluded.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        // File-backed tools use an OS read-only handle. Tests/ephemeral databases get an
        // isolated copy; query_only and the authorizer enforce the same read contract.
        let db = match cx.db.path().filter(|p| !p.is_empty()) {
            Some(path) => crate::db::open_readonly(std::path::Path::new(path))?,
            None => {
                let mut copy = Connection::open_in_memory()?;
                rusqlite::backup::Backup::new(cx.db, &mut copy)?.run_to_completion(100, std::time::Duration::from_millis(1), None)?;
                copy.execute_batch("PRAGMA query_only=ON")?;
                copy
            }
        };
        db.authorizer(Some(|ctx: AuthContext<'_>| {
            let allowed = match ctx.action {
                AuthAction::Select | AuthAction::Recursive | AuthAction::Function { .. } => true,
                AuthAction::Read { table_name, .. } => !SECRET_TABLES.contains(&table_name),
                AuthAction::Pragma { pragma_name, .. } => SCHEMA_PRAGMAS.contains(&pragma_name),
                _ => false,
            };
            if allowed { Authorization::Allow } else { Authorization::Deny }
        }))?;
        {
            let mut stmt = match db.prepare(&self.sql) {
                Ok(stmt) => stmt,
                Err(e) if e.to_string().contains("no such ") => crate::bail!("SQL_SCHEMA", "{e}. {}", schema_hint(&db, &self.sql)?),
                Err(e) => return Err(e.into()),
            };
            if !stmt.readonly() {
                crate::bail!("NOT_READ_ONLY", "sql_query only reads; use the session tools to change sessions");
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
        }
    }
}

/// Columns of the tables a failed query names (or the table list if it names none), so a guessed
/// column costs one retry instead of a schema lookup plus a retry.
fn schema_hint(db: &Connection, sql: &str) -> Result<String> {
    let words: std::collections::HashSet<&str> = sql.split(|c: char| !(c.is_alphanumeric() || c == '_')).collect();
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?
        .into_iter()
        .filter(|t| !SECRET_TABLES.contains(&t.as_str()))
        .collect();
    let named: Vec<&String> = tables.iter().filter(|t| words.contains(t.as_str())).collect();
    if named.is_empty() {
        return Ok(format!("Tables: {}", tables.join(", ")));
    }
    let mut out = vec![];
    for table in named {
        let columns: Vec<String> = db.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        out.push(format!("{table}({})", columns.join(", ")));
    }
    Ok(format!("Columns: {}", out.join("; ")))
}

#[derive(Deserialize, JsonSchema)]
pub struct NoteAdd {
    /// `owner`: a lasting preference or fact about the owner. `lesson`: how to do something next time
    /// (e.g. a workaround that worked). Never current state (a missed session, an open question):
    /// the database already holds it.
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Ts;

    #[test]
    fn query_is_read_only_and_capped() {
        let db = crate::domain::workout::tests::db();
        let dir = std::env::temp_dir();
        let mut cx = Ctx { db: &db, now: Ts::now(), request_id: "r".into(), data_dir: &dir, effects: vec![] };
        let query = |cx: &mut Ctx, sql: &str| SqlQuery { sql: sql.into() }.run(cx);
        assert!(query(&mut cx, "DELETE FROM agent_note RETURNING id").is_err());
        assert!(query(&mut cx, "PRAGMA query_only=OFF").is_err());
        assert!(query(&mut cx, "ATTACH DATABASE ':memory:' AS other").is_err());
        assert!(query(&mut cx, "SELECT * FROM calendar_credential").is_err());
        let result = query(&mut cx, "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<201) SELECT x FROM n").unwrap();
        assert_eq!(result["rows"].as_array().unwrap().len(), 200);
        assert_eq!(result["truncated"], true);
        assert!(!query(&mut cx, "SELECT name FROM sqlite_master").unwrap()["rows"].as_array().unwrap().is_empty());
        let guessed = query(&mut cx, "SELECT start_time FROM planned_session").unwrap_err().to_string();
        assert!(guessed.contains("planned_session(") && guessed.contains("planned_start_at"), "{guessed}");
        let missing = query(&mut cx, "SELECT * FROM sessions").unwrap_err().to_string();
        assert!(missing.contains("Tables:") && !missing.contains("calendar_credential"), "{missing}");
    }

}
