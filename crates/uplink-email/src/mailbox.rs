//! Mailbox state in the database, and the log entries that change it:
//! observations recorded by sync, and the user's label edits (SPEC §6.6).
//!
//! Each label of an entry is a field reconciled by the model (SPEC §6.6,
//! [`Label`]): *desired* (what the user asked for, shown) and *observed*
//! (what the provider last reported). Ops want a value, recording which op
//! did (`desired_by`, for the outbox); observations record what the
//! provider has, and desired follows unless uplink has an opinion; the
//! account's reconciler claims pushes, records how each ended, and reads
//! back what it pushed.

use crate::{
    compose,
    parse::{self, Parsed},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uplink_protocol::{
    compose::OutboxChanged,
    mail::{
        EntriesAdded, EntriesRemoved, EntryId, LabelId, LabelKind, LabelListChanged, LabelState,
        LabelsChanged, MessageId,
    },
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{self, params, OptionalExtension, Row, Transaction};
use uplink_storage::store::{Store, VariantHash};
use uplink_tx_log::{
    reconcile::{Attempt, Decision, Ended, Field},
    this_entry, Applied, Conflict, Inverse, Observation, Observed, Op, Registry,
};

/// Bytes of body text indexed for search (SPEC §8.7).
const INDEXED_BODY_BYTES: usize = 64 * 1024;

/// Records which messages have attachments, of those stored before it was
/// recorded as they were (V0014), reading their preferred variants from
/// `store`. Run on the writer before the dispatcher takes it, as a
/// migration that needs the files; returns how many it indexed.
pub fn index_attachments(
    conn: &mut uplink_storage::rusqlite::Connection,
    store: &Store,
) -> Result<usize> {
    let pending: Vec<(i64, String)> = conn
        .prepare("SELECT id, preferred_variant FROM message WHERE has_attachments IS NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    if !pending.is_empty() {
        tracing::info!("indexing the attachments of {} messages", pending.len());
    }
    for batch in pending.chunks(500) {
        let tx = conn.transaction()?;
        for (id, variant) in batch {
            let hash = VariantHash::parse(variant)
                .with_context(|| format!("message {id} has variant {variant}"))?;
            let raw = store.get(&hash)?;
            tx.execute(
                "UPDATE message SET has_attachments = ?1 WHERE id = ?2",
                params![parse::has_attachments(&raw), id],
            )?;
        }
        tx.commit()?;
    }
    Ok(pending.len())
}

pub fn register(registry: &mut Registry) {
    registry.register::<SetLabels>();
}

// ── observations ──────────────────────────────────────────────────────────

/// The state of part of an account's mailbox, as just fetched from the
/// provider. Recorded by the account's sync task, which is the only writer
/// of the account's entries.
#[derive(Debug, Serialize)]
pub struct MailboxObserved {
    /// `account.kind`, for an account observed for the first time.
    pub account_kind: &'static str,
    /// All of the account's labels, when they were listed. Required before
    /// entries may carry a label not seen before.
    pub labels: Option<Vec<ObservedLabel>>,
    /// Known entries the provider now knows by another id, as an IMAP
    /// server does a message moved to another folder (SPEC §8.10). Applied
    /// first: `updated` and `removed` name entries by their new ids.
    pub rekeyed: Vec<Rekeyed>,
    /// Entries fetched for the first time.
    pub added: Vec<NewEntry>,
    /// Current labels of known entries.
    pub updated: Vec<EntryLabels>,
    /// Entries no longer in the account, by external id.
    pub removed: Vec<String>,
    /// Replaces the account's sync state.
    pub sync_state: Option<SyncState>,
    /// The log's last entry as the first request of what this records was
    /// sent (SPEC §6.6.2).
    pub as_of: TxLogId,
}

/// An account's sync state: the provider's own, and whether it describes
/// a mirror of the mailbox.
#[derive(Debug, Serialize)]
pub enum SyncState {
    /// A listing of the mailbox is under way, as far as `state` describes;
    /// the account is not mirrored until it is done. `total`: how many
    /// messages the mailbox holds, if the provider says.
    Listing {
        state: serde_json::Value,
        total: Option<u32>,
    },
    /// Everything up to the point this describes is recorded.
    Synced(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedLabel {
    pub external_id: String,
    pub name: String,
    pub system: bool,
    pub kind: LabelKind,
}

/// `label.kind`'s value of `kind`.
pub(crate) fn kind_sql(kind: LabelKind) -> &'static str {
    match kind {
        LabelKind::Tag => "tag",
        LabelKind::Mailbox => "mailbox",
        LabelKind::Folder => "folder",
    }
}

/// The label kind in column `i` of `row`, of `label.kind`.
pub(crate) fn kind_at(row: &Row, i: usize) -> rusqlite::Result<LabelKind> {
    let kind: String = row.get(i)?;
    match kind.as_str() {
        "tag" => Ok(LabelKind::Tag),
        "mailbox" => Ok(LabelKind::Mailbox),
        "folder" => Ok(LabelKind::Folder),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            i,
            rusqlite::types::Type::Text,
            format!("no label kind {kind}").into(),
        )),
    }
}

#[derive(Debug, Serialize)]
pub struct NewEntry {
    pub external_id: String,
    pub thread_id: Option<String>,
    /// Unix ms.
    pub received_at: i64,
    /// External label ids.
    pub labels: Vec<String>,
    /// The raw message, already in the mail store.
    pub variant: VariantHash,
    pub size_bytes: i64,
    /// Parsed from the raw message; not recorded, as it is derived.
    #[serde(skip)]
    pub parsed: Parsed,
}

#[derive(Debug, Serialize)]
pub struct Rekeyed {
    /// The entry's external id until now.
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize)]
pub struct EntryLabels {
    pub external_id: String,
    /// External label ids.
    pub labels: Vec<String>,
}

