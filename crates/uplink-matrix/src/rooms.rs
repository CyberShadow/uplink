//! Rooms and their timelines as synced, and the read marker (SPEC §9.2,
//! §9.5).
//!
//! The read marker is desired state (§6.6): marking a room read moves it
//! locally, and the account's sync task sends a read receipt for it.
//! Receipts only move forward, so undoing a mark-read moves the marker
//! back in uplink only. A receipt of the account's own from another device
//! moves the marker forward.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    compose::OutboxChanged,
    matrix::{
        Encryption, MatrixSendsChanged, Membership, Room, RoomEvent, RoomEventId, RoomId,
        RoomsChanged, TimelineAdded, TimelinePage,
    },
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uplink_tx_log::{Applied, Inverse, Observation, Observed, Op};

use crate::{
    membership::{parse_desired, parse_membership},
    send,
};

pub const ACCOUNT_KIND: &str = "matrix";

// ── sync ──────────────────────────────────────────────────────────────────

/// `account.sync_state` of a Matrix account.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncState {
    /// The `since` token to sync from next.
    pub next_batch: String,
}

/// What a sync response changed. Sync responses that change nothing uplink
/// stores are not recorded, so the token in `account.sync_state` may lag:
/// syncing again from it delivers nothing new.
#[derive(Debug, Serialize)]
pub struct Synced {
    pub next_batch: String,
    pub rooms: Vec<ObservedRoom>,
}

#[derive(Debug, Serialize)]
pub struct ObservedRoom {
    pub room_id: String,
    pub name: String,
    pub encrypted: bool,
    pub membership: Membership,
    /// Whether the room is a space (of type `m.space`).
    pub space: bool,
    /// `mxc://` URI: the room's avatar, or of a direct chat without one,
    /// the other user's.
    pub avatar: Option<String>,
    /// Of a space, the rooms it lists.
    pub children: Vec<SpaceChild>,
    /// Timeline events, in order. Not in the log: they are stored.
    #[serde(skip)]
    pub events: Vec<ObservedEvent>,
    /// Events the account's own read receipts in the response are for.
    pub own_receipts: Vec<String>,
}

/// A room a space lists, by an `m.space.child` event with `via`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpaceChild {
    pub room_id: String,
    /// The event's `order`, if it is valid: at most 50 characters, from
    /// `\x20` to `\x7E`.
    pub order: Option<String>,
}

impl SpaceChild {
    /// Reads a space's `m.space.child` event; `None` if it lists no room
    /// (it has no `via`, as one that removes a child).
    pub fn parse(raw: &str) -> Result<Option<Self>> {
        let event: serde_json::Value = serde_json::from_str(raw).context("event JSON")?;
        let room_id = event["state_key"]
            .as_str()
            .with_context(|| format!("m.space.child without a state key: {raw}"))?;
        let content = &event["content"];
        let listed = content["via"].as_array().is_some_and(|via| !via.is_empty());
        let order = content["order"]
            .as_str()
            .filter(|o| o.len() <= 50 && o.bytes().all(|b| (0x20..=0x7e).contains(&b)));
        Ok(listed.then(|| Self {
            room_id: room_id.to_owned(),
            order: order.map(str::to_owned),
        }))
    }
}

#[derive(Debug, Clone)]
pub struct ObservedEvent {
    pub event_id: String,
    pub sender: String,
    pub sender_name: Option<String>,
    pub ts: i64,
    pub kind: String,
    pub msgtype: Option<String>,
    pub body: Option<String>,
    pub raw: String,
    pub encryption: Encryption,
}

