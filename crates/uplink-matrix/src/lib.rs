//! Matrix (SPEC §9): rooms and timelines synced with matrix-rust-sdk, the
//! read marker, joining and leaving, the order of spaces, rooms' tags,
//! and messages sent.

pub mod backfill;
pub mod client;
pub mod html;
pub mod media;
pub mod membership;
pub mod outbox;
pub mod preview;
pub mod query;
pub mod room_tags;
pub mod rooms;
pub mod security;
pub mod send;
pub mod service;
pub mod space_order;
pub mod sync;
pub mod typing;
pub mod verification;

#[cfg(test)]
mod tests;

use uplink_tx_log::Registry;

pub fn register(registry: &mut Registry) {
    registry.register::<rooms::MarkRead>();
    registry.register::<rooms::MarkFullyRead>();
    registry.register::<send::SendMessage>();
    registry.register::<send::CancelMessage>();
    registry.register::<send::KeepSent>();
    registry.register::<membership::ChangeMembership>();
    registry.register::<membership::RevertMembership>();
    registry.register::<space_order::OrderSpaces>();
    registry.register::<room_tags::SetRoomTags>();
}
