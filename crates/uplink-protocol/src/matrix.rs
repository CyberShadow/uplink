//! Matrix rooms, their timelines, and messages being sent (SPEC §9).

use crate::compose::OutboxState;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A room of one account, as uplink numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct RoomId(pub i64);

/// A timeline event, as uplink numbers it: in the order sync delivered
/// events, so also the timeline's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct RoomEventId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct MatrixOutboxId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixAccount {
    pub id: String,
    pub mxid: String,
    /// Whether the first sync has completed.
    pub synced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    Joined,
    Invited,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Room {
    pub id: RoomId,
    pub account: String,
    /// The Matrix room id, `!opaque:server`.
    pub room_id: String,
    pub name: String,
    pub encrypted: bool,
    pub membership: Membership,
    /// Messages from others after the read marker (counting those that
    /// could not be decrypted).
    pub unread: u32,
    /// The latest event's time, ms since the Unix epoch.
    pub last_event_ts: Option<i64>,
    pub read_up_to: Option<RoomEventId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Encryption {
    None,
    Decrypted,
    /// Encrypted, and uplink has not got the key.
    Undecryptable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RoomEvent {
    pub id: RoomEventId,
    /// The Matrix event id, `$opaque`.
    pub event_id: String,
    pub sender: String,
    pub sender_name: Option<String>,
    /// `origin_server_ts`, ms since the Unix epoch.
    pub ts: i64,
    /// The event type, e.g. `m.room.message`.
    pub kind: String,
    /// Of `m.room.message`: `m.text`, `m.notice`, `m.image`, ….
    pub msgtype: Option<String>,
    /// Of `m.room.message`: the plain-text body.
    pub body: Option<String>,
    /// The event as JSON, decrypted if it could be, for event types
    /// uplink does not display otherwise.
    pub raw: String,
    pub encryption: Encryption,
}

/// Which part of a room's timeline to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimelinePage {
    /// The latest events.
    Latest,
    /// Events before this one.
    Before { event: RoomEventId },
    /// Events after this one.
    After { event: RoomEventId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixOutboxItem {
    pub id: MatrixOutboxId,
    pub room: RoomId,
    pub body: String,
    pub state: OutboxState,
    pub error: Option<String>,
    /// Ms since the Unix epoch.
    pub queued_at: i64,
}

// ── events ────────────────────────────────────────────────────────────────

/// Rooms appeared, or their name, membership, unread count or read
/// marker changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RoomsChanged {
    pub rooms: Vec<RoomId>,
}

/// Events were added to the end of a room's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TimelineAdded {
    pub room: RoomId,
    pub events: Vec<RoomEventId>,
}

/// Matrix outbox items were added, removed or changed state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixOutboxChanged {
    pub items: Vec<MatrixOutboxId>,
}
