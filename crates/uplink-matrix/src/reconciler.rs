//! How the sender reconciles rooms' fields with the homeserver (SPEC
//! §6.6): a [`Kind`] of field (a room's tags, a space's order, a
//! membership) says how its fields are stored, pushed and read back;
//! [`Rooms`] makes it a kind of field of the reconciler, which drives it.
//! What a read back finds is recorded here, for any kind.

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::BTreeSet, fmt::Debug, future::Future, marker::PhantomData};
use uplink_protocol::{
    matrix::{RoomId, RoomsChanged},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{Connection, Transaction};
use uplink_tx_log::{
    reconcile::{Ended, Field},
    reconciler::{self, Changed, Fields, Values},
    Observation, Observed,
};

use crate::{outbox, rooms};

/// A room's id as Matrix has it.
pub type RoomIdRef = matrix_sdk::ruma::RoomId;

/// A room's field, as stored, of an account.
#[derive(Debug, Clone)]
pub struct Stored<V> {
    pub room: RoomId,
    pub room_id: String,
    /// Which of the room's fields of the kind: a tag; empty of a kind a
    /// room has one of.
    pub key: String,
    pub field: Field<V>,
}

/// A kind of a room's fields.
pub trait Kind: Send + Sync + 'static {
    type Value: Clone + PartialEq + Debug + Serialize + DeserializeOwned + Send + Sync + 'static;
    /// What reading back a room's fields of the kind tells of them.
    type Read: Debug + Serialize + Send + Sync + 'static;
    /// What one request sets on the homeserver.
    type Request: Send + Sync + 'static;

    /// The kinds of the observations of the kind's fields.
    const CLAIMED: &'static str;
    const PUSHED: &'static str;
    const READ: &'static str;
    const SETTLED: &'static str;
    /// The fields, as the log names them: "room tags".
    const WHAT: &'static str;

    fn load(conn: &Connection, room: RoomId, key: &str) -> Result<Field<Self::Value>>;
    fn store(tx: &Transaction, room: RoomId, key: &str, field: &Field<Self::Value>) -> Result<()>;
    /// The account's fields uplink has an opinion of, or is to read back.
    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored<Self::Value>>>;
    /// Records the room's fields as read by a request sent after `as_of`;
    /// returns whether those shown changed.
    fn observe(tx: &Transaction, room: RoomId, read: &Self::Read, as_of: TxLogId) -> Result<bool>;
    /// The log's description of a push of the room's (named `room`) field.
    fn pushed(room: &str, key: &str, value: &Self::Value, ended: &Ended) -> String;

    /// Whether the room, as listed, shows the field otherwise as `after`
    /// than as `before`: by its desired value, unless the kind shows more.
    fn shows_otherwise(before: &Field<Self::Value>, after: &Field<Self::Value>) -> bool {
        before.desired != after.desired
    }

    /// The requests that push the fields in flight, each with those it
    /// pushes, all of one room.
    fn requests(
        conn: &Connection,
        in_flight: Vec<Stored<Self::Value>>,
    ) -> Result<Requests<Self::Request, Self::Value>>;

    /// Sends the request to the homeserver, by itself: not by the SDK's
    /// room methods, which skip, or take for done, what its store says is
    /// so already.
    fn push(
        homeserver: &Homeserver,
        room: &RoomIdRef,
        request: &Self::Request,
    ) -> impl Future<Output = matrix_sdk::Result<()>> + Send;
    /// Reads the room's fields from the homeserver.
    fn read(
        homeserver: &Homeserver,
        room: &RoomIdRef,
    ) -> impl Future<Output = Result<Self::Read>> + Send;
}

/// Requests, each with the fields it pushes.
pub type Requests<R, V> = Vec<(R, Vec<Stored<V>>)>;

/// A request for each field in flight, of its key and the value pushed:
/// of a kind the homeserver sets field by field.
pub fn each<V: Clone>(in_flight: Vec<Stored<V>>) -> Requests<(String, V), V> {
    in_flight
        .into_iter()
        .map(|stored| {
            let attempt = stored.field.attempt.as_ref().expect("in flight");
            ((stored.key.clone(), attempt.value.clone()), vec![stored])
        })
        .collect()
}

/// The account's homeserver, as the sender asks it.
pub struct Homeserver {
    pub client: matrix_sdk::Client,
    pub user: matrix_sdk::ruma::OwnedUserId,
}

/// The room's fields, read back from the homeserver by a request sent
/// after `as_of` (SPEC §6.6.2).
#[derive(Serialize)]
#[serde(bound = "")]
pub struct Read<K: Kind> {
    pub room: RoomId,
    pub read: K::Read,
    pub as_of: TxLogId,
}

