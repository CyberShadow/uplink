use super::*;
use crate::sample::{SampleDelete, SampleInsert};
use std::sync::Arc;
use tokio::sync::broadcast;
use uplink_realtime::Published;

struct Fixture {
    _dir: tempfile::TempDir,
    /// Separate connection for inspecting the database.
    db: Connection,
    dispatcher: Dispatcher,
    events: broadcast::Receiver<Arc<Published>>,
}

fn fixture_with(registry: Registry) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let conn = uplink_storage::open(dir.path()).unwrap();
    conn.execute_batch(sample::SCHEMA).unwrap();
    let hub = Arc::new(Hub::new(64));
    let events = hub.subscribe(None).live;
    let db = Connection::open(uplink_storage::db_path(dir.path())).unwrap();
    Fixture {
        _dir: dir,
        db,
        dispatcher: Dispatcher::new(conn, registry, hub),
        events,
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
    inverse_type: String,
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

    fn published(&mut self) -> Vec<ServerEvent> {
        let mut out = vec![];
        while let Ok(p) = self.events.try_recv() {
            out.push(p.event.clone());
        }
        out
    }
}

fn row(
    id: i64,
    description: &str,
    op_type: &str,
    inverse_type: &str,
    undone_by: Option<i64>,
    undoes: Option<i64>,
) -> Row {
    Row {
        id,
        description: description.into(),
        op_type: op_type.into(),
        inverse_type: inverse_type.into(),
        undone_by,
        undoes,
    }
}

fn added(event: &ServerEvent) -> &TxLogEntry {
    match event {
        ServerEvent::TxLogEntryAdded { entry } => entry,
        other => panic!("expected tx_log_entry_added, got {other:?}"),
    }
}

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
            "sample.insert",
            "sample.delete",
            None,
            None
        )]
    );
    let events = f.published();
    assert_eq!(events.len(), 1);
    let entry = added(&events[0]);
    assert_eq!(entry.id, id);
    assert_eq!(entry.actor, "user");
    assert_eq!(entry.description, "insert sample 7 = 42");
    assert_eq!(entry.op_type, "sample.insert");
    assert_eq!(entry.undoes, None);
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
                "sample.insert",
                "sample.delete",
                Some(undo.0),
                None
            ),
            row(
                undo.0,
                "delete sample 7",
                "sample.delete",
                "sample.insert",
                None,
                Some(op.0)
            ),
        ]
    );
    let events = f.published();
    assert_eq!(events.len(), 2);
    assert_eq!(added(&events[0]).id, undo);
    assert_eq!(added(&events[0]).undoes, Some(op));
    assert_eq!(
        events[1],
        ServerEvent::TxLogEntryUndone {
            id: op,
            undone_by: undo
        }
    );

    let redo = f.dispatcher.undo(undo).await.unwrap();
    assert_eq!(f.samples(), vec![(7, 42)]);
    assert_eq!(
        f.log(),
        vec![
            row(
                op.0,
                "insert sample 7 = 42",
                "sample.insert",
                "sample.delete",
                Some(undo.0),
                None
            ),
            row(
                undo.0,
                "delete sample 7",
                "sample.delete",
                "sample.insert",
                Some(redo.0),
                Some(op.0)
            ),
            row(
                redo.0,
                "insert sample 7 = 42",
                "sample.insert",
                "sample.delete",
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
async fn failed_undo_rolls_back() {
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
    assert!(matches!(err, UndoError::Failed(_)), "{err:?}");
    assert_eq!(
        f.log(),
        vec![row(
            op.0,
            "insert sample 1 = 1",
            "sample.insert",
            "sample.delete",
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