impl Observation for MailboxObserved {
    const KIND: &'static str = "mail.observed";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        tx.execute(
            // Its mail is news from now on (SPEC §12.15).
            "INSERT INTO account (id, kind, news_since)
             VALUES (?1, ?2, CAST(unixepoch('subsec') * 1000 AS INTEGER))
             ON CONFLICT (id) DO NOTHING",
            [account, self.account_kind],
        )?;
        let kind: String =
            tx.query_row("SELECT kind FROM account WHERE id = ?1", [account], |r| {
                r.get(0)
            })?;
        ensure!(
            kind == self.account_kind,
            "account {account} is recorded as {kind}, not {}",
            self.account_kind
        );
        let labels = Labels::of(tx, account)?;
        // One task observes and pushes in turn: what it observes was asked
        // for after the pushes it recorded (see `account.labels_known_at`).
        ensure!(
            self.as_of >= labels.known_at,
            "observed as of {:?}, before {:?}",
            self.as_of,
            labels.known_at
        );
        let at = labels.at;

        let mut events = vec![];
        let mut description = vec![];

        if let Some(labels) = self.labels {
            if replace_labels(tx, account, labels)? {
                events.push(ServerEvent::LabelListChanged(LabelListChanged {
                    account: account.to_owned(),
                }));
            }
        }
        let label_ids = label_ids(tx, account)?;
        let resolve = |external: &[String]| -> Result<BTreeSet<LabelId>> {
            external
                .iter()
                .map(|l| {
                    label_ids
                        .get(l)
                        .copied()
                        .with_context(|| format!("label {l} of account {account} is not known"))
                })
                .collect()
        };

        for rekeyed in &self.rekeyed {
            let changed = tx.execute(
                "UPDATE mail_entry SET external_id = ?1 WHERE account_id = ?2 AND external_id = ?3",
                [&rekeyed.to, account, &rekeyed.from],
            )?;
            ensure!(
                changed == 1,
                "entry {} of account {account} is not known",
                rekeyed.from
            );
        }

        let mut label_changes = vec![];

        let mut added = vec![];
        for new in self.added {
            let resolved_labels = resolve(&new.labels)?;
            let entry = insert_entry(tx, account, new)?;
            for label in resolved_labels {
                labels.store(entry, label, &Field::new(true, at))?;
                label_changes.push(LabelState {
                    entry,
                    label,
                    desired: true,
                    observed: true,
                });
            }
            added.push(entry);
        }
        if !added.is_empty() {
            description.push(format!("received {}", messages(added.len())));
            events.push(ServerEvent::EntriesAdded(EntriesAdded { entries: added }));
        }

        let mut relabelled = 0;
        // Whether what was pending changed: reconciled, or gone with its
        // entry.
        let mut reconciled = false;
        for update in self.updated {
            let entry = entry_id(tx, account, &update.external_id)?;
            let observed = resolve(&update.labels)?;
            let (changes, was_pending) = labels.observe(entry, &observed, self.as_of, at)?;
            relabelled += usize::from(!changes.is_empty());
            reconciled |= was_pending;
            label_changes.extend(changes);
        }
        if relabelled > 0 {
            description.push(format!("labels changed on {}", messages(relabelled)));
        }
        if !label_changes.is_empty() {
            events.push(ServerEvent::LabelsChanged(LabelsChanged {
                changes: label_changes,
            }));
        }

        let mut removed = vec![];
        for external_id in &self.removed {
            let entry = entry_id(tx, account, external_id)?;
            reconciled |= tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM entry_label
                                WHERE entry = ?1 AND desired_by IS NOT NULL)",
                [entry.0],
                |r| r.get::<_, bool>(0),
            )?;
            tx.execute("DELETE FROM mail_entry WHERE id = ?1", [entry.0])?;
            removed.push(entry);
        }
        if !removed.is_empty() {
            description.push(format!("{} removed", messages(removed.len())));
            events.push(ServerEvent::EntriesRemoved(EntriesRemoved {
                entries: removed,
            }));
        }
        // Mail sent from the account may have been synced back.
        reconciled |= compose::sends_synced(tx, account)?;
        if reconciled {
            events.push(outbox_changed(account));
        }

        if let Some(state) = self.sync_state {
            let (state, synced, total) = match state {
                SyncState::Listing { state, total } => (state, false, total),
                SyncState::Synced(state) => (state, true, None),
            };
            tx.execute(
                "UPDATE account SET sync_state = ?1, synced = ?2, listing_total = ?3
                 WHERE id = ?4",
                params![state.to_string(), synced, total, account],
            )?;
        }

        Ok(Observed {
            description: capitalize(&if description.is_empty() {
                "synced".to_owned()
            } else {
                description.join(", ")
            }),
            events,
        })
    }
}

