//! The sync of a Gmail account (SPEC §8.1–8.2): history polls, as push
//! notifications tell of changes, label pushes by `batchModify`, and
//! sending.

use super::{
    api::{BatchModify, Format, Gmail, Message},
    oauth,
    pubsub::PubSub,
    watch::{Push, Watcher},
};
use crate::watching::Told;
use crate::{
    account::{Account, Outgoing, Provider, SendError, SyncTask},
    compose::{self, SendDrafted, SentCopy},
    mailbox::{
        EntryLabels, LabelChange, LabelsPushed, LabelsRefused, MailboxObserved, NewEntry,
        ObservedLabel, SyncState,
    },
    oauth::Credentials,
};
use anyhow::Result;
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use uplink_config::GmailAccountConfig;
use uplink_protocol::mail::{EntryId, LabelKind};
use uplink_storage::seal::MasterKey;
use uplink_tx_log::Dispatcher;

pub const KIND: &str = "gmail";
/// How often to poll for changes, as push notifications may be late or
/// dropped (SPEC §8.1).
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// How often to poll while there are no push notifications.
pub const UNWATCHED_POLL: Duration = Duration::from_secs(5);
/// Messages fetched at once.
const CONCURRENT_FETCHES: usize = 8;
/// While listing, how many messages fetched are stored at once: at
/// Gmail's quota, about 40 s of fetching.
const STORE_EVERY: usize = 50;
/// While listing, of how many messages labels are fetched at once: as
/// much quota as [`STORE_EVERY`] fetched whole. A step takes no longer,
/// so that the changes polled for between steps, new mail most of all,
/// are recorded as they come.
const RELABEL_EVERY: usize = 150;
/// Gmail's limit for `batchModify`.
const BATCH_MODIFY_MAX: usize = 1000;
/// How far Gmail's clock may be behind ours, in ms: its time for a send
/// may be that much before ours.
const CLOCK_SKEW_MS: i64 = 60 * 1000;

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
    /// The history id the mailbox's changes are recorded up to: as the
    /// listing began, then as the polls between its steps went on. The
    /// listing records the mailbox as it was; the polls, what changed
    /// since.
    // As it was named before the polls went on during the listing.
    #[serde(alias = "listing_from")]
    changes_from: String,
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
    let pubsub = PubSub::new(http.clone(), &config.endpoints.pubsub, credentials.clone());
    Ok(SyncTask::new(
        GmailSync::new(
            config.id.clone(),
            Gmail::new(http, &config.endpoints.api, credentials),
            Push::new(pubsub, &config.oauth.project, &config.id),
            UNWATCHED_POLL,
        ),
        config.id.clone(),
        config.identities(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        data_root,
        POLL_INTERVAL,
    ))
}

pub struct GmailSync {
    account: String,
    pub gmail: Gmail,
    push: Arc<Push>,
    unwatched_poll: Duration,
    /// Started as the task first waits.
    watcher: OnceLock<Watcher>,
    /// The listing under way, if one is: where it has got, between steps.
    /// Only in memory: Gmail's page tokens do not last, and a listing
    /// resumed after a restart lists anew, fetching only what it has not.
    cursor: tokio::sync::Mutex<Option<Cursor>>,
}

impl GmailSync {
    /// The sync of account `account`, whose changes are published to
    /// `push`; polled every `unwatched_poll` while they are not.
    pub fn new(account: String, gmail: Gmail, push: Push, unwatched_poll: Duration) -> Self {
        Self {
            account,
            gmail,
            push: Arc::new(push),
            unwatched_poll,
            watcher: OnceLock::new(),
            cursor: tokio::sync::Mutex::new(None),
        }
    }
}

/// A listing's progress through the mailbox.
struct Cursor {
    listing: Listing,
    /// How many messages the mailbox held as it began, for the progress
    /// shown.
    total: u32,
    /// What the account has: what it had as the listing began, and what
    /// the polls since added, less what they removed.
    known: HashSet<String>,
    /// What it had before the listing first began.
    earlier: HashSet<String>,
    /// Every message listed so far, or added by a poll: what is known but
    /// not this, at the end, has gone.
    listed: HashSet<String>,
    /// The next page to list.
    next: Next,
    /// Of the pages listed, the earlier messages whose labels are yet to
    /// fetch, and the new messages yet to fetch, in order.
    unlabelled: Vec<String>,
    unfetched: Vec<String>,
}

