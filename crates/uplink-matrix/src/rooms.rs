//! Rooms and their timelines as synced, and the read marker (SPEC §9.2,
//! §9.5).
//!
//! The read marker is desired state (§6.6): marking a room read moves it
//! locally, and the account's sync task sends a read receipt for it.
//! Receipts only move forward, so undoing a mark-read moves the marker
//! back in uplink only. A receipt of the account's own from another device
//! moves the marker forward.
//!
//! Timelines fold edits, reactions and redactions into the events they
//! are of, when those are stored (SPEC §9.2).

use anyhow::{ensure, Context, Result};
use matrix_sdk::{
    ruma::{
        canonical_json::{self, CanonicalJsonObject, RedactedBecause},
        room_version_rules::RedactionRules,
        RoomVersionId,
    },
    ROOM_VERSION_RULES_FALLBACK,
};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    matrix::{
        Encryption, EventEdit, MatrixSendsChanged, Membership, Reaction, Reactor, RepliedEvent,
        Room, RoomEvent, RoomEventId, RoomId, RoomsChanged, TimelineAdded, TimelineChanged,
        TimelinePage,
    },
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uplink_tx_log::{Applied, Inverse, Observation, Observed, Op};

use crate::{
    html,
    membership::{parse_desired, parse_membership},
    outbox, send,
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
    /// `m.room.topic`, if set and not empty.
    pub topic: Option<String>,
    /// How many members have joined, as the homeserver counts them.
    pub joined_members: u64,
    /// The room version, whose rules redact its events: the SDK's
    /// fallback if it does not know.
    pub version: String,
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
    /// What it relates to (V0025).
    pub relation: Relation,
}

/// What an event relates to, by its JSON (V0025).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Relation {
    pub rel_type: Option<String>,
    pub relates_to: Option<String>,
    pub reply_to: Option<String>,
    pub redacts: Option<String>,
    pub redacted: bool,
}

impl Relation {
    fn of(event: &serde_json::Value) -> Self {
        let relates = &event["content"]["m.relates_to"];
        let text = |value: &serde_json::Value| value.as_str().map(str::to_owned);
        Self {
            rel_type: text(&relates["rel_type"]),
            relates_to: text(&relates["event_id"]),
            reply_to: text(&relates["m.in_reply_to"]["event_id"]),
            // In the content from room version 11, at the top level before.
            redacts: (event["type"] == "m.room.redaction")
                .then(|| text(&event["content"]["redacts"]).or_else(|| text(&event["redacts"])))
                .flatten(),
            redacted: event["unsigned"]["redacted_because"].is_object(),
        }
    }
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
        let (msgtype, body) = message_text(&kind, &event["content"]);
        Ok(Self {
            event_id: field("event_id")?,
            sender: field("sender")?,
            sender_name,
            ts: event["origin_server_ts"]
                .as_i64()
                .with_context(|| format!("event has no origin_server_ts: {raw}"))?,
            msgtype,
            body,
            kind,
            relation: Relation::of(&event),
            raw: raw.to_owned(),
            encryption,
        })
    }
}

