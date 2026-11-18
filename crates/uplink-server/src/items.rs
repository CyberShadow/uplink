//! What a query matches of every kind (SPEC §8.7): mail's conversations,
//! rooms, to-dos and events, in one read, so as of one log position, and
//! in one list, the most recently active first. Each domain matches its
//! own; a term a kind lacks is false of it. The home view (SPEC §12.13) is
//! the matches of its buckets' queries.

use crate::seen::Watermark;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
};
use tokio::sync::watch;
use uplink_config::HomeBucket;
use uplink_email::query::{self as mail, ListError, Unknown};
use uplink_matrix::{
    query::{Group, Rooms},
    room_view::RoomsView,
};
use uplink_protocol::{
    calendar::CalendarEvent,
    mail::{EntryId, ListPosition, ListScope},
    matrix::{InvitationId, RoomEventId, RoomId},
    todos::Todo,
    Badge, Badges, Changed, ErrorCode, Item, ItemKey, ItemKind, ItemPosition, Mark, Marks,
    Notifications, Notifying, Place, ServerError, ServerResponse, TxLogId, UnknownNames, Unseen,
    Urgency,
};
use uplink_query::{Kind, Query, Term};
use uplink_realtime::{Attended, Listing};
use uplink_storage::rusqlite::{Connection, Transaction};
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The most items a page may ask for, as a list of mail.
const MAX_LIMIT: u32 = uplink_email::service::MAX_LIMIT;

#[derive(Clone)]
pub struct Items {
    reader: Reader,
    mail_accounts: Vec<mail::ConfiguredAccount>,
    rooms: Arc<RoomsView>,
    /// As configured, top to bottom; configured anew on reload.
    home: Arc<ArcSwap<Vec<HomeBucket>>>,
    /// As configured with them.
    notifications: Arc<ArcSwap<Notifications>>,
    /// What sessions attend, which is seen (SPEC §12.15).
    attended: watch::Receiver<Attended>,
}

/// A bucket of the home view, its query less those before it.
struct Bucket {
    name: String,
    badge: Option<Badge>,
    text: String,
    query: Query,
}

/// The items `changed` names, to check again of a query.
fn among(changed: &Changed) -> uplink_query::Among {
    uplink_query::Among {
        entries: changed.entries.iter().map(|e| e.0).collect(),
        rooms: changed.rooms.iter().map(|r| r.0).collect(),
        todos: changed.todos.iter().map(|t| t.0).collect(),
        events: vec![],
    }
}

/// Whether `query` holds rooms by a space, which holds them however
/// deeply: when a room changes, so may which rooms it holds.
fn of_spaces(query: &Query) -> bool {
    uplink_query::names(query, &|t| matches!(t, Term::Space(_)))
}

/// The keys of the conversations `changed` names: of its entries, and
/// those of entries gone.
fn conversations(tx: &Transaction, changed: &Changed) -> Result<HashSet<String>, ServerError> {
    Ok(mail::conversation_keys(tx, &changed.entries)
        .map_err(internal)?
        .into_iter()
        .chain(changed.conversations.iter().cloned())
        .collect())
}

/// The badged buckets, as configured when last derived, and what each
/// holds: kept by checking again only what changes.
pub struct Badged {
    buckets: Vec<Badging>,
}

struct Badging {
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
/// Of a query, the kinds what changed is checked again of wholly, rather
/// than item by item.
struct Whole {
    mail: bool,
    rooms: bool,
    todos: bool,
    events: bool,
}

impl Whole {
    /// Of `query`, by what `changed`: of a query naming a space, its
    /// rooms, as a room changed may change which rooms it holds; the
    /// events, which the log names only by their calendars. Time moves
    /// to-dos (`due:today`, `is:overdue`) and events (as of now) of any
    /// query, and mail and rooms of one bounded by a time
    /// (`newer_than:`).
    fn of(query: &Query, changed: &Changed) -> Self {
        let timed = changed.time
            && uplink_query::terms(query)
                .iter()
                .any(|term| matches!(term, Term::After(_) | Term::Before(_)));
        Self {
            mail: timed,
            rooms: timed || (of_spaces(query) && !changed.rooms.is_empty()),
            todos: changed.time,
            events: changed.time || changed.events,
        }
    }

    /// Whether `item` is of a kind checked again wholly.
    fn holds(&self, item: &Item) -> bool {
        match item {
            Item::Mail { .. } => self.mail,
            Item::Room { .. } => self.rooms,
            Item::Todo { .. } => self.todos,
            Item::Event { .. } => self.events,
        }
    }

