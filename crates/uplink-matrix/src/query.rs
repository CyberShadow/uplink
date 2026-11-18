//! Rooms and their messages by the query language (SPEC §8.7). A room's
//! own terms (its name, tags, the spaces it is in, whether it is a direct
//! chat or an invitation, its account) are matched as listed, not in SQL,
//! since rooms are few; they decide what they can, and its messages
//! decide the rest. A message has a text and sender, is unread, new, a
//! mention or notifying, and has a time: messages are many, so they are
//! matched in SQL, those after a marker by the index of positions.

use anyhow::Result;
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};
use uplink_protocol::matrix::{
    Invitation, InvitationId, Membership, Room, RoomEvent, RoomEventId, RoomHistory, RoomHits,
    RoomId,
};
use uplink_protocol::TxLogId;
use uplink_query::{text_matches, Fields, Kind, Query, Term};
use uplink_storage::rusqlite::{
    params, params_from_iter, types::Value, OptionalExtension, Transaction,
};

use crate::{
    room_tags,
    room_view::{Listed, RoomsView},
    rooms::{self, after, has_text, having, marked, message, pos_of, Marker, MAIN},
    service::ConfiguredAccount,
};

/// Whether `query` seeks particular messages: by their words, sender or
/// recipient, or time, outside a negation. A query that names only what
/// state a room's messages are in (`is:unread`, `is:new`, `is:mention`,
/// `is:notifying`) finds the room, which opens at them anyway.
fn seeks(query: &Query) -> bool {
    uplink_query::names(query, &|term| {
        matches!(
            term,
            Term::Text { .. } | Term::Me(_) | Term::After(_) | Term::Before(_)
        )
    })
}

/// The rooms of some accounts, as listed, to match queries against: read
/// once for as many queries, as the home view's buckets are.
pub struct Rooms {
    accounts: Arc<Vec<ConfiguredAccount>>,
    as_of: TxLogId,
    all: Arc<Listed>,
    spaces: Spaces,
    /// Of each `has:(…)` matched, the rooms its query matches: a room's
    /// own term then, found once for as many queries as name it (each
    /// bucket of home names those before it).
    has: RefCell<Vec<Has>>,
}

/// Of a `has:(…)`'s query, the rooms it matches, of those it was asked
/// of: all, or some (a log entry's, checked again).
struct Has {
    query: Query,
    of: Option<HashSet<i64>>,
    rooms: HashSet<i64>,
}

impl Has {
    fn of(&self, room: i64) -> bool {
        self.of.as_ref().is_none_or(|of| of.contains(&room))
    }
}

/// The rooms of those `query` is among, if it is: it holds no others,
/// whatever else it says.
fn scope(query: &Query) -> Option<Vec<i64>> {
    let among = |q: &Query| match q {
        Query::Term(Term::Among(among)) => Some(among.rooms.clone()),
        _ => None,
    };
    match query {
        Query::And(all) => all.iter().find_map(among),
        q => among(q),
    }
}

/// A room a query matches: by its own terms, or by its messages, which
/// are then its `hits` if the query seeks particular messages (`seeks`);
/// else, as of its own terms, the room is what was found.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub room: Room,
    pub hits: Option<RoomHits>,
}

impl Group {
    /// When it was last active: its latest hit, else its latest event
    /// (none, of an invitation).
    pub fn active(&self) -> Option<i64> {
        match &self.hits {
            Some(hits) => Some(hits.latest.ts),
            None => self.room.last_event_ts,
        }
    }
}

impl Rooms {
    /// As `view` keeps them, to read with `tx`.
    pub fn of(view: &RoomsView, tx: &Transaction) -> Result<Self> {
        let snapshot = uplink_tx_log::as_of(tx)?;
        let listed = view.listed();
        let spaces = Spaces::of(&listed.rooms);
        Ok(Self {
            accounts: view.accounts().clone(),
            as_of: snapshot.min(listed.as_of),
            all: listed,
            spaces,
            has: RefCell::new(vec![]),
        })
    }

