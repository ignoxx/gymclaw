/// Declares a row struct for an existing table plus get/select/insert/update/delete.
/// Field names are the column names; the first field is the primary key.
///
/// Domain code mutates a loaded row and calls `update` explicitly (there is no autoflush).
#[macro_export]
macro_rules! table {
    ($(#[$meta:meta])* $name:ident = $table:literal { $(#[$pkmeta:meta])* $pk:ident: $pkty:ty, $($(#[$fmeta:meta])* $field:ident: $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, serde::Serialize)]
        pub struct $name { $(#[$pkmeta])* pub $pk: $pkty, $($(#[$fmeta])* pub $field: $ty),* }

        impl $name {
            pub const TABLE: &'static str = $table;
            pub const FIELDS: &'static [&'static str] = &[stringify!($pk) $(, stringify!($field))*];

            /// Quoted column list (`r#type` → "type").
            pub fn columns() -> &'static str {
                static COLUMNS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
                COLUMNS.get_or_init(|| Self::FIELDS.iter().map(|f| format!("\"{}\"", f.trim_start_matches("r#"))).collect::<Vec<_>>().join(", "))
            }

            pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
                let mut i = 0usize;
                let mut next = || { i += 1; i - 1 };
                Ok($name { $pk: row.get(next())?, $($field: row.get(next())?),* })
            }

            pub fn get(db: &rusqlite::Connection, pk: impl rusqlite::ToSql) -> $crate::error::Result<Option<Self>> {
                use rusqlite::OptionalExtension;
                let sql = format!("SELECT {} FROM {} WHERE \"{}\" = ?1", Self::columns(), $table, stringify!($pk));
                Ok(db.query_row(&sql, [pk], Self::from_row).optional()?)
            }

            /// Rows matching `clause` (everything after FROM, e.g. "WHERE status = ?1 ORDER BY due_at").
            pub fn select(db: &rusqlite::Connection, clause: &str, params: impl rusqlite::Params) -> $crate::error::Result<Vec<Self>> {
                let sql = format!("SELECT {} FROM {} {}", Self::columns(), $table, clause);
                let mut stmt = db.prepare_cached(&sql)?;
                let rows = stmt.query_map(params, Self::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            }

            pub fn first(db: &rusqlite::Connection, clause: &str, params: impl rusqlite::Params) -> $crate::error::Result<Option<Self>> {
                Ok(Self::select(db, &format!("{clause} LIMIT 1"), params)?.into_iter().next())
            }

            pub fn insert(&self, db: &rusqlite::Connection) -> $crate::error::Result<()> {
                let marks = (1..=Self::FIELDS.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
                let sql = format!("INSERT INTO {} ({}) VALUES ({})", $table, Self::columns(), marks);
                db.prepare_cached(&sql)?.execute(rusqlite::params![self.$pk $(, self.$field)*])?;
                Ok(())
            }

            pub fn update(&self, db: &rusqlite::Connection) -> $crate::error::Result<()> {
                let sets = Self::FIELDS[1..].iter().enumerate().map(|(i, f)| format!("\"{}\" = ?{}", f.trim_start_matches("r#"), i + 2)).collect::<Vec<_>>().join(", ");
                let sql = format!("UPDATE {} SET {} WHERE \"{}\" = ?1", $table, sets, stringify!($pk));
                db.prepare_cached(&sql)?.execute(rusqlite::params![self.$pk $(, self.$field)*])?;
                Ok(())
            }

            pub fn delete(&self, db: &rusqlite::Connection) -> $crate::error::Result<()> {
                let sql = format!("DELETE FROM {} WHERE \"{}\" = ?1", $table, stringify!($pk));
                db.prepare_cached(&sql)?.execute([&self.$pk])?;
                Ok(())
            }
        }
    };
}
