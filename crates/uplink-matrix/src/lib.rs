//! Matrix (SPEC §9): rooms and timelines synced with matrix-rust-sdk, the
//! read marker, joining and leaving, and plain-text messages sent.

pub mod client;
pub mod html;
pub mod media;
pub mod membership;
pub mod outbox;
pub mod rooms;
pub mod security;
pub mod send;
pub mod service;
pub mod sync;
pub mod verification;

#[cfg(test)]
mod tests;

use uplink_tx_log::Registry;

pub fn register(registry: &mut Registry) {
    registry.register::<rooms::MarkRead>();
    registry.register::<send::SendMessage>();
    registry.register::<send::CancelMessage>();
    registry.register::<send::KeepSent>();
    registry.register::<membership::ChangeMembership>();
    registry.register::<membership::RevertMembership>();
}