    /// What is read of them and of the snapshot they are read with is as
    /// of this entry, or later (one is kept as entries commit, so may be
    /// of others than the other): to tell readers of entries after it as
    /// of then, which may find some of them read already.
    pub fn as_of(&self) -> TxLogId {
        self.as_of
    }

    /// Finds, of the rooms `of` (all, if not given), those of each
    /// `has:(…)` of `query` not yet found: of those `query` is to be
    /// matched against alone, as a log entry's are, not of every room's
    /// messages.
    fn resolve(&self, tx: &Transaction, query: &Query, of: Option<&[i64]>) -> Result<()> {
        for term in outer_terms(query) {
            let Term::Has(inner) = term else { continue };
            let found = |has: &Has| {
                has.query == **inner
                    && match of {
                        Some(of) => of.iter().all(|room| has.of(*room)),
                        None => has.of.is_none(),
                    }
            };
            if self.has.borrow().iter().any(found) {
                continue;
            }
            let asked = match of {
                Some(rooms) => (**inner).clone().among(uplink_query::Among {
                    rooms: rooms.to_vec(),
                    ..Default::default()
                }),
                None => (**inner).clone(),
            };
            let rooms = self
                .matching(tx, &asked)?
                .into_iter()
                .map(|group| group.room.id.0)
                .collect();
            self.has.borrow_mut().push(Has {
                query: (**inner).clone(),
                of: of.map(|of| of.iter().copied().collect()),
                rooms,
            });
        }
        Ok(())
    }

    /// Those but spaces, which are no views of their own.
    fn listed(&self) -> impl Iterator<Item = &Room> {
        self.all.rooms.iter().filter(|room| !room.space)
    }

    /// Those `query` matches, the most recently active first.
    pub fn matching(&self, tx: &Transaction, query: &Query) -> Result<Vec<Group>> {
        self.resolve(tx, query, scope(query).as_deref())?;
        let (mut groups, undecided) = self.decided(query);
        if let Some(mut search) = self.search(query, &undecided, true) {
            let sql = format!(
                "SELECT e.room, count(*), max(e.pos), e.id {from} GROUP BY e.room",
                from = search.from(),
            );
            // Of `max`, SQLite's other columns are of the row it is of.
            let found: Vec<(i64, u32, i64, i64)> = tx
                .prepare(&sql)?
                .query_map(params_from_iter(search.sql.params.iter()), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<Result<_, _>>()?;
            let seeks = seeks(query);
            for (id, count, _, latest) in found {
                let room = undecided
                    .iter()
                    .find(|r| r.id.0 == id)
                    .expect("messages are found of the rooms searched");
                let hits = if seeks {
                    let latest = rooms::listed_events(tx, RoomId(id), &[RoomEventId(latest)])?
                        .pop()
                        .expect("a message found is listed");
                    Some(RoomHits {
                        count,
                        latest: Box::new(latest),
                    })
                } else {
                    None
                };
                groups.push(Group {
                    room: (*room).clone(),
                    hits,
                });
            }
        }
        groups.sort_by(|a, b| b.active().cmp(&a.active()).then(a.room.id.cmp(&b.room.id)));
        Ok(groups)
    }

    /// Those `query` matches by their own terms, and those whose messages
    /// decide: of those it is among, if it is.
    fn decided(&self, query: &Query) -> (Vec<Group>, Vec<&Room>) {
        let mut groups = vec![];
        let mut undecided = vec![];
        let scope = scope(query);
        let scoped = |room: &&Room| scope.as_ref().is_none_or(|s| s.contains(&room.id.0));
        for room in self.listed().filter(scoped) {
            match uplink_query::decides(query, &|term| self.of_room(room, term)) {
                Some(true) => groups.push(Group {
                    room: room.clone(),
                    hits: None,
                }),
                Some(false) => {}
                None => undecided.push(room),
            }
        }
        (groups, undecided)
    }

    /// Of the rooms whose messages `query` looks through, those of which
    /// not the whole history is stored (SPEC §9.2): it may miss some.
    pub fn partly_searched(&self, tx: &Transaction, query: &Query) -> Result<Vec<RoomId>> {
        self.resolve(tx, query, scope(query).as_deref())?;
        let (_, undecided) = self.decided(query);
        Ok(match self.search(query, &undecided, true) {
            Some(_) => undecided
                .into_iter()
                .filter(|room| room.history != RoomHistory::Whole)
                .map(|room| room.id)
                .collect(),
            None => vec![],
        })
    }

    /// Those invited to.
    pub fn invited(&self) -> impl Iterator<Item = RoomId> + '_ {
        self.listed()
            .filter(|room| room.membership == Membership::Invited)
            .map(|room| room.id)
    }

