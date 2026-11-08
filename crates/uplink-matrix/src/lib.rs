//! Matrix (SPEC §9): rooms and timelines synced with matrix-rust-sdk, the
//! read marker, and plain-text messages sent through an outbox.

pub mod client;
pub mod membership;
pub mod outbox;
pub mod rooms;
pub mod service;
pub mod sync;

#[cfg(test)]
mod tests;

use uplink_tx_log::Registry;

pub fn register(registry: &mut Registry) {
    registry.register::<rooms::MarkRead>();
    registry.register::<outbox::SendMessage>();
    registry.register::<outbox::CancelMessage>();
    registry.register::<membership::ChangeMembership>();
    registry.register::<membership::RevertMembership>();
}
