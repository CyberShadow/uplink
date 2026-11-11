//! A room's tags (`m.tag` room account data, SPEC §9.5), as Element keeps
//! them: `m.favourite`, `m.lowpriority`, and the user's own, `u.*`. They
//! are desired state (§6.6), as mail's labels are: tagging records the
//! tags asked for, and the account's sync task sets them on the
//! homeserver. A room is a favourite or of low priority, not both, as
//! Element has it: tagging it one untags it the other.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use uplink_protocol::{
    matrix::{RoomId, RoomTagChange, RoomsChanged},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{this_entry, Applied, Conflict, Inverse, Observation, Observed, Op};

use crate::outbox;

pub const FAVOURITE: &str = "m.favourite";
pub const LOW_PRIORITY: &str = "m.lowpriority";

/// The longest tag the spec allows, in bytes.
const MAX_TAG: usize = 255;

/// Whether the user may tag a room `tag`: a favourite, of low priority,
/// or a tag of their own (`u.` and a name). Others, as other clients may
/// have set, may only be removed.
pub fn valid(tag: &str) -> bool {
    tag.len() <= MAX_TAG
        && (tag == FAVOURITE
            || tag == LOW_PRIORITY
            || tag
                .strip_prefix("u.")
                .is_some_and(|name| !name.trim().is_empty()))
}

/// The tags of `m.tag`'s `content`, as the homeserver has them.
pub fn parse(content: &serde_json::Value) -> Result<Vec<String>> {
    let Some(tags) = content.get("tags") else {
        return Ok(vec![]);
    };
    let tags = tags
        .as_object()
        .with_context(|| format!("m.tag's tags are not an object: {tags}"))?;
    Ok(tags.keys().cloned().collect())
}

/// The room's tags as shown: as asked for. Of `matrix_room r`; a JSON
/// array, in order.
pub(crate) const SHOWN: &str = "(SELECT json_group_array(tag) FROM
   (SELECT tag FROM matrix_room_tag t WHERE t.room = r.id AND t.desired ORDER BY tag))";

/// The tag as the user knows it: Element's name of its own, a user tag's
/// name, or else as it is.
fn name(tag: &str) -> &str {
    match tag {
        FAVOURITE => "Favourite",
        LOW_PRIORITY => "Low priority",
        _ => tag.strip_prefix("u.").unwrap_or(tag),
    }
}

