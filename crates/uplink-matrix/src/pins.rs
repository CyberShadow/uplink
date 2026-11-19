//! A room's pinned events (`m.room.pinned_events`, SPEC §9.5): a list the
//! room's members share, which a client writes whole. Pinning is desired
//! state (§6.6), a set of one field per event (§6.6.4): pinning or
//! unpinning records the pin asked for, and the account's sync task sets
//! the room's list on the homeserver, built from uplink's own state —
//! each event as uplink last pushed it, else as observed — never from the
//! SDK's store, which lags uplink's own pushes.
//!
//! The list keeps the homeserver's order: each event pinned there has its
//! place (`pos`) as last observed; one pinned since goes after them, in
//! the order asked for.

use anyhow::{Context, Result};
use matrix_sdk::ruma::{
    api::{
        client::state::{get_state_event_for_key, send_state_event},
        error::ErrorKind,
    },
    events::{AnyStateEventContent, StateEventType},
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use uplink_protocol::{
    matrix::{PinChange, RoomId, RoomsChanged},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{
    reconcile::{Ended, Field},
    this_entry, Applied, Conflict, Inverse, Op,
};

use crate::{
    outbox,
    reconciler::{Homeserver, Kind, Requests, RoomIdRef, Stored},
};

/// Whether an event is pinned (SPEC §6.6): had (`observed`) or asked for
/// (`desired`).
pub type Pin = Field<bool>;

/// The order the room's pins are shown and pushed in, of
/// `matrix_room_pin p`: as the homeserver has them, then those pinned
/// since, as asked for.
macro_rules! order {
    () => {
        "p.pos IS NULL, p.pos, p.desired_by, p.event"
    };
}
pub(crate) const ORDER: &str = order!();

/// The room's pinned events as shown: as asked for. Of `matrix_room r`;
/// a JSON array, in order.
pub(crate) const SHOWN: &str = concat!(
    "(SELECT json_group_array(event) FROM
       (SELECT p.event FROM matrix_room_pin p WHERE p.room = r.id AND p.desired ORDER BY ",
    order!(),
    "))"
);

/// A pin as stored: its field, and its place in the homeserver's list.
struct Row {
    field: Pin,
    pos: Option<i64>,
}

/// The room's pin of `event` as stored, if it has a row; else as known of
/// every event it has none of: neither pinned nor asked to be.
fn load(conn: &Connection, room: RoomId, event: &str) -> Result<(Row, bool)> {
    let stored: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT field, pos FROM matrix_room_pin WHERE room = ?1 AND event = ?2",
            params![room.0, event],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match stored {
        Some((json, pos)) => (
            Row {
                field: parse_field(&json)?,
                pos,
            },
            true,
        ),
        None => (
            Row {
                field: Field::new(false, known_at(conn, room)?),
                pos: None,
            },
            false,
        ),
    })
}

fn parse_field(json: &str) -> Result<Pin> {
    serde_json::from_str(json).with_context(|| format!("a pin's field: {json}"))
}

/// What is known of the room's pins it has no row of, since when.
fn known_at(conn: &Connection, room: RoomId) -> Result<TxLogId> {
    Ok(TxLogId(conn.query_row(
        "SELECT pins_known_at FROM matrix_room WHERE id = ?1",
        [room.0],
        |r| r.get(0),
    )?))
}

/// Stores the pin's field, its place kept, or none if new.
fn store(tx: &Transaction, room: RoomId, event: &str, field: &Pin) -> Result<()> {
    tx.execute(
        "INSERT INTO matrix_room_pin (room, event, field) VALUES (?1, ?2, ?3)
         ON CONFLICT (room, event) DO UPDATE SET field = excluded.field",
        params![room.0, event, serde_json::to_string(field)?],
    )?;
    Ok(())
}

/// Records the room's pinned events, in their order, as observed by a
/// request sent after `as_of` (SPEC §6.6.2); returns whether those shown
/// changed.
pub(crate) fn observe(
    tx: &Transaction,
    room: RoomId,
    pinned: &[String],
    as_of: TxLogId,
) -> Result<bool> {
    let at = this_entry(tx)?;
    let stored: Vec<String> = tx
        .prepare("SELECT event FROM matrix_room_pin WHERE room = ?1")?
        .query_map([room.0], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut unstored = Field::new(false, known_at(tx, room)?);
    unstored.observe(false, as_of, at)?;
    let mut changed = false;
    let listed: BTreeSet<&String> = pinned.iter().collect();
    let new = listed.iter().copied().filter(|e| !stored.contains(e));
    for event in stored.iter().chain(new) {
        let (Row { mut field, pos }, had_row) = load(tx, room, event)?;
        let (shown, shown_at) = (field.desired, pos);
        field.observe(listed.contains(event), as_of, at)?;
        // Its place, if the observation stood; else as it was.
        let pos = match (field.observed, pinned.iter().position(|e| e == event)) {
            (true, Some(at)) => Some(i64::try_from(at)?),
            (true, None) => pos,
            (false, _) => None,
        };
        changed |= field.desired != shown || field.desired && pos != shown_at;
        // A pin of no row stays so if it is as all of them are.
        if field == unstored {
            if had_row {
                tx.execute(
                    "DELETE FROM matrix_room_pin WHERE room = ?1 AND event = ?2",
                    params![room.0, event],
                )?;
            }
            continue;
        }
        tx.execute(
            "INSERT INTO matrix_room_pin (room, event, field, pos) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (room, event) DO UPDATE SET field = excluded.field, pos = excluded.pos",
            params![room.0, event, serde_json::to_string(&field)?, pos],
        )?;
    }
    tx.execute(
        "UPDATE matrix_room SET pins_known_at = ?2 WHERE id = ?1",
        params![room.0, unstored.known_at.0],
    )?;
    Ok(changed)
}

// ── ops ───────────────────────────────────────────────────────────────────

/// Pins events of rooms, or unpins them. Changes already in effect are
/// skipped, so the inverse reverts exactly what this op changed (§6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPins {
    pub changes: Vec<PinChange>,
}

impl Op for SetPins {
    const KIND: &'static str = "matrix.set_pins";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let this = this_entry(tx)?;
        let mut pending = BTreeMap::new();
        let mut inverse = vec![];
        let mut rooms = BTreeSet::new();
        for change in self.changes {
            let account: String = tx
                .query_row(
                    "SELECT account_id FROM matrix_room WHERE id = ?1",
                    [change.room.0],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Conflict("the room is no longer stored".into()))?;
            if let Entry::Vacant(vacant) = pending.entry(account) {
                let before = outbox::pending_rooms(tx, vacant.key())?;
                vacant.insert(before);
            }
            let (Row { mut field, .. }, _) = load(tx, change.room, &change.event)?;
            let pinned = field.desired;
            if pinned == change.pinned {
                continue;
            }
            field.want(change.pinned, this);
            store(tx, change.room, &change.event, &field)?;
            rooms.insert(change.room);
            inverse.push(PinChange { pinned, ..change });
        }
        let mut events = vec![];
        if !rooms.is_empty() {
            events.push(ServerEvent::RoomsChanged(RoomsChanged {
                rooms: rooms.into_iter().collect(),
            }));
        }
        for (account, before) in pending {
            if outbox::pending_rooms(tx, &account)? != before {
                events.push(outbox::outbox_changed(&account));
            }
        }
        // As the changes made, the inverse's being those undone.
        let pinned = inverse.iter().filter(|c| !c.pinned).count();
        let unpinned = inverse.len() - pinned;
        Ok(Applied {
            description: match (pinned, unpinned) {
                (0, 0) => "Changed nothing".to_owned(),
                (1, 0) => "Pinned a message".to_owned(),
                (0, 1) => "Unpinned a message".to_owned(),
                (n, 0) => format!("Pinned {n} messages"),
                (0, n) => format!("Unpinned {n} messages"),
                (p, u) => format!("Pinned {p} messages and unpinned {u}"),
            },
            inverse: Inverse::of(&SetPins { changes: inverse }),
            events,
        })
    }
}

// ── the reconciler's ──────────────────────────────────────────────────────

/// A room's pinned events, of which each is a field, keyed by the event.
pub struct Pins;

impl Kind for Pins {
    type Value = bool;
    /// The events pinned, in order.
    type Read = Vec<String>;
    /// The room's list, whole.
    type Request = Vec<String>;

    const CLAIMED: &'static str = "matrix.pins_claimed";
    const PUSHED: &'static str = "matrix.pins_set";
    const READ: &'static str = "matrix.pins_read";
    const SETTLED: &'static str = "matrix.pin_pushes_settled";
    const WHAT: &'static str = "pins";

    fn load(conn: &Connection, room: RoomId, event: &str) -> Result<Pin> {
        Ok(load(conn, room, event)?.0.field)
    }

    fn store(tx: &Transaction, room: RoomId, event: &str, field: &Pin) -> Result<()> {
        store(tx, room, event, field)
    }

    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored<bool>>> {
        conn.prepare(
            "SELECT r.id, r.room_id, p.event, p.field
             FROM matrix_room_pin p JOIN matrix_room r ON r.id = p.room
             WHERE r.account_id = ?1
               AND (p.desired_by IS NOT NULL OR json_extract(p.field, '$.check') IS NOT NULL)
             ORDER BY r.id, p.event",
        )?
        .query_map([account], |r| {
            Ok((
                RoomId(r.get(0)?),
                r.get(1)?,
                r.get(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .map(|row| {
            let (room, room_id, key, json) = row?;
            Ok(Stored {
                room,
                room_id,
                key,
                field: parse_field(&json)?,
            })
        })
        .collect()
    }

    fn observe(
        tx: &Transaction,
        room: RoomId,
        pinned: &Vec<String>,
        as_of: TxLogId,
    ) -> Result<bool> {
        observe(tx, room, pinned, as_of)
    }

    fn pushed(room: &str, _event: &str, pinned: &bool, ended: &Ended) -> String {
        let verb = if *pinned { "pin" } else { "unpin" };
        match ended {
            Ended::Applied if *pinned => format!("Pinned a message of '{room}' on the homeserver"),
            Ended::Applied => format!("Unpinned a message of '{room}' on the homeserver"),
            Ended::NotApplied {
                refused: Some(error),
            } => format!("The homeserver refused to {verb} a message of '{room}': {error}"),
            Ended::NotApplied { refused: None } => {
                format!("Could not {verb} a message of '{room}' on the homeserver")
            }
            Ended::Unknown { .. } => {
                format!("Whether the homeserver did {verb} a message of '{room}' is unknown")
            }
        }
    }

    /// One per room: its list, of every event as uplink last pushed it,
    /// else as observed.
    fn requests(
        conn: &Connection,
        in_flight: Vec<Stored<bool>>,
    ) -> Result<Requests<Vec<String>, bool>> {
        let mut by_room: BTreeMap<RoomId, Vec<Stored<bool>>> = BTreeMap::new();
        for stored in in_flight {
            by_room.entry(stored.room).or_default().push(stored);
        }
        by_room
            .into_iter()
            .map(|(room, fields)| {
                let pins: Vec<(String, String)> = conn
                    .prepare(&format!(
                        "SELECT p.event, p.field FROM matrix_room_pin p WHERE p.room = ?1
                         ORDER BY {ORDER}"
                    ))?
                    .query_map([room.0], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?;
                let mut list = vec![];
                for (event, json) in pins {
                    if *parse_field(&json)?.as_pushed() {
                        list.push(event);
                    }
                }
                Ok((list, fields))
            })
            .collect()
    }

    async fn push(
        homeserver: &Homeserver,
        room: &RoomIdRef,
        pinned: &Vec<String>,
    ) -> matrix_sdk::Result<()> {
        let content =
            serde_json::value::to_raw_value(&json!({ "pinned": pinned })).expect("JSON serializes");
        let request = send_state_event::v3::Request::new_raw(
            room.to_owned(),
            StateEventType::RoomPinnedEvents,
            String::new(),
            Raw::<AnyStateEventContent>::from_json(content),
        );
        homeserver.client.send(request).await?;
        Ok(())
    }

    async fn read(homeserver: &Homeserver, room: &RoomIdRef, _key: &str) -> Result<Vec<String>> {
        let request = get_state_event_for_key::v3::Request::new(
            room.to_owned(),
            StateEventType::RoomPinnedEvents,
            String::new(),
        );
        match homeserver.client.send(request).await {
            Ok(response) => {
                let content: serde_json::Value =
                    serde_json::from_str(response.event_or_content.get())?;
                Ok(parse(&content))
            }
            // Never pinned.
            Err(e) if e.client_api_error_kind() == Some(&ErrorKind::NotFound) => Ok(vec![]),
            Err(e) => Err(e).context("reading a room's pinned events"),
        }
    }
}

/// The events of `m.room.pinned_events`'s `content`, in order; those not
/// strings left out.
pub fn parse(content: &serde_json::Value) -> Vec<String> {
    content["pinned"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e.as_str().map(str::to_owned))
        .collect()
}

/// A pin pending, for the outbox.
pub(crate) struct Pending {
    pub account: String,
    /// The op that asked.
    pub op: TxLogId,
    pub change: PinChange,
    /// Why it needs a decision, if it does.
    pub refused: Option<String>,
}

/// The pins pending, by account, then by the op that asked, newest first.
pub(crate) fn pending(conn: &Connection) -> Result<Vec<Pending>> {
    conn.prepare(
        "SELECT r.account_id, p.room, p.event, p.field
         FROM matrix_room_pin p JOIN matrix_room r ON r.id = p.room
         WHERE p.desired_by IS NOT NULL
         ORDER BY r.account_id, p.desired_by DESC, p.room, p.event",
    )?
    .query_map([], |r| {
        Ok((
            r.get(0)?,
            RoomId(r.get(1)?),
            r.get(2)?,
            r.get::<_, String>(3)?,
        ))
    })?
    .map(|row| {
        let (account, room, event, json) = row?;
        let field = parse_field(&json)?;
        Ok(Pending {
            account,
            op: field.opinion.context("pending without an opinion")?,
            change: PinChange {
                room,
                event,
                pinned: field.desired,
            },
            refused: field.decision.as_ref().map(|d| d.reason().to_owned()),
        })
    })
    .collect()
}
