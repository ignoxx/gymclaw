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

/// Read-only pragmas the model may use to inspect the schema.
const SCHEMA_PRAGMAS: [&str; 5] = ["table_info", "table_xinfo", "index_list", "index_info", "foreign_key_list"];

/// For the duration of `f`, allow only plain reads and row changes outside the secret tables and
/// sqlite internals. Everything else (DDL, transaction control, ATTACH, other pragmas) is denied, so a
/// statement can't rename a secret table away or commit around the caller's savepoint.
fn guarded<T>(db: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    db.authorizer(Some(|ctx: AuthContext<'_>| {
        let allowed = match ctx.action {
            AuthAction::Select | AuthAction::Recursive | AuthAction::Function { .. } => true,
            AuthAction::Read { table_name, .. } => !SECRET_TABLES.contains(&table_name),
            AuthAction::Update { table_name, .. } | AuthAction::Insert { table_name } | AuthAction::Delete { table_name } => {
                !SECRET_TABLES.contains(&table_name) && !table_name.starts_with("sqlite_")
            }
            AuthAction::Pragma { pragma_name, .. } => SCHEMA_PRAGMAS.contains(&pragma_name),
            _ => false,
        };
        if allowed { Authorization::Allow } else { Authorization::Deny }
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
    const DESCRIPTION: &'static str = "Read anything in the GymClaw database with one SELECT. Use it for history or analysis. Schema: planned_session (times, status, workout_template_id, user_locked), workout_session, workout_exercise, set_log, workout_template, user_profile, agent_note and agent_event. JOIN on *_id; inspect sqlite_master or PRAGMA table_info for columns. UTC text timestamps, JSON text columns. At most 200 rows. OAuth credentials and private feed URLs are excluded.";

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
            let mut stmt = db.prepare(&self.sql)?;
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
        // All statements or none: a failing statement mid-batch undoes the ones before it.
        cx.db.execute_batch("SAVEPOINT sql_write")?;
        if let Err(e) = guarded(cx.db, || Ok(cx.db.execute_batch(&self.sql)?)) {
            cx.db.execute_batch("ROLLBACK TO sql_write; RELEASE sql_write")?;
            return Err(e);
        }
        cx.db.execute_batch("RELEASE sql_write")?;
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
    }

    #[test]
    fn sql_write_cannot_escape_the_guard_or_the_batch() {
        let db = crate::domain::workout::tests::db();
        let dir = std::env::temp_dir();
        let mut cx = Ctx { db: &db, now: Ts::now(), request_id: "r".into(), data_dir: &dir, effects: vec![] };
        let write = |cx: &mut Ctx, sql: &str| SqlWrite { sql: sql.into(), why: "test".into() }.run(cx);
        let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();

        // Renaming a secret table away (then reading it) is denied.
        assert!(write(&mut cx, "ALTER TABLE calendar_credential RENAME TO exposed").is_err());
        assert_eq!(count("SELECT count(*) FROM sqlite_master WHERE name = 'calendar_credential'"), 1);

        // A COMMIT can't save the first half of a failing batch.
        assert!(write(&mut cx, "INSERT INTO agent_note (kind, text, created_at, updated_at) VALUES ('owner', 'x', '', ''); COMMIT; SELECT * FROM nope").is_err());
        assert!(write(&mut cx, "INSERT INTO agent_note (kind, text, created_at, updated_at) VALUES ('owner', 'x', '', ''); SELECT * FROM nope").is_err());
        assert_eq!(count("SELECT count(*) FROM agent_note"), 0);

        assert_eq!(write(&mut cx, "INSERT INTO agent_note (kind, text, created_at, updated_at) VALUES ('owner', 'x', '', '')").unwrap()["rows_changed"], 1);
        assert_eq!(count("SELECT count(*) FROM agent_event WHERE type = 'agent.sql_write'"), 1);
    }
}
