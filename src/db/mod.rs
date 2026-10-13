//! SQLite: the same file and schema the Python app used. Synchronous on purpose: one owner, short
//! transactions, and domain code stays free of async. The async runtime calls in via `spawn_blocking`.
pub mod models;
mod table;

use std::path::Path;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
pub use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::error::Result;

/// Schema of the last Python (alembic) revision; fresh databases start here.
const BASELINE: &str = include_str!("baseline.sql");
const ALEMBIC_HEAD: &str = "3d5e7f9a1b20";
/// Rust-owned migrations after the baseline, applied in order and tracked by `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[include_str!("migrations/001_agent.sql")];

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
    Ok(conn)
}

/// Read-only connection for the dashboard and the agent's `sql_query` tool.
pub fn open_readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA query_only=ON;")?;
    Ok(conn)
}

pub fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    let has_alembic: bool = tx.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'alembic_version'", [], |r| r.get(0))?;
    if !has_alembic {
        tx.execute_batch(BASELINE)?;
        tx.execute("INSERT INTO alembic_version (version_num) VALUES (?1)", [ALEMBIC_HEAD])?;
    } else {
        let head: String = tx.query_row("SELECT version_num FROM alembic_version", [], |r| r.get(0))?;
        if head != ALEMBIC_HEAD {
            crate::bail!("SCHEMA_MISMATCH", "Database is at alembic {head}; run the last Python migration ({ALEMBIC_HEAD}) first");
        }
    }
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
    }
    tx.commit()?;
    Ok(())
}

/// Consistent copy of the live DB (`VACUUM INTO`), e.g. before the agent runs raw SQL writes.
pub fn snapshot(conn: &Connection, dest: &Path) -> Result<()> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
    Ok(())
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A JSON text column with a typed value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Json<T>(pub T);

impl<T: Serialize> ToSql for Json<T> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let text = serde_json::to_string(&self.0).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        Ok(ToSqlOutput::from(text))
    }
}

impl<T: DeserializeOwned> FromSql for Json<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        serde_json::from_str(value.as_str()?).map(Json).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl<T: Serialize> Serialize for Json<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<T> std::ops::Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// A text column holding one of a fixed set of values (the schema's CHECK constraints).
#[macro_export]
macro_rules! sql_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        pub enum $name { $(#[serde(rename = $text)] $variant),+ }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::error::Error;
            fn from_str(s: &str) -> $crate::error::Result<Self> {
                match s {
                    $($text => Ok($name::$variant),)+
                    other => Err($crate::error::Error::domain("INVALID_VALUE", format!(concat!("Unknown ", stringify!($name), ": {}"), other))),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                value.as_str()?.parse().map_err(|e: $crate::error::Error| rusqlite::types::FromSqlError::Other(e.to_string().into()))
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::models::{AgentEvent, SetLog, SetType};
    use crate::domain::profile::{get_profile, update_profile};
    use crate::time::Ts;

    #[test]
    fn migrations_and_restart() {
        let path = std::env::temp_dir().join(format!("gymclaw-test-{}.db", new_id()));
        let mut db = open(&path).unwrap();
        migrate(&mut db).unwrap();
        migrate(&mut db).unwrap();
        update_profile(&db, json!({"prep_minutes": 23}).as_object().unwrap()).unwrap();
        let at = Ts::parse("2026-10-01T00:00:00+00:00", None).unwrap();
        AgentEvent { id: new_id(), r#type: "test".into(), created_at: at, payload_json: Json(json!({})), handled_at: None, correlation_id: None }.insert(&db).unwrap();
        drop(db);
        let mut db = open(&path).unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(get_profile(&db).unwrap().prep_minutes, 23);
        assert_eq!(AgentEvent::select(&db, "", []).unwrap()[0].created_at, at);
        let version: String = db.query_row("SELECT version_num FROM alembic_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, ALEMBIC_HEAD);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = crate::domain::workout::tests::db();
        let orphan = SetLog {
            id: new_id(), workout_exercise_id: "missing".into(), set_number: 1, set_type: SetType::Working, weight: 80.0, reps: 9, rir_optional: None,
            logged_at: Ts::parse("2026-10-01T00:00:00+00:00", None).unwrap(), rest_started_at: None, rest_due_at: None,
        };
        assert!(orphan.insert(&db).is_err());
    }
}
