//! The sync of a Gmail account (SPEC §8.1–8.2): history polling, label
//! pushes by `batchModify`, and sending.

use super::{
    api::{BatchModify, Format, Gmail, Message},
    oauth,
};
use crate::{
    account::{Account, Provider, SendError, SyncTask},
    mailbox::{
        EntryLabels, LabelChange, LabelsPushed, MailboxObserved, NewEntry, ObservedLabel, SyncState,
    },
    oauth::Credentials,
};
use anyhow::Result;
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};
use uplink_config::GmailAccountConfig;
use uplink_protocol::mail::EntryId;
use uplink_storage::seal::MasterKey;
use uplink_tx_log::Dispatcher;

pub const KIND: &str = "gmail";
/// How often to poll for changes (SPEC §8.1).
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Messages fetched at once.
const CONCURRENT_FETCHES: usize = 8;
/// While listing, how many messages fetched are stored at once: at
/// Gmail's quota, about 40 s of fetching.
const STORE_EVERY: usize = 50;
/// Gmail's limit for `batchModify`.
const BATCH_MODIFY_MAX: usize = 1000;

/// `account.sync_state` of a Gmail account.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum State {
    /// The mailbox is recorded up to this history id.
    Synced { history_id: String },
    /// The mailbox is being listed, which resumes after an interruption.
    Listing(Listing),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Listing {
    /// The history id when the listing began: the polls after it record
    /// what changed from there.
    listing_from: String,
    /// The last entry recorded before the listing began. Those after it
    /// were fetched by the listing, so the polls cover their changes; those
    /// up to it may have changed unseen, and their labels are fetched anew.
    before: i64,
}

