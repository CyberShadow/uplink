//! Matrix rooms, their timelines, and messages being sent (SPEC §9).

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
pub struct MatrixSendId(pub i64);

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

/// Joining a room one is invited to, or leaving (or declining) one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum MembershipChange {
    Join,
    Leave,
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
    pub read_up_to: Option<ReadMarker>,
    /// A change the user asked for that has yet to take effect.
    pub pending: Option<MembershipChange>,
    /// Why the homeserver refused the last change asked for.
    pub membership_error: Option<String>,
    /// Whether the room is a space.
    pub space: bool,
    /// `mxc://server/media`: the room's avatar, or of a direct chat without
    /// one, the other user's. Served at
    /// `/api/rooms/{id}/avatar/{server}/{media}`.
    pub avatar: Option<String>,
    /// Of a space, the Matrix ids of the rooms it lists, joined or not, in
    /// the space's order.
    pub children: Vec<String>,
    /// `m.room.topic`, if set and not empty.
    pub topic: Option<String>,
    /// How many members have joined, as the homeserver counts them.
    pub joined_members: u64,
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
    /// Where it is in the timeline, which is in this order: events
    /// fetched later may be earlier (SPEC §9.2).
    pub pos: i64,
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
    /// The event as JSON, decrypted if it could be: what clients show of
    /// it beyond the fields here (media, state), and its source.
    pub raw: String,
    pub encryption: Encryption,
    /// Whether the event is redacted: `raw` is as redacted, with
    /// `unsigned.redacted_because`.
    pub redacted: bool,
    /// Of a message, its latest edit by its sender.
    pub edit: Option<EventEdit>,
    /// The reactions to it, by key, in the order first reacted with.
    pub reactions: Vec<Reaction>,
    /// Of a reply (`m.in_reply_to`), the event it answers, if stored.
    pub replied: Option<RepliedEvent>,
    /// Of a message with an HTML `formatted_body`, that, sanitized
    /// (SPEC §9.2).
    pub html: Option<String>,
}

/// An edit (`m.replace`) of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EventEdit {
    pub event_id: String,
    /// Ms since the Unix epoch.
    pub ts: i64,
    /// The message's content as edited (`m.new_content`), as JSON.
    pub content: String,
    /// Its HTML `formatted_body`, if it has one, sanitized (SPEC §9.2).
    pub html: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Reaction {
    /// What is reacted with: usually an emoji.
    pub key: String,
    /// Each sender once, in the order they reacted.
    pub senders: Vec<Reactor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Reactor {
    pub sender: String,
    pub sender_name: Option<String>,
    /// The reaction's event: redacting it takes the reaction back.
    pub event_id: String,
}

/// The event a reply answers, as the reply shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RepliedEvent {
    pub id: RoomEventId,
    pub event_id: String,
    pub sender: String,
    pub sender_name: Option<String>,
    pub kind: String,
    /// As last edited.
    pub msgtype: Option<String>,
    /// As last edited.
    pub body: Option<String>,
    pub redacted: bool,
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

/// Where a Matrix message being sent is (SPEC §9.3). Never unknown: the
/// homeserver deduplicates the transaction id, so it is sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum MatrixSendState {
    /// Waiting for the homeserver; cancelling takes it back.
    Queued,
    /// Being handed to the homeserver.
    Sending,
    /// Accepted; listed until its event is synced back.
    Sent,
    /// Refused by the homeserver; `error` says why. Needs a decision:
    /// cancelling drops it.
    Refused,
}

