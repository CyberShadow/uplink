//! Wire types for the WebSocket protocol (SPEC §5).
//!
//! Every message is a JSON text frame carrying a `type` discriminator.
//! Payload enums nest under a `body` field rather than being flattened, so
//! their own `type` tags don't collide with the envelope's.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub mod compose;
pub mod mail;
pub mod matrix;
pub mod todos;

use compose::{Draft, DraftContent, DraftId, OutboxId, Splice};
use mail::{EntryId, EntryList, LabelId, ListPosition, MessageId};
use matrix::{MatrixOutboxId, RoomEventId, RoomId, TimelinePage};
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
    /// A page of the list, newest first, starting after `after`.
    ListEntries {
        list: EntryList,
        after: Option<ListPosition>,
        limit: u32,
    },
    /// Full-text search across all accounts, newest first.
    Search {
        query: String,
        limit: u32,
    },
    /// The message's entries, in every account, newest first.
    ListMessageEntries {
        message: MessageId,
    },
    LoadMessage {
        message: MessageId,
    },
    /// Add and remove labels on entries, all of the labels' account.
    /// Archiving is removing `INBOX`; marking read, removing `UNREAD`.
    ModifyLabels {
        entries: Vec<EntryId>,
        add: Vec<LabelId>,
        remove: Vec<LabelId>,
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
    /// Move the draft to the outbox, to be sent after the send delay.
    SendDraft {
        draft: DraftId,
    },
    /// Take a queued or failed item out of the outbox, back into drafts.
    CancelSend {
        item: OutboxId,
    },
    /// Outbox items not yet synced back from the provider.
    ListOutbox,
    /// The configured Matrix accounts and their rooms.
    ListRooms,
    /// Up to `limit` events of the room's timeline, oldest first.
    ListTimeline {
        room: RoomId,
        page: TimelinePage,
        limit: u32,
    },
    /// Messages being sent, of all rooms.
    ListMatrixOutbox,
    /// Send a plain-text message to the room.
    SendMatrixMessage {
        room: RoomId,
        body: String,
    },
    /// Take a queued or failed message out of the Matrix outbox.
    CancelMatrixSend {
        item: MatrixOutboxId,
    },
    /// Move the room's read marker to `event`: it and everything before
    /// it are read.
    MarkRoomRead {
        room: RoomId,
        event: RoomEventId,
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
    /// Query results reflect the log up to and including entry `as_of`;
    /// events of later entries apply on top.
    Accounts {
        as_of: TxLogId,
        accounts: Vec<mail::Account>,
    },
    Entries {
        as_of: TxLogId,
        entries: Vec<mail::Entry>,
        /// Where the next page starts, if there may be one.
        next: Option<ListPosition>,
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
        items: Vec<compose::OutboxItem>,
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
    },
    MatrixOutbox {
        as_of: TxLogId,
        items: Vec<matrix::MatrixOutboxItem>,
    },
    Todos {
        as_of: TxLogId,
        todos: Vec<todos::Todo>,
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
            | Self::Entries { as_of, .. }
            | Self::Drafts { as_of, .. }
            | Self::Outbox { as_of, .. }
            | Self::Rooms { as_of, .. }
            | Self::Timeline { as_of, .. }
            | Self::MatrixOutbox { as_of, .. }
            | Self::Todos { as_of, .. }
            | Self::History { as_of, .. } => Some(*as_of),
            // Message content does not change.
            Self::Message { .. } => None,
        }
    }
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
    DraftEdited(compose::DraftEdited),
    DraftsChanged(compose::DraftsChanged),
    OutboxChanged(compose::OutboxChanged),
    RoomsChanged(matrix::RoomsChanged),
    TimelineAdded(matrix::TimelineAdded),
    MatrixOutboxChanged(matrix::MatrixOutboxChanged),
    TodosChanged(todos::TodosChanged),
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
