//! Subscriptions' windows (SPEC §5.7): what the client has of each, so
//! that only what changed of it is sent.

use std::collections::{HashMap, HashSet};
use uplink_protocol::{
    mail::ListScope, matrix::RoomId, Item, ItemKey, Listed, TxLogId, WindowChange,
};

/// What a window lists, as of a log position.
pub struct Listing {
    pub as_of: TxLogId,
    /// The first items of what the query matches, the most recently
    /// active first.
    pub items: Vec<Item>,
    /// Whether it matches more.
    pub more: bool,
    pub scope: ListScope,
    /// The rooms it looked through not whole ([`WindowChange`]'s).
    pub partial_rooms: Vec<RoomId>,
}

/// A subscription's window: its query's first `limit` items, as the
/// client was last sent them.
pub struct Window {
    pub query: String,
    pub limit: u32,
    items: HashMap<ItemKey, Item>,
    more: bool,
    partial_rooms: Vec<RoomId>,
}

impl Window {
    /// Of nothing, as the client has it before its first change.
    pub fn new(query: String, limit: u32) -> Self {
        Self {
            query,
            limit,
            items: HashMap::new(),
            more: false,
            partial_rooms: vec![],
        }
    }

    /// Takes what it lists now: what changed of it for the client.
    pub fn update(
        &mut self,
        items: Vec<Item>,
        more: bool,
        partial_rooms: Vec<RoomId>,
    ) -> WindowChange {
        let keys: HashSet<ItemKey> = items.iter().map(Item::key).collect();
        let removed: Vec<ItemKey> = self
            .items
            .keys()
            .filter(|key| !keys.contains(key))
            .cloned()
            .collect();
        for key in &removed {
            self.items.remove(key);
        }
        let mut upserted = vec![];
        for item in items {
            let key = item.key();
            if self.items.get(&key) != Some(&item) {
                upserted.push(Listed {
                    key: key.clone(),
                    position: item.position(),
                    item: item.clone(),
                });
                self.items.insert(key, item);
            }
        }
        self.more = more;
        self.partial_rooms.clone_from(&partial_rooms);
        WindowChange {
            upserted,
            removed,
            more,
            partial_rooms,
        }
    }

    /// As `update`, but nothing if nothing changed.
    pub fn changed(
        &mut self,
        items: Vec<Item>,
        more: bool,
        partial_rooms: Vec<RoomId>,
    ) -> Option<WindowChange> {
        let (more_was, partial_was) = (self.more, self.partial_rooms.clone());
        let change = self.update(items, more, partial_rooms);
        let same = change.upserted.is_empty()
            && change.removed.is_empty()
            && change.more == more_was
            && change.partial_rooms == partial_was;
        (!same).then_some(change)
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

    fn keys(listed: &[Listed]) -> Vec<&str> {
        listed.iter().map(|l| l.key.0.as_str()).collect()
    }

    #[test]
    fn only_what_changed_is_sent() {
        let mut window = Window::new("q".into(), 2);
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
}
