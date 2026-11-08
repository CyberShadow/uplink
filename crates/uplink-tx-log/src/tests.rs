use super::*;
use crate::sample::{SampleDelete, SampleInsert, SampleObserved};
use std::sync::Arc;
use tokio::sync::broadcast;

struct Fixture {
    dir: tempfile::TempDir,
    /// Separate connection for inspecting and tampering with the database.
    db: Connection,
    dispatcher: Dispatcher,
    published: broadcast::Receiver<Arc<LogEntry>>,
}

fn fixture_with(registry: Registry) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let conn = uplink_storage::open(dir.path()).unwrap();
    conn.execute_batch(sample::SCHEMA).unwrap();
    let hub = Arc::new(Hub::new(64));
    let published = hub.subscribe();
    let db = Connection::open(uplink_storage::db_path(dir.path())).unwrap();
    Fixture {
        dir,
        db,
        dispatcher: Dispatcher::new(conn, registry, hub),
        published,
    }
}

fn fixture() -> Fixture {
    let mut registry = Registry::new();
    sample::register(&mut registry);
    fixture_with(registry)
}

#[derive(Debug, PartialEq)]
struct Row {
    id: i64,
    description: String,
    op_type: String,
    inverse_type: Option<String>,
    undone_by: Option<i64>,
    undoes: Option<i64>,
}

impl Fixture {
    fn log(&self) -> Vec<Row> {
        self.db
            .prepare(
                "SELECT id, description, op_type, inverse_type, undone_by, undoes
                 FROM tx_log ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    description: r.get(1)?,
                    op_type: r.get(2)?,
                    inverse_type: r.get(3)?,
                    undone_by: r.get(4)?,
                    undoes: r.get(5)?,
                })
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn samples(&self) -> Vec<(i64, i64)> {
        self.db
            .prepare("SELECT id, x FROM sample ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn published(&mut self) -> Vec<LogEntry> {
        let mut out = vec![];
        while let Ok(entry) = self.published.try_recv() {
            out.push((*entry).clone());
        }
        out
    }

    fn reader(&self, max_replay: usize) -> Arc<LogReader> {
        let conn = uplink_storage::open_reader(self.dir.path()).unwrap();
        Arc::new(LogReader::new(conn, max_replay))
    }
}

fn row(
    id: i64,
    description: &str,
    op_type: &str,
    inverse_type: Option<&str>,
    undone_by: Option<i64>,
    undoes: Option<i64>,
) -> Row {
    Row {
        id,
        description: description.into(),
        op_type: op_type.into(),
        inverse_type: inverse_type.map(Into::into),
        undone_by,
        undoes,
    }
}

const INSERT: &str = "sample.insert";
const DELETE: &str = "sample.delete";

#[tokio::test]
async fn dispatch_applies_records_and_publishes() {
    let mut f = fixture();
    let id = f
        .dispatcher
        .dispatch(SampleInsert { id: 7, x: 42 })
        .await
        .unwrap();

    assert_eq!(f.samples(), vec![(7, 42)]);
    assert_eq!(
        f.log(),
        vec![row(
            id.0,
            "insert sample 7 = 42",
            INSERT,
            Some(DELETE),
            None,
            None
        )]
    );
    let published = f.published();
    assert_eq!(published.len(), 1);
    let entry = &published[0];
    assert_eq!(entry.id, id);
    assert_eq!(entry.source, Source::User);
    assert_eq!(entry.description, "insert sample 7 = 42");
    assert_eq!(entry.op_type, INSERT);
    assert_eq!(entry.undoes, None);
}

#[tokio::test]
async fn observation_is_recorded_without_inverse() {
    let mut f = fixture();
    let id = f
        .dispatcher
        .observe("acct".into(), SampleObserved { id: 3, x: 9 })
        .await
        .unwrap();

    assert_eq!(f.samples(), vec![(3, 9)]);
    assert_eq!(
        f.log(),
        vec![row(
            id.0,
            "observed sample 3 = 9",
            "sample.observed",
            None,
            None,
            None
        )]
    );
    let published = f.published();
    assert_eq!(published[0].source, Source::Account { id: "acct".into() });

    let err = f.dispatcher.undo(id).await.unwrap_err();
    assert!(
        matches!(err, UndoError::NotUndoable(i) if i == id),
        "{err:?}"
    );
    assert_eq!(f.log().len(), 1);
}

#[tokio::test]
async fn undo_and_redo_round_trip() {
    let mut f = fixture();
    let op = f
        .dispatcher
        .dispatch(SampleInsert { id: 7, x: 42 })
        .await
        .unwrap();
    f.published();

    let undo = f.dispatcher.undo(op).await.unwrap();
    assert_eq!(f.samples(), vec![]);
    assert_eq!(
        f.log(),
        vec![
            row(
                op.0,
                "insert sample 7 = 42",
                INSERT,
                Some(DELETE),
                Some(undo.0),
                None
            ),
            row(
                undo.0,
                "delete sample 7",
                DELETE,
                Some(INSERT),
                None,
                Some(op.0)
            ),
        ]
    );
    let published = f.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].id, undo);
    assert_eq!(published[0].undoes, Some(op));

    let redo = f.dispatcher.undo(undo).await.unwrap();
    assert_eq!(f.samples(), vec![(7, 42)]);
    assert_eq!(
        f.log(),
        vec![
            row(
                op.0,
                "insert sample 7 = 42",
                INSERT,
                Some(DELETE),
                Some(undo.0),
                None
            ),
            row(
                undo.0,
                "delete sample 7",
                DELETE,
                Some(INSERT),
                Some(redo.0),
                Some(op.0)
            ),
            row(
                redo.0,
                "insert sample 7 = 42",
                INSERT,
                Some(DELETE),
                None,
                Some(undo.0)
            ),
        ]
    );
}