/// Of an `m.room.message` event's `content`, the `msgtype` and plain-text
/// `body`.
fn message_text(kind: &str, content: &serde_json::Value) -> (Option<String>, Option<String>) {
    if kind != "m.room.message" {
        return (None, None);
    }
    let text = |name: &str| content[name].as_str().map(str::to_owned);
    (text("msgtype"), text("body"))
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

        let pending = outbox::pending_rooms(tx, account)?;
        let mut events = vec![];
        let mut changed_rooms = vec![];
        let mut sent = vec![];
        let mut added_total = 0;
        for room in self.rooms {
            let (id, upserted) = upsert_room(tx, account, &room)?;
            let changed = set_children(tx, id, &room.children)? || upserted;
            let added = insert_events(tx, id, &room.events)?;
            let redacted = redact(tx, id, &room.version)?;
            let touched = touched(tx, &added, &redacted)?;
            if send::sends_synced(tx, id)? {
                sent.push(id);
            }
            let read = observe_receipts(tx, id, &room.own_receipts)?;
            if changed || read || !added.is_empty() || !redacted.is_empty() {
                changed_rooms.push(id);
            }
            if !added.is_empty() {
                added_total += added.len();
                events.push(ServerEvent::TimelineAdded(TimelineAdded {
                    room: id,
                    events: added,
                }));
            }
            if !touched.is_empty() {
                events.push(ServerEvent::TimelineChanged(TimelineChanged {
                    room: id,
                    events: touched,
                }));
            }
        }
        if !changed_rooms.is_empty() {
            events.push(ServerEvent::RoomsChanged(RoomsChanged {
                rooms: changed_rooms,
            }));
        }
        let outbox_changed = !sent.is_empty() || outbox::pending_rooms(tx, account)? != pending;
        if !sent.is_empty() {
            events.push(ServerEvent::MatrixSendsChanged(MatrixSendsChanged {
                rooms: sent,
            }));
        }
        if outbox_changed {
            events.push(outbox::outbox_changed(account));
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
/// membership, type, avatar, topic or member count changed.
fn upsert_room(tx: &Transaction, account: &str, room: &ObservedRoom) -> Result<(RoomId, bool)> {
    let membership = membership_str(room.membership);
    type Stored = (
        i64,
        String,
        bool,
        String,
        bool,
        Option<String>,
        Option<String>,
        u64,
    );
    let existing: Option<Stored> = tx
        .query_row(
            "SELECT id, name, encrypted, membership, space, avatar, topic, joined_members
             FROM matrix_room WHERE account_id = ?1 AND room_id = ?2",
            [account, &room.room_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()?;
    Ok(match existing {
        Some((id, name, encrypted, old_membership, space, avatar, topic, joined)) => {
            let changed = name != room.name
                || encrypted != room.encrypted
                || old_membership != membership
                || space != room.space
                || avatar != room.avatar
                || topic != room.topic
                || joined != room.joined_members;
            if changed {
                // A membership asked for is had, however it came about.
                tx.execute(
                    "UPDATE matrix_room SET name = ?1, encrypted = ?2, membership = ?3,
                       space = ?4, avatar = ?5, topic = ?6, joined_members = ?7,
                       membership_by = CASE WHEN desired = ?3 THEN NULL ELSE membership_by END,
                       membership_error = CASE WHEN desired = ?3 THEN NULL ELSE membership_error END,
                       desired = CASE WHEN desired = ?3 THEN NULL ELSE desired END
                     WHERE id = ?8",
                    params![
                        room.name,
                        room.encrypted,
                        membership,
                        room.space,
                        room.avatar,
                        room.topic,
                        room.joined_members,
                        id
                    ],
                )?;
            }
            (RoomId(id), changed)
        }
        None => {
            let id = tx.query_row(
                "INSERT INTO matrix_room
                   (account_id, room_id, name, encrypted, membership, space, avatar, topic,
                    joined_members)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
                params![
                    account,
                    room.room_id,
                    room.name,
                    room.encrypted,
                    membership,
                    room.space,
                    room.avatar,
                    room.topic,
                    room.joined_members
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
           (room, event_id, sender, sender_name, ts, type, msgtype, body, raw_json, encryption,
            rel_type, relates_to, reply_to, redacts, redacted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT (room, event_id) DO NOTHING
         RETURNING id",
    )?;
    let mut added = vec![];
    for e in events {
        let r = &e.relation;
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
                    r.rel_type,
                    r.relates_to,
                    r.reply_to,
                    r.redacts,
                    r.redacted,
                ],
                |r| r.get(0),
            )
            .optional()?;
        added.extend(id.map(RoomEventId));
    }
    Ok(added)
}

/// Redacts the room's events that its stored redactions redact and are
/// not redacted yet, as the room `version`'s rules do; returns them.
/// Since a redaction may come before what it redacts, or (from before
/// V0025) have been stored without redacting it, it is not only those
/// just synced that redact.
fn redact(tx: &Transaction, room: RoomId, version: &str) -> Result<Vec<RoomEventId>> {
    let pending: Vec<(i64, String, String)> = tx
        .prepare(
            "SELECT t.id, t.raw_json, r.raw_json
             FROM matrix_event r JOIN matrix_event t ON t.room = r.room AND t.event_id = r.redacts
             WHERE r.room = ?1 AND r.redacts IS NOT NULL AND NOT t.redacted",
        )?
        .query_map([room.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let rules = RoomVersionId::try_from(version)
        .ok()
        .and_then(|v| v.rules())
        .unwrap_or(ROOM_VERSION_RULES_FALLBACK);
    let mut redacted = vec![];
    for (id, raw, redaction) in pending {
        let raw = redacted_json(&raw, &redaction, &rules.redaction)
            .with_context(|| format!("redacting event {id}"))?;
        tx.execute(
            "UPDATE matrix_event SET raw_json = ?1, redacted = 1, msgtype = NULL, body = NULL
             WHERE id = ?2",
            params![raw, id],
        )?;
        redacted.push(RoomEventId(id));
    }
    Ok(redacted)
}

/// The event JSON `raw` as `redaction` redacts it, by `rules`.
pub(crate) fn redacted_json(raw: &str, redaction: &str, rules: &RedactionRules) -> Result<String> {
    let because = RedactedBecause::from_json(serde_json::from_str(redaction)?);
    match serde_json::from_str::<CanonicalJsonObject>(raw) {
        Ok(event) => Ok(serde_json::to_string(&canonical_json::redact(
            event,
            rules,
            Some(because),
        )?)?),
        // Homeservers allow only canonical JSON in events, so only the
        // payload of an encrypted one can be otherwise (a float, say).
        // That is a message-like event, of which redaction keeps no
        // content.
        Err(_) => {
            let mut event: serde_json::Value = serde_json::from_str(raw)?;
            event["content"] = serde_json::json!({});
            event["unsigned"] = serde_json::json!({
                "redacted_because": serde_json::from_str::<serde_json::Value>(redaction)?,
            });
            Ok(event.to_string())
        }
    }
}

/// The room's listed events whose display `added` and `redacted` changed
/// (but those added): the events added or redacted reactions are of; those
/// whose content changed (redacted, or of edits added or redacted); and
/// the replies to those, which show what they reply to.
fn touched(
    tx: &Transaction,
    added: &[RoomEventId],
    redacted: &[RoomEventId],
) -> Result<Vec<RoomEventId>> {
    let mut target = tx.prepare(
        "SELECT t.id, e.rel_type FROM matrix_event e JOIN matrix_event t
           ON t.room = e.room AND t.event_id = e.relates_to
         WHERE e.id = ?1 AND e.rel_type IN ('m.replace', 'm.annotation')",
    )?;
    let mut touched = std::collections::BTreeSet::new();
    let mut content_changed: Vec<RoomEventId> = redacted.to_vec();
    for event in added.iter().chain(redacted) {
        let of: Option<(i64, String)> = target
            .query_row([event.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((of, rel_type)) = of {
            touched.insert(RoomEventId(of));
            if rel_type == "m.replace" {
                content_changed.push(RoomEventId(of));
            }
        }
    }
    let mut replies = tx.prepare(
        "SELECT r.id FROM matrix_event e JOIN matrix_event r
           ON r.room = e.room AND r.reply_to = e.event_id
         WHERE e.id = ?1",
    )?;
    for event in content_changed {
        touched.insert(event);
        for reply in replies.query_map([event.0], |r| r.get(0))? {
            touched.insert(RoomEventId(reply?));
        }
    }
    for event in added {
        touched.remove(event);
    }
    let touched = serde_json::to_string(&touched.into_iter().map(|e| e.0).collect::<Vec<_>>())?;
    Ok(tx
        .prepare(&format!(
            "SELECT e.id FROM matrix_event e
             WHERE e.id IN (SELECT value FROM json_each(?1)) AND {LISTED} ORDER BY e.id"
        ))?
        .query_map([touched], |r| Ok(RoomEventId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
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
        let (account, name, previous): (String, String, Option<i64>) = tx
            .query_row(
                "SELECT account_id, name, read_up_to FROM matrix_room WHERE id = ?1",
                [self.room.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .with_context(|| format!("no room {}", self.room.0))?;
        let pending = outbox::pending_rooms(tx, &account)?;
        tx.execute(
            "UPDATE matrix_room SET read_up_to = ?1 WHERE id = ?2",
            params![self.up_to.map(|e| e.0), self.room.0],
        )?;
        let mut events = vec![ServerEvent::RoomsChanged(RoomsChanged {
            rooms: vec![self.room],
        })];
        // A receipt to send, or none.
        if outbox::pending_rooms(tx, &account)? != pending {
            events.push(outbox::outbox_changed(&account));
        }
        Ok(Applied {
            description: format!("Read {name}"),
            inverse: Inverse::of(&MarkRead {
                room: self.room,
                up_to: previous.map(RoomEventId),
            }),
            events,
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

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let pending = outbox::pending_rooms(tx, account)?;
        tx.execute(
            "UPDATE matrix_room SET read_pushed = max(coalesce(read_pushed, 0), ?1) WHERE id = ?2",
            params![self.up_to.0, self.room.0],
        )?;
        // Nothing else clients show changed.
        let events = if outbox::pending_rooms(tx, account)? == pending {
            vec![]
        } else {
            vec![outbox::outbox_changed(account)]
        };
        Ok(Observed {
            description: "Sent a read receipt".to_owned(),
            events,
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
/// always are; edits and redacted messages do not.
pub fn rooms(tx: &Transaction, account: &str, mxid: &str) -> Result<Vec<Room>> {
    Ok(tx
        .prepare(&format!(
            "SELECT r.id, r.account_id, r.room_id, r.name, r.encrypted, r.membership,
                    (SELECT count(*) FROM matrix_event e
                     WHERE e.room = r.id AND e.id > coalesce(r.read_up_to, 0)
                       AND e.type IN ('m.room.message', 'm.room.encrypted')
                       AND e.sender != ?2 AND NOT e.redacted AND {LISTED}),
                    (SELECT max(ts) FROM matrix_event e WHERE e.room = r.id),
                    r.read_up_to, r.desired, r.membership_error, r.space, r.avatar,
                    -- In the space's order: by `order`, those without last.
                    (SELECT json_group_array(child) FROM
                       (SELECT child FROM matrix_space_child c WHERE c.space = r.id
                        ORDER BY child_order IS NULL, child_order, child)),
                    r.topic, r.joined_members
             FROM matrix_room r
             WHERE r.account_id = ?1 AND (r.membership != 'left' OR r.desired = 'joined')
             ORDER BY 8 DESC NULLS LAST, r.id"
        ))?
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
                topic: r.get(14)?,
                joined_members: r.get(15)?,
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

/// The account the room is of, and the contents of its event `event`: its
/// own, then its edits' new ones, latest first; `None` if the event is
/// not of the room.
pub fn event_contents(
    tx: &Transaction,
    room: RoomId,
    event: RoomEventId,
) -> Result<Option<(String, Vec<serde_json::Value>)>> {
    let Some((account, raw)): Option<(String, String)> = tx
        .query_row(
            "SELECT r.account_id, e.raw_json FROM matrix_event e JOIN matrix_room r ON r.id = e.room
             WHERE e.id = ?1 AND e.room = ?2",
            params![event.0, room.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let raw: serde_json::Value = serde_json::from_str(&raw)?;
    let mut contents = vec![raw["content"].clone()];
    let edits: Vec<String> = tx
        .prepare(&format!(
            "SELECT json_extract(e.raw_json, '$.content.\"m.new_content\"')
             FROM matrix_event t JOIN matrix_event e ON e.room = t.room AND e.relates_to = t.event_id
             WHERE t.id = ?1 AND {EDIT} ORDER BY e.ts DESC, e.event_id DESC"
        ))?
        .query_map([event.0], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for edit in edits {
        contents.push(serde_json::from_str(&edit)?);
    }
    Ok(Some((account, contents)))
}

/// The account a room is of and its Matrix id, if the room exists.
pub fn room_of(conn: &Connection, room: RoomId) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT account_id, room_id FROM matrix_room WHERE id = ?1",
            [room.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// What a send to the room needs to know of its event `event_id`.
pub struct StoredEvent {
    pub sender: String,
    pub kind: String,
    pub redacted: bool,
}

pub fn stored_event(
    conn: &Connection,
    room: RoomId,
    event_id: &str,
) -> Result<Option<StoredEvent>> {
    Ok(conn
        .query_row(
            "SELECT sender, type, redacted FROM matrix_event WHERE room = ?1 AND event_id = ?2",
            params![room.0, event_id],
            |r| {
                Ok(StoredEvent {
                    sender: r.get(0)?,
                    kind: r.get(1)?,
                    redacted: r.get(2)?,
                })
            },
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

/// Of an event `e`, whether its timeline lists it: not if it is folded
/// into a stored event (an edit, a reaction or a redaction of one), nor a
/// reaction redacted (which no longer says what it was of).
pub(crate) const LISTED: &str = "NOT (
      (e.rel_type IN ('m.replace', 'm.annotation') OR e.type = 'm.room.redaction')
      AND EXISTS (SELECT 1 FROM matrix_event t
                  WHERE t.room = e.room AND t.event_id = coalesce(e.redacts, e.relates_to)))
    AND NOT (e.type = 'm.reaction' AND e.redacted)";

const COLUMNS: &str = "e.id, e.event_id, e.sender, e.sender_name, e.ts, e.type, e.msgtype, \
                       e.body, e.raw_json, e.encryption, e.redacted, e.reply_to";

/// An event of [`COLUMNS`], without what relates to it, and the event it
/// replies to.
fn room_event(r: &Row) -> uplink_storage::rusqlite::Result<(RoomEvent, Option<String>)> {
    let encryption: String = r.get(9)?;
    Ok((
        RoomEvent {
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
            redacted: r.get(10)?,
            edit: None,
            reactions: vec![],
            replied: None,
            html: None,
        },
        r.get(11)?,
    ))
}

/// Up to `limit` events of the room's timeline, oldest first, and whether
/// there are earlier ones.
pub fn timeline(
    tx: &Transaction,
    room: RoomId,
    page: TimelinePage,
    limit: u32,
) -> Result<(Vec<RoomEvent>, bool)> {
    let select = |condition: &str, order: &str| {
        format!(
            "SELECT {COLUMNS} FROM matrix_event e
             WHERE e.room = ?1 AND {condition} AND {LISTED} ORDER BY e.id {order} LIMIT ?3"
        )
    };
    let (sql, after, newest_first) = match page {
        TimelinePage::Latest => (select("?2 IS NULL", "DESC"), None, true),
        TimelinePage::Before { event } => (select("e.id < ?2", "DESC"), Some(event.0), true),
        TimelinePage::After { event } => (select("e.id > ?2", "ASC"), Some(event.0), false),
    };
    let mut events = query_events(tx, room, &sql, params![room.0, after, limit])?;
    if newest_first {
        events.reverse();
    }
    let more_before = match events.first() {
        Some(first) => tx
            .query_row(
                &format!(
                    "SELECT 1 FROM matrix_event e WHERE e.room = ?1 AND e.id < ?2 AND {LISTED}"
                ),
                params![room.0, first.id.0],
                |_| Ok(()),
            )
            .optional()?
            .is_some(),
        None => false,
    };
    Ok((events, more_before))
}

/// Those of `events` that the room's timeline lists, as it lists them, in
/// its order.
pub fn listed_events(
    tx: &Transaction,
    room: RoomId,
    events: &[RoomEventId],
) -> Result<Vec<RoomEvent>> {
    let ids = serde_json::to_string(&events.iter().map(|e| e.0).collect::<Vec<_>>())?;
    query_events(
        tx,
        room,
        &format!(
            "SELECT {COLUMNS} FROM matrix_event e
             WHERE e.room = ?1 AND e.id IN (SELECT value FROM json_each(?2)) AND {LISTED}
             ORDER BY e.id"
        ),
        params![room.0, ids],
    )
}

/// The edits of the room's message `event` by its sender, oldest first.
pub fn edits(tx: &Transaction, room: RoomId, event: RoomEventId) -> Result<Vec<RoomEvent>> {
    query_events(
        tx,
        room,
        &format!(
            "SELECT {COLUMNS} FROM matrix_event t JOIN matrix_event e
               ON e.room = t.room AND e.relates_to = t.event_id
             WHERE t.room = ?1 AND t.id = ?2 AND {EDIT} ORDER BY e.ts, e.event_id"
        ),
        params![room.0, event.0],
    )
}

/// Of an event `t`, whether `e` is a valid edit of it: a message replacing
/// it, sent by its sender, with new content, and neither redacted. An edit
/// is not edited itself.
const EDIT: &str = "e.rel_type = 'm.replace' AND e.type = 'm.room.message' AND NOT e.redacted
    AND NOT t.redacted AND e.sender = t.sender AND coalesce(t.rel_type, '') != 'm.replace'
    AND json_type(e.raw_json, '$.content.\"m.new_content\"') = 'object'";

/// The events `sql` selects, of [`COLUMNS`], with what relates to them.
fn query_events(
    tx: &Transaction,
    room: RoomId,
    sql: &str,
    params: impl uplink_storage::rusqlite::Params,
) -> Result<Vec<RoomEvent>> {
    let rows: Vec<(RoomEvent, Option<String>)> = tx
        .prepare(sql)?
        .query_map(params, room_event)?
        .collect::<Result<_, _>>()?;
    let mut latest_edit = tx.prepare(&format!(
        "SELECT e.event_id, e.ts, json_extract(e.raw_json, '$.content.\"m.new_content\"')
         FROM matrix_event t JOIN matrix_event e ON e.room = t.room AND e.relates_to = t.event_id
         WHERE t.id = ?1 AND {EDIT}
         ORDER BY e.ts DESC, e.event_id DESC LIMIT 1"
    ))?;
    let mut reactions = tx.prepare(
        "SELECT json_extract(e.raw_json, '$.content.\"m.relates_to\".key'),
                e.sender, e.sender_name, e.event_id
         FROM matrix_event t JOIN matrix_event e ON e.room = t.room AND e.relates_to = t.event_id
         WHERE t.id = ?1 AND e.rel_type = 'm.annotation' AND e.type = 'm.reaction'
           AND NOT e.redacted
           AND json_type(e.raw_json, '$.content.\"m.relates_to\".key') = 'text'
         ORDER BY e.id",
    )?;
    let mut replied = tx.prepare(&format!(
        "SELECT t.id, t.event_id, t.sender, t.sender_name, t.type, t.redacted,
                coalesce(json_extract(e.raw_json, '$.content.\"m.new_content\".msgtype'), t.msgtype),
                coalesce(json_extract(e.raw_json, '$.content.\"m.new_content\".body'), t.body)
         FROM matrix_event t LEFT JOIN matrix_event e ON e.id = (
           SELECT e.id FROM matrix_event e
           WHERE e.room = t.room AND e.relates_to = t.event_id AND {EDIT}
           ORDER BY e.ts DESC, e.event_id DESC LIMIT 1)
         WHERE t.room = ?1 AND t.event_id = ?2"
    ))?;
    let mut events = Vec::with_capacity(rows.len());
    for (mut event, reply_to) in rows {
        event.edit = latest_edit
            .query_row([event.id.0], |r| {
                Ok(EventEdit {
                    event_id: r.get(0)?,
                    ts: r.get(1)?,
                    content: r.get(2)?,
                    html: None,
                })
            })
            .optional()?;
        if let Some(edit) = &mut event.edit {
            let content: serde_json::Value = serde_json::from_str(&edit.content)?;
            edit.html = html::sanitized(room, event.id, &content);
        }
        let raw: serde_json::Value = serde_json::from_str(&event.raw)?;
        event.html = html::sanitized(room, event.id, &raw["content"]);
        for row in reactions.query_map([event.id.0], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Reactor {
                    sender: r.get(1)?,
                    sender_name: r.get(2)?,
                    event_id: r.get(3)?,
                },
            ))
        })? {
            let (key, reactor) = row?;
            match event.reactions.iter_mut().find(|r| r.key == key) {
                Some(reaction) => {
                    // Each sender counts once per key.
                    if !reaction.senders.iter().any(|s| s.sender == reactor.sender) {
                        reaction.senders.push(reactor);
                    }
                }
                None => event.reactions.push(Reaction {
                    key,
                    senders: vec![reactor],
                }),
            }
        }
        if let Some(reply_to) = reply_to {
            event.replied = replied
                .query_row(params![room.0, reply_to], |r| {
                    Ok(RepliedEvent {
                        id: RoomEventId(r.get(0)?),
                        event_id: r.get(1)?,
                        sender: r.get(2)?,
                        sender_name: r.get(3)?,
                        kind: r.get(4)?,
                        redacted: r.get(5)?,
                        msgtype: r.get(6)?,
                        body: r.get(7)?,
                    })
                })
                .optional()?;
        }
        events.push(event);
    }
    Ok(events)
}
