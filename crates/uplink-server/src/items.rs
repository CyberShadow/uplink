//! What a query matches of every kind (SPEC §8.7): mail's conversations,
//! rooms, to-dos and events, in one read, so as of one log position, and
//! in one list, the most recently active first. Each domain matches its
//! own; a term a kind lacks is false of it. The home view (SPEC §12.13) is
//! the matches of its buckets' queries.

use crate::seen::Watermark;
use arc_swap::ArcSwap;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uplink_config::HomeBucket;
use uplink_email::query::{self as mail, ListError, Unknown};
use uplink_matrix::query::{Group, Rooms};
use uplink_protocol::{
    calendar::CalendarEvent,
    mail::{EntryId, ListPosition, ListScope},
    matrix::RoomEventId,
    todos::Todo,
    Arrival, Arrived, Badge, Badges, ErrorCode, Item, ItemKind, ItemPosition, LogEntry, Mark,
    Marks, Place, ServerError, ServerEvent, ServerResponse, TxLogId, UnknownNames,
};
use uplink_query::{Kind, Query, Term};
use uplink_realtime::Listing;
use uplink_storage::rusqlite::{Connection, Transaction};
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The most items a page may ask for, as a list of mail.
const MAX_LIMIT: u32 = uplink_email::service::MAX_LIMIT;

/// Of how many of a badged bucket's most recently active items what came
/// is looked for: more than come at once.
const ARRIVALS_AMONG: u32 = 100;

/// How recent what came must be to be news: mail received or a message
/// sent before, however newly it is stored (an account's first sync, a
/// room's history), is not (SPEC §12.16).
const RECENT: i64 = 60 * 60 * 1000;

/// The badges, what came into the badged buckets since the watermark
/// they were derived after, and the watermark they were derived at.
pub struct Derived {
    pub badges: Badges,
    pub arrivals: Vec<Arrival>,
    pub watermark: Watermark,
}

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

/// What changed of the items, as log entries say: for the badges to
/// check again.
#[derive(Debug, Default)]
pub struct Changed {
    /// The items named.
    pub among: uplink_query::Among,
    /// The events, which the log names only by their calendars: all.
    pub events: bool,
    /// What the log does not name item by item: everything, as at first.
    pub everything: bool,
}

impl Changed {
    /// With what `entry`'s events say changed.
    pub fn came(&mut self, entry: &LogEntry) {
        entry.events.iter().for_each(|event| self.add(event));
    }

    /// With what `event` says changed.
    fn add(&mut self, event: &ServerEvent) {
        let among = &mut self.among;
        match event {
            ServerEvent::EntriesAdded(added) => {
                among.entries.extend(added.entries.iter().map(|e| e.0))
            }
            ServerEvent::LabelsChanged(changed) => among
                .entries
                .extend(changed.changes.iter().map(|c| c.entry.0)),
            ServerEvent::RoomsChanged(changed) => {
                among.rooms.extend(changed.rooms.iter().map(|r| r.0))
            }
            ServerEvent::TimelineAdded(added) => among.rooms.push(added.room.0),
            ServerEvent::TimelineFilled(filled) => among.rooms.push(filled.room.0),
            ServerEvent::TimelineChanged(changed) => among.rooms.push(changed.room.0),
            ServerEvent::TodosChanged(changed) => {
                among.todos.extend(changed.todos.iter().map(|t| t.0))
            }
            ServerEvent::CalendarChanged(_) => self.events = true,
            // Gone, they are of no conversation to check again; a label
            // renamed or deleted, of what its name meant; what is seen,
            // of every dot.
            ServerEvent::EntriesRemoved(_)
            | ServerEvent::LabelListChanged(_)
            | ServerEvent::SeenChanged => self.everything = true,
            ServerEvent::AccountChanged(_)
            | ServerEvent::ImageRuleChanged(_)
            | ServerEvent::DraftEdited(_)
            | ServerEvent::DraftsChanged(_)
            | ServerEvent::OutboxChanged(_)
            | ServerEvent::MatrixSendsChanged(_) => {}
        }
    }
}

/// The badged buckets, as configured when last derived, and what each
/// holds: kept by checking again only what changes.
pub struct Badged {
    buckets: Vec<Badging>,
}