/// The labels whose pushes the account's reconciler claims, in one entry
/// (SPEC §6.6.3): those still wanted of `changes`.
#[derive(Debug, Serialize)]
pub struct LabelsClaimed {
    pub changes: Vec<(EntryId, LabelId)>,
}

impl Observation for LabelsClaimed {
    const KIND: &'static str = "mail.claimed";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let labels = Labels::of(tx, account)?;
        let mut entries = BTreeSet::new();
        for (entry, label) in self.changes {
            let mut field = labels.load(entry, label)?;
            // Unless an op settled it meanwhile.
            if field.wanted().is_some() {
                field.claim()?;
                labels.store(entry, label, &field)?;
                entries.insert(entry);
            }
        }
        Ok(Observed {
            description: format!("Pushing label changes on {}", messages(entries.len())),
            events: vec![],
        })
    }
}

/// How a push of label changes ended: each the value pushed (`desired`).
/// A change of an entry or label gone meanwhile went with it.
#[derive(Debug, Serialize)]
pub struct LabelsPushed {
    pub changes: Vec<LabelChange>,
    pub ended: Ended,
}

impl Observation for LabelsPushed {
    const KIND: &'static str = "mail.pushed";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let mut labels = Labels::of(tx, account)?;
        let at = labels.at;
        let mut entries = BTreeSet::new();
        let mut outbox = false;
        for change in &self.changes {
            if !labels.exists(change.entry, change.label)? {
                continue;
            }
            let mut field = labels.load(change.entry, change.label)?;
            let before = pending_of(&field);
            field.ended(self.ended.clone(), at)?;
            outbox |= pending_of(&field) != before;
            labels.store(change.entry, change.label, &field)?;
            entries.insert(change.entry);
        }
        if self.ended == Ended::Applied {
            labels.known(at)?;
        }
        let n = messages(entries.len());
        Ok(Observed {
            description: match &self.ended {
                Ended::Applied => format!("Pushed label changes on {n}"),
                Ended::NotApplied {
                    refused: Some(reason),
                } => format!("Refused label changes on {n}: {reason}"),
                Ended::NotApplied { refused: None } => {
                    format!("Could not push label changes on {n}")
                }
                Ended::Unknown { .. } => {
                    format!("Whether label changes on {n} were pushed is unknown")
                }
            },
            events: if outbox {
                vec![outbox_changed(account)]
            } else {
                vec![]
            },
        })
    }
}

/// Pushes whose outcome became unknown, as of `now_ms`: those in flight,
/// as a push stopped without telling how they ended (`interrupted`), else
/// those so long enough to expire (SPEC §6.6.3).
#[derive(Debug, Serialize)]
pub struct LabelsSettled {
    pub now_ms: i64,
    pub interrupted: bool,
}

impl Observation for LabelsSettled {
    const KIND: &'static str = "mail.settled";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let mut labels = Labels::of(tx, account)?;
        let at = labels.at;
        let mut settled = 0;
        for stored in reconciling(tx, account)? {
            let mut field = labels.load(stored.entry, stored.label)?;
            let before = field.clone();
            if self.interrupted {
                field.interrupted(self.now_ms);
            } else if field.expiring(self.now_ms) {
                field.expire(self.now_ms, at)?;
            }
            if field != before {
                labels.store(stored.entry, stored.label, &field)?;
                settled += 1;
            }
        }
        if !self.interrupted && settled > 0 {
            labels.known(at)?;
        }
        Ok(Observed {
            description: if self.interrupted {
                format!("Whether {settled} label changes were pushed is unknown")
            } else {
                format!("Took {settled} label changes pushed or not for good")
            },
            events: vec![],
        })
    }
}

/// A label of an entry the account's reconciler has to do with: one
/// uplink has an opinion of, or a push of which it awaits.
#[derive(Debug, Clone)]
pub struct Reconciling {
    pub entry: EntryId,
    /// The entry's id at the provider.
    pub external_id: String,
    pub label: LabelId,
    pub label_external_id: String,
    pub kind: LabelKind,
    pub field: Label,
}

/// The account's labels its reconciler has to do with, by entry.
pub fn reconciling(conn: &rusqlite::Connection, account: &str) -> Result<Vec<Reconciling>> {
    let known_at = labels_known_at(conn, account)?;
    conn.prepare(
        "SELECT e.id, e.external_id, l.id, l.external_id, l.kind,
                el.desired, el.observed, el.desired_by, el.decision, el.push
         FROM entry_label el
         JOIN mail_entry e ON e.id = el.entry
         JOIN label l ON l.id = el.label
         WHERE e.account_id = ?1 AND (el.desired_by IS NOT NULL OR el.push IS NOT NULL)
         ORDER BY e.id, l.id",
    )?
    .query_map([account], |r| {
        Ok((
            Reconciling {
                entry: EntryId(r.get(0)?),
                external_id: r.get(1)?,
                label: LabelId(r.get(2)?),
                label_external_id: r.get(3)?,
                kind: kind_at(r, 4)?,
                field: Field::new(false, known_at),
            },
            Columns {
                desired: r.get(5)?,
                observed: r.get(6)?,
                desired_by: r.get(7)?,
                decision: r.get(8)?,
                push: r.get(9)?,
            },
        ))
    })?
    .map(|row| {
        let (mut reconciling, columns) = row?;
        reconciling.field = columns.field(known_at)?;
        Ok(reconciling)
    })
    .collect()
}

