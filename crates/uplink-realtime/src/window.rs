//! Subscriptions' windows (SPEC §5.7): what the client has of each, so
//! that only what changed of it is sent.

use crate::session::Backend;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
    future::Future,
};
use uplink_protocol::{
    mail::ListScope, matrix::RoomId, Changed, Item, ItemKey, ItemPosition, Listed, ServerError,
    ServerResponse, SubscriptionId, TxLogId, UnknownNames, Unseen, WindowChange, WsServerMsg,
};

/// What a window lists, as of a log position.
pub struct Listing<T: Listable> {
    pub as_of: TxLogId,
    /// The first of what the query matches, in their order.
    pub listed: Vec<T>,
    /// Whether it matches more.
    pub more: bool,
    pub scope: T::Scope,
    /// The rooms it looked through not whole ([`WindowChange`]'s).
    pub partial_rooms: Vec<RoomId>,
}

/// What a window of items holds, as the client was last sent it: to be
/// listed again from, checking again only what changed.
pub struct Held {
    /// The most recently active first.
    pub items: Vec<Item>,
    /// Whether its query matches more.
    pub more: bool,
}

/// What a window lists (`Of`): items, or the unseen. Of each the same,
/// but for how it is listed, and what the client is sent of it.
pub trait Listable: Clone + PartialEq + Send + Sync + Sized + 'static {
    /// What its listing says besides: of items, their conversations'
    /// scope.
    type Scope: Send;

    fn key(&self) -> ItemKey;

    fn position(&self) -> ItemPosition;

    /// Whether `changed` may change what a window of it lists.
    fn moved_by(changed: &Changed) -> bool;

    /// The first `limit` of what `window`'s query matches, now that
    /// `changed` since it was last listed.
    fn list(
        backend: &impl Backend,
        window: &Window<Self>,
        limit: u32,
        changed: Changed,
    ) -> impl Future<Output = Result<Listing<Self>, ServerError>> + Send;

    /// Of `Subscribe` and `Extend`: what changed of the window.
    fn response(as_of: TxLogId, change: WindowChange<Self>, scope: Self::Scope) -> ServerResponse;

    /// What changed of the window, as it changes.
    fn changed(subscription: SubscriptionId, change: WindowChange<Self>) -> WsServerMsg;
}

impl Listable for Item {
    type Scope = ListScope;

    fn key(&self) -> ItemKey {
        Item::key(self)
    }

    fn position(&self) -> ItemPosition {
        Item::position(self)
    }

    /// Not what is seen: what items are is not of it.
    fn moved_by(changed: &Changed) -> bool {
        changed.of_items()
    }

    /// Of what it holds, but for what changed.
    fn list(
        backend: &impl Backend,
        window: &Window<Self>,
        limit: u32,
        changed: Changed,
    ) -> impl Future<Output = Result<Listing<Self>, ServerError>> + Send {
        backend.window(
            window.query.clone(),
            window.unknown,
            limit,
            window.held(),
            changed,
        )
    }

    fn response(as_of: TxLogId, change: WindowChange<Self>, scope: ListScope) -> ServerResponse {
        ServerResponse::Window {
            as_of,
            change,
            scope,
        }
    }

    fn changed(subscription: SubscriptionId, change: WindowChange<Self>) -> WsServerMsg {
        WsServerMsg::WindowChanged {
            subscription,
            change,
        }
    }
}

impl Listable for Unseen {
    type Scope = ();

    fn key(&self) -> ItemKey {
        Unseen::key(self)
    }

    fn position(&self) -> ItemPosition {
        Unseen::position(self)
    }

    fn moved_by(changed: &Changed) -> bool {
        !changed.is_empty()
    }

    /// Whole, whatever changed.
    fn list(
        backend: &impl Backend,
        window: &Window<Self>,
        limit: u32,
        _changed: Changed,
    ) -> impl Future<Output = Result<Listing<Self>, ServerError>> + Send {
        backend.unseen(window.query.clone(), window.unknown, limit)
    }