struct Badging {
    name: String,
    badge: Badge,
    query: Query,
    held: Held,
}

impl Badged {
    fn badges(&self) -> Badges {
        let mut places = Places::default();
        for bucket in &self.buckets {
            places.add(bucket.badge, &bucket.held);
        }
        Badges {
            marks: places.into(),
        }
    }
}

/// What a badged bucket holds, item by item, with where each is.
#[derive(Debug, Clone, Default, PartialEq)]
struct Held {
    /// Conversations by key.
    mail: HashMap<String, mail::Placed>,
    /// Rooms, by what each counts: its messages that notify, or 1.
    rooms: HashMap<uplink_protocol::matrix::RoomId, u32>,
    todos: HashSet<uplink_protocol::todos::TodoId>,
    events: HashSet<uplink_protocol::calendar::EventId>,
}

/// Of kinds checked again, those wholly.
struct Whole {
    rooms: bool,
    events: bool,
}

/// What a bucket's query holds of what changed: of the items named, and
/// of the kinds checked again wholly.
struct Fresh {
    among: Held,
    rooms: Option<HashMap<uplink_protocol::matrix::RoomId, u32>>,
    events: Option<HashSet<uplink_protocol::calendar::EventId>>,
}

impl Held {
    /// The items of `among` (the mail of `conversations`), and the kinds
    /// checked wholly, as `fresh` has them now.
    fn update(
        &mut self,
        conversations: &HashSet<String>,
        among: &uplink_query::Among,
        fresh: Fresh,
    ) {
        self.mail.retain(|key, _| !conversations.contains(key));
        self.rooms.retain(|room, _| !among.rooms.contains(&room.0));
        self.todos.retain(|todo| !among.todos.contains(&todo.0));
        self.events.retain(|event| !among.events.contains(&event.0));
        self.mail.extend(fresh.among.mail);
        self.rooms.extend(fresh.among.rooms);
        self.todos.extend(fresh.among.todos);
        self.events.extend(fresh.among.events);
        if let Some(rooms) = fresh.rooms {
            self.rooms = rooms;
        }
        if let Some(events) = fresh.events {
            self.events = events;
        }
    }
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
    /// Adds a bucket's: a conversation in each of its labels and its
    /// account, a room by its messages that notify, or 1.
    fn add(&mut self, badge: Badge, held: &Held) {
        let count = |n: usize| n as u32;
        for placed in held.mail.values() {
            for label in &placed.labels {
                self.labels.entry(*label).or_default().add(badge, 1);
            }
            self.accounts
                .entry(placed.account.clone())
                .or_default()
                .add(badge, 1);
        }
        self.home.add(badge, count(held.mail.len()));
        for (room, n) in &held.rooms {
            self.rooms.entry(*room).or_default().add(badge, *n);
        }
        self.home.add(badge, count(held.rooms.len()));
        self.todos.add(badge, count(held.todos.len()));
        self.home.add(badge, count(held.todos.len()));
        self.calendar.add(badge, count(held.events.len()));
        self.home.add(badge, count(held.events.len()));
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
    /// `readers` are read-only connections to the database, on which
    /// queries run at once: listing windows again and deriving badges are
    /// many, and long.
    pub fn new(
        readers: Vec<Connection>,
        mail_accounts: Vec<mail::ConfiguredAccount>,
        matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount>,
        home: Vec<HomeBucket>,
    ) -> Self {
        Self {
            reader: Reader::pool(readers),
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
        checked(limit)?;
        let (as_of, page, partial_rooms) = self
            .read_page(query, UnknownNames::Invalid, after, limit)
            .await?;
        Ok(ServerResponse::Items {
            as_of,
            items: page.items,
            next: page.next,
            scope: page.scope,
            partial_rooms,
        })
    }

    /// A subscription's window (SPEC §5.7): the first `limit` items of
    /// what `query` matches, as `list` lists them, however many.
    pub async fn window(
        &self,
        query: String,
        unknown: UnknownNames,
        limit: u32,
    ) -> Result<Listing, ServerError> {
        let (as_of, page, partial_rooms) = self.read_page(query, unknown, None, limit).await?;
        Ok(Listing {
            as_of,
            items: page.items,
            more: page.next.is_some(),
            scope: page.scope,
            partial_rooms,
        })
    }

    async fn read_page(
        &self,
        query: String,
        unknown: UnknownNames,
        after: Option<ItemPosition>,
        limit: u32,
    ) -> Result<(TxLogId, Page, Vec<uplink_protocol::matrix::RoomId>), ServerError> {
        let now = now();
        let today = uplink_query::today(now);
        let query = uplink_query::parse(&query, now).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                match unknown {
                    UnknownNames::Invalid => this.check_names(&tx, &query)?,
                    UnknownNames::False => {}
                }
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                let others = others(&tx, &rooms, &query, today, now)?;
                let partial_rooms = rooms.partly_searched(&query);
                Ok((
                    as_of,
                    this.page(&tx, &query, others, after, limit)?,
                    partial_rooms,
                ))
            })
            .await
    }