/// The account's outbox changed (SPEC §6.6).
pub(crate) fn outbox_changed(account: &str) -> ServerEvent {
    ServerEvent::OutboxChanged(OutboxChanged {
        accounts: vec![account.to_owned()],
    })
}

/// Makes the account's labels `labels`; returns whether anything changed.
fn replace_labels(tx: &Transaction, account: &str, labels: Vec<ObservedLabel>) -> Result<bool> {
    let mut known: HashMap<String, ObservedLabel> = tx
        .prepare("SELECT external_id, name, system, kind FROM label WHERE account_id = ?1")?
        .query_map([account], |r| {
            Ok(ObservedLabel {
                external_id: r.get(0)?,
                name: r.get(1)?,
                system: r.get(2)?,
                kind: kind_at(r, 3)?,
            })
        })?
        .map(|l| l.map(|l| (l.external_id.clone(), l)))
        .collect::<Result<_, _>>()?;
    let mut changed = false;
    for label in labels {
        if known.remove(&label.external_id).as_ref() == Some(&label) {
            continue;
        }
        tx.execute(
            "INSERT INTO label (account_id, external_id, name, system, kind)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (account_id, external_id)
             DO UPDATE SET name = excluded.name, system = excluded.system,
                           kind = excluded.kind",
            params![
                account,
                label.external_id,
                label.name,
                label.system,
                kind_sql(label.kind)
            ],
        )?;
        changed = true;
    }
    for gone in known.into_keys() {
        tx.execute(
            "DELETE FROM label WHERE account_id = ?1 AND external_id = ?2",
            params![account, gone],
        )?;
        changed = true;
    }
    Ok(changed)
}

fn label_ids(tx: &Transaction, account: &str) -> Result<HashMap<String, LabelId>> {
    Ok(tx
        .prepare("SELECT external_id, id FROM label WHERE account_id = ?1")?
        .query_map([account], |r| Ok((r.get(0)?, LabelId(r.get(1)?))))?
        .collect::<Result<_, _>>()?)
}

fn entry_id(tx: &Transaction, account: &str, external_id: &str) -> Result<EntryId> {
    tx.query_row(
        "SELECT id FROM mail_entry WHERE account_id = ?1 AND external_id = ?2",
        [account, external_id],
        |r| Ok(EntryId(r.get(0)?)),
    )
    .optional()?
    .with_context(|| format!("entry {external_id} of account {account} is not known"))
}