    fn response(as_of: TxLogId, change: WindowChange<Self>, (): ()) -> ServerResponse {
        ServerResponse::UnseenWindow { as_of, change }
    }

    fn changed(subscription: SubscriptionId, change: WindowChange<Self>) -> WsServerMsg {
        WsServerMsg::UnseenChanged {
            subscription,
            change,
        }
    }
}

/// A subscription's window, of what it lists.
pub enum Subscribed {
    Items(Window<Item>),
    Unseen(Window<Unseen>),
}

/// `$body`, of the window `$subscribed` is, named `$window`, whichever
/// it lists.
macro_rules! of_window {
    ($subscribed:expr, $window:ident => $body:expr) => {
        match $subscribed {
            $crate::window::Subscribed::Items($window) => $body,
            $crate::window::Subscribed::Unseen($window) => $body,
        }
    };
}
pub(crate) use of_window;

/// Whether `changed` may change what `window` lists.
fn moved_by<L: Listable>(_window: &Window<L>, changed: &Changed) -> bool {
    L::moved_by(changed)
}

/// A session's windows, by subscription, and those due to be listed
/// again, in turn.
#[derive(Default)]
pub struct Windows {
    windows: HashMap<SubscriptionId, Subscribed>,
    /// Of `windows`, those that may have changed since last listed, the
    /// longest due first: each once.
    due: VecDeque<SubscriptionId>,
}

impl Windows {
    /// Adds a window under a name new to it.
    pub fn insert(&mut self, subscription: SubscriptionId, window: Subscribed) {
        match self.windows.entry(subscription) {
            Entry::Vacant(vacant) => vacant.insert(window),
            Entry::Occupied(_) => panic!("subscription {} exists", subscription.0),
        };
    }

    pub fn contains(&self, subscription: SubscriptionId) -> bool {
        self.windows.contains_key(&subscription)
    }

    pub fn remove(&mut self, subscription: SubscriptionId) -> Option<Subscribed> {
        self.due.retain(|&due| due != subscription);
        self.windows.remove(&subscription)
    }

    /// What changed may change what windows list: each it may change is
    /// due, after those due already, to check again what changed since it
    /// was last listed.
    pub fn came(&mut self, changed: &Changed) {
        for (&subscription, window) in &mut self.windows {
            let moved = of_window!(window, window => {
                let moved = moved_by(window, changed);
                if moved {
                    window.changed.merge(changed.clone());
                }
                moved
            });
            if moved && !self.due.contains(&subscription) {
                self.due.push_back(subscription);
            }
        }
    }

    /// What changed since the window was last listed, as it is listed
    /// now, outside of its turn.
    pub fn take_changed(
        &mut self,
        subscription: SubscriptionId,
    ) -> Option<(&mut Subscribed, Changed)> {
        let window = self.windows.get_mut(&subscription)?;
        self.due.retain(|&due| due != subscription);
        let changed = of_window!(&mut *window, window => std::mem::take(&mut window.changed));
        Some((window, changed))
    }

    pub fn any_due(&self) -> bool {
        !self.due.is_empty()
    }

    /// The window due the longest, no longer due, and what changed since
    /// it was last listed.
    pub fn next_due(&mut self) -> Option<(SubscriptionId, &mut Subscribed, Changed)> {
        let subscription = self.due.pop_front()?;
        let window = self
            .windows
            .get_mut(&subscription)
            .expect("due windows are");
        let changed = of_window!(&mut *window, window => std::mem::take(&mut window.changed));
        Some((subscription, window, changed))
    }
}

/// A subscription's window: its query's first `limit` of what it lists,
/// as the client was last sent them.
pub struct Window<T: Listable> {
    pub query: String,
    pub unknown: UnknownNames,
    pub limit: u32,
    listed: HashMap<ItemKey, T>,
    more: bool,
    partial_rooms: Vec<RoomId>,
    /// What changed since it was last listed.
    changed: Changed,
}

