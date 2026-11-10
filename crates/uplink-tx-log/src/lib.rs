//! The log: op dispatcher, observations, undo and replay (SPEC §6).
//!
//! Every change to uplink's state is recorded in `tx_log`, in commit order:
//! user-initiated [`Op`]s run through [`Dispatcher::dispatch`], and
//! [`Observation`]s of external state through [`Dispatcher::observe`].
//! Each is applied and recorded in one SQLite transaction, and the entry is
//! published to the hub only after commit. Ops record their inverse, so
//! they can be undone; undo applies the inverse as a new op, so the log is
//! append-only and undoing an undo is a redo. The log doubles as the event
//! stream: clients resume from an entry id ([`LogReader::replay`]).

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use uplink_protocol::{HistoryEntry, LogEntry, ServerEvent, Source, TxLogId};
use uplink_realtime::{Hub, Replay};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

pub mod answer;
#[cfg(any(test, feature = "test-ops"))]
pub mod sample;

/// A user-initiated change to uplink's state.
pub trait Op: Serialize + DeserializeOwned + Send + 'static {
    /// Discriminator recorded in `tx_log`, e.g. `"mail.archive"`. Must be
    /// unique within a [`Registry`].
    const KIND: &'static str;

    /// Applies the op within `tx`, computing the inverse edit from what it
    /// changed. Returning an error rolls the transaction back; return a
    /// [`Conflict`] when the state the op expects is not what it finds.
    fn apply(self, tx: &Transaction) -> Result<Applied>;
}

/// Outcome of [`Op::apply`].
pub struct Applied {
    /// What the op did, for the log UI (e.g. "Archived 3 messages").
    pub description: String,
    /// The op that reverts this one.
    pub inverse: Inverse,
    /// What changed, for clients.
    pub events: Vec<ServerEvent>,
}

/// A change to external state, observed by syncing an account.
pub trait Observation: Serialize + Send + 'static {
    /// Discriminator recorded in `tx_log`, e.g. `"mail.received"`.
    const KIND: &'static str;

    /// Records the state observed in `account` within `tx`.
    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed>;
}

/// Outcome of [`Observation::apply`].
pub struct Observed {
    /// What was observed, for the log UI (e.g. "Received 3 messages").
    pub description: String,
    /// What changed, for clients.
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
            json: to_json(O::KIND, op),
        }
    }
}

fn to_json(kind: &str, value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| panic!("{kind} does not serialize: {e}"))
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

/// An applied op or observation, ready to be recorded.
struct Executed {
    kind: &'static str,
    op_json: String,
    description: String,
    /// `Some` exactly for ops.
    inverse: Option<Inverse>,
    events: Vec<ServerEvent>,
}