    /// Every account's rooms, of each account in turn, as
    /// [`rooms::rooms`] lists them.
    pub fn all(&self) -> &[Room] {
        &self.all.rooms
    }

    /// The room listed by this id, if it is.
    pub fn room(&self, id: RoomId) -> Option<&Room> {
        self.listed().find(|room| room.id == id)
    }

    /// The messages of `room` that `query` matches (a room's own search,
    /// SPEC §12.11: its words of their text, not of its name), newest
    /// first: up to `limit`, before `before` if given; and whether there
    /// are more.
    pub fn search_room(
        &self,
        tx: &Transaction,
        room: &Room,
        query: &Query,
        before: Option<RoomEventId>,
        limit: u32,
    ) -> Result<(Vec<RoomEvent>, bool)> {
        self.resolve(tx, query, Some(&[room.id.0]))?;
        let Some(mut search) = self.search(query, &[room], false) else {
            return Ok((vec![], false));
        };
        let from = search.from();
        let before = match before {
            Some(event) => format!("e.pos < {}", pos_of(&search.sql.param(event.0))),
            None => "1".to_owned(),
        };
        let one_more = search.sql.param(limit + 1);
        let mut ids: Vec<RoomEventId> = tx
            .prepare(&format!(
                "SELECT e.id {from} AND {before} ORDER BY e.pos DESC LIMIT {one_more}"
            ))?
            .query_map(params_from_iter(search.sql.params.iter()), |r| {
                Ok(RoomEventId(r.get(0)?))
            })?
            .collect::<Result<_, _>>()?;
        let more = ids.len() > limit as usize;
        ids.truncate(limit as usize);
        let mut events = rooms::listed_events(tx, room.id, &ids)?;
        events.reverse();
        Ok((events, more))
    }

