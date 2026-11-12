//! Rooms and their messages by the query language (SPEC §8.7). A room
//! matches by its name, what is unread in it, its tags, the spaces it is
//! in, and its latest activity: rooms are few, so they are matched as
//! listed, not in SQL. A message matches by its text and sender, whether
//! it is unread, mentions or notifies, its time, and its room's terms:
//! messages are many, so they are matched in SQL.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use uplink_protocol::matrix::{FoundMessage, Membership, Room, RoomEventId, RoomId};
use uplink_query::{text_matches, Fields, Kind, Query, Term};
use uplink_storage::rusqlite::{params_from_iter, types::Value, Transaction};

use crate::{
    room_tags,
    rooms::{self, has_text, having, pos_of, LISTED},
    service::ConfiguredAccount,
};

/// The rooms of some accounts, as listed, to match queries against: read
/// once for as many queries, as the home view's buckets are.
pub struct Rooms<'a> {
    accounts: &'a [ConfiguredAccount],
    all: Vec<Room>,
    spaces: Spaces,
}

impl<'a> Rooms<'a> {
    pub fn load(tx: &Transaction, accounts: &'a [ConfiguredAccount]) -> Result<Self> {
        let mut all = vec![];
        for account in accounts {
            all.extend(rooms::rooms(tx, &account.id, &account.mxid)?);
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

    /// Those `query` matches, most recently active first.
    pub fn matching(&self, query: &Query) -> Vec<Room> {
        let mut matched: Vec<Room> = self
            .listed()
            .filter(|room| uplink_query::matches(query, &|term| self.term_matches(room, term)))
            .cloned()
            .collect();
        matched.sort_by(|a, b| b.last_event_ts.cmp(&a.last_event_ts).then(a.id.cmp(&b.id)));
        matched
    }

    fn term_matches(&self, room: &Room, term: &Term) -> bool {
        term_matches(room, term, &self.spaces, self.accounts)
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

fn term_matches(room: &Room, term: &Term, spaces: &Spaces, accounts: &[ConfiguredAccount]) -> bool {
    match term {
        Term::Text {
            fields: None,
            text,
            exact,
        } => text_matches(&room.name, text, *exact),
        Term::Kind(kind) => *kind == Kind::Room,
        Term::Label(name) => room.tags.iter().any(|tag| names_tag(name, tag)),
        Term::Unread => room.unread > 0,
        // However the read marker moved (SPEC §12.15).
        Term::Mention => room.mentions > 0,
        Term::Notifying => room.notifying > 0,
        Term::Direct => room.direct,
        Term::Invited => room.membership == Membership::Invited,
        Term::Space(name) => spaces.within(&room.room_id, name),
        Term::Account(account) => accounts
            .iter()
            .any(|a| a.id == room.account && (a.id == *account || a.mxid == *account)),
        // Of everything, as Trash and Spam are not of rooms.
        Term::Anywhere => true,
        Term::After(at) => room.last_event_ts.is_some_and(|ts| ts >= *at),
        Term::Before(at) => room.last_event_ts.is_some_and(|ts| ts < *at),
        // Of messages, or of other kinds.
        Term::Text {
            fields: Some(_), ..
        }
        | Term::Me(_)
        | Term::HasAttachment
        | Term::Larger(_)
        | Term::Smaller(_)
        | Term::MessageId(_)
        | Term::Due(_)
        | Term::Overdue
        | Term::HasDue
        | Term::Done => false,
    }
}

impl Rooms<'_> {
    /// The messages of the rooms (spaces' and rooms left aside) that
    /// `query` matches, the latest first, up to `limit`.
    pub fn messages(
        &self,
        tx: &Transaction,
        query: &Query,
        limit: u32,
    ) -> Result<Vec<FoundMessage>> {
        let rooms: Vec<&Room> = self.listed().collect();
        let mut sql = Sql {
            params: vec![],
            mine: self.accounts.iter().map(|a| a.mxid.clone()).collect(),
        };
        let condition = Cond::of(query, &|term| {
            Cond::term(term, &|term| {
                rooms
                    .iter()
                    .filter(|room| self.term_matches(room, term))
                    .map(|room| room.id.0)
                    .collect()
            })
        });
        if condition == Cond::Const(false) || rooms.is_empty() {
            return Ok(vec![]);
        }
        let within = rooms
            .iter()
            .map(|room| room.id.0.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let matches = sql.render(&condition);
        sql.params.push(Value::Integer(limit.into()));
        let found: Vec<(i64, i64)> = tx
            .prepare(&format!(
                "SELECT e.room, e.id FROM matrix_event e JOIN matrix_room r ON r.id = e.room
             WHERE e.room IN ({within}) AND e.type = 'm.room.message' AND NOT e.redacted
               AND {LISTED} AND {matches}
             ORDER BY e.ts DESC, e.id DESC LIMIT ?{n}",
                n = sql.params.len(),
            ))?
            .query_map(params_from_iter(sql.params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut by_room: HashMap<i64, Vec<RoomEventId>> = HashMap::new();
        for (room, event) in &found {
            by_room.entry(*room).or_default().push(RoomEventId(*event));
        }
        let mut events = HashMap::new();
        for (room, ids) in by_room {
            for event in rooms::listed_events(tx, RoomId(room), &ids)? {
                events.insert(event.id, (RoomId(room), event));
            }
        }
        Ok(found
            .into_iter()
            .map(|(_, event)| {
                let (room, event) = events
                    .remove(&RoomEventId(event))
                    .expect("a listed event found is listed");
                FoundMessage { room, event }
            })
            .collect())
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
    /// After its room's read marker, and not the account's own.
    Unread,
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
            Query::And(all) => {
                let all: Vec<Cond> = all.iter().map(|q| Cond::of(q, term)).collect();
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
            Query::Or(all) => {
                let all: Vec<Cond> = all.iter().map(|q| Cond::of(q, term)).collect();
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
            Query::Not(q) => match Cond::of(q, term) {
                Cond::Const(c) => Cond::Const(!c),
                c => Cond::Not(Box::new(c)),
            },
            Query::Term(t) => term(t),
        }
    }

    /// `rooms` lists the rooms a term of rooms holds of.
    fn term(term: &Term, rooms: &impl Fn(&Term) -> Vec<i64>) -> Cond {
        match term {
            Term::Text {
                fields: None | Some(Fields::Body),
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
            Term::Mention => Cond::Highlights,
            Term::Notifying => Cond::Notifies,
            Term::After(at) => Cond::After(*at),
            Term::Before(at) => Cond::Before(*at),
            // Of everything, as Trash and Spam are not of messages.
            Term::Anywhere => Cond::Const(true),
            // Of its room.
            Term::Label(_) | Term::Account(_) | Term::Direct | Term::Invited | Term::Space(_) => {
                match rooms(term) {
                    ids if ids.is_empty() => Cond::Const(false),
                    ids => Cond::Rooms(ids),
                }
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
}

/// SQL of a condition on a message `e` of room `r`, and its parameters.
struct Sql {
    params: Vec<Value>,
    /// The accounts' Matrix ids.
    mine: Vec<String>,
}

impl Sql {
    fn param(&mut self, value: impl Into<Value>) -> String {
        self.params.push(value.into());
        format!("?{}", self.params.len())
    }

    fn mine(&mut self) -> String {
        let mine: Vec<String> = self
            .mine
            .clone()
            .into_iter()
            .map(|m| self.param(m))
            .collect();
        format!("({})", mine.join(", "))
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
            Cond::Unread => format!(
                "((r.read_up_to IS NULL OR e.pos > {read}) AND e.sender NOT IN {mine})",
                read = pos_of("r.read_up_to"),
                mine = self.mine(),
            ),
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
