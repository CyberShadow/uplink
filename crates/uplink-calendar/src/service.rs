//! Answers the calendar requests of the WebSocket protocol (SPEC §5.4).

use uplink_protocol::{ErrorCode, ServerResponse};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The longest stretch one request may list.
const MAX_RANGE_MS: i64 = 400 * 86_400_000;

#[derive(Clone)]
pub struct Calendar {
    reader: Reader,
}

impl Calendar {
    /// `reader` is a read-only connection to the database.
    pub fn new(reader: Connection) -> Self {
        Self {
            reader: Reader::new(reader),
        }
    }

    pub async fn list(&self, from: i64, to: i64) -> Response {
        if from >= to || to - from > MAX_RANGE_MS {
            return Err(error(
                ErrorCode::Invalid,
                format!("{from}–{to} is not a range of at most 400 days"),
            ));
        }
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let events = crate::events_between(&tx, from, to).map_err(internal)?;
                Ok(ServerResponse::Events { as_of, events })
            })
            .await
    }
}
