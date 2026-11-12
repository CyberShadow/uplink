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
use std::collections::HashSet;
use uplink_protocol::{
    matrix::{
        Encryption, EventEdit, GapId, MatrixSendsChanged, Membership, Reaction, Reactor,
        ReadMarker, RepliedEvent, Room, RoomEvent, RoomEventId, RoomId, RoomsChanged,
        ThreadSummary, TimelineAdded, TimelineChanged, TimelineFilled, TimelineGap, TimelinePage,
    },
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uplink_tx_log::{Applied, Inverse, Observation, Observed, Op};

use crate::{
    html,
    membership::{parse_desired, parse_membership},
    outbox, room_tags, send, space_order,
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
    /// Whether it is a direct chat, by the account's `m.direct`.
    pub direct: bool,
    /// `mxc://` URI: the room's avatar, or of a direct chat without one,
    /// the other user's.
    pub avatar: Option<String>,
    /// Of a space, the rooms it lists.
    pub children: Vec<SpaceChild>,
    /// Its MSC3230 order among the top-level spaces, if valid.
    pub space_order: Option<String>,
    /// Its tags (`m.tag`), as the homeserver has them.
    pub tags: Vec<String>,
    /// `m.room.topic`, if set and not empty.
    pub topic: Option<String>,
    /// Its address, `m.room.canonical_alias`, if it has one.
    pub alias: Option<String>,
    /// The events pinned (`m.room.pinned_events`), in its order.
    pub pinned: Vec<String>,
    /// Whether the account may pin and unpin events, by the room's power
    /// levels.
    pub may_pin: bool,
    /// How many members have joined, as the homeserver counts them.
    pub joined_members: u64,
    /// The room version, whose rules redact its events: the SDK's
    /// fallback if it does not know.
    pub version: String,
    /// Timeline events, in order. Not in the log: they are stored.
    #[serde(skip)]
    pub events: Vec<ObservedEvent>,
    /// Whether events since the last sync were left out: a gap before
    /// those of `events` (SPEC §9.2).
    pub limited: bool,
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
        Ok(listed.then(|| Self {
            room_id: room_id.to_owned(),
            order: space_order::parse(content),
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
    /// What the account's push rules make of it (V0032).
    pub push: Push,
}

/// What the account's push rules make of an event, as its client
/// evaluates them: whether it notifies, and whether it highlights. Not
/// evaluated, neither.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Push {
    pub notify: bool,
    pub highlight: bool,
}

impl Push {
    pub fn of(actions: Option<&[matrix_sdk::ruma::push::Action]>) -> Self {
        let actions = actions.unwrap_or_default();
        Self {
            notify: actions.iter().any(|a| a.should_notify()),
            highlight: actions.iter().any(|a| a.is_highlight()),
        }
    }
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
            push: Push::default(),
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
            let tagged = room_tags::observe(tx, id, &room.tags)?;
            let changed = set_children(tx, id, &room.children)? || upserted || tagged;
            let positions = synced_positions(tx, id, room.events.len())?;
            let inserted = insert_events(tx, id, &room.events, &positions)?;
            if let (true, Some(Some(first))) = (room.limited, inserted.first()) {
                tx.execute(
                    "INSERT INTO matrix_gap (room, before_event) VALUES (?1, ?2)",
                    params![id.0, first.0],
                )?;
            }
            let added: Vec<RoomEventId> = inserted.into_iter().flatten().collect();
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
/// membership, type, avatar, topic, address, pinned events, power to
/// pin, member count, space order or being a direct chat changed.
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
        String,
        bool,
        Option<String>,
        bool,
        Option<String>,
    );
    let pinned = serde_json::to_string(&room.pinned)?;
    let existing: Option<Stored> = tx
        .query_row(
            "SELECT id, name, encrypted, membership, space, avatar, topic, joined_members, pinned,
                    may_pin, space_order, direct, alias
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
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get(12)?,
                ))
            },
        )
        .optional()?;
    Ok(match existing {
        Some((
            id,
            name,
            encrypted,
            old_membership,
            space,
            avatar,
            topic,
            joined,
            old_pinned,
            may_pin,
            order,
            direct,
            alias,
        )) => {
            let changed = name != room.name
                || encrypted != room.encrypted
                || old_membership != membership
                || space != room.space
                || avatar != room.avatar
                || topic != room.topic
                || joined != room.joined_members
                || old_pinned != pinned
                || may_pin != room.may_pin
                || order != room.space_order
                || direct != room.direct
                || alias != room.alias;
            if changed {
                // A membership or space order asked for is had, however it
                // came about.
                tx.execute(
                    &format!(
                        "UPDATE matrix_room SET name = ?1, encrypted = ?2, membership = ?3,
                           space = ?4, avatar = ?5, topic = ?6, joined_members = ?7, pinned = ?9,
                           may_pin = ?10, direct = ?12, alias = ?13,
                           membership_by = CASE WHEN desired = ?3 THEN NULL ELSE membership_by END,
                           membership_error =
                             CASE WHEN desired = ?3 THEN NULL ELSE membership_error END,
                           desired = CASE WHEN desired = ?3 THEN NULL ELSE desired END,
                           space_order = ?11,
                           space_order_error =
                             CASE WHEN {had} THEN NULL ELSE space_order_error END,
                           space_order_desired =
                             CASE WHEN {had} THEN NULL ELSE space_order_desired END,
                           space_order_by = CASE WHEN {had} THEN NULL ELSE space_order_by END
                         WHERE id = ?8",
                        had = "space_order_by IS NOT NULL AND space_order_desired IS ?11",
                    ),
                    params![
                        room.name,
                        room.encrypted,
                        membership,
                        room.space,
                        room.avatar,
                        room.topic,
                        room.joined_members,
                        id,
                        pinned,
                        room.may_pin,
                        room.space_order,
                        room.direct,
                        room.alias,
                    ],
                )?;
            }
            (RoomId(id), changed)
        }
        None => {
            let id = tx.query_row(
                "INSERT INTO matrix_room
                   (account_id, room_id, name, encrypted, membership, space, avatar, topic,
                    joined_members, pinned, may_pin, space_order, direct, alias)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) RETURNING id",
                params![
                    account,
                    room.room_id,
                    room.name,
                    room.encrypted,
                    membership,
                    room.space,
                    room.avatar,
                    room.topic,
                    room.joined_members,
                    pinned,
                    room.may_pin,
                    room.space_order,
                    room.direct,
                    room.alias,
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

/// How far apart synced events are placed: the room for events
/// backfilled into a gap between two.
const SPACING: i64 = 1 << 20;

/// Positions for `n` events synced to the end of the room's timeline.
fn synced_positions(tx: &Transaction, room: RoomId, n: usize) -> Result<Vec<i64>> {
    let last: i64 = tx.query_row(
        "SELECT coalesce(max(pos), 0) FROM matrix_event WHERE room = ?1",
        [room.0],
        |r| r.get(0),
    )?;
    Ok((1..=n as i64).map(|i| last + i * SPACING).collect())
}

/// Stores the events not stored yet, at `positions`; returns, of each,
/// its id if it was stored now.
fn insert_events(
    tx: &Transaction,
    room: RoomId,
    events: &[ObservedEvent],
    positions: &[i64],
) -> Result<Vec<Option<RoomEventId>>> {
    assert_eq!(events.len(), positions.len());
    let mut insert = tx.prepare(
        "INSERT INTO matrix_event
           (room, event_id, sender, sender_name, ts, type, msgtype, body, raw_json, encryption,
            rel_type, relates_to, reply_to, redacts, redacted, pos, notify, highlight)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
         ON CONFLICT (room, event_id) DO NOTHING
         RETURNING id",
    )?;
    let mut added = vec![];
    for (e, pos) in events.iter().zip(positions) {
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
                    pos,
                    e.push.notify,
                    e.push.highlight,
                ],
                |r| r.get(0),
            )
            .optional()?;
        added.push(id.map(RoomEventId));
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
/// (but those added): the events added or redacted reactions are of; the
/// roots of threads they are of, which sum their threads up; those whose
/// content changed (redacted, or of edits added or redacted), the replies
/// to those, which show what they reply to, and the roots of threads of
/// which they are, whose latest message may be one; and the messages of
/// threads whose roots were added, which go from the main timeline to
/// the thread.
fn touched(
    tx: &Transaction,
    added: &[RoomEventId],
    redacted: &[RoomEventId],
) -> Result<Vec<RoomEventId>> {
    let mut target = tx.prepare(
        "SELECT t.id, e.rel_type FROM matrix_event e JOIN matrix_event t
           ON t.room = e.room AND t.event_id = e.relates_to
         WHERE e.id = ?1 AND e.rel_type IN ('m.replace', 'm.annotation', 'm.thread')",
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
         WHERE e.id = ?1
         UNION
         SELECT t.id FROM matrix_event e JOIN matrix_event t
           ON t.room = e.room AND t.event_id = e.relates_to
         WHERE e.id = ?1 AND e.rel_type = 'm.thread'",
    )?;
    let mut of_thread = tx.prepare(
        "SELECT m.id FROM matrix_event t JOIN matrix_event m
           ON m.room = t.room AND m.relates_to = t.event_id
         WHERE t.id = ?1 AND m.rel_type = 'm.thread'",
    )?;
    for event in content_changed {
        touched.insert(event);
        for reply in replies.query_map([event.0], |r| r.get(0))? {
            touched.insert(RoomEventId(reply?));
        }
    }
    for event in added {
        for message in of_thread.query_map([event.0], |r| r.get(0))? {
            touched.insert(RoomEventId(message?));
        }
    }
    for event in added {
        touched.remove(event);
    }
    let touched = serde_json::to_string(&touched.into_iter().map(|e| e.0).collect::<Vec<_>>())?;
    Ok(tx
        .prepare(&format!(
            "SELECT e.id FROM matrix_event e
             WHERE e.id IN (SELECT value FROM json_each(?1)) AND {LISTED} ORDER BY e.pos"
        ))?
        .query_map([touched], |r| Ok(RoomEventId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// Moves the read marker forward to the latest of the events the
/// account's own receipts are for, if that is past the latest receipt
/// already sent or observed; returns whether it moved.
fn observe_receipts(tx: &Transaction, room: RoomId, receipts: &[String]) -> Result<bool> {
    // The latest, by position.
    let mut latest: Option<(i64, i64)> = None;
    for event_id in receipts {
        let event: Option<(i64, i64)> = tx
            .query_row(
                "SELECT pos, id FROM matrix_event WHERE room = ?1 AND event_id = ?2",
                params![room.0, event_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        latest = latest.max(event);
    }
    let Some((pos, latest)) = latest else {
        return Ok(false);
    };
    Ok(tx.execute(
        &format!(
            "UPDATE matrix_room
             SET read_pushed = ?1,
                 read_up_to = CASE WHEN {up_to} IS NULL OR {up_to} < ?3 THEN ?1 ELSE read_up_to END
             WHERE id = ?2 AND ({pushed} IS NULL OR {pushed} < ?3)",
            up_to = pos_of("read_up_to"),
            pushed = pos_of("read_pushed"),
        ),
        params![latest, room.0, pos],
    )? > 0)
}

// ── decrypting late ───────────────────────────────────────────────────────

/// Events of the room stored undecryptable that the client has decrypted
/// since: their keys came later, from another device, the key backup or
/// an import (SPEC §9.2).
#[derive(Debug, Serialize)]
pub struct Decrypted {
    pub room_id: String,
    /// Not in the log: they are stored.
    #[serde(skip)]
    pub events: Vec<ObservedEvent>,
}

impl Observation for Decrypted {
    const KIND: &'static str = "matrix.decrypted";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let room: RoomId = tx
            .query_row(
                "SELECT id FROM matrix_room WHERE account_id = ?1 AND room_id = ?2",
                [account, &self.room_id],
                |r| Ok(RoomId(r.get(0)?)),
            )
            .with_context(|| format!("no room {} of {account}", self.room_id))?;
        // Those redacted meanwhile have no content left to show.
        let mut update = tx.prepare(
            "UPDATE matrix_event
             SET type = ?3, msgtype = ?4, body = ?5, raw_json = ?6, encryption = 'decrypted',
                 rel_type = ?7, relates_to = ?8, reply_to = ?9, redacts = ?10,
                 notify = ?11, highlight = ?12
             WHERE room = ?1 AND event_id = ?2 AND encryption = 'undecryptable' AND NOT redacted
             RETURNING id",
        )?;
        let mut decrypted = vec![];
        for e in &self.events {
            let r = &e.relation;
            let id = update
                .query_row(
                    params![
                        room.0,
                        e.event_id,
                        e.kind,
                        e.msgtype,
                        e.body,
                        e.raw,
                        r.rel_type,
                        r.relates_to,
                        r.reply_to,
                        r.redacts,
                        e.push.notify,
                        e.push.highlight,
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            decrypted.extend(id.map(RoomEventId));
        }
        // Edits and reactions now known change what they are of; the
        // events decrypted change themselves, and may now be relations no
        // longer listed.
        let mut changed: std::collections::BTreeSet<RoomEventId> =
            touched(tx, &decrypted, &[])?.into_iter().collect();
        changed.extend(&decrypted);
        let events = if decrypted.is_empty() {
            vec![]
        } else {
            vec![
                ServerEvent::TimelineChanged(TimelineChanged {
                    room,
                    events: changed.into_iter().collect(),
                }),
                // Its unread count, of messages.
                ServerEvent::RoomsChanged(RoomsChanged { rooms: vec![room] }),
            ]
        };
        Ok(Observed {
            description: match decrypted.len() {
                1 => "Decrypted 1 event".to_owned(),
                n => format!("Decrypted {n} events"),
            },
            events,
        })
    }
}

/// The account's events stored undecryptable (and not redacted), by
/// their room's Matrix id, with their encrypted JSON; of the session
/// `session` (a room's Matrix id and a Megolm session id) only, if given.
pub fn undecryptable(
    conn: &Connection,
    account: &str,
    session: Option<(&str, &str)>,
) -> Result<Vec<(String, String)>> {
    let (room_id, session_id) = session.unzip();
    Ok(conn
        .prepare(
            "SELECT r.room_id, e.raw_json FROM matrix_event e JOIN matrix_room r ON r.id = e.room
             WHERE r.account_id = ?1 AND e.encryption = 'undecryptable' AND NOT e.redacted
               AND (?2 IS NULL OR (r.room_id = ?2
                    AND json_extract(e.raw_json, '$.content.session_id') = ?3))
             ORDER BY e.id",
        )?
        .query_map(params![account, room_id, session_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?)
}

// ── the read marker ───────────────────────────────────────────────────────

/// Sets the room's read marker; `None` is before the first event.
/// `shown`: by the room being shown (SPEC §12.15).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkRead {
    pub room: RoomId,
    pub up_to: Option<RoomEventId>,
    #[serde(default)]
    pub shown: bool,
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
                shown: false,
            }),
            events,
        })
    }

    fn shown(&self) -> bool {
        self.shown
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
            &format!(
                "UPDATE matrix_room SET read_pushed = ?1
                 WHERE id = ?2 AND ({pushed} IS NULL OR {pushed} < {sent})",
                pushed = pos_of("read_pushed"),
                sent = pos_of("?1"),
            ),
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
        .prepare(&format!(
            "SELECT r.id, r.room_id, e.id, e.event_id
                 FROM matrix_room r JOIN matrix_event e ON e.id = r.read_up_to
                 WHERE r.account_id = ?1 AND r.membership = 'joined' AND {UNSENT_RECEIPT}"
        ))?
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
                    (SELECT count(*) FROM matrix_event e WHERE {unread} AND {MAIN}),
                    (SELECT max(ts) FROM matrix_event e WHERE e.room = r.id),
                    r.read_up_to, r.desired, r.membership_error, r.space, r.avatar,
                    -- In the space's order: by `order`, those without last.
                    (SELECT json_group_array(child) FROM
                       (SELECT child FROM matrix_space_child c WHERE c.space = r.id
                        ORDER BY child_order IS NULL, child_order, child)),
                    r.topic, r.joined_members, {read_pos}, r.pinned, r.may_pin,
                    -- Of threads too, as Element counts them.
                    (SELECT count(*) FROM matrix_event e WHERE {unread} AND e.notify),
                    (SELECT count(*) FROM matrix_event e WHERE {unread} AND e.highlight),
                    {shown_order}, {shown_tags}, r.direct,
                    (SELECT e.id FROM matrix_event e WHERE e.room = r.id AND {MAIN} AND {LISTED}
                     ORDER BY e.pos DESC LIMIT 1),
                    r.alias
             FROM matrix_room r
             WHERE r.account_id = ?1 AND (r.membership != 'left' OR r.desired = 'joined')
             ORDER BY 8 DESC NULLS LAST, r.id",
            read_pos = pos_of("r.read_up_to"),
            unread = format!(
                "e.room = r.id
                 AND (r.read_up_to IS NULL OR e.pos > {read_pos})
                 AND e.type IN ('m.room.message', 'm.room.encrypted')
                 AND e.sender != ?2 AND NOT e.redacted AND {LISTED}",
                read_pos = pos_of("r.read_up_to"),
            ),
            shown_order = space_order::SHOWN,
            shown_tags = room_tags::SHOWN,
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
                read_up_to: match (r.get::<_, Option<i64>>(8)?, r.get(16)?) {
                    (Some(event), Some(pos)) => Some(ReadMarker {
                        event: RoomEventId(event),
                        pos,
                    }),
                    (None, None) => None,
                    _ => unreachable!("a read marker is of a stored event"),
                },
                pending: parse_desired(r.get::<_, Option<String>>(9)?.as_deref()),
                membership_error: r.get(10)?,
                space: r.get(11)?,
                avatar: r.get(12)?,
                children: serde_json::from_str(&r.get::<_, String>(13)?)
                    .expect("json_group_array makes an array of strings"),
                topic: r.get(14)?,
                alias: r.get(25)?,
                joined_members: r.get(15)?,
                may_pin: r.get(18)?,
                notifying: r.get(19)?,
                highlighted: r.get(20)?,
                pinned: serde_json::from_str(&r.get::<_, String>(17)?)
                    .expect("pinned is a JSON array of strings"),
                space_order: r.get(21)?,
                tags: serde_json::from_str(&r.get::<_, String>(22)?)
                    .expect("json_group_array makes an array of strings"),
                direct: r.get(23)?,
                latest: r.get::<_, Option<i64>>(24)?.map(RoomEventId),
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

/// The ids of the account's rooms `room_ids` (Matrix ids), of those
/// recorded.
pub fn room_ids(
    conn: &Connection,
    account: &str,
    room_ids: &[String],
) -> Result<Vec<Option<RoomId>>> {
    let mut query =
        conn.prepare("SELECT id FROM matrix_room WHERE account_id = ?1 AND room_id = ?2")?;
    room_ids
        .iter()
        .map(|room_id| {
            Ok(query
                .query_row([account, room_id], |r| Ok(RoomId(r.get(0)?)))
                .optional()?)
        })
        .collect()
}

/// The stored ids of the room's events `event_ids` (Matrix ids), with
/// their positions, of those stored.
pub fn event_ids(
    conn: &Connection,
    room: RoomId,
    event_ids: &[String],
) -> Result<Vec<Option<(RoomEventId, i64)>>> {
    let mut query =
        conn.prepare("SELECT id, pos FROM matrix_event WHERE room = ?1 AND event_id = ?2")?;
    event_ids
        .iter()
        .map(|event_id| {
            Ok(query
                .query_row(params![room.0, event_id], |r| {
                    Ok((RoomEventId(r.get(0)?), r.get(1)?))
                })
                .optional()?)
        })
        .collect()
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
    /// Its relation (`m.relates_to`): its type, and the event it is of.
    pub rel_type: Option<String>,
    pub relates_to: Option<String>,
}

pub fn stored_event(
    conn: &Connection,
    room: RoomId,
    event_id: &str,
) -> Result<Option<StoredEvent>> {
    Ok(conn
        .query_row(
            "SELECT sender, type, redacted, rel_type, relates_to FROM matrix_event
             WHERE room = ?1 AND event_id = ?2",
            params![room.0, event_id],
            |r| {
                Ok(StoredEvent {
                    sender: r.get(0)?,
                    kind: r.get(1)?,
                    redacted: r.get(2)?,
                    rel_type: r.get(3)?,
                    relates_to: r.get(4)?,
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

/// Of an event `e`, whether the room's main timeline lists it: listed,
/// and not of a thread whose root is stored, which the thread lists
/// (`RoomEvent::thread_root`).
pub(crate) const MAIN: &str = "NOT (e.rel_type = 'm.thread'
      AND EXISTS (SELECT 1 FROM matrix_event t
                  WHERE t.room = e.room AND t.event_id = e.relates_to))";

/// An event's columns, and the event it replies to: but not the fallback
/// of a message of a thread (to the thread's latest), which only clients
/// that do not know threads show.
const COLUMNS: &str = "e.id, e.event_id, e.sender, e.sender_name, e.ts, e.type, e.msgtype, \
     e.body, e.raw_json, e.encryption, e.redacted, \
     CASE WHEN e.rel_type = 'm.thread' \
       AND json_extract(e.raw_json, '$.content.\"m.relates_to\".is_falling_back') \
     THEN NULL ELSE e.reply_to END, e.pos, \
     (SELECT t.id FROM matrix_event t \
      WHERE e.rel_type = 'm.thread' AND t.room = e.room AND t.event_id = e.relates_to), \
     e.highlight";

/// An event of [`COLUMNS`], without what relates to it, and the event it
/// replies to.
fn room_event(r: &Row) -> uplink_storage::rusqlite::Result<(RoomEvent, Option<String>)> {
    let encryption: String = r.get(9)?;
    Ok((
        RoomEvent {
            id: RoomEventId(r.get(0)?),
            pos: r.get(12)?,
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
            thread: None,
            thread_root: r.get::<_, Option<i64>>(13)?.map(RoomEventId),
            highlight: r.get(14)?,
        },
        r.get(11)?,
    ))
}

/// A page of the room's timeline: up to `limit` events, oldest first;
/// whether there are earlier ones; and the gaps among them (see
/// `ServerResponse::Timeline`).
pub fn timeline(
    tx: &Transaction,
    room: RoomId,
    page: TimelinePage,
    limit: u32,
) -> Result<(Vec<RoomEvent>, bool, Vec<TimelineGap>)> {
    let select = |condition: &str, order: &str| {
        format!(
            "SELECT {COLUMNS} FROM matrix_event e
             WHERE e.room = ?1 AND {condition} AND {LISTED} AND {MAIN}
             ORDER BY e.pos {order} LIMIT ?3"
        )
    };
    let anchor = pos_of("?2");
    let (sql, after, newest_first) = match page {
        TimelinePage::Latest => (select("?2 IS NULL", "DESC"), None, true),
        TimelinePage::Before { event } => (
            select(&format!("e.pos < {anchor}"), "DESC"),
            Some(event.0),
            true,
        ),
        TimelinePage::After { event } => (
            select(&format!("e.pos > {anchor}"), "ASC"),
            Some(event.0),
            false,
        ),
    };
    let mut events = query_events(tx, room, &sql, params![room.0, after, limit])?;
    if newest_first {
        events.reverse();
    }
    let more_before = match events.first() {
        Some(first) => tx
            .query_row(
                &format!(
                    "SELECT 1 FROM matrix_event e
                     WHERE e.room = ?1 AND e.pos < ?2 AND {LISTED} AND {MAIN}"
                ),
                params![room.0, first.pos],
                |_| Ok(()),
            )
            .optional()?
            .is_some(),
        None => false,
    };
    // A page is the latest if nothing listed comes after it.
    let latest = match events.last() {
        Some(last) => tx
            .query_row(
                &format!(
                    "SELECT 1 FROM matrix_event e
                     WHERE e.room = ?1 AND e.pos > ?2 AND {LISTED} AND {MAIN}"
                ),
                params![room.0, last.pos],
                |_| Ok(()),
            )
            .optional()?
            .is_none(),
        None => matches!(page, TimelinePage::Latest),
    };
    let listed: HashSet<RoomEventId> = events.iter().map(|e| e.id).collect();
    let gaps = gaps(tx, room)?
        .into_iter()
        .filter(|gap| match gap.before {
            Some(before) => listed.contains(&before),
            None => latest,
        })
        .collect();
    Ok((events, more_before, gaps))
}

/// The room's gaps.
pub fn gaps(tx: &Transaction, room: RoomId) -> Result<Vec<TimelineGap>> {
    Ok(tx
        .prepare(&format!(
            "SELECT g.id,
                    (SELECT e.id FROM matrix_event e
                     WHERE e.room = g.room AND e.pos >= {before} AND {LISTED} AND {MAIN}
                     ORDER BY e.pos LIMIT 1)
             FROM matrix_gap g WHERE g.room = ?1 ORDER BY {before}",
            before = pos_of("g.before_event"),
        ))?
        .query_map([room.0], |r| {
            Ok(TimelineGap {
                id: GapId(r.get(0)?),
                before: r.get::<_, Option<i64>>(1)?.map(RoomEventId),
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// The position of the event whose id `id` (an SQL expression) is.
pub(crate) fn pos_of(id: &str) -> String {
    format!("(SELECT pos FROM matrix_event WHERE id = {id})")
}

/// Of a room `r`, whether its read marker is past the latest receipt
/// sent or observed.
pub(crate) const UNSENT_RECEIPT: &str = "(r.read_up_to IS NOT NULL AND (r.read_pushed IS NULL
    OR (SELECT pos FROM matrix_event WHERE id = r.read_up_to)
       > (SELECT pos FROM matrix_event WHERE id = r.read_pushed)))";

// ── filling gaps ──────────────────────────────────────────────────────────

/// A page of events missing from the room's timeline, fetched from the
/// homeserver before the gap `gap` (SPEC §9.2).
#[derive(Debug, Serialize)]
pub struct Filled {
    pub room: RoomId,
    pub gap: GapId,
    /// The room version, whose rules redact events.
    pub version: String,
    /// Newest first, as the homeserver pages backwards. Not in the log:
    /// they are stored.
    #[serde(skip)]
    pub events: Vec<ObservedEvent>,
    /// Whether the homeserver has no earlier events.
    pub start: bool,
}

impl Observation for Filled {
    const KIND: &'static str = "matrix.filled";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let gap: Option<(i64, i64)> = tx
            .query_row(
                "SELECT g.before_event, e.pos FROM matrix_gap g JOIN matrix_event e
                   ON e.id = g.before_event
                 WHERE g.id = ?1 AND g.room = ?2",
                params![self.gap.0, self.room.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Filled meanwhile (by another session's request).
        let Some((_, before)) = gap else {
            return Ok(Observed {
                description: "Fetched earlier events".to_owned(),
                events: vec![],
            });
        };
        let previous: Option<i64> = tx.query_row(
            "SELECT max(pos) FROM matrix_event WHERE room = ?1 AND pos < ?2",
            params![self.room.0, before],
            |r| r.get(0),
        )?;
        // Up to the first already stored, which closes the gap.
        let stored = |e: &ObservedEvent| -> Result<bool> {
            Ok(tx
                .query_row(
                    "SELECT 1 FROM matrix_event WHERE room = ?1 AND event_id = ?2",
                    params![self.room.0, e.event_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        };
        let mut missing = vec![];
        let mut closed = false;
        for e in self.events {
            if stored(&e)? {
                closed = true;
                break;
            }
            missing.push(e);
        }
        let n = missing.len() as i64;
        ensure!(
            previous.is_none_or(|previous| before - n > previous),
            "{n} events do not fit in the gap {} of room {}",
            self.gap.0,
            self.room.0
        );
        // Oldest first, just before the gap's event.
        missing.reverse();
        let positions: Vec<i64> = (0..n).map(|i| before - n + i).collect();
        let added: Vec<RoomEventId> = insert_events(tx, self.room, &missing, &positions)?
            .into_iter()
            .map(|id| id.expect("events not stored are stored"))
            .collect();
        tx.execute("DELETE FROM matrix_gap WHERE id = ?1", [self.gap.0])?;
        let left = match added.first() {
            Some(oldest) if !closed && !self.start => {
                let id = tx.query_row(
                    "INSERT INTO matrix_gap (room, before_event) VALUES (?1, ?2) RETURNING id",
                    params![self.room.0, oldest.0],
                    |r| r.get(0),
                )?;
                Some(GapId(id))
            }
            _ => None,
        };
        let redacted = redact(tx, self.room, &self.version)?;
        let touched = touched(tx, &added, &redacted)?;
        let listed: Vec<RoomEventId> = listed_events(tx, self.room, &added)?
            .into_iter()
            .map(|e| e.id)
            .collect();
        let left = left.map(|left| {
            gaps(tx, self.room).map(|gaps| {
                *gaps
                    .iter()
                    .find(|g| g.id == left)
                    .expect("the gap left is the room's")
            })
        });
        let mut events = vec![ServerEvent::TimelineFilled(TimelineFilled {
            room: self.room,
            gap: self.gap,
            events: listed,
            left: left.transpose()?,
        })];
        if !touched.is_empty() {
            events.push(ServerEvent::TimelineChanged(TimelineChanged {
                room: self.room,
                events: touched,
            }));
        }
        // Unread counts, as of the read marker.
        events.push(ServerEvent::RoomsChanged(RoomsChanged {
            rooms: vec![self.room],
        }));
        Ok(Observed {
            description: match added.len() {
                1 => "Fetched 1 earlier event".to_owned(),
                n => format!("Fetched {n} earlier events"),
            },
            events,
        })
    }
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
             ORDER BY e.pos"
        ),
        params![room.0, ids],
    )
}

/// A pattern of `LIKE ... ESCAPE '\\'` that has `text`, its own `%` and
/// `_` (and the escape) escaped.
pub(crate) fn having(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Of a message `e`, whether its text, or that of one of its edits, is
/// like `pattern` (a parameter, of `having`).
pub(crate) fn has_text(pattern: &str) -> String {
    format!(
        "(e.body LIKE {pattern} ESCAPE '\\' OR EXISTS (
           SELECT 1 FROM matrix_event x
           WHERE x.room = e.room AND x.relates_to = e.event_id AND x.rel_type = 'm.replace'
             AND x.sender = e.sender AND NOT x.redacted
             AND json_extract(x.raw_json, '$.content.\"m.new_content\".body')
                 LIKE {pattern} ESCAPE '\\'))"
    )
}

/// The room's listed messages, of the main timeline or a thread, whose
/// text, or one of their edits', has every one of `words` (ignoring the
/// case of ASCII letters, as SQLite's `LIKE`), newest first: up to
/// `limit`, before `before` if given; and whether there are more.
pub fn search(
    tx: &Transaction,
    room: RoomId,
    words: &[String],
    before: Option<RoomEventId>,
    limit: u32,
) -> Result<(Vec<RoomEvent>, bool)> {
    let patterns: Vec<String> = words.iter().map(|word| having(word)).collect();
    let matches: Vec<String> = (0..patterns.len())
        .map(|i| has_text(&format!("?{}", i + 4)))
        .collect();
    let sql = format!(
        "SELECT {COLUMNS} FROM matrix_event e
         WHERE e.room = ?1 AND e.type = 'm.room.message' AND NOT e.redacted AND {LISTED}
           AND (?2 IS NULL OR e.pos < {before})
           AND {matches}
         ORDER BY e.pos DESC LIMIT ?3",
        before = pos_of("?2"),
        matches = matches.join(" AND "),
    );
    let mut params: Vec<Box<dyn uplink_storage::rusqlite::ToSql>> = vec![
        Box::new(room.0),
        Box::new(before.map(|e| e.0)),
        Box::new(limit + 1),
    ];
    for pattern in patterns {
        params.push(Box::new(pattern));
    }
    let mut events = query_events(
        tx,
        room,
        &sql,
        uplink_storage::rusqlite::params_from_iter(params.iter()),
    )?;
    let more = events.len() > limit as usize;
    events.truncate(limit as usize);
    Ok((events, more))
}

/// Of the room's pinned events, those stored and listed, in the order
/// pinned.
pub fn pinned(tx: &Transaction, room: RoomId) -> Result<Vec<RoomEvent>> {
    query_events(
        tx,
        room,
        &format!(
            "SELECT {COLUMNS} FROM matrix_room r, json_each(r.pinned) p
             JOIN matrix_event e ON e.room = r.id AND e.event_id = p.value
             WHERE r.id = ?1 AND {LISTED}
             ORDER BY p.key"
        ),
        params![room.0],
    )
}

/// The thread whose root is `root`, if the main timeline lists it: the
/// root, and the thread's messages, in timeline order.
pub fn thread(
    tx: &Transaction,
    room: RoomId,
    root: RoomEventId,
) -> Result<Option<(RoomEvent, Vec<RoomEvent>)>> {
    let Some(root) = query_events(
        tx,
        room,
        &format!(
            "SELECT {COLUMNS} FROM matrix_event e
             WHERE e.room = ?1 AND e.id = ?2 AND {LISTED} AND {MAIN}"
        ),
        params![room.0, root.0],
    )?
    .pop() else {
        return Ok(None);
    };
    let replies = query_events(
        tx,
        room,
        &format!(
            "SELECT {COLUMNS} FROM matrix_event e
             WHERE e.room = ?1 AND e.rel_type = 'm.thread' AND e.relates_to = ?2 AND {LISTED}
             ORDER BY e.pos"
        ),
        params![room.0, root.event_id],
    )?;
    Ok(Some((root, replies)))
}

/// The latest message of the thread whose root is `root` (a Matrix event
/// id), else the root: what a message sent to it answers by fallback.
pub fn thread_latest(conn: &Connection, room: RoomId, root: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT event_id FROM matrix_event
             WHERE room = ?1 AND rel_type = 'm.thread' AND relates_to = ?2
             ORDER BY pos DESC LIMIT 1",
            params![room.0, root],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| root.to_owned()))
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
    let mut thread = tx.prepare(&format!(
        "SELECT count(*),
                (SELECT e.event_id FROM matrix_event e
                 WHERE e.room = t.room AND e.relates_to = t.event_id AND e.rel_type = 'm.thread'
                   AND {LISTED}
                 ORDER BY e.pos DESC LIMIT 1)
         FROM matrix_event t JOIN matrix_event e ON e.room = t.room AND e.relates_to = t.event_id
         WHERE t.id = ?1 AND e.rel_type = 'm.thread' AND {LISTED}"
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
        let mut summed = |event_id: &str| {
            replied
                .query_row(params![room.0, event_id], |r| {
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
                .optional()
        };
        if let Some(reply_to) = reply_to {
            event.replied = summed(&reply_to)?;
        }
        let (replies, latest): (u32, Option<String>) =
            thread.query_row([event.id.0], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if let Some(latest) = latest {
            event.thread = Some(ThreadSummary {
                replies,
                latest: summed(&latest)?.expect("the thread's latest is stored"),
            });
        }
        events.push(event);
    }
    Ok(events)
}