/// What a send sends to a room (SPEC §9.3). Events it is of are named
/// by their Matrix ids, which the homeserver knows them by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MatrixContent {
    /// A message, in Markdown (formatted, if it has formatting, as HTML).
    Text {
        body: String,
        /// `m.emote` (`/me`) rather than `m.text`.
        emote: bool,
        /// The event it replies to.
        reply_to: Option<String>,
        /// The account's own message it edits (`m.replace`).
        replaces: Option<String>,
    },
    /// Reacts to an event with `key`, usually an emoji.
    Reaction { event: String, key: String },
    /// Redacts an event: deletes a message, or takes a reaction back.
    Redaction { event: String },
    /// A file uploaded to uplink, sent as an image, a video, audio or a
    /// file, by its type.
    File {
        blob: crate::compose::BlobId,
        name: String,
        mime: String,
        reply_to: Option<String>,
    },
    /// Names the room (`m.room.name`); empty takes the name away.
    Name { name: String },
    /// Sets the room's topic (`m.room.topic`); empty takes it away.
    Topic { topic: String },
    /// Invites, kicks, bans or unbans `user`, a Matrix user id.
    Member {
        user: String,
        action: MemberAction,
        reason: Option<String>,
    },
}

/// What a send does to a member of the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum MemberAction {
    Invite,
    Kick,
    Ban,
    Unban,
}

/// A message being sent, sent and not yet synced back, or sent though
/// cancelled (needing a decision).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixSend {
    pub id: MatrixSendId,
    pub room: RoomId,
    pub content: MatrixContent,
    pub state: MatrixSendState,
    /// Whether it is to be sent: `false` once cancelled while being
    /// sent. Sent, it needs a decision: keeping it sent.
    pub wanted: bool,
    pub error: Option<String>,
    /// Ms since the Unix epoch.
    pub queued_at: i64,
}

/// A room being joined or left (SPEC §9.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PendingMembership {
    pub room: RoomId,
    pub change: MembershipChange,
    /// The op that asked for it: undoing it keeps the room as it is.
    pub op: crate::compose::PendingOp,
    /// Why the homeserver refused it: it needs a decision, which undoing
    /// the op makes.
    pub refused: Option<String>,
}

// ── events ────────────────────────────────────────────────────────────────

/// Rooms appeared, or their name, membership, avatar, children, unread
/// count or read marker changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RoomsChanged {
    pub rooms: Vec<RoomId>,
}

/// The room's read marker: the event read up to, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ReadMarker {
    pub event: RoomEventId,
    pub pos: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct GapId(pub i64);

/// Events missing from a room's timeline (SPEC §9.2), which `FillGap`
/// fetches from the homeserver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TimelineGap {
    pub id: GapId,
    /// The listed event the gap is just before; `None` if it is after
    /// every listed event (of a sync that brought only edits, say).
    pub before: Option<RoomEventId>,
}

/// Events missing from a room's timeline were fetched: they fill `gap`,
/// which `left` replaces if they did not all come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TimelineFilled {
    pub room: RoomId,
    pub gap: GapId,
    /// Those listed, of the events fetched.
    pub events: Vec<RoomEventId>,
    pub left: Option<TimelineGap>,
}

/// Events were added to the end of a room's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TimelineAdded {
    pub room: RoomId,
    pub events: Vec<RoomEventId>,
}

/// What these events of the room show changed: they were edited,
/// reacted to or redacted, or the event they reply to was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TimelineChanged {
    pub room: RoomId,
    pub events: Vec<RoomEventId>,
}

/// Who is typing in the room, but the account: all of them, each time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Typing {
    pub room: RoomId,
    pub users: Vec<RoomMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RoomMember {
    pub user: String,
    /// Their display name in the room, if they have one.
    pub name: Option<String>,
}

/// Members of the room sent read receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ReceiptsChanged {
    pub room: RoomId,
}

/// The account's devices, their trust or its key backup changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SecurityChanged {
    pub account: String,
}

/// The account's device and its encryption, as its client has them
/// (SPEC §9.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixSecurity {
    pub account: String,
    /// uplink's device.
    pub device_id: String,
    /// Whether the account's cross-signing has signed uplink's device:
    /// others then trust it.
    pub verified: bool,
    /// Whether the device has the account's cross-signing keys (all
    /// three), and so can verify others.
    pub cross_signing: bool,
    pub backup: KeyBackup,
    pub recovery: Recovery,
    /// The account's devices, uplink's among them, most recently seen
    /// first.
    pub devices: Vec<MatrixDevice>,
    /// Verifications of the account's devices with uplink's, going on or
    /// ended since the last began, oldest first.
    pub verifications: Vec<MatrixVerification>,
}

