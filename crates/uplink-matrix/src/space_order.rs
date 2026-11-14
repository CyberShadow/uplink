//! Where the user puts each space among the top-level spaces (MSC3230,
//! SPEC §9.5): the `order` of the space's `org.matrix.msc3230.space_order`
//! room account data, as Element keeps it. It is desired state (§6.6), a
//! field of each room: reordering records the orders asked for, and the
//! account's sync task sets them on the homeserver. Undoing puts back the
//! orders replaced, unless they changed since.

use anyhow::{bail, ensure, Context, Result};
use matrix_sdk::ruma::{
    api::{
        client::config::{get_room_account_data, set_room_account_data},
        error::ErrorKind,
    },
    events::{AnyRoomAccountDataEventContent, RoomAccountDataEventType},
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uplink_protocol::{
    matrix::{RoomId, RoomsChanged, SpaceOrderChange},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{
    reconcile::{Ended, Field},
    this_entry, Applied, Conflict, Inverse, Op,
};

use crate::{
    outbox,
    reconciler::{self, Homeserver, Kind, Requests, RoomIdRef, Stored},
};

/// The room account data's event type, as Element names it.
pub const EVENT_TYPE: &str = "org.matrix.msc3230.space_order";

/// Whether `order` is one MSC3230 allows (as `m.space.child`'s `order`):
/// at most 50 characters, from `\x20` to `\x7E`.
pub fn valid(order: &str) -> bool {
    order.len() <= 50 && order.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// The order of the room account data's `content`, if valid.
pub fn parse(content: &serde_json::Value) -> Option<String> {
    content["order"]
        .as_str()
        .filter(|order| valid(order))
        .map(str::to_owned)
}

/// The room's order as shown: as asked for. Of `matrix_room r`.
pub(crate) const SHOWN: &str = "r.space_order_desired";

/// A space's order (SPEC §6.6): had (`observed`) or asked for
/// (`desired`), `None` for none.
pub type Order = Field<Option<String>>;

pub(crate) fn parse_field(json: &str) -> Result<Order> {
    serde_json::from_str(json).with_context(|| format!("a space's order field: {json}"))
}

fn load(conn: &Connection, room: RoomId) -> Result<Order> {
    let json: String = conn
        .query_row(
            "SELECT space_order_field FROM matrix_room WHERE id = ?1",
            [room.0],
            |r| r.get(0),
        )
        .with_context(|| format!("no room {}", room.0))?;
    parse_field(&json)
}

fn store(tx: &Transaction, room: RoomId, field: &Order) -> Result<()> {
    tx.execute(
        "UPDATE matrix_room SET space_order_field = ?2 WHERE id = ?1",
        params![room.0, serde_json::to_string(field)?],
    )?;
    Ok(())
}

/// Records the room's order as observed by a request sent after `as_of`
/// (SPEC §6.6.2); returns whether the order shown changed.
pub(crate) fn observe(
    tx: &Transaction,
    room: RoomId,
    order: Option<String>,
    as_of: TxLogId,
) -> Result<bool> {
    let mut field = load(tx, room)?;
    let shown = field.desired.clone();
    field.observe(order, as_of, this_entry(tx)?)?;
    store(tx, room, &field)?;
    Ok(field.desired != shown)
}

/// Reorders spaces: each from its order as it is to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderSpaces {
    pub changes: Vec<SpaceOrderChange>,
}

impl Op for OrderSpaces {
    const KIND: &'static str = "matrix.order_spaces";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        ensure!(!self.changes.is_empty(), "no spaces to reorder");
        let by = this_entry(tx)?;
        let mut account = None;
        let mut names = vec![];
        for change in &self.changes {
            ensure!(
                change.to.as_deref().is_none_or(valid),
                "invalid order {:?}",
                change.to
            );
            let (of, name, shown): (String, String, Option<String>) = tx
                .query_row(
                    &format!(
                        "SELECT r.account_id, r.name, {SHOWN} FROM matrix_room r WHERE r.id = ?1"
                    ),
                    [change.space.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .with_context(|| format!("no room {}", change.space.0))?;
            ensure!(
                *account.get_or_insert_with(|| of.clone()) == of,
                "spaces of different accounts reordered together"
            );
            if shown != change.from {
                bail!(Conflict(format!("'{name}' moved since")));
            }
            names.push(name);
        }
        let account = account.expect("there are changes");
        let pending = outbox::pending_rooms(tx, &account)?;
        for change in &self.changes {
            let mut field = load(tx, change.space)?;
            field.want(change.to.clone(), by);
            store(tx, change.space, &field)?;
        }
        let mut events = vec![ServerEvent::RoomsChanged(RoomsChanged {
            rooms: self.changes.iter().map(|c| c.space).collect(),
        })];
        if outbox::pending_rooms(tx, &account)? != pending {
            events.push(outbox::outbox_changed(&account));
        }
        Ok(Applied {
            description: match &names[..] {
                [name] => format!("Moved '{name}'"),
                names => format!("Reordered {} spaces", names.len()),
            },
            inverse: Inverse::of(&OrderSpaces {
                changes: self
                    .changes
                    .into_iter()
                    .map(|c| SpaceOrderChange {
                        space: c.space,
                        from: c.to,
                        to: c.from,
                    })
                    .collect(),
            }),
            events,
        })
    }
}

/// Spaces' orders, a field of each room.
pub struct Orders;

impl Kind for Orders {
    type Value = Option<String>;
    /// The room's order.
    type Read = Option<String>;
    type Request = (String, Option<String>);

    fn requests(
        _conn: &Connection,
        in_flight: Vec<Stored<Option<String>>>,
    ) -> Result<Requests<Self::Request, Option<String>>> {
        Ok(reconciler::each(in_flight))
    }

    const CLAIMED: &'static str = "matrix.space_orders_claimed";
    const PUSHED: &'static str = "matrix.space_order_set";
    const READ: &'static str = "matrix.space_order_read";
    const SETTLED: &'static str = "matrix.space_order_pushes_settled";
    const WHAT: &'static str = "space orders";

    fn load(conn: &Connection, room: RoomId, _key: &str) -> Result<Order> {
        load(conn, room)
    }

    fn store(tx: &Transaction, room: RoomId, _key: &str, field: &Order) -> Result<()> {
        store(tx, room, field)
    }

    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored<Option<String>>>> {
        conn.prepare(
            "SELECT id, room_id, space_order_field FROM matrix_room
             WHERE account_id = ?1
               AND (space_order_by IS NOT NULL
                    OR json_extract(space_order_field, '$.check') IS NOT NULL)
             ORDER BY id",
        )?
        .query_map([account], |r| {
            Ok((RoomId(r.get(0)?), r.get(1)?, r.get::<_, String>(2)?))
        })?
        .map(|row| {
            let (room, room_id, json) = row?;
            Ok(Stored {
                room,
                room_id,
                key: String::new(),
                field: parse_field(&json)?,
            })
        })
        .collect()
    }

    fn observe(
        tx: &Transaction,
        room: RoomId,
        order: &Option<String>,
        as_of: TxLogId,
    ) -> Result<bool> {
        observe(tx, room, order.clone(), as_of)
    }

    fn pushed(room: &str, _key: &str, _order: &Option<String>, ended: &Ended) -> String {
        match ended {
            Ended::Applied => format!("Ordered '{room}' on the homeserver"),
            Ended::NotApplied {
                refused: Some(error),
            } => format!("The homeserver refused to order '{room}': {error}"),
            Ended::NotApplied { refused: None } => {
                format!("Could not order '{room}' on the homeserver")
            }
            Ended::Unknown { .. } => {
                format!("Whether the homeserver ordered '{room}' is unknown")
            }
        }
    }

    async fn push(
        homeserver: &Homeserver,
        room: &RoomIdRef,
        (_, order): &(String, Option<String>),
    ) -> matrix_sdk::Result<()> {
        // No order is none in the content, as Element sets it.
        let content = match order {
            Some(order) => json!({ "order": order }),
            None => json!({}),
        };
        let content = serde_json::value::to_raw_value(&content).expect("JSON serializes");
        let request = set_room_account_data::v3::Request::new_raw(
            homeserver.user.clone(),
            room.to_owned(),
            RoomAccountDataEventType::from(EVENT_TYPE),
            Raw::<AnyRoomAccountDataEventContent>::from_json(content),
        );
        homeserver.client.send(request).await?;
        Ok(())
    }

    async fn read(homeserver: &Homeserver, room: &RoomIdRef) -> Result<Option<String>> {
        let request = get_room_account_data::v3::Request::new(
            homeserver.user.clone(),
            room.to_owned(),
            RoomAccountDataEventType::from(EVENT_TYPE),
        );
        match homeserver.client.send(request).await {
            Ok(response) => {
                let content: serde_json::Value =
                    serde_json::from_str(response.account_data.json().get())?;
                Ok(parse(&content))
            }
            // Never set.
            Err(e) if e.client_api_error_kind() == Some(&ErrorKind::NotFound) => Ok(None),
            Err(e) => Err(e).context("reading a space's order"),
        }
    }
}
