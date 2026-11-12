//! Mailbox state in the database, and the log entries that change it:
//! observations recorded by sync, and the user's label edits (SPEC §6.6).
//!
//! Label membership is stored twice per entry: *desired* (what the user
//! asked for) and *observed* (what the provider last reported). Ops change
//! desired, recording which op did while it differs (`desired_by`, for the
//! outbox); observations change observed, and desired follows unless a
//! local change is pending; the reconciler pushes differences and records
//! what it pushed, or what the provider refused.

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
        EntriesAdded, EntriesRemoved, EntryId, LabelId, LabelListChanged, LabelState,
        LabelsChanged, MessageId,
    },
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, OptionalExtension, Transaction};
use uplink_storage::store::{Store, VariantHash};
use uplink_tx_log::{this_entry, Applied, Conflict, Inverse, Observation, Observed, Op, Registry};

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
    /// One of the account's folders, of which an entry is in exactly one
    /// (SPEC §8.3).
    pub folder: bool,
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
            "INSERT INTO account (id, kind) VALUES (?1, ?2) ON CONFLICT (id) DO NOTHING",
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
            let labels = resolve(&new.labels)?;
            let entry = insert_entry(tx, account, new)?;
            for label in labels {
                set_label_row(tx, entry, label, true, true, None)?;
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
            let (changes, was_pending) = observe_labels(tx, entry, &observed)?;
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
                                WHERE entry = ?1 AND desired != observed)",
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

/// Label changes the reconciler has pushed to the provider, which now
/// reports them as observed.
#[derive(Debug, Serialize)]
pub struct LabelsPushed {
    pub changes: Vec<LabelChange>,
}

impl Observation for LabelsPushed {
    const KIND: &'static str = "mail.pushed";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let mut entries = BTreeSet::new();
        let mut changes = vec![];
        for change in self.changes {
            // Entries and labels removed meanwhile need no bookkeeping.
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM mail_entry WHERE id = ?1)
                    AND EXISTS (SELECT 1 FROM label WHERE id = ?2)",
                [change.entry.0, change.label.0],
                |r| r.get(0),
            )?;
            if !exists {
                continue;
            }
            // The user may have changed desired again since the push.
            let (desired, _) = label_row(tx, change.entry, change.label)?.unwrap_or_default();
            set_label_row(
                tx,
                change.entry,
                change.label,
                desired,
                change.desired,
                None,
            )?;
            entries.insert(change.entry);
            changes.push(LabelState {
                entry: change.entry,
                label: change.label,
                desired,
                observed: change.desired,
            });
        }
        Ok(Observed {
            description: format!("Pushed label changes on {}", messages(entries.len())),
            events: vec![
                ServerEvent::LabelsChanged(LabelsChanged { changes }),
                outbox_changed(account),
            ],
        })
    }
}

/// Label changes the provider refused, for `reason`: they are not pushed
/// again until the user decides (SPEC §6.6).
#[derive(Debug, Serialize)]
pub struct LabelsRefused {
    pub changes: Vec<LabelChange>,
    pub reason: String,
}