/// A verification of uplink's device with another of the account's, by
/// comparing emoji (SAS), as clients do (SPEC §9.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixVerification {
    pub flow_id: String,
    /// The other device, once known: a request uplink makes goes to every
    /// device of the account, and one answers.
    pub other_device: Option<String>,
    /// Whether uplink asked.
    pub we_started: bool,
    pub phase: VerificationPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerificationPhase {
    /// Asked: of the other device, which has not answered yet, or of the
    /// user, who accepts or not.
    Requested,
    /// Accepted; comparing starts.
    Ready,
    /// The devices are exchanging keys.
    Started,
    /// Both devices show these: the user says whether they match.
    Compare {
        emoji: Vec<SasEmoji>,
        decimals: Vec<u16>,
    },
    /// They match here; the other device has yet to say.
    Confirmed,
    Done,
    Cancelled {
        reason: String,
        by_us: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SasEmoji {
    pub symbol: String,
    pub description: String,
}

/// What the user says of a verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum VerificationAnswer {
    /// Of one asked of the user.
    Accept,
    /// The emoji match.
    Confirm,
    /// They do not: the other device may be an impostor's.
    Mismatch,
    Cancel,
}

/// The state of the key backup, which keeps room keys on the homeserver
/// for the account's devices (as the SDK's `BackupState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum KeyBackup {
    /// Not used by this device: there is none, or it lacks its key.
    Unknown,
    Creating,
    Enabling,
    Resuming,
    Enabled,
    /// Downloading the backup's keys, after enabling it.
    Downloading,
    Disabling,
}

/// Whether the account's secrets are kept in Secret Storage, for devices
/// to recover them (as the SDK's `RecoveryState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    Unknown,
    /// All are, and this device has them.
    Enabled,
    Disabled,
    /// Some are, or this device lacks them: recover with the key.
    Incomplete,
}

/// A device of the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixDevice {
    pub device_id: String,
    pub name: Option<String>,
    /// When it was last seen, ms since the Unix epoch, and where from.
    pub last_seen_ts: Option<i64>,
    pub last_seen_ip: Option<String>,
    /// Whether uplink's device trusts it: the account's cross-signing
    /// signed it.
    pub verified: bool,
    /// Whether it is uplink's.
    pub current: bool,
}

/// A room to create (SPEC §9.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NewRoom {
    /// Empty: none.
    pub name: String,
    pub topic: Option<String>,
    /// Matrix user ids to invite.
    pub invite: Vec<String>,
    pub encrypted: bool,
    /// The localpart of the address it is published under, as a public
    /// room anyone may join; else it is private, by invitation.
    pub alias: Option<String>,
}

/// A member of the room: joined, invited, left or banned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MemberInfo {
    pub user: String,
    /// Their display name in the room, if they have one.
    pub name: Option<String>,
    pub membership: MemberState,
    /// Their power level in the room; `None` for a creator of a room of
    /// version 12 or later, whose power is above any.
    pub power: Option<i64>,
    /// Whether the account may kick them, and ban them (unbanning needs
    /// both), as its power is above theirs and the room's levels say.
    pub kickable: bool,
    pub bannable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    Joined,
    Invited,
    Knocking,
    Left,
    Banned,
}

/// What the account may do in the room, as its power levels say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RoomPowers {
    pub invite: bool,
    pub name: bool,
    pub topic: bool,
}

/// How far a member of the room (not the account) has read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MemberReceipt {
    pub member: RoomMember,
    /// The stored event their receipt is for, and where it is.
    pub event: RoomEventId,
    pub pos: i64,
    /// When they read it, ms since the Unix epoch, if the receipt says.
    pub ts: Option<i64>,
}

/// Messages being sent to these rooms were added, changed state or went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatrixSendsChanged {
    pub rooms: Vec<RoomId>,
}