/// `(desired, observed, desired_by)` of the room's tag, if it has a row.
fn row(tx: &Transaction, room: RoomId, tag: &str) -> Result<Option<(bool, bool, Option<i64>)>> {
    Ok(tx
        .query_row(
            "SELECT desired, observed, desired_by FROM matrix_room_tag
             WHERE room = ?1 AND tag = ?2",
            params![room.0, tag],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

/// Stores the tag's state: no row if neither asked for nor had, and no op
/// (nor refusal) unless they differ.
fn set_row(
    tx: &Transaction,
    room: RoomId,
    tag: &str,
    desired: bool,
    observed: bool,
    by: Option<i64>,
) -> Result<()> {
    if !desired && !observed {
        tx.execute(
            "DELETE FROM matrix_room_tag WHERE room = ?1 AND tag = ?2",
            params![room.0, tag],
        )?;
        return Ok(());
    }
    let by = (desired != observed).then(|| by.expect("a pending change is an op's"));
    tx.execute(
        "INSERT INTO matrix_room_tag (room, tag, desired, observed, desired_by, refused)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL)
         ON CONFLICT (room, tag) DO UPDATE SET desired = excluded.desired,
           observed = excluded.observed, desired_by = excluded.desired_by, refused = NULL",
        params![room.0, tag, desired, observed, by],
    )?;
    Ok(())
}

/// Records the room's tags as synced; returns whether those shown changed.
/// A tag asked for prevails until set (or refused); one had as asked for,
/// however it came about, is no longer pending.
pub(crate) fn observe(tx: &Transaction, room: RoomId, tags: &[String]) -> Result<bool> {
    let stored: Vec<(String, bool, bool, Option<i64>)> = tx
        .prepare("SELECT tag, desired, observed, desired_by FROM matrix_room_tag WHERE room = ?1")?
        .query_map([room.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut changed = false;
    for (tag, desired, observed, by) in &stored {
        let had = tags.contains(tag);
        if had == *observed {
            continue;
        }
        // Pending, it is had as asked for now (it differed from what was).
        let shown = if by.is_some() { *desired } else { had };
        changed |= shown != *desired;
        set_row(tx, room, tag, shown, had, *by)?;
    }
    for tag in tags {
        if !stored.iter().any(|(t, ..)| t == tag) {
            set_row(tx, room, tag, true, true, None)?;
            changed = true;
        }
    }
    Ok(changed)
}

// ── ops ───────────────────────────────────────────────────────────────────

/// Sets the tags asked for of rooms. Changes already in effect are
/// skipped, so the inverse reverts exactly what this op changed (§6.2).
/// Tagging a room a favourite untags it of low priority, and the other
/// way: those implied changes are of the op and its inverse, but not of
/// its description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRoomTags {
    pub changes: Vec<RoomTagChange>,
}

/// The tag a room tagged `tag` loses, if any.
fn exclusive(tag: &str) -> Option<&'static str> {
    match tag {
        FAVOURITE => Some(LOW_PRIORITY),
        LOW_PRIORITY => Some(FAVOURITE),
        _ => None,
    }
}

impl Op for SetRoomTags {
    const KIND: &'static str = "matrix.set_room_tags";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let this = this_entry(tx)?.0;
        let implied: Vec<RoomTagChange> = self
            .changes
            .iter()
            .filter(|c| c.desired)
            .filter_map(|c| {
                let other = exclusive(&c.tag)?;
                let asked = self
                    .changes
                    .iter()
                    .any(|d| d.room == c.room && d.tag == other);
                (!asked).then(|| RoomTagChange {
                    room: c.room,
                    tag: other.to_owned(),
                    desired: false,
                })
            })
            .collect();
        let mut pending = BTreeMap::new();
        let mut inverse = vec![];
        let mut rooms = BTreeSet::new();
        // (tag, desired) → names of the rooms changed
        let mut described: BTreeMap<(String, bool), Vec<String>> = BTreeMap::new();
        let changes = self.changes.into_iter().map(|c| (c, true));
        for (change, describe) in changes.chain(implied.into_iter().map(|c| (c, false))) {
            let (account, room_name): (String, String) = tx
                .query_row(
                    "SELECT account_id, name FROM matrix_room WHERE id = ?1",
                    [change.room.0],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| Conflict("the room is no longer stored".into()))?;
            if let Entry::Vacant(vacant) = pending.entry(account) {
                let before = outbox::pending_rooms(tx, vacant.key())?;
                vacant.insert(before);
            }
            let (desired, observed, _) = row(tx, change.room, &change.tag)?.unwrap_or_default();
            if desired == change.desired {
                continue;
            }
            set_row(
                tx,
                change.room,
                &change.tag,
                change.desired,
                observed,
                Some(this),
            )?;
            rooms.insert(change.room);
            if describe {
                described
                    .entry((change.tag.clone(), change.desired))
                    .or_default()
                    .push(room_name);
            }
            inverse.push(RoomTagChange { desired, ..change });
        }
        let mut events = vec![];
        if !rooms.is_empty() {
            events.push(ServerEvent::RoomsChanged(RoomsChanged {
                rooms: rooms.into_iter().collect(),
            }));
        }
        for (account, before) in pending {
            if outbox::pending_rooms(tx, &account)? != before {
                events.push(outbox::outbox_changed(&account));
            }
        }
        let description: Vec<String> = described
            .into_iter()
            .map(|((tag, desired), names)| {
                let verb = if desired { "Tagged" } else { "Untagged" };
                let rooms = match &names[..] {
                    [name] => format!("'{name}'"),
                    names => format!("{} rooms", names.len()),
                };
                format!("{verb} {rooms} {}", name(&tag))
            })
            .collect();
        Ok(Applied {
            description: if description.is_empty() {
                "Changed nothing".to_owned()
            } else {
                description.join(", ")
            },
            inverse: Inverse::of(&SetRoomTags { changes: inverse }),
            events,
        })
    }
}

/// A tag to set or remove on the homeserver.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub room: RoomId,
    pub room_id: String,
    pub tag: String,
    pub desired: bool,
}

/// The account's tags still to set or remove, but those refused.
pub fn wanted(conn: &Connection, account: &str) -> Result<Vec<Wanted>> {
    Ok(conn
        .prepare(
            "SELECT r.id, r.room_id, t.tag, t.desired
             FROM matrix_room_tag t JOIN matrix_room r ON r.id = t.room
             WHERE r.account_id = ?1 AND t.desired != t.observed AND t.refused IS NULL
             ORDER BY r.id, t.tag",
        )?
        .query_map([account], |r| {
            Ok(Wanted {
                room: RoomId(r.get(0)?),
                room_id: r.get(1)?,
                tag: r.get(2)?,
                desired: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// A change of a room's tag pending, for the outbox.
pub(crate) struct Pending {
    pub account: String,
    /// The op that asked.
    pub op: TxLogId,
    pub change: RoomTagChange,
    /// Why the homeserver refused, if it did.
    pub refused: Option<String>,
}

/// The tags of rooms pending, by account, then by the op that asked,
/// newest first.
pub(crate) fn pending(conn: &Connection) -> Result<Vec<Pending>> {
    Ok(conn
        .prepare(
            "SELECT r.account_id, t.desired_by, t.room, t.tag, t.desired, t.refused
             FROM matrix_room_tag t JOIN matrix_room r ON r.id = t.room
             WHERE t.desired != t.observed
             ORDER BY r.account_id, t.desired_by DESC, t.room, t.tag",
        )?
        .query_map([], |r| {
            Ok(Pending {
                account: r.get(0)?,
                op: TxLogId(r.get(1)?),
                change: RoomTagChange {
                    room: RoomId(r.get(2)?),
                    tag: r.get(3)?,
                    desired: r.get(4)?,
                },
                refused: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// The homeserver has the room tagged `tag`, or not, as `tagged` says; or
/// refused, saying why.
#[derive(Debug, Serialize)]
pub struct RoomTagSet {
    pub room: RoomId,
    pub tag: String,
    pub tagged: bool,
    pub refused: Option<String>,
}

impl Observation for RoomTagSet {
    const KIND: &'static str = "matrix.room_tag_set";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let before = outbox::pending_rooms(tx, account)?;
        let room_name: String = tx
            .query_row(
                "SELECT name FROM matrix_room WHERE id = ?1",
                [self.room.0],
                |r| r.get(0),
            )
            .with_context(|| format!("no room {}", self.room.0))?;
        let (desired, observed, by) = row(tx, self.room, &self.tag)?.unwrap_or_default();
        let mut events = vec![];
        match &self.refused {
            // Unless asked otherwise meanwhile (that is the sync task's
            // next step), refused until the user decides.
            Some(error) => {
                tx.execute(
                    "UPDATE matrix_room_tag SET refused = ?3
                     WHERE room = ?1 AND tag = ?2 AND desired = ?4 AND desired != observed",
                    params![self.room.0, self.tag, error, self.tagged],
                )?;
            }
            None if observed != self.tagged => {
                // Asked for no longer, the tag shown is the homeserver's.
                let shown = if by.is_some() { desired } else { self.tagged };
                set_row(tx, self.room, &self.tag, shown, self.tagged, by)?;
                if shown != desired {
                    events.push(ServerEvent::RoomsChanged(RoomsChanged {
                        rooms: vec![self.room],
                    }));
                }
            }
            None => {}
        }
        if outbox::pending_rooms(tx, account)? != before {
            events.push(outbox::outbox_changed(account));
        }
        let tag = name(&self.tag);
        Ok(Observed {
            description: match (&self.refused, self.tagged) {
                (None, true) => format!("Tagged '{room_name}' {tag} on the homeserver"),
                (None, false) => format!("Untagged '{room_name}' {tag} on the homeserver"),
                (Some(error), _) => {
                    format!("The homeserver refused to tag '{room_name}' {tag}: {error}")
                }
            },
            events,
        })
    }
}