fn insert_entry(tx: &Transaction, account: &str, new: NewEntry) -> Result<EntryId> {
    let known: Option<i64> = tx
        .query_row(
            "SELECT message FROM message_variant WHERE sha256 = ?1",
            [&new.variant.0],
            |r| r.get(0),
        )
        .optional()?;
    let message = match known {
        Some(message) => MessageId(message),
        None => insert_variant(tx, account, &new.variant, new.size_bytes, &new.parsed)?,
    };
    let id = tx
        .query_row(
            "INSERT INTO mail_entry
               (account_id, external_id, message, variant, thread_id, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             RETURNING id",
            params![
                account,
                new.external_id,
                message.0,
                new.variant.0,
                new.thread_id,
                new.received_at
            ],
            |r| r.get(0),
        )
        .with_context(|| format!("recording entry {} of account {account}", new.external_id))?;
    Ok(EntryId(id))
}

/// Records a variant new to uplink, first seen in account `source`;
/// returns the logical message it belongs to.
pub(crate) fn insert_variant(
    tx: &Transaction,
    source: &str,
    hash: &VariantHash,
    size_bytes: i64,
    parsed: &Parsed,
) -> Result<MessageId> {
    let message = message_for_variant(tx, hash, parsed)?;
    tx.execute(
        "INSERT INTO message_variant (sha256, message, source, size_bytes)
         VALUES (?1, ?2, ?3, ?4)",
        params![hash.0, message.0, source, size_bytes],
    )?;
    Ok(message)
}

/// The logical message a new variant belongs to, created if need be
/// (SPEC §4.4).
fn message_for_variant(tx: &Transaction, hash: &VariantHash, parsed: &Parsed) -> Result<MessageId> {
    if let Some(message_id) = &parsed.message_id {
        let existing: Option<(i64, Option<i64>, String)> = tx
            .query_row(
                "SELECT id, date_sent, senders FROM message WHERE message_id = ?1",
                [message_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match existing {
            None => return insert_message(tx, message_id, hash, parsed),
            Some((id, date_sent, senders)) => {
                let senders: Vec<String> = serde_json::from_str(&senders)?;
                if date_sent == parsed.date_sent
                    && senders.iter().any(|s| parsed.senders.contains(s))
                {
                    return Ok(MessageId(id));
                }
                tracing::warn!(
                    message_id,
                    variant = hash.0,
                    "Message-ID collision: date or sender differ; storing as a separate message"
                );
            }
        }
    }
    insert_message(
        tx,
        &format!("{}@variant.uplink.invalid", hash.0),
        hash,
        parsed,
    )
}

fn insert_message(
    tx: &Transaction,
    message_id: &str,
    hash: &VariantHash,
    parsed: &Parsed,
) -> Result<MessageId> {
    let id: i64 = tx.query_row(
        "INSERT INTO message
           (message_id, preferred_variant, subject, from_addr, from_name,
            to_addrs, cc_addrs, date_sent, senders, snippet, has_attachments)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         RETURNING id",
        params![
            message_id,
            hash.0,
            parsed.subject,
            parsed.from_addr,
            parsed.from_name,
            serde_json::to_string(&parsed.to_addrs)?,
            serde_json::to_string(&parsed.cc_addrs)?,
            parsed.date_sent,
            serde_json::to_string(&parsed.senders)?,
            parsed.snippet,
            parsed.has_attachments,
        ],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO message_fts (rowid, subject, from_addr, from_name, to_addrs, cc_addrs,
                                  body_text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            parsed.subject,
            parsed.from_addr,
            parsed.from_name,
            parsed.to_addrs.join(" "),
            parsed.cc_addrs.join(" "),
            parse::truncate(&parsed.body_text, INDEXED_BODY_BYTES),
        ],
    )?;
    Ok(MessageId(id))
}

/// A label of an entry (SPEC §6.6): had (`observed`), or asked for
/// (`desired`).
pub type Label = Field<bool>;

/// What of a [`Label`] its row's `push` holds: the rest is in columns.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Push {
    attempt: Option<Attempt<bool>>,
    straggler: Option<i64>,
    check: Option<TxLogId>,
    not_kept: u8,
}

/// An `entry_label` row's columns of its field.
struct Columns {
    desired: bool,
    observed: bool,
    desired_by: Option<i64>,
    decision: Option<String>,
    push: Option<String>,
}

impl Columns {
    /// The field, known as of the account's `known_at`.
    fn field(self, known_at: TxLogId) -> Result<Label> {
        let push: Push = match &self.push {
            Some(json) => {
                serde_json::from_str(json).with_context(|| format!("a label's push: {json}"))?
            }
            None => Push::default(),
        };
        let decision = match &self.decision {
            Some(json) => Some(
                serde_json::from_str(json)
                    .with_context(|| format!("a label's decision: {json}"))?,
            ),
            None => None,
        };
        Ok(Field {
            desired: self.desired,
            opinion: self.desired_by.map(TxLogId),
            observed: self.observed,
            known_at,
            attempt: push.attempt,
            straggler: push.straggler,
            check: push.check,
            not_kept: push.not_kept,
            decision,
        })
    }
}

/// When the account's labels are known as of: every field's `known_at`.
fn labels_known_at(conn: &rusqlite::Connection, account: &str) -> Result<TxLogId> {
    Ok(TxLogId(
        conn.query_row(
            "SELECT labels_known_at FROM account WHERE id = ?1",
            [account],
            |r| r.get(0),
        )
        .optional()?
        // Not recorded yet: nothing is known.
        .unwrap_or(0),
    ))
}

/// An account's labels in a transaction: their fields, stored as rows,
/// all known as of the account's `labels_known_at` (V0073): the last push
/// applied, or expired. Observations are not: one task observes, in the
/// order it fetched, and pushes in turn, so what it observes was fetched
/// after the observations before, and as fresh as those recorded with it.
/// Only this writes `entry_label`, so that rows are as fields are.
struct Labels<'a> {
    tx: &'a Transaction<'a>,
    account: String,
    known_at: TxLogId,
    /// The log entry the transaction records.
    at: TxLogId,
}

impl<'a> Labels<'a> {
    fn of(tx: &'a Transaction<'a>, account: &str) -> Result<Self> {
        Ok(Self {
            tx,
            account: account.to_owned(),
            known_at: labels_known_at(tx, account)?,
            at: this_entry(tx)?,
        })
    }

    /// Whether both the entry and the label still exist.
    fn exists(&self, entry: EntryId, label: LabelId) -> Result<bool> {
        Ok(self
            .tx
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM mail_entry WHERE id = ?1)
                AND EXISTS (SELECT 1 FROM label WHERE id = ?2)",
            )?
            .query_row([entry.0, label.0], |r| r.get(0))?)
    }

    /// The label of the entry; neither had nor asked for if it has no row.
    fn load(&self, entry: EntryId, label: LabelId) -> Result<Label> {
        let columns = self
            .tx
            .prepare_cached(
                "SELECT desired, observed, desired_by, decision, push FROM entry_label
                 WHERE entry = ?1 AND label = ?2",
            )?
            .query_row([entry.0, label.0], |r| {
                Ok(Columns {
                    desired: r.get(0)?,
                    observed: r.get(1)?,
                    desired_by: r.get(2)?,
                    decision: r.get(3)?,
                    push: r.get(4)?,
                })
            })
            .optional()?;
        match columns {
            Some(columns) => columns.field(self.known_at),
            None => Ok(Field::new(false, self.known_at)),
        }
    }

    /// Stores the label of the entry; its row goes once it is neither had
    /// nor asked for, nor awaited. Its `known_at` is the account's, which
    /// [`Self::known`] raises.
    fn store(&self, entry: EntryId, label: LabelId, field: &Label) -> Result<()> {
        ensure!(
            field.known_at <= self.at,
            "a label known as of later than now"
        );
        let push = Push {
            attempt: field.attempt.clone(),
            straggler: field.straggler,
            check: field.check,
            not_kept: field.not_kept,
        };
        let empty = push == Push::default();
        // Cached: an op may set many, and each statement's triggers are
        // compiled with it.
        if field.desired || field.observed || field.opinion.is_some() || !empty {
            self.tx
                .prepare_cached(
                    "INSERT INTO entry_label
                       (entry, label, desired, observed, desired_by, decision, push)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (entry, label)
                     DO UPDATE SET desired = excluded.desired, observed = excluded.observed,
                       desired_by = excluded.desired_by, decision = excluded.decision,
                       push = excluded.push",
                )?
                .execute(params![
                    entry.0,
                    label.0,
                    field.desired,
                    field.observed,
                    field.opinion.map(|o| o.0),
                    field
                        .decision
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    (!empty).then(|| serde_json::to_string(&push)).transpose()?,
                ])?;
        } else {
            self.tx
                .prepare_cached("DELETE FROM entry_label WHERE entry = ?1 AND label = ?2")?
                .execute([entry.0, label.0])?;
        }
        Ok(())
    }

    /// The account's labels are known as of `at`, this entry, recording a
    /// push applied or expired: as every field it set `known_at` of was,
    /// and conservatively the rest.
    fn known(&mut self, at: TxLogId) -> Result<()> {
        if at > self.known_at {
            self.tx.execute(
                "UPDATE account SET labels_known_at = ?2 WHERE id = ?1",
                params![self.account, at.0],
            )?;
            self.known_at = at;
        }
        Ok(())
    }

    /// Records that the provider reports exactly `observed` on `entry`, by
    /// a request sent after `as_of`, recorded at `at`; and whether what is
    /// pending changed. A label without an opinion or a read-back asked
    /// for that was observed so already is as it was: its row is not
    /// written.
    fn observe(
        &self,
        entry: EntryId,
        observed: &BTreeSet<LabelId>,
        as_of: TxLogId,
        at: TxLogId,
    ) -> Result<(Vec<LabelState>, bool)> {
        let mut rows: BTreeMap<LabelId, Label> = BTreeMap::new();
        let stored: Vec<(LabelId, Columns)> = self
            .tx
            .prepare_cached(
                "SELECT label, desired, observed, desired_by, decision, push FROM entry_label
                 WHERE entry = ?1",
            )?
            .query_map([entry.0], |r| {
                Ok((
                    LabelId(r.get(0)?),
                    Columns {
                        desired: r.get(1)?,
                        observed: r.get(2)?,
                        desired_by: r.get(3)?,
                        decision: r.get(4)?,
                        push: r.get(5)?,
                    },
                ))
            })?
            .collect::<Result<_, _>>()?;
        for (label, columns) in stored {
            rows.insert(label, columns.field(self.known_at)?);
        }
        for &label in observed {
            rows.entry(label)
                .or_insert_with(|| Field::new(false, self.known_at));
        }
        let mut changes = vec![];
        let mut was_pending = false;
        for (label, mut field) in rows {
            let now_observed = observed.contains(&label);
            let settled = field.opinion.is_none() && field.check.is_none();
            if settled && now_observed == field.observed {
                continue;
            }
            let before = (field.desired, field.observed, pending_of(&field));
            field.observe(now_observed, as_of, at)?;
            self.store(entry, label, &field)?;
            was_pending |= pending_of(&field) != before.2;
            if (field.desired, field.observed) != (before.0, before.1) {
                changes.push(LabelState {
                    entry,
                    label,
                    desired: field.desired,
                    observed: field.observed,
                });
            }
        }
        Ok((changes, was_pending))
    }
}

