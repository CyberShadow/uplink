//! Messages being sent (SPEC §6.7, §9.3): desired state, fulfilled once.
//!
//! Sending queues the message, wanted; the account's sync task claims it,
//! sends it with transaction id `uplink-<id>`, and records the event id
//! the homeserver gave it. It is kept once that event is synced. Matrix
//! messages have no send delay, so cancelling takes one back at once only
//! until the task claims it (in practice, while the homeserver is
//! unreachable); after, it makes the send unwanted, settled by the
//! outcome: not sent or refused takes it back. Sent, the task takes it
//! back as nearly as Matrix allows ([`MatrixContent::retractable`]):
//! redacts it, or reverses a pin. What can't be taken back, or what the
//! homeserver refused to, needs a decision, which keeping it sent makes.
//! Wanted again once taken back (the undo undone), it is sent anew.
//!
//! Since the homeserver deduplicates a transaction id, sending a message
//! again is safe; so a passing failure, or an interrupted send, queues it
//! again, and its outcome is never unknown. Taking it back is the same.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    compose::OutboxChanged,
    matrix::{
        MatrixContent, MatrixSend, MatrixSendId, MatrixSendState, MatrixSendsChanged, MemberAction,
        RoomId,
    },
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uplink_tx_log::{this_entry, Applied, Conflict, Inverse, Observation, Observed, Op};

/// The events of a change to the room's sends: its timeline's, and its
/// account's outbox's.
fn changed(tx: &Transaction, room: RoomId) -> Result<Vec<ServerEvent>> {
    let account = crate::rooms::room_account(tx, room)?.context("the room is gone")?;
    Ok(vec![
        ServerEvent::MatrixSendsChanged(MatrixSendsChanged { rooms: vec![room] }),
        ServerEvent::OutboxChanged(OutboxChanged {
            accounts: vec![account],
        }),
    ])
}

/// The transaction id a send is sent with.
pub fn transaction_id(item: MatrixSendId) -> String {
    format!("uplink-{}", item.0)
}

/// The transaction id a send is taken back with.
pub fn take_back_transaction_id(item: MatrixSendId) -> String {
    format!("uplink-{}-back", item.0)
}

/// What takes back `content`, sent as `event_id`: `None` if nothing
/// need be, as the pin it sent changed nothing.
fn take_back(content: &MatrixContent, event_id: Option<&str>) -> Option<MatrixContent> {
    assert!(
        content.retractable(),
        "only what is retractable is taken back"
    );
    match content {
        MatrixContent::Pin { event, pinned } => event_id.map(|_| MatrixContent::Pin {
            event: event.clone(),
            pinned: !pinned,
        }),
        _ => Some(MatrixContent::Redaction {
            event: event_id
                .expect("a message, file or reaction is sent as an event")
                .to_owned(),
        }),
    }
}

const COLUMNS: &str = "s.id, s.room, s.content, s.state, s.wanted, s.error, s.queued_at";

/// A send of [`COLUMNS`], from `offset` on.
fn send_row_at(r: &Row, offset: usize) -> uplink_storage::rusqlite::Result<MatrixSend> {
    let state: String = r.get(offset + 3)?;
    let content = content(&r.get::<_, String>(offset + 2)?);
    Ok(MatrixSend {
        id: MatrixSendId(r.get(offset)?),
        room: RoomId(r.get(offset + 1)?),
        retractable: content.retractable(),
        content,
        state: match state.as_str() {
            "queued" => MatrixSendState::Queued,
            "sending" => MatrixSendState::Sending,
            "sent" => MatrixSendState::Sent,
            "refused" => MatrixSendState::Refused,
            "taking_back" => MatrixSendState::TakingBack,
            _ => unreachable!("the schema allows no send state {state}"),
        },
        wanted: r.get(offset + 4)?,
        error: r.get(offset + 5)?,
        queued_at: r.get(offset + 6)?,
    })
}