impl ObservedEvent {
    /// Reads what uplink stores of an event from its JSON (decrypted, if it
    /// was encrypted and could be). Fails only if the JSON is not a room
    /// event at all; any content is fine.
    pub fn parse(raw: &str, sender_name: Option<String>, encryption: Encryption) -> Result<Self> {
        let event: serde_json::Value = serde_json::from_str(raw).context("event JSON")?;
        let field = |name: &str| {
            event[name]
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("event has no {name}: {raw}"))
        };
        let kind = field("type")?;
        let content = &event["content"];
        let message = kind == "m.room.message";
        let text = |name: &str| {
            message
                .then(|| content[name].as_str().map(str::to_owned))
                .flatten()
        };
        Ok(Self {
            event_id: field("event_id")?,
            sender: field("sender")?,
            sender_name,
            ts: event["origin_server_ts"]
                .as_i64()
                .with_context(|| format!("event has no origin_server_ts: {raw}"))?,
            msgtype: text("msgtype"),
            body: text("body"),
            kind,
            raw: raw.to_owned(),
            encryption,
        })
    }
}

fn membership_str(membership: Membership) -> &'static str {
    match membership {
        Membership::Joined => "joined",
        Membership::Invited => "invited",
        Membership::Left => "left",
    }
}

fn encryption_str(encryption: Encryption) -> &'static str {
    match encryption {
        Encryption::None => "none",
        Encryption::Decrypted => "decrypted",
        Encryption::Undecryptable => "undecryptable",
    }
}

impl Observation for Synced {
    const KIND: &'static str = "matrix.synced";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        tx.execute(
            "INSERT INTO account (id, kind) VALUES (?1, ?2) ON CONFLICT (id) DO NOTHING",
            [account, ACCOUNT_KIND],
        )?;
        let kind: String =
            tx.query_row("SELECT kind FROM account WHERE id = ?1", [account], |r| {
                r.get(0)
            })?;
        ensure!(
            kind == ACCOUNT_KIND,
            "account {account} is a {kind} account"
        );

        let mut events = vec![];
        let mut changed_rooms = vec![];
        let mut sent = vec![];
        let mut added_total = 0;
        for room in self.rooms {
            let (id, upserted) = upsert_room(tx, account, &room)?;
            let changed = set_children(tx, id, &room.children)? || upserted;
            let added = insert_events(tx, id, &room.events)?;
            if send::sends_synced(tx, id)? {
                sent.push(id);
            }
            let read = observe_receipts(tx, id, &room.own_receipts)?;
            if changed || read || !added.is_empty() {
                changed_rooms.push(id);
            }
            if !added.is_empty() {
                added_total += added.len();
                events.push(ServerEvent::TimelineAdded(TimelineAdded {
                    room: id,
                    events: added,
                }));
            }
        }
        if !changed_rooms.is_empty() {
            events.push(ServerEvent::RoomsChanged(RoomsChanged {
                rooms: changed_rooms,
            }));
        }
        if !sent.is_empty() {
            events.push(ServerEvent::MatrixSendsChanged(MatrixSendsChanged {
                rooms: sent,
            }));
            events.push(ServerEvent::OutboxChanged(OutboxChanged {
                accounts: vec![account.to_owned()],
            }));
        }
        let state = serde_json::to_string(&SyncState {
            next_batch: self.next_batch,
        })?;
        tx.execute(
            "UPDATE account SET sync_state = ?1 WHERE id = ?2",
            params![state, account],
        )?;
        Ok(Observed {
            description: match added_total {
                0 => "Synced rooms".to_owned(),
                1 => "Received 1 event".to_owned(),
                n => format!("Received {n} events"),
            },
            events,
        })
    }
}

