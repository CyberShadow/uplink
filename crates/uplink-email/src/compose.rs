//! Drafts and mail sends, and the log entries that change them (SPEC
//! §6.7, §7).
//!
//! A draft is edited by splices, each based on the revision it was
//! computed against, so a stale edit from another tab is refused rather
//! than applied in the wrong place. Sending turns the draft into a send,
//! which the account's sync task hands to the provider once the send
//! delay has passed; until it claims the send, cancelling takes it back
//! for sure, returning the draft.
//!
//! Files are attached by uploading them to the blob store, then attaching
//! the blob, as an op of its own beside the text's splices (§7.4).

use crate::{
    mailbox::{insert_variant, outbox_changed},
    parse::Parsed,
};
use anyhow::{bail, ensure, Context, Result};
use mail_builder::{
    headers::{address::Address, message_id::MessageId as MessageIdHeader},
    MessageBuilder,
};
use mail_parser::{HeaderName, MessageParser};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};
use uplink_protocol::{
    compose::{
        BlobId, Draft, DraftAttachment, DraftAttachmentId, DraftContent, DraftEdited, DraftField,
        DraftId, DraftsChanged, OutboxId, OutboxItem, SendState, Splice,
    },
    mail::MessageId,
    ServerEvent,
};
use uplink_storage::rusqlite::{params, OptionalExtension, Row, Transaction};
use uplink_storage::store::{Store, VariantHash};
use uplink_tx_log::{this_entry, Applied, Conflict, Inverse, Observation, Observed, Op, Registry};

pub fn register(registry: &mut Registry) {
    registry.register::<PutDraft>();
    registry.register::<DeleteDraft>();
    registry.register::<EditDraft>();
    registry.register::<AttachToDraft>();
    registry.register::<DetachFromDraft>();
    registry.register::<SendDraft>();
    registry.register::<CancelSend>();
    registry.register::<KeepSent>();
}

// ── drafts ────────────────────────────────────────────────────────────────

const DRAFT_COLUMNS: &str = "id, account_id, revision, to_addrs, cc_addrs, bcc_addrs, subject,
                             body_html, in_reply_to, updated_at";

/// The draft, without its attachments.
fn draft_row(r: &Row) -> uplink_storage::rusqlite::Result<Draft> {
    Ok(Draft {
        id: DraftId(r.get(0)?),
        account: r.get(1)?,
        revision: r.get(2)?,
        content: DraftContent {
            to: r.get(3)?,
            cc: r.get(4)?,
            bcc: r.get(5)?,
            subject: r.get(6)?,
            body: r.get(7)?,
        },
        attachments: vec![],
        in_reply_to: r.get::<_, Option<i64>>(8)?.map(MessageId),
        updated_at: r.get(9)?,
    })
}