    /// The messages `query` matches, as messages (its rooms' own terms
    /// holding of each), that are unseen (SPEC §12.15): sent once uplink
    /// began to sync their account, and stored after their room was last
    /// seen, by `seen` (Home's) and `rooms` (a room's own, as seen as of
    /// its events). The newest first, up to `limit`, with their rooms;
    /// and whether there are more.
    pub fn unseen(
        &self,
        tx: &Transaction,
        query: &Query,
        seen: RoomEventId,
        rooms: &HashMap<RoomId, RoomEventId>,
        limit: u32,
    ) -> Result<(Vec<(&Room, RoomEvent)>, bool)> {
        // Of the rooms with messages stored since they were seen alone, so
        // that it costs as much however many rooms there are.
        // By the range of ids (`+room`: not by an index of rooms, which
        // walks every message).
        let mut stored =
            tx.prepare("SELECT room, max(id) FROM matrix_event WHERE id > ?1 GROUP BY +room")?;
        let since: Vec<i64> = stored
            .query_map([seen.0], |r| {
                Ok((RoomId(r.get(0)?), RoomEventId(r.get(1)?)))
            })?
            .filter_map(|row| match row {
                Ok((room, latest)) => rooms
                    .get(&room)
                    .is_none_or(|own| latest > *own)
                    .then_some(Ok(room.0)),
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<_, _>>()?;
        let listed: Vec<&Room> = self.listed().filter(|r| since.contains(&r.id.0)).collect();
        if listed.is_empty() {
            return Ok((vec![], false));
        }
        let query = query.clone().among(uplink_query::Among {
            rooms: since.clone(),
            ..Default::default()
        });
        self.resolve(tx, &query, Some(&since))?;
        let Some(mut search) = self.search(&query, &listed, true) else {
            return Ok((vec![], false));
        };
        let from = search.from();
        let seen = search.sql.param(seen.0);
        let rooms_seen: HashMap<String, i64> = rooms
            .iter()
            .map(|(room, event)| (room.0.to_string(), event.0))
            .collect();
        let rooms_seen = search.sql.param(serde_json::to_string(&rooms_seen)?);
        let one_more = search.sql.param(limit + 1);
        let found: Vec<(RoomId, RoomEventId)> = tx
            .prepare(&format!(
                "SELECT e.room, e.id {from}
                   AND e.id > {seen}
                   AND e.id > coalesce(
                     (SELECT s.value FROM json_each({rooms_seen}) s
                      WHERE CAST(s.key AS INTEGER) = e.room), 0)
                   AND e.ts >= (SELECT a.news_since FROM matrix_room n
                                JOIN account a ON a.id = n.account_id WHERE n.id = e.room)
                 ORDER BY e.ts DESC, e.id DESC
                 LIMIT {one_more}"
            ))?
            .query_map(params_from_iter(search.sql.params.iter()), |r| {
                Ok((RoomId(r.get(0)?), RoomEventId(r.get(1)?)))
            })?
            .collect::<Result<_, _>>()?;
        let more = found.len() > limit as usize;
        let mut events: HashMap<RoomEventId, RoomEvent> = HashMap::new();
        let mut by_room: HashMap<RoomId, Vec<RoomEventId>> = HashMap::new();
        for (room, event) in found.iter().take(limit as usize) {
            by_room.entry(*room).or_default().push(*event);
        }
        for (room, ids) in by_room {
            events.extend(
                rooms::listed_events(tx, room, &ids)?
                    .into_iter()
                    .map(|event| (event.id, event)),
            );
        }
        let unseen = found
            .into_iter()
            .take(limit as usize)
            .map(|(room, event)| {
                let room = *listed
                    .iter()
                    .find(|r| r.id == room)
                    .expect("messages are found of the rooms searched");
                let event = events.remove(&event).expect("a message found is listed");
                (room, event)
            })
            .collect();
        Ok((unseen, more))
    }

    /// The invitations of the rooms `query` matches that are unseen (SPEC
    /// §12.15): news, and stored after their room was last seen, by `seen`
    /// (Home's) and `rooms` (a room's own, as seen as of invitations).
    pub fn unseen_invitations(
        &self,
        tx: &Transaction,
        query: &Query,
        seen: InvitationId,
        rooms: &HashMap<RoomId, InvitationId>,
    ) -> Result<Vec<(&Room, Invitation)>> {
        // Of the rooms with invitations stored since they were seen alone.
        let mut stored =
            tx.prepare("SELECT room, id FROM matrix_invitation WHERE news AND id > ?1")?;
        let since: Vec<i64> = stored
            .query_map([seen.0], |r| {
                Ok((RoomId(r.get(0)?), InvitationId(r.get(1)?)))
            })?
            .filter_map(|row| match row {
                Ok((room, id)) => rooms
                    .get(&room)
                    .is_none_or(|own| id > *own)
                    .then_some(Ok(room.0)),
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<_, _>>()?;
        if since.is_empty() {
            return Ok(vec![]);
        }
        let query = query.clone().among(uplink_query::Among {
            rooms: since,
            ..Default::default()
        });
        let invited: HashSet<RoomId> = self
            .matching(tx, &query)?
            .into_iter()
            .filter(|group| group.room.membership == Membership::Invited)
            .map(|group| group.room.id)
            .collect();
        let mut stmt = tx.prepare(
            "SELECT id, inviter, inviter_name, stored_at FROM matrix_invitation
             WHERE room = ?1 AND news AND id > ?2",
        )?;
        let mut unseen = vec![];
        for room in self.listed().filter(|room| invited.contains(&room.id)) {
            let after = rooms.get(&room.id).map_or(seen, |own| (*own).max(seen));
            let invitation = stmt
                .query_row(params![room.id.0, after.0], |r| {
                    Ok(Invitation {
                        id: InvitationId(r.get(0)?),
                        inviter: r.get(1)?,
                        inviter_name: r.get(2)?,
                        stored_at: r.get(3)?,
                    })
                })
                .optional()?;
            unseen.extend(invitation.map(|invitation| (room, invitation)));
        }
        Ok(unseen)
    }

    /// What `query` asks of the messages of `rooms`, those it leaves to
    /// them, words of their rooms' names (`names`) or not; `None` if no
    /// message can match.
    fn search(&self, query: &Query, rooms: &[&Room], names: bool) -> Option<Search> {
        let condition = Cond::of(query, &|term| {
            Cond::term(term, &|term| {
                let of_name = matches!(term, Term::Text { fields: None, .. });
                rooms
                    .iter()
                    .filter(|room| (names || !of_name) && self.of_room(room, term) == Some(true))
                    .map(|room| room.id.0)
                    .collect()
            })
        });
        if condition == Cond::Const(false) || rooms.is_empty() {
            return None;
        }
        Some(Search {
            condition,
            rooms: rooms.iter().map(|room| room.id.0).collect(),
            sql: Sql {
                params: vec![],
                accounts: self
                    .accounts
                    .iter()
                    .map(|a| (a.id.clone(), a.mxid.clone()))
                    .collect(),
            },
        })
    }

    /// What a room's own terms say of it; `None` of a term of its
    /// messages (and of words its name lacks, which they may have).
    /// A `has:(…)` is of the room, as `resolve` found it.
    fn of_room(&self, room: &Room, term: &Term) -> Option<bool> {
        match term {
            Term::Has(query) => Some(
                self.has
                    .borrow()
                    .iter()
                    .find(|has| has.query == **query && has.of(room.id.0))
                    .expect("a query's has:(…) are resolved of a room before it is matched")
                    .rooms
                    .contains(&room.id.0),
            ),
            _ => of_room(room, term, &self.spaces, &self.accounts),
        }
    }
}

/// The terms of `query`, but those within a `has:(…)` of it.
fn outer_terms(query: &Query) -> Vec<&Term> {
    match query {
        Query::And(all) | Query::Or(all) => all.iter().flat_map(outer_terms).collect(),
        Query::Not(q) => outer_terms(q),
        Query::Term(t) => vec![t],
    }
}

/// A tag as a query names it: its name as the user knows it, or as it
/// is, ignoring case, spaces, `_` and `-` (`tag:lowpriority` is
/// `m.lowpriority`, "Low priority").
fn names_tag(name: &str, tag: &str) -> bool {
    let fold = |s: &str| -> String {
        s.chars()
            .filter(|c| !matches!(c, ' ' | '_' | '-'))
            .flat_map(char::to_lowercase)
            .collect()
    };
    let name = fold(name);
    name == fold(tag) || name == fold(room_tags::name(tag))
}

/// Whether some room's tag, or one of Element's, is by this name: else a
/// query naming it names no tag.
pub fn tag_known(tx: &Transaction, name: &str) -> Result<bool> {
    let tags: Vec<String> = tx
        .prepare("SELECT DISTINCT tag FROM matrix_room_tag WHERE desired")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let known = [room_tags::FAVOURITE, room_tags::LOW_PRIORITY]
        .into_iter()
        .chain(tags.iter().map(String::as_str))
        .any(|tag| names_tag(name, tag));
    Ok(known)
}

/// The spaces each room is in, however deeply, by name.
struct Spaces {
    parents: HashMap<String, Vec<String>>,
    names: HashMap<String, String>,
}

impl Spaces {
    fn of(rooms: &[Room]) -> Self {
        let mut parents: HashMap<String, Vec<String>> = HashMap::new();
        let mut names = HashMap::new();
        for space in rooms.iter().filter(|r| r.space) {
            names.insert(space.room_id.clone(), space.name.clone());
            for child in &space.children {
                parents
                    .entry(child.clone())
                    .or_default()
                    .push(space.room_id.clone());
            }
        }
        Self { parents, names }
    }

    /// Whether the room is in a space named `name`, ignoring case.
    fn within(&self, room: &str, name: &str) -> bool {
        let mut seen = HashSet::new();
        let mut next = vec![room];
        while let Some(room) = next.pop() {
            for parent in self.parents.get(room).into_iter().flatten() {
                if !seen.insert(parent) {
                    continue;
                }
                if self.names[parent].eq_ignore_ascii_case(name) {
                    return true;
                }
                next.push(parent);
            }
        }
        false
    }
}

fn of_room(
    room: &Room,
    term: &Term,
    spaces: &Spaces,
    accounts: &[ConfiguredAccount],
) -> Option<bool> {
    match term {
        // Its name, or its messages' text.
        Term::Text {
            fields: None,
            text,
            exact,
        } => text_matches(&room.name, text, *exact).then_some(true),
        Term::Kind(kind) => Some(*kind == Kind::Room),
        Term::Label(name) => Some(room.tags.iter().any(|tag| names_tag(name, tag))),
        Term::Direct => Some(room.direct),
        Term::Invited => Some(room.membership == Membership::Invited),
        Term::Space(name) => Some(spaces.within(&room.room_id, name)),
        Term::Among(among) => Some(among.rooms.contains(&room.id.0)),
        Term::Account(account) => Some(
            accounts
                .iter()
                .any(|a| a.id == room.account && (a.id == *account || a.mxid == *account)),
        ),
        // Of everything, as Trash and Spam are not of rooms.
        Term::Anywhere => Some(true),
        Term::Has(_) => unreachable!("Rooms::of_room decides has:(…)"),
        // Of its messages.
        Term::Text {
            fields: Some(Fields::Body | Fields::From),
            ..
        }
        | Term::Me(Fields::From)
        | Term::Unread
        | Term::New
        | Term::Mention
        | Term::Notifying
        | Term::After(_)
        | Term::Before(_) => None,
        // Of mail, or of other kinds.
        Term::Text {
            fields: Some(Fields::To | Fields::Cc | Fields::Subject),
            ..
        }
        | Term::Me(Fields::To | Fields::Cc | Fields::Subject | Fields::Body)
        | Term::HasAttachment
        | Term::Larger(_)
        | Term::Smaller(_)
        | Term::MessageId(_)
        | Term::Due(_)
        | Term::Overdue
        | Term::HasDue
        | Term::Done => Some(false),
    }
}

/// What a query asks of the messages of some rooms.
struct Search {
    condition: Cond,
    rooms: Vec<i64>,
    sql: Sql,
}

impl Search {
    /// SQL from `FROM` of its messages, `e`: those of the messages it
    /// finds by their ids, if it finds them apart (`apart`), else as one.
    fn from(&mut self) -> String {
        let condition = self.condition.clone();
        match condition.apart() {
            Some(_) => format!(
                "FROM matrix_event e WHERE e.id IN ({})",
                self.ids(&condition)
            ),
            None => self.scan(&condition),
        }
    }

    /// SQL of the ids of the messages `condition` holds of: those of each
    /// of its alternatives, if they are bounded apart.
    fn ids(&mut self, condition: &Cond) -> String {
        match condition.apart() {
            Some(any) => any
                .iter()
                .map(|c| self.ids(c))
                .collect::<Vec<_>>()
                .join(" UNION ALL "),
            None => format!("SELECT e.id {}", self.scan(condition)),
        }
    }

    /// SQL from `FROM` of the messages, `e` of room `r`, `condition`
    /// holds of: room by room, those after a marker by the index of their
    /// positions, if it holds only of those.
    fn scan(&mut self, condition: &Cond) -> String {
        let matches = self.sql.render(condition);
        let after = match condition.markers() {
            None => "1".to_owned(),
            Some(markers) => {
                let positions: Vec<&str> = markers.into_iter().map(Marker::pos).collect();
                match positions.as_slice() {
                    [one] => format!("e.pos > {one}"),
                    _ => format!("e.pos > min({})", positions.join(", ")),
                }
            }
        };
        let rooms = self
            .rooms
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let me = self.sql.me();
        format!(
            "FROM {marked} r CROSS JOIN matrix_event e
             WHERE e.room = r.id AND {after} AND {message} AND {matches}",
            marked = marked(&me, &format!("r.id IN ({rooms})")),
            message = message(),
        )
    }
}

/// What a message must be, folded as built: a term of other kinds is
/// false of it.
#[derive(Debug, Clone, PartialEq)]
enum Cond {
    Const(bool),
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
    /// Its text, or an edit's, has this.
    Text(String),
    /// Its sender's id or name has this.
    Sender(String),
    /// Sent by one of the accounts.
    Mine,
    /// After its room's read marker, of the main timeline, and not the
    /// account's own: as its room counts it unread.
    Unread,
    /// After its room's fully-read marker, and not the account's own.
    New,
    Highlights,
    Notifies,
    /// In one of these rooms.
    Rooms(Vec<i64>),
    After(i64),
    Before(i64),
}

impl Cond {
    /// `term` makes a room term a condition, given the rooms it holds of.
    fn of(query: &Query, term: &impl Fn(&Term) -> Cond) -> Cond {
        match query {
            Query::And(all) => Cond::and(all.iter().map(|q| Cond::of(q, term)).collect()),
            Query::Or(all) => Cond::or(all.iter().map(|q| Cond::of(q, term)).collect()),
            Query::Not(q) => match Cond::of(q, term) {
                Cond::Const(c) => Cond::Const(!c),
                c => Cond::Not(Box::new(c)),
            },
            Query::Term(t) => term(t),
        }
    }

    fn and(all: Vec<Cond>) -> Cond {
        if all.contains(&Cond::Const(false)) {
            return Cond::Const(false);
        }
        let all: Vec<Cond> = all
            .into_iter()
            .filter(|c| *c != Cond::Const(true))
            .collect();
        match all.len() {
            0 => Cond::Const(true),
            1 => all.into_iter().next().expect("one"),
            _ => Cond::And(all),
        }
    }

    fn or(all: Vec<Cond>) -> Cond {
        if all.contains(&Cond::Const(true)) {
            return Cond::Const(true);
        }
        let all: Vec<Cond> = all
            .into_iter()
            .filter(|c| *c != Cond::Const(false))
            .collect();
        match all.len() {
            0 => Cond::Const(false),
            1 => all.into_iter().next().expect("one"),
            _ => Cond::Or(all),
        }
    }

    /// `rooms` lists the rooms a term of rooms holds of.
    fn term(term: &Term, rooms: &impl Fn(&Term) -> Vec<i64>) -> Cond {
        let of_rooms = |term| match rooms(term) {
            ids if ids.is_empty() => Cond::Const(false),
            ids => Cond::Rooms(ids),
        };
        match term {
            // Its room's name, or its text.
            Term::Text {
                fields: None, text, ..
            } => Cond::or(vec![of_rooms(term), Cond::Text(text.clone())]),
            Term::Text {
                fields: Some(Fields::Body),
                text,
                ..
            } => Cond::Text(text.clone()),
            Term::Text {
                fields: Some(Fields::From),
                text,
                ..
            } => Cond::Sender(text.clone()),
            Term::Me(Fields::From) => Cond::Mine,
            // A message is of its room.
            Term::Kind(kind) => Cond::Const(*kind == Kind::Room),
            Term::Unread => Cond::Unread,
            Term::New => Cond::New,
            Term::Mention => Cond::Highlights,
            Term::Notifying => Cond::Notifies,
            Term::After(at) => Cond::After(*at),
            Term::Before(at) => Cond::Before(*at),
            // Of everything, as Trash and Spam are not of messages.
            Term::Anywhere => Cond::Const(true),
            // Of its room.
            Term::Label(_)
            | Term::Account(_)
            | Term::Direct
            | Term::Invited
            | Term::Space(_)
            | Term::Among(_)
            | Term::Has(_) => of_rooms(term),
            // Of mail, or of other kinds.
            Term::Text {
                fields: Some(Fields::To | Fields::Cc | Fields::Subject),
                ..
            }
            | Term::Me(Fields::To | Fields::Cc | Fields::Subject | Fields::Body)
            | Term::HasAttachment
            | Term::Larger(_)
            | Term::Smaller(_)
            | Term::MessageId(_)
            | Term::Due(_)
            | Term::Overdue
            | Term::HasDue
            | Term::Done => Cond::Const(false),
        }
    }

    /// Of alternatives, those bounded otherwise one than another (by
    /// other markers, or some by none): searched as one, all would be
    /// after the least bound of any, so each is searched by its own
    /// (`is:mention is:new` of a room without a fully-read marker, by the
    /// index of its mentions, not of all its messages).
    fn apart(&self) -> Option<&[Cond]> {
        match self {
            Cond::Or(any) if any.iter().any(|c| c.markers() != any[0].markers()) => Some(any),
            _ => None,
        }
    }

    /// The markers a message it holds of is after one of, if any is:
    /// what bounds a search to what is after them.
    fn markers(&self) -> Option<BTreeSet<Marker>> {
        match self {
            Cond::Unread => Some(BTreeSet::from([Marker::Read])),
            Cond::New => Some(BTreeSet::from([Marker::FullyRead])),
            Cond::And(all) => all.iter().find_map(Cond::markers),
            Cond::Or(any) => any.iter().try_fold(BTreeSet::new(), |mut all, c| {
                all.extend(c.markers()?);
                Some(all)
            }),
            _ => None,
        }
    }
}

/// SQL of a condition on a message `e` of room `r`, and its parameters.
struct Sql {
    params: Vec<Value>,
    /// The accounts, and their Matrix ids.
    accounts: Vec<(String, String)>,
}

impl Sql {
    fn param(&mut self, value: impl Into<Value>) -> String {
        self.params.push(value.into());
        format!("?{}", self.params.len())
    }

    /// The accounts' Matrix ids.
    fn mine(&mut self) -> String {
        let mine: Vec<String> = self
            .accounts
            .clone()
            .into_iter()
            .map(|(_, mxid)| self.param(mxid))
            .collect();
        format!("({})", mine.join(", "))
    }

    /// The Matrix id of `r`'s account.
    fn me(&mut self) -> String {
        let cases: Vec<String> = self
            .accounts
            .clone()
            .into_iter()
            .map(|(id, mxid)| format!("WHEN {} THEN {}", self.param(id), self.param(mxid)))
            .collect();
        format!("(CASE r.account_id {} END)", cases.join(" "))
    }

    fn render(&mut self, cond: &Cond) -> String {
        let join = |this: &mut Self, all: &[Cond], op: &str| {
            let all: Vec<String> = all.iter().map(|c| this.render(c)).collect();
            format!("({})", all.join(op))
        };
        match cond {
            Cond::Const(c) => (if *c { "1" } else { "0" }).to_owned(),
            Cond::And(all) => join(self, all, " AND "),
            Cond::Or(all) => join(self, all, " OR "),
            Cond::Not(c) => format!("NOT {}", self.render(c)),
            Cond::Text(text) => {
                let p = self.param(having(text));
                has_text(&p)
            }
            Cond::Sender(text) => {
                let p = self.param(having(text));
                format!("(e.sender LIKE {p} ESCAPE '\\' OR e.sender_name LIKE {p} ESCAPE '\\')")
            }
            Cond::Mine => format!("e.sender IN {}", self.mine()),
            Cond::Unread => {
                let me = self.me();
                format!("({} AND {MAIN})", after(Marker::Read, &me))
            }
            Cond::New => {
                let me = self.me();
                after(Marker::FullyRead, &me)
            }
            Cond::Highlights => "e.highlight".to_owned(),
            Cond::Notifies => "e.notify".to_owned(),
            Cond::Rooms(ids) => format!(
                "e.room IN ({})",
                ids.iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Cond::After(at) => format!("e.ts >= {at}"),
            Cond::Before(at) => format!("e.ts < {at}"),
        }
    }
}
