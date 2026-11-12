//! What a query matches of every kind (SPEC §8.7): mail's conversations,
//! rooms, to-dos and events, in one read, so as of one log position. Each
//! domain matches its own; a term a kind lacks is false of it. The home
//! view (SPEC §12.13) is the matches of its buckets' queries.

use arc_swap::ArcSwap;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uplink_config::HomeBucket;
use uplink_email::query::{self as mail, ListError, Unknown};
use uplink_matrix::query::Rooms;
use uplink_protocol::{
    calendar::CalendarEvent,
    mail::{EntryId, ListPosition},
    matrix::Room,
    todos::Todo,
    Activity, Badge, BadgedBucket, Badges, ErrorCode, Mark, Marks, ServerError, ServerResponse,
    BADGE_LIMIT,
};
use uplink_query::{Query, Term};
use uplink_storage::rusqlite::{Connection, Transaction};
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The most conversations a page may ask for, as a list of mail.
const MAX_LIMIT: u32 = uplink_email::service::MAX_LIMIT;

/// The most rooms' messages a search lists: the latest; a room's own
/// search finds more.
const MESSAGES: u32 = 50;

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
        for room in &others.rooms {
            let n = room.notifying.max(1);
            self.rooms.entry(room.id).or_default().add(badge, n);
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

/// What a query matches of kinds other than mail.
struct Others {
    rooms: Vec<Room>,
    todos: Vec<Todo>,
    events: Vec<CalendarEvent>,
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

    pub async fn list(&self, query: String, after: Option<ListPosition>, limit: u32) -> Response {
        let (today, now) = checked(limit)?;
        let query = uplink_query::parse(&query, now).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                this.check_names(&tx, &query)?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let page = this.mail(&tx, &query, after, limit)?;
                // A page after `after` is of mail alone.
                let (others, messages) = match after {
                    None => {
                        let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                        let messages = rooms.messages(&tx, &query, MESSAGES).map_err(internal)?;
                        (others(&tx, &rooms, &query, today, now)?, messages)
                    }
                    Some(_) => (
                        Others {
                            rooms: vec![],
                            todos: vec![],
                            events: vec![],
                        },
                        vec![],
                    ),
                };
                Ok(ServerResponse::Items {
                    as_of,
                    conversations: page.conversations,
                    next: page.next,
                    scope: page.scope,
                    rooms: others.rooms,
                    messages,
                    todos: others.todos,
                    events: others.events,
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
                        let page = this.mail(&tx, &bucket.query, None, limit)?;
                        let others = others(&tx, &rooms, &bucket.query, today, now)?;
                        Ok(uplink_protocol::HomeBucket {
                            name: bucket.name,
                            query: bucket.text,
                            conversations: page.conversations,
                            more: page.next.is_some(),
                            rooms: others.rooms,
                            todos: others.todos,
                            events: others.events,
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
                let buckets = badged
                    .into_iter()
                    .map(|(badge, bucket)| {
                        let page = this.mail(&tx, &bucket.query, None, BADGE_LIMIT as u32)?;
                        let others = others(&tx, &rooms, &bucket.query, today, now)?;
                        let places =
                            mail::places(&tx, &this.mail_accounts, &bucket.query, Unknown::False)
                                .map_err(list_error)?;
                        marks.add(badge, &places, &others);
                        let mail = page
                            .conversations
                            .iter()
                            .map(|c| Activity::Mail { entry: c.latest.id });
                        let rooms = others.rooms.iter().map(|r| Activity::Room {
                            room: r.id,
                            event: r.latest,
                        });
                        let todos = others.todos.iter().map(|t| Activity::Todo { todo: t.id });
                        let events = others
                            .events
                            .iter()
                            .map(|e| Activity::Event { event: e.id });
                        let all = mail.chain(rooms).chain(todos).chain(events);
                        Ok(BadgedBucket {
                            name: bucket.name,
                            badge,
                            items: all.take(BADGE_LIMIT).collect(),
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

    fn mail(
        &self,
        tx: &Transaction,
        query: &Query,
        after: Option<ListPosition>,
        limit: u32,
    ) -> Result<mail::Page, ServerError> {
        mail::conversations(tx, &self.mail_accounts, query, Unknown::False, after, limit)
            .map_err(list_error)
    }

    /// Fails on a label or account no kind has: a misspelt name lists
    /// nothing, as in a list of mail.
    fn check_names(&self, tx: &Transaction, query: &Query) -> Result<(), ServerError> {
        for term in uplink_query::terms(query) {
            match term {
                Term::Label(name) => {
                    let known = mail::label_known(tx, name).map_err(internal)?
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
        rooms: rooms.matching(query),
        todos: uplink_todos::matching(tx, query, today).map_err(internal)?,
        events: uplink_calendar::matching(tx, query, now).map_err(internal)?,
    })
}

fn invalid(why: String) -> ServerError {
    error(ErrorCode::Invalid, why)
}

/// Today, as days since 1970-01-01, and now, in ms, for a `limit` checked.
fn list_error(e: ListError) -> ServerError {
    match e {
        ListError::Invalid(e) => invalid(e.0),
        ListError::Internal(e) => internal(e),
    }
}

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
        let names = |response: Response| match response.unwrap() {
            ServerResponse::Items {
                conversations,
                rooms,
                todos,
                events,
                ..
            } => {
                assert!(conversations.is_empty() && events.is_empty());
                let rooms = rooms.into_iter().map(|r| r.name);
                rooms
                    .chain(todos.into_iter().map(|t| t.title))
                    .collect::<Vec<_>>()
            }
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(names(list("rent").await), ["Rent talk", "Pay rent"]);
        assert_eq!(
            names(list("is:invite OR is:overdue").await),
            ["Rent talk", "Pay rent"]
        );
        assert_eq!(names(list("tag:home").await), ["Rent talk"]);
        assert_eq!(names(list("rent -account:mx").await), ["Pay rent"]);
        let refused = |response: Response| response.unwrap_err().message;
        assert_eq!(refused(list("tag:work").await), "no label or tag work");
        assert_eq!(refused(list("account:nobody").await), "no account nobody");
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
                let rooms = b.rooms.iter().map(|r| r.name.clone());
                let todos = b.todos.iter().map(|t| t.title.clone());
                (b.name.as_str(), rooms.chain(todos).collect::<Vec<_>>())
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
        let room = buckets[0].rooms[0].id;
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
    }
}
