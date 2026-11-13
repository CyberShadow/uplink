//! Answers the calendar requests of the WebSocket protocol (SPEC §5.4).

use arc_swap::ArcSwap;
use std::sync::Arc;
use uplink_protocol::{
    calendar::{EventId, TimeZone},
    ErrorCode, ServerResponse,
};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The longest stretch one request may list.
const MAX_RANGE_MS: i64 = 400 * 86_400_000;

#[derive(Clone)]
pub struct Calendar {
    reader: Reader,
    time_zones: Arc<ArcSwap<Vec<TimeZone>>>,
}

impl Calendar {
    /// `reader` is a read-only connection to the database.
    pub fn new(reader: Connection, time_zones: Vec<TimeZone>) -> Self {
        Self {
            reader: Reader::new(reader),
            time_zones: Arc::new(ArcSwap::from_pointee(time_zones)),
        }
    }

    pub fn configure(&self, time_zones: Vec<TimeZone>) {
        self.time_zones.store(Arc::new(time_zones));
    }

    pub async fn list(&self, from: i64, to: i64) -> Response {
        if from >= to || to - from > MAX_RANGE_MS {
            return Err(error(
                ErrorCode::Invalid,
                format!("{from}–{to} is not a range of at most 400 days"),
            ));
        }
        let time_zones = Vec::clone(&self.time_zones.load());
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let events = crate::events_between(&tx, from, to).map_err(internal)?;
                Ok(ServerResponse::Events {
                    as_of,
                    events,
                    time_zones,
                })
            })
            .await
    }

    pub async fn get(&self, id: EventId) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let (event, raw) = crate::event(&tx, id)
                    .map_err(internal)?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no event {}", id.0)))?;
                let details = crate::details::details(event, &raw).map_err(internal)?;
                Ok(ServerResponse::Event {
                    as_of,
                    details: Box::new(details),
                })
            })
            .await
    }
}
