//! Answers the Matrix requests of the WebSocket protocol (SPEC §5.4).

use crate::{
    outbox::{self, CancelMessage, SendMessage},
    rooms::{self, MarkRead},
};
use std::sync::Arc;
use uplink_protocol::{
    matrix::{MatrixAccount, MatrixOutboxId, Membership, RoomEventId, RoomId, TimelinePage},
    ErrorCode, ServerResponse,
};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{
    answer::{error, internal, Reader, Response},
    Dispatcher,
};

/// The most events a timeline request may ask for.
pub const MAX_LIMIT: u32 = 500;

/// A configured Matrix account.
#[derive(Debug, Clone)]
pub struct ConfiguredAccount {
    pub id: String,
    pub mxid: String,
}

#[derive(Clone)]
pub struct Matrix {
    dispatcher: Dispatcher,
    reader: Reader,
    accounts: Arc<Vec<ConfiguredAccount>>,
}

impl Matrix {
    /// `reader` is a read-only connection to the database `dispatcher`
    /// writes.
    pub fn new(
        dispatcher: Dispatcher,
        reader: Connection,
        accounts: Vec<ConfiguredAccount>,
    ) -> Self {
        Self {
            dispatcher,
            reader: Reader::new(reader),
            accounts: Arc::new(accounts),
        }
    }

    pub async fn list_rooms(&self) -> Response {
        let configured = self.accounts.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let mut accounts = vec![];
                let mut rooms = vec![];
                for account in configured.iter() {
                    accounts.push(MatrixAccount {
                        id: account.id.clone(),
                        mxid: account.mxid.clone(),
                        synced: rooms::sync_state(&tx, &account.id)
                            .map_err(internal)?
                            .is_some(),
                    });
                    rooms.extend(rooms::rooms(&tx, &account.id, &account.mxid).map_err(internal)?);
                }
                Ok(ServerResponse::Rooms {
                    as_of,
                    accounts,
                    rooms,
                })
            })
            .await
    }

    pub async fn list_timeline(&self, room: RoomId, page: TimelinePage, limit: u32) -> Response {
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(error(
                ErrorCode::Invalid,
                format!("limit must be between 1 and {MAX_LIMIT}"),
            ));
        }
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                if rooms::room_account(&tx, room).map_err(internal)?.is_none() {
                    return Err(no_room(room));
                }
                let (events, more_before) =
                    rooms::timeline(&tx, room, page, limit).map_err(internal)?;
                Ok(ServerResponse::Timeline {
                    as_of,
                    room,
                    events,
                    more_before,
                })
            })
            .await
    }

    pub async fn list_outbox(&self) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let items = outbox::items(&tx).map_err(internal)?;
                Ok(ServerResponse::MatrixOutbox { as_of, items })
            })
            .await
    }

    /// Only to joined rooms of configured accounts.
    pub async fn send_message(&self, room: RoomId, body: String) -> Response {
        if body.trim().is_empty() {
            return Err(error(ErrorCode::Invalid, "the message is empty".into()));
        }
        let accounts = self.accounts.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let account = rooms::room_account(&tx, room)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(room))?;
                let configured = accounts.iter().find(|a| a.id == account);
                let joined = rooms::rooms(&tx, &account, configured.map_or("", |a| &a.mxid))
                    .map_err(internal)?
                    .into_iter()
                    .any(|r| r.id == room && r.membership == Membership::Joined);
                if configured.is_none() || !joined {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!(
                            "room {} is not a joined room of a configured account",
                            room.0
                        ),
                    ));
                }
                Ok(())
            })
            .await?;
        self.dispatcher.answer(SendMessage { room, body }).await
    }

    pub async fn cancel_send(&self, item: MatrixOutboxId) -> Response {
        self.dispatcher.answer(CancelMessage { item }).await
    }

    pub async fn mark_read(&self, room: RoomId, event: RoomEventId) -> Response {
        self.reader
            .read(move |conn| {
                if rooms::event_in_room(conn, room, event).map_err(internal)? {
                    Ok(())
                } else {
                    Err(error(
                        ErrorCode::NotFound,
                        format!("no event {} in room {}", event.0, room.0),
                    ))
                }
            })
            .await?;
        self.dispatcher
            .answer(MarkRead {
                room,
                up_to: Some(event),
            })
            .await
    }
}

fn no_room(room: RoomId) -> uplink_protocol::ServerError {
    error(ErrorCode::NotFound, format!("no room {}", room.0))
}
