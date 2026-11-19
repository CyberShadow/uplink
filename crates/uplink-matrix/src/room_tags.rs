//! A room's tags (`m.tag` room account data, SPEC §9.5), as Element keeps
//! them: `m.favourite`, `m.lowpriority`, and the user's own, `u.*`. They
//! are desired state (§6.6), as mail's labels are: tagging records the
//! tags asked for, and the account's sync task sets them on the
//! homeserver. Each is a reconciled field of its own, as Matrix sets and
//! removes tags one by one. A room is a favourite or of low priority, not both, as
//! Element has it: tagging it one untags it the other.

use anyhow::{Context, Result};
use matrix_sdk::ruma::{
    api::client::tag::{create_tag, delete_tag, get_tags},
    events::tag::TagInfo,
};
use serde::{Deserialize, Serialize};
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use uplink_protocol::{
    matrix::{RoomId, RoomTagChange, RoomsChanged},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{
    reconcile::{Ended, Field},
    this_entry, Applied, Conflict, Inverse, Op,
};

use crate::{
    outbox,
    reconciler::{self, Homeserver, Kind, Requests, RoomIdRef, Stored},
};

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
pub(crate) fn name(tag: &str) -> &str {
    match tag {
        FAVOURITE => "Favourite",
        LOW_PRIORITY => "Low priority",
        _ => tag.strip_prefix("u.").unwrap_or(tag),
    }
}

/// A tag's state (SPEC §6.6): had (`observed`) or asked for (`desired`).
pub type Tag = Field<bool>;

/// The room's tag as stored, or, without a row, as known of every tag it
/// has none of: neither had nor asked for.
fn load(tx: &Connection, room: RoomId, tag: &str) -> Result<(Tag, bool)> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT field FROM matrix_room_tag WHERE room = ?1 AND tag = ?2",
            params![room.0, tag],
            |r| r.get(0),
        )
        .optional()?;
    match stored {
        Some(json) => Ok((parse_field(&json)?, true)),
        None => Ok((Field::new(false, known_at(tx, room)?), false)),
    }
}

fn parse_field(json: &str) -> Result<Tag> {
    serde_json::from_str(json).with_context(|| format!("a room tag's field: {json}"))
}

/// What is known of the room's tags it has no row of, since when.
fn known_at(tx: &Connection, room: RoomId) -> Result<TxLogId> {
    Ok(TxLogId(tx.query_row(
        "SELECT tags_known_at FROM matrix_room WHERE id = ?1",
        [room.0],
        |r| r.get(0),
    )?))
}

fn store(tx: &Transaction, room: RoomId, tag: &str, field: &Tag) -> Result<()> {
    tx.execute(
        "INSERT INTO matrix_room_tag (room, tag, field) VALUES (?1, ?2, ?3)
         ON CONFLICT (room, tag) DO UPDATE SET field = excluded.field",
        params![room.0, tag, serde_json::to_string(field)?],
    )?;
    Ok(())
}

