//! What a query matches of every kind (SPEC §8.7): mail's conversations,
//! rooms, to-dos and events, in one read, so as of one log position, and
//! in one list, the most recently active first. Each domain matches its
//! own; a term a kind lacks is false of it. The home view (SPEC §12.13) is
//! the matches of its buckets' queries.

use arc_swap::ArcSwap;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uplink_config::HomeBucket;
use uplink_email::query::{self as mail, ListError, Unknown};
use uplink_matrix::query::{Group, Rooms};
use uplink_protocol::{
    calendar::CalendarEvent,
    mail::{EntryId, ListPosition, ListScope},
    todos::Todo,
    Activity, Badge, BadgedBucket, Badges, ErrorCode, Item, ItemKind, ItemPosition, Mark, Marks,
    Place, ServerError, ServerResponse, BADGE_LIMIT,
};
use uplink_query::{Query, Term};
use uplink_storage::rusqlite::{Connection, Transaction};
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The most items a page may ask for, as a list of mail.
const MAX_LIMIT: u32 = uplink_email::service::MAX_LIMIT;

#[derive(Clone)]
pub struct Items {
    reader: Reader,
    mail_accounts: Vec<mail::ConfiguredAccount>,
    matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount>,
    /// As configured, top to bottom; configured anew on reload.
    home: Arc<ArcSwap<Vec<HomeBucket>>>,
}

/// A bucket of the home view, its query less those before it.
struct Bucket {
    name: String,
    badge: Option<Badge>,
    text: String,
    query: Query,
}

/// The marks of the places, as they are added up bucket by bucket.
#[derive(Default)]
struct Places {
    home: Mark,
    labels: BTreeMap<uplink_protocol::mail::LabelId, Mark>,
    accounts: BTreeMap<String, Mark>,
    rooms: BTreeMap<uplink_protocol::matrix::RoomId, Mark>,
    todos: Mark,
    calendar: Mark,
}

impl Places {
    /// Adds a bucket's: a room by its messages that notify, or 1.
    fn add(&mut self, badge: Badge, mail: &mail::Places, others: &Others) {
        let count = |n: usize| n as u32;
        for (label, n) in &mail.labels {
            self.labels.entry(*label).or_default().add(badge, *n as u32);
        }
        for (account, n) in &mail.accounts {
            self.accounts
                .entry(account.clone())
                .or_default()
                .add(badge, *n as u32);
            self.home.add(badge, *n as u32);
        }
        for group in &others.rooms {
            let n = group.room.notifying.max(1);
            self.rooms.entry(group.room.id).or_default().add(badge, n);
        }
        self.home.add(badge, count(others.rooms.len()));
        self.todos.add(badge, count(others.todos.len()));
        self.home.add(badge, count(others.todos.len()));
        self.calendar.add(badge, count(others.events.len()));
        self.home.add(badge, count(others.events.len()));
    }
}

impl From<Places> for Marks {
    fn from(places: Places) -> Self {
        fn marked<K>(marks: BTreeMap<K, Mark>) -> Vec<(K, Mark)> {
            marks
                .into_iter()
                .filter(|(_, mark)| !mark.is_none())
                .collect()
        }
        Marks {
            home: places.home,
            labels: marked(places.labels),
            accounts: marked(places.accounts),
            rooms: marked(places.rooms),
            todos: places.todos,
            calendar: places.calendar,
        }
    }
}

/// When the places were last seen, by their keys (SPEC §12.15).
struct Seen(std::collections::HashMap<String, crate::seen::Watermark>);

impl Seen {
    fn load(tx: &Transaction) -> anyhow::Result<Self> {
        Ok(Self(crate::seen::all(tx)?.into_iter().collect()))
    }

    /// The latest of `kind` seen when any of `places` was last seen.
    fn of(&self, places: &[Place], kind: fn(&crate::seen::Watermark) -> i64) -> Option<i64> {
        places
            .iter()
            .filter_map(|place| self.0.get(&crate::seen::key(place)))
            .map(kind)
            .max()
    }