impl Observation for LabelsRefused {
    const KIND: &'static str = "mail.refused";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let mut entries = BTreeSet::new();
        for change in &self.changes {
            // Only as the push asked for it: the user may have changed it
            // since, or the entry may be gone.
            let refused = tx.execute(
                "UPDATE entry_label SET refused = ?1
                 WHERE entry = ?2 AND label = ?3 AND desired = ?4 AND desired != observed",
                params![self.reason, change.entry.0, change.label.0, change.desired],
            )?;
            if refused > 0 {
                entries.insert(change.entry);
            }
        }
        Ok(Observed {
            description: format!(
                "Refused label changes on {}: {}",
                messages(entries.len()),
                self.reason
            ),
            events: vec![outbox_changed(account)],
        })
    }
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
        .prepare("SELECT external_id, name, system, folder FROM label WHERE account_id = ?1")?
        .query_map([account], |r| {
            Ok(ObservedLabel {
                external_id: r.get(0)?,
                name: r.get(1)?,
                system: r.get(2)?,
                folder: r.get(3)?,
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
            "INSERT INTO label (account_id, external_id, name, system, folder)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (account_id, external_id)
             DO UPDATE SET name = excluded.name, system = excluded.system,
                           folder = excluded.folder",
            params![
                account,
                label.external_id,
                label.name,
                label.system,
                label.folder
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

/// Records that the provider reports exactly `observed` on `entry`; and
/// whether that changed a label pending a push.
fn observe_labels(
    tx: &Transaction,
    entry: EntryId,
    observed: &BTreeSet<LabelId>,
) -> Result<(Vec<LabelState>, bool)> {
    let mut rows: BTreeMap<LabelId, (bool, bool)> = tx
        .prepare("SELECT label, desired, observed FROM entry_label WHERE entry = ?1")?
        .query_map([entry.0], |r| {
            Ok((LabelId(r.get(0)?), (r.get(1)?, r.get(2)?)))
        })?
        .collect::<Result<_, _>>()?;
    for &label in observed {
        rows.entry(label).or_insert((false, false));
    }
    let mut changes = vec![];
    let mut was_pending = false;
    for (label, (desired, was_observed)) in rows {
        let now_observed = observed.contains(&label);
        if now_observed == was_observed {
            continue;
        }
        was_pending |= desired != was_observed;
        // Without a pending local change, the remote change wins; with one,
        // the local change stands and is pushed.
        let desired = if desired == was_observed {
            now_observed
        } else {
            desired
        };
        set_label_row(tx, entry, label, desired, now_observed, None)?;
        changes.push(LabelState {
            entry,
            label,
            desired,
            observed: now_observed,
        });
    }
    Ok((changes, was_pending))
}

fn label_row(tx: &Transaction, entry: EntryId, label: LabelId) -> Result<Option<(bool, bool)>> {
    Ok(tx
        .prepare_cached(
            "SELECT desired, observed FROM entry_label WHERE entry = ?1 AND label = ?2",
        )?
        .query_row([entry.0, label.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// Records `desired` and `observed`; `by` is the op setting `desired`, if
/// one is. A difference keeps the op that made it, and a refusal of it,
/// until an op sets `desired` again; once there is none, neither stays.
fn set_label_row(
    tx: &Transaction,
    entry: EntryId,
    label: LabelId,
    desired: bool,
    observed: bool,
    by: Option<TxLogId>,
) -> Result<()> {
    // Cached: an op may set many, and each statement's triggers are
    // compiled with it.
    if desired || observed {
        tx.prepare_cached(
            "INSERT INTO entry_label (entry, label, desired, observed, desired_by)
             VALUES (?1, ?2, ?3, ?4, CASE WHEN ?3 != ?4 THEN ?5 END)
             ON CONFLICT (entry, label)
             DO UPDATE SET desired = excluded.desired, observed = excluded.observed,
               desired_by = CASE WHEN excluded.desired = excluded.observed THEN NULL
                                 WHEN ?5 IS NOT NULL THEN ?5
                                 ELSE desired_by END,
               refused = CASE WHEN excluded.desired = excluded.observed OR ?5 IS NOT NULL
                              THEN NULL ELSE refused END",
        )?
        .execute(params![
            entry.0,
            label.0,
            desired,
            observed,
            by.map(|b| b.0)
        ])?;
    } else {
        tx.prepare_cached("DELETE FROM entry_label WHERE entry = ?1 AND label = ?2")?
            .execute([entry.0, label.0])?;
    }
    Ok(())
}

// ── ops ───────────────────────────────────────────────────────────────────

/// Sets the desired membership of labels on entries. Changes that are
/// already in effect are skipped, so the inverse reverts exactly what this
/// op changed (SPEC §6.2).
///
/// An entry is in exactly one of its account's folders, if the account has
/// them (SPEC §8.3): adding a folder takes the entry out of the others, and
/// removing its last one puts it in the Archive. Those implied changes are
/// part of the op, and of its inverse, but not of its description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetLabels {
    pub changes: Vec<LabelChange>,
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
        for (entry, requested) in by_entry {
            let implied = implied_by_folders(tx, entry, &requested)?;
            // Moving into a folder is leaving the other: described as one.
            let mut moved = false;
            let mut left = vec![];
            for c in &requested {
                if is_folder(tx, c.label)? {
                    if c.desired {
                        moved = true;
                    } else {
                        left.push(c.label);
                    }
                }
            }
            changes.extend(
                requested
                    .into_iter()
                    .map(|c| (c, moved && left.contains(&c.label))),
            );
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
            let (desired, observed) =
                label_row(tx, change.entry, change.label)?.unwrap_or_default();
            if desired == change.desired {
                continue;
            }
            set_label_row(
                tx,
                change.entry,
                change.label,
                change.desired,
                observed,
                Some(this),
            )?;
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
            let (external_id, name, folder): (String, String, bool) = tx.query_row(
                "SELECT external_id, name, folder FROM label WHERE id = ?1",
                [label.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            description.push(describe_label_change(
                &external_id,
                &name,
                folder,
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
            inverse: Inverse::of(&SetLabels { changes: inverse }),
            events: [ServerEvent::LabelsChanged(LabelsChanged {
                changes: events,
            })]
            .into_iter()
            .chain(accounts.iter().map(|a| outbox_changed(a)))
            .collect(),
        })
    }
}

/// The changes that keep `entry` in exactly one folder, given the
/// `requested` ones; none if its account has no folders.
fn implied_by_folders(
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
    let folders: BTreeMap<LabelId, String> = tx
        .prepare_cached("SELECT id, external_id FROM label WHERE account_id = ?1 AND folder")?
        .query_map([&account], |r| Ok((LabelId(r.get(0)?), r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    if folders.is_empty() {
        return Ok(vec![]);
    }
    let mut desired: BTreeSet<LabelId> = tx
        .prepare_cached(
            "SELECT el.label FROM entry_label el JOIN label l ON l.id = el.label
             WHERE el.entry = ?1 AND el.desired AND l.folder",
        )?
        .query_map([entry.0], |r| Ok(LabelId(r.get(0)?)))?
        .collect::<Result<_, _>>()?;
    let mut added = vec![];
    for change in requested.iter().filter(|c| folders.contains_key(&c.label)) {
        if change.desired {
            desired.insert(change.label);
            added.push(change.label);
        } else {
            desired.remove(&change.label);
        }
    }
    let implied = |label, desired| LabelChange {
        entry,
        label,
        desired,
    };
    match &added[..] {
        [] if desired.is_empty() => {
            let archive = folders
                .iter()
                .find(|(_, external_id)| *external_id == "ARCHIVE")
                .map(|(&id, _)| id)
                .with_context(|| format!("account {account} has folders, but no Archive"))?;
            Ok(vec![implied(archive, true)])
        }
        [] => Ok(vec![]),
        &[into] => Ok(desired
            .into_iter()
            .filter(|&f| f != into)
            .map(|f| implied(f, false))
            .collect()),
        _ => bail!(Conflict("a message is in one folder at a time".into())),
    }
}

/// Whether the label is a folder; a label gone is refused later.
fn is_folder(tx: &Transaction, label: LabelId) -> Result<bool> {
    Ok(tx
        .prepare_cached("SELECT folder FROM label WHERE id = ?1")?
        .query_row([label.0], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
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