    /// The kinds checked again wholly.
    fn kinds(&self) -> impl Iterator<Item = Kind> {
        [
            (Kind::Mail, self.mail),
            (Kind::Room, self.rooms),
            (Kind::Todo, self.todos),
            (Kind::Event, self.events),
        ]
        .into_iter()
        .filter_map(|(kind, whole)| whole.then_some(kind))
    }
}

/// What a bucket's query holds of what changed: of the items named, and
/// of the kinds checked again wholly.
struct Fresh {
    among: Held,
    /// Of each kind checked again wholly, what it holds of it.
    whole: Vec<(Kind, Held)>,
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
        for (kind, held) in fresh.whole {
            match kind {
                Kind::Mail => self.mail = held.mail,
                Kind::Room => self.rooms = held.rooms,
                Kind::Todo => self.todos = held.todos,
                Kind::Event => self.events = held.events,
            }
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

/// When the places were last seen, by their keys (SPEC §12.15): those
/// attended, now.
struct Seen(HashMap<String, Watermark>);

impl Seen {
    fn load(tx: &Transaction, attended: &Attended) -> anyhow::Result<Self> {
        let mut seen: HashMap<String, Watermark> = crate::seen::all(tx)?.into_iter().collect();
        let now = crate::seen::latest(tx)?;
        for place in attended.iter() {
            for key in crate::seen::keys(tx, place)? {
                seen.insert(key, now);
            }
        }
        Ok(Self(seen))
    }

    /// Of mail, as its queries have it.
    fn mail(&self) -> mail::Seen {
        self.0
            .iter()
            .map(|(key, watermark)| (key.clone(), watermark.mail))
            .collect()
    }

    /// Of `kind`, Home's, and each room's own.
    fn rooms<T>(&self, kind: fn(&Watermark) -> i64, of: fn(i64) -> T) -> (T, HashMap<RoomId, T>) {
        let home = self.0.get(&crate::seen::key(&Place::Home)).map_or(0, kind);
        let rooms = self
            .0
            .iter()
            .filter_map(|(key, watermark)| {
                let room = key
                    .strip_prefix("room:")?
                    .parse()
                    .expect("a room's key is of its id");
                Some((RoomId(room), of(kind(watermark))))
            })
            .collect();
        (of(home), rooms)
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
    /// many, and long. `rooms` are the configured Matrix accounts' rooms,
    /// as the writer keeps them.
    pub fn new(
        readers: Vec<Connection>,
        mail_accounts: Vec<mail::ConfiguredAccount>,
        rooms: Arc<RoomsView>,
        home: Vec<HomeBucket>,
        notifications: Notifications,
        attended: watch::Receiver<Attended>,
    ) -> Self {
        Self {
            reader: Reader::pool(readers),
            mail_accounts,
            rooms,
            home: Arc::new(ArcSwap::from_pointee(home)),
            notifications: Arc::new(ArcSwap::from_pointee(notifications)),
            attended,
        }
    }

    /// What is attended now.
    fn attended(&self) -> Attended {
        self.attended.borrow().clone()
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

    pub fn configure_home(&self, home: Vec<HomeBucket>, notifications: Notifications) {
        self.home.store(Arc::new(home));
        self.notifications.store(Arc::new(notifications));
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
    /// what `query` matches, as `list` lists them, however many. Of what
    /// it `held`, only what `changed` is checked again: what the query
    /// matches of it now takes its place, and what it no longer matches
    /// leaves, which more after it replaces.
    pub async fn window(
        &self,
        query: String,
        unknown: UnknownNames,
        limit: u32,
        held: uplink_realtime::Held,
        changed: Changed,
    ) -> Result<Listing<Item>, ServerError> {
        if changed.everything {
            let (as_of, page, partial_rooms) = self.read_page(query, unknown, None, limit).await?;
            return Ok(Listing {
                as_of,
                listed: page.items,
                more: page.next.is_some(),
                scope: page.scope,
                partial_rooms,
            });
        }
        let now = now();
        let today = uplink_query::today(now);
        // It listed, so it parses; its names were known then.
        let query = uplink_query::parse(&query, now).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let rooms = Rooms::of(&this.rooms, &tx).map_err(internal)?;
                let as_of = rooms.as_of();
                let page = |query: &Query, after, limit| {
                    let others = others(&tx, &rooms, query, today, now)?;
                    this.page(&tx, query, others, after, limit)
                };
                let conversations = conversations(&tx, &changed)?;
                let whole = Whole::of(&query, &changed);
                let touched = |item: &Item| {
                    whole.holds(item)
                        || match item {
                            Item::Mail { .. } => conversations.contains(&item.key().0),
                            Item::Room { room, .. } => changed.rooms.contains(&room.id),
                            Item::Todo { todo } => changed.todos.contains(&todo.id),
                            Item::Event { .. } => false,
                        }
                };
                // What it matches of what changed: of the first `limit`,
                // as no more can be of the window, and one too many, as
                // then it has more.
                let Page {
                    items: mut fresh,
                    scope,
                    ..
                } = page(&query.clone().among(among(&changed)), None, limit + 1)?;
                let of_kind = |kind| query.clone().and(Query::Term(Term::Kind(kind)));
                for kind in whole.kinds() {
                    fresh.extend(page(&of_kind(kind), None, limit + 1)?.items);
                }
                // Of more than it held, what is after its last is not known
                // whole: it is listed after it, if needed.
                let last = held
                    .more
                    .then(|| held.items.last().expect("more is after some").position());
                let mut items: Vec<Item> = held
                    .items
                    .into_iter()
                    .filter(|item| !touched(item))
                    .chain(fresh)
                    .filter(|item| last.is_none_or(|last| item.position() <= last))
                    .map(|item| (item.key(), item))
                    .collect::<HashMap<_, _>>()
                    .into_values()
                    .collect();
                items.sort_by_key(Item::position);
                let more = match (items.len().cmp(&(limit as usize)), last) {
                    (Ordering::Greater, _) => {
                        items.truncate(limit as usize);
                        true
                    }
                    // What is after it is listed: to fill it, and to know if
                    // there is more, one too many.
                    (Ordering::Less | Ordering::Equal, Some(last)) => {
                        let left = limit as usize - items.len();
                        let mut after = page(&query, Some(last), left as u32 + 1)?.items;
                        let more = after.len() > left;
                        after.truncate(left);
                        items.extend(after);
                        more
                    }
                    (Ordering::Less | Ordering::Equal, None) => false,
                };
                Ok(Listing {
                    as_of,
                    listed: items,
                    more,
                    scope,
                    partial_rooms: rooms.partly_searched(&tx, &query).map_err(internal)?,
                })
            })
            .await
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
                let rooms = Rooms::of(&this.rooms, &tx).map_err(internal)?;
                let as_of = rooms.as_of();
                let others = others(&tx, &rooms, &query, today, now)?;
                let partial_rooms = rooms.partly_searched(&tx, &query).map_err(internal)?;
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
        let buckets = self.buckets(now());
        let notifying = [Urgency::Interrupting, Urgency::Quiet]
            .into_iter()
            .filter_map(|urgency| {
                let of: Vec<String> = buckets
                    .iter()
                    .filter(|bucket| bucket.badge.and_then(Badge::urgency) == Some(urgency))
                    .map(|bucket| format!("({})", bucket.text))
                    .collect();
                (!of.is_empty()).then(|| Notifying {
                    urgency,
                    query: of.join(" OR "),
                })
            })
            .collect();
        Ok(ServerResponse::Home {
            buckets: buckets
                .into_iter()
                .map(|bucket| uplink_protocol::HomeBucket {
                    name: bucket.name,
                    query: bucket.text,
                })
                .collect(),
            notifications: **self.notifications.load(),
            notifying,
        })
    }

    /// What the badged buckets hold (SPEC §12.14), as `home` lists them,
    /// item by item; and so the marks of the places their items are in.
    pub async fn badged(&self) -> Result<(Badged, Badges), ServerError> {
        let now = now();
        let today = uplink_query::today(now);
        let buckets: Vec<_> = self
            .buckets(now)
            .into_iter()
            .filter_map(|bucket| Some((bucket.badge?, bucket)))
            .collect();
        let this = self.clone();
        let attended = self.attended();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let rooms = Rooms::of(&this.rooms, &tx).map_err(internal)?;
                let seen = Seen::load(&tx, &attended).map_err(internal)?;
                let mut badged = Badged { buckets: vec![] };
                for (badge, bucket) in buckets {
                    let held = this.hold(&tx, &rooms, &seen, badge, &bucket.query, today, now)?;
                    badged.buckets.push(Badging {
                        badge,
                        query: bucket.query,
                        held,
                    });
                }
                let badges = badged.badges();
                Ok((badged, badges))
            })
            .await
    }

    /// `badged` kept: what `changed` names checked again; of what is
    /// seen, the `dot` buckets derived anew. What the log does not name
    /// item by item, everything is derived anew for.
    pub async fn rebadged(
        &self,
        badged: Badged,
        changed: Changed,
    ) -> Result<(Badged, Badges), ServerError> {
        if changed.everything {
            return self.badged().await;
        }
        let now = now();
        let today = uplink_query::today(now);
        let this = self.clone();
        let attended = self.attended();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let rooms = Rooms::of(&this.rooms, &tx).map_err(internal)?;
                let seen = Seen::load(&tx, &attended).map_err(internal)?;
                let conversations = conversations(&tx, &changed)?;
                let among = among(&changed);
                let mut badged = badged;
                for bucket in &mut badged.buckets {
                    let hold = |query: Query| {
                        this.hold(&tx, &rooms, &seen, bucket.badge, &query, today, now)
                    };
                    // A `dot` holds what is unseen: of what is seen, all of
                    // it may change.
                    if changed.seen && bucket.badge == Badge::Dot {
                        bucket.held = hold(bucket.query.clone())?;
                        continue;
                    }
                    let of_kind = |kind| bucket.query.clone().and(Query::Term(Term::Kind(kind)));
                    let fresh = Fresh {
                        among: hold(bucket.query.clone().among(among.clone()))?,
                        whole: Whole::of(&bucket.query, &changed)
                            .kinds()
                            .map(|kind| Ok((kind, hold(of_kind(kind))?)))
                            .collect::<Result<_, ServerError>>()?,
                    };
                    bucket.held.update(&conversations, &among, fresh);
                }
                let badges = badged.badges();
                Ok((badged, badges))
            })
            .await
    }