impl Cursor {
    /// The account's sync state while listing, its changes recorded up to
    /// `changes_from`.
    fn sync_state(&self, changes_from: &str) -> SyncState {
        let listing = Listing {
            changes_from: changes_from.to_owned(),
            ..self.listing.clone()
        };
        SyncState::Listing {
            state: serde_json::to_value(State::Listing(listing)).expect("serializes"),
            total: Some(self.total),
        }
    }

    /// Takes in what a poll recorded, up to `history_id`: what it added
    /// is not fetched again, nor what it removed taken as gone again.
    fn polled(&mut self, history_id: String, added: &[String], removed: &[String]) {
        self.listing.changes_from = history_id;
        for id in removed {
            self.known.remove(id);
        }
        self.known.extend(added.iter().cloned());
        self.listed.extend(added.iter().cloned());
        self.unfetched.retain(|id| !added.contains(id));
    }
}

enum Next {
    First,
    Page(String),
    Done,
}

impl Provider for GmailSync {
    const KIND: &'static str = KIND;
    // Filed as it is accepted (SPEC §6.7).
    const FILES_SENT: bool = true;

    /// Polls; and, listing the mailbox, takes one step: one page listed,
    /// or a batch of its messages fetched, or of their labels. The polls
    /// go on while the listing does, which takes days, so that it holds up
    /// no new mail.
    async fn sync(&self, account: &Account) -> Result<bool> {
        let mut cursor = self.cursor.lock().await;
        let Some(listing) = cursor.as_mut() else {
            let resumed = match account.sync_state::<State>().await? {
                Some(State::Synced { history_id }) => {
                    if self.poll(account, &history_id, None).await? {
                        return Ok(false);
                    }
                    None
                }
                Some(State::Listing(listing)) => {
                    tracing::info!(account = account.id, "resuming the listing of the mailbox");
                    Some(listing)
                }
                None => None,
            };
            *cursor = Some(self.start_listing(account, resumed).await?);
            return Ok(true);
        };
        let from = listing.listing.changes_from.clone();
        if !self.poll(account, &from, Some(listing)).await? {
            *cursor = Some(self.start_listing(account, None).await?);
            return Ok(true);
        }
        let more = self.list_step(account, listing).await?;
        if !more {
            *cursor = None;
        }
        Ok(more)
    }

    async fn push(&self, account: &Account) -> Result<()> {
        self.push_labels(account).await
    }

    /// In two steps (SPEC §8.2): the message is made a draft, recorded,
    /// then sent by its id, which Gmail answers with its copy's. Sending a
    /// draft again sends nothing, as it is gone once sent.
    async fn send(&self, account: &Account, mail: Outgoing<'_>) -> Result<SentCopy, SendError> {
        let (draft, thread) = match (mail.provider_draft, mail.provider_thread) {
            (Some(draft), Some(thread)) => (draft.to_owned(), thread.to_owned()),
            (None, None) => {
                self.check_send_as(mail.from).await?;
                let drafted = self.gmail.create_draft(mail.raw, mail.thread).await?;
                account
                    .observe(SendDrafted {
                        item: mail.item,
                        provider_draft: drafted.id.clone(),
                        provider_thread: Some(drafted.thread.clone()),
                    })
                    .await
                    .map_err(SendError::NotSent)?;
                (drafted.id, drafted.thread)
            }
            drafted => unreachable!("Gmail drafts are recorded with their thread: {drafted:?}"),
        };
        match self.gmail.send_draft(&draft).await? {
            Some(copy) => Ok(SentCopy::Id(copy)),
            None => self.find_copy(account, &thread, mail.not_before).await,
        }
    }

    /// What Gmail publishes, watched from the first wait on.
    async fn told(&self) -> Told {
        self.watcher
            .get_or_init(|| {
                Watcher::start(
                    self.account.clone(),
                    self.gmail.clone(),
                    self.push.clone(),
                    self.unwatched_poll,
                )
            })
            .told()
            .await
    }
}

impl GmailSync {
    /// Refuses a message from an address Gmail would not send it from:
    /// it sends mail from an address that is not one of its send-as
    /// addresses from the account's own instead (SPEC §8.2).
    async fn check_send_as(&self, from: &str) -> Result<(), SendError> {
        let send_as = self.gmail.send_as().await.map_err(SendError::NotSent)?;
        match send_as
            .iter()
            .find(|s| s.send_as_email.eq_ignore_ascii_case(from))
        {
            Some(s) if s.usable() => Ok(()),
            Some(_) => Err(SendError::Rejected(format!(
                "Gmail's send-as address {from} is not verified yet"
            ))),
            None => Err(SendError::Rejected(format!(
                "{from} is not one of Gmail's send-as addresses \
                 (Settings, Accounts, Send mail as)"
            ))),
        }
    }

