//! The sync task of a Gmail account: mirrors the mailbox into uplink and
//! pushes the user's label changes back (SPEC §6.6, §8.1).
//!
//! One task per account does everything, in turn: fetch changes and
//! record them, push pending changes and record them as pushed. Since a
//! fetch is always recorded before the next push starts, a stale fetch can
//! never be recorded over a push and revert it.

use super::api::{BatchModify, Format, Gmail, Message};
use crate::{
    mailbox::{EntryLabels, LabelChange, LabelsPushed, MailboxObserved, NewEntry, ObservedLabel},
    parse::parse,
    store::MailStore,
};
use anyhow::{Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast::{self, error::RecvError};
use uplink_protocol::{
    mail::{EntryId, LabelId},
    LogEntry, Source,
};
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::{Dispatcher, Observation};

pub const KIND: &str = "gmail";
/// How often to poll for changes (SPEC §8.1).
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Messages fetched at once.
const CONCURRENT_FETCHES: usize = 8;
/// Gmail's limit for `batchModify`.
const BATCH_MODIFY_MAX: usize = 1000;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// `account.sync_state` of a Gmail account: the mailbox is recorded up to
/// this history id.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncState {
    history_id: String,
}

pub struct Sync {
    pub account: String,
    pub gmail: Gmail,
    pub dispatcher: Dispatcher,
    /// A reader connection of the task's own.
    pub reader: Connection,
    pub store: MailStore,
    pub poll_interval: Duration,
}

impl Sync {
    /// Runs until the process exits, retrying failures with backoff.
    pub async fn run(self) {
        Task {
            wake: self.dispatcher.hub().subscribe(),
            reader: Arc::new(Mutex::new(self.reader)),
            account: self.account,
            gmail: self.gmail,
            dispatcher: self.dispatcher,
            store: self.store,
            poll_interval: self.poll_interval,
        }
        .run()
        .await
    }
}

struct Task {
    account: String,
    gmail: Gmail,
    dispatcher: Dispatcher,
    store: MailStore,
    poll_interval: Duration,
    reader: Arc<Mutex<Connection>>,
    wake: broadcast::Receiver<Arc<LogEntry>>,
}

impl Task {
    async fn run(mut self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.step().await {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                    self.wait().await;
                }
                Err(e) => {
                    tracing::warn!(account = self.account, "sync failed: {e:#}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    async fn step(&self) -> Result<()> {
        let synced = match self.sync_state().await? {
            Some(state) => self.poll(&state.history_id).await?,
            None => false,
        };
        if !synced {
            self.full_sync().await?;
        }
        self.push().await
    }

    /// Until the poll interval passes or the user changes something.
    async fn wait(&mut self) {
        let deadline = tokio::time::sleep(self.poll_interval);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return,
                entry = self.wake.recv() => match entry {
                    Ok(entry) if entry.source == Source::User => return,
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return,
                    Err(RecvError::Closed) => panic!("the hub outlives sync tasks"),
                },
            }
        }
    }

