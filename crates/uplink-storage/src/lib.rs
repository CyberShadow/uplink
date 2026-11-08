//! SQLite storage: on-disk layout, connection settings and migrations
//! (SPEC §4).

use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub use rusqlite;

mod embedded {
    refinery::embed_migrations!("migrations");
}

/// Path of the main database file under the data root (SPEC §4.1).
pub fn db_path(data_root: &Path) -> PathBuf {
    data_root.join("db").join("uplink.sqlite")
}

/// Opens the main database under `data_root`, creating it if needed,
/// applies the connection settings of SPEC §4.2, and runs pending
/// migrations.
///
/// Fails if the migrations already applied to the database differ from the
/// embedded set (an applied migration was edited or removed).
pub fn open(data_root: &Path) -> Result<Connection> {
    let path = db_path(data_root);
    let dir = path.parent().expect("db_path has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut conn =
        Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;

    let journal_mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    ensure!(
        journal_mode == "wal",
        "{}: could not enable WAL (journal_mode is {journal_mode})",
        path.display()
    );
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -64000)?;

    embedded::migrations::runner()
        .run(&mut conn)
        .with_context(|| format!("migrating {}", path.display()))?;

    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma<T: rusqlite::types::FromSql>(conn: &Connection, name: &str) -> T {
        conn.pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn open_creates_db_and_applies_settings() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        assert!(db_path(dir.path()).is_file());
        assert_eq!(pragma::<String>(&conn, "journal_mode"), "wal");
        assert_eq!(pragma::<i64>(&conn, "synchronous"), 1); // NORMAL
        assert_eq!(pragma::<i64>(&conn, "foreign_keys"), 1);
        assert_eq!(pragma::<i64>(&conn, "temp_store"), 2); // MEMORY
        assert_eq!(pragma::<i64>(&conn, "cache_size"), -64000);
        let n: i64 = conn
            .query_row("SELECT count(*) FROM tx_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        drop(open(dir.path()).unwrap());
        drop(open(dir.path()).unwrap());
    }

    /// SPEC §4.2 / §15.7: startup must refuse a database whose applied
    /// migrations were edited.
    #[test]
    fn edited_migration_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        let tampered = conn
            .execute(
                "UPDATE refinery_schema_history SET checksum = '1' WHERE version = 1",
                [],
            )
            .unwrap();
        assert_eq!(tampered, 1);
        drop(conn);

        let err = open(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("is different than filesystem one"),
            "unexpected error: {err:#}"
        );
    }
}