/// A send's stored content.
fn content(json: &str) -> MatrixContent {
    serde_json::from_str(json).expect("sends store their content as written")
}

fn send_row(r: &Row) -> uplink_storage::rusqlite::Result<MatrixSend> {
    send_row_at(r, 0)
}

/// Of `matrix_send s`, those not yet reconciled: being sent, sent and
/// not yet synced back, or sent though cancelled and not yet taken back.
const UNRECONCILED: &str = "NOT ((s.wanted AND s.synced) OR s.state = 'taken_back')";

/// The room's sends not yet reconciled.
pub fn sends(tx: &Transaction, room: RoomId) -> Result<Vec<MatrixSend>> {
    Ok(tx
        .prepare(&format!(
            "SELECT {COLUMNS} FROM matrix_send s
             WHERE s.room = ?1 AND {UNRECONCILED} ORDER BY s.id"
        ))?
        .query_map([room.0], send_row)?
        .collect::<Result<_, _>>()?)
}

/// Every account's sends not yet reconciled, with their accounts.
pub(crate) fn unreconciled(tx: &Transaction) -> Result<Vec<(String, MatrixSend)>> {
    Ok(tx
        .prepare(&format!(
            "SELECT r.account_id, {COLUMNS} FROM matrix_send s JOIN matrix_room r ON r.id = s.room
             WHERE {UNRECONCILED} ORDER BY s.id"
        ))?
        .query_map([], |r| Ok((r.get(0)?, send_row_at(r, 1)?)))?
        .collect::<Result<_, _>>()?)
}

/// The send, unless it is gone or taken back.
fn send(tx: &Transaction, id: MatrixSendId) -> Result<Option<MatrixSend>> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM matrix_send s WHERE s.id = ?1 AND s.state <> 'taken_back'"
            ),
            [id.0],
            send_row,
        )
        .optional()?)
}

/// Marks the room's sent messages whose events are now synced; whether
/// there were any.
pub(crate) fn sends_synced(tx: &Transaction, room: RoomId) -> Result<bool> {
    Ok(tx.execute(
        "UPDATE matrix_send SET synced = 1
         WHERE room = ?1 AND state = 'sent' AND NOT synced
           AND event_id IN (SELECT event_id FROM matrix_event WHERE room = ?1)",
        [room.0],
    )? > 0)
}

/// Wanted again, a send refused taking back needs no decision.
fn set_wanted(tx: &Transaction, item: MatrixSendId, wanted: bool) -> Result<()> {
    tx.execute(
        "UPDATE matrix_send SET wanted = ?1, desired_by = ?2, error = NULL WHERE id = ?3",
        params![wanted, this_entry(tx)?.0, item.0],
    )?;
    Ok(())
}