/// The room's id, and whether it is new or its name, encryption,
/// membership, type or avatar changed.
fn upsert_room(tx: &Transaction, account: &str, room: &ObservedRoom) -> Result<(RoomId, bool)> {
    let membership = membership_str(room.membership);
    type Stored = (i64, String, bool, String, bool, Option<String>);
    let existing: Option<Stored> = tx
        .query_row(
            "SELECT id, name, encrypted, membership, space, avatar FROM matrix_room
             WHERE account_id = ?1 AND room_id = ?2",
            [account, &room.room_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    Ok(match existing {
        Some((id, name, encrypted, old_membership, space, avatar)) => {
            let changed = name != room.name
                || encrypted != room.encrypted
                || old_membership != membership
                || space != room.space
                || avatar != room.avatar;
            if changed {
                // A membership asked for is had, however it came about.
                tx.execute(
                    "UPDATE matrix_room SET name = ?1, encrypted = ?2, membership = ?3,
                       space = ?4, avatar = ?5,
                       desired = CASE WHEN desired = ?3 THEN NULL ELSE desired END
                     WHERE id = ?6",
                    params![
                        room.name,
                        room.encrypted,
                        membership,
                        room.space,
                        room.avatar,
                        id
                    ],
                )?;
            }
            (RoomId(id), changed)
        }
        None => {
            let id = tx.query_row(
                "INSERT INTO matrix_room
                   (account_id, room_id, name, encrypted, membership, space, avatar)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
                params![
                    account,
                    room.room_id,
                    room.name,
                    room.encrypted,
                    membership,
                    room.space,
                    room.avatar
                ],
                |r| r.get(0),
            )?;
            (RoomId(id), true)
        }
    })
}

/// Records the rooms the space lists; returns whether they changed.
fn set_children(tx: &Transaction, space: RoomId, children: &[SpaceChild]) -> Result<bool> {
    let stored: Vec<(String, Option<String>)> = tx
        .prepare(
            "SELECT child, child_order FROM matrix_space_child WHERE space = ?1 ORDER BY child",
        )?
        .query_map([space.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut observed: Vec<(String, Option<String>)> = children
        .iter()
        .map(|c| (c.room_id.clone(), c.order.clone()))
        .collect();
    observed.sort();
    if stored == observed {
        return Ok(false);
    }
    tx.execute("DELETE FROM matrix_space_child WHERE space = ?1", [space.0])?;
    let mut insert = tx.prepare(
        "INSERT INTO matrix_space_child (space, child, child_order) VALUES (?1, ?2, ?3)",
    )?;
    for (child, order) in observed {
        insert.execute(params![space.0, child, order])?;
    }
    Ok(true)
}

/// Stores the events not stored yet; returns their ids.
fn insert_events(
    tx: &Transaction,
    room: RoomId,
    events: &[ObservedEvent],
) -> Result<Vec<RoomEventId>> {
    let mut insert = tx.prepare(
        "INSERT INTO matrix_event
           (room, event_id, sender, sender_name, ts, type, msgtype, body, raw_json, encryption)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (room, event_id) DO NOTHING
         RETURNING id",
    )?;
    let mut added = vec![];
    for e in events {
        let id = insert
            .query_row(
                params![
                    room.0,
                    e.event_id,
                    e.sender,
                    e.sender_name,
                    e.ts,
                    e.kind,
                    e.msgtype,
                    e.body,
                    e.raw,
                    encryption_str(e.encryption),
                ],
                |r| r.get(0),
            )
            .optional()?;
        added.extend(id.map(RoomEventId));
    }
    Ok(added)
}

/// Moves the read marker forward to the latest of the events the
/// account's own receipts are for, if that is past the latest receipt
/// already sent or observed; returns whether it moved.
fn observe_receipts(tx: &Transaction, room: RoomId, receipts: &[String]) -> Result<bool> {
    let mut latest: Option<i64> = None;
    for event_id in receipts {
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM matrix_event WHERE room = ?1 AND event_id = ?2",
                params![room.0, event_id],
                |r| r.get(0),
            )
            .optional()?;
        latest = latest.max(id);
    }
    let Some(latest) = latest else {
        return Ok(false);
    };
    Ok(tx.execute(
        "UPDATE matrix_room
         SET read_pushed = ?1, read_up_to = max(coalesce(read_up_to, 0), ?1)
         WHERE id = ?2 AND coalesce(read_pushed, 0) < ?1",
        params![latest, room.0],
    )? > 0)
}

// ── the read marker ───────────────────────────────────────────────────────

/// Sets the room's read marker; `None` is before the first event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkRead {
    pub room: RoomId,
    pub up_to: Option<RoomEventId>,
}

impl Op for MarkRead {
    const KIND: &'static str = "matrix.mark_read";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let (name, previous): (String, Option<i64>) = tx
            .query_row(
                "SELECT name, read_up_to FROM matrix_room WHERE id = ?1",
                [self.room.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .with_context(|| format!("no room {}", self.room.0))?;
        tx.execute(
            "UPDATE matrix_room SET read_up_to = ?1 WHERE id = ?2",
            params![self.up_to.map(|e| e.0), self.room.0],
        )?;
        Ok(Applied {
            description: format!("Read {name}"),
            inverse: Inverse::of(&MarkRead {
                room: self.room,
                up_to: previous.map(RoomEventId),
            }),
            events: vec![ServerEvent::RoomsChanged(RoomsChanged {
                rooms: vec![self.room],
            })],
        })
    }
}

/// A read receipt was sent for the room's event `up_to`.
#[derive(Debug, Serialize)]
pub struct ReceiptSent {
    pub room: RoomId,
    pub up_to: RoomEventId,
}

impl Observation for ReceiptSent {
    const KIND: &'static str = "matrix.receipt_sent";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        tx.execute(
            "UPDATE matrix_room SET read_pushed = max(coalesce(read_pushed, 0), ?1) WHERE id = ?2",
            params![self.up_to.0, self.room.0],
        )?;
        // Nothing clients show changed.
        Ok(Observed {
            description: "Sent a read receipt".to_owned(),
            events: vec![],
        })
    }
}

/// A read receipt to send: the room's read marker is past the latest
/// receipt.
#[derive(Debug, Clone)]
pub struct UnsentReceipt {
    pub room: RoomId,
    pub room_id: String,
    pub up_to: RoomEventId,
    pub event_id: String,
}

pub fn unsent_receipts(conn: &Connection, account: &str) -> Result<Vec<UnsentReceipt>> {
    Ok(conn
        .prepare(
            "SELECT r.id, r.room_id, e.id, e.event_id
             FROM matrix_room r JOIN matrix_event e ON e.id = r.read_up_to
             WHERE r.account_id = ?1 AND r.membership = 'joined'
               AND r.read_up_to > coalesce(r.read_pushed, 0)",
        )?
        .query_map([account], |r| {
            Ok(UnsentReceipt {
                room: RoomId(r.get(0)?),
                room_id: r.get(1)?,
                up_to: RoomEventId(r.get(2)?),
                event_id: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

// ── queries ───────────────────────────────────────────────────────────────

pub fn sync_state(conn: &Connection, account: &str) -> Result<Option<SyncState>> {
    let state: Option<Option<String>> = conn
        .query_row(
            "SELECT sync_state FROM account WHERE id = ?1",
            [account],
            |r| r.get(0),
        )
        .optional()?;
    state
        .flatten()
        .map(|s| serde_json::from_str(&s).context("decoding the Matrix sync state"))
        .transpose()
}

/// The rooms of `account`, but those left (unless joining them again),
/// most recently active first.
/// `mxid` is the account's user: its own messages are never unread.
/// Undecryptable events count as unread messages, which they nearly
/// always are.
pub fn rooms(tx: &Transaction, account: &str, mxid: &str) -> Result<Vec<Room>> {
    Ok(tx
        .prepare(
            "SELECT r.id, r.account_id, r.room_id, r.name, r.encrypted, r.membership,
                    (SELECT count(*) FROM matrix_event e
                     WHERE e.room = r.id AND e.id > coalesce(r.read_up_to, 0)
                       AND e.type IN ('m.room.message', 'm.room.encrypted')
                       AND e.sender != ?2),
                    (SELECT max(ts) FROM matrix_event e WHERE e.room = r.id),
                    r.read_up_to, r.desired, r.membership_error, r.space, r.avatar,
                    -- In the space's order: by `order`, those without last.
                    (SELECT json_group_array(child) FROM
                       (SELECT child FROM matrix_space_child c WHERE c.space = r.id
                        ORDER BY child_order IS NULL, child_order, child))
             FROM matrix_room r
             WHERE r.account_id = ?1 AND (r.membership != 'left' OR r.desired = 'joined')
             ORDER BY 8 DESC NULLS LAST, r.id",
        )?
        .query_map([account, mxid], |r| {
            Ok(Room {
                id: RoomId(r.get(0)?),
                account: r.get(1)?,
                room_id: r.get(2)?,
                name: r.get(3)?,
                encrypted: r.get(4)?,
                membership: parse_membership(&r.get::<_, String>(5)?),
                unread: r.get(6)?,
                last_event_ts: r.get(7)?,
                read_up_to: r.get::<_, Option<i64>>(8)?.map(RoomEventId),
                pending: parse_desired(r.get::<_, Option<String>>(9)?.as_deref()),
                membership_error: r.get(10)?,
                space: r.get(11)?,
                avatar: r.get(12)?,
                children: serde_json::from_str(&r.get::<_, String>(13)?)
                    .expect("json_group_array makes an array of strings"),
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// The account a room is of and its avatar, if the room exists.
pub fn room_avatar(conn: &Connection, room: RoomId) -> Result<Option<(String, Option<String>)>> {
    Ok(conn
        .query_row(
            "SELECT account_id, avatar FROM matrix_room WHERE id = ?1",
            [room.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The account a room is of, if the room exists.
pub fn room_account(conn: &Connection, room: RoomId) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT account_id FROM matrix_room WHERE id = ?1",
            [room.0],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether `event` is of `room`.
pub fn event_in_room(conn: &Connection, room: RoomId, event: RoomEventId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM matrix_event WHERE id = ?1 AND room = ?2",
            params![event.0, room.0],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn room_event(r: &Row) -> uplink_storage::rusqlite::Result<RoomEvent> {
    let encryption: String = r.get(9)?;
    Ok(RoomEvent {
        id: RoomEventId(r.get(0)?),
        event_id: r.get(1)?,
        sender: r.get(2)?,
        sender_name: r.get(3)?,
        ts: r.get(4)?,
        kind: r.get(5)?,
        msgtype: r.get(6)?,
        body: r.get(7)?,
        raw: r.get(8)?,
        encryption: match encryption.as_str() {
            "none" => Encryption::None,
            "decrypted" => Encryption::Decrypted,
            "undecryptable" => Encryption::Undecryptable,
            _ => unreachable!("the schema allows no encryption {encryption}"),
        },
    })
}

/// Up to `limit` events of the room's timeline, oldest first, and whether
/// there are earlier ones.
pub fn timeline(
    tx: &Transaction,
    room: RoomId,
    page: TimelinePage,
    limit: u32,
) -> Result<(Vec<RoomEvent>, bool)> {
    const COLUMNS: &str =
        "id, event_id, sender, sender_name, ts, type, msgtype, body, raw_json, encryption";
    let (mut events, newest_first) = match page {
        TimelinePage::Latest => (
            query_events(
                tx,
                &format!(
                    "SELECT {COLUMNS} FROM matrix_event WHERE room = ?1 ORDER BY id DESC LIMIT ?2"
                ),
                params![room.0, limit],
            )?,
            true,
        ),
        TimelinePage::Before { event } => (
            query_events(
                tx,
                &format!(
                    "SELECT {COLUMNS} FROM matrix_event WHERE room = ?1 AND id < ?2
                     ORDER BY id DESC LIMIT ?3"
                ),
                params![room.0, event.0, limit],
            )?,
            true,
        ),
        TimelinePage::After { event } => (
            query_events(
                tx,
                &format!(
                    "SELECT {COLUMNS} FROM matrix_event WHERE room = ?1 AND id > ?2
                     ORDER BY id LIMIT ?3"
                ),
                params![room.0, event.0, limit],
            )?,
            false,
        ),
    };
    if newest_first {
        events.reverse();
    }
    let more_before = match events.first() {
        Some(first) => tx
            .query_row(
                "SELECT 1 FROM matrix_event WHERE room = ?1 AND id < ?2",
                params![room.0, first.id.0],
                |_| Ok(()),
            )
            .optional()?
            .is_some(),
        None => false,
    };
    Ok((events, more_before))
}

fn query_events(
    tx: &Transaction,
    sql: &str,
    params: impl uplink_storage::rusqlite::Params,
) -> Result<Vec<RoomEvent>> {
    Ok(tx
        .prepare(sql)?
        .query_map(params, room_event)?
        .collect::<Result<_, _>>()?)
}
