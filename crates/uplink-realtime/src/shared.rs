//! Windows shared by the sessions (SPEC §5.7): of each query, one
//! listing, kept current once for every session subscribed to it, and,
//! pinned, kept while none is (home's, §12.13), so that it shows at once.

use crate::{hub::Hub, session::Backend, window::Listable};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{
        broadcast::error::{RecvError, TryRecvError},
        watch, Notify,
    },
    time::{interval_at, Instant, MissedTickBehavior},
};
use uplink_protocol::{
    matrix::RoomId, Changed, Item, Of, ServerError, TxLogId, UnknownNames, Unseen,
};

/// Time alone changes what queries match, into `due:today` or out of
/// `newer_than:`: listings are checked again this often.
const EVERY: Duration = Duration::from_secs(60);

/// What a listing is of: subscriptions to the same share it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub of: Of,
    pub query: String,
    pub unknown: UnknownNames,
}

/// A listing, as last listed.
#[derive(Clone)]
pub enum State<T: Listable> {
    Unlisted,
    Listed(Arc<Snapshot<T>>),
    /// It failed to list again, which is the server's failure (or a
    /// label's that is gone): it is listed no more, and subscribing anew
    /// lists anew.
    Failed(ServerError),
}

/// What a query lists, as of a log position: its first `limit`.
pub struct Snapshot<T: Listable> {
    pub limit: u32,
    pub as_of: TxLogId,
    /// The first of what the query matches, in their order.
    pub listed: Vec<T>,
    /// Whether it matches more.
    pub more: bool,
    pub scope: T::Scope,
    pub partial_rooms: Vec<RoomId>,
}

impl<T: Listable> Snapshot<T> {
    /// Its first `limit`, of no more than it lists, and whether the query
    /// matches more.
    pub fn first(&self, limit: u32) -> (&[T], bool) {
        assert!(limit <= self.limit, "a snapshot lists its first only");
        let shown = self.listed.len().min(limit as usize);
        (
            &self.listed[..shown],
            self.more || self.listed.len() > shown,
        )
    }
}

/// A listing's watch, of what it lists.
pub enum Sender {
    Items(watch::Sender<State<Item>>),
    Unseen(watch::Sender<State<Unseen>>),
}

/// The listings, kept by [`keep`].
#[derive(Default)]
pub struct Shared {
    listings: Mutex<Listings>,
    /// A listing is wanted: new, or of more than it lists.
    wanted: Notify,
}

#[derive(Default)]
struct Listings {
    /// Of `entries`, by what each is of: those that list, not those that
    /// failed.
    by_key: HashMap<Key, u64>,
    entries: HashMap<u64, Entry>,
    /// Of `entries`, those that may have changed since last listed, the
    /// longest due first: each once.
    due: VecDeque<u64>,
    /// What the pins hold: of each its entry, and its holder.
    pins: Vec<(u64, u64)>,
    /// The next number of an entry, or of a holder.
    next: u64,
}

struct Entry {
    key: Key,
    /// What each holder (a subscription or a pin) wants listed of it:
    /// how many.
    holders: HashMap<u64, u32>,
    /// What changed since last listed.
    changed: Changed,
    sender: Sender,
}

impl Entry {
    /// How many it is to list: as many as any holder wants.
    fn limit(&self) -> u32 {
        *self.holders.values().max().expect("held, an entry is")
    }

    /// Whether it is to be listed before any due: as it lists fewer than
    /// it is to, as when it was never listed.
    fn wanted(&self) -> bool {
        fn short<T: Listable>(sender: &watch::Sender<State<T>>, limit: u32) -> bool {
            match &*sender.borrow() {
                State::Unlisted => true,
                State::Listed(snapshot) => snapshot.limit < limit,
                State::Failed(_) => false,
            }
        }
        match &self.sender {
            Sender::Items(sender) => short(sender, self.limit()),
            Sender::Unseen(sender) => short(sender, self.limit()),
        }
    }
}

impl Listings {
    /// A holder of `key`'s entry, made if there is none, wanting `limit`.
    fn hold(&mut self, key: Key, limit: u32) -> (u64, u64) {
        let entry = match self.by_key.get(&key) {
            Some(&entry) => entry,
            None => {
                let entry = self.number();
                let sender = match key.of {
                    Of::Items => Sender::Items(watch::channel(State::Unlisted).0),
                    Of::Unseen => Sender::Unseen(watch::channel(State::Unlisted).0),
                };
                self.by_key.insert(key.clone(), entry);
                self.entries.insert(
                    entry,
                    Entry {
                        key,
                        holders: HashMap::new(),
                        changed: Changed::default(),
                        sender,
                    },
                );
                entry
            }
        };
        let holder = self.number();
        self.entries
            .get_mut(&entry)
            .expect("found or made")
            .holders
            .insert(holder, limit);
        (entry, holder)
    }

