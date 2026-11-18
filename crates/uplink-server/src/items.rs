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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
};
use uplink_config::HomeBucket;
use uplink_email::query::{self as mail, ListError, Unknown};
use uplink_matrix::{
    query::{Group, Rooms},
    room_view::RoomsView,
};
use uplink_protocol::{
    calendar::CalendarEvent,
    mail::{EntryId, ListPosition, ListScope},
    matrix::{Membership, RoomEventId, RoomId},
    todos::Todo,
    Arrival, Arrived, Badge, Badges, Changed, ErrorCode, Item, ItemKind, ItemPosition, Mark, Marks,
    Place, ServerError, ServerResponse, TxLogId, UnknownNames, Urgency,
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

/// The badges, what came into the badged buckets since they were last
/// derived, what told of before is no news any more, and as of when
/// they were derived now.
pub struct Derived {
    pub badges: Badges,
    pub arrivals: Vec<Arrival>,
    pub recalls: Vec<Arrived>,
    pub since: Since,
}

/// As of when the badges were derived: what came after is news. An
/// invitation, which has no message, is news if its room was not invited
/// to then. And what was told of that is news still, to recall once it
/// is not (SPEC §12.16).
#[derive(Debug, Clone)]
pub struct Since {
    watermark: Watermark,
    invited: BTreeSet<RoomId>,
    standing: Standing,
}

/// What told of stands, one per conversation or room, as the client's
/// notifications are (`tag`): a later arrival of one replaces it.
type Standing = HashMap<Notified, Arrived>;

/// What an arrival is of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Notified {
    /// By its key, as `Held` has it.
    Conversation(String),
    Room(RoomId),
}

