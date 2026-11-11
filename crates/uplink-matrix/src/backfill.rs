//! Fetching events missing from a room's timeline (SPEC §9.2): the
//! homeserver's page of them before a stored event.

use crate::{rooms::ObservedEvent, sync::timeline_event};
use anyhow::{Context, Result};
use matrix_sdk::{
    room::MessagesOptions,
    ruma::{api::client::filter::RoomEventFilter, EventId, UInt},
    Room,
};
use std::collections::HashMap;

/// Events fetched at once, as Element does.
const PAGE: u32 = 50;

/// A page of events before a stored one.
pub struct Page {
    /// The room version, whose rules redact them.
    pub version: String,
    /// Newest first.
    pub events: Vec<ObservedEvent>,
    /// Whether there are no earlier events.
    pub start: bool,
}

/// The page of events just before the room's event `before`. Pagination
/// tokens are not kept: the event's context gives one.
pub async fn page(room: &Room, before: &str) -> Result<Page> {
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
    Ok(Page {
        version: room
            .version()
            .unwrap_or(matrix_sdk::ROOM_VERSION_FALLBACK)
            .to_string(),
        events,
        start,
    })
}