/// What of a label the outbox shows: the op asking, and its decision.
fn pending_of(field: &Label) -> (Option<TxLogId>, Option<Decision>) {
    (field.opinion, field.decision.clone())
}

/// The reason a label's decision (`entry_label.decision`, as JSON) gives.
pub(crate) fn decision_reason(json: &str) -> Result<String> {
    let decision: Decision =
        serde_json::from_str(json).with_context(|| format!("a label's decision: {json}"))?;
    Ok(decision.reason().to_owned())
}

// ── ops ───────────────────────────────────────────────────────────────────

/// Sets the desired membership of labels on entries. Changes that are
/// already in effect are skipped, so the inverse reverts exactly what this
/// op changed (SPEC §6.2).
///
/// Where an entry is, its account's folders and mailboxes say, if it has
/// them (SPEC §6.6): it is in one folder, or in any number of mailboxes.
/// Adding a folder takes the entry out of every other place, adding a
/// mailbox takes it out of the folders, and leaving its last place puts it
/// in the Archive. Those implied changes are part of the op, and of its
/// inverse, but not of its description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetLabels {
    pub changes: Vec<LabelChange>,
    /// By a view being shown (SPEC §12.15): opening a conversation reads
    /// it.
    #[serde(default)]
    pub shown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelChange {
    pub entry: EntryId,
    pub label: LabelId,
    pub desired: bool,
}

