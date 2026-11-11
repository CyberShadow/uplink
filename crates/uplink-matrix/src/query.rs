//! Rooms by the query language (SPEC §8.7): a room matches by its name,
//! what is unread in it, its tags, the spaces it is in, and its latest
//! activity. Rooms are few, so they are matched as listed, not in SQL.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use uplink_protocol::matrix::{Membership, Room};
use uplink_query::{text_matches, Kind, Query, Term};
use uplink_storage::rusqlite::Transaction;

use crate::{room_tags, rooms, service::ConfiguredAccount};

/// The rooms of `accounts` that `query` matches, most recently active
/// first: those listed, but spaces, which are no views of their own.
pub fn rooms(tx: &Transaction, accounts: &[ConfiguredAccount], query: &Query) -> Result<Vec<Room>> {
    let mut all = vec![];
    for account in accounts {
        all.extend(rooms::rooms(tx, &account.id, &account.mxid)?);
    }
    let spaces = Spaces::of(&all);
    let mut matched: Vec<Room> = all
        .iter()
        .filter(|room| !room.space)
        .filter(|room| {
            uplink_query::matches(query, &|term| term_matches(room, term, &spaces, accounts))
        })
        .cloned()
        .collect();
    matched.sort_by(|a, b| b.last_event_ts.cmp(&a.last_event_ts).then(a.id.cmp(&b.id)));
    Ok(matched)
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
        Term::Mention => room.highlighted > 0,
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
