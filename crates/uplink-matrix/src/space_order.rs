//! Where the user puts each space among the top-level spaces (MSC3230,
//! SPEC §9.5): the `order` of the space's `org.matrix.msc3230.space_order`
//! room account data, as Element keeps it. It is desired state (§6.6):
//! reordering records the orders asked for, and the account's sync task
//! sets them on the homeserver. Undoing puts back the orders replaced,
//! unless they changed since.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    matrix::{RoomId, RoomsChanged, SpaceOrderChange},
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{this_entry, Applied, Conflict, Inverse, Observation, Observed, Op};

use crate::outbox;

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

/// The room's order as shown: as asked for while being changed, else as
/// the homeserver has it. Of `matrix_room r`.
pub(crate) const SHOWN: &str =
    "CASE WHEN r.space_order_by IS NULL THEN r.space_order ELSE r.space_order_desired END";

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
        let by = this_entry(tx)?.0;
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
            tx.execute(
                "UPDATE matrix_room
                 SET space_order_desired = ?1, space_order_by = ?2, space_order_error = NULL
                 WHERE id = ?3",
                params![change.to, by, change.space.0],
            )?;
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

/// An order to set on the homeserver.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub room: RoomId,
    pub room_id: String,
    pub order: Option<String>,
}

/// The orders of the account's spaces still to set, but those refused.
pub fn wanted(conn: &Connection, account: &str) -> Result<Vec<Wanted>> {
    Ok(conn
        .prepare(
            "SELECT id, room_id, space_order_desired FROM matrix_room
             WHERE account_id = ?1 AND space_order_by IS NOT NULL AND space_order_error IS NULL
             ORDER BY id",
        )?
        .query_map([account], |r| {
            Ok(Wanted {
                room: RoomId(r.get(0)?),
                room_id: r.get(1)?,
                order: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// The homeserver has the space's order set to `order`; or refused,
/// saying why.
#[derive(Debug, Serialize)]
pub struct SpaceOrderSet {
    pub room: RoomId,
    pub order: Option<String>,
    pub refused: Option<String>,
}

impl Observation for SpaceOrderSet {
    const KIND: &'static str = "matrix.space_order_set";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let pending = outbox::pending_rooms(tx, account)?;
        let name: String = tx
            .query_row(
                "SELECT name FROM matrix_room WHERE id = ?1",
                [self.room.0],
                |r| r.get(0),
            )
            .with_context(|| format!("no room {}", self.room.0))?;
        // Unless asked for another meanwhile (that is the sync task's next
        // step), done, or still asked for, refused, until the user decides.
        let asked = "space_order_by IS NOT NULL AND space_order_desired IS ?1";
        match &self.refused {
            None => tx.execute(
                &format!(
                    "UPDATE matrix_room SET space_order = ?1,
                       space_order_by = CASE WHEN {asked} THEN NULL ELSE space_order_by END,
                       space_order_desired = CASE WHEN {asked} THEN NULL ELSE space_order_desired END
                     WHERE id = ?2"
                ),
                params![self.order, self.room.0],
            )?,
            Some(error) => tx.execute(
                &format!("UPDATE matrix_room SET space_order_error = ?3 WHERE id = ?2 AND {asked}"),
                params![self.order, self.room.0, error],
            )?,
        };
        // The order shown, as asked for, is as it was.
        let events = if outbox::pending_rooms(tx, account)? == pending {
            vec![]
        } else {
            vec![outbox::outbox_changed(account)]
        };
        Ok(Observed {
            description: match &self.refused {
                None => format!("Ordered '{name}' on the homeserver"),
                Some(error) => format!("The homeserver refused to order '{name}': {error}"),
            },
            events,
        })
    }
}
