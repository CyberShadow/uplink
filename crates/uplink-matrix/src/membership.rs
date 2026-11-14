//! Joining and leaving rooms (SPEC §9.7), as desired state (§6.6), a
//! field of each room: the user's op records the membership asked for,
//! and the account's sync task asks the homeserver for it. One refused
//! stays asked for, and refused, until the user decides: undoing the op
//! keeps the room as it is.
//!
//! Undoing is going back to how the room was, as far as that is still
//! possible: before anything was asked of the homeserver, the change is
//! cancelled; after, it is reversed (joined rooms are left; rooms left are
//! joined again, which the homeserver may refuse). If another membership
//! was asked for meanwhile, undo is refused.

use anyhow::{bail, Context, Result};
use matrix_sdk::ruma::api::client::{
    filter::FilterDefinition,
    membership::{join_room_by_id, leave_room},
    sync::sync_events::{self, v3::Filter},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uplink_protocol::{
    compose::OutboxChanged,
    matrix::{Membership, MembershipChange, RoomId, RoomsChanged},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{
    reconcile::{Ended, Field},
    this_entry, Applied, Conflict, Inverse, Op,
};

use crate::reconciler::{Homeserver, Kind, RoomIdRef, Stored};

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

fn opposite(change: MembershipChange) -> MembershipChange {
    match change {
        MembershipChange::Join => MembershipChange::Leave,
        MembershipChange::Leave => MembershipChange::Join,
    }
}

/// The change that pushes `membership`: leaving for an invitation too,
/// which no change can bring back, as leaving is nearest to it (the
/// homeserver keeping it left, the user decides, SPEC §6.6.3).
pub(crate) fn change_to(membership: Membership) -> MembershipChange {
    match membership {
        Membership::Joined => MembershipChange::Join,
        Membership::Invited | Membership::Left => MembershipChange::Leave,
    }
}

/// The desired membership as a change, if one is asked for.
pub(crate) fn parse_desired(desired: Option<&str>) -> Option<MembershipChange> {
    desired.map(|desired| change_to(parse_membership(desired)))
}

/// A room's membership (SPEC §6.6): had (`observed`) or asked for
/// (`desired`).
pub type Member = Field<Membership>;

pub(crate) fn parse_field(json: &str) -> Result<Member> {
    serde_json::from_str(json).with_context(|| format!("a room's membership field: {json}"))
}

/// The JSON of the field of a room first seen with `membership` by a
/// request sent after `as_of`.
pub(crate) fn new_field(membership: Membership, as_of: TxLogId) -> String {
    serde_json::to_string(&Field::new(membership, as_of)).expect("a field serializes")
}

fn load(conn: &Connection, room: RoomId) -> Result<Member> {
    let json: String = conn
        .query_row(
            "SELECT membership_field FROM matrix_room WHERE id = ?1",
            [room.0],
            |r| r.get(0),
        )
        .with_context(|| format!("no room {}", room.0))?;
    parse_field(&json)
}

fn store(tx: &Transaction, room: RoomId, field: &Member) -> Result<()> {
    tx.execute(
        "UPDATE matrix_room SET membership_field = ?2 WHERE id = ?1",
        params![room.0, serde_json::to_string(field)?],
    )?;
    Ok(())
}

/// Records the room's membership as observed by a request sent after
/// `as_of` (SPEC §6.6.2); returns whether it or the change pending
/// changed.
pub(crate) fn observe(
    tx: &Transaction,
    room: RoomId,
    membership: Membership,
    as_of: TxLogId,
) -> Result<bool> {
    let mut field = load(tx, room)?;
    let before = field.clone();
    field.observe(membership, as_of, this_entry(tx)?)?;
    store(tx, room, &field)?;
    Ok(shown(&field) != shown(&before))
}

/// What a room shows of its membership: as had, the change pending, and
/// why it waits for the user's decision.
fn shown(field: &Member) -> (Membership, Option<MembershipChange>, Option<&str>) {
    let pending = field.opinion.map(|_| change_to(field.desired));
    let decision = field.decision.as_ref().map(|d| d.reason());
    (field.observed, pending, decision)
}

struct State {
    account: String,
    name: String,
    field: Member,
}

fn state(tx: &Transaction, room: RoomId) -> Result<State> {
    let (account, name) = tx
        .query_row(
            "SELECT account_id, name FROM matrix_room WHERE id = ?1",
            [room.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .with_context(|| format!("no room {}", room.0))?;
    Ok(State {
        account,
        name,
        field: load(tx, room)?,
    })
}

/// The room changed, and with it what its account has pending.
fn changed(room: RoomId, state: &State) -> Vec<ServerEvent> {
    vec![
        ServerEvent::RoomsChanged(RoomsChanged { rooms: vec![room] }),
        ServerEvent::OutboxChanged(OutboxChanged {
            accounts: vec![state.account.clone()],
        }),
    ]
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
        let mut room = state(tx, self.room)?;
        if room.field.pending() {
            bail!(Conflict(format!("'{}' is changing already", room.name)));
        }
        let had = room.field.observed;
        if had == target(self.change) {
            bail!(Conflict(format!("'{}' is that way already", room.name)));
        }
        room.field.want(target(self.change), this_entry(tx)?);
        store(tx, self.room, &room.field)?;
        let name = &room.name;
        Ok(Applied {
            description: match (self.change, had) {
                (MembershipChange::Join, _) => format!("Joined '{name}'"),
                (MembershipChange::Leave, Membership::Invited) => format!("Declined '{name}'"),
                (MembershipChange::Leave, _) => format!("Left '{name}'"),
            },
            inverse: Inverse::of(&RevertMembership {
                room: self.room,
                change: self.change,
                prior: had,
            }),
            events: changed(self.room, &room),
        })
    }
}

/// Undoes `change`, made when the membership was `prior`: cancels it if
/// nothing of it was asked of the homeserver, or else reverses it.
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
        let mut room = state(tx, self.room)?;
        let name = room.name.clone();
        if room.field.desired != target(self.change) {
            bail!(Conflict(format!("'{name}' changed since")));
        }
        let by = this_entry(tx)?;
        let unasked = room.field.attempt.is_none() && room.field.observed == self.prior;
        let applied = if unasked {
            room.field.want(self.prior, by);
            Applied {
                description: format!("Kept '{name}' as it was"),
                inverse: Inverse::of(&ChangeMembership {
                    room: self.room,
                    change: self.change,
                }),
                events: changed(self.room, &room),
            }
        } else {
            let reverse = opposite(self.change);
            room.field.want(target(reverse), by);
            Applied {
                description: match reverse {
                    MembershipChange::Join => format!("Joined '{name}' again"),
                    MembershipChange::Leave => format!("Left '{name}'"),
                },
                inverse: Inverse::of(&RevertMembership {
                    room: self.room,
                    change: reverse,
                    prior: target(self.change),
                }),
                events: changed(self.room, &room),
            }
        };
        store(tx, self.room, &room.field)?;
        Ok(applied)
    }
}

/// Rooms' memberships, a field of each room.
pub struct Memberships;

impl Kind for Memberships {
    type Value = Membership;
    /// The room's membership.
    type Read = Membership;

    const CLAIMED: &'static str = "matrix.memberships_claimed";
    const PUSHED: &'static str = "matrix.membership_changed";
    const READ: &'static str = "matrix.membership_read";
    const SETTLED: &'static str = "matrix.membership_pushes_settled";
    const WHAT: &'static str = "memberships";

    fn load(conn: &Connection, room: RoomId, _key: &str) -> Result<Member> {
        load(conn, room)
    }

    fn store(tx: &Transaction, room: RoomId, _key: &str, field: &Member) -> Result<()> {
        store(tx, room, field)
    }

    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored<Membership>>> {
        conn.prepare(
            "SELECT id, room_id, membership_field FROM matrix_room
             WHERE account_id = ?1
               AND (membership_by IS NOT NULL
                    OR json_extract(membership_field, '$.check') IS NOT NULL)
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
        membership: &Membership,
        as_of: TxLogId,
    ) -> Result<bool> {
        observe(tx, room, *membership, as_of)
    }

    fn shows_otherwise(before: &Member, after: &Member) -> bool {
        shown(before) != shown(after)
    }

    fn pushed(room: &str, _key: &str, membership: &Membership, ended: &Ended) -> String {
        let verb = match change_to(*membership) {
            MembershipChange::Join => "join",
            MembershipChange::Leave => "leave",
        };
        match ended {
            Ended::Applied => match change_to(*membership) {
                MembershipChange::Join => format!("Joined '{room}' on the homeserver"),
                MembershipChange::Leave => format!("Left '{room}' on the homeserver"),
            },
            Ended::NotApplied {
                refused: Some(error),
            } => format!("The homeserver refused to {verb} '{room}': {error}"),
            Ended::NotApplied { refused: None } => {
                format!("Could not {verb} '{room}' on the homeserver")
            }
            Ended::Unknown { .. } => format!("Whether '{room}' was {verb}ed is unknown"),
        }
    }

    async fn push(
        homeserver: &Homeserver,
        room: &RoomIdRef,
        _key: &str,
        membership: &Membership,
    ) -> matrix_sdk::Result<()> {
        let client = &homeserver.client;
        let room = room.to_owned();
        match change_to(*membership) {
            MembershipChange::Join => {
                client.send(join_room_by_id::v3::Request::new(room)).await?;
            }
            MembershipChange::Leave => {
                client.send(leave_room::v3::Request::new(room)).await?;
            }
        }
        Ok(())
    }

    /// By a sync of the room alone, from the start, which tells which of
    /// joined, invited and left it is (none: never in it, so left).
    async fn read(homeserver: &Homeserver, room: &RoomIdRef) -> Result<Membership> {
        let mut filter = FilterDefinition::ignore_all();
        filter.room.rooms = Some(vec![room.to_owned()]);
        filter.room.include_leave = true;
        let mut request = sync_events::v3::Request::new();
        request.filter = Some(Filter::FilterDefinition(filter));
        request.timeout = Some(Duration::ZERO);
        let response = homeserver
            .client
            .send(request)
            .await
            .context("reading a room's membership")?;
        Ok(if response.rooms.join.contains_key(room) {
            Membership::Joined
        } else if response.rooms.invite.contains_key(room) {
            Membership::Invited
        } else {
            Membership::Left
        })
    }
}