fn execute<O: Op>(tx: &Transaction, op: O) -> Result<Executed> {
    let op_json = to_json(O::KIND, &op);
    let applied = op.apply(tx)?;
    Ok(Executed {
        kind: O::KIND,
        op_json,
        description: applied.description,
        inverse: Some(applied.inverse),
        events: applied.events,
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
    #[error("tx_log entry {} is an observation, not a user op", .0 .0)]
    NotUndoable(TxLogId),
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
        self.on_writer(move |inner| {
            inner
                .write(Source::User, |tx| Ok((execute(tx, op)?, None)))
                .map(|entry| entry.id)
        })
        .await
    }

    /// Applies and records a change to external state observed by syncing
    /// `account`.
    pub async fn observe<O: Observation>(
        &self,
        account: String,
        observation: O,
    ) -> Result<TxLogId> {
        self.on_writer(move |inner| {
            inner
                .write(
                    Source::Account {
                        id: account.clone(),
                    },
                    |tx| {
                        let op_json = to_json(O::KIND, &observation);
                        let observed = observation.apply(&account, tx)?;
                        let executed = Executed {
                            kind: O::KIND,
                            op_json,
                            description: observed.description,
                            inverse: None,
                            events: observed.events,
                        };
                        Ok((executed, None))
                    },
                )
                .map(|entry| entry.id)
        })
        .await
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

    /// Runs `execute` and records its result in one transaction, then
    /// publishes the entry. `execute` also says which entry, if any, the
    /// new one undoes.
    fn write<E>(
        &self,
        source: Source,
        execute: impl FnOnce(&Transaction) -> Result<(Executed, Option<TxLogId>), E>,
    ) -> Result<LogEntry, E>
    where
        E: From<anyhow::Error>,
    {
        let mut conn = self.lock();
        let tx = conn.transaction().context("beginning transaction")?;
        let expected = this_entry(&tx)?;
        let (executed, undoes) = execute(&tx)?;
        let entry = self.record(&tx, executed, source, undoes)?;
        assert_eq!(
            entry.id, expected,
            "entries are recorded as `this_entry` says"
        );
        if let Some(undone) = undoes {
            tx.execute(
                "UPDATE tx_log SET undone_by = ?1 WHERE id = ?2",
                [entry.id.0, undone.0],
            )
            .context("marking tx_log entry undone")?;
        }
        tx.commit().context("committing")?;
        // Publish while still holding the writer, so that entries reach the
        // hub in commit order.
        self.hub.publish(entry.clone());
        Ok(entry)
    }

    fn undo(&self, id: TxLogId) -> Result<TxLogId, UndoError> {
        let entry = self.write(Source::User, |tx| {
            let (account, inverse, undone_by) = tx
                .query_row(
                    "SELECT account_id, inverse_type, inverse_json, undone_by
                     FROM tx_log WHERE id = ?1",
                    [id.0],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            Option::zip(
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                            ),
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .optional()
                .context("reading tx_log entry")?
                .ok_or(UndoError::NotFound(id))?;
            if account.is_some() {
                return Err(UndoError::NotUndoable(id));
            }
            if let Some(by) = undone_by {
                return Err(UndoError::AlreadyUndone {
                    id,
                    undone_by: TxLogId(by),
                });
            }
            let (inverse_type, inverse_json) =
                inverse.expect("the schema requires an inverse for user ops");
            let execute = self
                .registry
                .ops
                .get(inverse_type.as_str())
                .with_context(|| format!("inverse op kind {inverse_type} is not registered"))?;
            let executed =
                execute(tx, &inverse_json).map_err(|e| match e.downcast::<Conflict>() {
                    Ok(conflict) => UndoError::Conflict { id, conflict },
                    Err(e) => UndoError::Failed(e),
                })?;
            Ok((executed, Some(id)))
        })?;
        Ok(entry.id)
    }

    fn record(
        &self,
        tx: &Transaction,
        executed: Executed,
        source: Source,
        undoes: Option<TxLogId>,
    ) -> Result<LogEntry> {
        if let Some(inverse) = &executed.inverse {
            anyhow::ensure!(
                self.registry.ops.contains_key(inverse.kind),
                "{} op has an inverse of unregistered kind {}",
                executed.kind,
                inverse.kind
            );
        }
        let account = match &source {
            Source::User => None,
            Source::Account { id } => Some(id.as_str()),
        };
        let ts = now_ms();
        let events_json = serde_json::to_string(&executed.events).expect("events serialize");
        let id = tx
            .query_row(
                "INSERT INTO tx_log
                   (ts, account_id, description, op_type, op_json,
                    inverse_type, inverse_json, events_json, undoes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 RETURNING id",
                params![
                    ts,
                    account,
                    executed.description,
                    executed.kind,
                    executed.op_json,
                    executed.inverse.as_ref().map(|i| i.kind),
                    executed.inverse.as_ref().map(|i| &i.json),
                    events_json,
                    undoes.map(|u| u.0),
                ],
                |r| r.get(0),
            )
            .context("recording tx_log entry")?;
        Ok(LogEntry {
            id: TxLogId(id),
            ts,
            source,
            description: executed.description,
            op_type: executed.kind.to_owned(),
            undoes,
            events: executed.events,
        })
    }
}

/// The entry that the op or observation being applied in `tx` will be
/// recorded as, for state that names the op that set it (SPEC §6.6). Ids
/// are never reused (`AUTOINCREMENT`), and the dispatcher is the only
/// writer, so it is the next one.
pub fn this_entry(tx: &Transaction) -> Result<TxLogId> {
    let last: i64 = tx.query_row(
        "SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'tx_log'), 0)",
        [],
        |r| r.get(0),
    )?;
    Ok(TxLogId(last + 1))
}

/// The last entry recorded, as seen by `tx`: the position a query run in
/// `tx` reflects.
pub fn as_of(tx: &Transaction) -> Result<TxLogId> {
    Ok(TxLogId(tx.query_row(
        "SELECT coalesce(max(id), 0) FROM tx_log",
        [],
        |r| r.get(0),
    )?))
}

/// Reads the log for clients catching up (SPEC §5.3).
pub struct LogReader {
    conn: Mutex<Connection>,
    max_replay: usize,
}

impl LogReader {
    /// `conn` is a connection to the database written by the
    /// [`Dispatcher`]. A client further than `max_replay` entries behind
    /// must reload instead of being replayed.
    pub fn new(conn: Connection, max_replay: usize) -> Self {
        Self {
            conn: Mutex::new(conn),
            max_replay,
        }
    }

    pub async fn replay(self: &Arc<Self>, since: Option<TxLogId>) -> Result<Replay> {
        let reader = self.clone();
        match tokio::task::spawn_blocking(move || reader.replay_blocking(since)).await {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }

    fn replay_blocking(&self, since: Option<TxLogId>) -> Result<Replay> {
        let mut conn = self.conn.lock().expect("database connection poisoned");
        // One read transaction, so head and entries are one snapshot.
        let tx = conn.transaction()?;
        let head =
            TxLogId(tx.query_row("SELECT coalesce(max(id), 0) FROM tx_log", [], |r| r.get(0))?);
        let entries = match since {
            Some(since) if since <= head => {
                let entries: Vec<LogEntry> = tx
                    .prepare(
                        "SELECT id, ts, account_id, description, op_type, undoes, events_json
                         FROM tx_log WHERE id > ?1 ORDER BY id LIMIT ?2",
                    )?
                    .query_map(params![since.0, self.max_replay as i64 + 1], log_entry)?
                    .collect::<Result<_, _>>()?;
                (entries.len() <= self.max_replay).then_some(entries)
            }
            // Never connected, or a position this log never had.
            _ => None,
        };
        Ok(Replay { head, entries })
    }
}

impl LogReader {
    /// Up to `limit` of the user's ops before `before`, newest first, and
    /// whether there are more; as of the head it returns.
    pub async fn history(
        self: &Arc<Self>,
        before: Option<TxLogId>,
        limit: u32,
    ) -> Result<(TxLogId, Vec<HistoryEntry>, bool)> {
        let reader = self.clone();
        match tokio::task::spawn_blocking(move || reader.history_blocking(before, limit)).await {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }

    fn history_blocking(
        &self,
        before: Option<TxLogId>,
        limit: u32,
    ) -> Result<(TxLogId, Vec<HistoryEntry>, bool)> {
        let mut conn = self.conn.lock().expect("database connection poisoned");
        let tx = conn.transaction()?;
        let head = as_of(&tx)?;
        let mut entries: Vec<HistoryEntry> = tx
            .prepare(
                "SELECT id, ts, account_id, description, op_type, undoes, events_json, undone_by
                 FROM tx_log WHERE account_id IS NULL AND id < ?1 ORDER BY id DESC LIMIT ?2",
            )?
            .query_map(
                params![before.map_or(i64::MAX, |b| b.0), i64::from(limit) + 1],
                |r| {
                    Ok(HistoryEntry {
                        entry: log_entry(r)?,
                        undone_by: r.get::<_, Option<i64>>(7)?.map(TxLogId),
                    })
                },
            )?
            .collect::<Result<_, _>>()?;
        let more = entries.len() > limit as usize;
        entries.truncate(limit as usize);
        Ok((head, entries, more))
    }
}

fn log_entry(r: &Row) -> uplink_storage::rusqlite::Result<LogEntry> {
    let events_json: String = r.get(6)?;
    Ok(LogEntry {
        id: TxLogId(r.get(0)?),
        ts: r.get(1)?,
        source: match r.get::<_, Option<String>>(2)? {
            None => Source::User,
            Some(id) => Source::Account { id },
        },
        description: r.get(3)?,
        op_type: r.get(4)?,
        undoes: r.get::<_, Option<i64>>(5)?.map(TxLogId),
        events: serde_json::from_str(&events_json).expect("recorded events deserialize"),
    })
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
