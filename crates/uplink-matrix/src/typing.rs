//! Whether the account is typing in a room, as desired state (SPEC §9.5):
//! a request records it, and a task per room tells the homeserver the
//! latest, so that a request never waits on the homeserver, and "stopped"
//! never goes out before the "typing" it follows.

use crate::client::Clients;
use matrix_sdk::ruma::OwnedRoomId;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
use uplink_protocol::matrix::RoomId;

/// The rooms' wanted typing, each with its task.
#[derive(Clone, Default)]
pub struct Typing(Arc<Mutex<HashMap<RoomId, watch::Sender<bool>>>>);

impl Typing {
    /// The account `account` is typing in the room, or has stopped.
    pub fn set(
        &self,
        clients: &Clients,
        account: String,
        room: RoomId,
        room_id: OwnedRoomId,
        typing: bool,
    ) {
        let mut rooms = self.0.lock().expect("typing poisoned");
        match rooms.get(&room) {
            Some(wanted) => {
                wanted.send_replace(typing);
            }
            None => {
                let (wanted, receiver) = watch::channel(typing);
                tokio::spawn(tell(clients.clone(), account, room_id, receiver));
                rooms.insert(room, wanted);
            }
        }
    }
}

/// Tells the homeserver the latest typing wanted, each time it changes,
/// for as long as the server runs. Typing is of the moment: a failure is
/// logged, and the next change tells it anew.
async fn tell(
    clients: Clients,
    account: String,
    room_id: OwnedRoomId,
    mut wanted: watch::Receiver<bool>,
) {
    loop {
        let typing = *wanted.borrow_and_update();
        // The client of the moment: it is restored anew on reconnecting.
        let room = clients
            .get(&account)
            .and_then(|client| client.get_room(&room_id));
        match room {
            Some(room) => {
                if let Err(e) = room.typing_notice(typing).await {
                    tracing::warn!(account, room = %room_id, "telling typing: {e}");
                }
            }
            None => {
                tracing::warn!(account, room = %room_id, "telling typing: the client has no such room")
            }
        }
        if wanted.changed().await.is_err() {
            return;
        }
    }
}
