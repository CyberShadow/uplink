//! The badges (SPEC §12.14): what the badged home buckets hold, kept
//! current in the hub, which sends them to every session; and what comes
//! into them from others, which it sends as notices (SPEC §12.16).

use crate::{items::Items, seen::Watermark};
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use uplink_protocol::Notice;
use uplink_realtime::Hub;

/// Time alone moves items, into `due:today` or out of `newer_than:`.
const EVERY: Duration = Duration::from_secs(60);

/// Sets the hub's badges anew after log entries (those that come while
/// they are read, at once), when the buckets are configured anew, and
/// each minute, with notices of what came since `since`, the watermark
/// the hub's, set once already, were derived at.
pub async fn keep(items: Items, hub: Arc<Hub>, mut since: Watermark) {
    let mut entries = hub.subscribe();
    let mut notices = hub.notices();
    let mut minute = interval_at(Instant::now() + EVERY, EVERY);
    minute.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            entry = entries.recv() => match entry {
                // Lagged: entries came, unseen.
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => unreachable!("the hub outlives the server"),
            },
            notice = notices.recv() => match notice {
                Ok(notice) if *notice == Notice::HomeChanged => {}
                Ok(_) => continue,
                // Lagged: one missed may have been of the buckets.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => unreachable!("the hub outlives the server"),
            },
            _ = minute.tick() => {}
        }
        // Those that came meanwhile are read with these.
        loop {
            match entries.try_recv() {
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => unreachable!("the hub outlives the server"),
            }
        }
        match items.badges(Some(since)).await {
            Ok(derived) => {
                hub.set_badges(derived.badges);
                for arrival in derived.arrivals {
                    hub.notify(Notice::Arrival(arrival));
                }
                since = derived.watermark;
            }
            Err(e) => tracing::error!("badges: reading them failed: {}", e.message),
        }
    }
}