fn with_attachments(tx: &Transaction, mut draft: Draft) -> Result<Draft> {
    draft.attachments = tx
        .prepare_cached(
            "SELECT id, blob, name, mime, size FROM draft_attachment
             WHERE draft = ?1 ORDER BY id",
        )?
        .query_map([&draft.id.0], |r| {
            Ok(DraftAttachment {
                id: DraftAttachmentId(r.get(0)?),
                blob: BlobId(r.get(1)?),
                name: r.get(2)?,
                mime: r.get(3)?,
                size: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(draft)
}

pub(crate) fn drafts(tx: &Transaction) -> Result<Vec<Draft>> {
    tx.prepare(&format!(
        "SELECT {DRAFT_COLUMNS} FROM draft ORDER BY updated_at DESC, id"
    ))?
    .query_map([], draft_row)?
    .map(|draft| with_attachments(tx, draft?))
    .collect()
}

pub(crate) fn draft(tx: &Transaction, id: &DraftId) -> Result<Option<Draft>> {
    tx.query_row(
        &format!("SELECT {DRAFT_COLUMNS} FROM draft WHERE id = ?1"),
        [&id.0],
        draft_row,
    )
    .optional()?
    .map(|draft| with_attachments(tx, draft))
    .transpose()
}

fn existing_draft(tx: &Transaction, id: &DraftId) -> Result<Draft> {
    draft(tx, id)?.ok_or_else(|| Conflict("the draft no longer exists".into()).into())
}

fn insert_draft(tx: &Transaction, draft: &Draft) -> Result<()> {
    let c = &draft.content;
    let inserted = tx.execute(
        &format!(
            "INSERT INTO draft ({DRAFT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (id) DO NOTHING"
        ),
        params![
            draft.id.0,
            draft.account,
            draft.revision,
            c.to,
            c.cc,
            c.bcc,
            c.subject,
            c.body,
            draft.in_reply_to.map(|m| m.0),
            draft.updated_at,
        ],
    )?;
    if inserted == 0 {
        bail!(Conflict(format!("draft {} exists already", draft.id.0)));
    }
    for a in &draft.attachments {
        insert_attachment(tx, &draft.id, Some(a.id), &a.blob, &a.name, &a.mime, a.size)?;
    }
    Ok(())
}

/// Inserts the attachment as `id`, or as a new one.
fn insert_attachment(
    tx: &Transaction,
    draft: &DraftId,
    id: Option<DraftAttachmentId>,
    blob: &BlobId,
    name: &str,
    mime: &str,
    size: i64,
) -> Result<DraftAttachmentId> {
    Ok(DraftAttachmentId(tx.query_row(
        "INSERT INTO draft_attachment (id, draft, blob, name, mime, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         RETURNING id",
        params![id.map(|id| id.0), draft.0, blob.0, name, mime, size],
        |r| r.get(0),
    )?))
}

/// Deletes the draft and its attachments.
fn delete_draft(tx: &Transaction, id: &DraftId) -> Result<Draft> {
    let draft = existing_draft(tx, id)?;
    tx.execute("DELETE FROM draft WHERE id = ?1", [&id.0])?;
    Ok(draft)
}

fn drafts_changed(id: &DraftId) -> ServerEvent {
    ServerEvent::DraftsChanged(DraftsChanged {
        drafts: vec![id.clone()],
    })
}

/// How a draft is named in descriptions.
fn title(content: &DraftContent) -> String {
    match content.subject.trim() {
        "" => "(no subject)".to_owned(),
        subject => format!("'{subject}'"),
    }
}

/// Creates a draft, or restores a deleted one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutDraft {
    pub id: DraftId,
    pub account: String,
    pub revision: i64,
    pub content: DraftContent,
    pub attachments: Vec<DraftAttachment>,
    pub in_reply_to: Option<MessageId>,
}

impl PutDraft {
    fn of(draft: Draft) -> Self {
        Self {
            id: draft.id,
            account: draft.account,
            revision: draft.revision,
            content: draft.content,
            attachments: draft.attachments,
            in_reply_to: draft.in_reply_to,
        }
    }
}

impl Op for PutDraft {
    const KIND: &'static str = "draft.put";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let description = format!("Saved draft {}", title(&self.content));
        let draft = Draft {
            id: self.id,
            account: self.account,
            revision: self.revision,
            content: self.content,
            attachments: self.attachments,
            in_reply_to: self.in_reply_to,
            updated_at: now_ms(),
        };
        insert_draft(tx, &draft)?;
        Ok(Applied {
            description,
            inverse: Inverse::of(&DeleteDraft {
                id: draft.id.clone(),
            }),
            events: vec![drafts_changed(&draft.id)],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteDraft {
    pub id: DraftId,
}

impl Op for DeleteDraft {
    const KIND: &'static str = "draft.delete";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let draft = delete_draft(tx, &self.id)?;
        Ok(Applied {
            description: format!("Discarded draft {}", title(&draft.content)),
            inverse: Inverse::of(&PutDraft::of(draft)),
            events: vec![drafts_changed(&self.id)],
        })
    }
}

/// Applies `splice` to a draft at `revision`. Inverses, which undo applies
/// to whatever revision the draft has reached, have no revision; they
/// check only that the text they remove is where they expect it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditDraft {
    pub id: DraftId,
    pub revision: Option<i64>,
    pub splice: Splice,
}

impl Op for EditDraft {
    const KIND: &'static str = "draft.edit";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let draft = existing_draft(tx, &self.id)?;
        if let Some(revision) = self.revision {
            if revision != draft.revision {
                bail!(Conflict(format!(
                    "the draft is at revision {}, not {revision}",
                    draft.revision
                )));
            }
        }
        let splice = &self.splice;
        let column = match splice.field {
            DraftField::To => "to_addrs",
            DraftField::Cc => "cc_addrs",
            DraftField::Bcc => "bcc_addrs",
            DraftField::Subject => "subject",
            DraftField::Body => "body_html",
        };
        let c = &draft.content;
        let text = match splice.field {
            DraftField::To => &c.to,
            DraftField::Cc => &c.cc,
            DraftField::Bcc => &c.bcc,
            DraftField::Subject => &c.subject,
            DraftField::Body => &c.body,
        };
        let edited = apply_splice(text, splice)
            .ok_or_else(|| Conflict("the draft's text changed".into()))?;
        let revision = draft.revision + 1;
        tx.execute(
            &format!(
                "UPDATE draft SET {column} = ?1, revision = ?2, updated_at = ?3 WHERE id = ?4"
            ),
            params![edited, revision, now_ms(), self.id.0],
        )?;
        let subject = match splice.field {
            DraftField::Subject => &edited,
            _ => &c.subject,
        };
        Ok(Applied {
            description: format!(
                "Edited draft {}",
                title(&DraftContent {
                    subject: subject.clone(),
                    ..c.clone()
                })
            ),
            inverse: Inverse::of(&EditDraft {
                id: self.id.clone(),
                revision: None,
                splice: Splice {
                    field: splice.field,
                    offset: splice.offset,
                    delete: splice.insert.clone(),
                    insert: splice.delete.clone(),
                },
            }),
            events: vec![ServerEvent::DraftEdited(DraftEdited {
                draft: self.id,
                revision,
                splice: self.splice,
            })],
        })
    }
}

/// Attaches a stored blob to a draft; with an `id`, as the attachment it
/// was before it was detached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachToDraft {
    pub draft: DraftId,
    pub id: Option<DraftAttachmentId>,
    pub blob: BlobId,
    pub name: String,
    pub mime: String,
    pub size: i64,
}

impl Op for AttachToDraft {
    const KIND: &'static str = "draft.attach";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let draft = existing_draft(tx, &self.draft)?;
        let id = insert_attachment(
            tx,
            &self.draft,
            self.id,
            &self.blob,
            &self.name,
            &self.mime,
            self.size,
        )?;
        touch(tx, &self.draft)?;
        Ok(Applied {
            description: format!(
                "Attached '{}' to draft {}",
                self.name,
                title(&draft.content)
            ),
            inverse: Inverse::of(&DetachFromDraft {
                draft: self.draft.clone(),
                attachment: id,
            }),
            events: vec![drafts_changed(&self.draft)],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachFromDraft {
    pub draft: DraftId,
    pub attachment: DraftAttachmentId,
}

impl Op for DetachFromDraft {
    const KIND: &'static str = "draft.detach";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let draft = existing_draft(tx, &self.draft)?;
        let attachment = draft
            .attachments
            .into_iter()
            .find(|a| a.id == self.attachment)
            .ok_or_else(|| Conflict("the file is no longer attached".into()))?;
        tx.execute(
            "DELETE FROM draft_attachment WHERE id = ?1",
            [self.attachment.0],
        )?;
        touch(tx, &self.draft)?;
        Ok(Applied {
            description: format!(
                "Removed '{}' from draft {}",
                attachment.name,
                title(&draft.content)
            ),
            inverse: Inverse::of(&AttachToDraft {
                draft: self.draft.clone(),
                id: Some(attachment.id),
                blob: attachment.blob,
                name: attachment.name,
                mime: attachment.mime,
                size: attachment.size,
            }),
            events: vec![drafts_changed(&self.draft)],
        })
    }
}

fn touch(tx: &Transaction, draft: &DraftId) -> Result<()> {
    tx.execute(
        "UPDATE draft SET updated_at = ?1 WHERE id = ?2",
        params![now_ms(), draft.0],
    )?;
    Ok(())
}

/// `text` with `splice` applied, if what it deletes is at its offset.
fn apply_splice(text: &str, splice: &Splice) -> Option<String> {
    let start = byte_offset(text, splice.offset)?;
    let end = start + splice.delete.len();
    if !text[start..].starts_with(&splice.delete) {
        return None;
    }
    Some([&text[..start], &splice.insert, &text[end..]].concat())
}

/// The byte offset of `units` UTF-16 code units into `text`; `None` past
/// the end or inside a surrogate pair.
fn byte_offset(text: &str, units: u32) -> Option<usize> {
    let units = units as usize;
    let mut counted = 0;
    for (i, c) in text.char_indices() {
        if counted == units {
            return Some(i);
        }
        counted += c.len_utf16();
        if counted > units {
            return None;
        }
    }
    (counted == units).then_some(text.len())
}

// ── sends ─────────────────────────────────────────────────────────────────
//
// A send is desired state (SPEC §6.7): wanted, or no longer, and what the
// provider made of it, observed. Cancelling a send the provider has not
// been handed (or refused) takes it back at once; one handed over is
// taken back only if the provider refuses it, and if it went out, it
// needs the user's decision: keeping it sent.

const SEND_COLUMNS: &str = "id, account_id, draft, state, wanted, error, not_before";

fn send_row(r: &Row) -> uplink_storage::rusqlite::Result<OutboxItem> {
    let draft: String = r.get(2)?;
    let state: String = r.get(3)?;
    Ok(OutboxItem {
        id: OutboxId(r.get(0)?),
        account: r.get(1)?,
        draft: serde_json::from_str(&draft).expect("sent drafts deserialize"),
        state: match state.as_str() {
            "queued" => SendState::Queued,
            "sending" => SendState::Sending,
            "unknown" => SendState::Unknown,
            "sent" => SendState::Sent,
            "refused" => SendState::Refused,
            _ => unreachable!("the schema allows no send state {state}"),
        },
        wanted: r.get(4)?,
        error: r.get(5)?,
        not_before: r.get(6)?,
    })
}

/// The sends not yet reconciled: not yet synced back, or no longer
/// wanted.
pub(crate) fn outbox(tx: &Transaction) -> Result<Vec<OutboxItem>> {
    Ok(tx
        .prepare(&format!(
            "SELECT {SEND_COLUMNS} FROM mail_send WHERE NOT synced OR NOT wanted
             ORDER BY not_before, id"
        ))?
        .query_map([], send_row)?
        .collect::<Result<_, _>>()?)
}

fn send(tx: &Transaction, id: OutboxId) -> Result<Option<OutboxItem>> {
    Ok(tx
        .query_row(
            &format!("SELECT {SEND_COLUMNS} FROM mail_send WHERE id = ?1"),
            [id.0],
            send_row,
        )
        .optional()?)
}

/// Queues a draft to be sent `delay_ms` from now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendDraft {
    pub id: DraftId,
    pub delay_ms: i64,
}

impl Op for SendDraft {
    const KIND: &'static str = "mail.send";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let this = this_entry(tx)?;
        let draft = delete_draft(tx, &self.id)?;
        let message_id = new_message_id(&draft.account);
        let item = OutboxId(tx.query_row(
            "INSERT INTO mail_send
               (account_id, draft, message_id, delay_ms, not_before, wanted, desired_by,
                state, synced)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 'queued', 0)
             RETURNING id",
            params![
                draft.account,
                serde_json::to_string(&draft)?,
                message_id,
                self.delay_ms,
                now_ms() + self.delay_ms,
                this.0,
            ],
            |r| r.get(0),
        )?);
        Ok(Applied {
            description: format!("Sent {}", title(&draft.content)),
            inverse: Inverse::of(&CancelSend { item }),
            events: vec![drafts_changed(&self.id), outbox_changed(&draft.account)],
        })
    }
}

/// A Message-ID for mail sent from the account: random, so unique.
fn new_message_id(account: &str) -> String {
    let random: [u8; 16] = rand::random();
    let random: String = random.iter().map(|b| format!("{b:02x}")).collect();
    format!("{random}@{account}.uplink")
}

/// Takes a send back: at once, restoring its draft, if the provider has
/// not been handed it or refused it; otherwise it is no longer wanted,
/// which the provider's refusal fulfils, and its going out makes a
/// decision for the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelSend {
    pub item: OutboxId,
}