    /// Records changes since `from`; `false` if history from there is no
    /// longer available.
    async fn poll(&self, from: &str) -> Result<bool> {
        let gmail = &self.gmail;
        let mut touched = vec![];
        let mut seen = HashSet::new();
        let mut page_token = None;
        let latest = loop {
            let Some(page) = gmail.history(from, page_token.as_deref()).await? else {
                tracing::info!(account = self.account, "history expired; syncing fully");
                return Ok(false);
            };
            for message in page.history.into_iter().flat_map(|r| r.messages) {
                if seen.insert(message.id.clone()) {
                    touched.push(message.id);
                }
            }
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => break page.history_id,
            }
        };
        if touched.is_empty() && latest == from {
            return Ok(true);
        }
        let known = self.known().await?;
        let (old, new): (Vec<String>, Vec<String>) =
            touched.into_iter().partition(|id| known.contains(id));
        let added = self.fetch_new(new).await?;
        let mut updated = vec![];
        let mut removed = vec![];
        for (id, message) in self.fetch_labels(old).await? {
            match message {
                Some(labels) => updated.push(labels),
                None => removed.push(id),
            }
        }
        let labels = if added.is_empty() && updated.is_empty() {
            None
        } else {
            // Messages may carry labels created since the last listing.
            Some(self.labels().await?)
        };
        self.observe(MailboxObserved {
            account_kind: KIND,
            labels,
            added,
            updated,
            removed,
            sync_state: Some(state(latest)),
        })
        .await
        .map(|()| true)
    }

    /// Records the whole mailbox, as of the history id read first.
    async fn full_sync(&self) -> Result<()> {
        let gmail = &self.gmail;
        let start = gmail.profile().await?.history_id;
        self.observe(MailboxObserved {
            labels: Some(self.labels().await?),
            ..observed()
        })
        .await?;
        let known = self.known().await?;
        let mut listed = HashSet::new();
        let mut page_token = None;
        loop {
            let page = gmail.messages(page_token.as_deref()).await?;
            let ids: Vec<String> = page.messages.into_iter().map(|m| m.id).collect();
            listed.extend(ids.iter().cloned());
            let (old, new): (Vec<String>, Vec<String>) =
                ids.into_iter().partition(|id| known.contains(id));
            let added = self.fetch_new(new).await?;
            // Messages deleted since they were listed are recorded as
            // removed by the first poll, which starts before the listing.
            let updated: Vec<EntryLabels> = self
                .fetch_labels(old)
                .await?
                .into_iter()
                .filter_map(|(_, labels)| labels)
                .collect();
            if !added.is_empty() || !updated.is_empty() {
                self.observe(MailboxObserved {
                    added,
                    updated,
                    ..observed()
                })
                .await?;
            }
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => break,
            }
        }
        self.observe(MailboxObserved {
            removed: known.difference(&listed).cloned().collect(),
            sync_state: Some(state(start)),
            ..observed()
        })
        .await
    }

    /// Pushes label changes not yet in effect at Gmail.
    async fn push(&self) -> Result<()> {
        let account = self.account.clone();
        let pending: Vec<(EntryId, String, LabelId, String, bool)> = self
            .read(move |conn| {
                Ok(conn
                    .prepare(
                        "SELECT e.id, e.external_id, l.id, l.external_id, el.desired
                         FROM entry_label el
                         JOIN mail_entry e ON e.id = el.entry
                         JOIN label l ON l.id = el.label
                         WHERE e.account_id = ?1 AND el.desired != el.observed
                         ORDER BY e.id, l.id",
                    )?
                    .query_map([account], |r| {
                        Ok((
                            EntryId(r.get(0)?),
                            r.get(1)?,
                            LabelId(r.get(2)?),
                            r.get(3)?,
                            r.get(4)?,
                        ))
                    })?
                    .collect::<Result<_, _>>()?)
            })
            .await?;
        if pending.is_empty() {
            return Ok(());
        }
        // Entries needing the same change go in one request.
        let mut per_entry: BTreeMap<EntryId, (String, Vec<String>, Vec<String>)> = BTreeMap::new();
        let mut changes = vec![];
        for (entry, external_id, label, external_label, desired) in pending {
            let (_, add, remove) = per_entry
                .entry(entry)
                .or_insert_with(|| (external_id, vec![], vec![]));
            if desired { add } else { remove }.push(external_label);
            changes.push(LabelChange {
                entry,
                label,
                desired,
            });
        }
        let mut per_change: BTreeMap<(Vec<String>, Vec<String>), Vec<String>> = BTreeMap::new();
        for (external_id, add, remove) in per_entry.into_values() {
            per_change
                .entry((add, remove))
                .or_default()
                .push(external_id);
        }
        for ((add, remove), ids) in per_change {
            for ids in ids.chunks(BATCH_MODIFY_MAX) {
                self.gmail
                    .batch_modify(&BatchModify {
                        ids,
                        add_label_ids: &add,
                        remove_label_ids: &remove,
                    })
                    .await?;
            }
        }
        self.observe(LabelsPushed { changes }).await
    }

    async fn labels(&self) -> Result<Vec<ObservedLabel>> {
        Ok(self
            .gmail
            .labels()
            .await?
            .into_iter()
            .map(|l| ObservedLabel {
                system: l.kind.as_deref() == Some("system"),
                external_id: l.id,
                name: l.name,
            })
            .collect())
    }

    /// Fetches and stores messages new to uplink; skips those deleted
    /// meanwhile.
    async fn fetch_new(&self, ids: Vec<String>) -> Result<Vec<NewEntry>> {
        let entries: Vec<Option<NewEntry>> = stream::iter(ids)
            .map(|id| async move {
                match self.gmail.message(&id, Format::Raw).await? {
                    Some(message) => self.store(message).await.map(Some),
                    None => Ok(None),
                }
            })
            .buffered(CONCURRENT_FETCHES)
            .try_collect()
            .await?;
        Ok(entries.into_iter().flatten().collect())
    }

    /// Current labels of known messages; `None` for those deleted.
    async fn fetch_labels(&self, ids: Vec<String>) -> Result<Vec<(String, Option<EntryLabels>)>> {
        stream::iter(ids)
            .map(|id| async move {
                let message = self.gmail.message(&id, Format::Minimal).await?;
                let labels = message.map(|m| EntryLabels {
                    external_id: m.id,
                    labels: m.label_ids,
                });
                Ok((id, labels))
            })
            .buffered(CONCURRENT_FETCHES)
            .try_collect()
            .await
    }

    async fn store(&self, message: Message) -> Result<NewEntry> {
        let raw = message.raw()?;
        let store = self.store.clone();
        let (variant, parsed, size) = tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((store.put(&raw)?, parse(&raw), raw.len()))
        })
        .await??;
        Ok(NewEntry {
            received_at: message.internal_date()?,
            external_id: message.id,
            thread_id: message.thread_id,
            labels: message.label_ids,
            variant,
            size_bytes: size.try_into()?,
            parsed,
        })
    }

    async fn sync_state(&self) -> Result<Option<SyncState>> {
        let account = self.account.clone();
        let state: Option<String> = self
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT sync_state FROM account WHERE id = ?1",
                        [account],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten())
            })
            .await?;
        state
            .map(|s| serde_json::from_str(&s).with_context(|| format!("sync state {s}")))
            .transpose()
    }

    /// External ids of the account's entries.
    async fn known(&self) -> Result<HashSet<String>> {
        let account = self.account.clone();
        self.read(move |conn| {
            Ok(conn
                .prepare("SELECT external_id FROM mail_entry WHERE account_id = ?1")?
                .query_map([account], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    async fn observe(&self, observation: impl Observation) -> Result<()> {
        self.dispatcher
            .observe(self.account.clone(), observation)
            .await
            .map(drop)
    }

    async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let reader = self.reader.clone();
        tokio::task::spawn_blocking(move || f(&reader.lock().expect("reader poisoned"))).await?
    }
}

fn state(history_id: String) -> serde_json::Value {
    serde_json::to_value(SyncState { history_id }).expect("sync state serializes")
}

fn observed() -> MailboxObserved {
    MailboxObserved {
        account_kind: KIND,
        labels: None,
        added: vec![],
        updated: vec![],
        removed: vec![],
        sync_state: None,
    }
}
