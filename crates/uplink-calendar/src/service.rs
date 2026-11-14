//! Answers the calendar requests of the WebSocket protocol (SPEC §5.4).

use crate::respond::{answerable, Respond};
use arc_swap::ArcSwap;
use std::sync::Arc;
use uplink_protocol::{
    calendar::{EventId, EventResponse, TimeZone},
    ErrorCode, ServerResponse,
};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{
    answer::{error, internal, Reader, Response},
    Dispatcher,
};

/// The longest stretch one request may list.
const MAX_RANGE_MS: i64 = 400 * 86_400_000;

#[derive(Clone)]
pub struct Calendar {
    dispatcher: Dispatcher,
    reader: Reader,
    time_zones: Arc<ArcSwap<Vec<TimeZone>>>,
}

impl Calendar {
    /// `reader` is a read-only connection to the database.
    pub fn new(dispatcher: Dispatcher, reader: Connection, time_zones: Vec<TimeZone>) -> Self {
        Self {
            dispatcher,
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

    /// Answers the meeting request `event` as `response`.
    pub async fn respond(&self, event: EventId, response: EventResponse) -> Response {
        if crate::respond::action(response).is_none() {
            return Err(error(
                ErrorCode::Invalid,
                format!("{response:?} is not an answer to give"),
            ));
        }
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let (found, _) = crate::event(&tx, event)
                    .map_err(internal)?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no event {}", event.0)))?;
                // As shown: an answer asked for is not of the organizer.
                if !answerable(found.response) {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!("'{}' is not a meeting request to answer", found.subject),
                    ));
                }
                Ok(())
            })
            .await?;
        self.dispatcher.answer(Respond { event, response }).await
    }
}
