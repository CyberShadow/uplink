//! Subscriptions' windows (SPEC §5.7): of each, what the client has of
//! the listing it is of, so that only what changed of it is sent.

use crate::{
    session::Backend,
    shared::{Sender, Snapshot, State, Subscription},
};
use futures_util::future::{select_all, FutureExt};
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    future::Future,
};
use tokio::sync::watch;
use uplink_protocol::{
    mail::ListScope, matrix::RoomId, Changed, Item, ItemKey, ItemPosition, Listed, Of, ServerError,
    ServerResponse, SubscriptionId, TxLogId, UnknownNames, Unseen, WindowChange, WsServerMsg,
};

/// What a query lists, as of a log position.
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

/// What a listing of items held when last listed: to be listed again
/// from, checking again only what changed.
pub struct Held {
    /// The most recently active first.
    pub items: Vec<Item>,
    /// Whether its query matches more.
    pub more: bool,
}

/// What a window lists (`Of`): items, or the unseen. Of each the same,
/// but for how it is listed, and what the client is sent of it.
pub trait Listable: Clone + PartialEq + Send + Sync + Sized + 'static {
    const OF: Of;

    /// What its listing says besides: of items, their conversations'
    /// scope.
    type Scope: Clone + PartialEq + Send + Sync;

    fn key(&self) -> ItemKey;

    fn position(&self) -> ItemPosition;

    /// Whether `changed` may change what a listing of it lists.
    fn moved_by(changed: &Changed) -> bool;

    /// Of a listing's watch, that of its kind.
    fn sender(sender: &Sender) -> &watch::Sender<State<Self>>;

    /// The first `limit` of what `query` matches, now that `changed`
    /// since it `held` these (of everything, if it held none).
    fn list(
        backend: &impl Backend,
        query: &str,
        unknown: UnknownNames,
        limit: u32,
        held: Option<&Snapshot<Self>>,
        changed: Changed,
    ) -> impl Future<Output = Result<Listing<Self>, ServerError>> + Send;

    /// Of `Subscribe` and `Extend`: what changed of the window.
    fn response(as_of: TxLogId, change: WindowChange<Self>, scope: Self::Scope) -> ServerResponse;

    /// What changed of the window, as it changes.
    fn changed(subscription: SubscriptionId, change: WindowChange<Self>) -> WsServerMsg;
}

impl Listable for Item {
    const OF: Of = Of::Items;

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

    fn sender(sender: &Sender) -> &watch::Sender<State<Self>> {
        match sender {
            Sender::Items(sender) => sender,
            Sender::Unseen(_) => unreachable!("a listing of items is of items"),
        }
    }

