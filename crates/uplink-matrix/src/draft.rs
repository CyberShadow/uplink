//! What is being written to rooms, and the log entries that change it
//! (SPEC §7.6).
//!
//! A box (a room's, or one of its threads') holds the text as typed,
//! edited by splices each based on the revision it was computed against,
//! as a mail draft is (§7.2); what its message replies to; and, of a
//! room's box, the files to send with it. A box never written to has no
//! row: it is empty, at revision 0. Sending is not of the draft: the
//! client sends what it makes of the text, and empties the box by the
//! same ops that filled it.

use crate::rooms;
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    compose::{BlobId, DraftAttachment, DraftAttachmentId, Splice},
    matrix::{RoomBox, RoomDraft, RoomDraftEdited, RoomDraftsChanged, RoomEventId, RoomId},
    ServerEvent,
};
use uplink_storage::rusqlite::{params, OptionalExtension, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Op};

/// A box's row.
struct Stored {
    id: i64,
    revision: i64,
    text: String,
    reply_to: Option<RoomEventId>,
}

/// The box's row, made if it has none.
fn row(tx: &Transaction, at: &RoomBox) -> Result<Stored> {
    let stored = tx
        .query_row(
            "SELECT id, revision, text, reply_to FROM matrix_draft
             WHERE room = ?1 AND thread IS ?2",
            params![at.room.0, at.thread.map(|t| t.0)],
            |r| {
                Ok(Stored {
                    id: r.get(0)?,
                    revision: r.get(1)?,
                    text: r.get(2)?,
                    reply_to: r.get::<_, Option<i64>>(3)?.map(RoomEventId),
                })
            },
        )
        .optional()?;
    if let Some(stored) = stored {
        return Ok(stored);
    }
    let id = tx.query_row(
        "INSERT INTO matrix_draft (room, thread, revision, text) VALUES (?1, ?2, 0, '')
         RETURNING id",
        params![at.room.0, at.thread.map(|t| t.0)],
        |r| r.get(0),
    )?;
    Ok(Stored {
        id,
        revision: 0,
        text: String::new(),
        reply_to: None,
    })
}

