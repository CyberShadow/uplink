//! The badges (SPEC §12.14): what the badged home buckets hold, kept
//! current in the hub, which sends them to every session; and what comes
//! into them from others, which it sends as notices (SPEC §12.16).

use crate::{
    items::{Badged, Changed, Items},
    seen::Watermark,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use uplink_protocol::Notice;
use uplink_realtime::Hub;

/// Time alone moves items, into `due:today` or out of `newer_than:`.
const EVERY: Duration = Duration::from_secs(60);

/// Keeps the hub's badges, `badged` derived at the watermark `since`:
/// after log entries (those that come while they are kept, at once), by
/// checking again what they name; and derived anew when the buckets are
/// configured anew, each minute, and after entries missed.
pub async fn keep(items: Items, hub: Arc<Hub>, badged: Badged, mut since: Watermark) {
    let mut badged = Some(badged);
    let mut entries = hub.subscribe();
    let mut notices = hub.notices();
    let mut minute = interval_at(Instant::now() + EVERY, EVERY);
    minute.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let mut changed = Changed::default();
        tokio::select! {
            entry = entries.recv() => match entry {
                Ok(entry) => changed.came(&entry),
                // Missed: what they named is unknown.
                Err(RecvError::Lagged(_)) => changed.everything = true,
                Err(RecvError::Closed) => unreachable!("the hub outlives the server"),
            },
            notice = notices.recv() => match notice {
                Ok(notice) if *notice == Notice::HomeChanged => changed.everything = true,
                Ok(_) => continue,
                // Lagged: one missed may have been of the buckets.
                Err(RecvError::Lagged(_)) => changed.everything = true,
                Err(RecvError::Closed) => unreachable!("the hub outlives the server"),
            },
            _ = minute.tick() => changed.everything = true,
        }
        // Those that came meanwhile are checked with these.
        loop {
            match entries.try_recv() {
                Ok(entry) => changed.came(&entry),
                Err(TryRecvError::Lagged(_)) => changed.everything = true,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => unreachable!("the hub outlives the server"),
            }
        }
        let derived = match badged.take() {
            Some(kept) => items.rebadged(kept, changed, since).await,
            // What it held was lost with a failure: anew.
            None => items.badged(Some(since)).await,
        };
        match derived {
            Ok((kept, derived)) => {
                badged = Some(kept);
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