impl<T: Listable> Window<T> {
    /// Of nothing, as the client has it before its first change.
    pub fn new(query: String, unknown: UnknownNames, limit: u32) -> Self {
        Self {
            query,
            unknown,
            limit,
            listed: HashMap::new(),
            more: false,
            partial_rooms: vec![],
            changed: Changed::default(),
        }
    }

    /// Takes what it lists now: what changed of it for the client.
    pub fn update(
        &mut self,
        listed: Vec<T>,
        more: bool,
        partial_rooms: Vec<RoomId>,
    ) -> WindowChange<T> {
        self.difference(listed, more, partial_rooms).1
    }

    /// As `update`, but nothing if nothing changed.
    pub fn changed(
        &mut self,
        listed: Vec<T>,
        more: bool,
        partial_rooms: Vec<RoomId>,
    ) -> Option<WindowChange<T>> {
        let (more_was, partial_was) = (self.more, self.partial_rooms.clone());
        let (same, change) = self.difference(listed, more, partial_rooms);
        (!(same && self.more == more_was && self.partial_rooms == partial_was)).then_some(change)
    }

    /// Takes what it lists now: whether no item was upserted or
    /// removed, and what changed of it for the client.
    fn difference(
        &mut self,
        listed: Vec<T>,
        more: bool,
        partial_rooms: Vec<RoomId>,
    ) -> (bool, WindowChange<T>) {
        let keys: HashSet<ItemKey> = listed.iter().map(T::key).collect();
        let removed: Vec<ItemKey> = self
            .listed
            .keys()
            .filter(|key| !keys.contains(key))
            .cloned()
            .collect();
        for key in &removed {
            self.listed.remove(key);
        }
        let mut upserted = vec![];
        for one in listed {
            let key = one.key();
            if self.listed.get(&key) != Some(&one) {
                upserted.push(one.clone());
                self.listed.insert(key, one);
            }
        }
        self.more = more;
        self.partial_rooms.clone_from(&partial_rooms);
        let same = upserted.is_empty() && removed.is_empty();
        let upserted = upserted
            .into_iter()
            .map(|item| Listed {
                key: item.key(),
                position: item.position(),
                item,
            })
            .collect();
        let change = WindowChange {
            upserted,
            removed,
            more,
            partial_rooms,
        };
        (same, change)
    }
}

