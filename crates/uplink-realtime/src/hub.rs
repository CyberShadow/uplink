use std::sync::Arc;
use tokio::sync::broadcast;
use uplink_protocol::LogEntry;

/// Fans newly committed log entries out to all connected sessions.
///
/// Entries must be published in commit order. Sessions catch up on
/// anything older from the log itself (see [`crate::Backend::replay`]).
pub struct Hub {
    live: broadcast::Sender<Arc<LogEntry>>,
}

impl Hub {
    /// `capacity` bounds how far a session may fall behind before it is
    /// disconnected (it can then reconnect and resume from the log).
    pub fn new(capacity: usize) -> Self {
        Self {
            live: broadcast::channel(capacity).0,
        }
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
