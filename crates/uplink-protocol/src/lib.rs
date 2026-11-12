//! Wire types for the WebSocket protocol (SPEC §5).
//!
//! Every message is a JSON text frame carrying a `type` discriminator.
//! Payload enums nest under a `body` field rather than being flattened, so
//! their own `type` tags don't collide with the envelope's.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub mod calendar;
pub mod compose;
pub mod mail;
pub mod matrix;
pub mod todos;
pub mod typescript;

use compose::{BlobId, Draft, DraftAttachmentId, DraftContent, DraftId, OutboxId, Splice};
use mail::{EntryId, LabelId, ListPosition, MessageId};
use matrix::{MatrixSendId, MembershipChange, RoomEventId, RoomId, TimelinePage};
use todos::{TodoEdit, TodoId};

/// Id of a log entry. Entries are numbered from 1 in commit order, so an
/// id also serves as a position in the event stream; 0 is the position
/// before the first entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct TxLogId(pub i64);

// ── envelopes ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WsClientMsg {
    Ping {
        seq: u64,
    },
    /// Must be the first message on a connection.
    Hello {
        /// The version of the protocol the client speaks
        /// ([`typescript::version`]); a server speaking another closes the
        /// connection, and the client loads anew.
        version: String,
        /// The last entry the client received on a previous connection, if
        /// it wants to resume rather than load afresh.
        since: Option<TxLogId>,
    },
    /// The server answers with a `Response` or `RequestFailed` carrying the
    /// same `request_id`.
    Request {
        request_id: u64,
        body: ClientRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WsServerMsg {
    Pong {
        seq: u64,
    },
    /// Reply to `Hello`.
    ///
    /// If `resumed`, every entry after the client's `since` follows
    /// immediately, then live ones. Otherwise the client must discard its
    /// state and reload, and adopt `cursor` as its position; only entries
    /// after it will follow.
    Welcome {
        /// The most recent entry at the time of `Hello`.
        cursor: TxLogId,
        resumed: bool,
    },
    Response {
        request_id: u64,
        body: ServerResponse,
    },
    RequestFailed {
        request_id: u64,
        error: ServerError,
    },
    Entry {
        entry: LogEntry,
    },
    /// What the badged buckets hold (SPEC §12.14): right after `Welcome`,
    /// and again whenever it changes.
    Badges {
        badges: Badges,
    },
    /// What is only of the moment, as it happens: not logged, so not
    /// replayed. A client that reconnects forgets what notices told it
    /// (SPEC §5.5).
    Notice {
        notice: Notice,
    },
}

/// A notice of the moment (SPEC §5.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    /// The home view's buckets were configured anew: list it again.
    HomeChanged,
    Typing(matrix::Typing),
    ReceiptsChanged(matrix::ReceiptsChanged),
    /// The account's devices, their trust or its key backup changed.
    SecurityChanged(matrix::SecurityChanged),
}