impl Op for CancelSend {
    const KIND: &'static str = "mail.cancel_send";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let item = send(tx, self.item)?
            .ok_or_else(|| Conflict("the message was sent again since".into()))?;
        ensure!(
            item.wanted,
            Conflict("sending it is already being cancelled".into())
        );
        let title = title(&item.draft.content);
        match item.state {
            SendState::Queued | SendState::Refused => {
                let delay_ms = take_back(tx, &item)?;
                Ok(Applied {
                    description: format!("Cancelled sending {title}"),
                    inverse: Inverse::of(&SendDraft {
                        id: item.draft.id.clone(),
                        delay_ms,
                    }),
                    events: vec![
                        drafts_changed(&item.draft.id),
                        outbox_changed(&item.account),
                    ],
                })
            }
            SendState::Sending | SendState::Unknown | SendState::Sent => {
                set_wanted(tx, self.item, false)?;
                Ok(Applied {
                    description: format!("Cancelled sending {title}, which may have gone out"),
                    inverse: Inverse::of(&KeepSent { item: self.item }),
                    events: vec![outbox_changed(&item.account)],
                })
            }
        }
    }
}

/// Wants a send cancelled when it could no longer be taken back for sure
/// again: resolves one that went out regardless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeepSent {
    pub item: OutboxId,
}

