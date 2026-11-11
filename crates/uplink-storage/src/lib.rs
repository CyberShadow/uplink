//! SQLite storage: on-disk layout, connection settings and migrations
//! (SPEC §4).

use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub use rusqlite;

pub mod seal;
pub mod store;

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

/// Opens a read-only connection to the database [`open`] created under
/// `data_root`.
pub fn open_reader(data_root: &Path) -> Result<Connection> {
    let path = db_path(data_root);
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} for reading", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

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
    fn reader_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        drop(open(dir.path()).unwrap());
        let reader = open_reader(dir.path()).unwrap();
        reader
            .query_row("SELECT count(*) FROM tx_log", [], |r| r.get::<_, i64>(0))
            .unwrap();
        reader.execute("DELETE FROM tx_log", []).unwrap_err();
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

    /// A label's counts, as SPEC §4 defines them, counted from scratch.
    fn counted(conn: &Connection) -> Vec<(i64, i64, i64)> {
        let conversation = "coalesce(e.thread_id, '#' || e.id)";
        conn.prepare(&format!(
            "WITH hidden AS (
               SELECT el.entry FROM entry_label el JOIN label h ON h.id = el.label
               WHERE el.desired AND h.external_id IN ('TRASH', 'SPAM')),
             unread AS (
               SELECT el.entry FROM entry_label el JOIN label u ON u.id = el.label
               WHERE el.desired AND u.external_id = 'UNREAD')
             SELECT l.id,
               (SELECT count(DISTINCT {conversation}) FROM entry_label el
                JOIN mail_entry e ON e.id = el.entry
                WHERE el.label = l.id AND el.desired
                  AND (l.external_id IN ('TRASH', 'SPAM')
                       OR el.entry NOT IN (SELECT entry FROM hidden))),
               (SELECT count(DISTINCT {conversation}) FROM unread u
                JOIN entry_label el ON el.entry = u.entry AND el.label = l.id AND el.desired
                JOIN mail_entry e ON e.id = el.entry
                WHERE l.external_id IN ('TRASH', 'SPAM')
                   OR u.entry NOT IN (SELECT entry FROM hidden))
             FROM label l ORDER BY l.id"
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn kept(conn: &Connection) -> Vec<(i64, i64, i64)> {
        conn.prepare("SELECT id, conversations, unread_conversations FROM label ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The label counts the schema keeps (V0017) stay those counted from
    /// scratch, through every kind of change, in a long pseudo-random run;
    /// and the migration's own count of existing mail agrees.
    #[test]
    fn label_counts_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, kind) VALUES ('a', 'gmail'), ('b', 'gmail');
             INSERT INTO message (id, message_id, preferred_variant, to_addrs, cc_addrs,
                                  senders, snippet)
             VALUES (1, 'm', 'v', '[]', '[]', '[]', '');
             INSERT INTO message_variant (sha256, message, source, size_bytes)
             VALUES ('v', 1, 'a', 0);",
        )
        .unwrap();
        const LABELS: [&str; 6] = ["INBOX", "UNREAD", "TRASH", "SPAM", "STARRED", "Label_1"];
        let add_label = |account: &str, external_id: &str| {
            conn.execute(
                "INSERT OR IGNORE INTO label (account_id, external_id, name, system)
                 VALUES (?1, ?2, ?2, 1)",
                params![account, external_id],
            )
            .unwrap();
        };
        for account in ["a", "b"] {
            for label in LABELS {
                add_label(account, label);
            }
        }
        // xorshift: the same run every time.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n) as i64
        };
        let threads = [None, Some("t1"), Some("t2"), Some("t3")];
        let mut external = 0;
        for step in 0..1500 {
            let account = ["a", "b"][next(2) as usize];
            let entries: Vec<i64> = conn
                .prepare("SELECT id FROM mail_entry WHERE account_id = ?1")
                .unwrap()
                .query_map([account], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let labels: Vec<i64> = conn
                .prepare("SELECT id FROM label WHERE account_id = ?1")
                .unwrap()
                .query_map([account], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let entry = entries
                .get(next(entries.len().max(1) as u64) as usize)
                .copied();
            let label = labels[next(labels.len() as u64) as usize];
            let thread = threads[next(threads.len() as u64) as usize];
            match (next(100), entry) {
                (0..12, _) | (_, None) => {
                    external += 1;
                    conn.execute(
                        "INSERT INTO mail_entry (account_id, external_id, message, variant,
                                                 thread_id, received_at)
                         VALUES (?1, ?2, 1, 'v', ?3, ?4)",
                        params![account, external.to_string(), thread, step],
                    )
                    .unwrap();
                }
                // As the mailbox writes them: an upsert, or a removal.
                (12..70, Some(entry)) => {
                    let (desired, observed) = (next(3) > 0, next(2) == 0);
                    conn.execute(
                        "INSERT INTO entry_label (entry, label, desired, observed)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT (entry, label)
                         DO UPDATE SET desired = excluded.desired, observed = excluded.observed",
                        params![entry, label, desired, observed || !desired],
                    )
                    .unwrap();
                }
                (70..85, Some(entry)) => {
                    conn.execute(
                        "DELETE FROM entry_label WHERE entry = ?1 AND label = ?2",
                        [entry, label],
                    )
                    .unwrap();
                }
                (85..92, Some(entry)) => {
                    conn.execute("DELETE FROM mail_entry WHERE id = ?1", [entry])
                        .unwrap();
                }
                (92..97, Some(entry)) => {
                    conn.execute(
                        "UPDATE mail_entry SET thread_id = ?2 WHERE id = ?1",
                        params![entry, thread],
                    )
                    .unwrap();
                }
                (_, Some(_)) => {
                    // A label goes, and (as a new one) comes back.
                    let external_id: String = conn
                        .query_row(
                            "SELECT external_id FROM label WHERE id = ?1",
                            [label],
                            |r| r.get(0),
                        )
                        .unwrap();
                    conn.execute("DELETE FROM label WHERE id = ?1", [label])
                        .unwrap();
                    add_label(account, &external_id);
                }
            }
            assert_eq!(kept(&conn), counted(&conn), "after step {step}");
        }
        let total: i64 = kept(&conn).iter().map(|(_, total, _)| total).sum();
        assert!(total > 20, "the run ends with mail: {total}");

        // The migration counts existing mail the same.
        let migration = include_str!("../migrations/V0017__label_counts.sql");
        let populate = &migration[migration.find("-- The mailboxes as they are.").unwrap()..];
        conn.execute_batch(
            "DELETE FROM counted_label; DELETE FROM label_conversation;
             UPDATE label SET conversations = 0, unread_conversations = 0;",
        )
        .unwrap();
        conn.execute_batch(populate).unwrap();
        assert_eq!(kept(&conn), counted(&conn));
    }

    /// A database migrated as far as `version`, as an older uplink left it.
    fn migrated_to(data_root: &Path, version: i32) -> Connection {
        let path = db_path(data_root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut conn = Connection::open(&path).unwrap();
        embedded::migrations::runner()
            .set_target(refinery::Target::Version(version))
            .run(&mut conn)
            .unwrap();
        conn
    }

    /// Recorded `outbox_changed` events take the shape that names
    /// accounts (V0022), as replay deserializes them.
    #[test]
    fn outbox_changed_events_name_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 21);
        let log = |id: i64, account: Option<&str>, events: &str| {
            let inverse = account.is_none().then_some("{}");
            conn.execute(
                "INSERT INTO tx_log (id, ts, account_id, description, op_type, op_json,
                                     inverse_type, inverse_json, events_json)
                 VALUES (?1, 0, ?2, '', 'x', '{}', ?3, ?3, ?4)",
                params![id, account, inverse.map(|_| "y"), events],
            )
            .unwrap();
        };
        log(
            1,
            None,
            r#"[{"type":"drafts_changed","drafts":["d"]},{"type":"outbox_changed","items":[7]}]"#,
        );
        log(2, Some("b"), r#"[{"type":"outbox_changed","items":[9]}]"#);
        log(3, None, r#"[{"type":"outbox_changed","items":[42]}]"#);
        log(4, None, r#"[{"type":"outbox_changed","accounts":["a"]}]"#);
        conn.execute(
            "INSERT INTO mail_send (id, account_id, draft, message_id, delay_ms, not_before,
                                    wanted, desired_by, state, synced)
             VALUES (7, 'a', '{}', 'm', 0, 0, 1, 1, 'queued', 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let conn = open(dir.path()).unwrap();
        let events: Vec<String> = conn
            .prepare("SELECT events_json FROM tx_log ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            events,
            [
                r#"[{"type":"drafts_changed","drafts":["d"]},{"type":"outbox_changed","accounts":["a"]}]"#,
                r#"[{"type":"outbox_changed","accounts":["b"]}]"#,
                r#"[{"type":"outbox_changed","accounts":[]}]"#,
                r#"[{"type":"outbox_changed","accounts":["a"]}]"#,
            ]
        );
    }

    /// Matrix outbox items become sends (V0023), wanted, with the op that
    /// queued them; recorded events name their rooms.
    #[test]
    fn matrix_outbox_items_become_sends() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 22);
        conn.execute_batch(
            r#"INSERT INTO account (id, kind) VALUES ('mx', 'matrix');
               INSERT INTO matrix_room (id, account_id, room_id, name, encrypted, membership)
               VALUES (5, 'mx', '!r:x', 'Room', 0, 'joined');
               INSERT INTO tx_log (id, ts, description, op_type, op_json, inverse_type,
                                   inverse_json, events_json)
               VALUES (1, 0, '', 'matrix.send', '{}', 'matrix.cancel_send', '{}',
                       '[{"type":"matrix_outbox_changed","items":[3]}]'),
                      (2, 0, '', 'matrix.send', '{}', 'matrix.cancel_send', '{}',
                       '[{"type":"matrix_outbox_changed","items":[4]}]'),
                      (3, 0, '', 'matrix.send', '{}', 'matrix.cancel_send', '{}',
                       '[{"type":"matrix_outbox_changed","items":[1]}]');
               INSERT INTO matrix_outbox (id, room, body, queued_at, state, error, event_id)
               VALUES (3, 5, 'hi', 0, 'failed', 'M_FORBIDDEN', NULL),
                      (4, 5, 'yo', 0, 'sent', NULL, '$e'),
                      (9, 5, 'gone', 0, 'sent', NULL, '$g');
               DELETE FROM matrix_outbox WHERE id = 9;"#,
        )
        .unwrap();
        drop(conn);

        let conn = open(dir.path()).unwrap();
        let sends: Vec<(i64, String, i64, i64)> = conn
            .prepare("SELECT id, state, wanted, desired_by FROM matrix_send ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            sends,
            [(3, "refused".into(), 1, 1), (4, "sent".into(), 1, 2)]
        );
        let events: Vec<String> = conn
            .prepare("SELECT events_json FROM tx_log ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            events,
            [
                r#"[{"type":"matrix_sends_changed","rooms":[5]}]"#,
                r#"[{"type":"matrix_sends_changed","rooms":[5]}]"#,
                // Its send was deleted, synced back.
                r#"[{"type":"matrix_sends_changed","rooms":[]}]"#,
            ]
        );
        // Ids go on from the old table's, past those deleted.
        let next: i64 = conn
            .query_row(
                "INSERT INTO matrix_send (room, body, queued_at, wanted, desired_by, state, synced)
                 VALUES (5, 'x', 0, 1, 1, 'queued', 0) RETURNING id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(next, 10);
    }

    /// A membership asked for records the op that asked (V0024); refusals
    /// of memberships no longer asked for were decided on already.
    #[test]
    fn memberships_asked_for_record_their_op() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 23);
        conn.execute_batch(
            r#"INSERT INTO account (id, kind) VALUES ('mx', 'matrix');
               INSERT INTO tx_log (id, ts, description, op_type, op_json, inverse_type,
                                   inverse_json, events_json)
               VALUES (1, 0, '', 'matrix.change_membership', '{"room":5,"change":"join"}',
                       'matrix.revert_membership', '{}', '[]'),
                      (2, 0, '', 'matrix.revert_membership',
                       '{"room":5,"change":"join","prior":"invited"}',
                       'matrix.change_membership', '{}', '[]'),
                      (3, 0, '', 'matrix.change_membership', '{"room":6,"change":"leave"}',
                       'matrix.revert_membership', '{}', '[]');
               INSERT INTO matrix_room (id, account_id, room_id, name, encrypted, membership,
                                        desired, membership_error)
               VALUES (5, 'mx', '!a:x', 'A', 0, 'joined', 'left', NULL),
                      (6, 'mx', '!b:x', 'B', 0, 'joined', NULL, 'M_FORBIDDEN'),
                      (7, 'mx', '!c:x', 'C', 0, 'joined', NULL, NULL);"#,
        )
        .unwrap();
        drop(conn);

        let conn = open(dir.path()).unwrap();
        let rooms: Vec<(i64, Option<i64>, Option<String>)> = conn
            .prepare("SELECT id, membership_by, membership_error FROM matrix_room ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rooms,
            [(5, Some(2), None), (6, None, None), (7, None, None)]
        );
    }
}