impl Window<Item> {
    /// What it holds.
    pub fn held(&self) -> Held {
        let mut items: Vec<Item> = self.listed.values().cloned().collect();
        items.sort_by_key(Item::position);
        Held {
            items,
            more: self.more,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uplink_protocol::todos::{Todo, TodoId};

    fn todo(id: i64, updated_at: i64, title: &str) -> Item {
        Item::Todo {
            todo: Todo {
                id: TodoId(id),
                title: title.into(),
                notes: String::new(),
                due: None,
                message: None,
                created_at: 0,
                updated_at,
                done_at: None,
            },
        }
    }

    fn keys(listed: &[Listed<Item>]) -> Vec<&str> {
        listed.iter().map(|l| l.key.0.as_str()).collect()
    }

    #[test]
    fn only_what_changed_is_sent() {
        let mut window = Window::<Item>::new("q".into(), UnknownNames::Invalid, 2);
        let first = window.update(vec![todo(1, 20, "a"), todo(2, 10, "b")], true, vec![]);
        assert_eq!(keys(&first.upserted), ["todo:1", "todo:2"]);
        assert!(first.removed.is_empty() && first.more);

        assert_eq!(
            window.changed(vec![todo(1, 20, "a"), todo(2, 10, "b")], true, vec![]),
            None
        );

        // 2 changes, 1 leaves, 3 comes in; then there is no more.
        let change = window
            .changed(vec![todo(2, 30, "b!"), todo(3, 5, "c")], false, vec![])
            .unwrap();
        assert_eq!(keys(&change.upserted), ["todo:2", "todo:3"]);
        assert_eq!(change.upserted[0].position.at, 30);
        assert_eq!(change.removed, [ItemKey("todo:1".into())]);
        assert!(!change.more);

        // That there is more is news too.
        let change = window
            .changed(vec![todo(2, 30, "b!"), todo(3, 5, "c")], true, vec![])
            .unwrap();
        assert!(change.upserted.is_empty() && change.removed.is_empty() && change.more);
    }

    /// What is seen changes what is unseen, not what items are: windows
    /// of the unseen are due of it, of items not (SPEC §12.15).
    #[test]
    fn what_is_seen_is_of_windows_of_the_unseen() {
        let mut windows = Windows::default();
        windows.insert(
            SubscriptionId(1),
            Subscribed::Items(Window::new("q".into(), UnknownNames::Invalid, 1)),
        );
        windows.insert(
            SubscriptionId(2),
            Subscribed::Unseen(Window::new("q".into(), UnknownNames::Invalid, 1)),
        );
        windows.came(&Changed::seen());
        let (due, _, changed) = windows.next_due().unwrap();
        assert_eq!((due.0, changed), (2, Changed::seen()));
        assert!(windows.next_due().is_none());
        // Of what items are too, both are due, of all of it.
        let mut both = Changed::time();
        both.merge(Changed::seen());
        windows.came(&both);
        let mut due: Vec<_> =
            std::iter::from_fn(|| windows.next_due().map(|(id, _, changed)| (id.0, changed)))
                .collect();
        due.sort_by_key(|(id, _)| *id);
        assert_eq!(due, [(1, both.clone()), (2, both)]);
    }

    #[test]
    fn windows_are_listed_in_turn_of_what_changed_since() {
        let mut windows = Windows::default();
        for id in 1..=3 {
            windows.insert(
                SubscriptionId(id),
                Subscribed::Items(Window::new("q".into(), UnknownNames::Invalid, 1)),
            );
        }
        let next =
            |windows: &mut Windows| windows.next_due().map(|(id, _, changed)| (id.0, changed));
        let todos = |ids: &[i64]| Changed {
            todos: ids.iter().map(|&id| TodoId(id)).collect(),
            ..Changed::default()
        };
        assert_eq!(next(&mut windows), None);
        windows.came(&todos(&[1]));
        let (first, changed) = next(&mut windows).unwrap();
        assert_eq!(changed, todos(&[1]));
        // Due again before the others are listed, it is listed after them,
        // and they of all that changed since they were.
        windows.came(&todos(&[2]));
        let mut rest = vec![next(&mut windows).unwrap(), next(&mut windows).unwrap()];
        assert!(rest
            .iter()
            .all(|(id, changed)| *id != first && *changed == todos(&[1, 2])));
        assert_eq!(next(&mut windows), Some((first, todos(&[2]))));
        assert_eq!(next(&mut windows), None);
        // Listed out of turn, it is due no more.
        windows.came(&todos(&[3]));
        let (_, changed) = windows.take_changed(SubscriptionId(first)).unwrap();
        assert_eq!(changed, todos(&[3]));
        // Gone, it is due no more.
        windows.came(&todos(&[4]));
        windows.remove(SubscriptionId(first));
        rest.sort_by_key(|(id, _)| *id);
        let mut listed = vec![next(&mut windows).unwrap(), next(&mut windows).unwrap()];
        listed.sort_by_key(|(id, _)| *id);
        let ids = |l: &[(u64, Changed)]| l.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        assert_eq!(ids(&listed), ids(&rest));
        assert!(listed.iter().all(|(_, changed)| *changed == todos(&[3, 4])));
        assert!(!windows.any_due());
    }
}
