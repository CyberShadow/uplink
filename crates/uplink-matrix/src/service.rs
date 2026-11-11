//! Answers the Matrix requests of the WebSocket protocol (SPEC §5.4).

use crate::{
    client::{self, Clients},
    membership::ChangeMembership,
    rooms::{self, MarkRead},
    send::{self, CancelMessage, KeepSent, SendMessage},
};
use std::sync::Arc;
use uplink_protocol::{
    matrix::{
        MatrixAccount, MatrixSendId, Membership, MembershipChange, RoomEventId, RoomId,
        TimelinePage,
    },
    ErrorCode, ServerError, ServerResponse,
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
    clients: Clients,
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
            clients: Clients::default(),
        }
    }

    /// The accounts' clients, which their sync tasks restore.
    pub fn clients(&self) -> Clients {
        self.clients.clone()
    }

    /// A thumbnail of the room's avatar `mxc`, and its media type: only of
    /// its current one, so that the client cannot have the server fetch
    /// whatever it names.
    pub async fn avatar(
        &self,
        room: RoomId,
        mxc: String,
    ) -> Result<(&'static str, Vec<u8>), ServerError> {
        let account = self
            .reader
            .read({
                let mxc = mxc.clone();
                move |conn| match rooms::room_avatar(conn, room).map_err(internal)? {
                    Some((account, Some(avatar))) if avatar == mxc => Ok(account),
                    Some(_) => Err(error(
                        ErrorCode::NotFound,
                        format!("{mxc} is not the avatar of room {}", room.0),
                    )),
                    None => Err(no_room(room)),
                }
            })
            .await?;
        let client = self.clients.get(&account).ok_or_else(|| {
            error(
                ErrorCode::NotFound,
                format!("{account} is not connected to its homeserver yet"),
            )
        })?;
        let image = client::avatar_thumbnail(&client, &mxc)
            .await
            .map_err(internal)?;
        let mime = image_type(&image).ok_or_else(|| {
            internal(anyhow::anyhow!(
                "the thumbnail of {mxc} is not an image shown"
            ))
        })?;
        Ok((mime, image))
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

    pub async fn list_room_events(&self, room: RoomId, events: Vec<RoomEventId>) -> Response {
        if events.len() > MAX_LIMIT as usize {
            return Err(error(
                ErrorCode::Invalid,
                format!("at most {MAX_LIMIT} events at once"),
            ));
        }
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                if rooms::room_account(&tx, room).map_err(internal)?.is_none() {
                    return Err(no_room(room));
                }
                let events = rooms::listed_events(&tx, room, &events).map_err(internal)?;
                Ok(ServerResponse::RoomEvents {
                    as_of,
                    room,
                    events,
                })
            })
            .await
    }

    pub async fn list_edits(&self, room: RoomId, event: RoomEventId) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                if !rooms::event_in_room(&tx, room, event).map_err(internal)? {
                    return Err(no_event(room, event));
                }
                let edits = rooms::edits(&tx, room, event).map_err(internal)?;
                Ok(ServerResponse::Edits {
                    as_of,
                    room,
                    event,
                    edits,
                })
            })
            .await
    }

    /// The room's sends not yet reconciled, for the end of its timeline.
    pub async fn list_sends(&self, room: RoomId) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                rooms::room_account(&tx, room)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(room))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let sends = send::sends(&tx, room).map_err(internal)?;
                Ok(ServerResponse::MatrixSends { as_of, room, sends })
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

    pub async fn cancel_send(&self, item: MatrixSendId) -> Response {
        self.dispatcher.answer(CancelMessage { item }).await
    }

    pub async fn keep_sent(&self, item: MatrixSendId) -> Response {
        self.dispatcher.answer(KeepSent { item }).await
    }

    /// Joining only rooms one is invited to; leaving joined rooms, or
    /// declining invitations.
    pub async fn change_membership(&self, room: RoomId, change: MembershipChange) -> Response {
        let accounts = self.accounts.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let account = rooms::room_account(&tx, room)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(room))?;
                let configured = accounts
                    .iter()
                    .find(|a| a.id == account)
                    .ok_or_else(|| no_room(room))?;
                let listed = rooms::rooms(&tx, &account, &configured.mxid)
                    .map_err(internal)?
                    .into_iter()
                    .find(|r| r.id == room)
                    .ok_or_else(|| no_room(room))?;
                let allowed = match change {
                    MembershipChange::Join => listed.membership == Membership::Invited,
                    MembershipChange::Leave => listed.membership != Membership::Left,
                };
                if !allowed {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!("cannot {change:?} room {} as it is", room.0),
                    ));
                }
                Ok(())
            })
            .await?;
        self.dispatcher
            .answer(ChangeMembership { room, change })
            .await
    }

    pub async fn mark_read(&self, room: RoomId, event: RoomEventId) -> Response {
        self.reader
            .read(move |conn| {
                if rooms::event_in_room(conn, room, event).map_err(internal)? {
                    Ok(())
                } else {
                    Err(no_event(room, event))
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

/// The media type of the image formats homeservers make thumbnails in,
/// by their signatures; the homeserver's word for it is not kept.
fn image_type(image: &[u8]) -> Option<&'static str> {
    match image {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

fn no_room(room: RoomId) -> uplink_protocol::ServerError {
    error(ErrorCode::NotFound, format!("no room {}", room.0))
}

fn no_event(room: RoomId, event: RoomEventId) -> ServerError {
    error(
        ErrorCode::NotFound,
        format!("no event {} in room {}", event.0, room.0),
    )
}