    /// The copy of a send whose draft is gone: sent by an earlier attempt
    /// whose answer was lost, or deleted at Gmail. A copy is in the draft's
    /// thread, in Sent, from no earlier than the send was due, and no other
    /// send's; with none, it is not known to have gone out, and with
    /// several, which it is cannot be told.
    async fn find_copy(
        &self,
        account: &Account,
        thread: &str,
        not_before: i64,
    ) -> Result<SentCopy, SendError> {
        let messages = self
            .gmail
            .thread(thread)
            .await
            .map_err(SendError::Unknown)?
            .unwrap_or_default();
        let id = account.id.clone();
        let told = account
            .read(move |conn| compose::told_copies(conn, &id))
            .await
            .map_err(SendError::Unknown)?;
        let mut copies = vec![];
        for message in messages {
            let since_due =
                message.internal_date().map_err(SendError::Unknown)? >= not_before - CLOCK_SKEW_MS;
            if since_due
                && message.label_ids.iter().any(|l| l == "SENT")
                && !told.contains(&message.id)
            {
                copies.push(message.id);
            }
        }
        match <[String; 1]>::try_from(copies) {
            Ok([copy]) => Ok(SentCopy::Id(copy)),
            Err(copies) if copies.is_empty() => {
                Err(SendError::Rejected("its draft is gone from Gmail".into()))
            }
            Err(_) => Ok(SentCopy::Untold),
        }
    }

    /// Records changes since `from`; `false` if history from there is no
    /// longer available. A `listing` under way is told what was recorded.
    async fn poll(
        &self,
        account: &Account,
        from: &str,
        listing: Option<&mut Cursor>,
    ) -> Result<bool> {
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
        let added_ids: Vec<String> = added.iter().map(|e| e.external_id.clone()).collect();
        let sync_state = match &listing {
            Some(cursor) => cursor.sync_state(&latest),
            None => synced(latest.clone()),
        };
        account
            .observe(MailboxObserved {
                account_kind: KIND,
                labels,
                rekeyed: vec![],
                added,
                updated,
                removed: removed.clone(),
                sync_state: Some(sync_state),
            })
            .await?;
        if let Some(cursor) = listing {
            cursor.polled(latest, &added_ids, &removed);
        }
        Ok(true)
    }

    /// Begins recording the whole mailbox, as of the history id read when
    /// the listing began; continues `resumed` if given. A resumed listing
    /// lists anew (Gmail's page tokens do not last) but fetches only what
    /// it has not.
    async fn start_listing(&self, account: &Account, resumed: Option<Listing>) -> Result<Cursor> {
        // Its total, for the progress shown, is as of now.
        let profile = self.gmail.profile().await?;
        let listing = match resumed {
            Some(listing) => listing,
            None => Listing {
                changes_from: profile.history_id,
                before: account.last_entry().await?,
            },
        };
        let cursor = Cursor {
            known: account.known().await?,
            earlier: account.known_up_to(listing.before).await?,
            listing,
            total: profile.messages_total,
            listed: HashSet::new(),
            next: Next::First,
            unlabelled: vec![],
            unfetched: vec![],
        };
        account
            .observe(MailboxObserved {
                labels: Some(self.labels().await?),
                sync_state: Some(cursor.sync_state(&cursor.listing.changes_from)),
                ..observed()
            })
            .await?;
        Ok(cursor)
    }

