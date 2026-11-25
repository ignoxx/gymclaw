//! SQLite. Synchronous on purpose: one owner, short
//! transactions, and domain code stays free of async. The async runtime calls in via `spawn_blocking`.
pub mod models;
mod table;
#[cfg(test)]
pub mod test;

use std::path::Path;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
pub use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::error::Result;

/// The schema migrations start from (the long-lived production DB began here); tests replay it.
#[cfg(test)]
const BASELINE: &str = include_str!("baseline.sql");
/// The current schema: baseline plus every migration. Fresh databases are created from it directly, so
/// tests don't replay history; `schema_snapshot_is_current` keeps it equal to the migrated result.
const SCHEMA: &str = include_str!("schema.sql");
/// Baseline marker: the one row of the `alembic_version` table (the name is historical). Every DB,
/// fresh or the long-lived production one, carries exactly this value; anything else is refused.
const BASELINE_MARKER: &str = "3d5e7f9a1b20";
/// Migrations after the baseline, applied in order and tracked by `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_agent.sql"),
    include_str!("migrations/002_learned_notes.sql"),
    include_str!("migrations/003_session_follows.sql"),
    include_str!("migrations/004_plan_text.sql"),
    include_str!("migrations/005_profile_rest.sql"),
    include_str!("migrations/006_timed_sets.sql"),
    include_str!("migrations/007_set_measures.sql"),
    include_str!("migrations/008_goals_activities.sql"),
    include_str!("migrations/009_extensions.sql"),
    include_str!("migrations/010_gym_locations.sql"),
    include_str!("migrations/011_dead_state.sql"),
    include_str!("migrations/012_weight_unit.sql"),
    include_str!("migrations/013_ics_token.sql"),
    include_str!("migrations/014_drop_alembic_version.sql"),
    include_str!("migrations/015_hot_query_indexes.sql"),
];

/// Schema `PRAGMA user_version` this binary expects after `migrate()`.
pub fn expected_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Read-write connection. WAL lets readers (the dashboard, a tick waiting on a feed fetch) run alongside a
/// writer, and transactions take the write lock when they begin: a read-then-write transaction that
/// races another writer would otherwise fail at once with "database is locked" instead of waiting out
/// `busy_timeout`. Transactions that call the network inside opt back into DEFERRED (`network_tx`).
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL;")?;
    conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
    Ok(conn)
}

/// Run `f` in one transaction (holding the write lock, see `open`), committed only when it succeeds:
/// a failure midway leaves nothing behind. `domain::core::mutate` relies on its caller for this.
pub fn in_tx<T>(conn: &mut Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let tx = conn.transaction()?;
    let value = f(&tx)?;
    tx.commit()?;
    Ok(value)
}

/// A DEFERRED transaction for work that calls the network: it doesn't hold the write lock through the
/// fetch, so taps and typed sets aren't stalled behind the network. Under WAL its reads never block a writer;
/// if one commits mid-fetch, this transaction's own write fails and the next tick retries.
pub fn network_tx(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?)
}

/// Read-only connection for the dashboard and the agent's `sql_query` tool.
pub fn open_readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA query_only=ON;")?;
    Ok(conn)
}

/// Foreign keys are off while migrating, so a migration can rebuild a table other tables reference
/// (SQLite's documented way to change a CHECK); a migration that leaves more broken foreign keys than
/// it found is rolled back.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let enforced: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys=OFF")?;
    let result = apply(conn);
    if enforced {
        conn.execute_batch("PRAGMA foreign_keys=ON")?;
    }
    result
}