    /// Of what it held, but for what changed.
    fn list(
        backend: &impl Backend,
        query: &str,
        unknown: UnknownNames,
        limit: u32,
        held: Option<&Snapshot<Self>>,
        changed: Changed,
    ) -> impl Future<Output = Result<Listing<Self>, ServerError>> + Send {
        let held = Held {
            items: held.map(|held| held.listed.clone()).unwrap_or_default(),
            more: held.is_some_and(|held| held.more),
        };
        backend.window(query.to_owned(), unknown, limit, held, changed)
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
    const OF: Of = Of::Unseen;

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

    fn sender(sender: &Sender) -> &watch::Sender<State<Self>> {
        match sender {
            Sender::Unseen(sender) => sender,
            Sender::Items(_) => unreachable!("a listing of the unseen is of the unseen"),
        }
    }

    /// Whole, whatever changed.
    fn list(
        backend: &impl Backend,
        query: &str,
        unknown: UnknownNames,
        limit: u32,
        _held: Option<&Snapshot<Self>>,
        _changed: Changed,
    ) -> impl Future<Output = Result<Listing<Self>, ServerError>> + Send {
        backend.unseen(query.to_owned(), unknown, limit)
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

/// A session's windows, by subscription.
#[derive(Default)]
pub struct Windows {
    windows: HashMap<SubscriptionId, Subscribed>,
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

    pub fn get_mut(&mut self, subscription: SubscriptionId) -> Option<&mut Subscribed> {
        self.windows.get_mut(&subscription)
    }

    pub fn remove(&mut self, subscription: SubscriptionId) -> Option<Subscribed> {
        self.windows.remove(&subscription)
    }

    /// Once the listing of a window was listed anew, since last looked
    /// at: which; never, of none.
    pub async fn changed(&mut self) -> SubscriptionId {
        if self.windows.is_empty() {
            return std::future::pending().await;
        }
        let changes = self.windows.iter_mut().map(|(&subscription, window)| {
            async move {
                of_window!(window, window => window.subscription.changed().await);
                subscription
            }
            .boxed()
        });
        select_all(changes).await.0
    }
}

/// A subscription's window: its listing's first `limit`, as the client
/// was last sent them.
pub struct Window<T: Listable> {
    pub subscription: Subscription<T>,
    listed: HashMap<ItemKey, T>,
    more: bool,
    partial_rooms: Vec<RoomId>,
}

impl<T: Listable> Window<T> {
    /// Of nothing, as the client has it before its first change.
    pub fn new(subscription: Subscription<T>) -> Self {
        Self {
            subscription,
            listed: HashMap::new(),
            more: false,
            partial_rooms: vec![],
        }
    }

    /// Takes what `snapshot` lists of it: what changed of it for the
    /// client.
    pub fn update(&mut self, snapshot: &Snapshot<T>) -> WindowChange<T> {
        let (listed, more) = snapshot.first(self.subscription.limit());
        self.difference(listed, more, &snapshot.partial_rooms).1
    }

    /// As `update`, but nothing if nothing changed.
    pub fn changed(&mut self, snapshot: &Snapshot<T>) -> Option<WindowChange<T>> {
        let (listed, more) = snapshot.first(self.subscription.limit());
        let (more_was, partial_was) = (self.more, self.partial_rooms.clone());
        let (same, change) = self.difference(listed, more, &snapshot.partial_rooms);
        (!(same && self.more == more_was && self.partial_rooms == partial_was)).then_some(change)
    }

    /// Takes what it lists now: whether no item was upserted or
    /// removed, and what changed of it for the client.
    fn difference(
        &mut self,
        listed: &[T],
        more: bool,
        partial_rooms: &[RoomId],
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
            if self.listed.get(&key) != Some(one) {
                upserted.push(one.clone());
                self.listed.insert(key, one.clone());
            }
        }
        self.more = more;
        self.partial_rooms = partial_rooms.to_vec();
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
            partial_rooms: partial_rooms.to_vec(),
        };
        (same, change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::Shared;
    use std::sync::Arc;
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

    fn snapshot(limit: u32, listed: Vec<Item>, more: bool) -> Snapshot<Item> {
        Snapshot {
            limit,
            as_of: TxLogId(0),
            listed,
            more,
            scope: ListScope::Except { labels: vec![] },
            partial_rooms: vec![],
        }
    }

    #[test]
    fn only_what_changed_is_sent() {
        let shared = Arc::new(Shared::default());
        let mut window =
            Window::<Item>::new(shared.subscribe("q".into(), UnknownNames::Invalid, 2));
        let first = window.update(&snapshot(2, vec![todo(1, 20, "a"), todo(2, 10, "b")], true));
        assert_eq!(keys(&first.upserted), ["todo:1", "todo:2"]);
        assert!(first.removed.is_empty() && first.more);

        assert_eq!(
            window.changed(&snapshot(2, vec![todo(1, 20, "a"), todo(2, 10, "b")], true)),
            None
        );

        // 2 changes, 1 leaves, 3 comes in; then there is no more.
        let change = window
            .changed(&snapshot(
                2,
                vec![todo(2, 30, "b!"), todo(3, 5, "c")],
                false,
            ))
            .unwrap();
        assert_eq!(keys(&change.upserted), ["todo:2", "todo:3"]);
        assert_eq!(change.upserted[0].position.at, 30);
        assert_eq!(change.removed, [ItemKey("todo:1".into())]);
        assert!(!change.more);

        // That there is more is news too.
        let change = window
            .changed(&snapshot(2, vec![todo(2, 30, "b!"), todo(3, 5, "c")], true))
            .unwrap();
        assert!(change.upserted.is_empty() && change.removed.is_empty() && change.more);

        // Of a listing of more than it shows, its first: what is past them
        // is that there is more.
        let change = window
            .changed(&snapshot(
                3,
                vec![todo(2, 30, "b!"), todo(3, 5, "c"), todo(4, 1, "d")],
                false,
            ))
            .map(|change| change.more);
        assert_eq!(change, None);
    }
}