    /// `holder` lets go of `entry`, which goes with its last.
    fn release(&mut self, entry: u64, holder: u64) {
        let held = self.entries.get_mut(&entry).expect("held, an entry is");
        held.holders.remove(&holder).expect("a holder holds");
        if held.holders.is_empty() {
            let gone = self.entries.remove(&entry).expect("found");
            if self.by_key.get(&gone.key) == Some(&entry) {
                self.by_key.remove(&gone.key);
            }
            self.due.retain(|&due| due != entry);
        }
    }

    fn number(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// `changed`, of each that lists that it may change: each is due,
    /// after those due already.
    fn came(&mut self, changed: &Changed) {
        for (key, &entry) in &self.by_key {
            let moved = match key.of {
                Of::Items => Item::moved_by(changed),
                Of::Unseen => Unseen::moved_by(changed),
            };
            if !moved {
                continue;
            }
            let held = self.entries.get_mut(&entry).expect("listed, an entry is");
            held.changed.merge(changed.clone());
            if !self.due.contains(&entry) {
                self.due.push_back(entry);
            }
        }
    }

    /// The listing to list next: one wanted (the oldest), else the one due
    /// the longest, no longer due, with what changed since it was listed.
    fn next(&mut self) -> Option<Job> {
        let wanted = self
            .by_key
            .values()
            .copied()
            .filter(|entry| self.entries[entry].wanted())
            .min();
        let entry = match wanted {
            Some(entry) => {
                self.due.retain(|&due| due != entry);
                entry
            }
            None => self.due.pop_front()?,
        };
        let held = self.entries.get_mut(&entry).expect("listed, an entry is");
        Some(Job {
            entry,
            key: held.key.clone(),
            limit: held.limit(),
            changed: std::mem::take(&mut held.changed),
        })
    }
}

/// A listing to list, as of what changed since it was listed.
struct Job {
    entry: u64,
    key: Key,
    limit: u32,
    changed: Changed,
}

impl Shared {
    /// A subscription to the listing of `query`, of `limit` items: the
    /// listing is shared with others of it, and listed if it is not.
    pub fn subscribe<T: Listable>(
        self: &Arc<Self>,
        query: String,
        unknown: UnknownNames,
        limit: u32,
    ) -> Subscription<T> {
        let key = Key {
            of: T::OF,
            query,
            unknown,
        };
        let mut listings = self.lock();
        let (entry, holder) = listings.hold(key, limit);
        let state = T::sender(&listings.entries[&entry].sender).subscribe();
        drop(listings);
        self.wanted.notify_one();
        Subscription {
            shared: self.clone(),
            entry,
            holder,
            limit,
            state,
        }
    }

    /// These are listed, of as many each, until pinned otherwise: the
    /// listings of those pinned before and not now go, unless subscribed
    /// to.
    pub fn pin(&self, pins: Vec<(Key, u32)>) {
        let mut listings = self.lock();
        let held: Vec<(u64, u64)> = pins
            .into_iter()
            .map(|(key, limit)| listings.hold(key, limit))
            .collect();
        for (entry, holder) in std::mem::replace(&mut listings.pins, held) {
            listings.release(entry, holder);
        }
        drop(listings);
        self.wanted.notify_one();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Listings> {
        self.listings.lock().expect("no holder panics")
    }

    /// Lists `job`'s listing again, and tells its subscriptions, if it
    /// changed.
    async fn list<T: Listable>(&self, backend: &impl Backend, job: Job) {
        let held = {
            let listings = self.lock();
            let Some(entry) = listings.entries.get(&job.entry) else {
                // Let go of meanwhile.
                return;
            };
            let held = match &*T::sender(&entry.sender).borrow() {
                State::Listed(snapshot) => Some(snapshot.clone()),
                State::Unlisted => None,
                State::Failed(_) => unreachable!("a listing that failed is not listed"),
            };
            held
        };
        let changed = match held {
            Some(_) => job.changed,
            None => Changed::everything(),
        };
        let started = Instant::now();
        let listed = T::list(
            backend,
            &job.key.query,
            job.key.unknown,
            job.limit,
            held.as_deref(),
            changed,
        )
        .await;
        tracing::debug!(
            "windows: {:?} of {} listed in {:?}",
            job.key.query,
            job.limit,
            started.elapsed()
        );
        let mut listings = self.lock();
        let Some(entry) = listings.entries.get(&job.entry) else {
            return;
        };
        let sender = T::sender(&entry.sender);
        match listed {
            Ok(listing) => {
                let snapshot = Snapshot {
                    limit: job.limit,
                    as_of: listing.as_of,
                    listed: listing.listed,
                    more: listing.more,
                    scope: listing.scope,
                    partial_rooms: listing.partial_rooms,
                };
                sender.send_if_modified(|state| {
                    // The same as of a later position is no news.
                    let same = match state {
                        State::Listed(was) => {
                            was.limit == snapshot.limit
                                && was.listed == snapshot.listed
                                && was.more == snapshot.more
                                && was.scope == snapshot.scope
                                && was.partial_rooms == snapshot.partial_rooms
                        }
                        State::Unlisted | State::Failed(_) => false,
                    };
                    if !same {
                        *state = State::Listed(Arc::new(snapshot));
                    }
                    !same
                });
            }
            Err(error) => {
                tracing::error!(
                    "windows: listing {:?} failed: {}",
                    job.key.query,
                    error.message
                );
                sender.send_replace(State::Failed(error));
                listings.by_key.remove(&job.key);
                listings.due.retain(|&due| due != job.entry);
            }
        }
    }
}

/// A session's hold of a listing, of its first `limit`: what it lists
/// as it changes.
pub struct Subscription<T: Listable> {
    shared: Arc<Shared>,
    entry: u64,
    holder: u64,
    limit: u32,
    state: watch::Receiver<State<T>>,
}

impl<T: Listable> Subscription<T> {
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Of `limit` items now, which it is listed of.
    pub fn widen(&mut self, limit: u32) {
        let mut listings = self.shared.lock();
        let entry = listings.entries.get_mut(&self.entry).expect("held");
        entry.holders.insert(self.holder, limit);
        drop(listings);
        self.limit = limit;
        self.shared.wanted.notify_one();
    }

