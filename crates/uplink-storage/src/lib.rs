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

    drop_sparse_occurrences(&mut conn)
        .with_context(|| format!("repairing the calendar of {}", path.display()))?;
    embedded::migrations::runner()
        .run(&mut conn)
        .with_context(|| format!("migrating {}", path.display()))?;

    Ok(conn)
}

/// Before V0046, the calendar sync recorded the occurrences of a series as
/// Graph's delta sends them, sparse (id, type, start, end, seriesMasterId
/// only), with no subject; V0046 and V0051 fill columns from every event's
/// JSON, and fail on them. Being Graph's, they are dropped, and their
/// accounts' calendars listed anew, whole (the sync now completes them
/// from their series). Done ahead of the migrations, as an applied one
/// is not to change.
fn drop_sparse_occurrences(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    let tables = |name: &str| -> rusqlite::Result<bool> {
        tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )
    };
    if !tables("refinery_schema_history")? || !tables("calendar_event")? {
        return Ok(());
    }
    let version: i64 = tx.query_row(
        "SELECT coalesce(max(version), 0) FROM refinery_schema_history",
        [],
        |r| r.get(0),
    )?;
    if version >= 46 {
        return Ok(());
    }
    const SPARSE: &str = "json_extract(raw, '$.isCancelled') IS NULL";
    tx.execute(
        &format!(
            "DELETE FROM calendar_sync WHERE account_id IN
               (SELECT account_id FROM calendar_event WHERE {SPARSE})"
        ),
        [],
    )?;
    tx.execute(&format!("DELETE FROM calendar_event WHERE {SPARSE}"), [])?;
    tx.commit()?;
    Ok(())
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

    /// Occurrences recorded sparse before V0046 are dropped, and their
    /// accounts' calendars listed anew, so that the migrations reading
    /// every event's JSON pass; whole events stay.
    #[test]
    fn sparse_occurrences_are_dropped_before_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 45);
        let whole = r#"{"type":"singleInstance","isCancelled":false,"showAs":"busy",
            "responseStatus":{"response":"organizer"},"isReminderOn":false,
            "lastModifiedDateTime":"2026-11-01T10:00:00Z"}"#;
        let sparse = r#"{"type":"occurrence","id":"o","seriesMasterId":"m",
            "start":{},"end":{}}"#;
        conn.execute_batch(&format!(
            r#"INSERT INTO account (id, kind) VALUES ('a', 'm365'), ('b', 'm365');
               INSERT INTO calendar_event (account_id, external_id, series, exception,
                   subject, location, preview, start_ts, end_ts, all_day, raw)
               VALUES ('a', 'e', NULL, 0, 'Whole', '', '', 0, 1, 0, '{whole}'),
                      ('a', 'o', 'm', 0, '', '', '', 0, 1, 0, '{sparse}'),
                      ('b', 'f', NULL, 0, 'Whole', '', '', 0, 1, 0, '{whole}');
               INSERT INTO calendar_sync (account_id, state) VALUES ('a', '{{}}'), ('b', '{{}}');"#
        ))
        .unwrap();
        drop(conn);
        let conn = open(dir.path()).unwrap();
        let rows: Vec<String> = conn
            .prepare("SELECT external_id FROM calendar_event ORDER BY 1")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, ["e", "f"]);
        let synced: Vec<String> = conn
            .prepare("SELECT account_id FROM calendar_sync")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(synced, ["b"]);
    }

    /// Senders' addresses whose images are shown become sender rules
    /// (V0041), recorded ops and events too, as undo and replay read them.
    #[test]
    fn image_senders_become_rules() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 40);
        conn.execute_batch(
            r#"INSERT INTO image_sender (address) VALUES ('s@x');
               INSERT INTO tx_log (id, ts, description, op_type, op_json, inverse_type,
                                   inverse_json, events_json)
               VALUES (1, 0, '', 'mail.always_show_images', '{"sender":"s@x","always":true}',
                       'mail.always_show_images', '{"sender":"s@x","always":false}',
                       '[{"type":"image_sender_changed","sender":"s@x","always":true}]'),
                      (2, 0, '', 'x', '{}', 'y', '{}', '[{"type":"drafts_changed","drafts":[]}]');"#,
        )
        .unwrap();
        drop(conn);

        let conn = open(dir.path()).unwrap();
        let rules: Vec<(String, String)> = conn
            .prepare("SELECT kind, value FROM image_rule")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rules, [("sender".into(), "s@x".into())]);
        let log: Vec<(String, String, String)> = conn
            .prepare("SELECT op_json, inverse_json, events_json FROM tx_log ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            log,
            [
                (
                    r#"{"rule":{"type":"sender","address":"s@x"},"always":true}"#.into(),
                    r#"{"rule":{"type":"sender","address":"s@x"},"always":false}"#.into(),
                    r#"[{"type":"image_rule_changed","rule":{"type":"sender","address":"s@x"},"always":true}]"#.into(),
                ),
                (
                    "{}".into(),
                    "{}".into(),
                    r#"[{"type":"drafts_changed","drafts":[]}]"#.into()
                ),
            ]
        );
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
                "INSERT INTO matrix_send (room, content, queued_at, wanted, desired_by, state, synced)
                 VALUES (5, '{}', 0, 1, 1, 'queued', 0) RETURNING id",
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

    /// Matrix sends' text becomes their content (V0026), and so do the
    /// recorded ops that send it; ids go on past those deleted.
    #[test]
    fn matrix_sends_have_content() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 25);
        conn.execute_batch(
            r#"INSERT INTO account (id, kind) VALUES ('mx', 'matrix');
               INSERT INTO matrix_room (id, account_id, room_id, name, encrypted, membership)
               VALUES (5, 'mx', '!r:x', 'Room', 0, 'joined');
               INSERT INTO tx_log (id, ts, description, op_type, op_json, inverse_type,
                                   inverse_json, events_json)
               VALUES (1, 0, '', 'matrix.send', '{"room":5,"body":"hi"}',
                       'matrix.cancel_send', '{"item":3}', '[]'),
                      (2, 0, '', 'matrix.cancel_send', '{"item":3}',
                       'matrix.send', '{"room":5,"body":"hi"}', '[]');
               INSERT INTO matrix_send (id, room, body, queued_at, wanted, desired_by, state,
                                        synced)
               VALUES (3, 5, 'hi', 0, 1, 1, 'queued', 0), (4, 5, 'gone', 0, 1, 1, 'queued', 0);
               DELETE FROM matrix_send WHERE id = 4;"#,
        )
        .unwrap();
        drop(conn);

        // As V0026 left them: later ones change content again (V0030).
        let conn = migrated_to(dir.path(), 26);
        let text = r#"{"type":"text","body":"hi","emote":false,"reply_to":null,"replaces":null}"#;
        let content: String = conn
            .query_row("SELECT content FROM matrix_send WHERE id = 3", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(content, text);
        let ops: Vec<(String, String)> = conn
            .prepare("SELECT op_json, inverse_json FROM tx_log ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let send = format!(r#"{{"room":5,"content":{text}}}"#);
        assert_eq!(
            ops,
            [
                (send.clone(), r#"{"item":3}"#.to_owned()),
                (r#"{"item":3}"#.to_owned(), send),
            ]
        );
        let next: i64 = conn
            .query_row(
                "INSERT INTO matrix_send (room, content, queued_at, wanted, desired_by, state, synced)
                 VALUES (5, '{}', 0, 1, 1, 'queued', 0) RETURNING id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(next, 5);
    }

    /// Matrix sends of text, and the log's, are Markdown (V0030), as
    /// their contents say since; sends are kept as they were (V0029).
    #[test]
    fn matrix_texts_are_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 28);
        let text = r#"{"type":"text","body":"hi","emote":false,"reply_to":null,"replaces":null}"#;
        let op = format!(r#"{{"room":5,"content":{text}}}"#);
        conn.execute_batch(&format!(
            r#"INSERT INTO account (id, kind) VALUES ('mx', 'matrix');
               INSERT INTO matrix_room (id, account_id, room_id, name, encrypted, membership)
               VALUES (5, 'mx', '!r:x', 'Room', 0, 'joined');
               INSERT INTO tx_log (id, ts, description, op_type, op_json, inverse_type,
                                   inverse_json, events_json)
               VALUES (1, 0, '', 'matrix.send', '{op}', 'matrix.cancel_send', '{{"item":3}}', '[]'),
                      (2, 0, '', 'matrix.cancel_send', '{{"item":3}}', 'matrix.send', '{op}', '[]'),
                      (3, 0, '', 'matrix.send',
                       '{{"room":5,"content":{{"type":"reaction","event":"$e","key":"x"}}}}',
                       'matrix.cancel_send', '{{"item":4}}', '[]');
               INSERT INTO matrix_send (id, room, content, queued_at, wanted, desired_by, state,
                                        error, event_id, synced)
               VALUES (3, 5, '{text}', 0, 1, 1, 'sent', NULL, '$e', 0),
                      (4, 5, '{{"type":"reaction","event":"$e","key":"x"}}', 0, 1, 3, 'queued',
                       NULL, NULL, 0);"#
        ))
        .unwrap();
        drop(conn);

        let conn = open(dir.path()).unwrap();
        let markdown = |sql: &str| -> Vec<Option<bool>> {
            conn.prepare(sql)
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            markdown("SELECT json_extract(content, '$.markdown') FROM matrix_send ORDER BY id"),
            [Some(true), None]
        );
        assert_eq!(
            markdown("SELECT json_extract(op_json, '$.content.markdown') FROM tx_log ORDER BY id"),
            [Some(true), None, None]
        );
        assert_eq!(
            markdown(
                "SELECT json_extract(inverse_json, '$.content.markdown') FROM tx_log ORDER BY id"
            ),
            [None, Some(true), None]
        );
        let event: String = conn
            .query_row("SELECT event_id FROM matrix_send WHERE id = 3", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(event, "$e");
    }

    /// Drafts say whom they are from (V0055): the account's own address,
    /// as all were, in the outbox's and the log's copies too, as sending,
    /// undo and redo read them.
    #[test]
    fn drafts_are_from_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 54);
        let content = r#"{"to":"b@x","cc":"","bcc":"","subject":"Hi","body":"<p>hi</p>"}"#;
        let draft = format!(
            r#"{{"id":"d","account":"a","revision":0,"content":{content},"attachments":[],"in_reply_to":null,"updated_at":0}}"#
        );
        let put = format!(
            r#"{{"id":"d","account":"a","revision":0,"content":{content},"attachments":[],"in_reply_to":null}}"#
        );
        conn.execute_batch(&format!(
            r#"INSERT INTO draft (id, account_id, revision, to_addrs, cc_addrs, bcc_addrs,
                                  subject, body_html, updated_at)
               VALUES ('e', 'a', 0, 'b@x', '', '', 'Hi', '', 0);
               INSERT INTO tx_log (id, ts, description, op_type, op_json, inverse_type,
                                   inverse_json, events_json)
               VALUES (1, 0, '', 'draft.put', '{put}', 'draft.delete', '{{"id":"d"}}', '[]'),
                      (2, 0, '', 'draft.delete', '{{"id":"d"}}', 'draft.put', '{put}', '[]'),
                      (3, 0, '', 'mail.send', '{{"id":"d","delay_ms":0}}', 'mail.cancel_send',
                       '{{"item":1}}', '[]');
               INSERT INTO mail_send (id, account_id, draft, message_id, delay_ms, not_before,
                                      wanted, desired_by, state, synced)
               VALUES (1, 'a', '{draft}', 'm@a', 0, 0, 1, 3, 'queued', 0);"#
        ))
        .unwrap();
        drop(conn);

        let conn = open(dir.path()).unwrap();
        let from = |sql: &str| -> Vec<Option<String>> {
            conn.prepare(sql)
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let empty = || Some(String::new());
        assert_eq!(from("SELECT from_addr FROM draft"), [empty()]);
        assert_eq!(
            from("SELECT json_extract(draft, '$.content.from') FROM mail_send"),
            [empty()]
        );
        assert_eq!(
            from("SELECT json_extract(op_json, '$.content.from') FROM tx_log ORDER BY id"),
            [empty(), None, None]
        );
        assert_eq!(
            from("SELECT json_extract(inverse_json, '$.content.from') FROM tx_log ORDER BY id"),
            [None, empty(), None]
        );
    }

    /// The full-text index gets Cc (V0031), from what was stored before.
    #[test]
    fn search_indexes_cc() {
        let dir = tempfile::tempdir().unwrap();
        let conn = migrated_to(dir.path(), 30);
        conn.execute_batch(
            r#"INSERT INTO message (id, message_id, preferred_variant, subject, from_addr,
                                   to_addrs, cc_addrs, senders, snippet)
               VALUES (1, '1@x', 'h1', 'One', 'a@x', '["b@x"]', '["c@x","d@y"]', '[]', ''),
                      (2, '2@x', 'h2', 'Two', 'a@x', '["b@x"]', '[]', '[]', '');
               INSERT INTO message_fts (rowid, subject, from_addr, from_name, to_addrs, body_text)
               VALUES (1, 'One', 'a@x', '', 'b@x', 'body one'),
                      (2, 'Two', 'a@x', '', 'b@x', 'body two');"#,
        )
        .unwrap();
        drop(conn);

        let conn = open(dir.path()).unwrap();
        let found = |fts: &str| -> Vec<i64> {
            conn.prepare("SELECT rowid FROM message_fts WHERE message_fts MATCH ?1 ORDER BY rowid")
                .unwrap()
                .query_map([fts], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(found("{cc_addrs} : \"d@y\""), [1]);
        assert_eq!(found("{cc_addrs} : \"b\""), Vec::<i64>::new());
        assert_eq!(found("body"), [1, 2]);
        assert_eq!(found("{to_addrs} : \"b@x\""), [1, 2]);
    }
}