fn apply(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    let broken = |tx: &rusqlite::Transaction| -> Result<i64> {
        Ok(tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?)
    };
    let before = broken(&tx)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let tables: i64 = tx.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if version == 0 && tables == 0 {
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", MIGRATIONS.len()))?;
    } else {
        let has_alembic: bool =
            tx.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'alembic_version'", [], |r| r.get(0))?;
        if has_alembic {
            let marker: String = tx.query_row("SELECT version_num FROM alembic_version", [], |r| r.get(0))?;
            if marker != BASELINE_MARKER {
                crate::bail!(
                    "SCHEMA_MISMATCH",
                    "Database has baseline marker {marker}, expected {BASELINE_MARKER}; it predates the baseline and can't be upgraded"
                );
            }
        } else if version == 0 {
            crate::bail!(
                "SCHEMA_MISMATCH",
                "Database has existing tables with user_version = 0 and no baseline marker"
            );
        }
    }
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
    }
    let after = broken(&tx)?;
    if after > before {
        crate::bail!("SCHEMA_MISMATCH", "Migrations broke {} foreign keys", after - before);
    }
    // Seed an ICS export token for existing profiles that predate migration 013.
    let token = new_id().replace('-', "");
    tx.execute("UPDATE user_profile SET ics_token = ?1 WHERE ics_token IS NULL", [&token])?;
    tx.commit()?;
    Ok(())
}

