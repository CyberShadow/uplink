//! Op dispatcher, transaction log and undo (SPEC §6).
//!
//! Every user-initiated mutation of persistent state is an [`Op`] run
//! through [`Dispatcher::dispatch`]. The op is applied and recorded in
//! `tx_log` together with its inverse in one SQLite transaction; events are
//! broadcast only after commit. Undo applies the recorded inverse as a new
//! op, so the log is append-only and undoing an undo is a redo.

use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use uplink_protocol::{ServerEvent, TxLogEntry, TxLogId};
use uplink_realtime::Hub;
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};

#[cfg(any(test, feature = "test-ops"))]
pub mod sample;

/// A state-mutating operation.
pub trait Op: Serialize + DeserializeOwned + Send + 'static {
    /// Discriminator recorded in `tx_log`, e.g. `"mail.archive"`. Must be
    /// unique within a [`Registry`].
    const KIND: &'static str;

    /// Applies the op within `tx`, computing the inverse edit from what it
    /// changed. Returning an error rolls the transaction back; return a
    /// [`Conflict`] when the state the op expects is not what it finds.
    fn apply(self, tx: &Transaction) -> Result<Applied>;

    /// Human-readable description for the transaction log UI.
    fn describe(&self) -> String;
}

/// Outcome of [`Op::apply`].
pub struct Applied {
    /// The op that reverts this one.
    pub inverse: Inverse,
    /// Broadcast once the transaction commits.
    pub events: Vec<ServerEvent>,
}

/// A serialized op, recorded as the inverse of another.
pub struct Inverse {
    kind: &'static str,
    json: String,
}

impl Inverse {
    pub fn of<O: Op>(op: &O) -> Self {
        Self {
            kind: O::KIND,
            json: to_json(op),
        }
    }
}

fn to_json<O: Op>(op: &O) -> String {
    serde_json::to_string(op).unwrap_or_else(|e| panic!("{} op does not serialize: {e}", O::KIND))
}

/// The op kinds an undo may instantiate from a recorded inverse.
#[derive(Default)]
pub struct Registry {
    ops: HashMap<&'static str, ApplyJson>,
}

type ApplyJson = fn(&Transaction, &str) -> Result<Executed>;

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<O: Op>(&mut self) {
        let previous = self.ops.insert(O::KIND, execute_json::<O>);
        assert!(previous.is_none(), "op kind {} registered twice", O::KIND);
    }
}

/// An applied op, ready to be recorded.
struct Executed {
    kind: &'static str,
    op_json: String,
    description: String,
    applied: Applied,
}

fn execute<O: Op>(tx: &Transaction, op: O) -> Result<Executed> {
    let op_json = to_json(&op);
    let description = op.describe();
    let applied = op.apply(tx)?;
    Ok(Executed {
        kind: O::KIND,
        op_json,
        description,
        applied,
    })
}

fn execute_json<O: Op>(tx: &Transaction, json: &str) -> Result<Executed> {
    let op: O =
        serde_json::from_str(json).with_context(|| format!("decoding {} op {json}", O::KIND))?;
    execute(tx, op)
}

/// An op found state other than what it expects (e.g. a value it would
/// replace was changed in the meantime). Ops return it through
/// [`anyhow::Error`]; it is reported as a refusal rather than a failure.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Conflict(pub String);

#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error("tx_log entry {} does not exist", .0 .0)]
    NotFound(TxLogId),
    #[error("tx_log entry {} was already undone by entry {}", .id.0, .undone_by.0)]
    AlreadyUndone { id: TxLogId, undone_by: TxLogId },
    /// The inverse cannot be applied to the current state (SPEC §6.2).
    #[error("tx_log entry {} cannot be undone: {}", .id.0, .conflict)]
    Conflict { id: TxLogId, conflict: Conflict },
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// The only writer to the database (SPEC §4.2).
#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    registry: Registry,
    hub: Arc<Hub>,
}

/// Recorded as the actor of user-initiated ops.
const USER: &str = "user";