// ── requests ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Undo a log entry. Undoing an undo entry is a redo.
    Undo {
        tx_log_id: TxLogId,
    },
    /// The user's ops, newest first, starting before `before` (SPEC §6.3):
    /// what can be undone, and what was.
    ListHistory {
        before: Option<TxLogId>,
        limit: u32,
    },
    /// The configured mail accounts and their labels.
    ListAccounts,
    /// A page of the conversations `query` lists (SPEC §8.7), in every
    /// account, newest first, starting after `after`. Lists of labels are
    /// queries too, as in notmuch: the Inbox is `in:inbox`.
    ListEntries {
        query: String,
        after: Option<ListPosition>,
        limit: u32,
    },
    /// What `query` matches of every kind (SPEC §8.7): a page of mail's
    /// conversations, as `ListEntries` lists them, and the rooms, to-dos
    /// and events. A page after `after` is of mail alone, the rest being
    /// on the first.
    ListItems {
        query: String,
        after: Option<ListPosition>,
        limit: u32,
    },
    /// The home view (SPEC §12.13): each configured bucket's items, as
    /// `ListItems` lists them, of up to `limit` conversations; an item is
    /// in the first bucket that matches it.
    ListHome {
        limit: u32,
    },
    /// The entries of the entry's conversation, in Trash and Spam too,
    /// oldest first.
    ListConversation {
        entry: EntryId,
    },
    /// The message's entries, in every account, newest first.
    ListMessageEntries {
        message: MessageId,
    },
    LoadMessage {
        message: MessageId,
    },
    /// Have the rule (of any case) show remote images unasked, or no
    /// longer.
    AlwaysShowImages {
        rule: mail::ImageRule,
        always: bool,
    },
    /// Add and remove labels on entries, all of the labels' account.
    /// Archiving is removing `INBOX`; marking read, removing `UNREAD`.
    ModifyLabels {
        entries: Vec<EntryId>,
        add: Vec<LabelId>,
        remove: Vec<LabelId>,
    },
    /// Does `action` to the conversations, as one op.
    ActOnConversations {
        conversations: mail::ConversationSet,
        action: mail::ConversationAction,
    },
    /// How many conversations `query` lists, as a search does.
    CountConversations {
        query: String,
    },
    /// All drafts.
    ListDrafts,
    /// Create a draft, with revision 0.
    CreateDraft {
        id: DraftId,
        account: String,
        content: DraftContent,
        in_reply_to: Option<MessageId>,
    },
    /// Apply `splice` to the draft at `revision`; refused with `conflict`
    /// if the draft is at another revision.
    EditDraft {
        draft: DraftId,
        revision: i64,
        splice: Splice,
    },
    DeleteDraft {
        draft: DraftId,
    },
    /// Attach the uploaded `blob` to the draft, as a file `name` of type
    /// `mime`.
    AttachToDraft {
        draft: DraftId,
        blob: BlobId,
        name: String,
        mime: String,
    },
    DetachFromDraft {
        draft: DraftId,
        attachment: DraftAttachmentId,
    },
    /// Move the draft to the outbox, to be sent after the send delay.
    SendDraft {
        draft: DraftId,
    },
    /// Take a mail being sent back, into drafts: at once if it has not
    /// been handed to the provider, or if the provider refuses it; if it
    /// went out, it needs a decision (SPEC §6.7).
    CancelSend {
        item: OutboxId,
    },
    /// Keep a mail sent though cancelled: resolves it.
    KeepSent {
        item: OutboxId,
    },
    /// What is not yet reconciled with the providers: per account with
    /// anything, what is pending (SPEC §6.6).
    ListOutbox,
    /// Resolve the changes the provider refused, of `op` (`None`: those
    /// made before ops were recorded with them) in `account`, by keeping
    /// what the provider has: an op, undoable like any other.
    KeepObserved {
        account: String,
        op: Option<TxLogId>,
    },
    /// The configured Matrix accounts and their rooms.
    ListRooms,
    /// Up to `limit` events of the room's timeline, oldest first.
    ListTimeline {
        room: RoomId,
        page: TimelinePage,
        limit: u32,
    },
    /// Fetch events missing from the room's timeline: the homeserver's
    /// page of them before the gap. Answered `Done` once they are stored,
    /// as `TimelineFilled` says.
    FillGap {
        room: RoomId,
        gap: matrix::GapId,
    },
    /// These events of the room's timeline, as listed there; those
    /// unknown or not listed are left out.
    ListRoomEvents {
        room: RoomId,
        events: Vec<RoomEventId>,
    },
    /// The edits of the room's message `event`, oldest first.
    ListEdits {
        room: RoomId,
        event: RoomEventId,
    },
    /// Send to the room: a message, a reaction, a redaction or a file.
    SendMatrixMessage {
        room: RoomId,
        content: matrix::MatrixContent,
    },
    /// Take a message being sent back: at once if it has not been handed
    /// to the homeserver, or if the homeserver refuses it; if it went out,
    /// it needs a decision (SPEC §9.3).
    CancelMatrixSend {
        item: MatrixSendId,
    },
    /// Keep a message sent though cancelled: resolves it.
    KeepMatrixSent {
        item: MatrixSendId,
    },
    /// Move the room's read marker to `event`: it and everything before
    /// it are read.
    MarkRoomRead {
        room: RoomId,
        event: RoomEventId,
    },
    /// Tell the room the account is typing, or has stopped. Not an op:
    /// typing is of the moment.
    SetTyping {
        room: RoomId,
        typing: bool,
    },
    /// How far the room's members have read, as their receipts say.
    ListReceipts {
        room: RoomId,
    },
    /// The room's members, and what the account may do in it.
    ListMembers {
        room: RoomId,
    },
    /// The room's pinned events, of those stored.
    ListPinned {
        room: RoomId,
    },
    /// A thread: its root, and its messages.
    ListThread {
        room: RoomId,
        root: RoomEventId,
    },
    /// The room's messages whose text (or an edit's) has every word of
    /// `query`, newest first: up to `limit`, before `before` if given.
    SearchRoom {
        room: RoomId,
        query: String,
        before: Option<RoomEventId>,
        limit: u32,
    },
    /// A preview of the page `url` that the room's message `event` links,
    /// but in encrypted rooms (SPEC §9.8). Not logged.
    PreviewUrl {
        room: RoomId,
        event: RoomEventId,
        url: String,
    },
    /// Creates a room, which the account joins. Answered once the
    /// homeserver has; sync then brings it.
    CreateMatrixRoom {
        account: String,
        room: matrix::NewRoom,
    },
    /// Opens a direct chat with `user`: the account's joined one, or one
    /// created, inviting them.
    StartDirectChat {
        account: String,
        user: String,
    },
    /// Joins a room by its address (`#alias:server`) or id.
    JoinMatrixRoom {
        account: String,
        room: String,
    },
    /// The room the address (`#alias:server`) is of, as the account's
    /// homeserver resolves it. Not logged.
    ResolveRoomAlias {
        account: String,
        alias: String,
    },
    /// The account's device, its trust, key backup and recovery, and the
    /// account's other devices (SPEC §9.4).
    GetMatrixSecurity {
        account: String,
    },
    /// Unlock Secret Storage with the recovery key or passphrase: the
    /// device then has the cross-signing keys, and so is verified, and the
    /// key backup's key. Not an op: it is the account's, not uplink's.
    RecoverMatrix {
        account: String,
        recovery: String,
    },
    /// Ask to verify uplink's device with another of the account's: `device`,
    /// or whichever answers first. Answered with the security.
    VerifyMatrixDevice {
        account: String,
        device: Option<String>,
    },
    /// Answer a verification. Answered with the security.
    AnswerVerification {
        account: String,
        flow_id: String,
        answer: matrix::VerificationAnswer,
    },
    /// Import room keys from an export (an uploaded blob), as another
    /// client makes it, encrypted with `passphrase`.
    ImportRoomKeys {
        account: String,
        blob: compose::BlobId,
        passphrase: String,
    },
    /// Join a room one is invited to, or leave or decline one (SPEC §9.7).
    ChangeMembership {
        room: RoomId,
        change: MembershipChange,
    },
    /// Reorder top-level spaces (MSC3230): each change is to a joined
    /// space of the same account.
    OrderSpaces {
        changes: Vec<matrix::SpaceOrderChange>,
    },
    /// Tag rooms and untag them (`m.tag`): added tags are `m.favourite`,
    /// `m.lowpriority` or the user's own, `u.` and a name. A favourite
    /// is not of low priority, and the other way.
    ModifyRoomTags {
        rooms: Vec<RoomId>,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// The events overlapping `from`–`to` (ms since the Unix epoch), of
    /// every account's calendar, by start.
    ListEvents {
        from: i64,
        to: i64,
    },
    /// All to-dos.
    ListTodos,
    /// Flagging a message for follow-up is creating a to-do of it.
    CreateTodo {
        title: String,
        notes: String,
        due: Option<String>,
        message: Option<MessageId>,
    },
    EditTodo {
        todo: TodoId,
        edit: TodoEdit,
    },
    DeleteTodo {
        todo: TodoId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerResponse {
    /// The undo was recorded as the log entry `tx_log_id`.
    Undone {
        tx_log_id: TxLogId,
    },
    /// The op was recorded as the log entry `tx_log_id`.
    Dispatched {
        tx_log_id: TxLogId,
    },
    /// Done, and nothing logged: of what is of the moment.
    Done,
    /// Of `GetMatrixSecurity`, `RecoverMatrix`, `VerifyMatrixDevice` and
    /// `AnswerVerification`: as the client has it
    /// now, not logged.
    MatrixSecurity {
        security: matrix::MatrixSecurity,
    },
    /// Of `ImportRoomKeys`: how many of the keys exported were new.
    RoomKeysImported {
        imported: u64,
        total: u64,
    },
    /// Query results reflect the log up to and including entry `as_of`;
    /// events of later entries apply on top.
    Accounts {
        as_of: TxLogId,
        accounts: Vec<mail::Account>,
    },
    Conversations {
        as_of: TxLogId,
        conversations: Vec<mail::Conversation>,
        /// Where the next page starts, if there may be one.
        next: Option<ListPosition>,
        /// The entries the list's conversations are made of, as label
        /// changes move entries into and out of them.
        scope: mail::ListScope,
    },
    Entries {
        as_of: TxLogId,
        entries: Vec<mail::Entry>,
    },
    /// Of `CountConversations`.
    ConversationCount {
        count: u64,
    },
    Home {
        as_of: TxLogId,
        buckets: Vec<HomeBucket>,
    },
    Items {
        as_of: TxLogId,
        conversations: Vec<mail::Conversation>,
        next: Option<ListPosition>,
        scope: mail::ListScope,
        /// Most recently active first.
        rooms: Vec<matrix::Room>,
        /// Rooms' messages, the latest first, up to a few.
        messages: Vec<matrix::FoundMessage>,
        /// Open ones first, by due date.
        todos: Vec<todos::Todo>,
        /// Those yet to end, soonest first, then those past, latest first.
        events: Vec<calendar::CalendarEvent>,
    },
    Message {
        message: mail::MessageContent,
    },
    Drafts {
        as_of: TxLogId,
        drafts: Vec<Draft>,
    },
    Outbox {
        as_of: TxLogId,
        accounts: Vec<compose::AccountOutbox>,
    },
    Rooms {
        as_of: TxLogId,
        accounts: Vec<matrix::MatrixAccount>,
        rooms: Vec<matrix::Room>,
    },
    Timeline {
        as_of: TxLogId,
        room: RoomId,
        events: Vec<matrix::RoomEvent>,
        /// Whether there are earlier events than those listed.
        more_before: bool,
        /// The gaps just before the events listed, and after the last, if
        /// it is the latest.
        gaps: Vec<matrix::TimelineGap>,
        /// Of a page that reaches the timeline's end (the latest, or one
        /// after an event that is not full): the room's messages being
        /// sent, sent and not yet synced back, or sent though cancelled,
        /// which follow it. With the events, so that one synced back
        /// shows in one place or the other, never both or neither.
        sends: Option<Vec<matrix::MatrixSend>>,
    },
    RoomEvents {
        as_of: TxLogId,
        room: RoomId,
        events: Vec<matrix::RoomEvent>,
    },
    /// Of `ListEdits`: the edit events, as stored.
    Edits {
        as_of: TxLogId,
        room: RoomId,
        event: RoomEventId,
        edits: Vec<matrix::RoomEvent>,
    },
    /// Of `SearchRoom`: whether there are more, before the last.
    RoomSearch {
        as_of: TxLogId,
        room: RoomId,
        events: Vec<matrix::RoomEvent>,
        more: bool,
    },
    /// Of `PreviewUrl`: `None` if the page says nothing of itself.
    UrlPreview {
        preview: Option<matrix::UrlPreview>,
    },
    /// Of `ListPinned`: in the order pinned.
    Pinned {
        as_of: TxLogId,
        room: RoomId,
        events: Vec<matrix::RoomEvent>,
    },
    /// Of `ListThread`: its messages, oldest first.
    Thread {
        as_of: TxLogId,
        room: RoomId,
        root: Box<matrix::RoomEvent>,
        replies: Vec<matrix::RoomEvent>,
    },
    /// Of `ListReceipts`: as the homeserver last said, not logged.
    Receipts {
        room: RoomId,
        receipts: Vec<matrix::MemberReceipt>,
    },
    /// Of `CreateMatrixRoom`, `StartDirectChat` and `JoinMatrixRoom`: the
    /// room joined, by its Matrix id, which sync lists soon if not yet.
    MatrixRoomJoined {
        room_id: String,
    },
    /// Of `ResolveRoomAlias`: the room, by its Matrix id.
    RoomAliasResolved {
        room_id: String,
    },
    /// Of `ListMembers`: as the account's client knows them, not logged.
    Members {
        room: RoomId,
        members: Vec<matrix::MemberInfo>,
        powers: matrix::RoomPowers,
    },
    Todos {
        as_of: TxLogId,
        todos: Vec<todos::Todo>,
    },
    Events {
        as_of: TxLogId,
        events: Vec<calendar::CalendarEvent>,
    },
    History {
        as_of: TxLogId,
        entries: Vec<HistoryEntry>,
        /// Whether there are earlier ones than those listed.
        more: bool,
    },
}

impl ServerResponse {
    /// The log entry up to which the response reflects the log. The server
    /// sends every entry up to it before the response, and only later ones
    /// after.
    pub fn position(&self) -> Option<TxLogId> {
        match self {
            Self::Undone { tx_log_id } | Self::Dispatched { tx_log_id } => Some(*tx_log_id),
            Self::Accounts { as_of, .. }
            | Self::Conversations { as_of, .. }
            | Self::Entries { as_of, .. }
            | Self::Items { as_of, .. }
            | Self::Home { as_of, .. }
            | Self::Drafts { as_of, .. }
            | Self::Outbox { as_of, .. }
            | Self::Rooms { as_of, .. }
            | Self::Timeline { as_of, .. }
            | Self::RoomEvents { as_of, .. }
            | Self::Edits { as_of, .. }
            | Self::Thread { as_of, .. }
            | Self::RoomSearch { as_of, .. }
            | Self::Pinned { as_of, .. }
            | Self::Todos { as_of, .. }
            | Self::Events { as_of, .. }
            | Self::History { as_of, .. } => Some(*as_of),
            // Message content does not change; a count is of the moment;
            // the rest is not logged.
            Self::Message { .. }
            | Self::ConversationCount { .. }
            | Self::Done
            | Self::Receipts { .. }
            | Self::Members { .. }
            | Self::MatrixRoomJoined { .. }
            | Self::RoomAliasResolved { .. }
            | Self::MatrixSecurity { .. }
            | Self::UrlPreview { .. }
            | Self::RoomKeysImported { .. } => None,
        }
    }
}

/// A bucket of the home view, as configured, and what is in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct HomeBucket {
    pub name: String,
    /// What it lists: its query, less what earlier buckets list.
    pub query: String,
    pub conversations: Vec<mail::Conversation>,
    /// Whether it lists more conversations than these.
    pub more: bool,
    pub rooms: Vec<matrix::Room>,
    pub todos: Vec<todos::Todo>,
    pub events: Vec<calendar::CalendarEvent>,
}

/// How a home bucket shows on the app's icon (SPEC §12.14): by how many
/// items are in it, or by a dot while any is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Badge {
    Count,
    Dot,
}

