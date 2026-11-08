//! Mailbox state in the database, and the log entries that change it:
//! observations recorded by sync, and the user's label edits (SPEC §6.6).
//!
//! Label membership is stored twice per entry: *desired* (what the user
//! asked for) and *observed* (what the provider last reported). Ops change
//! desired; observations change observed, and desired follows unless a
//! local change is pending; the reconciler pushes differences and records
//! what it pushed.

use crate::{
    parse::{self, Parsed},
    store::VariantHash,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uplink_protocol::{
    mail::{
        EntriesAdded, EntriesRemoved, EntryId, LabelId, LabelListChanged, LabelState,
        LabelsChanged, MessageId,
    },
    ServerEvent,
};
use uplink_storage::rusqlite::{params, OptionalExtension, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Observation, Observed, Op, Registry};

/// Bytes of body text indexed for search (SPEC §8.7).
const INDEXED_BODY_BYTES: usize = 64 * 1024;

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
    /// Entries fetched for the first time.
    pub added: Vec<NewEntry>,
    /// Current labels of known entries.
    pub updated: Vec<EntryLabels>,
    /// Entries no longer in the account, by external id.
    pub removed: Vec<String>,
    /// Replaces the account's sync state: everything up to the point it
    /// describes is now recorded.
    pub sync_state: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedLabel {
    pub external_id: String,
    pub name: String,
    pub system: bool,
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

        let mut label_changes = vec![];

        let mut added = vec![];
        for new in self.added {
            let labels = resolve(&new.labels)?;
            let entry = insert_entry(tx, account, new)?;
            for label in labels {
                set_label_row(tx, entry, label, true, true)?;
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
        for update in self.updated {
            let entry = entry_id(tx, account, &update.external_id)?;
            let observed = resolve(&update.labels)?;
            let changes = observe_labels(tx, entry, &observed)?;
            relabelled += usize::from(!changes.is_empty());
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
            tx.execute("DELETE FROM mail_entry WHERE id = ?1", [entry.0])?;
            removed.push(entry);
        }
        if !removed.is_empty() {
            description.push(format!("{} removed", messages(removed.len())));
            events.push(ServerEvent::EntriesRemoved(EntriesRemoved {
                entries: removed,
            }));
        }

        if let Some(state) = self.sync_state {
            tx.execute(
                "UPDATE account SET sync_state = ?1 WHERE id = ?2",
                params![state.to_string(), account],
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

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
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
            set_label_row(tx, change.entry, change.label, desired, change.desired)?;
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
            events: vec![ServerEvent::LabelsChanged(LabelsChanged { changes })],
        })
    }
}

/// Makes the account's labels `labels`; returns whether anything changed.
fn replace_labels(tx: &Transaction, account: &str, labels: Vec<ObservedLabel>) -> Result<bool> {
    let mut known: HashMap<String, ObservedLabel> = tx
        .prepare("SELECT external_id, name, system FROM label WHERE account_id = ?1")?
        .query_map([account], |r| {
            Ok(ObservedLabel {
                external_id: r.get(0)?,
                name: r.get(1)?,
                system: r.get(2)?,
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
            "INSERT INTO label (account_id, external_id, name, system) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account_id, external_id)
             DO UPDATE SET name = excluded.name, system = excluded.system",
            params![account, label.external_id, label.name, label.system],
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
        None => {
            let message = message_for_variant(tx, &new.variant, &new.parsed)?;
            tx.execute(
                "INSERT INTO message_variant (sha256, message, source, size_bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![new.variant.0, message.0, account, new.size_bytes],
            )?;
            message
        }
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
            to_addrs, cc_addrs, date_sent, senders, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
        ],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO message_fts (rowid, subject, from_addr, from_name, to_addrs, body_text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            parsed.subject,
            parsed.from_addr,
            parsed.from_name,
            parsed.to_addrs.join(" "),
            parse::truncate(&parsed.body_text, INDEXED_BODY_BYTES),
        ],
    )?;
    Ok(MessageId(id))
}

/// Records that the provider reports exactly `observed` on `entry`.
fn observe_labels(
    tx: &Transaction,
    entry: EntryId,
    observed: &BTreeSet<LabelId>,
) -> Result<Vec<LabelState>> {
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
    for (label, (desired, was_observed)) in rows {
        let now_observed = observed.contains(&label);
        if now_observed == was_observed {
            continue;
        }
        // Without a pending local change, the remote change wins; with one,
        // the local change stands and is pushed.
        let desired = if desired == was_observed {
            now_observed
        } else {
            desired
        };
        set_label_row(tx, entry, label, desired, now_observed)?;
        changes.push(LabelState {
            entry,
            label,
            desired,
            observed: now_observed,
        });
    }
    Ok(changes)
}

fn label_row(tx: &Transaction, entry: EntryId, label: LabelId) -> Result<Option<(bool, bool)>> {
    Ok(tx
        .query_row(
            "SELECT desired, observed FROM entry_label WHERE entry = ?1 AND label = ?2",
            [entry.0, label.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn set_label_row(
    tx: &Transaction,
    entry: EntryId,
    label: LabelId,
    desired: bool,
    observed: bool,
) -> Result<()> {
    if desired || observed {
        tx.execute(
            "INSERT INTO entry_label (entry, label, desired, observed) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (entry, label)
             DO UPDATE SET desired = excluded.desired, observed = excluded.observed",
            params![entry.0, label.0, desired, observed],
        )?;
    } else {
        tx.execute(
            "DELETE FROM entry_label WHERE entry = ?1 AND label = ?2",
            [entry.0, label.0],
        )?;
    }
    Ok(())
}

// ── ops ───────────────────────────────────────────────────────────────────

/// Sets the desired membership of labels on entries. Changes that are
/// already in effect are skipped, so the inverse reverts exactly what this
/// op changed (SPEC §6.2).
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
        let mut inverse = vec![];
        let mut events = vec![];
        // (label, desired) → entries changed
        let mut groups: BTreeMap<(LabelId, bool), usize> = BTreeMap::new();
        for change in self.changes {
            let entry_account: String = tx
                .query_row(
                    "SELECT account_id FROM mail_entry WHERE id = ?1",
                    [change.entry.0],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Conflict("the message is no longer in its account".into()))?;
            let label_account: String = tx
                .query_row(
                    "SELECT account_id FROM label WHERE id = ?1",
                    [change.label.0],
                    |r| r.get(0),
                )
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
            set_label_row(tx, change.entry, change.label, change.desired, observed)?;
            inverse.push(LabelChange { desired, ..change });
            *groups.entry((change.label, change.desired)).or_default() += 1;
            events.push(LabelState {
                entry: change.entry,
                label: change.label,
                desired: change.desired,
                observed,
            });
        }
        let mut description = vec![];
        for ((label, desired), count) in groups {
            let (external_id, name): (String, String) = tx.query_row(
                "SELECT external_id, name FROM label WHERE id = ?1",
                [label.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            description.push(describe_label_change(&external_id, &name, desired, count));
        }
        Ok(Applied {
            description: if description.is_empty() {
                "Changed nothing".to_owned()
            } else {
                capitalize(&description.join(", "))
            },
            inverse: Inverse::of(&SetLabels { changes: inverse }),
            events: vec![ServerEvent::LabelsChanged(LabelsChanged {
                changes: events,
            })],
        })
    }
}

fn describe_label_change(external_id: &str, name: &str, desired: bool, count: usize) -> String {
    let n = messages(count);
    match (external_id, desired) {
        ("INBOX", false) => format!("archived {n}"),
        ("INBOX", true) => format!("moved {n} to Inbox"),
        ("UNREAD", false) => format!("marked {n} read"),
        ("UNREAD", true) => format!("marked {n} unread"),
        ("STARRED", true) => format!("starred {n}"),
        ("STARRED", false) => format!("unstarred {n}"),
        ("TRASH", true) => format!("moved {n} to Trash"),
        ("TRASH", false) => format!("restored {n} from Trash"),
        (_, true) => format!("labelled {n} '{name}'"),
        (_, false) => format!("removed label '{name}' from {n}"),
    }
}

fn messages(count: usize) -> String {
    match count {
        1 => "1 message".to_owned(),
        n => format!("{n} messages"),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