/// Records the room's tags as observed by a request sent after `as_of`
/// (SPEC §6.6.2); returns whether those shown changed.
pub(crate) fn observe(
    tx: &Transaction,
    room: RoomId,
    tags: &[String],
    as_of: TxLogId,
) -> Result<bool> {
    let at = this_entry(tx)?;
    let stored: Vec<String> = tx
        .prepare("SELECT tag FROM matrix_room_tag WHERE room = ?1")?
        .query_map([room.0], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let unstored = Field::new(false, known_at(tx, room)?);
    let mut after = unstored.clone();
    after.observe(false, as_of, at)?;
    let mut changed = false;
    let new = tags.iter().filter(|t| !stored.contains(t));
    for tag in stored.iter().chain(new) {
        let (mut field, had_row) = load(tx, room, tag)?;
        let shown = field.desired;
        field.observe(tags.contains(tag), as_of, at)?;
        changed |= field.desired != shown;
        // A tag without a row stays so if it is as all of them are.
        if had_row || field != after {
            store(tx, room, tag, &field)?;
        }
    }
    tx.execute(
        "UPDATE matrix_room SET tags_known_at = ?2 WHERE id = ?1",
        params![room.0, after.known_at.0],
    )?;
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
        let this = this_entry(tx)?;
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
            let (mut field, _) = load(tx, change.room, &change.tag)?;
            let desired = field.desired;
            if desired == change.desired {
                continue;
            }
            field.want(change.desired, this);
            store(tx, change.room, &change.tag, &field)?;
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

/// A room's tags, of which each is a field, keyed by the tag.
pub struct Tags;

impl Kind for Tags {
    type Value = bool;
    /// The room's tags.
    type Read = Vec<String>;
    type Request = (String, bool);

    fn requests(
        _conn: &Connection,
        in_flight: Vec<Stored<bool>>,
    ) -> Result<Requests<Self::Request, bool>> {
        Ok(reconciler::each(in_flight))
    }

    const CLAIMED: &'static str = "matrix.room_tags_claimed";
    const PUSHED: &'static str = "matrix.room_tag_set";
    const READ: &'static str = "matrix.room_tags_read";
    const SETTLED: &'static str = "matrix.room_tag_pushes_settled";
    const WHAT: &'static str = "room tags";

    fn load(conn: &Connection, room: RoomId, tag: &str) -> Result<Tag> {
        Ok(load(conn, room, tag)?.0)
    }

    fn store(tx: &Transaction, room: RoomId, tag: &str, field: &Tag) -> Result<()> {
        store(tx, room, tag, field)
    }

    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored<bool>>> {
        conn.prepare(
            "SELECT r.id, r.room_id, t.tag, t.field
             FROM matrix_room_tag t JOIN matrix_room r ON r.id = t.room
             WHERE r.account_id = ?1
               AND (t.desired_by IS NOT NULL OR json_extract(t.field, '$.check') IS NOT NULL)
             ORDER BY r.id, t.tag",
        )?
        .query_map([account], |r| {
            Ok((
                RoomId(r.get(0)?),
                r.get(1)?,
                r.get(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .map(|row| {
            let (room, room_id, key, json) = row?;
            let field = parse_field(&json)?;
            Ok(Stored {
                room,
                room_id,
                key,
                field,
            })
        })
        .collect()
    }

    fn observe(tx: &Transaction, room: RoomId, tags: &Vec<String>, as_of: TxLogId) -> Result<bool> {
        observe(tx, room, tags, as_of)
    }

    fn pushed(room: &str, tag: &str, tagged: &bool, ended: &Ended) -> String {
        let tag = name(tag);
        let verb = if *tagged { "tag" } else { "untag" };
        match ended {
            Ended::Applied if *tagged => format!("Tagged '{room}' {tag} on the homeserver"),
            Ended::Applied => format!("Untagged '{room}' {tag} on the homeserver"),
            Ended::NotApplied {
                refused: Some(error),
            } => format!("The homeserver refused to {verb} '{room}' {tag}: {error}"),
            Ended::NotApplied { refused: None } => {
                format!("Could not {verb} '{room}' {tag} on the homeserver")
            }
            Ended::Unknown { .. } => {
                format!("Whether the homeserver did {verb} '{room}' {tag} is unknown")
            }
        }
    }

    async fn push(
        homeserver: &Homeserver,
        room: &RoomIdRef,
        (tag, tagged): &(String, bool),
    ) -> matrix_sdk::Result<()> {
        let (user, room, tag) = (homeserver.user.clone(), room.to_owned(), tag.clone());
        let client = &homeserver.client;
        if *tagged {
            let request = create_tag::v3::Request::new(user, room, tag, TagInfo::new());
            client.send(request).await.map(drop)?;
        } else {
            client
                .send(delete_tag::v3::Request::new(user, room, tag))
                .await
                .map(drop)?;
        }
        Ok(())
    }

    async fn read(homeserver: &Homeserver, room: &RoomIdRef, _key: &str) -> Result<Vec<String>> {
        let request = get_tags::v3::Request::new(homeserver.user.clone(), room.to_owned());
        let response = homeserver
            .client
            .send(request)
            .await
            .context("reading a room's tags")?;
        Ok(response.tags.keys().map(ToString::to_string).collect())
    }
}

/// A change of a room's tag pending, for the outbox.
pub(crate) struct Pending {
    pub account: String,
    /// The op that asked.
    pub op: TxLogId,
    pub change: RoomTagChange,
    /// Why it needs a decision, if it does.
    pub refused: Option<String>,
}

/// The tags of rooms pending, by account, then by the op that asked,
/// newest first.
pub(crate) fn pending(conn: &Connection) -> Result<Vec<Pending>> {
    conn.prepare(
        "SELECT r.account_id, t.room, t.tag, t.field
         FROM matrix_room_tag t JOIN matrix_room r ON r.id = t.room
         WHERE t.desired_by IS NOT NULL
         ORDER BY r.account_id, t.desired_by DESC, t.room, t.tag",
    )?
    .query_map([], |r| {
        Ok((
            r.get(0)?,
            RoomId(r.get(1)?),
            r.get(2)?,
            r.get::<_, String>(3)?,
        ))
    })?
    .map(|row| {
        let (account, room, tag, json) = row?;
        let field = parse_field(&json)?;
        Ok(Pending {
            account,
            op: field.opinion.context("pending without an opinion")?,
            change: RoomTagChange {
                room,
                tag,
                desired: field.desired,
            },
            refused: field.decision.as_ref().map(|d| d.reason().to_owned()),
        })
    })
    .collect()
}
