//! Fixtures shared by the tests of every module.
use std::path::PathBuf;

use super::models::RuntimeSettings;
use super::{Connection, migrate, new_id, open};

/// A migrated DB at `<fresh temp dir>/gymclaw.db`. A file, not `:memory:`: `sql_query`, the
/// dashboard, the outbox and the bot open their own connections to the path.
pub fn file() -> (PathBuf, Connection) {
    let path = std::env::temp_dir().join(format!("gymclaw-test-{}", new_id())).join("gymclaw.db");
    let mut db = open(&path).unwrap();
    migrate(&mut db).unwrap();
    (path, db)
}

pub fn db() -> Connection {
    file().1
}

/// The runtime row as `runtime_resume` leaves it without extra grants (owner 42). Change fields with
/// `RuntimeSettings { enabled: false, ..authority() }.insert(&db)`.
pub fn authority() -> RuntimeSettings {
    RuntimeSettings {
        id: 1,
        profile: crate::runtime::outbox::PROFILE.into(),
        recipient: "42".into(),
        project_root: String::new(),
        python_path: String::new(),
        template_id: None,
        enabled: true,
        calendar_writes_enabled: false,
        crowd_polling_enabled: false,
    }
}

/// A fresh DB with the one-exercise "short" template the planning tests schedule.
pub fn with_short_template() -> Connection {
    let db = db();
    let short = serde_json::json!({"id": "short", "name": "Short", "exercises": [{"id": "bench", "name": "Bench", "role": "press",
        "primary": true, "working_sets": 2, "target_weight": 80, "warmup_weight": 50}]});
    crate::domain::templates::import_template(&db, &serde_json::from_value(short).unwrap()).unwrap();
    db
}