#[tokio::test]
async fn undo_twice_is_refused() {
    let mut f = fixture();
    let op = f
        .dispatcher
        .dispatch(SampleInsert { id: 1, x: 1 })
        .await
        .unwrap();
    let undo = f.dispatcher.undo(op).await.unwrap();
    f.published();

    let err = f.dispatcher.undo(op).await.unwrap_err();
    assert!(
        matches!(err, UndoError::AlreadyUndone { id, undone_by } if id == op && undone_by == undo),
        "{err:?}"
    );
    assert_eq!(f.log().len(), 2);
    assert_eq!(f.published(), vec![]);
}

#[tokio::test]
async fn undo_of_missing_entry_is_refused() {
    let f = fixture();
    let err = f.dispatcher.undo(TxLogId(99)).await.unwrap_err();
    assert!(matches!(err, UndoError::NotFound(TxLogId(99))), "{err:?}");
}

#[tokio::test]
async fn failed_apply_records_nothing() {
    let mut f = fixture();
    f.dispatcher
        .dispatch(SampleInsert { id: 1, x: 1 })
        .await
        .unwrap();
    f.published();

    // Primary key conflict.
    f.dispatcher
        .dispatch(SampleInsert { id: 1, x: 2 })
        .await
        .unwrap_err();
    assert_eq!(f.samples(), vec![(1, 1)]);
    assert_eq!(f.log().len(), 1);
    assert_eq!(f.published(), vec![]);
}

#[tokio::test]
async fn conflicting_undo_is_refused() {
    let mut f = fixture();
    let op = f
        .dispatcher
        .dispatch(SampleInsert { id: 1, x: 1 })
        .await
        .unwrap();
    f.published();

    // The row the inverse would delete is gone.
    f.db.execute("DELETE FROM sample", []).unwrap();
    let err = f.dispatcher.undo(op).await.unwrap_err();
    assert!(
        matches!(err, UndoError::Conflict { id, .. } if id == op),
        "{err:?}"
    );
    assert_eq!(
        f.log(),
        vec![row(
            op.0,
            "insert sample 1 = 1",
            INSERT,
            Some(DELETE),
            None,
            None
        )]
    );
    assert_eq!(f.published(), vec![]);
}

#[tokio::test]
async fn unregistered_inverse_kind_is_rejected_at_dispatch() {
    let mut registry = Registry::new();
    registry.register::<SampleInsert>();
    let mut f = fixture_with(registry);

    let err = f
        .dispatcher
        .dispatch(SampleInsert { id: 1, x: 1 })
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains(SampleDelete::KIND), "{err:#}");
    assert_eq!(f.samples(), vec![]);
    assert_eq!(f.log(), vec![]);
    assert_eq!(f.published(), vec![]);
}

#[test]
#[should_panic(expected = "registered twice")]
fn duplicate_registration_panics() {
    let mut registry = Registry::new();
    registry.register::<SampleInsert>();
    registry.register::<SampleInsert>();
}

// ── replay ────────────────────────────────────────────────────────────────

fn ids(entries: &[LogEntry]) -> Vec<i64> {
    entries.iter().map(|e| e.id.0).collect()
}

#[tokio::test]
async fn replay_returns_entries_as_published() {
    let mut f = fixture();
    let op = f
        .dispatcher
        .dispatch(SampleInsert { id: 1, x: 1 })
        .await
        .unwrap();
    f.dispatcher
        .observe("acct".into(), SampleObserved { id: 2, x: 2 })
        .await
        .unwrap();
    f.dispatcher.undo(op).await.unwrap();
    let published = f.published();

    let replay = f.reader(10).replay(Some(TxLogId(0))).await.unwrap();
    assert_eq!(replay.head, TxLogId(3));
    assert_eq!(replay.entries.unwrap(), published);

    let replay = f.reader(10).replay(Some(TxLogId(1))).await.unwrap();
    assert_eq!(ids(&replay.entries.unwrap()), vec![2, 3]);

    let replay = f.reader(10).replay(Some(TxLogId(3))).await.unwrap();
    assert_eq!(replay.entries.unwrap(), vec![]);
}

#[tokio::test]
async fn replay_requires_reload() {
    let f = fixture();
    for id in 1..=3 {
        f.dispatcher
            .dispatch(SampleInsert { id, x: id })
            .await
            .unwrap();
    }
    let reader = f.reader(2);
    // A fresh client.
    let replay = reader.replay(None).await.unwrap();
    assert_eq!(replay.head, TxLogId(3));
    assert!(replay.entries.is_none());
    // Too far behind.
    assert!(reader
        .replay(Some(TxLogId(0)))
        .await
        .unwrap()
        .entries
        .is_none());
    assert_eq!(
        ids(&reader
            .replay(Some(TxLogId(1)))
            .await
            .unwrap()
            .entries
            .unwrap()),
        vec![2, 3]
    );
    // A position this log never had (e.g. the data directory was replaced).
    assert!(reader
        .replay(Some(TxLogId(4)))
        .await
        .unwrap()
        .entries
        .is_none());
}

#[tokio::test]
async fn replay_of_empty_log() {
    let f = fixture();
    let replay = f.reader(2).replay(Some(TxLogId(0))).await.unwrap();
    assert_eq!(replay.head, TxLogId(0));
    assert_eq!(replay.entries.unwrap(), vec![]);
}