    /// The buckets, as configured, each with the query that lists what it
    /// matches, less what those before it list. A name no kind has is
    /// false in it, not an error: the default buckets name Gmail's labels
    /// and Element's tags, which an account may lack.
    pub fn home(&self) -> Response {
        let buckets = self
            .buckets(now())
            .into_iter()
            .map(|bucket| uplink_protocol::HomeBucket {
                name: bucket.name,
                query: bucket.text,
            })
            .collect();
        Ok(ServerResponse::Home { buckets })
    }

    /// What the badged buckets hold (SPEC §12.14), as `home` lists them,
    /// item by item; and so the marks of the places their items are in.
    /// And what came into them from others since the watermark `since`
    /// (SPEC §12.16), of the `ARRIVALS_AMONG` most recently active items
    /// of each: nothing, at first.
    pub async fn badged(&self, since: Option<Watermark>) -> Result<(Badged, Derived), ServerError> {
        let now = now();
        let today = uplink_query::today(now);
        let buckets: Vec<_> = self
            .buckets(now)
            .into_iter()
            .filter_map(|bucket| Some((bucket.badge?, bucket)))
            .collect();
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                let seen = Seen::load(&tx).map_err(internal)?;
                let watermark = crate::seen::latest(&tx).map_err(internal)?;
                let mut badged = Badged { buckets: vec![] };
                let mut arrivals = vec![];
                for (badge, bucket) in buckets {
                    let held = this.hold(&tx, &rooms, &seen, badge, &bucket.query, today, now)?;
                    if let Some(since) = &since {
                        let others = others(&tx, &rooms, &bucket.query, today, now)?;
                        let page = this.page(&tx, &bucket.query, others, None, ARRIVALS_AMONG)?;
                        arrivals.extend(this.arrivals(
                            &tx,
                            &bucket.name,
                            badge,
                            &page.items,
                            since,
                            now,
                        )?);
                    }
                    badged.buckets.push(Badging {
                        name: bucket.name,
                        badge,
                        query: bucket.query,
                        held,
                    });
                }
                let badges = badged.badges();
                Ok((
                    badged,
                    Derived {
                        badges,
                        arrivals,
                        watermark,
                    },
                ))
            })
            .await
    }

    /// `badged` kept: what `changed` names checked again, and of it what
    /// came since the watermark `since`. What the log does not name item
    /// by item, everything is derived anew for.
    pub async fn rebadged(
        &self,
        badged: Badged,
        changed: Changed,
        since: Watermark,
    ) -> Result<(Badged, Derived), ServerError> {
        if changed.everything {
            return self.badged(Some(since)).await;
        }
        let now = now();
        let today = uplink_query::today(now);
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                let seen = Seen::load(&tx).map_err(internal)?;
                let watermark = crate::seen::latest(&tx).map_err(internal)?;
                let entries: Vec<EntryId> =
                    changed.among.entries.iter().map(|&e| EntryId(e)).collect();
                let conversations: HashSet<String> = mail::conversation_keys(&tx, &entries)
                    .map_err(internal)?
                    .into_iter()
                    .collect();
                let mut badged = badged;
                let mut arrivals = vec![];
                for bucket in &mut badged.buckets {
                    let hold = |query: Query| {
                        this.hold(&tx, &rooms, &seen, bucket.badge, &query, today, now)
                    };
                    let of_kind = |kind| bucket.query.clone().and(Query::Term(Term::Kind(kind)));
                    // A space's rooms are of it however deeply, which it
                    // changes alone.
                    let spaces =
                        uplink_query::names(&bucket.query, &|t| matches!(t, Term::Space(_)));
                    let whole = Whole {
                        rooms: spaces && !changed.among.rooms.is_empty(),
                        events: changed.events,
                    };
                    let fresh = Fresh {
                        among: hold(bucket.query.clone().among(changed.among.clone()))?,
                        rooms: match whole.rooms {
                            true => Some(hold(of_kind(Kind::Room))?.rooms),
                            false => None,
                        },
                        events: match whole.events {
                            true => Some(hold(of_kind(Kind::Event))?.events),
                            false => None,
                        },
                    };
                    bucket.held.update(&conversations, &changed.among, fresh);
                    // What came is of what was stored since: what the log
                    // named.
                    let came = uplink_query::Among {
                        entries: changed.among.entries.clone(),
                        rooms: changed.among.rooms.clone(),
                        ..Default::default()
                    };
                    let limit = (came.entries.len() + came.rooms.len()) as u32;
                    if limit > 0 {
                        let query = bucket.query.clone().among(came);
                        let others = others(&tx, &rooms, &query, today, now)?;
                        let page = this.page(&tx, &query, others, None, limit)?;
                        arrivals.extend(this.arrivals(
                            &tx,
                            &bucket.name,
                            bucket.badge,
                            &page.items,
                            &since,
                            now,
                        )?);
                    }
                }
                let badges = badged.badges();
                Ok((
                    badged,
                    Derived {
                        badges,
                        arrivals,
                        watermark,
                    },
                ))
            })
            .await
    }

    /// What a badged bucket's `query` holds: of a `dot`, what is unseen
    /// (SPEC §12.15).
    #[allow(clippy::too_many_arguments)]
    fn hold(
        &self,
        tx: &Transaction,
        rooms: &Rooms,
        seen: &Seen,
        badge: Badge,
        query: &Query,
        today: i64,
        now: i64,
    ) -> Result<Held, ServerError> {
        let unseen = badge == Badge::Dot;
        let mail = mail::placed(tx, &self.mail_accounts, query, Unknown::False, unseen)
            .map_err(list_error)?;
        let others = others(tx, rooms, query, today, now)?;
        let others = match unseen {
            true => seen.unseen(&others),
            false => others,
        };
        Ok(Held {
            mail: mail
                .into_iter()
                .map(|placed| (placed.key.clone(), placed))
                .collect(),
            rooms: others
                .rooms
                .iter()
                .map(|group| (group.room.id, group.room.notifying.max(1)))
                .collect(),
            todos: others.todos.iter().map(|todo| todo.id).collect(),
            events: others.events.iter().map(|event| event.id).collect(),
        })
    }

    /// Of `items`, a bucket's, what came from others since `since`, and
    /// recently: what an item last active before then cannot have.
    fn arrivals(
        &self,
        tx: &Transaction,
        bucket: &str,
        badge: Badge,
        items: &[Item],
        since: &Watermark,
        now: i64,
    ) -> Result<Vec<Arrival>, ServerError> {
        let after = now - RECENT;
        let recent: Vec<Item> = items
            .iter()
            .filter(|item| item.position().at >= after)
            .cloned()
            .collect();
        Ok(self
            .arrived(tx, &recent, since, after)?
            .into_iter()
            .map(|item| Arrival {
                bucket: bucket.to_owned(),
                badge,
                item,
            })
            .collect())
    }

    /// What came from others since `since`, sent or received after
    /// `after`: of each conversation its newest entry, of each room its
    /// newest message that notifies; to-dos and events are the user's own.
    fn arrived(
        &self,
        tx: &Transaction,
        items: &[Item],
        since: &Watermark,
        after: i64,
    ) -> Result<Vec<Arrived>, ServerError> {
        let mut arrived = vec![];
        for item in items {
            match item {
                Item::Mail { conversation } => {
                    let since = EntryId(since.mail);
                    let entry = mail::arrived(tx, conversation, since, after).map_err(internal)?;
                    arrived.extend(entry.map(|entry| Arrived::Mail { entry }));
                }
                Item::Room { room, .. } => {
                    let mxid = &self
                        .matrix_accounts
                        .iter()
                        .find(|a| a.id == room.account)
                        .expect("rooms are of the configured accounts")
                        .mxid;
                    let since = RoomEventId(since.room_event);
                    let event = uplink_matrix::rooms::arrived(tx, room.id, mxid, since, after)
                        .map_err(internal)?;
                    arrived.extend(event.map(|event| Arrived::Message {
                        room: room.id,
                        event,
                    }));
                }
                Item::Todo { .. } | Item::Event { .. } => {}
            }
        }
        Ok(arrived)
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
            .filter(|item| after.is_none_or(|after| item.position() > after))
            .collect();
        items.sort_by_key(Item::position);
        // More mail is after the last of its page, so after the last here.
        let more = items.len() > limit as usize || mail.next.is_some();
        items.truncate(limit as usize);
        Ok(Page {
            next: more.then(|| items.last().expect("more is after some").position()),
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

    /// A room for the account `mx`, joined, of these events.
    fn room(events: Vec<uplink_matrix::rooms::ObservedEvent>) -> Synced {
        Synced {
            next_batch: "next".into(),
            rooms: vec![ObservedRoom {
                room_id: "!r:x".into(),
                name: "Rent talk".into(),
                encrypted: false,
                membership: Membership::Joined,
                space: false,
                direct: false,
                avatar: None,
                children: vec![],
                space_order: None,
                tags: vec![],
                topic: None,
                alias: None,
                pinned: vec![],
                may_pin: false,
                joined_members: 2,
                created: None,
                version: "11".into(),
                events,
                limited: false,
                own_receipts: vec![],
                fully_read: None,
            }],
        }
    }

    /// Kept by checking again what each log entry names, the badges are
    /// as derived anew, what each bucket holds too, and what came.
    #[tokio::test]
    async fn kept_badges_are_as_derived_anew() {
        use uplink_email::mailbox::{EntryLabels, MailboxObserved, NewEntry, ObservedLabel};
        use uplink_protocol::{mail::LabelKind, todos::TodoEdit, ServerResponse};
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        uplink_email::mailbox::register(&mut registry);
        uplink_matrix::register(&mut registry);
        uplink_todos::register(&mut registry);
        crate::seen::register(&mut registry);
        let hub = Arc::new(Hub::new(4096));
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            hub.clone(),
        );
        let store = uplink_storage::store::Store::mail(dir.path());
        let observed = |labels, added, updated| MailboxObserved {
            account_kind: "gmail",
            labels,
            rekeyed: vec![],
            added,
            updated,
            removed: vec![],
            sync_state: None,
        };
        let names = ["INBOX", "UNREAD", "STARRED", "TRASH", "SPAM", "Label_1"];
        let labels = names
            .iter()
            .map(|id| ObservedLabel {
                external_id: id.to_string(),
                name: id.to_string(),
                system: !id.starts_with("Label_"),
                kind: LabelKind::Tag,
            })
            .collect();
        dispatcher
            .observe("a".into(), observed(Some(labels), vec![], vec![]))
            .await
            .unwrap();
        let entry = |id: &str, thread: &str, labels: &[&str]| {
            let raw = format!(
                "Message-ID: <{id}@x>\r\nFrom: bob@x\r\nTo: a@x\r\nSubject: s {id}\r\n\r\nhi\r\n"
            );
            let raw = raw.as_bytes();
            NewEntry {
                external_id: id.to_owned(),
                thread_id: Some(thread.to_owned()),
                received_at: now(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                variant: store.put(raw).unwrap(),
                size_bytes: raw.len() as i64,
                parsed: uplink_email::parse::parse(raw),
            }
        };
        let message = |id: &str, notify: bool, highlight: bool| {
            let raw = serde_json::json!({
                "type": "m.room.message",
                "event_id": id,
                "sender": "@bob:x",
                "origin_server_ts": now(),
                "content": { "msgtype": "m.text", "body": "hi" },
            });
            uplink_matrix::rooms::ObservedEvent {
                push: uplink_matrix::rooms::Push { notify, highlight },
                ..uplink_matrix::rooms::ObservedEvent::parse(
                    &raw.to_string(),
                    None,
                    uplink_protocol::matrix::Encryption::None,
                )
                .unwrap()
            }
        };
        let in_room = |id: &str, events| {
            let mut synced = room(events);
            synced.rooms[0].room_id = id.to_owned();
            synced.rooms[0].name = id.to_owned();
            synced
        };
        let todos = uplink_todos::service::Todos::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
        );
        let items = Items::new(
            vec![uplink_storage::open_reader(dir.path()).unwrap()],
            vec![mail::ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
                identities: vec![],
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }],
            vec![uplink_matrix::service::ConfiguredAccount {
                id: "mx".into(),
                mxid: "@me:x".into(),
                history_members: 100,
            }],
            vec![],
        );
        let bucket = |name: &str, query: &str, badge| HomeBucket {
            name: name.into(),
            query: query.into(),
            badge: Some(badge),
        };
        items.configure_home(vec![
            bucket("Urgent", "is:starred OR is:mention", Badge::Count),
            bucket("Unread", "is:unread", Badge::Dot),
            bucket("Later", "is:todo OR label:Label_1", Badge::Count),
        ]);
        let mut entries = hub.subscribe();
        let (mut badged, first) = items.badged(None).await.unwrap();
        let mut since = first.watermark;
        let mut todo_ids = vec![];
        let mut came = 0;
        // Of the labels, by step: as many sets as there are steps of a kind.
        let sets: [&[&str]; 5] = [
            &["INBOX", "UNREAD"],
            &["INBOX", "STARRED"],
            &["Label_1", "UNREAD"],
            &[],
            &["TRASH", "STARRED"],
        ];
        for step in 0..60usize {
            let labelled = sets[step % sets.len()];
            match step % 6 {
                // Mail, into one of a few conversations.
                0 | 3 => {
                    let id = format!("m{step}");
                    let thread = format!("t{}", step % 4);
                    dispatcher
                        .observe(
                            "a".into(),
                            observed(None, vec![entry(&id, &thread, labelled)], vec![]),
                        )
                        .await
                        .unwrap();
                }
                // An earlier entry relabelled.
                1 => {
                    let id = format!("m{}", (step / 2) / 3 * 6);
                    let update = EntryLabels {
                        external_id: id,
                        labels: labelled.iter().map(|l| l.to_string()).collect(),
                    };
                    dispatcher
                        .observe("a".into(), observed(None, vec![], vec![update]))
                        .await
                        .unwrap();
                }
                // A message, into one of a few rooms; or one read by the
                // account, which leaves it.
                2 => {
                    let room = format!("!r{}:x", step / 6 % 3);
                    let event = format!("$e{step}");
                    let mut synced =
                        in_room(&room, vec![message(&event, step % 4 == 2, step % 8 == 2)]);
                    if step % 5 == 2 {
                        synced.rooms[0].own_receipts = vec![event];
                    }
                    dispatcher.observe("mx".into(), synced).await.unwrap();
                }
                4 => {
                    let created = todos
                        .create(format!("To do {step}"), String::new(), None, None)
                        .await
                        .unwrap();
                    let ServerResponse::Dispatched { .. } = created else {
                        panic!("{created:?}")
                    };
                    let listed = todos.list().await.unwrap();
                    let ServerResponse::Todos { todos: all, .. } = listed else {
                        panic!("{listed:?}")
                    };
                    todo_ids = all.iter().map(|t| t.id).collect();
                }
                // A to-do done.
                5 => {
                    if let Some(&todo) = todo_ids.get(step % 3) {
                        todos
                            .edit(todo, TodoEdit::Done { done: true })
                            .await
                            .unwrap();
                    }
                }
                _ => unreachable!(),
            }
            let mut changed = Changed::default();
            while let Ok(entry) = entries.try_recv() {
                changed.came(&entry);
            }
            assert!(!changed.everything, "step {step}");
            let (kept, derived) = items.rebadged(badged, changed, since).await.unwrap();
            let (anew, derived_anew) = items.badged(Some(since)).await.unwrap();
            let held = |b: &Badged| {
                b.buckets
                    .iter()
                    .map(|b| (b.name.clone(), b.held.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(held(&kept), held(&anew), "step {step}");
            assert_eq!(derived.badges, derived_anew.badges, "step {step}");
            assert_eq!(derived.arrivals, derived_anew.arrivals, "step {step}");
            came += derived.arrivals.len();
            badged = kept;
            since = derived.watermark;
        }
        // Something was counted, and something came.
        assert!(badged.badges().marks.home.count > 0);
        assert!(came > 0);
    }

    /// What came into a badged bucket since the watermark is told of, by
    /// the bucket; of an unbadged one, nothing.
    #[tokio::test]
    async fn arrivals_are_of_badged_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        uplink_matrix::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
        );
        let message = |id: &str| {
            let raw = serde_json::json!({
                "type": "m.room.message",
                "event_id": id,
                "sender": "@bob:x",
                "origin_server_ts": now(),
                "content": { "msgtype": "m.text", "body": "rent is due" },
            });
            let parse = uplink_matrix::rooms::ObservedEvent::parse;
            uplink_matrix::rooms::ObservedEvent {
                push: uplink_matrix::rooms::Push {
                    notify: true,
                    highlight: false,
                },
                ..parse(
                    &raw.to_string(),
                    None,
                    uplink_protocol::matrix::Encryption::None,
                )
                .unwrap()
            }
        };
        let observe = |events| dispatcher.observe("mx".into(), room(events));
        observe(vec![message("$1")]).await.unwrap();
        let items = Items::new(
            vec![uplink_storage::open_reader(dir.path()).unwrap()],
            vec![],
            vec![uplink_matrix::service::ConfiguredAccount {
                id: "mx".into(),
                mxid: "@me:x".into(),
                history_members: 100,
            }],
            vec![],
        );
        let bucket = |query: &str, badge| HomeBucket {
            name: query.into(),
            query: query.into(),
            badge,
        };
        let (_, first) = items.badged(None).await.unwrap();
        assert!(first.arrivals.is_empty(), "nothing came before the first");
        observe(vec![message("$2")]).await.unwrap();
        let arrivals = |buckets| {
            items.configure_home(buckets);
            let items = items.clone();
            async move {
                items
                    .badged(Some(first.watermark))
                    .await
                    .unwrap()
                    .1
                    .arrivals
            }
        };
        let arrived = arrivals(vec![bucket("is:notifying", Some(Badge::Dot))]).await;
        let [Arrival {
            bucket: name,
            badge: Badge::Dot,
            item: Arrived::Message { event, .. },
        }] = &arrived[..]
        else {
            panic!("{arrived:?}")
        };
        assert_eq!(name, "is:notifying");
        assert!(event.0 > first.watermark.room_event);
        assert!(arrivals(vec![bucket("is:notifying", None)])
            .await
            .is_empty());
    }

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
                        created: None,
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
            vec![uplink_storage::open_reader(dir.path()).unwrap()],
            vec![],
            vec![uplink_matrix::service::ConfiguredAccount {
                id: "mx".into(),
                mxid: "@me:x".into(),
                history_members: 100,
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
        let ServerResponse::Home { buckets } = items.home().unwrap() else {
            panic!()
        };
        let mut windows = vec![];
        for bucket in &buckets {
            let window = items
                .window(bucket.query.clone(), UnknownNames::False, 10)
                .await
                .unwrap();
            windows.push((bucket.name.as_str(), window));
        }
        let listed: Vec<_> = windows
            .iter()
            .map(|(name_of, w)| (*name_of, w.items.iter().map(name).collect::<Vec<_>>()))
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
        assert!(!windows[0].1.more);
        // As a search's, the name is refused.
        assert_eq!(
            items
                .window(buckets[2].query.clone(), UnknownNames::Invalid, 10)
                .await
                .err()
                .unwrap()
                .message,
            "no label or tag nowhere"
        );

        // The badged ones' marks.
        let Item::Room { room, .. } = &windows[0].1.items[0] else {
            panic!()
        };
        let room = room.id;
        assert_eq!(
            items.badged(None).await.unwrap().1.badges,
            Badges {
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
            let marks = items.badged(None).await.unwrap().1.badges.marks;
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
