//! What of the Matrix accounts is not yet reconciled with their
//! homeservers (SPEC §6.6): messages being sent, rooms being joined or
//! left, read receipts to send; and why an account's changes are not
//! reaching its homeserver.

use crate::{
    membership::parse_desired,
    rooms::{ACCOUNT_KIND, UNSENT_RECEIPT},
    send,
};
use anyhow::Result;
use std::collections::BTreeMap;
use uplink_protocol::{
    compose::{AccountOutbox, Failing, OutboxChanged, Pending, PendingOp},
    matrix::{MatrixSend, PendingMembership, RoomId},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{Connection, Transaction};

#[derive(Default)]
struct MatrixOutbox {
    failing: Option<Failing>,
    sends: Vec<MatrixSend>,
    memberships: Vec<PendingMembership>,
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
            "SELECT r.account_id, r.id, r.desired, r.membership_error, t.id, t.description, t.ts
             FROM matrix_room r LEFT JOIN tx_log t ON t.id = r.membership_by
             WHERE r.desired IS NOT NULL
             ORDER BY r.account_id, r.membership_by DESC",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                PendingMembership {
                    room: RoomId(r.get(1)?),
                    change: parse_desired(Some(&r.get::<_, String>(2)?)).expect("desired is set"),
                    // Set with `desired`: an error if not.
                    op: PendingOp {
                        id: TxLogId(r.get(4)?),
                        description: r.get(5)?,
                        ts: r.get(6)?,
                    },
                    refused: r.get(3)?,
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    for (account, membership) in memberships {
        accounts
            .entry(account)
            .or_default()
            .memberships
            .push(membership);
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
                receipts: outbox.receipts,
            },
        })
        .collect())
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

/// What of the account's rooms is pending: their memberships asked for
/// (and refusals) and receipts to send. Taken before and after a change,
/// it tells whether the account's outbox changed.
#[derive(Debug, PartialEq)]
pub(crate) struct PendingRooms(Vec<(i64, Option<String>, Option<String>, bool)>);

pub(crate) fn pending_rooms(conn: &Connection, account: &str) -> Result<PendingRooms> {
    Ok(PendingRooms(
        conn.prepare(&format!(
            "SELECT r.id, r.desired, r.membership_error,
                    r.membership = 'joined' AND {UNSENT_RECEIPT} AS receipt
             FROM matrix_room r
             WHERE r.account_id = ?1 AND (r.desired IS NOT NULL OR receipt)
             ORDER BY r.id"
        ))?
        .query_map([account], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?,
    ))
}

pub(crate) fn outbox_changed(account: &str) -> ServerEvent {
    ServerEvent::OutboxChanged(OutboxChanged {
        accounts: vec![account.to_owned()],
    })
}
