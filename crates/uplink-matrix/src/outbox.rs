//! Messages being sent (SPEC §6.7, §9.3).
//!
//! Sending queues the message; the account's sync task claims it, sends
//! it with transaction id `uplink-<id>`, and records the event id the
//! homeserver gave it. The item is deleted once that event is synced.
//! Matrix messages have no send delay: undo can cancel one only until the
//! task claims it, i.e. while the homeserver is unreachable.
//!
//! Since the homeserver deduplicates a transaction id, sending an item
//! again is safe; so a passing failure, or an interrupted send, requeues
//! the item, and only a refusal fails it.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    compose::OutboxState,
    matrix::{MatrixOutboxChanged, MatrixOutboxId, MatrixOutboxItem, RoomId},
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Observation, Observed, Op};

pub fn changed(items: Vec<MatrixOutboxId>) -> ServerEvent {
    ServerEvent::MatrixOutboxChanged(MatrixOutboxChanged { items })
}

/// The transaction id an item is sent with.
pub fn transaction_id(item: MatrixOutboxId) -> String {
    format!("uplink-{}", item.0)
}

const COLUMNS: &str = "id, room, body, state, error, queued_at";

fn item_row(r: &Row) -> uplink_storage::rusqlite::Result<MatrixOutboxItem> {
    let state: String = r.get(3)?;
    Ok(MatrixOutboxItem {
        id: MatrixOutboxId(r.get(0)?),
        room: RoomId(r.get(1)?),
        body: r.get(2)?,
        state: match state.as_str() {
            "queued" => OutboxState::Queued,
            "sending" => OutboxState::Sending,
            "sent" => OutboxState::Sent,
            "failed" => OutboxState::Failed,
            _ => unreachable!("the schema allows no outbox state {state}"),
        },
        error: r.get(4)?,
        queued_at: r.get(5)?,
    })
}

pub fn items(tx: &Transaction) -> Result<Vec<MatrixOutboxItem>> {
    Ok(tx
        .prepare(&format!("SELECT {COLUMNS} FROM matrix_outbox ORDER BY id"))?
        .query_map([], item_row)?
        .collect::<Result<_, _>>()?)
}

fn item(tx: &Transaction, id: MatrixOutboxId) -> Result<Option<MatrixOutboxItem>> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM matrix_outbox WHERE id = ?1"),
            [id.0],
            item_row,
        )
        .optional()?)
}

fn set_state(tx: &Transaction, id: MatrixOutboxId, state: &str, error: Option<&str>) -> Result<()> {
    tx.execute(
        "UPDATE matrix_outbox SET state = ?1, error = ?2 WHERE id = ?3",
        params![state, error, id.0],
    )?;
    Ok(())
}

/// Deletes the room's sent items whose events are now synced; returns
/// them.
pub(crate) fn synced_back(tx: &Transaction, room: RoomId) -> Result<Vec<MatrixOutboxId>> {
    Ok(tx
        .prepare(
            "DELETE FROM matrix_outbox
             WHERE room = ?1 AND state = 'sent' AND event_id IN
               (SELECT event_id FROM matrix_event WHERE room = ?1)
             RETURNING id",
        )?
        .query_map([room.0], |r| Ok(MatrixOutboxId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// How a message is named in descriptions.
fn quote(body: &str) -> String {
    const MAX: usize = 40;
    match body.char_indices().nth(MAX) {
        Some((end, _)) => format!("'{}…'", &body[..end]),
        None => format!("'{body}'"),
    }
}

/// Queues a message to the room.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessage {
    pub room: RoomId,
    pub body: String,
}

impl Op for SendMessage {
    const KIND: &'static str = "matrix.send";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let id = MatrixOutboxId(tx.query_row(
            "INSERT INTO matrix_outbox (room, body, queued_at, state)
             VALUES (?1, ?2, ?3, 'queued') RETURNING id",
            params![self.room.0, self.body, now_ms()],
            |r| r.get(0),
        )?);
        Ok(Applied {
            description: format!("Sent {}", quote(&self.body)),
            inverse: Inverse::of(&CancelMessage { item: id }),
            events: vec![changed(vec![id])],
        })
    }
}

/// Takes a queued or failed message out of the outbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelMessage {
    pub item: MatrixOutboxId,
}