impl<K: Kind> Observation for Read<K> {
    const KIND: &'static str = K::READ;

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let before = outbox::pending_rooms(tx, account)?;
        let changed = K::observe(tx, self.room, &self.read, self.as_of)?;
        let name = rooms::name(tx, self.room)?;
        Ok(Observed {
            description: format!("Read the {} of '{name}' from the homeserver", K::WHAT),
            events: events(tx, account, self.room, before, changed)?,
        })
    }
}

/// A kind of a room's fields as a kind of the reconciler's: each keyed
/// by the room and the key in it, and pushed and read back by the room's
/// id.
pub struct Rooms<K>(PhantomData<K>);

/// The account's rooms' fields of a kind, in a transaction.
pub struct RoomFields<'t, K> {
    tx: &'t Transaction<'t>,
    kind: PhantomData<K>,
}

impl<K: Kind> Fields<Rooms<K>> for RoomFields<'_, K> {
    fn load(&self, (room, key): &(RoomId, String)) -> Result<Option<Field<K::Value>>> {
        K::load(self.tx, *room, key).map(Some)
    }

    fn store(&mut self, (room, key): &(RoomId, String), field: &Field<K::Value>) -> Result<()> {
        K::store(self.tx, *room, key, field)
    }
}

impl<K: Kind> reconciler::Kind for Rooms<K> {
    type Key = (RoomId, String);
    type Value = K::Value;
    type Entity = RoomId;
    /// The room's id as Matrix has it.
    type Target = String;
    type Fields<'t> = RoomFields<'t, K>;

    const CLAIMED: &'static str = K::CLAIMED;
    const PUSHED: &'static str = K::PUSHED;
    const SETTLED: &'static str = K::SETTLED;

    fn entity((room, _): &(RoomId, String)) -> RoomId {
        *room
    }

    fn fields<'t>(tx: &'t Transaction<'t>, _account: &'t str) -> Result<RoomFields<'t, K>> {
        Ok(RoomFields {
            tx,
            kind: PhantomData,
        })
    }

    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<reconciler::Stored<Self>>> {
        Ok(K::reconciling(conn, account)?
            .into_iter()
            .map(|s| reconciler::Stored {
                key: (s.room, s.key),
                target: s.room_id,
                field: s.field,
            })
            .collect())
    }

    fn events(
        _tx: &Transaction,
        account: &str,
        changed: &[Changed<Self>],
    ) -> Result<Vec<ServerEvent>> {
        let shown: BTreeSet<RoomId> = changed
            .iter()
            .filter(|c| K::shows_otherwise(&c.before, &c.after))
            .map(|c| c.key.0)
            .collect();
        let mut events = vec![];
        if !shown.is_empty() {
            events.push(ServerEvent::RoomsChanged(RoomsChanged {
                rooms: shown.into_iter().collect(),
            }));
        }
        if changed.iter().any(Changed::pending) {
            events.push(outbox::outbox_changed(account));
        }
        Ok(events)
    }

    fn claimed(_tx: &Transaction, claimed: &[(RoomId, String)]) -> Result<String> {
        Ok(format!(
            "Setting {} {} on the homeserver",
            claimed.len(),
            K::WHAT
        ))
    }

    fn pushed(tx: &Transaction, pushes: &[(Ended, Values<Self>)]) -> Result<String> {
        let mut parts = vec![];
        for (ended, fields) in pushes {
            for ((room, key), value) in fields {
                parts.push(K::pushed(&rooms::name(tx, *room)?, key, value, ended));
            }
        }
        Ok(reconciler::joined(parts))
    }

    fn settled(settled: usize, interrupted: bool) -> String {
        let what = K::WHAT;
        if interrupted {
            format!("Whether {settled} {what} were set on the homeserver is unknown")
        } else {
            format!("Took {settled} {what} set or not on the homeserver for good")
        }
    }
}

/// A field of a kind as the reconciler has it, as the kind has it.
pub fn stored<K: Kind>(s: reconciler::Stored<Rooms<K>>) -> Stored<K::Value> {
    let (room, key) = s.key;
    Stored {
        room,
        room_id: s.target,
        key,
        field: s.field,
    }
}

/// What changed for clients: the room's fields shown, if `shown`; the
/// outbox, if not as `before`.
fn events(
    tx: &Transaction,
    account: &str,
    room: RoomId,
    before: outbox::PendingRooms,
    shown: bool,
) -> Result<Vec<ServerEvent>> {
    let mut events = vec![];
    if shown {
        events.push(ServerEvent::RoomsChanged(RoomsChanged {
            rooms: vec![room],
        }));
    }
    if outbox::pending_rooms(tx, account)? != before {
        events.push(outbox::outbox_changed(account));
    }
    Ok(events)
}
