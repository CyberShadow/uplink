//! Fetching events missing from a room's timeline (SPEC §9.2): the
//! homeserver's page of them before a stored event.

use crate::{rooms::Filled, sync::timeline_event};
use anyhow::{Context, Result};
use matrix_sdk::{
    room::MessagesOptions,
    ruma::{
        api::client::filter::RoomEventFilter,
        events::receipt::{ReceiptThread, ReceiptType},
        EventId, UInt,
    },
    Room,
};
use std::collections::HashMap;
use uplink_protocol::matrix::{GapId, RoomId};
use uplink_tx_log::Dispatcher;

/// Events fetched at once, as Element does.
const PAGE: u32 = 50;

/// Fetches the page of events just before the gap `gap` of the room
/// (`sdk_room`, the account's client's), whose event after it is
/// `before`, and stores them.
pub async fn fill(
    dispatcher: &Dispatcher,
    account: String,
    sdk_room: &Room,
    room: RoomId,
    gap: GapId,
    before: &str,
) -> Result<()> {
    let filled = page(sdk_room, room, gap, before).await?;
    dispatcher.observe(account, filled).await?;
    Ok(())
}

/// The page of events just before the room's event `before`. Pagination
/// tokens are not kept: the event's context gives one.
async fn page(room: &Room, id: RoomId, gap: GapId, before: &str) -> Result<Filled> {
    let before = <&EventId>::try_from(before)?;
    let context = room
        .event_with_context(before, true, UInt::from(0u32), None)
        .await
        .with_context(|| format!("fetching the context of {before}"))?;
    let token = context
        .prev_batch_token
        .with_context(|| format!("the homeserver gave no token before {before}"))?;
    let mut options = MessagesOptions::backward().from(token.as_str());
    options.limit = UInt::from(PAGE);
    options.filter = RoomEventFilter::with_lazy_loading();
    let messages = room
        .messages(options)
        .await
        .with_context(|| format!("fetching events before {before}"))?;
    // The senders' names then, as the page's member events say.
    let mut names: HashMap<String, Option<String>> = HashMap::new();
    for state in &messages.state {
        let event: serde_json::Value = serde_json::from_str(state.json().get())?;
        if event["type"] == "m.room.member" {
            if let Some(user) = event["state_key"].as_str() {
                names.insert(
                    user.to_owned(),
                    event["content"]["displayname"].as_str().map(str::to_owned),
                );
            }
        }
    }
    let start = messages.end.is_none();
    let mut events = Vec::with_capacity(messages.chunk.len());
    for event in messages.chunk {
        events.push(timeline_event(room, event, &mut names).await?);
    }
    Ok(Filled {
        room: id,
        gap,
        version: room
            .version()
            .unwrap_or(matrix_sdk::ROOM_VERSION_FALLBACK)
            .to_string(),
        events,
        start,
        own_receipts: own_receipts(room).await?,
        fully_read: room.fully_read_event_id().map(|e| e.to_string()),
    })
}

/// The events of the account's own read receipts in the room, as its
/// client keeps them: public and private, of the timeline as a whole and
/// of its main part.
async fn own_receipts(room: &Room) -> Result<Vec<String>> {
    let own = room.own_user_id();
    let mut events = vec![];
    for kind in [ReceiptType::Read, ReceiptType::ReadPrivate] {
        for thread in [ReceiptThread::Unthreaded, ReceiptThread::Main] {
            if let Some((event, _)) = room.load_user_receipt(kind.clone(), &thread, own).await? {
                events.push(event.to_string());
            }
        }
    }
    Ok(events)
}
