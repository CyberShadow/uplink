use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use uplink_protocol::{Badges, LogEntry, Notice};

/// Fans newly committed log entries out to all connected sessions,
/// notices of the moment, which are not logged, and the badges as they
/// are (SPEC §12.14).
///
/// Entries must be published in commit order. Sessions catch up on
/// anything older from the log itself (see [`crate::Backend::replay`]).
pub struct Hub {
    live: broadcast::Sender<Arc<LogEntry>>,
    notices: broadcast::Sender<Arc<Notice>>,
    /// Of no bucket until the server first sets them, before it serves.
    badges: watch::Sender<Arc<Badges>>,
}

/// How many notices a session may fall behind by; it misses those older.
const NOTICES: usize = 256;

impl Hub {
    /// `capacity` bounds how far a session may fall behind before it is
    /// disconnected (it can then reconnect and resume from the log).
    pub fn new(capacity: usize) -> Self {
        Self {
            live: broadcast::channel(capacity).0,
            notices: broadcast::channel(NOTICES).0,
            badges: watch::channel(Arc::new(Badges { buckets: vec![] })).0,
        }
    }

    /// Sessions are sent `badges` if they differ from those they were.
    pub fn set_badges(&self, badges: Badges) {
        self.badges.send_if_modified(|current| {
            let changed = **current != badges;
            if changed {
                *current = Arc::new(badges);
            }
            changed
        });
    }

    /// The badges, as they are and as they change.
    pub fn badges(&self) -> watch::Receiver<Arc<Badges>> {
        self.badges.subscribe()
    }

    pub fn notify(&self, notice: Notice) {
        // An error only means there are no sessions to deliver to.
        let _ = self.notices.send(Arc::new(notice));
    }

    /// Every notice from now on.
    pub fn notices(&self) -> broadcast::Receiver<Arc<Notice>> {
        self.notices.subscribe()
    }

    pub fn publish(&self, entry: LogEntry) {
        // An error only means there are no sessions to deliver to.
        let _ = self.live.send(Arc::new(entry));
    }

    /// Every entry published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<LogEntry>> {
        self.live.subscribe()
    }
}
