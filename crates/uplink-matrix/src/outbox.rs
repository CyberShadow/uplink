//! What of the Matrix accounts is not yet reconciled with their
//! homeservers (SPEC §6.6): messages being sent, rooms being joined or
//! left, spaces being reordered, rooms being tagged, events being pinned,
//! read receipts to send; and why an account's changes are not reaching
//! its homeserver.

use crate::{
    membership, pins, room_tags,
    rooms::{ACCOUNT_KIND, UNSENT_RECEIPT},
    send, space_order,
};
use anyhow::Result;
use std::collections::BTreeMap;
use uplink_protocol::{
    compose::{AccountOutbox, Failing, OutboxChanged, Pending, PendingOp},
    matrix::{
        MatrixSend, PendingMembership, PendingPins, PendingRoomTags, PendingSpaceOrders, RoomId,
    },
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{Connection, Transaction};

#[derive(Default)]
struct MatrixOutbox {
    failing: Option<Failing>,
    sends: Vec<MatrixSend>,
    memberships: Vec<PendingMembership>,
    space_orders: Vec<PendingSpaceOrders>,
    room_tags: Vec<PendingRoomTags>,
    pins: Vec<PendingPins>,
    receipts: Vec<RoomId>,
}

/// Each Matrix account with anything not yet reconciled, in id order.
pub fn outbox(tx: &Transaction) -> Result<Vec<AccountOutbox>> {
    let mut accounts: BTreeMap<String, MatrixOutbox> = BTreeMap::new();
    for (account, failing) in uplink_tx_log::push::failing(tx, &[ACCOUNT_KIND])? {
        accounts.entry(account).or_default().failing = Some(failing);
    }
    for (account, send) in send::unreconciled(tx)? {
        accounts.entry(account).or_default().sends.push(send);
    }
    let memberships: Vec<(String, PendingMembership)> = tx
        .prepare(
            "SELECT r.account_id, r.id, r.membership_field, t.id, t.description, t.ts
             FROM matrix_room r JOIN tx_log t ON t.id = r.membership_by
             ORDER BY r.account_id, r.membership_by DESC",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                RoomId(r.get(1)?),
                r.get::<_, String>(2)?,
                PendingOp {
                    id: TxLogId(r.get(3)?),
                    description: r.get(4)?,
                    ts: r.get(5)?,
                },
            ))
        })?
        .map(|row| {
            let (account, room, field, op) = row?;
            let field = membership::parse_field(&field)?;
            Ok((
                account,
                PendingMembership {
                    room,
                    change: membership::change_to(field.desired),
                    op,
                    refused: field.decision.as_ref().map(|d| d.reason().to_owned()),
                },
            ))
        })
        .collect::<Result<_>>()?;
    for (account, membership) in memberships {
        accounts
            .entry(account)
            .or_default()
            .memberships
            .push(membership);
    }
    for (account, orders) in space_orders(tx)? {
        accounts
            .entry(account)
            .or_default()
            .space_orders
            .push(orders);
    }
    for (account, tags) in tag_changes(tx)? {
        accounts.entry(account).or_default().room_tags.push(tags);
    }
    for (account, pins) in pin_changes(tx)? {
        accounts.entry(account).or_default().pins.push(pins);
    }
    for (account, room) in unsent_receipts(tx)? {
        accounts.entry(account).or_default().receipts.push(room);
    }
    Ok(accounts
        .into_iter()
        .map(|(account, outbox)| AccountOutbox {
            account,
            failing: outbox.failing,
            pending: Pending::Matrix {
                sends: outbox.sends,
                memberships: outbox.memberships,
                space_orders: outbox.space_orders,
                room_tags: outbox.room_tags,
                pins: outbox.pins,
                receipts: outbox.receipts,
            },
        })
        .collect())
}