impl Op for CancelMessage {
    const KIND: &'static str = "matrix.cancel_send";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let item = item(tx, self.item)?.ok_or_else(|| Conflict("the message was sent".into()))?;
        match item.state {
            OutboxState::Queued | OutboxState::Failed => {}
            OutboxState::Sending => bail!(Conflict("the message is being sent".into())),
            OutboxState::Sent => bail!(Conflict("the message was sent".into())),
        }
        tx.execute("DELETE FROM matrix_outbox WHERE id = ?1", [self.item.0])?;
        Ok(Applied {
            description: format!("Cancelled sending {}", quote(&item.body)),
            inverse: Inverse::of(&SendMessage {
                room: item.room,
                body: item.body,
            }),
            events: vec![changed(vec![self.item])],
        })
    }
}

/// A message to send.
#[derive(Debug, Clone)]
pub struct Due {
    pub item: MatrixOutboxId,
    pub room_id: String,
    pub body: String,
}

/// The account's queued messages, in the order they were queued.
pub fn due(conn: &Connection, account: &str) -> Result<Vec<Due>> {
    Ok(conn
        .prepare(
            "SELECT o.id, r.room_id, o.body FROM matrix_outbox o JOIN matrix_room r ON r.id = o.room
             WHERE r.account_id = ?1 AND o.state = 'queued' ORDER BY o.id",
        )?
        .query_map([account], |r| {
            Ok(Due {
                item: MatrixOutboxId(r.get(0)?),
                room_id: r.get(1)?,
                body: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Messages a previous run of the account's sync task claimed but did
/// not finish sending.
pub fn interrupted(conn: &Connection, account: &str) -> Result<Vec<MatrixOutboxId>> {
    Ok(conn
        .prepare(
            "SELECT o.id FROM matrix_outbox o JOIN matrix_room r ON r.id = o.room
             WHERE r.account_id = ?1 AND o.state = 'sending'",
        )?
        .query_map([account], |r| Ok(MatrixOutboxId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// The sync task is about to send a message: from now on, it can no
/// longer be cancelled. Refused with a [`Conflict`] if the message is no
/// longer queued.
#[derive(Debug, Serialize)]
pub struct SendStarted {
    pub item: MatrixOutboxId,
}

impl Observation for SendStarted {
    const KIND: &'static str = "matrix.send_started";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = item(tx, self.item)?
            .filter(|i| i.state == OutboxState::Queued)
            .ok_or_else(|| Conflict("no longer queued".into()))?;
        set_state(tx, self.item, "sending", None)?;
        Ok(Observed {
            description: format!("Sending {}", quote(&item.body)),
            events: vec![changed(vec![self.item])],
        })
    }
}

/// The homeserver accepted a message as event `event_id`. If sync
/// delivered the event already, the item is done with.
#[derive(Debug, Serialize)]
pub struct SendSucceeded {
    pub item: MatrixOutboxId,
    pub event_id: String,
}

impl Observation for SendSucceeded {
    const KIND: &'static str = "matrix.send_succeeded";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = item(tx, self.item)?.context("the outbox item is gone")?;
        ensure!(
            item.state == OutboxState::Sending,
            "message {} is not being sent",
            self.item.0
        );
        tx.execute(
            "UPDATE matrix_outbox SET state = 'sent', event_id = ?1 WHERE id = ?2",
            params![self.event_id, self.item.0],
        )?;
        synced_back(tx, item.room)?;
        Ok(Observed {
            description: format!("Sent {}", quote(&item.body)),
            events: vec![changed(vec![self.item])],
        })
    }
}

/// Sending a message did not succeed: `refused` with the homeserver's
/// reason, or else, for a passing reason or an interrupted send, the
/// message is queued again.
#[derive(Debug, Serialize)]
pub struct SendFailed {
    pub item: MatrixOutboxId,
    pub refused: Option<String>,
}

impl Observation for SendFailed {
    const KIND: &'static str = "matrix.send_failed";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = item(tx, self.item)?.context("the outbox item is gone")?;
        ensure!(
            item.state == OutboxState::Sending,
            "message {} is not being sent",
            self.item.0
        );
        let description = match &self.refused {
            Some(reason) => {
                set_state(tx, self.item, "failed", Some(reason))?;
                format!("Could not send {}: {reason}", quote(&item.body))
            }
            None => {
                set_state(tx, self.item, "queued", None)?;
                format!("Will retry sending {}", quote(&item.body))
            }
        };
        Ok(Observed {
            description,
            events: vec![changed(vec![self.item])],
        })
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis()
        .try_into()
        .expect("timestamp fits in i64")
}