    /// The listing, once it lists as many as subscribed to; or why it
    /// failed.
    pub async fn listed(&mut self) -> Result<Arc<Snapshot<T>>, ServerError> {
        loop {
            match self.current() {
                State::Listed(snapshot) if snapshot.limit >= self.limit => return Ok(snapshot),
                State::Failed(error) => return Err(error),
                State::Listed(_) | State::Unlisted => self.changed().await,
            }
        }
    }

    /// Once it was listed anew, since last looked at.
    pub async fn changed(&mut self) {
        self.state.changed().await.expect("held, a listing is kept");
    }

    /// As last listed, looked at.
    pub fn current(&mut self) -> State<T> {
        self.state.borrow_and_update().clone()
    }
}

impl<T: Listable> Drop for Subscription<T> {
    fn drop(&mut self) {
        self.shared.lock().release(self.entry, self.holder);
    }
}

/// Keeps the hub's listings current: lists those wanted, at once; and
/// those that changed, one at a time, each due once however many entries
/// came (SPEC §5.7): after entries, what they name; each minute, what
/// time moves; as what is attended changes, the unseen whole; after
/// entries missed, everything.
pub async fn keep(hub: Arc<Hub>, backend: impl Backend) {
    let shared = hub.windows().clone();
    let mut entries = hub.subscribe();
    let mut attended = hub.attended();
    let mut minute = interval_at(Instant::now() + EVERY, EVERY);
    minute.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // What came is listed with what is listed next.
        let mut changed = Changed::default();
        loop {
            match entries.try_recv() {
                Ok(entry) => changed.came(&entry),
                Err(TryRecvError::Lagged(_)) => changed.merge(Changed::everything()),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => unreachable!("the hub outlives its keeper"),
            }
        }
        let job = {
            let mut listings = shared.lock();
            if attended.has_changed().expect("the hub outlives its keeper") {
                attended.borrow_and_update();
                // What is unseen.
                changed.merge(Changed::seen());
            }
            listings.came(&changed);
            listings.next()
        };
        if let Some(job) = job {
            match job.key.of {
                Of::Items => shared.list::<Item>(&backend, job).await,
                Of::Unseen => shared.list::<Unseen>(&backend, job).await,
            }
            continue;
        }
        tokio::select! {
            entry = entries.recv() => {
                let changed = match entry {
                    Ok(entry) => {
                        let mut changed = Changed::default();
                        changed.came(&entry);
                        changed
                    }
                    Err(RecvError::Lagged(_)) => Changed::everything(),
                    Err(RecvError::Closed) => unreachable!("the hub outlives its keeper"),
                };
                shared.lock().came(&changed);
            }
            attention = attended.changed() => {
                attention.expect("the hub outlives its keeper");
                attended.borrow_and_update();
                shared.lock().came(&Changed::seen());
            }
            _ = minute.tick() => shared.lock().came(&Changed::time()),
            () = shared.wanted.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uplink_protocol::{mail::ListScope, todos::TodoId};

    fn key(query: &str) -> Key {
        Key {
            of: Of::Items,
            query: query.into(),
            unknown: UnknownNames::False,
        }
    }

    fn todos(ids: &[i64]) -> Changed {
        Changed {
            todos: ids.iter().map(|&id| TodoId(id)).collect(),
            ..Changed::default()
        }
    }

    /// `entry` listed, of `limit`.
    fn listed(listings: &Listings, entry: u64, limit: u32) {
        Item::sender(&listings.entries[&entry].sender).send_replace(State::Listed(Arc::new(
            Snapshot {
                limit,
                as_of: TxLogId(0),
                listed: vec![],
                more: false,
                scope: ListScope::Except { labels: vec![] },
                partial_rooms: vec![],
            },
        )));
    }

    fn next(listings: &mut Listings) -> Option<(String, u32, Changed)> {
        listings
            .next()
            .map(|job| (job.key.query, job.limit, job.changed))
    }

    #[test]
    fn listings_are_listed_wanted_first_then_in_turn_of_what_changed_since() {
        let mut listings = Listings::default();
        let (a, held) = listings.hold(key("a"), 2);
        let (b, _) = listings.hold(key("b"), 1);
        // Of the same query, the same, of as many as any holder wants.
        let (same, _) = listings.hold(key("a"), 5);
        assert_eq!(same, a);

        // Never listed, they are wanted, the oldest first.
        assert_eq!(
            next(&mut listings).map(|n| (n.0, n.1)),
            Some(("a".into(), 5))
        );
        listed(&listings, a, 5);
        assert_eq!(next(&mut listings).map(|n| n.0), Some("b".into()));
        listed(&listings, b, 1);
        assert_eq!(next(&mut listings), None);

        // Changed, each is due once, with all that changed since it was
        // listed; due again before the other is listed, it is listed after.
        listings.came(&todos(&[1]));
        let (first, _, changed) = next(&mut listings).unwrap();
        assert_eq!(changed, todos(&[1]));
        listings.came(&todos(&[2]));
        let (second, _, changed) = next(&mut listings).unwrap();
        assert_ne!(first, second);
        assert_eq!(changed, todos(&[1, 2]));
        assert_eq!(
            next(&mut listings).map(|n| (n.0, n.2)),
            Some((first, todos(&[2])))
        );
        assert_eq!(next(&mut listings), None);

        // Wanted wider, it is listed before those due, and is due no more.
        listings.came(&todos(&[3]));
        listings
            .entries
            .get_mut(&b)
            .unwrap()
            .holders
            .values_mut()
            .for_each(|l| *l = 3);
        assert_eq!(
            next(&mut listings).map(|n| (n.0, n.1, n.2)),
            Some(("b".into(), 3, todos(&[3])))
        );
        listed(&listings, b, 3);
        assert_eq!(next(&mut listings).map(|n| n.0), Some("a".into()));
        assert_eq!(next(&mut listings), None);

        // Let go of by its last holder, it is gone, and due no more.
        listings.came(&todos(&[4]));
        listings.release(a, held);
        assert_eq!(listings.entries[&a].limit(), 5);
        let other = *listings.entries[&a].holders.keys().next().unwrap();
        listings.release(a, other);
        assert!(!listings.by_key.contains_key(&key("a")));
        assert_eq!(next(&mut listings).map(|n| n.0), Some("b".into()));
        assert_eq!(next(&mut listings), None);
    }

    /// What is seen changes what is unseen, not what items are: listings
    /// of the unseen are due of it, of items not (SPEC §12.15).
    #[test]
    fn what_is_seen_is_of_listings_of_the_unseen() {
        let mut listings = Listings::default();
        let (items, _) = listings.hold(key("q"), 1);
        let unseen = Key {
            of: Of::Unseen,
            ..key("q")
        };
        let (of_unseen, _) = listings.hold(unseen, 1);
        listed(&listings, items, 1);
        Unseen::sender(&listings.entries[&of_unseen].sender).send_replace(State::Listed(Arc::new(
            Snapshot {
                limit: 1,
                as_of: TxLogId(0),
                listed: vec![],
                more: false,
                scope: (),
                partial_rooms: vec![],
            },
        )));
        listings.came(&Changed::seen());
        let job = listings.next().unwrap();
        assert_eq!((job.entry, job.changed), (of_unseen, Changed::seen()));
        assert!(listings.next().is_none());
        // Of what items are too, both are due, of all of it.
        let mut both = Changed::time();
        both.merge(Changed::seen());
        listings.came(&both);
        let mut due: Vec<_> = std::iter::from_fn(|| listings.next())
            .map(|job| (job.entry, job.changed))
            .collect();
        due.sort_by_key(|(entry, _)| *entry);
        assert_eq!(due, [(items, both.clone()), (of_unseen, both)]);
    }
}