/// The spaces being reordered, by the op that asked, newest first, with
/// their accounts.
fn space_orders(conn: &Connection) -> Result<Vec<(String, PendingSpaceOrders)>> {
    let rows: Vec<(String, i64, i64, String, i64, String)> = conn
        .prepare(
            "SELECT r.account_id, r.id, t.id, t.description, t.ts, r.space_order_field
             FROM matrix_room r JOIN tx_log t ON t.id = r.space_order_by
             ORDER BY r.account_id, t.id DESC, r.id",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut orders: Vec<(String, PendingSpaceOrders)> = vec![];
    for (account, room, op, description, ts, field) in rows {
        let field = space_order::parse_field(&field)?;
        let refused = field.decision.as_ref().map(|d| d.reason().to_owned());
        let same = orders.last().is_some_and(|(_, last)| last.op.id.0 == op);
        if !same {
            orders.push((
                account,
                PendingSpaceOrders {
                    op: PendingOp {
                        id: TxLogId(op),
                        description,
                        ts,
                    },
                    spaces: vec![],
                    refused: None,
                },
            ));
        }
        let (_, last) = orders.last_mut().expect("pushed");
        last.spaces.push(RoomId(room));
        last.refused = last.refused.take().or(refused);
    }
    Ok(orders)
}

/// The rooms being tagged, by the op that asked, newest first, with their
/// accounts.
fn tag_changes(tx: &Transaction) -> Result<Vec<(String, PendingRoomTags)>> {
    let mut tags: Vec<(String, PendingRoomTags)> = vec![];
    for room_tags::Pending {
        account,
        op,
        change,
        refused,
    } in room_tags::pending(tx)?
    {
        if tags.last().is_none_or(|(_, last)| last.op.id != op) {
            let (description, ts) = tx.query_row(
                "SELECT description, ts FROM tx_log WHERE id = ?1",
                [op.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            tags.push((
                account,
                PendingRoomTags {
                    op: PendingOp {
                        id: op,
                        description,
                        ts,
                    },
                    changes: vec![],
                    refused: None,
                },
            ));
        }
        let (_, last) = tags.last_mut().expect("pushed");
        last.changes.push(change);
        last.refused = last.refused.take().or(refused);
    }
    Ok(tags)
}

/// The events being pinned or unpinned, by the op that asked, newest
/// first, with their accounts.
fn pin_changes(tx: &Transaction) -> Result<Vec<(String, PendingPins)>> {
    let mut pins: Vec<(String, PendingPins)> = vec![];
    for pins::Pending {
        account,
        op,
        change,
        refused,
    } in pins::pending(tx)?
    {
        if pins.last().is_none_or(|(_, last)| last.op.id != op) {
            let (description, ts) = tx.query_row(
                "SELECT description, ts FROM tx_log WHERE id = ?1",
                [op.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            pins.push((
                account,
                PendingPins {
                    op: PendingOp {
                        id: op,
                        description,
                        ts,
                    },
                    changes: vec![],
                    refused: None,
                },
            ));
        }
        let (_, last) = pins.last_mut().expect("pushed");
        last.changes.push(change);
        last.refused = last.refused.take().or(refused);
    }
    Ok(pins)
}

/// The joined rooms read past their latest receipt, with their accounts.
fn unsent_receipts(conn: &Connection) -> Result<Vec<(String, RoomId)>> {
    Ok(conn
        .prepare(&format!(
            "SELECT r.account_id, r.id FROM matrix_room r
                 WHERE r.membership = 'joined' AND {UNSENT_RECEIPT}
                 ORDER BY r.account_id, r.id"
        ))?
        .query_map([], |r| Ok((r.get(0)?, RoomId(r.get(1)?))))?
        .collect::<Result<_, _>>()?)
}

/// What of the account's rooms is pending: their memberships, space
/// orders, tags and pins asked for (and refusals) and receipts to send.
/// Taken before and after a change, it tells whether the account's
/// outbox changed.
#[derive(Debug, PartialEq)]
pub(crate) struct PendingRooms(Vec<PendingRoom>, Vec<PendingKeyed>, Vec<PendingKeyed>);

/// A room's tag or pin: `(room, key, desired, desired_by, decision)`.
type PendingKeyed = (i64, String, bool, i64, Option<String>);

type PendingRoom = (
    i64,
    Option<String>,
    Option<String>,
    bool,
    Option<i64>,
    Option<String>,
);

pub(crate) fn pending_rooms(conn: &Connection, account: &str) -> Result<PendingRooms> {
    let tags = conn
        .prepare(
            "SELECT t.room, t.tag, t.desired, t.desired_by, t.decision
             FROM matrix_room_tag t JOIN matrix_room r ON r.id = t.room
             WHERE r.account_id = ?1 AND t.desired_by IS NOT NULL
             ORDER BY t.room, t.tag",
        )?
        .query_map([account], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    let pins = conn
        .prepare(
            "SELECT p.room, p.event, p.desired, p.desired_by, p.decision
             FROM matrix_room_pin p JOIN matrix_room r ON r.id = p.room
             WHERE r.account_id = ?1 AND p.desired_by IS NOT NULL
             ORDER BY p.room, p.event",
        )?
        .query_map([account], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(PendingRooms(
        conn.prepare(&format!(
            "SELECT r.id, r.desired, r.membership_decision,
                    r.membership = 'joined' AND {UNSENT_RECEIPT} AS receipt,
                    r.space_order_by, r.space_order_decision
             FROM matrix_room r
             WHERE r.account_id = ?1
               AND (r.desired IS NOT NULL OR receipt OR r.space_order_by IS NOT NULL)
             ORDER BY r.id"
        ))?
        .query_map([account], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?,
        tags,
        pins,
    ))
}

pub(crate) fn outbox_changed(account: &str) -> ServerEvent {
    ServerEvent::OutboxChanged(OutboxChanged {
        accounts: vec![account.to_owned()],
    })
}