impl Op for SetLabels {
    const KIND: &'static str = "mail.set_labels";

    fn shown(&self) -> bool {
        self.shown
    }

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let this = this_entry(tx)?;
        let mut inverse = vec![];
        let mut events = vec![];
        let mut accounts = BTreeSet::new();
        // By change, the entries changed so, and their conversations.
        let mut groups: BTreeMap<(LabelId, bool), (usize, BTreeSet<Conversation>)> =
            BTreeMap::new();
        // Each entry's changes, the entries in the order first changed.
        let mut by_entry: Vec<(EntryId, Vec<LabelChange>)> = vec![];
        let mut at: HashMap<EntryId, usize> = HashMap::new();
        for &change in &self.changes {
            let i = *at.entry(change.entry).or_insert_with(|| {
                by_entry.push((change.entry, vec![]));
                by_entry.len() - 1
            });
            by_entry[i].1.push(change);
        }
        let mut changes = vec![];
        let mut labels_of: BTreeMap<String, Labels> = BTreeMap::new();
        for (entry, requested) in by_entry {
            let implied = implied_by_places(tx, entry, &requested)?;
            // Moving is leaving what the place moved to implies leaving:
            // described as one.
            let kinds = requested
                .iter()
                .map(|c| label_kind(tx, c.label))
                .collect::<Result<Vec<_>>>()?;
            let added: Vec<LabelKind> = requested
                .iter()
                .zip(&kinds)
                .filter(|(c, _)| c.desired)
                .filter_map(|(_, &kind)| kind.filter(|&k| k != LabelKind::Tag))
                .collect();
            for (c, kind) in requested.into_iter().zip(kinds) {
                let left = !c.desired
                    && kind.is_some_and(|k| {
                        k != LabelKind::Tag && added.iter().any(|&a| leaves(a, k))
                    });
                changes.push((c, left));
            }
            changes.extend(implied.into_iter().map(|c| (c, true)));
        }
        for (change, undescribed) in changes {
            let (entry_account, thread): (String, Option<String>) = tx
                .prepare_cached("SELECT account_id, thread_id FROM mail_entry WHERE id = ?1")?
                .query_row([change.entry.0], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or_else(|| Conflict("the message is no longer in its account".into()))?;
            let label_account: String = tx
                .prepare_cached("SELECT account_id FROM label WHERE id = ?1")?
                .query_row([change.label.0], |r| r.get(0))
                .optional()?
                .ok_or_else(|| Conflict("the label no longer exists".into()))?;
            ensure!(
                entry_account == label_account,
                "label {} is not of entry {}'s account",
                change.label.0,
                change.entry.0
            );
            let labels = match labels_of.entry(entry_account.clone()) {
                std::collections::btree_map::Entry::Occupied(of) => of.into_mut(),
                std::collections::btree_map::Entry::Vacant(of) => {
                    of.insert(Labels::of(tx, &entry_account)?)
                }
            };
            let mut field = labels.load(change.entry, change.label)?;
            let desired = field.desired;
            if desired == change.desired {
                continue;
            }
            field.want(change.desired, this);
            labels.store(change.entry, change.label, &field)?;
            let observed = field.observed;
            let conversation = match thread {
                Some(thread) => Conversation::Thread(entry_account.clone(), thread),
                None => Conversation::Alone(change.entry),
            };
            accounts.insert(entry_account);
            inverse.push(LabelChange { desired, ..change });
            if !undescribed {
                let (count, conversations) =
                    groups.entry((change.label, change.desired)).or_default();
                *count += 1;
                conversations.insert(conversation);
            }
            events.push(LabelState {
                entry: change.entry,
                label: change.label,
                desired: change.desired,
                observed,
            });
        }
        let mut description = vec![];
        for ((label, desired), (count, conversations)) in groups {
            let (external_id, name, kind): (String, String, LabelKind) = tx.query_row(
                "SELECT external_id, name, kind FROM label WHERE id = ?1",
                [label.0],
                |r| Ok((r.get(0)?, r.get(1)?, kind_at(r, 2)?)),
            )?;
            description.push(describe_label_change(
                &external_id,
                &name,
                kind == LabelKind::Folder,
                desired,
                count,
                conversations.len(),
            ));
        }
        Ok(Applied {
            description: if description.is_empty() {
                "Changed nothing".to_owned()
            } else {
                capitalize(&description.join(", "))
            },
            inverse: Inverse::of(&SetLabels {
                changes: inverse,
                shown: false,
            }),
            events: [ServerEvent::LabelsChanged(LabelsChanged {
                changes: events,
            })]
            .into_iter()
            .chain(accounts.iter().map(|a| outbox_changed(a)))
            .collect(),
        })
    }
}