    /// Whether anything was stored since `place` was last seen.
    pub async fn stale(&self, place: Place) -> Result<bool, ServerError> {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                crate::seen::stale(&tx, &place).map_err(internal)
            })
            .await
    }

    /// What a window of the unseen lists (SPEC §5.7, §12.15): the first
    /// `limit` unseen messages `query` matches, each as it matches it
    /// within its item, the newest first: mail's entries, rooms' messages
    /// and invitations.
    pub async fn unseen(
        &self,
        query: String,
        unknown: UnknownNames,
        limit: u32,
    ) -> Result<Listing<Unseen>, ServerError> {
        let now = now();
        let query = uplink_query::parse(&query, now).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        let attended = self.attended();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                match unknown {
                    UnknownNames::Invalid => this.check_names(&tx, &query)?,
                    UnknownNames::False => {}
                }
                let rooms = Rooms::of(&this.rooms, &tx).map_err(internal)?;
                let seen = Seen::load(&tx, &attended).map_err(internal)?;
                let (entries, mail_more) = mail::unseen(
                    &tx,
                    &this.mail_accounts,
                    &query,
                    Unknown::False,
                    &seen.mail(),
                    limit,
                )
                .map_err(list_error)?;
                let (home, own) = seen.rooms(|w| w.room_event, RoomEventId);
                let (messages, rooms_more) = rooms
                    .unseen(&tx, &query, home, &own, limit)
                    .map_err(internal)?;
                let (home, own) = seen.rooms(|w| w.invitation, InvitationId);
                let invitations = rooms
                    .unseen_invitations(&tx, &query, home, &own)
                    .map_err(internal)?;
                let mut unseen: Vec<Unseen> = entries
                    .into_iter()
                    .map(|(key, entry)| Unseen::Mail {
                        conversation: ItemKey(key),
                        entry,
                    })
                    .chain(messages.into_iter().map(|(room, event)| Unseen::Message {
                        room: room.id,
                        name: room.name.clone(),
                        event: Box::new(event),
                    }))
                    .chain(
                        invitations
                            .into_iter()
                            .map(|(room, invitation)| Unseen::Invite {
                                room: room.id,
                                name: room.name.clone(),
                                invitation,
                            }),
                    )
                    .collect();
                unseen.sort_by_key(Unseen::position);
                let more = mail_more || rooms_more || unseen.len() > limit as usize;
                unseen.truncate(limit as usize);
                Ok(Listing {
                    as_of: rooms.as_of(),
                    listed: unseen,
                    more,
                    scope: (),
                    partial_rooms: rooms.partly_searched(&tx, &query).map_err(internal)?,
                })
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
        let mail_seen = seen.mail();
        let mail = mail::placed(
            tx,
            &self.mail_accounts,
            query,
            Unknown::False,
            unseen.then_some(&mail_seen),
        )
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

    /// The buckets, each with its query less the items of those before
    /// it (`-has:(…)`, not `-(…)`, which is of each message: a room with
    /// a message of an earlier bucket and one not would be in both): so an
    /// item is in the first that matches it.
    fn buckets(&self, now: i64) -> Vec<Bucket> {
        let mut before: Vec<String> = vec![];
        let mut buckets = vec![];
        for bucket in self.home.load_full().iter() {
            let text = std::iter::once(format!("({})", bucket.query))
                .chain(before.iter().map(|q| format!("-has:({q})")))
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
                            .rooms
                            .accounts()
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

    /// As configured by default.
    const NOTIFICATIONS: Notifications = Notifications {
        limit: 100,
        idle_minutes: 5,
    };

    /// A room for the account `mx`, joined, of these events, synced by a
    /// request sent after `as_of`.
    fn room(
        as_of: uplink_protocol::TxLogId,
        room_id: &str,
        membership: Membership,
        events: Vec<uplink_matrix::rooms::ObservedEvent>,
    ) -> Synced {
        Synced {
            next_batch: "next".into(),
            packs: None,
            as_of,
            rooms: vec![ObservedRoom {
                room_id: room_id.into(),
                name: room_id.into(),
                encrypted: false,
                membership,
                space: false,
                direct: false,
                avatar: None,
                bridge: None,
                children: vec![],
                space_order: None,
                tags: None,
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
                image_packs: vec![],
                invitation: (membership == Membership::Invited).then(|| {
                    uplink_matrix::rooms::ObservedInvitation {
                        inviter: "@bob:x".into(),
                        inviter_name: None,
                    }
                }),
            }],
        }
    }

    /// A log entry's windows and badges are checked again by what it
    /// names, of the rooms as the writer keeps them (SPEC §5.7, §9.5): the
    /// work of a message to one room is as much however many rooms there
    /// are, not of listing them all again for each window; and so is the
    /// windows of the unseen's, listed anew, of the rooms with messages
    /// since they were seen.
    #[tokio::test]
    async fn a_message_costs_its_windows_as_much_however_many_rooms() {
        use uplink_storage::work::Work;

        let message = |id: String| {
            let raw = serde_json::json!({
                "type": "m.room.message",
                "event_id": id,
                "sender": "@bob:x",
                "origin_server_ts": now(),
                "content": { "msgtype": "m.text", "body": "hi" },
            });
            uplink_matrix::rooms::ObservedEvent::parse(
                &raw.to_string(),
                None,
                uplink_protocol::matrix::Encryption::None,
            )
            .unwrap()
        };
        // Of `rooms` rooms of a few unread messages each, the work of
        // one more to one of them.
        let work_of = |rooms: usize| async move {
            let dir = tempfile::tempdir().unwrap();
            let mut registry = Registry::new();
            uplink_matrix::register(&mut registry);
            crate::seen::register(&mut registry);
            let hub = Arc::new(Hub::new(64));
            let dispatcher = Dispatcher::new(
                uplink_storage::open(dir.path()).unwrap(),
                registry,
                hub.clone(),
            );
            // Each synced as the log is then.
            let log = uplink_storage::open_reader(dir.path()).unwrap();
            let as_of = || uplink_tx_log::as_of(&log).unwrap();
            let synced = Synced {
                rooms: (0..rooms)
                    .flat_map(|r| {
                        let events = (0..3).map(|m| message(format!("$e{r}-{m}"))).collect();
                        room(as_of(), &format!("!r{r}:x"), Membership::Joined, events).rooms
                    })
                    .collect(),
                ..room(as_of(), "!r0:x", Membership::Joined, vec![])
            };
            dispatcher.observe("mx".into(), synced).await.unwrap();
            let reader = uplink_storage::open_reader(dir.path()).unwrap();
            let work = Work::default();
            work.count_on(&reader);
            let items = Items::new(
                vec![reader],
                vec![],
                RoomsView::kept(
                    &dispatcher,
                    vec![uplink_matrix::service::ConfiguredAccount {
                        id: "mx".into(),
                        mxid: "@me:x".into(),
                        history_members: 2,
                    }],
                )
                .unwrap(),
                [
                    ("Urgent", "is:mention OR is:invite", Badge::Alert),
                    ("Unread", "is:unread", Badge::Dot),
                ]
                .map(|(name, query, badge)| HomeBucket {
                    name: name.into(),
                    query: query.into(),
                    badge: Some(badge),
                })
                .into(),
                NOTIFICATIONS,
                hub.attended(),
            );
            let queries = [
                "is:unread",
                "is:mention OR is:invite",
                "is:unread -is:mention",
            ];
            let mut windows = vec![];
            for query in queries {
                let held = uplink_realtime::Held {
                    items: vec![],
                    more: false,
                };
                let listing = items
                    .window(
                        query.into(),
                        UnknownNames::False,
                        20,
                        held,
                        Changed::everything(),
                    )
                    .await
                    .unwrap();
                windows.push(listing);
            }
            // Seen, as it is while used: what is unseen is what came since.
            dispatcher
                .dispatch(crate::seen::MarkSeen { place: Place::Home })
                .await
                .unwrap();
            let (badged, _) = items.badged().await.unwrap();
            let mut entries = hub.subscribe();
            let more = room(
                as_of(),
                &format!("!r{}:x", rooms / 2),
                Membership::Joined,
                vec![message("$more".into())],
            );
            dispatcher.observe("mx".into(), more).await.unwrap();
            let mut changed = Changed::default();
            while let Ok(entry) = entries.try_recv() {
                changed.came(&entry);
            }
            let before = work.steps();
            for (query, listing) in queries.iter().zip(windows) {
                let held = uplink_realtime::Held {
                    items: listing.listed,
                    more: listing.more,
                };
                items
                    .window(
                        (*query).into(),
                        UnknownNames::False,
                        20,
                        held,
                        changed.clone(),
                    )
                    .await
                    .unwrap();
            }
            items.rebadged(badged, changed).await.unwrap();
            // The windows of the unseen, listed anew.
            for query in queries {
                items
                    .unseen(query.into(), UnknownNames::False, NOTIFICATIONS.limit)
                    .await
                    .unwrap();
            }
            work.steps() - before
        };
        let (few, many) = (work_of(40).await, work_of(160).await);
        assert!(
            (many as f64) < 1.5 * few as f64,
            "of 40 rooms {few} steps, of 160 {many}"
        );
    }

    /// Kept by checking again what each log entry names, the badges are
    /// as derived anew, and what each bucket holds too; and windows are as
    /// listed anew.
    #[tokio::test]
    async fn kept_badges_and_windows_are_as_derived_anew() {
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
        // Each listed as the log is then.
        let reader = uplink_storage::open_reader(dir.path()).unwrap();
        let observed = |labels, added, updated| MailboxObserved {
            as_of: uplink_tx_log::as_of(&reader).unwrap(),
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
            RoomsView::kept(
                &dispatcher,
                vec![uplink_matrix::service::ConfiguredAccount {
                    id: "mx".into(),
                    mxid: "@me:x".into(),
                    history_members: 100,
                }],
            )
            .unwrap(),
            vec![],
            NOTIFICATIONS,
            dispatcher.hub().attended(),
        );
        let bucket = |name: &str, query: &str, badge| HomeBucket {
            name: name.into(),
            query: query.into(),
            badge: Some(badge),
        };
        items.configure_home(
            vec![
                bucket(
                    "Urgent",
                    "is:starred OR is:mention OR is:invite",
                    Badge::Alert,
                ),
                bucket("Unread", "is:unread", Badge::Dot),
                bucket("Later", "is:todo OR label:Label_1", Badge::Count),
            ],
            NOTIFICATIONS,
        );
        let mut entries = hub.subscribe();
        // Few, for items to leave them, and more to come in after.
        let window = |query: &str, limit, held, changed| {
            items.window(query.into(), UnknownNames::False, limit, held, changed)
        };
        let anew = || uplink_realtime::Held {
            items: vec![],
            more: false,
        };
        let mut windows = vec![];
        for (query, limit) in [
            ("is:unread", 2),
            ("is:starred OR is:mention", 3),
            ("is:todo OR label:Label_1", 2),
            ("is:unread OR is:todo", 4),
            ("label:INBOX", 3),
        ] {
            let listing = window(query, limit, anew(), Changed::everything())
                .await
                .unwrap();
            windows.push((query, limit, listing));
        }
        let mut left = 0;
        let (mut badged, _) = items.badged().await.unwrap();
        let mut todo_ids = vec![];
        let mut seen_marks = vec![];
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
                // Mail, into one of a few conversations; or, as a sync
                // lists them, into several at once.
                0 | 3 => {
                    let id = format!("m{step}");
                    let thread = format!("t{}", step % 4);
                    let mut added = vec![entry(&id, &thread, labelled)];
                    if step % 4 == 3 {
                        added.extend((1..5).map(|n| {
                            entry(&format!("{id}-{n}"), &format!("b{step}-{n}"), labelled)
                        }));
                    }
                    dispatcher
                        .observe("a".into(), observed(None, added, vec![]))
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
                    let id = format!("!r{}:x", step / 6 % 3);
                    let event = format!("$e{step}");
                    let mut synced = room(
                        uplink_tx_log::as_of(&reader).unwrap(),
                        &id,
                        Membership::Joined,
                        vec![message(&event, step % 4 == 2, step % 8 == 2)],
                    );
                    if step % 5 == 2 {
                        synced.rooms[0].own_receipts = vec![event];
                    }
                    dispatcher.observe("mx".into(), synced).await.unwrap();
                    // And now and then an invitation.
                    if step % 4 == 2 {
                        let invite = room(
                            uplink_tx_log::as_of(&reader).unwrap(),
                            &format!("!i{step}:x"),
                            Membership::Invited,
                            vec![],
                        );
                        dispatcher.observe("mx".into(), invite).await.unwrap();
                    }
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
            // Now and then mail gone: of a conversation that holds others
            // still (`t3`'s, of every twelfth step from the third), and of
            // one it was alone in.
            if step >= 23 && step % 12 == 11 {
                let gone = format!("m{}", step - 20);
                let removed = MailboxObserved {
                    removed: vec![format!("{gone}-1"), gone],
                    ..observed(None, vec![], vec![])
                };
                dispatcher.observe("a".into(), removed).await.unwrap();
            }
            // Now and then places seen, and no longer (undone).
            match step % 7 {
                3 => {
                    let place = [Place::Home, Place::Inboxes, Place::Todos][step / 7 % 3].clone();
                    let mark = dispatcher.dispatch(crate::seen::MarkSeen { place });
                    seen_marks.push(mark.await.unwrap());
                }
                6 => {
                    if let Some(mark) = seen_marks.pop() {
                        dispatcher.undo(mark).await.unwrap();
                    }
                }
                _ => {}
            }
            let mut changed = Changed::default();
            while let Ok(entry) = entries.try_recv() {
                changed.came(&entry);
            }
            assert!(!changed.everything, "step {step}");
            for (query, limit, listing) in &mut windows {
                let held = uplink_realtime::Held {
                    items: listing.listed.clone(),
                    more: listing.more,
                };
                let kept = window(query, *limit, held, changed.clone()).await.unwrap();
                let listed = window(query, *limit, anew(), Changed::everything())
                    .await
                    .unwrap();
                let keys = |l: &Listing<Item>| l.listed.iter().map(Item::key).collect::<Vec<_>>();
                assert_eq!(keys(&kept), keys(&listed), "{query}, step {step}");
                assert_eq!(kept.listed, listed.listed, "{query}, step {step}");
                assert_eq!(kept.more, listed.more, "{query}, step {step}");
                assert_eq!(
                    kept.partial_rooms, listed.partial_rooms,
                    "{query}, step {step}"
                );
                left += listing
                    .listed
                    .iter()
                    .filter(|item| !kept.listed.iter().any(|k| k.key() == item.key()))
                    .count();
                *listing = kept;
            }
            let (kept, badges) = items.rebadged(badged, changed).await.unwrap();
            let (anew, badges_anew) = items.badged().await.unwrap();
            let held = |b: &Badged| b.buckets.iter().map(|b| b.held.clone()).collect::<Vec<_>>();
            assert_eq!(held(&kept), held(&anew), "step {step}");
            assert_eq!(badges, badges_anew, "step {step}");
            badged = kept;
        }
        // Something was counted.
        assert!(badged.badges().marks.home.count > 0);
        assert!(left > 0, "items left windows");
        // An empty window, and more come at once than it holds.
        let listing = window("label:SPAM", 2, anew(), Changed::everything())
            .await
            .unwrap();
        assert!(listing.listed.is_empty());
        let added = (0..3)
            .map(|n| entry(&format!("s{n}"), &format!("s{n}"), &["SPAM"]))
            .collect();
        dispatcher
            .observe("a".into(), observed(None, added, vec![]))
            .await
            .unwrap();
        let mut changed = Changed::default();
        while let Ok(entry) = entries.try_recv() {
            changed.came(&entry);
        }
        let held = uplink_realtime::Held {
            items: listing.listed,
            more: listing.more,
        };
        let kept = window("label:SPAM", 2, held, changed).await.unwrap();
        assert_eq!(kept.listed.len(), 2);
        assert!(kept.more);
    }

    /// Time moves items no entry names: into `due:today`, out of
    /// `newer_than:`. Windows and badges checked again of what time moves
    /// are as listed anew.
    #[tokio::test]
    async fn what_time_moves_is_checked_again() {
        use uplink_email::mailbox::{MailboxObserved, NewEntry, ObservedLabel};
        use uplink_protocol::mail::LabelKind;
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        uplink_email::mailbox::register(&mut registry);
        uplink_todos::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(16)),
        );
        let raw = b"Message-ID: <m@x>\r\nFrom: bob@x\r\nTo: a@x\r\nSubject: s\r\n\r\nhi\r\n";
        let store = uplink_storage::store::Store::mail(dir.path());
        let reader = uplink_storage::open_reader(dir.path()).unwrap();
        dispatcher
            .observe(
                "a".into(),
                MailboxObserved {
                    as_of: uplink_tx_log::as_of(&reader).unwrap(),
                    account_kind: "gmail",
                    labels: Some(vec![ObservedLabel {
                        external_id: "INBOX".into(),
                        name: "INBOX".into(),
                        system: true,
                        kind: LabelKind::Tag,
                    }]),
                    rekeyed: vec![],
                    added: vec![NewEntry {
                        external_id: "m".into(),
                        thread_id: Some("t".into()),
                        received_at: now(),
                        labels: vec!["INBOX".into()],
                        variant: store.put(raw).unwrap(),
                        size_bytes: raw.len() as i64,
                        parsed: uplink_email::parse::parse(raw),
                    }],
                    updated: vec![],
                    removed: vec![],
                    sync_state: None,
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
                "Later".into(),
                String::new(),
                Some("2999-01-01".into()),
                None,
            )
            .await
            .unwrap();
        let items = Items::new(
            vec![uplink_storage::open_reader(dir.path()).unwrap()],
            vec![mail::ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
                identities: vec![],
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }],
            RoomsView::kept(&dispatcher, vec![]).unwrap(),
            vec![],
            NOTIFICATIONS,
            dispatcher.hub().attended(),
        );
        let queries = ["newer_than:1d", "due:today", "newer_than:1d OR due:today"];
        items.configure_home(
            queries
                .iter()
                .map(|query| HomeBucket {
                    name: query.to_string(),
                    query: query.to_string(),
                    badge: Some(Badge::Count),
                })
                .collect(),
            NOTIFICATIONS,
        );
        let window = |query: &str, held, changed| {
            items.window(query.into(), UnknownNames::False, 10, held, changed)
        };
        let anew = || uplink_realtime::Held {
            items: vec![],
            more: false,
        };
        let mut listed = vec![];
        for query in queries {
            listed.push(window(query, anew(), Changed::everything()).await.unwrap());
        }
        let (badged, _) = items.badged().await.unwrap();
        // A day on: the mail is older, the to-do due.
        let writer =
            uplink_storage::rusqlite::Connection::open(dir.path().join("db").join("uplink.sqlite"))
                .unwrap();
        writer
            .execute_batch(&format!(
                "UPDATE mail_entry SET received_at = received_at - 2 * 86400000;
                 UPDATE todo SET due = date({} * 86400, 'unixepoch');",
                uplink_query::today(now())
            ))
            .unwrap();
        for (query, listing) in queries.iter().zip(listed) {
            let held = uplink_realtime::Held {
                items: listing.listed.clone(),
                more: listing.more,
            };
            let kept = window(query, held, Changed::time()).await.unwrap();
            let anew = window(query, anew(), Changed::everything()).await.unwrap();
            assert_ne!(kept.listed, listing.listed, "{query}: time moved it");
            assert_eq!(kept.listed, anew.listed, "{query}");
        }
        let (kept, badges) = items.rebadged(badged, Changed::time()).await.unwrap();
        let (anew, badges_anew) = items.badged().await.unwrap();
        let held = |b: &Badged| b.buckets.iter().map(|b| b.held.clone()).collect::<Vec<_>>();
        assert_eq!(held(&kept), held(&anew));
        assert_eq!(badges, badges_anew);
    }

    /// What a window of the unseen lists is each unseen message the query
    /// matches, as a message: of mail, each entry; of a room, each message,
    /// and its invitation; the newest first. What is seen, or in a place
    /// attended, is not; what came after, is.
    #[tokio::test]
    async fn the_unseen_are_messages_unseen() {
        use uplink_email::mailbox::{MailboxObserved, NewEntry, ObservedLabel};
        use uplink_protocol::mail::LabelKind;
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        uplink_email::mailbox::register(&mut registry);
        uplink_matrix::register(&mut registry);
        crate::seen::register(&mut registry);
        let hub = Arc::new(Hub::new(64));
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            hub.clone(),
        );
        let store = uplink_storage::store::Store::mail(dir.path());
        // Each listed as the log is then.
        let reader = uplink_storage::open_reader(dir.path()).unwrap();
        let observed = |labels, added| MailboxObserved {
            as_of: uplink_tx_log::as_of(&reader).unwrap(),
            account_kind: "gmail",
            labels,
            rekeyed: vec![],
            added,
            updated: vec![],
            removed: vec![],
            sync_state: None,
        };
        let labels = ["INBOX", "UNREAD", "STARRED", "TRASH", "SPAM"]
            .iter()
            .map(|id| ObservedLabel {
                external_id: id.to_string(),
                name: id.to_string(),
                system: true,
                kind: LabelKind::Tag,
            })
            .collect();
        dispatcher
            .observe("a".into(), observed(Some(labels), vec![]))
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
        let mail = |added| dispatcher.observe("a".into(), observed(None, added));
        let message = |id: &str| {
            let raw = serde_json::json!({
                "type": "m.room.message",
                "event_id": id,
                "sender": "@bob:x",
                "origin_server_ts": now(),
                "content": { "msgtype": "m.text", "body": "hi" },
            });
            uplink_matrix::rooms::ObservedEvent {
                push: uplink_matrix::rooms::Push {
                    notify: true,
                    highlight: false,
                },
                ..uplink_matrix::rooms::ObservedEvent::parse(
                    &raw.to_string(),
                    None,
                    uplink_protocol::matrix::Encryption::None,
                )
                .unwrap()
            }
        };
        let matrix = |room| dispatcher.observe("mx".into(), room);
        let as_of = || uplink_tx_log::as_of(&reader).unwrap();
        let items = Items::new(
            vec![uplink_storage::open_reader(dir.path()).unwrap()],
            vec![mail::ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
                identities: vec![],
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }],
            RoomsView::kept(
                &dispatcher,
                vec![uplink_matrix::service::ConfiguredAccount {
                    id: "mx".into(),
                    mxid: "@me:x".into(),
                    history_members: 100,
                }],
            )
            .unwrap(),
            vec![],
            NOTIFICATIONS,
            hub.attended(),
        );
        // The account's first sync, whose invitation is no news.
        matrix(room(as_of(), "!old:x", Membership::Invited, vec![]))
            .await
            .unwrap();
        mail(vec![
            entry("m1", "t", &["INBOX", "UNREAD"]),
            entry("m2", "t", &["INBOX", "UNREAD"]),
            entry("s1", "u", &["INBOX", "UNREAD", "STARRED"]),
        ])
        .await
        .unwrap();
        matrix(room(
            as_of(),
            "!r:x",
            Membership::Joined,
            vec![message("$1")],
        ))
        .await
        .unwrap();
        matrix(room(as_of(), "!i:x", Membership::Invited, vec![]))
            .await
            .unwrap();
        // As home's second bucket has it, after one of what is starred.
        let query = "(is:unread OR is:invite) -has:(is:starred)";
        let unseen = || async {
            let listing = items
                .unseen(query.into(), UnknownNames::False, 10)
                .await
                .unwrap();
            assert!(!listing.more);
            listing
                .listed
                .iter()
                .map(|unseen| match unseen {
                    Unseen::Mail { entry, .. } => entry.subject.clone().unwrap(),
                    Unseen::Message { event, .. } => event.event_id.clone(),
                    Unseen::Invite { name, .. } => name.clone(),
                })
                .collect::<Vec<_>>()
        };
        // The newest first: the invitation, as stored; of t, each entry;
        // not u, which is starred.
        let mut all = unseen().await;
        assert_eq!(all[0], "!i:x");
        all.sort();
        assert_eq!(all, ["!i:x", "$1", "s m1", "s m2"]);
        // Seen in Home, none is; what comes after is.
        dispatcher
            .dispatch(crate::seen::MarkSeen { place: Place::Home })
            .await
            .unwrap();
        assert!(unseen().await.is_empty());
        mail(vec![entry("m3", "t", &["INBOX", "UNREAD"])])
            .await
            .unwrap();
        matrix(room(
            as_of(),
            "!r:x",
            Membership::Joined,
            vec![message("$2")],
        ))
        .await
        .unwrap();
        let mut came = unseen().await;
        came.sort();
        assert_eq!(came, ["$2", "s m3"]);
        // What is in a place attended is seen, while it is.
        let Item::Room { room: r, .. } = items
            .list("is:room is:unread".into(), None, 1)
            .await
            .map(|response| match response {
                ServerResponse::Items { mut items, .. } => items.remove(0),
                other => panic!("{other:?}"),
            })
            .unwrap()
        else {
            panic!()
        };
        hub.attend(1, Some(Place::Room { room: r.id }));
        assert_eq!(unseen().await, ["s m3"]);
        hub.attend(1, Some(Place::Inboxes));
        assert_eq!(unseen().await, ["$2"]);
        hub.attend(1, None);
        assert_eq!(unseen().await.len(), 2);
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
                    packs: None,
                    as_of: uplink_protocol::TxLogId(0),
                    rooms: vec![ObservedRoom {
                        room_id: "!r:x".into(),
                        name: "Rent talk".into(),
                        encrypted: false,
                        membership: Membership::Invited,
                        space: false,
                        direct: false,
                        avatar: None,
                        bridge: None,
                        children: vec![],
                        space_order: None,
                        tags: Some(vec!["u.home".into()]),
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
                        image_packs: vec![],
                        invitation: Some(uplink_matrix::rooms::ObservedInvitation {
                            inviter: "@bob:x".into(),
                            inviter_name: None,
                        }),
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
            RoomsView::kept(
                &dispatcher,
                vec![uplink_matrix::service::ConfiguredAccount {
                    id: "mx".into(),
                    mxid: "@me:x".into(),
                    history_members: 100,
                }],
            )
            .unwrap(),
            vec![],
            NOTIFICATIONS,
            dispatcher.hub().attended(),
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
        items.configure_home(
            vec![
                bucket("Now", "is:invite", Some(Badge::Alert)),
                bucket("Rent", "rent", Some(Badge::Count)),
                bucket("Else", "tag:nowhere OR is:todo", Some(Badge::Dot)),
            ],
            NOTIFICATIONS,
        );
        let ServerResponse::Home {
            buckets,
            notifications,
            notifying,
        } = items.home().unwrap()
        else {
            panic!()
        };
        let mut windows = vec![];
        for bucket in &buckets {
            let window = items
                .window(
                    bucket.query.clone(),
                    UnknownNames::False,
                    10,
                    uplink_realtime::Held {
                        items: vec![],
                        more: false,
                    },
                    Changed::everything(),
                )
                .await
                .unwrap();
            windows.push((bucket.name.as_str(), window));
        }
        let listed: Vec<_> = windows
            .iter()
            .map(|(name_of, w)| (*name_of, w.listed.iter().map(name).collect::<Vec<_>>()))
            .collect();
        assert_eq!(
            listed,
            [
                ("Now", vec!["Rent talk".to_owned()]),
                ("Rent", vec!["Pay rent".to_owned()]),
                ("Else", vec![]),
            ]
        );
        assert_eq!(notifications, NOTIFICATIONS);
        assert_eq!(
            notifying,
            [
                Notifying {
                    urgency: Urgency::Interrupting,
                    query: "((is:invite))".into()
                },
                Notifying {
                    urgency: Urgency::Quiet,
                    query: "((rent) -has:(is:invite))".into()
                },
            ]
        );
        assert_eq!(
            buckets[2].query,
            "(tag:nowhere OR is:todo) -has:(is:invite) -has:(rent)"
        );
        assert!(!windows[0].1.more);
        // As a search's, the name is refused.
        assert_eq!(
            items
                .window(
                    buckets[2].query.clone(),
                    UnknownNames::Invalid,
                    10,
                    uplink_realtime::Held {
                        items: vec![],
                        more: false
                    },
                    Changed::everything()
                )
                .await
                .err()
                .unwrap()
                .message,
            "no label or tag nowhere"
        );

        // The badged ones' marks, each kind apart.
        let Item::Room { room, .. } = &windows[0].1.listed[0] else {
            panic!()
        };
        let room = room.id;
        let alert = Mark {
            alert: 1,
            ..Mark::default()
        };
        let count = Mark {
            count: 1,
            ..Mark::default()
        };
        assert_eq!(
            items.badged().await.unwrap().1,
            Badges {
                // An invitation counts as one.
                marks: Marks {
                    home: Mark {
                        alert: 1,
                        count: 1,
                        dot: false
                    },
                    rooms: vec![(room, alert)],
                    todos: count,
                    ..Marks::default()
                },
            }
        );

        // A dot is of what is unseen: what is new in a place since it was
        // seen, or in Home, which holds every bucket's.
        items.configure_home(
            vec![bucket("Todo", "is:todo", Some(Badge::Dot))],
            NOTIFICATIONS,
        );
        let dots = || async {
            let marks = items.badged().await.unwrap().1.marks;
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