/// The badged home buckets, in order, and what is in each; and the
/// marks of the nav's places.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Badges {
    pub buckets: Vec<BadgedBucket>,
    pub marks: Marks,
}

/// Of each place of the nav that has a mark (SPEC §12.14), the mark:
/// those without are not listed. Folded places' are the client's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Marks {
    /// The icon's.
    pub home: Mark,
    pub labels: Vec<(mail::LabelId, Mark)>,
    /// Of each mail account's All Mail.
    pub accounts: Vec<(String, Mark)>,
    pub rooms: Vec<(RoomId, Mark)>,
    pub todos: Mark,
    pub calendar: Mark,
}

/// Of the badged buckets' items in a place: how many of those of `count`
/// buckets (a room by its messages that notify, or 1), and whether any of
/// a `dot` bucket's is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub count: u32,
    pub dot: bool,
}

impl Mark {
    pub fn add(&mut self, badge: Badge, count: u32) {
        match badge {
            Badge::Count => self.count += count,
            Badge::Dot => self.dot |= count > 0,
        }
    }

    pub fn is_none(&self) -> bool {
        *self == Mark::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct BadgedBucket {
    pub name: String,
    pub badge: Badge,
    /// Its items, each by its latest activity: up to `BADGE_LIMIT`, more
    /// than an icon counts (`99+`).
    pub items: Vec<Activity>,
}

/// The most items a badged bucket names.
pub const BADGE_LIMIT: usize = 100;

/// An item of a bucket, by its latest activity: what comes of it anew
/// (a reply, a message) is another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Activity {
    /// A conversation, by its latest entry.
    Mail {
        entry: EntryId,
    },
    /// A room, by its latest event, if any.
    Room {
        room: RoomId,
        event: Option<RoomEventId>,
    },
    Todo {
        todo: TodoId,
    },
    Event {
        event: calendar::EventId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ServerError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    /// The entry is an observation of external state, not a user op.
    NotUndoable,
    AlreadyUndone,
    /// The current state is not what the request expects (SPEC §6.2).
    Conflict,
    /// The request is malformed, e.g. names a label of another account.
    Invalid,
    Internal,
}

// ── log entries ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
    pub id: TxLogId,
    /// Milliseconds since the Unix epoch.
    pub ts: i64,
    pub source: Source,
    pub description: String,
    pub op_type: String,
    /// Set if this entry is the undo of entry `undoes`, which is thereby
    /// undone.
    pub undoes: Option<TxLogId>,
    /// What the entry changed, for clients to apply to their state.
    pub events: Vec<ServerEvent>,
}

/// A user's op as the history lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub entry: LogEntry,
    /// The entry that undid it, if it was undone.
    pub undone_by: Option<TxLogId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// An op the user dispatched; it can be undone.
    User,
    /// A change to external state, observed by syncing an account.
    Account { id: String },
}

