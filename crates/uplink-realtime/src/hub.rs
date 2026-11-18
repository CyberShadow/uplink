use crate::shared::Shared;
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::{broadcast, watch};
use uplink_protocol::{Badges, LogEntry, Notice, Place};

/// The places some session attends (SPEC §12.15).
pub type Attended = Arc<HashSet<Place>>;

/// Fans newly committed log entries out to all connected sessions,
/// notices of the moment, which are not logged, and the badges as they
/// are (SPEC §12.14); and keeps their windows' listings, shared (SPEC
/// §5.7).
///
/// Entries must be published in commit order. Sessions catch up on
/// anything older from the log itself (see [`crate::Backend::replay`]).
pub struct Hub {
    live: broadcast::Sender<Arc<LogEntry>>,
    notices: broadcast::Sender<Arc<Notice>>,
    /// Of no bucket until the server first sets them, before it serves.
    badges: watch::Sender<Arc<Badges>>,
    /// What each session attends, by its number; and the places of all,
    /// as they change.
    attending: Mutex<HashMap<u64, Place>>,
    attended: watch::Sender<Attended>,
    sessions: AtomicU64,
    windows: Arc<Shared>,
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
            badges: watch::channel(Arc::new(Badges::default())).0,
            attending: Mutex::new(HashMap::new()),
            attended: watch::channel(Attended::default()).0,
            sessions: AtomicU64::new(0),
            windows: Arc::default(),
        }
    }

    /// The windows' listings, kept by [`crate::keep_windows`].
    pub fn windows(&self) -> &Arc<Shared> {
        &self.windows
    }

    /// A number for a session, of none other.
    pub fn session(&self) -> u64 {
        self.sessions.fetch_add(1, Ordering::Relaxed)
    }

    /// `session` attends `place` now, or none.
    pub fn attend(&self, session: u64, place: Option<Place>) {
        let mut attending = self.attending.lock().expect("no holder panics");
        match place {
            Some(place) => attending.insert(session, place),
            None => attending.remove(&session),
        };
        let places: HashSet<Place> = attending.values().cloned().collect();
        self.attended.send_if_modified(|current| {
            let changed = **current != places;
            if changed {
                *current = Arc::new(places);
            }
            changed
        });
    }

    /// The places attended, as they are and as they change.
    pub fn attended(&self) -> watch::Receiver<Attended> {
        self.attended.subscribe()
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
