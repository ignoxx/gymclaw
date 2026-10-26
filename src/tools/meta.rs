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
    /// One read-only SQLite statement. Timestamps are stored as naive UTC text; JSON columns are text.
    pub sql: String,
}

impl Tool for SqlQuery {
    const NAME: &'static str = "sql_query";
    const DESCRIPTION: &'static str = "Read anything in the GymClaw database with one SELECT. Use it for history or analysis. The schema is in your instructions (\"Database\"); JOIN on *_id. Timestamps are stored as naive UTC text (compare against UTC in WHERE) but come back in the owner's local time with offset. JSON text columns. At most 200 rows. OAuth credentials and private feed URLs are excluded.";

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
                // Syntax errors and denied tables are a wrong query, not a broken database.
                Err(e) => crate::bail!("INVALID_SQL", "{e}"),
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

/// The model-visible schema, one `table(col, …)` line per table, read live so it always matches
/// the migrated DB. Goes into the system prompt and into failed-query errors.
pub fn schema(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = db.prepare(
        "SELECT m.name, group_concat(p.name, ', ') FROM sqlite_master m JOIN pragma_table_info(m.name) p
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' AND m.name != 'alembic_version' GROUP BY m.name ORDER BY m.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().filter(|(t, _)| !SECRET_TABLES.contains(&t.as_str())).map(|(t, cols)| (t.clone(), format!("{t}({cols})"))).collect())
}

/// Columns of the tables a failed query names (every table if it names none).
fn schema_hint(db: &Connection, sql: &str) -> Result<String> {
    let words: std::collections::HashSet<&str> = sql.split(|c: char| !(c.is_alphanumeric() || c == '_')).collect();
    let tables = schema(db)?;
    let named: Vec<&str> = tables.iter().filter(|(t, _)| words.contains(t.as_str())).map(|(_, line)| line.as_str()).collect();
    let lines = if named.is_empty() { tables.iter().map(|(_, line)| line.as_str()).collect() } else { named };
    Ok(format!("Schema: {}", lines.join("; ")))
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
    const DESCRIPTION: &'static str = "Remember something for every future conversation (it goes into your instructions). Each kind has a \
character budget, shown with the notes; when it's full, merge, shorten or delete notes first and keep what matters most.";

    fn run(self, cx: &mut Ctx) -> Result<Value> {
        let note = AgentNote { id: None, kind: self.kind, text: self.text.trim().into(), created_at: cx.now, updated_at: cx.now };
        check_budget(cx.db, note.kind, None, &note.text)?;
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
        check_budget(cx.db, note.kind, Some(&note), self.text.trim())?;
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

/// Characters each kind of note may use in total. Memory goes into every prompt, so it stays small and
/// the agent has to choose: a new note past the budget fails until older ones are condensed.
pub fn budget(kind: NoteKind) -> usize {
    match kind {
        NoteKind::Owner => 1500,
        NoteKind::Lesson => 800,
    }
}

/// Characters the notes of `kind` use now.
pub fn used(db: &Connection, kind: NoteKind) -> Result<usize> {
    Ok(notes(db)?.iter().filter(|n| n.kind == kind).map(|n| n.text.chars().count()).sum())
}

/// Refuse a write that grows `kind` past its budget. Shrinking is always allowed, so memory that is
/// already over (e.g. after the budget was lowered) can be condensed step by step.
fn check_budget(db: &Connection, kind: NoteKind, replacing: Option<&AgentNote>, text: &str) -> Result<()> {
    let (old, new) = (replacing.map_or(0, |n| n.text.chars().count()), text.chars().count());
    let (used, limit) = (used(db, kind)?, budget(kind));
    if new > old && used - old + new > limit {
        crate::bail!("NOTE_BUDGET", "{} are at {used}/{limit} chars and this needs {} more. Condense first: merge related notes, shorten wording, \
delete what no longer matters (note_edit/note_delete), then retry.", label(kind), new - old);
    }
    Ok(())
}

fn label(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::Owner => "Owner notes",
        NoteKind::Lesson => "Lessons",
    }
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
        assert!(guessed.contains("planned_session(") && guessed.contains("planned_start_at") && !guessed.contains("set_log("), "{guessed}");
        let missing = query(&mut cx, "SELECT * FROM sessions").unwrap_err().to_string();
        assert!(missing.contains("set_log(") && !missing.contains("calendar_credential"), "{missing}");
    }

    #[test]
    fn notes_stay_within_budget_but_can_always_shrink() {
        let db = crate::domain::workout::tests::db();
        let dir = std::env::temp_dir();
        let mut cx = Ctx { db: &db, now: Ts::now(), request_id: "r".into(), data_dir: &dir, effects: vec![] };
        let limit = budget(NoteKind::Lesson);
        let add = |cx: &mut Ctx, text: String| NoteAdd { kind: NoteKind::Lesson, text }.run(cx);
        let id = add(&mut cx, "a".repeat(limit - 10)).unwrap()["id"].as_i64().unwrap();
        add(&mut cx, "b".repeat(10)).unwrap();
        let full = add(&mut cx, "c".into()).unwrap_err();
        assert_eq!(full.code(), "NOTE_BUDGET");
        assert!(full.to_string().contains(&format!("{limit}/{limit}")), "{full}");
        // Owner notes have their own budget.
        NoteAdd { kind: NoteKind::Owner, text: "Trains at noon".into() }.run(&mut cx).unwrap();
        // Condensing frees room for the new lesson.
        NoteEdit { id, text: "a".repeat(100) }.run(&mut cx).unwrap();
        add(&mut cx, "c".into()).unwrap();
        // Over budget already (budget lowered): shrinking still works, growing doesn't.
        cx.db.execute("UPDATE agent_note SET text = ?1 WHERE id = ?2", (&"a".repeat(limit), id)).unwrap();
        assert!(NoteEdit { id, text: "a".repeat(limit + 1) }.run(&mut cx).is_err());
        NoteEdit { id, text: "a".repeat(limit - 1) }.run(&mut cx).unwrap();
    }

}
