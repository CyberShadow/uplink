//! Wire types for the WebSocket protocol (SPEC §5).
//!
//! Every message is a JSON text frame carrying a `type` discriminator.
//! Payload enums nest under a `body` field rather than being flattened, so
//! their own `type` tags don't collide with the envelope's.

use serde::{Deserialize, Serialize};

/// Monotonic position in the server's event stream. Only meaningful
/// together with the [`Epoch`] it was issued under.
pub type Cursor = u64;

/// Identifies one server process lifetime. Cursors are not persisted, so a
/// cursor from a different epoch cannot be resumed from.
pub type Epoch = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        /// Where the client left off on a previous connection, if anywhere.
        resume: Option<Resume>,
    },
    /// The server answers with a `Response` or `RequestFailed` carrying the
    /// same `request_id`.
    Request {
        request_id: u64,
        body: ClientRequest,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    pub epoch: Epoch,
    /// Cursor of the last event the client received.
    pub cursor: Cursor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WsServerMsg {
    Pong {
        seq: u64,
    },
    /// Reply to `Hello`.
    ///
    /// If `resumed`, every event after the client's resume cursor follows
    /// immediately, then live events; the client keeps tracking its own
    /// cursor. Otherwise the client must discard its state and reload, and
    /// adopt `cursor` as its baseline; only events after it will follow.
    Welcome {
        epoch: Epoch,
        /// Cursor of the most recent event at the time of `Hello`.
        cursor: Cursor,
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
    Event {
        cursor: Cursor,
        body: ServerEvent,
    },
}

// ── requests ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Undo a tx_log entry. Undoing an undo entry is a redo.
    Undo { tx_log_id: TxLogId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerResponse {
    /// The undo was recorded as the tx_log entry `tx_log_id`.
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
    AlreadyUndone,
    /// The current state is not what the request expects (SPEC §6.2).
    Conflict,
    Internal,
}

// ── events ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerEvent {
    TxLogEntryAdded { entry: TxLogEntry },
    TxLogEntryUndone { id: TxLogId, undone_by: TxLogId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxLogEntry {
    pub id: TxLogId,
    /// Milliseconds since the Unix epoch.
    pub ts: i64,
    pub actor: String,
    pub description: String,
    pub op_type: String,
    /// Set if this entry is the undo of another entry.
    pub undoes: Option<TxLogId>,
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
    fn unknown_fields_rejected() {
        assert!(
            serde_json::from_value::<WsClientMsg>(json!({"type": "ping", "seq": 1, "x": 2}))
                .is_err()
        );
    }

    #[test]
    fn server_event_shape() {
        let msg = WsServerMsg::Event {
            cursor: 5,
            body: ServerEvent::TxLogEntryUndone {
                id: TxLogId(1),
                undone_by: TxLogId(2),
            },
        };
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({
                "type": "event",
                "cursor": 5,
                "body": { "type": "tx_log_entry_undone", "id": 1, "undone_by": 2 },
            })
        );
    }
}