/// Consistent backup copy of the DB (`VACUUM INTO`).
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
    use crate::db::models::AgentEvent;
    use crate::domain::profile::{get_profile, update_profile};
    use crate::time::Ts;

    #[test]
    fn migrations_and_restart() {
        let (path, mut db) = test::file();
        migrate(&mut db).unwrap();
        update_profile(&db, json!({"prep_minutes": 23}).as_object().unwrap()).unwrap();
        let at = Ts::parse("2026-10-01T00:00:00+00:00", None).unwrap();
        AgentEvent {
            id: new_id(),
            r#type: "test".into(),
            created_at: at,
            payload_json: Json(json!({})),
            handled_at: None,
            correlation_id: None,
        }
        .insert(&db)
        .unwrap();
        drop(db);
        let mut db = open(&path).unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(get_profile(&db).unwrap().prep_minutes, 23);
        assert_eq!(AgentEvent::select(&db, "", []).unwrap()[0].created_at, at);
        let has_alembic: i64 = db
            .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'alembic_version'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(has_alembic, 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 2026-11-13: a set tap failed with "database is locked" while the calendar sync held its
    /// transaction open across a Google fetch. A tap must commit while a sync waits on the network.
    #[test]
    fn a_tap_commits_while_a_sync_waits_on_the_network() {
        let (path, mut setup) = test::file();
        setup.execute_batch("CREATE TABLE t (x INTEGER)").unwrap();
        let sync = open(&path).unwrap();
        let fetching = network_tx(&sync).unwrap();
        fetching.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0)).unwrap();
        // Short timeout: under the old rollback journal this waited out busy_timeout and failed.
        setup.busy_timeout(std::time::Duration::from_millis(50)).unwrap();
        let tap = setup.transaction().unwrap();
        tap.execute("INSERT INTO t VALUES (1)", []).unwrap();
        tap.commit().unwrap();
        // The sync's own write now fails (its snapshot is stale) and the next tick retries it.
        assert!(fetching.execute("INSERT INTO t VALUES (2)", []).is_err());
        drop(fetching);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 006 rebuilds set_log, which notification_job references, and turns the owner's cardio Cycling
    /// (3 × 8–10 at 0 kg) into one 25-minute timed block.
    #[test]
    fn timed_sets_migration_keeps_sets_and_times_the_cardio_bike() {
        let mut db = Connection::open_in_memory().unwrap();
        // Off while planting rows whose parents a test doesn't need; on for migrate, as `open` leaves it.
        db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        db.execute_batch(BASELINE).unwrap();
        db.execute("INSERT INTO alembic_version (version_num) VALUES (?1)", [BASELINE_MARKER]).unwrap();
        for sql in &MIGRATIONS[..5] {
            db.execute_batch(sql).unwrap();
        }
        db.execute_batch(
            r#"PRAGMA user_version = 5;
            INSERT INTO workout_template VALUES ('cardio', '{"id":"cardio","name":"Cardio","exercises":[
                {"id":"warm","name":"Rower","role":"warmup","target_weight":0},
                {"id":"cycling","name":"Cycling","role":"conditioning","working_sets":3,"target_weight":0}]}', '2026-11-01 00:00:00');
            INSERT INTO set_log (id, workout_exercise_id, set_number, set_type, weight, reps, logged_at)
                VALUES ('s1', 'e1', 1, 'WORKING', 80, 8, '2026-11-01 18:00:00');
            INSERT INTO notification_job (id, kind, workout_session_id, set_log_id, due_at, status, payload_json)
                VALUES ('j1', 'REST', 'w1', 's1', '2026-11-01 18:01:30', 'FIRED', '{}');
            PRAGMA foreign_keys=ON;"#,
        )
        .unwrap();
        migrate(&mut db).unwrap();
        let (reps, seconds): (i64, Option<i64>) = db
            .query_row("SELECT reps, seconds FROM set_log WHERE id = 's1'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((reps, seconds), (8, None));
        let to_sets: i64 = db
            .query_row(
                "SELECT count(*) FROM pragma_foreign_key_check('notification_job') WHERE \"table\" = 'set_log'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(to_sets, 0, "the rest job still points at its set");
        let template = crate::domain::templates::get_template(&db, "cardio").unwrap();
        let [rower, bike] = &template.exercises[..] else { panic!("two exercises") };
        assert_eq!((rower.set_seconds(), bike.set_seconds(), bike.working_sets), (None, Some(1500), 1));
        let enforced: bool = db.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert!(enforced, "foreign keys are back on after migrating");
    }

    /// Every schema object, as `schema.sql` lists them.
    fn dump(db: &Connection) -> String {
        let mut stmt = db
            .prepare(
                "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
                 ORDER BY tbl_name, type DESC, name",
            )
            .unwrap();
        let objects: Vec<String> =
            stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|s| s.unwrap() + ";").collect();
        objects.join("\n\n")
    }

    /// `schema.sql` is what baseline plus every migration produces (the upgrade path), so agents can
    /// read the schema in one place and fresh databases start from it. A stale file is rewritten.
    #[test]
    fn schema_snapshot_is_current() {
        let mut upgraded = Connection::open_in_memory().unwrap();
        upgraded.execute_batch(BASELINE).unwrap();
        upgraded.execute("INSERT INTO alembic_version (version_num) VALUES (?1)", [BASELINE_MARKER]).unwrap();
        migrate(&mut upgraded).unwrap();
        let actual = format!(
            "-- Generated from baseline.sql plus every migration (user_version {}); don't edit. When a\n\
             -- migration changes the schema, `cargo test schema_snapshot` rewrites this file: review and\n\
             -- commit it. Fresh databases are created from this file.\n\n{}\n",
            MIGRATIONS.len(),
            dump(&upgraded)
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/db/schema.sql");
        if std::fs::read_to_string(&path).ok().as_deref() != Some(actual.as_str()) {
            std::fs::write(&path, &actual).unwrap();
            panic!("src/db/schema.sql was stale and has been regenerated; review the diff and commit it");
        }
        let mut fresh = Connection::open_in_memory().unwrap();
        migrate(&mut fresh).unwrap();
        assert_eq!(dump(&fresh), dump(&upgraded), "a fresh database matches a migrated one");
    }

    #[test]
    fn hot_queries_use_partial_and_fk_indexes() {
        let db = test::db();
        let plan = |sql: &str| -> String {
            let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            stmt.query_map([], |r| r.get::<_, String>(3)).unwrap().map(|r| r.unwrap()).collect::<Vec<_>>().join(" | ")
        };
        assert!(
            plan("SELECT * FROM agent_event WHERE type = 'coach.app_view' AND handled_at IS NULL ORDER BY rowid")
                .contains("ix_agent_event_unhandled")
        );
        assert!(
            plan("SELECT * FROM notification_delivery WHERE status = 'PENDING' AND recipient = '42' ORDER BY created_at, id")
                .contains("ix_notification_delivery_status")
        );
        assert!(
            plan("SELECT * FROM notification_job WHERE status = 'PENDING' ORDER BY due_at, id")
                .contains("ix_notification_job_pending")
        );
        assert!(
            plan("SELECT * FROM workout_exercise WHERE workout_session_id = 'w1'")
                .contains("ix_workout_exercise_session")
        );
    }
}