    /// Takes the listing a step: fetches the labels of a batch of the
    /// earlier messages listed, or a batch of the new ones, or lists the
    /// next page, or, at the end, records what has gone and that the
    /// mailbox is synced. `true` if there is more. The cursor moves only
    /// once a step is recorded, so a failed one is retried.
    async fn list_step(&self, account: &Account, cursor: &mut Cursor) -> Result<bool> {
        if !cursor.unlabelled.is_empty() {
            let batch = cursor.unlabelled[..cursor.unlabelled.len().min(RELABEL_EVERY)].to_vec();
            let fetched = batch.len();
            // Messages deleted since they were listed are recorded as
            // removed by the polls.
            let updated: Vec<EntryLabels> = self
                .fetch_labels(batch)
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
            cursor.unlabelled.drain(..fetched);
            return Ok(true);
        }
        if !cursor.unfetched.is_empty() {
            // Stored a few at a time, as a page takes minutes of quota: so
            // that they show, and pushes and sends go between.
            let batch = cursor.unfetched[..cursor.unfetched.len().min(STORE_EVERY)].to_vec();
            let fetched = batch.len();
            let added = self.fetch_new(account, batch).await?;
            if !added.is_empty() {
                account
                    .observe(MailboxObserved {
                        added,
                        ..observed()
                    })
                    .await?;
            }
            cursor.unfetched.drain(..fetched);
            return Ok(true);
        }
        let token = match &cursor.next {
            Next::First => None,
            Next::Page(token) => Some(token.clone()),
            Next::Done => {
                account
                    .observe(MailboxObserved {
                        removed: cursor.known.difference(&cursor.listed).cloned().collect(),
                        sync_state: Some(synced(cursor.listing.changes_from.clone())),
                        ..observed()
                    })
                    .await?;
                return Ok(false);
            }
        };
        let page = self.gmail.messages(token.as_deref()).await?;
        let ids: Vec<String> = page.messages.into_iter().map(|m| m.id).collect();
        cursor.unlabelled = ids
            .iter()
            .filter(|id| cursor.earlier.contains(*id))
            .cloned()
            .collect();
        cursor.unfetched = ids
            .iter()
            .filter(|id| !cursor.known.contains(*id))
            .cloned()
            .collect();
        cursor.listed.extend(ids);
        cursor.next = page.next_page_token.map_or(Next::Done, Next::Page);
        Ok(true)
    }

    /// Pushes label changes not yet in effect at Gmail.
    async fn push_labels(&self, account: &Account) -> Result<()> {
        let pending = account.pending().await?;
        if pending.is_empty() {
            return Ok(());
        }
        // Entries needing the same change go in one request.
        struct Entry {
            external_id: String,
            add: Vec<String>,
            remove: Vec<String>,
            changes: Vec<LabelChange>,
        }
        let mut per_entry: BTreeMap<EntryId, Entry> = BTreeMap::new();
        for p in pending {
            let entry = per_entry.entry(p.entry).or_insert_with(|| Entry {
                external_id: p.external_id,
                add: vec![],
                remove: vec![],
                changes: vec![],
            });
            if p.desired {
                &mut entry.add
            } else {
                &mut entry.remove
            }
            .push(p.label_external_id);
            entry.changes.push(LabelChange {
                entry: p.entry,
                label: p.label,
                desired: p.desired,
            });
        }
        let mut per_change: BTreeMap<(Vec<String>, Vec<String>), Vec<Entry>> = BTreeMap::new();
        for entry in per_entry.into_values() {
            per_change
                .entry((entry.add.clone(), entry.remove.clone()))
                .or_default()
                .push(entry);
        }
        let mut pushed = vec![];
        let mut refused: BTreeMap<String, Vec<LabelChange>> = BTreeMap::new();
        for ((add, remove), entries) in per_change {
            // A batch refused is split until the entries refused are found.
            let mut batches: Vec<&[Entry]> = entries.chunks(BATCH_MODIFY_MAX).rev().collect();
            while let Some(batch) = batches.pop() {
                let ids: Vec<String> = batch.iter().map(|e| e.external_id.clone()).collect();
                let outcome = self
                    .gmail
                    .batch_modify(&BatchModify {
                        ids: &ids,
                        add_label_ids: &add,
                        remove_label_ids: &remove,
                    })
                    .await?;
                match outcome {
                    Ok(()) => pushed.extend(batch.iter().flat_map(|e| e.changes.iter().copied())),
                    Err(reason) if batch.len() == 1 => refused
                        .entry(reason)
                        .or_default()
                        .extend(batch[0].changes.iter().copied()),
                    Err(_) => {
                        let (first, second) = batch.split_at(batch.len() / 2);
                        batches.push(second);
                        batches.push(first);
                    }
                }
            }
        }
        for (reason, changes) in refused {
            account.observe(LabelsRefused { changes, reason }).await?;
        }
        if !pushed.is_empty() {
            account.observe(LabelsPushed { changes: pushed }).await?;
        }
        Ok(())
    }

    async fn labels(&self) -> Result<Vec<ObservedLabel>> {
        Ok(self
            .gmail
            .labels()
            .await?
            .into_iter()
            .map(|l| ObservedLabel {
                system: l.kind.as_deref() == Some("system"),
                kind: LabelKind::Tag,
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
        rekeyed: vec![],
        added: vec![],
        updated: vec![],
        removed: vec![],
        sync_state: None,
    }
}