impl Op for KeepSent {
    const KIND: &'static str = "mail.keep_sent";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let item = send(tx, self.item)?
            .filter(|i| !i.wanted)
            .ok_or_else(|| Conflict("sending it is no longer being cancelled".into()))?;
        set_wanted(tx, self.item, true)?;
        Ok(Applied {
            description: format!("Kept {} sent", title(&item.draft.content)),
            inverse: Inverse::of(&CancelSend { item: self.item }),
            events: vec![outbox_changed(&item.account)],
        })
    }
}

fn set_wanted(tx: &Transaction, item: OutboxId, wanted: bool) -> Result<()> {
    tx.execute(
        "UPDATE mail_send SET wanted = ?1, desired_by = ?2 WHERE id = ?3",
        params![wanted, this_entry(tx)?.0, item.0],
    )?;
    Ok(())
}

/// Takes back a send the provider does not have: deletes it, restoring
/// its draft; returns the send delay it was queued with.
fn take_back(tx: &Transaction, item: &OutboxItem) -> Result<i64> {
    let delay_ms = tx.query_row(
        "DELETE FROM mail_send WHERE id = ?1 RETURNING delay_ms",
        [item.id.0],
        |r| r.get(0),
    )?;
    insert_draft(tx, &item.draft)?;
    Ok(delay_ms)
}