/// A change to state that clients mirror (SPEC §5.4). Events carry ids
/// rather than content, since they are stored in the log; clients query
/// what they display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    EntriesAdded(mail::EntriesAdded),
    EntriesRemoved(mail::EntriesRemoved),
    LabelsChanged(mail::LabelsChanged),
    LabelListChanged(mail::LabelListChanged),
    ImageRuleChanged(mail::ImageRuleChanged),
    DraftEdited(compose::DraftEdited),
    DraftsChanged(compose::DraftsChanged),
    OutboxChanged(compose::OutboxChanged),
    RoomsChanged(matrix::RoomsChanged),
    TimelineAdded(matrix::TimelineAdded),
    TimelineFilled(matrix::TimelineFilled),
    TimelineChanged(matrix::TimelineChanged),
    MatrixSendsChanged(matrix::MatrixSendsChanged),
    TodosChanged(todos::TodosChanged),
    CalendarChanged(calendar::CalendarChanged),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_request_nests_body() {
        let msg: WsClientMsg = serde_json::from_value(json!({
            "type": "request",
            "request_id": 7,
            "body": { "type": "undo", "tx_log_id": 3 },
        }))
        .unwrap();
        assert_eq!(
            msg,
            WsClientMsg::Request {
                request_id: 7,
                body: ClientRequest::Undo {
                    tx_log_id: TxLogId(3)
                },
            }
        );
    }

    #[test]
    fn event_shape() {
        let event = ServerEvent::EntriesAdded(mail::EntriesAdded {
            entries: vec![EntryId(1)],
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json, json!({ "type": "entries_added", "entries": [1] }));
        assert_eq!(serde_json::from_value::<ServerEvent>(json).unwrap(), event);
        assert!(serde_json::from_value::<ServerEvent>(
            json!({ "type": "entries_added", "entries": [1], "x": 2 })
        )
        .is_err());
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(
            serde_json::from_value::<WsClientMsg>(json!({"type": "ping", "seq": 1, "x": 2}))
                .is_err()
        );
    }

    #[test]
    fn entry_shape() {
        let msg = WsServerMsg::Entry {
            entry: LogEntry {
                id: TxLogId(5),
                ts: 1000,
                source: Source::Account { id: "gmail".into() },
                description: "d".into(),
                op_type: "t".into(),
                undoes: None,
                events: vec![],
            },
        };
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({
                "type": "entry",
                "entry": {
                    "id": 5,
                    "ts": 1000,
                    "source": { "type": "account", "id": "gmail" },
                    "description": "d",
                    "op_type": "t",
                    "undoes": null,
                    "events": [],
                },
            })
        );
    }
}