/// A sent send no longer wanted that nothing need take back (a pin that
/// changed nothing) is taken back already.
fn settle_unwanted(tx: &Transaction, item: MatrixSendId) -> Result<()> {
    let (sent, event_id): (String, Option<String>) = tx.query_row(
        "SELECT content, event_id FROM matrix_send
         WHERE id = ?1 AND state = 'sent' AND NOT wanted",
        [item.0],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let sent = content(&sent);
    if sent.retractable() && take_back(&sent, event_id.as_deref()).is_none() {
        tx.execute(
            "UPDATE matrix_send SET state = 'taken_back' WHERE id = ?1",
            [item.0],
        )?;
    }
    Ok(())
}

/// How a message is named in descriptions.
fn quote(body: &str) -> String {
    const MAX: usize = 40;
    match body.char_indices().nth(MAX) {
        Some((end, _)) => format!("'{}…'", &body[..end]),
        None => format!("'{body}'"),
    }
}

/// How a send's content is named in descriptions.
fn named(content: &MatrixContent) -> String {
    match content {
        MatrixContent::Text {
            body,
            replaces: Some(_),
            ..
        } => format!("the edit {}", quote(body)),
        MatrixContent::Text { body, .. } => quote(body),
        MatrixContent::Reaction { key, .. } => format!("the reaction {key}"),
        MatrixContent::Redaction { .. } => "the deletion".to_owned(),
        MatrixContent::File { name, .. } => quote(name),
        MatrixContent::Name { name } => format!("the name {}", quote(name)),
        MatrixContent::Topic { topic } => format!("the topic {}", quote(topic)),
        MatrixContent::Pin { pinned: true, .. } => "a pin".to_owned(),
        MatrixContent::Pin { pinned: false, .. } => "an unpin".to_owned(),
        MatrixContent::Member { user, action, .. } => format!(
            "the {} of {user}",
            match action {
                MemberAction::Invite => "invitation",
                MemberAction::Kick => "removal",
                MemberAction::Ban => "ban",
                MemberAction::Unban => "unban",
            }
        ),
    }
}

/// What sending `content` to the room does, in words.
fn described(tx: &Transaction, room: RoomId, content: &MatrixContent) -> Result<String> {
    Ok(match content {
        MatrixContent::Text {
            body,
            replaces: Some(_),
            ..
        } => format!("Edited a message to {}", quote(body)),
        MatrixContent::Text {
            body,
            reply_to: Some(_),
            ..
        } => format!("Replied {}", quote(body)),
        MatrixContent::Text { body, .. } => format!("Sent {}", quote(body)),
        MatrixContent::Reaction { key, .. } => format!("Reacted with {key}"),
        MatrixContent::Redaction { event } => {
            let redacted: Option<(String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT type, body,
                            json_extract(raw_json, '$.content.\"m.relates_to\".key')
                     FROM matrix_event WHERE room = ?1 AND event_id = ?2",
                    params![room.0, event],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match redacted {
                Some((kind, _, Some(key))) if kind == "m.reaction" => {
                    format!("Took back the reaction {key}")
                }
                Some((_, Some(body), _)) => format!("Deleted {}", quote(&body)),
                _ => "Deleted an event".to_owned(),
            }
        }
        MatrixContent::File { name, .. } => format!("Sent {}", quote(name)),
        MatrixContent::Name { name } if name.is_empty() => "Took the room's name away".to_owned(),
        MatrixContent::Name { name } => format!("Named the room {}", quote(name)),
        MatrixContent::Topic { topic } if topic.is_empty() => {
            "Took the room's topic away".to_owned()
        }
        MatrixContent::Topic { topic } => format!("Set the topic to {}", quote(topic)),
        MatrixContent::Pin { pinned: true, .. } => "Pinned a message".to_owned(),
        MatrixContent::Pin { pinned: false, .. } => "Unpinned a message".to_owned(),
        MatrixContent::Member { user, action, .. } => format!(
            "{} {user}",
            match action {
                MemberAction::Invite => "Invited",
                MemberAction::Kick => "Removed",
                MemberAction::Ban => "Banned",
                MemberAction::Unban => "Unbanned",
            }
        ),
    })
}

/// Queues `content` to the room.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessage {
    pub room: RoomId,
    pub content: MatrixContent,
}

impl Op for SendMessage {
    const KIND: &'static str = "matrix.send";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let id = MatrixSendId(tx.query_row(
            "INSERT INTO matrix_send (room, content, queued_at, wanted, desired_by, state, synced)
             VALUES (?1, ?2, ?3, 1, ?4, 'queued', 0) RETURNING id",
            params![
                self.room.0,
                serde_json::to_string(&self.content)?,
                now_ms(),
                this_entry(tx)?.0
            ],
            |r| r.get(0),
        )?);
        Ok(Applied {
            description: described(tx, self.room, &self.content)?,
            inverse: Inverse::of(&CancelMessage { item: id }),
            events: changed(tx, self.room)?,
        })
    }
}

/// Takes a message being sent back: at once if the homeserver has not
/// got it, or else once it is known not to; if it went out, that needs a
/// decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelMessage {
    pub item: MatrixSendId,
}

