//! How the sender reconciles rooms' fields with the homeserver (SPEC
//! §6.6): a [`Kind`] of field (a room's tags, a space's order, a
//! membership) says how its fields are stored, pushed and read back; the
//! observations here record, for any kind, what the sender does to them:
//! claiming pushes, how each ended, what was read back, and pushes whose
//! outcome became unknown or expired.

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
    this_entry, Observation, Observed,
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
    ) -> Result<Vec<(Self::Request, Vec<Stored<Self::Value>>)>>;

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

/// A request for each field in flight, of its key and the value pushed:
/// of a kind the homeserver sets field by field.
pub fn each<V: Clone>(in_flight: Vec<Stored<V>>) -> Vec<((String, V), Vec<Stored<V>>)> {
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

/// The fields whose pushes the sender claims, in one entry (SPEC
/// §6.6.3): those still wanted of `fields`.
#[derive(Serialize)]
#[serde(bound = "")]
pub struct Claimed<K: Kind> {
    pub fields: Vec<(RoomId, String)>,
    #[serde(skip)]
    kind: PhantomData<K>,
}

impl<K: Kind> Claimed<K> {
    pub fn new(fields: Vec<(RoomId, String)>) -> Self {
        Self {
            fields,
            kind: PhantomData,
        }
    }
}

impl<K: Kind> Observation for Claimed<K> {
    const KIND: &'static str = K::CLAIMED;

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let mut claimed = 0;
        for (room, key) in &self.fields {
            let mut field = K::load(tx, *room, key)?;
            // Unless an op settled it meanwhile.
            if field.wanted().is_some() {
                field.claim()?;
                K::store(tx, *room, key, &field)?;
                claimed += 1;
            }
        }
        Ok(Observed {
            description: format!("Setting {claimed} {} on the homeserver", K::WHAT),
            events: vec![],
        })
    }
}

/// How a push of the room's field ended: of `value`, the value pushed.
#[derive(Serialize)]
#[serde(bound = "")]
pub struct Pushed<K: Kind> {
    pub room: RoomId,
    pub key: String,
    pub value: K::Value,
    pub ended: Ended,
}

impl<K: Kind> Observation for Pushed<K> {
    const KIND: &'static str = K::PUSHED;

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let before = outbox::pending_rooms(tx, account)?;
        let mut field = K::load(tx, self.room, &self.key)?;
        let shown = field.clone();
        field.ended(self.ended.clone(), this_entry(tx)?)?;
        K::store(tx, self.room, &self.key, &field)?;
        let changed = K::shows_otherwise(&shown, &field);
        let name = rooms::name(tx, self.room)?;
        Ok(Observed {
            description: K::pushed(&name, &self.key, &self.value, &self.ended),
            events: events(tx, account, self.room, before, changed)?,
        })
    }
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

/// Pushes whose outcome became unknown, as of `now_ms`: those in flight,
/// as the process stopped (`interrupted`), else those so long enough to
/// expire (SPEC §6.6.3).
#[derive(Serialize)]
#[serde(bound = "")]
pub struct Settled<K: Kind> {
    pub now_ms: i64,
    pub interrupted: bool,
    #[serde(skip)]
    kind: PhantomData<K>,
}

impl<K: Kind> Settled<K> {
    pub fn new(now_ms: i64, interrupted: bool) -> Self {
        Self {
            now_ms,
            interrupted,
            kind: PhantomData,
        }
    }
}

impl<K: Kind> Observation for Settled<K> {
    const KIND: &'static str = K::SETTLED;

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let before_outbox = outbox::pending_rooms(tx, account)?;
        let at = this_entry(tx)?;
        let mut settled = 0;
        let mut shown = BTreeSet::new();
        for mut stored in K::reconciling(tx, account)? {
            let field = &mut stored.field;
            let before = field.clone();
            if self.interrupted {
                field.interrupted(self.now_ms);
            } else if field.expiring(self.now_ms) {
                field.expire(self.now_ms, at)?;
            }
            if *field != before {
                K::store(tx, stored.room, &stored.key, field)?;
                settled += 1;
                if K::shows_otherwise(&before, field) {
                    shown.insert(stored.room);
                }
            }
        }
        let mut events = vec![];
        if !shown.is_empty() {
            events.push(ServerEvent::RoomsChanged(RoomsChanged {
                rooms: shown.into_iter().collect(),
            }));
        }
        if outbox::pending_rooms(tx, account)? != before_outbox {
            events.push(outbox::outbox_changed(account));
        }
        let what = K::WHAT;
        Ok(Observed {
            description: if self.interrupted {
                format!("Whether {settled} {what} were set on the homeserver is unknown")
            } else {
                format!("Took {settled} {what} set or not on the homeserver for good")
            },
            events,
        })
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