    /// What of `others` came since its places were last seen: a room by
    /// its latest event, a to-do or an event by itself.
    fn unseen(&self, others: &Others) -> Others {
        let after = |seen: Option<i64>, id: i64| seen.is_none_or(|seen| id > seen);
        Others {
            rooms: others
                .rooms
                .iter()
                .filter(|group| {
                    let places = [
                        Place::Home,
                        Place::Room {
                            room: group.room.id,
                        },
                    ];
                    after(
                        self.of(&places, |w| w.room_event),
                        group.room.latest.map_or(0, |e| e.0),
                    )
                })
                .cloned()
                .collect(),
            todos: others
                .todos
                .iter()
                .filter(|todo| after(self.of(&[Place::Home, Place::Todos], |w| w.todo), todo.id.0))
                .cloned()
                .collect(),
            events: others
                .events
                .iter()
                .filter(|event| {
                    after(
                        self.of(&[Place::Home, Place::Calendar], |w| w.event),
                        event.id.0,
                    )
                })
                .cloned()
                .collect(),
        }
    }
}

/// What a query matches of kinds other than mail: few, so all of them.
struct Others {
    rooms: Vec<Group>,
    todos: Vec<Todo>,
    events: Vec<CalendarEvent>,
}

/// A page of a list of every kind.
struct Page {
    items: Vec<Item>,
    next: Option<ItemPosition>,
    scope: ListScope,
}

/// Where an item is listed.
fn position(item: &Item) -> ItemPosition {
    let (at, kind, id) = match item {
        Item::Mail { conversation } => (
            conversation.latest.received_at,
            ItemKind::Mail,
            conversation.latest.id.0,
        ),
        Item::Room { room, hits } => (
            // An invitation has no activity of its own: the oldest.
            hits.as_ref()
                .map(|hits| hits.latest.ts)
                .or(room.last_event_ts)
                .unwrap_or(0),
            ItemKind::Room,
            room.id.0,
        ),
        Item::Todo { todo } => (todo.updated_at, ItemKind::Todo, todo.id.0),
        Item::Event { event } => (event.modified, ItemKind::Event, event.id.0),
    };
    ItemPosition { at, kind, id }
}

/// Mail's position after which its conversations are listed after
/// `after`: conversations are first of what is as recently active.
fn mail_after(after: ItemPosition) -> ListPosition {
    ListPosition {
        received_at: after.at,
        entry: EntryId(match after.kind {
            ItemKind::Mail => after.id,
            // Before every entry as recent.
            ItemKind::Room | ItemKind::Todo | ItemKind::Event => i64::MIN,
        }),
    }
}

impl Items {
    /// `reader` is a read-only connection to the database.
    pub fn new(
        reader: Connection,
        mail_accounts: Vec<mail::ConfiguredAccount>,
        matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount>,
        home: Vec<HomeBucket>,
    ) -> Self {
        Self {
            reader: Reader::new(reader),
            mail_accounts,
            matrix_accounts,
            home: Arc::new(ArcSwap::from_pointee(home)),
        }
    }

