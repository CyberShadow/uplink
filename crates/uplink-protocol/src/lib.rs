//! Wire types for the WebSocket protocol (SPEC §5).
//!
//! Every message is a JSON text frame carrying a `type` discriminator.
//! Payload enums nest under a `body` field rather than being flattened, so
//! their own `type` tags don't collide with the envelope's.

use serde::{Deserialize, Serialize};

/// Id of a log entry. Entries are numbered from 1 in commit order, so an
/// id also serves as a position in the event stream; 0 is the position
/// before the first entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TxLogId(pub i64);

// ── envelopes ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Undo a log entry. Undoing an undo entry is a redo.
    Undo { tx_log_id: TxLogId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerResponse {
    /// The undo was recorded as the log entry `tx_log_id`.
    Undone { tx_log_id: TxLogId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    /// The entry is an observation of external state, not a user op.
    NotUndoable,
    AlreadyUndone,
    /// The current state is not what the request expects (SPEC §6.2).
    Conflict,
    Internal,
}

// ── log entries ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// An op the user dispatched; it can be undone.
    User,
    /// A change to external state, observed by syncing an account.
    Account { id: String },
}

/// A change to state that clients mirror. Domain variants arrive with the
/// domains (SPEC §5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerEvent {}

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