impl Dispatcher {
    /// `conn` must be the migrated database from [`uplink_storage::open`].
    pub fn new(conn: Connection, registry: Registry, hub: Arc<Hub>) -> Self {
        Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                registry,
                hub,
            }),
        }
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.inner.hub
    }

    /// Applies `op` and records it. Its inverse must be of a registered
    /// kind, so that the entry can be undone.
    pub async fn dispatch<O: Op>(&self, op: O) -> Result<TxLogId> {
        self.on_writer(move |inner| inner.dispatch(op)).await
    }

    /// Applies the inverse of entry `id`, recording it as a new entry that
    /// undoes `id`.
    pub async fn undo(&self, id: TxLogId) -> Result<TxLogId, UndoError> {
        self.on_writer(move |inner| inner.undo(id)).await
    }

    async fn on_writer<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner) -> R + Send + 'static,
    ) -> R {
        let inner = self.inner.clone();
        match tokio::task::spawn_blocking(move || f(&inner)).await {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("database connection poisoned")
    }

    fn dispatch<O: Op>(&self, op: O) -> Result<TxLogId> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let executed = execute(&tx, op)?;
        let entry = self.record(&tx, &executed, None)?;
        tx.commit()?;
        let id = entry.id;
        self.publish(
            &conn,
            executed.applied.events,
            [ServerEvent::TxLogEntryAdded { entry }],
        );
        Ok(id)
    }

    fn undo(&self, id: TxLogId) -> Result<TxLogId, UndoError> {
        let mut conn = self.lock();
        let tx = conn.transaction().context("beginning transaction")?;

        let (inverse_type, inverse_json, undone_by): (String, String, Option<i64>) = tx
            .query_row(
                "SELECT inverse_type, inverse_json, undone_by FROM tx_log WHERE id = ?1",
                [id.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .context("reading tx_log entry")?
            .ok_or(UndoError::NotFound(id))?;
        if let Some(by) = undone_by {
            return Err(UndoError::AlreadyUndone {
                id,
                undone_by: TxLogId(by),
            });
        }

        let execute = self
            .registry
            .ops
            .get(inverse_type.as_str())
            .with_context(|| format!("inverse op kind {inverse_type} is not registered"))?;
        let executed = execute(&tx, &inverse_json).map_err(|e| match e.downcast::<Conflict>() {
            Ok(conflict) => UndoError::Conflict { id, conflict },
            Err(e) => UndoError::Failed(e),
        })?;
        let entry = self.record(&tx, &executed, Some(id))?;
        let undo_id = entry.id;
        tx.execute(
            "UPDATE tx_log SET undone_by = ?1 WHERE id = ?2",
            [undo_id.0, id.0],
        )
        .context("marking tx_log entry undone")?;
        tx.commit().context("committing undo")?;

        self.publish(
            &conn,
            executed.applied.events,
            [
                ServerEvent::TxLogEntryAdded { entry },
                ServerEvent::TxLogEntryUndone {
                    id,
                    undone_by: undo_id,
                },
            ],
        );
        Ok(undo_id)
    }

    fn record(
        &self,
        tx: &Transaction,
        executed: &Executed,
        undoes: Option<TxLogId>,
    ) -> Result<TxLogEntry> {
        let inverse = &executed.applied.inverse;
        ensure!(
            self.registry.ops.contains_key(inverse.kind),
            "{} op has an inverse of unregistered kind {}",
            executed.kind,
            inverse.kind
        );
        let ts = now_ms();
        let id = tx
            .query_row(
                "INSERT INTO tx_log
                   (ts, actor, description, op_type, op_json, inverse_type, inverse_json, undoes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 RETURNING id",
                params![
                    ts,
                    USER,
                    executed.description,
                    executed.kind,
                    executed.op_json,
                    inverse.kind,
                    inverse.json,
                    undoes.map(|u| u.0),
                ],
                |r| r.get(0),
            )
            .context("recording tx_log entry")?;
        Ok(TxLogEntry {
            id: TxLogId(id),
            ts,
            actor: USER.to_owned(),
            description: executed.description.clone(),
            op_type: executed.kind.to_owned(),
            undoes,
        })
    }

    /// Publishes the events of a committed transaction. Requiring the
    /// connection guard keeps the writer locked, so that events are
    /// published in commit order.
    fn publish(
        &self,
        _writer: &MutexGuard<'_, Connection>,
        events: Vec<ServerEvent>,
        log_events: impl IntoIterator<Item = ServerEvent>,
    ) {
        for event in events.into_iter().chain(log_events) {
            self.hub.publish(event);
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis()
        .try_into()
        .expect("timestamp fits in i64")
}

#[cfg(test)]
mod tests;