    /// How many conversations `query` lists, as `list` lists them.
    pub async fn count(&self, query: String) -> Response {
        let query = uplink_query::parse(&query, now()).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                this.check_names(&tx, &query)?;
                let count = mail::count(&tx, &this.mail_accounts, &query, Unknown::False)
                    .map_err(list_error)?;
                Ok(ServerResponse::ConversationCount { count })
            })
            .await
    }

    /// The entries of every conversation `query` lists, as `list` lists
    /// them, each conversation's oldest first.
    pub async fn matching(&self, query: String) -> Result<Vec<Vec<EntryId>>, ServerError> {
        let query = uplink_query::parse(&query, now()).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                this.check_names(&tx, &query)?;
                mail::matching(&tx, &this.mail_accounts, &query, Unknown::False).map_err(list_error)
            })
            .await
    }

    pub fn configure_home(&self, home: Vec<HomeBucket>) {
        self.home.store(Arc::new(home));
    }

    pub async fn list(&self, query: String, after: Option<ItemPosition>, limit: u32) -> Response {
        let (today, now) = checked(limit)?;
        let query = uplink_query::parse(&query, now).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                this.check_names(&tx, &query)?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                let others = others(&tx, &rooms, &query, today, now)?;
                let page = this.page(&tx, &query, others, after, limit)?;
                Ok(ServerResponse::Items {
                    as_of,
                    items: page.items,
                    next: page.next,
                    scope: page.scope,
                })
            })
            .await
    }

    /// Each bucket lists what its query matches, less what those before it
    /// list. A name no kind has is false, not an error: the default
    /// buckets name Gmail's labels and Element's tags, which an account
    /// may lack.
    pub async fn home(&self, limit: u32) -> Response {
        let (today, now) = checked(limit)?;
        let buckets = self.buckets(now);
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                // Read once, for every bucket.
                let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                let buckets = buckets
                    .into_iter()
                    .map(|bucket| {
                        let others = others(&tx, &rooms, &bucket.query, today, now)?;
                        let page = this.page(&tx, &bucket.query, others, None, limit)?;
                        Ok(uplink_protocol::HomeBucket {
                            name: bucket.name,
                            query: bucket.text,
                            items: page.items,
                            more: page.next.is_some(),
                        })
                    })
                    .collect::<Result<_, ServerError>>()?;
                Ok(ServerResponse::Home { as_of, buckets })
            })
            .await
    }

    /// What the badged buckets hold (SPEC §12.14), as `home` lists them:
    /// up to `BADGE_LIMIT` items each; and the marks of the places their
    /// items are in, of them all.
    pub async fn badges(&self) -> Result<Badges, ServerError> {
        let now = now();
        let today = uplink_query::today(now);
        let badged: Vec<_> = self
            .buckets(now)
            .into_iter()
            .filter_map(|bucket| Some((bucket.badge?, bucket)))
            .collect();
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                let mut marks = Places::default();
                let seen = Seen::load(&tx).map_err(internal)?;
                let buckets = badged
                    .into_iter()
                    .map(|(badge, bucket)| {
                        let others = others(&tx, &rooms, &bucket.query, today, now)?;
                        // A dot is of what is unseen (SPEC §12.15).
                        let unseen = badge == Badge::Dot;
                        let places = mail::places(
                            &tx,
                            &this.mail_accounts,
                            &bucket.query,
                            Unknown::False,
                            unseen,
                        )
                        .map_err(list_error)?;
                        match unseen {
                            true => marks.add(badge, &places, &seen.unseen(&others)),
                            false => marks.add(badge, &places, &others),
                        }
                        let page =
                            this.page(&tx, &bucket.query, others, None, BADGE_LIMIT as u32)?;
                        let items = page.items.iter().map(|item| match item {
                            Item::Mail { conversation } => Activity::Mail {
                                entry: conversation.latest.id,
                            },
                            Item::Room { room, .. } => Activity::Room {
                                room: room.id,
                                event: room.latest,
                            },
                            Item::Todo { todo } => Activity::Todo { todo: todo.id },
                            Item::Event { event } => Activity::Event { event: event.id },
                        });
                        Ok(BadgedBucket {
                            name: bucket.name,
                            badge,
                            items: items.collect(),
                        })
                    })
                    .collect::<Result<_, ServerError>>()?;
                Ok(Badges {
                    buckets,
                    marks: marks.into(),
                })
            })
            .await
    }

    /// The buckets, each with its query less those before it: so an item
    /// is in the first that matches it.
    fn buckets(&self, now: i64) -> Vec<Bucket> {
        let mut before: Vec<String> = vec![];
        let mut buckets = vec![];
        for bucket in self.home.load_full().iter() {
            let text = std::iter::once(format!("({})", bucket.query))
                .chain(before.iter().map(|q| format!("-({q})")))
                .collect::<Vec<_>>()
                .join(" ");
            let query = uplink_query::parse(&text, now)
                .expect("buckets' queries are checked as configured, so parse in parentheses");
            before.push(bucket.query.clone());
            buckets.push(Bucket {
                name: bucket.name.clone(),
                badge: bucket.badge,
                text,
                query,
            });
        }
        buckets
    }

    /// The page of what `query` matches, mail and `others`, after `after`:
    /// up to `limit` items, the most recently active first.
    fn page(
        &self,
        tx: &Transaction,
        query: &Query,
        others: Others,
        after: Option<ItemPosition>,
        limit: u32,
    ) -> Result<Page, ServerError> {
        let mail = mail::conversations(
            tx,
            &self.mail_accounts,
            query,
            Unknown::False,
            after.map(mail_after),
            limit,
        )
        .map_err(list_error)?;
        let conversations = mail
            .conversations
            .into_iter()
            .map(|conversation| Item::Mail { conversation });
        let rooms = others
            .rooms
            .into_iter()
            .map(|Group { room, hits }| Item::Room {
                room: Box::new(room),
                hits,
            });
        let todos = others.todos.into_iter().map(|todo| Item::Todo { todo });
        let events = others.events.into_iter().map(|event| Item::Event { event });
        let mut items: Vec<Item> = conversations
            .chain(rooms)
            .chain(todos)
            .chain(events)
            .filter(|item| after.is_none_or(|after| position(item) > after))
            .collect();
        items.sort_by_key(position);
        // More mail is after the last of its page, so after the last here.
        let more = items.len() > limit as usize || mail.next.is_some();
        items.truncate(limit as usize);
        Ok(Page {
            next: more.then(|| position(items.last().expect("more is after some"))),
            items,
            scope: mail.scope,
        })
    }

    /// Fails on a label or account no kind has: a misspelt name lists
    /// nothing, as in a list of mail.
    fn check_names(&self, tx: &Transaction, query: &Query) -> Result<(), ServerError> {
        for term in uplink_query::terms(query) {
            match term {
                Term::Label(name) => {
                    let known = uplink_query::CATEGORIES
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(name))
                        || mail::label_known(tx, name).map_err(internal)?
                        || uplink_matrix::query::tag_known(tx, name).map_err(internal)?;
                    if !known {
                        return Err(invalid(format!("no label or tag {name}")));
                    }
                }
                Term::Account(account) => {
                    let known = self
                        .mail_accounts
                        .iter()
                        .any(|a| a.id == *account || a.address.eq_ignore_ascii_case(account))
                        || self
                            .matrix_accounts
                            .iter()
                            .any(|a| a.id == *account || a.mxid == *account);
                    if !known {
                        return Err(invalid(format!("no account {account}")));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn others(
    tx: &Transaction,
    rooms: &Rooms,
    query: &Query,
    today: i64,
    now: i64,
) -> Result<Others, ServerError> {
    Ok(Others {
        rooms: rooms.matching(tx, query).map_err(internal)?,
        todos: uplink_todos::matching(tx, query, today).map_err(internal)?,
        events: uplink_calendar::matching(tx, query, now).map_err(internal)?,
    })
}

fn invalid(why: String) -> ServerError {
    error(ErrorCode::Invalid, why)
}

fn list_error(e: ListError) -> ServerError {
    match e {
        ListError::Invalid(e) => invalid(e.0),
        ListError::Internal(e) => internal(e),
    }
}

/// Today, as days since 1970-01-01, and now, in ms, for a `limit` checked.
fn checked(limit: u32) -> Result<(i64, i64), ServerError> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(invalid(format!("limit must be between 1 and {MAX_LIMIT}")));
    }
    let now = now();
    Ok((uplink_query::today(now), now))
}

/// In ms since the epoch.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use uplink_matrix::rooms::{ObservedRoom, Synced};
    use uplink_protocol::matrix::Membership;
    use uplink_realtime::Hub;
    use uplink_tx_log::{Dispatcher, Registry};

    #[tokio::test]
    async fn queries_list_every_kind_and_names_must_be_of_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        uplink_matrix::register(&mut registry);
        uplink_todos::register(&mut registry);
        crate::seen::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
        );
        dispatcher
            .observe(
                "mx".into(),
                Synced {
                    next_batch: "next".into(),
                    rooms: vec![ObservedRoom {
                        room_id: "!r:x".into(),
                        name: "Rent talk".into(),
                        encrypted: false,
                        membership: Membership::Invited,
                        space: false,
                        direct: false,
                        avatar: None,
                        children: vec![],
                        space_order: None,
                        tags: vec!["u.home".into()],
                        topic: None,
                        alias: None,
                        pinned: vec![],
                        may_pin: false,
                        joined_members: 1,
                        version: "11".into(),
                        events: vec![],
                        limited: false,
                        own_receipts: vec![],
                        fully_read: None,
                    }],
                },
            )
            .await
            .unwrap();
        let todos = uplink_todos::service::Todos::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
        );
        todos
            .create(
                "Pay rent".into(),
                String::new(),
                Some("2026-11-10".into()),
                None,
            )
            .await
            .unwrap();
        let items = Items::new(
            uplink_storage::open_reader(dir.path()).unwrap(),
            vec![],
            vec![uplink_matrix::service::ConfiguredAccount {
                id: "mx".into(),
                mxid: "@me:x".into(),
            }],
            vec![],
        );
        let list = |query: &str| {
            let items = items.clone();
            let query = query.to_owned();
            async move { items.list(query, None, 10).await }
        };
        fn name(item: &Item) -> String {
            match item {
                Item::Room { room, .. } => room.name.clone(),
                Item::Todo { todo } => todo.title.clone(),
                other => panic!("unexpected {other:?}"),
            }
        }
        let names = |response: Response| match response.unwrap() {
            ServerResponse::Items { items, .. } => items.iter().map(name).collect::<Vec<_>>(),
            other => panic!("unexpected {other:?}"),
        };
        // The most recently active first: an invitation is not active.
        assert_eq!(names(list("rent").await), ["Pay rent", "Rent talk"]);
        assert_eq!(
            names(list("is:invite OR is:overdue").await),
            ["Pay rent", "Rent talk"]
        );
        assert_eq!(names(list("tag:home").await), ["Rent talk"]);
        assert_eq!(names(list("rent -account:mx").await), ["Pay rent"]);
        // Page by page, of every kind.
        let ServerResponse::Items {
            items: first,
            next: Some(next),
            ..
        } = items.list("rent".into(), None, 1).await.unwrap()
        else {
            panic!()
        };
        let ServerResponse::Items {
            items: second,
            next: None,
            ..
        } = items.list("rent".into(), Some(next), 1).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (name(&first[0]), name(&second[0])),
            ("Pay rent".into(), "Rent talk".into())
        );
        let refused = |response: Response| response.unwrap_err().message;
        assert_eq!(refused(list("tag:work").await), "no label or tag work");
        assert_eq!(refused(list("account:nobody").await), "no account nobody");
        // Gmail's categories, though no account has them, list nothing.
        assert!(names(list("category:promotions").await).is_empty());
        assert_eq!(
            refused(list("category:junk").await),
            "no label or tag CATEGORY_JUNK"
        );
        assert!(refused(items.list("x".into(), None, 0).await).starts_with("limit"));
        // Counted, and resolved to conversations, as listed: a room's tag
        // is no mail's, and a name no kind has is refused.
        assert!(matches!(
            items.count("tag:home".into()).await.unwrap(),
            ServerResponse::ConversationCount { count: 0 }
        ));
        assert_eq!(
            items.matching("tag:home".into()).await.unwrap(),
            Vec::<Vec<EntryId>>::new()
        );
        assert_eq!(
            refused(items.count("tag:work".into()).await),
            "no label or tag work"
        );
        assert_eq!(
            items.matching("tag:work".into()).await.unwrap_err().message,
            "no label or tag work"
        );

        // An item is in the first bucket that matches it; a name no kind
        // has is false.
        let bucket = |name: &str, query: &str, badge| HomeBucket {
            name: name.into(),
            query: query.into(),
            badge,
        };
        items.configure_home(vec![
            bucket("Now", "is:invite", Some(Badge::Count)),
            bucket("Rent", "rent", None),
            bucket("Else", "tag:nowhere OR is:todo", Some(Badge::Dot)),
        ]);
        let ServerResponse::Home { buckets, .. } = items.home(10).await.unwrap() else {
            panic!()
        };
        let listed: Vec<_> = buckets
            .iter()
            .map(|b| {
                (
                    b.name.as_str(),
                    b.items.iter().map(name).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("Now", vec!["Rent talk".to_owned()]),
                ("Rent", vec!["Pay rent".to_owned()]),
                ("Else", vec![]),
            ]
        );
        assert_eq!(
            buckets[2].query,
            "(tag:nowhere OR is:todo) -(is:invite) -(rent)"
        );
        assert!(!buckets[0].more);

        // The badged ones, by their items' latest activity.
        let Item::Room { room, .. } = &buckets[0].items[0] else {
            panic!()
        };
        let room = room.id;
        assert_eq!(
            items.badges().await.unwrap(),
            Badges {
                buckets: vec![
                    BadgedBucket {
                        name: "Now".into(),
                        badge: Badge::Count,
                        items: vec![Activity::Room { room, event: None }],
                    },
                    BadgedBucket {
                        name: "Else".into(),
                        badge: Badge::Dot,
                        items: vec![],
                    },
                ],
                // An invitation counts as one.
                marks: Marks {
                    home: Mark {
                        count: 1,
                        dot: false
                    },
                    rooms: vec![(
                        room,
                        Mark {
                            count: 1,
                            dot: false
                        }
                    )],
                    ..Marks::default()
                },
            }
        );

        // A dot is of what is unseen: what is new in a place since it was
        // seen, or in Home, which holds every bucket's.
        items.configure_home(vec![bucket("Todo", "is:todo", Some(Badge::Dot))]);
        let dots = || async {
            let marks = items.badges().await.unwrap().marks;
            (marks.home.dot, marks.todos.dot)
        };
        assert_eq!(dots().await, (true, true));
        let seen = dispatcher
            .dispatch(crate::seen::MarkSeen {
                place: Place::Todos,
            })
            .await
            .unwrap();
        assert_eq!(dots().await, (false, false));
        todos
            .create("Pay bills".into(), String::new(), None, None)
            .await
            .unwrap();
        assert_eq!(dots().await, (true, true));
        let home = dispatcher
            .dispatch(crate::seen::MarkSeen { place: Place::Home })
            .await
            .unwrap();
        assert_eq!(dots().await, (false, false));
        // Undone, as any op: the new to-do is unseen again, as the To-dos
        // were seen before it.
        dispatcher.undo(home).await.unwrap();
        assert_eq!(dots().await, (true, true));
        dispatcher.undo(seen).await.unwrap();
        assert_eq!(dots().await, (true, true));
        // Seeing is done by showing, which `z` passes over; its undo is
        // the user's.
        let log = Arc::new(uplink_tx_log::LogReader::new(
            uplink_storage::open_reader(dir.path()).unwrap(),
            10,
        ));
        let (_, history, _) = log.history(None, 4).await.unwrap();
        let listed: Vec<_> = history
            .iter()
            .map(|h| (h.entry.description.as_str(), h.entry.shown))
            .collect();
        assert_eq!(
            listed,
            [
                ("Not seen: To-dos", false),
                ("Not seen: Home", false),
                ("Seen: Home", true),
                ("Added 'Pay bills'", false),
            ]
        );
    }
}