impl Op for CancelMessage {
    const KIND: &'static str = "matrix.cancel_send";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let item = send(tx, self.item)?
            .ok_or_else(|| Conflict("the message was sent again since".into()))?;
        ensure!(
            item.wanted,
            Conflict("sending it is already being cancelled".into())
        );
        let events = changed(tx, item.room)?;
        match item.state {
            MatrixSendState::Queued | MatrixSendState::Refused => {
                tx.execute("DELETE FROM matrix_send WHERE id = ?1", [self.item.0])?;
                Ok(Applied {
                    description: format!("Cancelled sending {}", named(&item.content)),
                    inverse: Inverse::of(&SendMessage {
                        room: item.room,
                        content: item.content,
                    }),
                    events,
                })
            }
            MatrixSendState::Sending | MatrixSendState::Sent => {
                set_wanted(tx, self.item, false)?;
                let body = named(&item.content);
                let description = match (item.state, item.retractable) {
                    (MatrixSendState::Sending, _) => {
                        format!("Cancelled sending {body}, which may have gone out")
                    }
                    (_, true) => format!("Took back {body}"),
                    (_, false) => format!("Cancelled sending {body}, which went out"),
                };
                if item.state == MatrixSendState::Sent {
                    settle_unwanted(tx, self.item)?;
                }
                Ok(Applied {
                    description,
                    inverse: Inverse::of(&KeepSent { item: self.item }),
                    events,
                })
            }
            MatrixSendState::TakingBack => unreachable!("only what is unwanted is taken back"),
        }
    }
}

/// Wants a send cancelled when it could no longer be taken back for sure
/// again: resolves one that went out regardless. One being taken back,
/// or taken back, is sent anew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeepSent {
    pub item: MatrixSendId,
}

impl Op for KeepSent {
    const KIND: &'static str = "matrix.keep_sent";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let (room, content, state): (i64, String, String) = tx
            .query_row(
                "SELECT room, content, state FROM matrix_send WHERE id = ?1 AND NOT wanted",
                [self.item.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Conflict("sending it is no longer being cancelled".into()))?;
        let (room, content) = (RoomId(room), self::content(&content));
        match state.as_str() {
            "sending" | "sent" => {
                set_wanted(tx, self.item, true)?;
                Ok(Applied {
                    description: format!("Kept {} sent", named(&content)),
                    inverse: Inverse::of(&CancelMessage { item: self.item }),
                    events: changed(tx, room)?,
                })
            }
            "taking_back" | "taken_back" => {
                let sent = SendMessage { room, content }.apply(tx)?;
                Ok(Applied {
                    description: format!("{} again", sent.description),
                    ..sent
                })
            }
            _ => unreachable!("the schema has a send unwanted only once handed over"),
        }
    }
}

/// A message to send.
#[derive(Debug, Clone)]
pub struct Due {
    pub item: MatrixSendId,
    pub room: RoomId,
    pub room_id: String,
    pub content: MatrixContent,
}