/// The sync task of `config`'s account, storing mail under `data_root`.
pub fn task(
    config: &GmailAccountConfig,
    key: Arc<MasterKey>,
    http: reqwest::Client,
    dispatcher: Dispatcher,
    data_root: &Path,
) -> Result<SyncTask<GmailSync>> {
    let credentials = Credentials::new(http.clone(), oauth::grant(config), key);
    Ok(SyncTask::new(
        GmailSync {
            gmail: Gmail::new(http, &config.endpoints.api, credentials),
        },
        config.id.clone(),
        config.address.clone(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        data_root,
        POLL_INTERVAL,
    ))
}

pub struct GmailSync {
    pub gmail: Gmail,
}

impl Provider for GmailSync {
    const KIND: &'static str = KIND;

    async fn sync(&self, account: &Account) -> Result<()> {
        match account.sync_state::<State>().await? {
            Some(State::Synced { history_id }) => {
                if !self.poll(account, &history_id).await? {
                    self.list(account, None).await?;
                }
            }
            Some(State::Listing(listing)) => {
                tracing::info!(account = account.id, "resuming the listing of the mailbox");
                self.list(account, Some(listing)).await?;
            }
            None => self.list(account, None).await?,
        }
        Ok(())
    }

    async fn push(&self, account: &Account) -> Result<()> {
        self.push_labels(account).await
    }

    async fn send(&self, raw: &[u8], thread: Option<&str>) -> Result<(), SendError> {
        self.gmail.send_message(raw, thread).await
    }
}

impl GmailSync {
    /// Records changes since `from`; `false` if history from there is no
    /// longer available.
    async fn poll(&self, account: &Account, from: &str) -> Result<bool> {
        let gmail = &self.gmail;
        let mut touched = vec![];
        let mut seen = HashSet::new();
        let mut page_token = None;
        let latest = loop {
            let Some(page) = gmail.history(from, page_token.as_deref()).await? else {
                tracing::info!(account = account.id, "history expired; syncing fully");
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
        let known = account.known().await?;
        let (old, new): (Vec<String>, Vec<String>) =
            touched.into_iter().partition(|id| known.contains(id));
        let added = self.fetch_new(account, new).await?;
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
        account
            .observe(MailboxObserved {
                account_kind: KIND,
                labels,
                added,
                updated,
                removed,
                sync_state: Some(synced(latest)),
            })
            .await
            .map(|()| true)
    }

    /// Records the whole mailbox, as of the history id read when the
    /// listing began; continues `resumed` if given. A resumed listing
    /// lists anew (Gmail's page tokens do not last) but fetches only what
    /// it has not.
    async fn list(&self, account: &Account, resumed: Option<Listing>) -> Result<()> {
        let gmail = &self.gmail;
        // Its total, for the progress shown, is as of now.
        let profile = gmail.profile().await?;
        let listing = match resumed {
            Some(listing) => listing,
            None => Listing {
                listing_from: profile.history_id,
                before: account.last_entry().await?,
            },
        };
        account
            .observe(MailboxObserved {
                labels: Some(self.labels().await?),
                sync_state: Some(SyncState::Listing {
                    state: serde_json::to_value(State::Listing(listing.clone()))
                        .expect("serializes"),
                    total: Some(profile.messages_total),
                }),
                ..observed()
            })
            .await?;
        let known = account.known().await?;
        let earlier = account.known_up_to(listing.before).await?;
        let mut listed = HashSet::new();
        let mut page_token = None;
        loop {
            let page = gmail.messages(page_token.as_deref()).await?;
            let ids: Vec<String> = page.messages.into_iter().map(|m| m.id).collect();
            listed.extend(ids.iter().cloned());
            let new: Vec<String> = ids
                .iter()
                .filter(|id| !known.contains(*id))
                .cloned()
                .collect();
            // Stored a few at a time, as a page takes minutes of quota: so
            // that they show, and an interruption loses little.
            for batch in new.chunks(STORE_EVERY) {
                let added = self.fetch_new(account, batch.to_vec()).await?;
                if !added.is_empty() {
                    account
                        .observe(MailboxObserved {
                            added,
                            ..observed()
                        })
                        .await?;
                }
            }
            // Messages deleted since they were listed are recorded as
            // removed by the first poll, which starts before the listing.
            let old = ids.into_iter().filter(|id| earlier.contains(id)).collect();
            let updated: Vec<EntryLabels> = self
                .fetch_labels(old)
                .await?
                .into_iter()
                .filter_map(|(_, labels)| labels)
                .collect();
            if !updated.is_empty() {
                account
                    .observe(MailboxObserved {
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
        account
            .observe(MailboxObserved {
                removed: known.difference(&listed).cloned().collect(),
                sync_state: Some(synced(listing.listing_from)),
                ..observed()
            })
            .await
    }

    /// Pushes label changes not yet in effect at Gmail.
    async fn push_labels(&self, account: &Account) -> Result<()> {
        let pending = account.pending().await?;
        if pending.is_empty() {
            return Ok(());
        }
        // Entries needing the same change go in one request.
        let mut per_entry: BTreeMap<EntryId, (String, Vec<String>, Vec<String>)> = BTreeMap::new();
        let mut changes = vec![];
        for p in pending {
            let (_, add, remove) = per_entry
                .entry(p.entry)
                .or_insert_with(|| (p.external_id, vec![], vec![]));
            if p.desired { add } else { remove }.push(p.label_external_id);
            changes.push(LabelChange {
                entry: p.entry,
                label: p.label,
                desired: p.desired,
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
        account.observe(LabelsPushed { changes }).await
    }

    async fn labels(&self) -> Result<Vec<ObservedLabel>> {
        Ok(self
            .gmail
            .labels()
            .await?
            .into_iter()
            .map(|l| ObservedLabel {
                system: l.kind.as_deref() == Some("system"),
                folder: false,
                external_id: l.id,
                name: l.name,
            })
            .collect())
    }

    /// Fetches and stores messages new to uplink; skips those deleted
    /// meanwhile.
    async fn fetch_new(&self, account: &Account, ids: Vec<String>) -> Result<Vec<NewEntry>> {
        let entries: Vec<Option<NewEntry>> = stream::iter(ids)
            .map(|id| async move {
                match self.gmail.message(&id, Format::Raw).await? {
                    Some(message) => store(account, message).await.map(Some),
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
}

/// Stores a message fetched in full.
async fn store(account: &Account, message: Message) -> Result<NewEntry> {
    let (variant, parsed, size_bytes) = account.store_raw(message.raw()?).await?;
    Ok(NewEntry {
        received_at: message.internal_date()?,
        external_id: message.id,
        thread_id: message.thread_id,
        labels: message.label_ids,
        variant,
        size_bytes,
        parsed,
    })
}

fn synced(history_id: String) -> SyncState {
    SyncState::Synced(serde_json::to_value(State::Synced { history_id }).expect("serializes"))
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
