//! Rooms and their messages by the query language (SPEC §8.7). A room's
//! own terms (its name, tags, the spaces it is in, whether it is a direct
//! chat or an invitation, its account) are matched as listed, not in SQL,
//! since rooms are few; they decide what they can, and its messages
//! decide the rest. A message has a text and sender, is unread, new, a
//! mention or notifying, and has a time: messages are many, so they are
//! matched in SQL, those after a marker by the index of positions.

use anyhow::Result;
use std::collections::{BTreeSet, HashMap, HashSet};
use uplink_protocol::matrix::{
    Membership, Room, RoomEvent, RoomEventId, RoomHistory, RoomHits, RoomId,
};
use uplink_query::{text_matches, Fields, Kind, Query, Term};
use uplink_storage::rusqlite::{params_from_iter, types::Value, Transaction};

use crate::{
    room_tags,
    rooms::{self, after, has_text, having, message, pos_of, Marker, MAIN},
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
pub struct Rooms<'a> {
    accounts: &'a [ConfiguredAccount],
    all: Vec<Room>,
    spaces: Spaces,
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

impl<'a> Rooms<'a> {
    pub fn load(tx: &Transaction, accounts: &'a [ConfiguredAccount]) -> Result<Self> {
        let mut all = vec![];
        for account in accounts {
            all.extend(rooms::rooms(tx, account)?);
        }
        let spaces = Spaces::of(&all);
        Ok(Self {
            accounts,
            all,
            spaces,
        })
    }

    /// Those but spaces, which are no views of their own.
    fn listed(&self) -> impl Iterator<Item = &Room> {
        self.all.iter().filter(|room| !room.space)
    }

    /// Those `query` matches, the most recently active first.
    pub fn matching(&self, tx: &Transaction, query: &Query) -> Result<Vec<Group>> {
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
    /// decide.
    fn decided(&self, query: &Query) -> (Vec<Group>, Vec<&Room>) {
        let mut groups = vec![];
        let mut undecided = vec![];
        for room in self.listed() {
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
    pub fn partly_searched(&self, query: &Query) -> Vec<RoomId> {
        let (_, undecided) = self.decided(query);
        match self.search(query, &undecided, true) {
            Some(_) => undecided
                .into_iter()
                .filter(|room| room.history != RoomHistory::Whole)
                .map(|room| room.id)
                .collect(),
            None => vec![],
        }
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
    fn of_room(&self, room: &Room, term: &Term) -> Option<bool> {
        of_room(room, term, &self.spaces, self.accounts)
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
        Term::Account(account) => Some(
            accounts
                .iter()
                .any(|a| a.id == room.account && (a.id == *account || a.mxid == *account)),
        ),
        // Of everything, as Trash and Spam are not of rooms.
        Term::Anywhere => Some(true),
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
    /// SQL from `FROM` of its messages, `e` of room `r`: room by room,
    /// those after a marker by the index of their positions, if the
    /// condition holds only of those.
    fn from(&mut self) -> String {
        let matches = self.sql.render(&self.condition);
        let after = match self.condition.markers() {
            None => "1".to_owned(),
            Some(markers) => {
                let positions: Vec<String> = markers.into_iter().map(Marker::pos).collect();
                match positions.as_slice() {
                    [one] => format!("e.pos > {one}"),
                    _ => format!("e.pos > min({})", positions.join(", ")),
                }
            }
        };
        format!(
            "FROM matrix_room r CROSS JOIN matrix_event e
             WHERE r.id IN ({rooms}) AND e.room = r.id AND {after} AND {message} AND {matches}",
            rooms = self
                .rooms
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", "),
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
            Term::Label(_) | Term::Account(_) | Term::Direct | Term::Invited | Term::Space(_) => {
                of_rooms(term)
            }
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