/// The files attached to the box of row `draft`, in the order attached.
fn attachments(tx: &Transaction, draft: i64) -> Result<Vec<DraftAttachment>> {
    Ok(tx
        .prepare_cached(
            "SELECT id, blob, name, mime, size FROM matrix_draft_attachment
             WHERE draft = ?1 ORDER BY id",
        )?
        .query_map([draft], |r| {
            Ok(DraftAttachment {
                id: DraftAttachmentId(r.get(0)?),
                blob: BlobId(r.get(1)?),
                name: r.get(2)?,
                mime: r.get(3)?,
                size: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Every box written to, even if emptied since.
pub fn drafts(tx: &Transaction) -> Result<Vec<RoomDraft>> {
    let rows: Vec<(RoomBox, Stored)> = tx
        .prepare("SELECT room, thread, id, revision, text, reply_to FROM matrix_draft ORDER BY id")?
        .query_map([], |r| {
            Ok((
                RoomBox {
                    room: RoomId(r.get(0)?),
                    thread: r.get::<_, Option<i64>>(1)?.map(RoomEventId),
                },
                Stored {
                    id: r.get(2)?,
                    revision: r.get(3)?,
                    text: r.get(4)?,
                    reply_to: r.get::<_, Option<i64>>(5)?.map(RoomEventId),
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(at, stored)| {
            // As the timeline lists it; one it lists no longer is not
            // shown replied to.
            let reply_to = match stored.reply_to {
                Some(event) => rooms::listed_events(tx, at.room, &[event])?.pop(),
                None => None,
            };
            Ok(RoomDraft {
                attachments: attachments(tx, stored.id)?,
                at,
                revision: stored.revision,
                text: stored.text,
                reply_to,
            })
        })
        .collect()
}

/// How a box's draft is named in descriptions.
fn named(tx: &Transaction, at: &RoomBox) -> Result<String> {
    let room = rooms::name(tx, at.room)?;
    Ok(match at.thread {
        None => format!("the draft in {room}"),
        Some(_) => format!("the draft in a thread of {room}"),
    })
}

fn changed(room: RoomId) -> Vec<ServerEvent> {
    vec![ServerEvent::RoomDraftsChanged(RoomDraftsChanged {
        rooms: vec![room],
    })]
}

/// Applies `splice` to the box's text at `revision`. Inverses, which undo
/// applies to whatever revision the text has reached, have no revision;
/// they check only that the text they remove is where they expect it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditDraft {
    pub at: RoomBox,
    pub revision: Option<i64>,
    pub splice: Splice,
}

impl Op for EditDraft {
    const KIND: &'static str = "matrix.draft.edit";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let draft = row(tx, &self.at)?;
        if let Some(revision) = self.revision {
            if revision != draft.revision {
                bail!(Conflict(format!(
                    "the draft is at revision {}, not {revision}",
                    draft.revision
                )));
            }
        }
        let text = self
            .splice
            .apply(&draft.text)
            .ok_or_else(|| Conflict("the draft's text changed".into()))?;
        let revision = draft.revision + 1;
        tx.execute(
            "UPDATE matrix_draft SET text = ?1, revision = ?2 WHERE id = ?3",
            params![text, revision, draft.id],
        )?;
        Ok(Applied {
            description: format!("Edited {}", named(tx, &self.at)?),
            inverse: Inverse::of(&EditDraft {
                at: self.at,
                revision: None,
                splice: self.splice.inverse(),
            }),
            events: vec![ServerEvent::RoomDraftEdited(RoomDraftEdited {
                at: self.at,
                revision,
                splice: self.splice,
            })],
        })
    }
}

/// Has the box's message reply to `reply_to`, an event of its room, or
/// to nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetReply {
    pub at: RoomBox,
    pub reply_to: Option<RoomEventId>,
}

impl Op for SetReply {
    const KIND: &'static str = "matrix.draft.reply";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        if let Some(event) = self.reply_to {
            ensure!(
                rooms::event_in_room(tx, self.at.room, event)?,
                Conflict("the message replied to is not of the room".into())
            );
        }
        let draft = row(tx, &self.at)?;
        tx.execute(
            "UPDATE matrix_draft SET reply_to = ?1 WHERE id = ?2",
            params![self.reply_to.map(|e| e.0), draft.id],
        )?;
        let room = rooms::name(tx, self.at.room)?;
        Ok(Applied {
            description: match self.reply_to {
                Some(_) => format!("Began a reply in {room}"),
                None => format!("Left off replying in {room}"),
            },
            inverse: Inverse::of(&SetReply {
                at: self.at,
                reply_to: draft.reply_to,
            }),
            events: changed(self.at.room),
        })
    }
}

/// Attaches a stored blob to the room's box; with an `id`, as the
/// attachment it was before it was detached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachToDraft {
    pub room: RoomId,
    pub id: Option<DraftAttachmentId>,
    pub blob: BlobId,
    pub name: String,
    pub mime: String,
    pub size: i64,
}

/// The room's own box, which alone has files.
fn of_room(room: RoomId) -> RoomBox {
    RoomBox { room, thread: None }
}

impl Op for AttachToDraft {
    const KIND: &'static str = "matrix.draft.attach";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let at = of_room(self.room);
        let draft = row(tx, &at)?;
        let id = DraftAttachmentId(tx.query_row(
            "INSERT INTO matrix_draft_attachment (id, draft, blob, name, mime, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             RETURNING id",
            params![
                self.id.map(|id| id.0),
                draft.id,
                self.blob.0,
                self.name,
                self.mime,
                self.size
            ],
            |r| r.get(0),
        )?);
        Ok(Applied {
            description: format!("Attached '{}' to {}", self.name, named(tx, &at)?),
            inverse: Inverse::of(&DetachFromDraft {
                room: self.room,
                attachment: id,
            }),
            events: changed(self.room),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachFromDraft {
    pub room: RoomId,
    pub attachment: DraftAttachmentId,
}

impl Op for DetachFromDraft {
    const KIND: &'static str = "matrix.draft.detach";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let at = of_room(self.room);
        let draft = row(tx, &at)?;
        let attachment = attachments(tx, draft.id)?
            .into_iter()
            .find(|a| a.id == self.attachment)
            .ok_or_else(|| Conflict("the file is no longer attached".into()))?;
        tx.execute(
            "DELETE FROM matrix_draft_attachment WHERE id = ?1",
            [self.attachment.0],
        )?;
        Ok(Applied {
            description: format!("Removed '{}' from {}", attachment.name, named(tx, &at)?),
            inverse: Inverse::of(&AttachToDraft {
                room: self.room,
                id: Some(attachment.id),
                blob: attachment.blob,
                name: attachment.name,
                mime: attachment.mime,
                size: attachment.size,
            }),
            events: changed(self.room),
        })
    }
}