#[derive(Clone)]
pub struct Items {
    reader: Reader,
    mail_accounts: Vec<mail::ConfiguredAccount>,
    rooms: Arc<RoomsView>,
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

/// The keys of the conversations of the entries `changed` names.
fn conversations(tx: &Transaction, changed: &Changed) -> Result<HashSet<String>, ServerError> {
    Ok(mail::conversation_keys(tx, &changed.entries)
        .map_err(internal)?
        .into_iter()
        .collect())
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

    /// What was derived, of what the buckets hold now, with what came
    /// (`arrivals`), and what stood before.
    fn derived(
        &self,
        tx: &Transaction,
        rooms: &Rooms,
        arrivals: Vec<Arrival>,
        standing: Standing,
    ) -> Result<Derived, ServerError> {
        let (standing, recalls) = self.stand(tx, rooms, standing, &arrivals)?;
        Ok(Derived {
            badges: self.badges(),
            arrivals,
            recalls,
            since: Since {
                watermark: crate::seen::latest(tx).map_err(internal)?,
                invited: rooms.invited().collect(),
                standing,
            },
        })
    }

    /// Whether a bucket that notifies holds it.
    fn notifies(&self, notified: &Notified) -> bool {
        self.buckets
            .iter()
            .filter(|bucket| bucket.badge.urgency().is_some())
            .any(|bucket| match notified {
                Notified::Conversation(key) => bucket.held.mail.contains_key(key),
                Notified::Room(room) => bucket.held.rooms.contains_key(room),
            })
    }

    /// What stands of `standing` and `arrivals`: each arrival, replacing
    /// what stood of its conversation or room; and what stood that is
    /// news still: a bucket that notifies holds its conversation or room,
    /// and what it told of is not deleted, nor the invitation answered.
    /// And what stood that is not, to recall.
    fn stand(
        &self,
        tx: &Transaction,
        rooms: &Rooms,
        standing: Standing,
        arrivals: &[Arrival],
    ) -> Result<(Standing, Vec<Arrived>), ServerError> {
        let mut told = Standing::new();
        for arrival in arrivals {
            told.insert(notified(tx, &arrival.item)?, arrival.item.clone());
        }
        let mut recalls = vec![];
        for (notified, arrived) in standing {
            if told.contains_key(&notified) {
                continue;
            }
            match self.notifies(&notified) && kept(tx, rooms, &arrived)? {
                true => {
                    told.insert(notified, arrived);
                }
                false => recalls.push(arrived),
            }
        }
        Ok((told, recalls))
    }
}

/// What `arrived` is of.
fn notified(tx: &Transaction, arrived: &Arrived) -> Result<Notified, ServerError> {
    Ok(match arrived {
        Arrived::Mail { entry } => {
            let keys = mail::conversation_keys(tx, &[*entry]).map_err(internal)?;
            let [key] = <[String; 1]>::try_from(keys).expect("what came is stored");
            Notified::Conversation(key)
        }
        Arrived::Message { room, .. } | Arrived::Invite { room, .. } => Notified::Room(*room),
    })
}

/// Whether what `arrived` told of is not deleted, nor the invitation
/// answered.
fn kept(tx: &Transaction, rooms: &Rooms, arrived: &Arrived) -> Result<bool, ServerError> {
    Ok(match arrived {
        Arrived::Mail { entry } => mail::kept(tx, *entry).map_err(internal)?,
        Arrived::Message { event, .. } => {
            uplink_matrix::query::kept(tx, *event).map_err(internal)?
        }
        Arrived::Invite { room, .. } => rooms.invited().any(|invited| invited == *room),
    })
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
    /// many, and long. `rooms` are the configured Matrix accounts' rooms,
    /// as the writer keeps them.
    pub fn new(
        readers: Vec<Connection>,
        mail_accounts: Vec<mail::ConfiguredAccount>,
        rooms: Arc<RoomsView>,
        home: Vec<HomeBucket>,
    ) -> Self {
        Self {
            reader: Reader::pool(readers),
            mail_accounts,
            rooms,
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
    ) -> Result<Listing, ServerError> {
        if changed.everything {
            let (as_of, page, partial_rooms) = self.read_page(query, unknown, None, limit).await?;
            return Ok(Listing {
                as_of,
                items: page.items,
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
                    items,
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
    /// And what came into those that notify, from others, since `since`
    /// (SPEC §12.16), of the `ARRIVALS_AMONG` most recently active items
    /// of each: nothing, at first.
    pub async fn badged(&self, since: Option<Since>) -> Result<(Badged, Derived), ServerError> {
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
                let rooms = Rooms::of(&this.rooms, &tx).map_err(internal)?;
                let seen = Seen::load(&tx).map_err(internal)?;
                let mut badged = Badged { buckets: vec![] };
                let mut arrivals = vec![];
                for (badge, bucket) in buckets {
                    let held = this.hold(&tx, &rooms, &seen, badge, &bucket.query, today, now)?;
                    if let (Some(since), Some(urgency)) = (&since, badge.urgency()) {
                        let others = others(&tx, &rooms, &bucket.query, today, now)?;
                        let page = this.page(&tx, &bucket.query, others, None, ARRIVALS_AMONG)?;
                        arrivals.extend(this.arrivals(
                            &tx,
                            &rooms,
                            &bucket.name,
                            &bucket.query,
                            urgency,
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
                let standing = since.map(|since| since.standing).unwrap_or_default();
                let derived = badged.derived(&tx, &rooms, arrivals, standing)?;
                Ok((badged, derived))
            })
            .await
    }

    /// `badged` kept: what `changed` names checked again, and of it what
    /// came, into those that notify, since `since`. What the log does not name item by item,
    /// everything is derived anew for.
    pub async fn rebadged(
        &self,
        badged: Badged,
        changed: Changed,
        since: Since,
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
                let rooms = Rooms::of(&this.rooms, &tx).map_err(internal)?;
                let seen = Seen::load(&tx).map_err(internal)?;
                let conversations = conversations(&tx, &changed)?;
                let among = among(&changed);
                let mut badged = badged;
                let mut arrivals = vec![];
                for bucket in &mut badged.buckets {
                    let hold = |query: Query| {
                        this.hold(&tx, &rooms, &seen, bucket.badge, &query, today, now)
                    };
                    let of_kind = |kind| bucket.query.clone().and(Query::Term(Term::Kind(kind)));
                    let fresh = Fresh {
                        among: hold(bucket.query.clone().among(among.clone()))?,
                        whole: Whole::of(&bucket.query, &changed)
                            .kinds()
                            .map(|kind| Ok((kind, hold(of_kind(kind))?)))
                            .collect::<Result<_, ServerError>>()?,
                    };
                    bucket.held.update(&conversations, &among, fresh);
                    // What came is of what was stored since: what the log
                    // named.
                    let came = uplink_query::Among {
                        entries: among.entries.clone(),
                        rooms: among.rooms.clone(),
                        ..Default::default()
                    };
                    let limit = (came.entries.len() + came.rooms.len()) as u32;
                    if let (true, Some(urgency)) = (limit > 0, bucket.badge.urgency()) {
                        let query = bucket.query.clone().among(came);
                        let others = others(&tx, &rooms, &query, today, now)?;
                        let page = this.page(&tx, &query, others, None, limit)?;
                        arrivals.extend(this.arrivals(
                            &tx,
                            &rooms,
                            &bucket.name,
                            &bucket.query,
                            urgency,
                            &page.items,
                            &since,
                            now,
                        )?);
                    }
                }
                let derived = badged.derived(&tx, &rooms, arrivals, since.standing)?;
                Ok((badged, derived))
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

    /// Of `items`, those of the bucket of `query`, what came from others
    /// since `since`, and recently: what an item last active before then
    /// cannot have, but an invitation, which has no message.
    #[allow(clippy::too_many_arguments)]
    fn arrivals(
        &self,
        tx: &Transaction,
        rooms: &Rooms,
        bucket: &str,
        query: &Query,
        urgency: Urgency,
        items: &[Item],
        since: &Since,
        now: i64,
    ) -> Result<Vec<Arrival>, ServerError> {
        let after = now - RECENT;
        let recent: Vec<Item> = items
            .iter()
            .filter(|item| match item {
                Item::Room { room, .. } if room.membership == Membership::Invited => true,
                _ => item.position().at >= after,
            })
            .cloned()
            .collect();
        Ok(self
            .arrived(tx, rooms, query, &recent, since, after)?
            .into_iter()
            .map(|item| Arrival {
                bucket: bucket.to_owned(),
                urgency,
                item,
            })
            .collect())
    }

    /// What came from others into the bucket of `query` since `since`,
    /// sent or received after `after`, of its `items`: of each conversation
    /// its newest entry, of each room its newest message the query matches,
    /// and invitations; to-dos and events are the user's own.
    fn arrived(
        &self,
        tx: &Transaction,
        rooms: &Rooms,
        query: &Query,
        items: &[Item],
        since: &Since,
        after: i64,
    ) -> Result<Vec<Arrived>, ServerError> {
        let mut arrived = vec![];
        let mut joined = vec![];
        for item in items {
            match item {
                Item::Mail { conversation } => {
                    let since = EntryId(since.watermark.mail);
                    let entry = mail::arrived(tx, conversation, since, after).map_err(internal)?;
                    arrived.extend(entry.map(|entry| Arrived::Mail { entry }));
                }
                Item::Room { room, .. } => match room.membership {
                    Membership::Invited if !since.invited.contains(&room.id) => {
                        arrived.push(Arrived::Invite {
                            room: room.id,
                            name: room.name.clone(),
                        })
                    }
                    Membership::Invited => {}
                    Membership::Joined | Membership::Left => joined.push(room.id),
                },
                Item::Todo { .. } | Item::Event { .. } => {}
            }
        }
        let since = RoomEventId(since.watermark.room_event);
        let messages = rooms
            .arrived(tx, query, &joined, since, after)
            .map_err(internal)?;
        arrived.extend(
            messages
                .into_iter()
                .map(|(room, event)| Arrived::Message { room, event }),
        );
        Ok(arrived)
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
    use uplink_protocol::{matrix::Membership, Urgency};
    use uplink_realtime::Hub;
    use uplink_tx_log::{Dispatcher, Registry};

    /// What a sync requested after every entry tells of: current.
    const FRESH: uplink_protocol::TxLogId = uplink_protocol::TxLogId(i64::MAX);

    /// A room for the account `mx`, joined, of these events.
    fn room(
        room_id: &str,
        membership: Membership,
        events: Vec<uplink_matrix::rooms::ObservedEvent>,
    ) -> Synced {
        Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
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
                image_packs: vec![],
            }],
        }
    }

    /// Kept by checking again what each log entry names, the badges are
    /// as derived anew, what each bucket holds too, and what came; and
    /// windows are as listed anew.
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
        );
        let bucket = |name: &str, query: &str, badge| HomeBucket {
            name: name.into(),
            query: query.into(),
            badge: Some(badge),
        };
        items.configure_home(vec![
            bucket(
                "Urgent",
                "is:starred OR is:mention OR is:invite",
                Badge::Alert,
            ),
            bucket("Unread", "is:unread", Badge::Dot),
            bucket("Later", "is:todo OR label:Label_1", Badge::Count),
        ]);
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
        let (mut badged, first) = items.badged(None).await.unwrap();
        let mut since = first.since;
        let mut todo_ids = vec![];
        let mut came = 0;
        let mut invited = 0;
        let mut recalled = 0;
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
                        let invite = room(&format!("!i{step}:x"), Membership::Invited, vec![]);
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
            let mut changed = Changed::default();
            while let Ok(entry) = entries.try_recv() {
                changed.came(&entry);
            }
            assert!(!changed.everything, "step {step}");
            for (query, limit, listing) in &mut windows {
                let held = uplink_realtime::Held {
                    items: listing.items.clone(),
                    more: listing.more,
                };
                let kept = window(query, *limit, held, changed.clone()).await.unwrap();
                let listed = window(query, *limit, anew(), Changed::everything())
                    .await
                    .unwrap();
                let keys = |l: &Listing| l.items.iter().map(Item::key).collect::<Vec<_>>();
                assert_eq!(keys(&kept), keys(&listed), "{query}, step {step}");
                assert_eq!(kept.items, listed.items, "{query}, step {step}");
                assert_eq!(kept.more, listed.more, "{query}, step {step}");
                assert_eq!(
                    kept.partial_rooms, listed.partial_rooms,
                    "{query}, step {step}"
                );
                left += listing
                    .items
                    .iter()
                    .filter(|item| !kept.items.iter().any(|k| k.key() == item.key()))
                    .count();
                *listing = kept;
            }
            let (kept, derived) = items
                .rebadged(badged, changed, since.clone())
                .await
                .unwrap();
            let (anew, derived_anew) = items.badged(Some(since.clone())).await.unwrap();
            let held = |b: &Badged| {
                b.buckets
                    .iter()
                    .map(|b| (b.name.clone(), b.held.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(held(&kept), held(&anew), "step {step}");
            assert_eq!(derived.badges, derived_anew.badges, "step {step}");
            assert_eq!(derived.arrivals, derived_anew.arrivals, "step {step}");
            let sorted = |mut recalls: Vec<Arrived>| {
                recalls.sort_by_key(|item| format!("{item:?}"));
                recalls
            };
            recalled += derived.recalls.len();
            assert_eq!(
                sorted(derived.recalls),
                sorted(derived_anew.recalls),
                "step {step}"
            );
            came += derived.arrivals.len();
            invited += derived
                .arrivals
                .iter()
                .filter(|a| matches!(a.item, Arrived::Invite { .. }))
                .count();
            badged = kept;
            since = derived.since;
        }
        // Something was counted, and something came.
        assert!(badged.badges().marks.home.count > 0);
        assert!(came > 0);
        assert!(invited > 0, "invitations came");
        assert!(recalled > 0, "what came was recalled");
        assert!(left > 0, "items left windows");
        // An empty window, and more come at once than it holds.
        let listing = window("label:SPAM", 2, anew(), Changed::everything())
            .await
            .unwrap();
        assert!(listing.items.is_empty());
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
            items: listing.items,
            more: listing.more,
        };
        let kept = window("label:SPAM", 2, held, changed).await.unwrap();
        assert_eq!(kept.items.len(), 2);
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
        dispatcher
            .observe(
                "a".into(),
                MailboxObserved {
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
        let (badged, first) = items.badged(None).await.unwrap();
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
                items: listing.items.clone(),
                more: listing.more,
            };
            let kept = window(query, held, Changed::time()).await.unwrap();
            let anew = window(query, anew(), Changed::everything()).await.unwrap();
            assert_ne!(kept.items, listing.items, "{query}: time moved it");
            assert_eq!(kept.items, anew.items, "{query}");
        }
        let (kept, derived) = items
            .rebadged(badged, Changed::time(), first.since.clone())
            .await
            .unwrap();
        let (anew, derived_anew) = items.badged(Some(first.since)).await.unwrap();
        let held = |b: &Badged| {
            b.buckets
                .iter()
                .map(|b| (b.name.clone(), b.held.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(held(&kept), held(&anew));
        assert_eq!(derived.badges, derived_anew.badges);
    }

    /// What came into a badged bucket since it was last derived is told
    /// of, by the bucket: a message the bucket's query matches, an
    /// invitation; of an unbadged one, nothing.
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
        let message = |id: &str, notify| {
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
                    notify,
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
        let observe = |room| dispatcher.observe("mx".into(), room);
        observe(room("!r:x", Membership::Joined, vec![message("$1", true)]))
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
        );
        let bucket = |query: &str, badge| HomeBucket {
            name: query.into(),
            query: query.into(),
            badge,
        };
        let (_, first) = items.badged(None).await.unwrap();
        assert!(first.arrivals.is_empty(), "nothing came before the first");
        // A message that does not notify, and an invitation.
        observe(room("!r:x", Membership::Joined, vec![message("$2", false)]))
            .await
            .unwrap();
        observe(room("!i:x", Membership::Invited, vec![]))
            .await
            .unwrap();
        let arrivals = |buckets, since: &Since| {
            items.configure_home(buckets);
            let items = items.clone();
            let since = since.clone();
            async move {
                let (_, derived) = items.badged(Some(since)).await.unwrap();
                let arrived: Vec<_> = derived
                    .arrivals
                    .into_iter()
                    .map(|a| {
                        let item = match a.item {
                            Arrived::Message { .. } => "message",
                            Arrived::Invite { .. } => "invite",
                            Arrived::Mail { .. } => "mail",
                        };
                        (a.bucket, a.urgency, item)
                    })
                    .collect();
                (arrived, derived.since)
            }
        };
        let (arrived, _) = arrivals(
            vec![
                bucket("is:invite", Some(Badge::Alert)),
                bucket("is:unread", Some(Badge::Count)),
            ],
            &first.since,
        )
        .await;
        assert_eq!(
            arrived,
            [
                ("is:invite".into(), Urgency::Interrupting, "invite"),
                ("is:unread".into(), Urgency::Quiet, "message"),
            ]
        );
        // The message is no news to a bucket of what notifies; nothing is
        // to a bucket with a dot, or none.
        let (arrived, _) = arrivals(
            vec![bucket("is:unread is:notifying", Some(Badge::Count))],
            &first.since,
        )
        .await;
        assert!(arrived.is_empty(), "{arrived:?}");
        let (arrived, _) =
            arrivals(vec![bucket("is:unread", Some(Badge::Dot))], &first.since).await;
        assert!(arrived.is_empty(), "{arrived:?}");
        let (arrived, now) = arrivals(vec![bucket("is:invite", None)], &first.since).await;
        assert!(arrived.is_empty(), "{arrived:?}");
        // Once told of, the invitation is not news again.
        let (arrived, _) = arrivals(vec![bucket("is:invite", Some(Badge::Alert))], &now).await;
        assert!(arrived.is_empty(), "{arrived:?}");
    }

    /// What was told of is recalled once it is no news: its conversation
    /// or room no longer in a bucket that notifies (read elsewhere), or
    /// what it told of deleted (mail trashed, a message redacted, though
    /// what else came keeps the item in the bucket), or the invitation
    /// answered; not before, nor once a later arrival replaces it.
    #[tokio::test]
    async fn recalls_are_of_what_is_no_news() {
        use uplink_email::mailbox::{EntryLabels, MailboxObserved, NewEntry, ObservedLabel};
        use uplink_protocol::mail::LabelKind;
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        uplink_email::mailbox::register(&mut registry);
        uplink_matrix::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
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
        let labels = ["INBOX", "UNREAD", "TRASH", "SPAM"]
            .iter()
            .map(|id| ObservedLabel {
                external_id: id.to_string(),
                name: id.to_string(),
                system: true,
                kind: LabelKind::Tag,
            })
            .collect();
        let mail = |added, updated| dispatcher.observe("a".into(), observed(None, added, updated));
        dispatcher
            .observe("a".into(), observed(Some(labels), vec![], vec![]))
            .await
            .unwrap();
        let entry = |id: &str, thread: &str| {
            let raw = format!(
                "Message-ID: <{id}@x>\r\nFrom: bob@x\r\nTo: a@x\r\nSubject: s {id}\r\n\r\nhi\r\n"
            );
            let raw = raw.as_bytes();
            NewEntry {
                external_id: id.to_owned(),
                thread_id: Some(thread.to_owned()),
                received_at: now(),
                labels: vec!["INBOX".into(), "UNREAD".into()],
                variant: store.put(raw).unwrap(),
                size_bytes: raw.len() as i64,
                parsed: uplink_email::parse::parse(raw),
            }
        };
        let labelled = |id: &str, labels: &[&str]| EntryLabels {
            external_id: id.into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
        };
        // Bob's, now, and notifying.
        let event = |mut raw: serde_json::Value| {
            raw["sender"] = "@bob:x".into();
            raw["origin_server_ts"] = now().into();
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
        let message = |id: &str| {
            event(serde_json::json!({
                "type": "m.room.message",
                "event_id": id,
                "content": { "msgtype": "m.text", "body": "hi" },
            }))
        };
        let matrix = |room| dispatcher.observe("mx".into(), room);
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
        );
        items.configure_home(vec![
            HomeBucket {
                name: "Urgent".into(),
                query: "is:unread OR is:invite".into(),
                badge: Some(Badge::Alert),
            },
            HomeBucket {
                name: "Unread".into(),
                query: "label:INBOX".into(),
                badge: Some(Badge::Dot),
            },
        ]);
        let (_, first) = items.badged(None).await.unwrap();
        let mut since = first.since;
        // Derived anew and kept agree on what is recalled.
        let mut derive = async || {
            let (badged, derived) = items.badged(Some(since.clone())).await.unwrap();
            let (_, kept) = items
                .rebadged(badged, Changed::default(), since.clone())
                .await
                .unwrap();
            assert_eq!(kept.recalls, derived.recalls);
            since = derived.since;
            let mut recalls = derived.recalls;
            recalls.sort_by_key(|item| format!("{item:?}"));
            (derived.arrivals, recalls)
        };
        mail(
            vec![entry("m1", "t"), entry("m2", "t"), entry("n1", "u")],
            vec![],
        )
        .await
        .unwrap();
        // First, so that the messages after are news: what the account's
        // first sync brings is history.
        matrix(room("!i:x", Membership::Invited, vec![]))
            .await
            .unwrap();
        matrix(room(
            "!r:x",
            Membership::Joined,
            vec![message("$0"), message("$1")],
        ))
        .await
        .unwrap();
        matrix(room("!s:x", Membership::Joined, vec![message("$2")]))
            .await
            .unwrap();
        let (arrivals, recalls) = derive().await;
        assert_eq!(arrivals.len(), 5, "{arrivals:?}");
        assert!(recalls.is_empty(), "{recalls:?}");
        // In the order stored: of t, m2 (m1 is earlier); u's n1; of !r,
        // $1 ($0 is earlier); !s's $2; the invitation.
        let mut arrivals: Vec<Arrived> = arrivals.into_iter().map(|a| a.item).collect();
        arrivals.sort_by_key(|item| match item {
            Arrived::Mail { entry } => (0, entry.0),
            Arrived::Message { event, .. } => (1, event.0),
            Arrived::Invite { room, .. } => (2, room.0),
        });
        let [m2, n1, r1, s2, invite] = <[Arrived; 5]>::try_from(arrivals).unwrap();
        assert!(matches!(invite, Arrived::Invite { .. }), "{invite:?}");

        // m2 (of t, the one told of) trashed, t's m1 unread still; n1
        // stands. $1 redacted, $0 unread still; !s read; !i joined.
        mail(vec![], vec![labelled("m2", &["TRASH", "UNREAD"])])
            .await
            .unwrap();
        let redaction = event(serde_json::json!({
            "type": "m.room.redaction",
            "event_id": "$x",
            "redacts": "$1",
            "content": { "redacts": "$1" },
        }));
        matrix(room("!r:x", Membership::Joined, vec![redaction]))
            .await
            .unwrap();
        let mut read = room("!s:x", Membership::Joined, vec![]);
        read.rooms[0].own_receipts = vec!["$2".into()];
        matrix(read).await.unwrap();
        matrix(room("!i:x", Membership::Joined, vec![]))
            .await
            .unwrap();
        let (arrivals, recalls) = derive().await;
        assert!(arrivals.is_empty(), "{arrivals:?}");
        let mut expected = vec![m2, r1, s2, invite];
        expected.sort_by_key(|item| format!("{item:?}"));
        assert_eq!(recalls, expected);
        // n1 stands, and nothing more is recalled while nothing changes.
        let (_, recalls) = derive().await;
        assert!(recalls.is_empty(), "{recalls:?}");

        // A later arrival of u replaces n1's: n1 is not recalled, nor
        // recalled after, but n2 is, once u is read.
        mail(vec![entry("n2", "u")], vec![]).await.unwrap();
        let (arrivals, recalls) = derive().await;
        let [n2] = <[Arrival; 1]>::try_from(arrivals).unwrap();
        assert_ne!(n2.item, n1);
        assert!(recalls.is_empty(), "{recalls:?}");
        mail(
            vec![],
            vec![labelled("n1", &["INBOX"]), labelled("n2", &["INBOX"])],
        )
        .await
        .unwrap();
        let (_, recalls) = derive().await;
        assert_eq!(recalls, [n2.item]);
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
                    requested: FRESH,
                    next_batch: "next".into(),
                    packs: None,
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
                        image_packs: vec![],
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
            bucket("Now", "is:invite", Some(Badge::Alert)),
            bucket("Rent", "rent", Some(Badge::Count)),
            bucket("Else", "tag:nowhere OR is:todo", Some(Badge::Dot)),
        ]);
        let ServerResponse::Home { buckets } = items.home().unwrap() else {
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
        let Item::Room { room, .. } = &windows[0].1.items[0] else {
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
            items.badged(None).await.unwrap().1.badges,
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