/// The account's queued messages, in the order they were queued.
pub fn due(conn: &Connection, account: &str) -> Result<Vec<Due>> {
    Ok(conn
        .prepare(
            "SELECT s.id, r.room_id, s.content, s.room FROM matrix_send s JOIN matrix_room r ON r.id = s.room
             WHERE r.account_id = ?1 AND s.state = 'queued' ORDER BY s.id",
        )?
        .query_map([account], |r| {
            Ok(Due {
                item: MatrixSendId(r.get(0)?),
                room: RoomId(r.get(3)?),
                room_id: r.get(1)?,
                content: content(&r.get::<_, String>(2)?),
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Messages a previous run of the account's sync task claimed but did
/// not finish sending.
pub fn interrupted(conn: &Connection, account: &str) -> Result<Vec<MatrixSendId>> {
    Ok(conn
        .prepare(
            "SELECT s.id FROM matrix_send s JOIN matrix_room r ON r.id = s.room
             WHERE r.account_id = ?1 AND s.state = 'sending'",
        )?
        .query_map([account], |r| Ok(MatrixSendId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// The account's sends cancelled once sent, to take back, in the order
/// they were queued: each with what takes it back.
pub fn take_backs_due(conn: &Connection, account: &str) -> Result<Vec<Due>> {
    let rows: Vec<(i64, String, String, i64, Option<String>)> = conn
        .prepare(
            "SELECT s.id, r.room_id, s.content, s.room, s.event_id
             FROM matrix_send s JOIN matrix_room r ON r.id = s.room
             WHERE r.account_id = ?1 AND s.state = 'sent' AND NOT s.wanted AND s.error IS NULL
             ORDER BY s.id",
        )?
        .query_map([account], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(item, room_id, sent, room, event_id)| {
            let sent = content(&sent);
            sent.retractable().then(|| Due {
                item: MatrixSendId(item),
                room: RoomId(room),
                room_id,
                // Those that need nothing are taken back when cancelled.
                content: take_back(&sent, event_id.as_deref())
                    .expect("a send that needs nothing to take it back is taken back"),
            })
        })
        .collect())
}

/// Sends a previous run of the account's sync task was interrupted
/// taking back.
pub fn interrupted_take_backs(conn: &Connection, account: &str) -> Result<Vec<MatrixSendId>> {
    Ok(conn
        .prepare(
            "SELECT s.id FROM matrix_send s JOIN matrix_room r ON r.id = s.room
             WHERE r.account_id = ?1 AND s.state = 'taking_back'",
        )?
        .query_map([account], |r| Ok(MatrixSendId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// The send being sent, claimed by the sync task.
fn sending(tx: &Transaction, item: MatrixSendId) -> Result<MatrixSend> {
    let send = send(tx, item)?.context("the send is gone")?;
    ensure!(
        send.state == MatrixSendState::Sending,
        "message {} is not being sent",
        item.0
    );
    Ok(send)
}

/// The sync task is about to send a message: from now on, cancelling it
/// can no longer take it back at once. Refused with a [`Conflict`] if
/// the message is no longer queued.
#[derive(Debug, Serialize)]
pub struct SendStarted {
    pub item: MatrixSendId,
}

impl Observation for SendStarted {
    const KIND: &'static str = "matrix.send_started";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = send(tx, self.item)?
            .filter(|i| i.state == MatrixSendState::Queued)
            .ok_or_else(|| Conflict("no longer queued".into()))?;
        tx.execute(
            "UPDATE matrix_send SET state = 'sending' WHERE id = ?1",
            [self.item.0],
        )?;
        Ok(Observed {
            description: format!("Sending {}", named(&item.content)),
            events: changed(tx, item.room)?,
        })
    }
}

/// The homeserver accepted a message as event `event_id`; if sync
/// delivered the event already, the send is synced. Of a change to a
/// member, the homeserver gives no event id: it is synced at once.
#[derive(Debug, Serialize)]
pub struct SendSucceeded {
    pub item: MatrixSendId,
    pub event_id: Option<String>,
}

impl Observation for SendSucceeded {
    const KIND: &'static str = "matrix.send_succeeded";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = sending(tx, self.item)?;
        tx.execute(
            "UPDATE matrix_send SET state = 'sent', event_id = ?1, synced = ?1 IS NULL
             WHERE id = ?2",
            params![self.event_id, self.item.0],
        )?;
        sends_synced(tx, item.room)?;
        let body = named(&item.content);
        let description = match (item.wanted, item.retractable) {
            (true, _) => format!("Sent {body}"),
            (false, true) => format!("Sent {body} before the cancel; taking it back"),
            (false, false) => format!("Sent {body} before the cancel could take it back"),
        };
        if !item.wanted {
            settle_unwanted(tx, self.item)?;
        }
        Ok(Observed {
            description,
            events: changed(tx, item.room)?,
        })
    }
}

/// Sending a message did not succeed: `refused` with the homeserver's
/// reason, or else, for a passing reason or an interrupted send, it is
/// not sent. Either way, one no longer wanted is taken back; one wanted
/// is queued again, or refused.
#[derive(Debug, Serialize)]
pub struct SendFailed {
    pub item: MatrixSendId,
    pub refused: Option<String>,
}

impl Observation for SendFailed {
    const KIND: &'static str = "matrix.send_failed";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = sending(tx, self.item)?;
        let body = named(&item.content);
        let description = match (&self.refused, item.wanted) {
            (_, false) => {
                tx.execute("DELETE FROM matrix_send WHERE id = ?1", [self.item.0])?;
                format!("Did not send {body}, as cancelled")
            }
            (Some(reason), true) => {
                tx.execute(
                    "UPDATE matrix_send SET state = 'refused', error = ?1 WHERE id = ?2",
                    params![reason, self.item.0],
                )?;
                format!("Could not send {body}: {reason}")
            }
            (None, true) => {
                tx.execute(
                    "UPDATE matrix_send SET state = 'queued' WHERE id = ?1",
                    [self.item.0],
                )?;
                format!("Will retry sending {body}")
            }
        };
        Ok(Observed {
            description,
            events: changed(tx, item.room)?,
        })
    }
}

/// The send being taken back, claimed by the sync task.
fn taking_back(tx: &Transaction, item: MatrixSendId) -> Result<MatrixSend> {
    let send = send(tx, item)?.context("the send is gone")?;
    ensure!(
        send.state == MatrixSendState::TakingBack,
        "message {} is not being taken back",
        item.0
    );
    Ok(send)
}

/// The sync task is about to take back a message sent though cancelled.
/// Refused with a [`Conflict`] if it is no longer to be (kept sent
/// meanwhile).
#[derive(Debug, Serialize)]
pub struct TakeBackStarted {
    pub item: MatrixSendId,
}

impl Observation for TakeBackStarted {
    const KIND: &'static str = "matrix.take_back_started";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = send(tx, self.item)?
            .filter(|i| i.state == MatrixSendState::Sent && !i.wanted && i.error.is_none())
            .ok_or_else(|| Conflict("no longer to be taken back".into()))?;
        tx.execute(
            "UPDATE matrix_send SET state = 'taking_back' WHERE id = ?1",
            [self.item.0],
        )?;
        Ok(Observed {
            description: format!("Taking back {}", named(&item.content)),
            events: changed(tx, item.room)?,
        })
    }
}

/// The homeserver took back a message sent though cancelled.
#[derive(Debug, Serialize)]
pub struct TakeBackSucceeded {
    pub item: MatrixSendId,
}

impl Observation for TakeBackSucceeded {
    const KIND: &'static str = "matrix.take_back_succeeded";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = taking_back(tx, self.item)?;
        tx.execute(
            "UPDATE matrix_send SET state = 'taken_back' WHERE id = ?1",
            [self.item.0],
        )?;
        Ok(Observed {
            description: format!("Took back {}", named(&item.content)),
            events: changed(tx, item.room)?,
        })
    }
}

/// Taking back a message did not succeed: `refused` with the
/// homeserver's reason, which needs a decision; or else, for a passing
/// reason or an interrupted attempt, it is tried again.
#[derive(Debug, Serialize)]
pub struct TakeBackFailed {
    pub item: MatrixSendId,
    pub refused: Option<String>,
}

impl Observation for TakeBackFailed {
    const KIND: &'static str = "matrix.take_back_failed";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = taking_back(tx, self.item)?;
        tx.execute(
            "UPDATE matrix_send SET state = 'sent', error = ?1 WHERE id = ?2",
            params![self.refused, self.item.0],
        )?;
        let body = named(&item.content);
        Ok(Observed {
            description: match &self.refused {
                Some(reason) => format!("Could not take back {body}: {reason}"),
                None => format!("Will retry taking back {body}"),
            },
            events: changed(tx, item.room)?,
        })
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis()
        .try_into()
        .expect("timestamp fits in i64")
}
