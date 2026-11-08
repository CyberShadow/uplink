//! Joining and leaving rooms (SPEC §9.7), as desired state (§6.6): the
//! user's op records the membership asked for, and the account's sync task
//! asks the homeserver for it, then records the outcome.
//!
//! Undoing is going back to how the room was, as far as that is still
//! possible: before the change took effect, it is cancelled; after, it is
//! reversed (joined rooms are left; rooms left are joined again, which the
//! homeserver may refuse). If the membership changed otherwise meanwhile,
//! undo is refused.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    matrix::{Membership, MembershipChange, RoomId, RoomsChanged},
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Observation, Observed, Op};

pub(crate) fn parse_membership(membership: &str) -> Membership {
    match membership {
        "joined" => Membership::Joined,
        "invited" => Membership::Invited,
        "left" => Membership::Left,
        _ => unreachable!("the schema allows no membership {membership}"),
    }
}

/// The membership `change` leads to.
fn target(change: MembershipChange) -> Membership {
    match change {
        MembershipChange::Join => Membership::Joined,
        MembershipChange::Leave => Membership::Left,
    }
}

fn target_str(change: MembershipChange) -> &'static str {
    match change {
        MembershipChange::Join => "joined",
        MembershipChange::Leave => "left",
    }
}

fn opposite(change: MembershipChange) -> MembershipChange {
    match change {
        MembershipChange::Join => MembershipChange::Leave,
        MembershipChange::Leave => MembershipChange::Join,
    }
}

/// The desired membership as a change, if one is asked for.
pub(crate) fn parse_desired(desired: Option<&str>) -> Option<MembershipChange> {
    desired.map(|desired| match desired {
        "joined" => MembershipChange::Join,
        "left" => MembershipChange::Leave,
        _ => unreachable!("the schema allows no desired membership {desired}"),
    })
}

struct State {
    name: String,
    membership: Membership,
    desired: Option<MembershipChange>,
}

fn state(tx: &Transaction, room: RoomId) -> Result<State> {
    tx.query_row(
        "SELECT name, membership, desired FROM matrix_room WHERE id = ?1",
        [room.0],
        |r| {
            Ok(State {
                name: r.get(0)?,
                membership: parse_membership(&r.get::<_, String>(1)?),
                desired: parse_desired(r.get::<_, Option<String>>(2)?.as_deref()),
            })
        },
    )
    .optional()?
    .with_context(|| format!("no room {}", room.0))
}

fn desire(tx: &Transaction, room: RoomId, desired: Option<MembershipChange>) -> Result<()> {
    tx.execute(
        "UPDATE matrix_room SET desired = ?1, membership_error = NULL WHERE id = ?2",
        params![desired.map(target_str), room.0],
    )?;
    Ok(())
}

fn changed(room: RoomId) -> Vec<ServerEvent> {
    vec![ServerEvent::RoomsChanged(RoomsChanged {
        rooms: vec![room],
    })]
}

/// Asks for joining or leaving the room.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeMembership {
    pub room: RoomId,
    pub change: MembershipChange,
}

impl Op for ChangeMembership {
    const KIND: &'static str = "matrix.change_membership";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let room = state(tx, self.room)?;
        if room.desired.is_some() {
            bail!(Conflict(format!("'{}' is changing already", room.name)));
        }
        if room.membership == target(self.change) {
            bail!(Conflict(format!("'{}' is that way already", room.name)));
        }
        desire(tx, self.room, Some(self.change))?;
        let name = &room.name;
        Ok(Applied {
            description: match (self.change, room.membership) {
                (MembershipChange::Join, _) => format!("Joined '{name}'"),
                (MembershipChange::Leave, Membership::Invited) => format!("Declined '{name}'"),
                (MembershipChange::Leave, _) => format!("Left '{name}'"),
            },
            inverse: Inverse::of(&RevertMembership {
                room: self.room,
                change: self.change,
                prior: room.membership,
            }),
            events: changed(self.room),
        })
    }
}

/// Undoes `change`, made when the membership was `prior`: cancels it if it
/// has not taken effect, or else reverses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevertMembership {
    pub room: RoomId,
    pub change: MembershipChange,
    pub prior: Membership,
}

impl Op for RevertMembership {
    const KIND: &'static str = "matrix.revert_membership";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let room = state(tx, self.room)?;
        let name = &room.name;
        let pending = room.membership == self.prior && room.desired == Some(self.change);
        let done = room.membership == target(self.change) && room.desired.is_none();
        if pending {
            desire(tx, self.room, None)?;
            Ok(Applied {
                description: format!("Kept '{name}' as it was"),
                inverse: Inverse::of(&ChangeMembership {
                    room: self.room,
                    change: self.change,
                }),
                events: changed(self.room),
            })
        } else if done {
            let reverse = opposite(self.change);
            desire(tx, self.room, Some(reverse))?;
            Ok(Applied {
                description: match reverse {
                    MembershipChange::Join => format!("Joined '{name}' again"),
                    MembershipChange::Leave => format!("Left '{name}'"),
                },
                inverse: Inverse::of(&RevertMembership {
                    room: self.room,
                    change: reverse,
                    prior: room.membership,
                }),
                events: changed(self.room),
            })
        } else {
            bail!(Conflict(format!("'{name}' changed since")))
        }
    }
}

/// A membership change to ask the homeserver for.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub room: RoomId,
    pub room_id: String,
    pub change: MembershipChange,
}

pub fn wanted(conn: &Connection, account: &str) -> Result<Vec<Wanted>> {
    Ok(conn
        .prepare(
            "SELECT id, room_id, desired FROM matrix_room
             WHERE account_id = ?1 AND desired IS NOT NULL AND desired != membership",
        )?
        .query_map([account], |r| {
            Ok(Wanted {
                room: RoomId(r.get(0)?),
                room_id: r.get(1)?,
                change: parse_desired(Some(&r.get::<_, String>(2)?)).expect("desired is set"),
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// The homeserver did as asked; or refused, saying why.
#[derive(Debug, Serialize)]
pub struct MembershipChanged {
    pub room: RoomId,
    pub change: MembershipChange,
    pub refused: Option<String>,
}

impl Observation for MembershipChanged {
    const KIND: &'static str = "matrix.membership_changed";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let room = state(tx, self.room)?;
        // Asked for something else meanwhile (or nothing): that is the
        // sync task's next step.
        if room.desired != Some(self.change) {
            return Ok(Observed {
                description: format!("Changed '{}', as asked before", room.name),
                events: vec![],
            });
        }
        match &self.refused {
            None => tx.execute(
                "UPDATE matrix_room SET membership = ?1, desired = NULL WHERE id = ?2",
                params![target_str(self.change), self.room.0],
            )?,
            Some(error) => tx.execute(
                "UPDATE matrix_room SET desired = NULL, membership_error = ?1 WHERE id = ?2",
                params![error, self.room.0],
            )?,
        };
        let name = &room.name;
        Ok(Observed {
            description: match (&self.refused, self.change) {
                (None, MembershipChange::Join) => format!("Joined '{name}' on the homeserver"),
                (None, MembershipChange::Leave) => format!("Left '{name}' on the homeserver"),
                (Some(error), _) => format!("The homeserver refused to change '{name}': {error}"),
            },
            events: changed(self.room),
        })
    }
}