/// A send due to be handed to the provider.
#[derive(Debug, Clone)]
pub struct Due {
    pub item: OutboxId,
    pub draft: Draft,
    pub message_id: String,
    /// When it was due (unix ms): it went out no earlier.
    pub not_before: i64,
    /// The provider's draft it is sent as, once made (SPEC §8.2, §8.4).
    pub provider_draft: Option<String>,
    /// Gmail: the thread of that draft.
    pub provider_thread: Option<String>,
}

/// The account's queued sends whose time has come, and when the next
/// one comes due.
pub(crate) fn due(
    conn: &uplink_storage::rusqlite::Connection,
    account: &str,
    now: i64,
) -> Result<(Vec<Due>, Option<i64>)> {
    let due = conn
        .prepare(
            "SELECT id, draft, message_id, not_before, provider_draft, provider_thread
             FROM mail_send
             WHERE account_id = ?1 AND state = 'queued' AND not_before <= ?2
             ORDER BY not_before, id",
        )?
        .query_map(params![account, now], |r| {
            let draft: String = r.get(1)?;
            Ok(Due {
                item: OutboxId(r.get(0)?),
                draft: serde_json::from_str(&draft).expect("sent drafts deserialize"),
                message_id: r.get(2)?,
                not_before: r.get(3)?,
                provider_draft: r.get(4)?,
                provider_thread: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let next = conn.query_row(
        "SELECT min(not_before) FROM mail_send
         WHERE account_id = ?1 AND state = 'queued' AND not_before > ?2",
        params![account, now],
        |r| r.get(0),
    )?;
    Ok((due, next))
}

/// Sends the account's sync task claimed but did not finish: it was
/// interrupted, and they may or may not have gone out.
pub(crate) fn interrupted(
    conn: &uplink_storage::rusqlite::Connection,
    account: &str,
) -> Result<Vec<OutboxId>> {
    Ok(conn
        .prepare("SELECT id FROM mail_send WHERE account_id = ?1 AND state = 'sending'")?
        .query_map([account], |r| Ok(OutboxId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// The account's sends whose outcome is unknown, attempted before `time`
/// (unix ms).
pub(crate) fn unknown_before(
    conn: &uplink_storage::rusqlite::Connection,
    account: &str,
    time: i64,
) -> Result<Vec<OutboxId>> {
    Ok(conn
        .prepare(
            "SELECT id FROM mail_send
             WHERE account_id = ?1 AND state = 'unknown' AND attempted_at < ?2",
        )?
        .query_map(params![account, time], |r| Ok(OutboxId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// The sync task is about to hand a send to the provider: from now on, a
/// cancel can no longer take it back at once. Refused with a [`Conflict`]
/// if it is no longer queued.
#[derive(Debug, Serialize)]
pub struct SendStarted {
    pub item: OutboxId,
}

impl Observation for SendStarted {
    const KIND: &'static str = "mail.send_started";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let item = send(tx, self.item)?
            .filter(|i| i.state == SendState::Queued)
            .ok_or_else(|| Conflict("no longer queued".into()))?;
        tx.execute(
            "UPDATE mail_send SET state = 'sending', attempted_at = ?1 WHERE id = ?2",
            params![now_ms(), self.item.0],
        )?;
        Ok(Observed {
            description: format!("Sending {}", title(&item.draft.content)),
            events: vec![outbox_changed(account)],
        })
    }
}

/// The provider made a send being sent into a draft of its own, which it
/// is then sent as, by its id: sending it again sends it once only
/// (SPEC §8.2, §8.4).
#[derive(Debug, Serialize)]
pub struct SendDrafted {
    pub item: OutboxId,
    pub provider_draft: String,
    /// Gmail: the draft's thread, where its copy is once sent.
    pub provider_thread: Option<String>,
}

impl Observation for SendDrafted {
    const KIND: &'static str = "mail.send_drafted";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let item = send(tx, self.item)?.context("the send is gone")?;
        ensure!(
            item.state == SendState::Sending,
            "{:?} is not being sent",
            self.item
        );
        tx.execute(
            "UPDATE mail_send SET provider_draft = ?1, provider_thread = ?2 WHERE id = ?3",
            params![self.provider_draft, self.provider_thread, self.item.0],
        )?;
        Ok(Observed {
            description: format!("Drafted {} at the provider", title(&item.draft.content)),
            events: vec![],
        })
    }
}

/// The provider accepted a send. The message as sent is now in the mail
/// store; recording it creates the logical message, which the provider's
/// copy joins once synced (SPEC §8.6).
#[derive(Debug, Serialize)]
pub struct SendSucceeded {
    pub item: OutboxId,
    pub variant: VariantHash,
    pub size_bytes: i64,
    #[serde(skip)]
    pub parsed: Parsed,
    pub copy: SentCopy,
}

/// Which of the provider's messages is its copy of a send it accepted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum SentCopy {
    /// The one with the send's Message-ID, which the provider keeps.
    OurMessageId,
    /// The provider's message of this id, under a Message-ID of the
    /// provider's own (Gmail, SPEC §8.2).
    Id(String),
    /// One under a Message-ID of the provider's own, but which cannot be
    /// told: the send is as synced as it can be.
    Untold,
}

impl Observation for SendSucceeded {
    const KIND: &'static str = "mail.send_succeeded";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let item = send(tx, self.item)?.context("the send is gone")?;
        ensure!(
            item.state == SendState::Sending,
            "{:?} is not being sent",
            self.item
        );
        let known: Option<i64> = tx
            .query_row(
                "SELECT message FROM message_variant WHERE sha256 = ?1",
                [&self.variant.0],
                |r| r.get(0),
            )
            .optional()?;
        let message = match known {
            Some(message) => MessageId(message),
            None => insert_variant(tx, account, &self.variant, self.size_bytes, &self.parsed)?,
        };
        let (provider_copy, synced) = match &self.copy {
            SentCopy::OurMessageId => (None, false),
            SentCopy::Id(id) => (Some(id), false),
            SentCopy::Untold => (None, true),
        };
        tx.execute(
            "UPDATE mail_send SET state = 'sent', message = ?1, provider_copy = ?2, synced = ?3
             WHERE id = ?4",
            params![message.0, provider_copy, synced, self.item.0],
        )?;
        // The provider's copy may have been synced already.
        sends_synced(tx, account)?;
        let title = title(&item.draft.content);
        Ok(Observed {
            description: if item.wanted {
                format!("Sent {title}")
            } else {
                format!("Sent {title} before the cancel could take it back")
            },
            events: vec![outbox_changed(account)],
        })
    }
}

/// What came of handing a send to the provider, if not its acceptance.
#[derive(Debug, Serialize)]
pub enum SendOutcome {
    /// Not sent, for a passing reason: to be tried again.
    NotSent,
    /// Refused by the provider, or unbuildable, for this reason.
    Refused(String),
    /// The provider may or may not have taken it, as this says.
    Unknown(String),
}

/// Handing a send to the provider did not get it accepted. A send no
/// longer wanted that did not go out is taken back.
#[derive(Debug, Serialize)]
pub struct SendFailed {
    pub item: OutboxId,
    pub outcome: SendOutcome,
}

impl Observation for SendFailed {
    const KIND: &'static str = "mail.send_failed";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let item = send(tx, self.item)?.context("the send is gone")?;
        ensure!(
            item.state == SendState::Sending,
            "{:?} is not being sent",
            self.item
        );
        let title = title(&item.draft.content);
        let mut events = vec![outbox_changed(account)];
        let description = match self.outcome {
            SendOutcome::Unknown(error) => {
                set_state(tx, self.item, "unknown", Some(&error))?;
                format!("Sending {title} may have failed: {error}")
            }
            SendOutcome::NotSent | SendOutcome::Refused(_) if !item.wanted => {
                take_back(tx, &item)?;
                events.push(drafts_changed(&item.draft.id));
                format!("Took back {title}: it did not go out")
            }
            SendOutcome::NotSent => {
                set_state(tx, self.item, "queued", None)?;
                format!("Failed to send {title}; will retry")
            }
            SendOutcome::Refused(error) => {
                set_state(tx, self.item, "refused", Some(&error))?;
                format!("Failed to send {title}: {error}")
            }
        };
        Ok(Observed {
            description,
            events,
        })
    }
}

/// A send whose outcome was unknown did not go out: a sync begun since
/// the attempt would have seen the provider's copy. It is sent again, or,
/// no longer wanted, taken back.
#[derive(Debug, Serialize)]
pub struct SendNotFound {
    pub item: OutboxId,
}

impl Observation for SendNotFound {
    const KIND: &'static str = "mail.send_not_found";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let item = send(tx, self.item)?.context("the send is gone")?;
        ensure!(
            item.state == SendState::Unknown,
            "{:?}'s outcome is known",
            self.item
        );
        let title = title(&item.draft.content);
        let mut events = vec![outbox_changed(account)];
        let description = if item.wanted {
            set_state(tx, self.item, "queued", None)?;
            format!("{} did not go out; sending it again", capitalize(&title))
        } else {
            take_back(tx, &item)?;
            events.push(drafts_changed(&item.draft.id));
            format!("Took back {title}: it did not go out")
        };
        Ok(Observed {
            description,
            events,
        })
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn set_state(tx: &Transaction, item: OutboxId, state: &str, error: Option<&str>) -> Result<()> {
    tx.execute(
        "UPDATE mail_send SET state = ?1, error = ?2 WHERE id = ?3",
        params![state, error, item.0],
    )?;
    Ok(())
}

/// Records the account's sends whose copy the provider has as synced:
/// sent, whatever was known of them, and reconciled if wanted. A copy
/// under the send's Message-ID, in Sent, is a variant of the send's
/// message; one the provider told is of its own Message-ID, so the send's
/// message joins it (SPEC §8.6). Returns whether there were any.
pub(crate) fn sends_synced(tx: &Transaction, account: &str) -> Result<bool> {
    let told: Vec<(OutboxId, MessageId, MessageId)> = tx
        .prepare(
            "SELECT s.id, s.message, e.message FROM mail_send s
             JOIN mail_entry e ON e.account_id = s.account_id AND e.external_id = s.provider_copy
             WHERE s.account_id = ?1 AND NOT s.synced",
        )?
        .query_map([account], |r| {
            Ok((
                OutboxId(r.get(0)?),
                MessageId(r.get(1)?),
                MessageId(r.get(2)?),
            ))
        })?
        .collect::<Result<_, _>>()?;
    for &(item, sent, copy) in &told {
        if sent != copy {
            fold_message(tx, sent, copy)?;
        }
        tx.execute("UPDATE mail_send SET synced = 1 WHERE id = ?1", [item.0])?;
    }
    let synced = tx.execute(
        "UPDATE mail_send
         SET state = 'sent', error = NULL, synced = 1,
             message = (SELECT m.id FROM message m WHERE m.message_id = mail_send.message_id)
         WHERE account_id = ?1 AND NOT synced AND state IN ('unknown', 'sent')
           AND EXISTS (
             SELECT 1 FROM message m
             JOIN mail_entry e ON e.message = m.id AND e.account_id = ?1
             JOIN entry_label el ON el.entry = e.id AND el.observed
             JOIN label l ON l.id = el.label AND l.external_id = 'SENT'
             WHERE m.message_id = mail_send.message_id)",
        [account],
    )?;
    Ok(!told.is_empty() || synced > 0)
}

/// Makes the logical message `from` part of `into`: its variants, and
/// all that refers to it.
fn fold_message(tx: &Transaction, from: MessageId, into: MessageId) -> Result<()> {
    for (table, column) in [
        ("message_variant", "message"),
        ("mail_entry", "message"),
        ("mail_send", "message"),
        ("todo", "message"),
        ("draft", "in_reply_to"),
    ] {
        tx.execute(
            &format!("UPDATE {table} SET {column} = ?1 WHERE {column} = ?2"),
            [into.0, from.0],
        )?;
    }
    tx.execute("DELETE FROM message_fts WHERE rowid = ?1", [from.0])?;
    tx.execute("DELETE FROM message WHERE id = ?1", [from.0])?;
    Ok(())
}

/// The account's messages that the provider told are copies of sends.
pub(crate) fn told_copies(
    conn: &uplink_storage::rusqlite::Connection,
    account: &str,
) -> Result<HashSet<String>> {
    Ok(conn
        .prepare("SELECT provider_copy FROM mail_send WHERE account_id = ?1 AND provider_copy IS NOT NULL")?
        .query_map([account], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

// ── outgoing messages ─────────────────────────────────────────────────────

/// Parses an address list as typed: comma-separated, each `addr` or
/// `Name <addr>`.
pub fn parse_addresses(list: &str) -> Result<Vec<Recipient>> {
    if list.trim().is_empty() {
        return Ok(vec![]);
    }
    let header = format!("To: {}\r\n\r\n", list.replace(['\r', '\n'], " "));
    let message = MessageParser::new()
        .with_address_headers()
        .parse(header.as_bytes())
        .context("unparseable addresses")?;
    let mut addresses = vec![];
    for address in message.to().into_iter().flat_map(|a| a.iter()) {
        let addr = address.address().unwrap_or_default();
        let at = addr.find('@');
        ensure!(
            at.is_some_and(|at| at > 0 && at + 1 < addr.len())
                && !addr.contains(char::is_whitespace),
            "not an address: {}",
            address.name().unwrap_or(addr)
        );
        addresses.push((address.name().map(str::to_owned), addr.to_owned()));
    }
    Ok(addresses)
}

type Recipient = (Option<String>, String);

/// The draft's To, Cc and Bcc recipients; fails unless there is one.
fn recipients(draft: &DraftContent) -> Result<[Vec<Recipient>; 3]> {
    let lists = [
        parse_addresses(&draft.to).context("To")?,
        parse_addresses(&draft.cc).context("Cc")?,
        parse_addresses(&draft.bcc).context("Bcc")?,
    ];
    ensure!(
        lists.iter().any(|l| !l.is_empty()),
        "the message has no recipients"
    );
    Ok(lists)
}

/// Fails, saying why, if `draft` could not be sent.
pub fn check_sendable(draft: &DraftContent) -> Result<()> {
    recipients(draft).map(drop)
}

/// What a reply needs from the message it replies to.
#[derive(Debug, Clone, Default)]
pub struct ReplyContext {
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

impl ReplyContext {
    /// For a reply to the message `raw` with Message-ID `message_id`.
    pub fn of(message_id: &str, raw: &[u8]) -> Self {
        let mut references: Vec<String> = MessageParser::new()
            .with_message_ids()
            .parse_headers(raw)
            .and_then(|m| {
                m.header(HeaderName::References).map(|v| {
                    v.as_text_list()
                        .map(|l| l.iter().map(|r| r.to_string()).collect())
                        .unwrap_or_default()
                })
            })
            .unwrap_or_default();
        references.push(message_id.to_owned());
        Self {
            in_reply_to: Some(message_id.to_owned()),
            references,
        }
    }
}

/// A file attached to an outgoing message.
#[derive(Debug, Clone)]
pub struct File {
    pub name: String,
    pub mime: String,
    pub content: Vec<u8>,
}

impl File {
    /// The draft's attachments, read from `blobs`.
    pub fn read_all(blobs: &Store, attachments: &[DraftAttachment]) -> Result<Vec<File>> {
        attachments
            .iter()
            .map(|a| {
                Ok(File {
                    name: a.name.clone(),
                    mime: a.mime.clone(),
                    content: blobs
                        .get(&VariantHash(a.blob.0.clone()))
                        .with_context(|| format!("attachment '{}'", a.name))?,
                })
            })
            .collect()
    }
}

/// The RFC 5322 message for `draft`, with `files` attached, from `from`,
/// dated `date` (unix seconds). The HTML is sanitized, and a plain-text
/// part derived from it.
pub fn build_message(
    from: &str,
    draft: &DraftContent,
    files: Vec<File>,
    message_id: &str,
    reply: &ReplyContext,
    date: i64,
) -> Result<Vec<u8>> {
    let [to, cc, bcc] = recipients(draft)?.map(|list| {
        list.into_iter()
            .map(|(name, addr)| Address::new_address(name, addr))
            .collect::<Vec<_>>()
    });
    let html = ammonia::clean(&draft.body);
    let text = mail_parser::decoders::html::html_to_text(&html);
    let mut message = MessageBuilder::new()
        .from(Address::new_address(None::<&str>, from.to_owned()))
        .subject(draft.subject.clone())
        .message_id(message_id.to_owned())
        .date(date)
        .html_body(html)
        .text_body(text);
    if !to.is_empty() {
        message = message.to(Address::new_list(to));
    }
    if !cc.is_empty() {
        message = message.cc(Address::new_list(cc));
    }
    if !bcc.is_empty() {
        message = message.bcc(Address::new_list(bcc));
    }
    for file in files {
        message = message.attachment(file.mime, file.name, file.content);
    }
    if let Some(in_reply_to) = &reply.in_reply_to {
        message = message
            .in_reply_to(in_reply_to.clone())
            .references(MessageIdHeader::new_list(reply.references.iter().cloned()));
    }
    Ok(message.write_to_vec()?)
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis()
        .try_into()
        .expect("timestamp fits in i64")
}

#[cfg(test)]
mod tests;