/// The changes that keep `entry` in one folder or in mailboxes (see
/// [`SetLabels`]), given the `requested` ones; none if its account has
/// neither.
fn implied_by_places(
    tx: &Transaction,
    entry: EntryId,
    requested: &[LabelChange],
) -> Result<Vec<LabelChange>> {
    let account: Option<String> = tx
        .prepare_cached("SELECT account_id FROM mail_entry WHERE id = ?1")?
        .query_row([entry.0], |r| r.get(0))
        .optional()?;
    // Refused below, as the entry is gone.
    let Some(account) = account else {
        return Ok(vec![]);
    };
    let places: BTreeMap<LabelId, (String, LabelKind)> = tx
        .prepare_cached(
            "SELECT id, external_id, kind FROM label WHERE account_id = ?1 AND kind != 'tag'",
        )?
        .query_map([&account], |r| {
            Ok((LabelId(r.get(0)?), (r.get(1)?, kind_at(r, 2)?)))
        })?
        .collect::<Result<_, _>>()?;
    if places.is_empty() {
        return Ok(vec![]);
    }
    let mut desired: BTreeSet<LabelId> = tx
        .prepare_cached(
            "SELECT el.label FROM entry_label el JOIN label l ON l.id = el.label
             WHERE el.entry = ?1 AND el.desired AND l.kind != 'tag'",
        )?
        .query_map([entry.0], |r| Ok(LabelId(r.get(0)?)))?
        .collect::<Result<_, _>>()?;
    let mut added = vec![];
    for change in requested.iter().filter(|c| places.contains_key(&c.label)) {
        if change.desired {
            desired.insert(change.label);
            added.push(change.label);
        } else {
            desired.remove(&change.label);
        }
    }
    let kind = |label: &LabelId| places[label].1;
    match &added[..] {
        [] if desired.is_empty() => {
            let archive = places
                .iter()
                .find(|(_, (external_id, kind))| {
                    external_id == "ARCHIVE" && *kind == LabelKind::Folder
                })
                .map(|(&id, _)| id)
                .with_context(|| format!("account {account} has places, but no Archive"))?;
            Ok(vec![LabelChange {
                entry,
                label: archive,
                desired: true,
            }])
        }
        [_, _, ..] if added.iter().any(|a| kind(a) == LabelKind::Folder) => {
            bail!(Conflict("a message is in one folder at a time".into()))
        }
        added => Ok(desired
            .iter()
            .filter(|&l| !added.contains(l) && added.iter().any(|a| leaves(kind(a), kind(l))))
            .map(|&label| LabelChange {
                entry,
                label,
                desired: false,
            })
            .collect()),
    }
}

/// Whether an entry put in a place of kind `into` leaves one of kind
/// `other` (see [`SetLabels`]): a folder leaves every other place, a
/// mailbox the folders.
fn leaves(into: LabelKind, other: LabelKind) -> bool {
    into == LabelKind::Folder || other == LabelKind::Folder
}

/// The label's kind; none for a label gone, which is refused later.
fn label_kind(tx: &Transaction, label: LabelId) -> Result<Option<LabelKind>> {
    Ok(tx
        .prepare_cached("SELECT kind FROM label WHERE id = ?1")?
        .query_row([label.0], |r| kind_at(r, 0))
        .optional()?)
}

fn describe_label_change(
    external_id: &str,
    name: &str,
    folder: bool,
    desired: bool,
    count: usize,
    conversations: usize,
) -> String {
    let n = in_conversations(count, conversations);
    match (external_id, desired) {
        ("INBOX", false) => format!("archived {n}"),
        ("INBOX", true) => format!("moved {n} to Inbox"),
        ("UNREAD", false) => format!("marked {n} read"),
        ("UNREAD", true) => format!("marked {n} unread"),
        ("STARRED", true) => format!("starred {n}"),
        ("STARRED", false) => format!("unstarred {n}"),
        ("TRASH", true) => format!("moved {n} to Trash"),
        ("TRASH", false) => format!("restored {n} from Trash"),
        ("SPAM", true) => format!("reported {n} as spam"),
        ("SPAM", false) => format!("marked {n} not spam"),
        ("ARCHIVE", true) => format!("moved {n} to Archive"),
        (_, true) if folder => format!("moved {n} to '{name}'"),
        (_, false) if folder => format!("moved {n} out of '{name}'"),
        (_, true) => format!("labelled {n} '{name}'"),
        (_, false) => format!("removed label '{name}' from {n}"),
    }
}

/// A conversation the entries changed are of, as lists group them: an
/// account's thread, or an entry without one by itself.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Conversation {
    Thread(String, String),
    Alone(EntryId),
}

fn messages(count: usize) -> String {
    match count {
        1 => "1 message".to_owned(),
        n => format!("{n} messages"),
    }
}

/// The messages changed, and, unless each is of a conversation of its
/// own, the conversations they are of, as lists show conversations.
fn in_conversations(count: usize, conversations: usize) -> String {
    let messages = messages(count);
    match conversations {
        _ if conversations == count => messages,
        1 => format!("{messages} in 1 conversation"),
        n => format!("{messages} in {n} conversations"),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
