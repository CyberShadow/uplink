//! The sync of a Microsoft 365 account (SPEC §8.3–8.4): folder delta
//! polling, woken by IDLE on Exchange's IMAP as the Inbox changes; pushes
//! by moving and updating messages, and sending.
//!
//! Folders are labels of which an entry has exactly one; the well-known
//! ones take the ids of the Gmail system labels that mean the same, so
//! the rest of uplink treats both alike. Read, flagged and high importance
//! are the labels UNREAD, STARRED and IMPORTANT, and categories are labels
//! `cat:<name>`.

use super::api::{
    Changed, Delta, Failed, Flag, FlagStatus, Graph, Importance, Message, MessageUpdate,
};
use crate::{
    account::{Account, Outgoing, Pending, Provider, PushEnded, SendError, SyncTask},
    compose::{SendDrafted, SentCopy},
    imap::watch::{Watched, Watcher},
    mail_server::sign_in::{self, SignIn},
    mailbox::{self, EntryLabels, MailboxObserved, NewEntry, ObservedLabel},
    watching::Told,
};
use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::OnceLock,
    time::Duration,
};
use tokio_rustls::TlsConnector;
use uplink_config::M365AccountConfig;
use uplink_protocol::{
    mail::{EntryId, LabelKind},
    TxLogId,
};
use uplink_tx_log::{reconcile::Ended, Dispatcher};

pub const KIND: &str = "m365";
/// Messages fetched at once.
const CONCURRENT_FETCHES: usize = 8;
/// New entries recorded per log entry.
const BATCH: usize = 100;

/// The well-known folders uplink shows, by Graph's name for them, and the
/// external ids that stand for them.
const WELL_KNOWN: [(&str, &str); 6] = [
    ("inbox", "INBOX"),
    ("drafts", "DRAFT"),
    ("sentitems", "SENT"),
    ("deleteditems", "TRASH"),
    ("junkemail", "SPAM"),
    ("archive", "ARCHIVE"),
];
/// Mail on its way out; not shown, as Gmail does not show it either.
const OUTBOX: &str = "outbox";
const UNREAD: &str = "UNREAD";
const STARRED: &str = "STARRED";
const IMPORTANT: &str = "IMPORTANT";
const CATEGORY: &str = "cat:";

/// `account.sync_state` of a Microsoft 365 account: each folder is
/// recorded up to its delta link.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncState {
    /// Delta links, by folder id.
    folders: BTreeMap<String, String>,
}

/// The sync task of `config`'s account, reaching it through `graph`,
/// watching it by IMAP, if the account opts in to that, with a token of
/// the same refresh token, and storing mail under `data_root`.
pub fn task(
    config: &M365AccountConfig,
    graph: Graph,
    dispatcher: Dispatcher,
    data_root: &Path,
) -> Result<SyncTask<M365Sync>> {
    let exchange = match watched(config) {
        Some(watched) => Some(ExchangeImap {
            watched,
            sign_in: SignIn::OAuth(
                graph
                    .credentials()
                    .redeemed_as(super::oauth::imap_grant(config)),
            ),
            tls: sign_in::tls(vec![])?,
        }),
        None => None,
    };
    Ok(SyncTask::new(
        M365Sync::new(graph, exchange),
        config.id.clone(),
        config.identities(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        data_root,
        Duration::from_secs(config.poll_interval_secs.into()),
    ))
}

/// What of the account is watched, if the account opts in to IMAP: its
/// Inbox, at Exchange's IMAP server, signed in as its address.
pub fn watched(config: &M365AccountConfig) -> Option<Watched> {
    Some(Watched {
        account: config.id.clone(),
        server: config.imap.clone()?,
        user: config.address.clone(),
        folders: vec![],
    })
}

/// Where the Inbox is watched, by IDLE, and how it is reached (SPEC §8.3).
pub struct ExchangeImap {
    pub watched: Watched,
    /// By a token for Exchange.
    pub sign_in: SignIn,
    pub tls: TlsConnector,
}

pub struct M365Sync {
    graph: Graph,
    /// If the account opts in to IMAP; else it is only polled.
    exchange: Option<ExchangeImap>,
    /// Begun as the task first waits.
    watcher: OnceLock<Watcher>,
}

impl M365Sync {
    pub fn new(graph: Graph, exchange: Option<ExchangeImap>) -> Self {
        Self {
            graph,
            exchange,
            watcher: OnceLock::new(),
        }
    }
}

/// A folder uplink shows.
#[derive(Debug)]
struct Folder {
    /// Graph's.
    id: String,
    external_id: String,
    /// The path of display names, for those not well-known.
    name: String,
    well_known: bool,
}

/// A message as a delta query, or a fetch, reported it.
#[derive(Clone)]
struct Seen {
    message: Message,
    /// The external id of its folder.
    folder: String,
}

impl Provider for M365Sync {
    const KIND: &'static str = KIND;
    // Filed as it is accepted (SPEC §6.7).
    const FILES_SENT: bool = true;

    /// Records all of the changes in one call: a first sync blocks pushes
    /// and sends until done (SPEC §8.3), unlike Gmail's.
    async fn sync(&self, account: &Account) -> Result<bool> {
        let as_of = account.as_of().await?;
        let folders = self.folders().await?;
        let state = account.sync_state::<SyncState>().await?.unwrap_or_default();

        // Every folder's changes, before deciding what left the mailbox:
        // a message moved is gone from one folder and new in another.
        let mut seen: HashMap<String, Seen> = HashMap::new();
        let mut doubtful = BTreeSet::new();
        let mut left = vec![];
        let mut listed = BTreeSet::new();
        let mut links = BTreeMap::new();
        for folder in &folders {
            let (items, link, full) = self.delta(folder, state.folders.get(&folder.id)).await?;
            for item in items {
                match item {
                    Delta::Message(message) => {
                        let id = message.id.clone();
                        let folder = folder.external_id.clone();
                        if seen.insert(id.clone(), Seen { message, folder }).is_some() {
                            doubtful.insert(id);
                        }
                    }
                    Delta::Removed { id, .. } => left.push((id, folder.external_id.clone())),
                    // Fetched whole, as those doubtful are.
                    Delta::Changed { id } => {
                        doubtful.insert(id);
                    }
                }
            }
            if full {
                listed.insert(folder.external_id.clone());
            }
            links.insert(folder.id.clone(), link);
        }

        // Where the account's entries were last seen.
        let known = known_folders(account).await?;
        for (id, from) in left {
            // Unless it was last seen in another folder already.
            if !seen.contains_key(&id) && known.get(&id).is_some_and(|f| f.as_ref() == Some(&from))
            {
                doubtful.insert(id);
            }
        }
        // A folder listed anew lists all it has.
        for (id, folder) in &known {
            if !seen.contains_key(id) && folder.as_ref().is_none_or(|f| listed.contains(f)) {
                doubtful.insert(id.clone());
            }
        }
        let mut removed = vec![];
        for (id, message) in self.verify(&folders, doubtful).await? {
            match message {
                Some(seen_now) => {
                    seen.insert(id, seen_now);
                }
                None => {
                    seen.remove(&id);
                    if known.contains_key(&id) {
                        removed.push(id);
                    }
                }
            }
        }

        let (old, new): (Vec<Seen>, Vec<Seen>) = seen
            .into_values()
            .partition(|s| known.contains_key(&s.message.id));
        let updated: Vec<EntryLabels> = old
            .iter()
            .map(|s| EntryLabels {
                external_id: s.message.id.clone(),
                labels: labels_of(s),
            })
            .collect();
        let labels = self
            .labels(account, &folders, old.iter().chain(&new))
            .await?;
        let sync_state = SyncState { folders: links };
        if labels.is_none()
            && new.is_empty()
            && updated.is_empty()
            && removed.is_empty()
            && sync_state == state
        {
            return Ok(false);
        }

        let mut labels = labels;
        // The first listing (or one resumed: delta links are recorded only
        // as it ends) says how many messages it is storing, for its
        // progress; what is new is fetched in batches, each recorded.
        if state.folders.is_empty() && !new.is_empty() {
            let total = known.len() - removed.len() + new.len();
            account
                .observe(MailboxObserved {
                    labels: labels.take(),
                    sync_state: Some(mailbox::SyncState::Listing {
                        state: serde_json::to_value(&state).expect("sync state serializes"),
                        total: Some(u32::try_from(total).unwrap_or(u32::MAX)),
                    }),
                    ..observed(as_of)
                })
                .await?;
        }
        let mut new = new;
        // Oldest first, so the newest are the last recorded.
        new.sort_by(|a, b| {
            a.message
                .received_date_time
                .cmp(&b.message.received_date_time)
        });
        for batch in new.chunks(BATCH) {
            let added = self.fetch_new(account, batch).await?;
            account
                .observe(MailboxObserved {
                    labels: labels.take(),
                    added,
                    ..observed(as_of)
                })
                .await?;
        }
        account
            .observe(MailboxObserved {
                labels,
                updated,
                removed,
                sync_state: Some(mailbox::SyncState::Synced(
                    serde_json::to_value(sync_state).expect("sync state serializes"),
                )),
                ..observed(as_of)
            })
            .await
            .map(|()| false)
    }

    async fn push(&self, account: &Account, claimed: Vec<Pending>) -> Vec<PushEnded> {
        let mut per_entry: BTreeMap<EntryId, Vec<Pending>> = BTreeMap::new();
        for p in claimed {
            per_entry.entry(p.entry).or_default().push(p);
        }
        let mut ended = vec![];
        let mut failed = false;
        for (entry, pending) in per_entry {
            if failed {
                ended.push(PushEnded::of(
                    &pending,
                    Ended::NotApplied { refused: None },
                    None,
                ));
                continue;
            }
            let pushed = self.push_entry(account, entry, &pending).await;
            failed = pushed.iter().any(|p| p.error.is_some());
            ended.extend(pushed);
        }
        ended
    }

    async fn read(&self, account: &Account, entries: Vec<(EntryId, String)>) -> Result<()> {
        let as_of = account.as_of().await?;
        let folders = self.folders().await?;
        let ids = entries.into_iter().map(|(_, id)| id).collect();
        let mut seen = vec![];
        let mut removed = vec![];
        for (id, message) in self.verify(&folders, ids).await? {
            match message {
                Some(message) => seen.push(message),
                None => removed.push(id),
            }
        }
        let updated = seen
            .iter()
            .map(|s| EntryLabels {
                external_id: s.message.id.clone(),
                labels: labels_of(s),
            })
            .collect();
        account
            .observe(MailboxObserved {
                labels: self.labels(account, &folders, seen.iter()).await?,
                updated,
                removed,
                ..observed(as_of)
            })
            .await
    }

    /// In two steps (SPEC §8.4): the message is made a draft, recorded,
    /// then sent by its id. Graph threads it by its headers.
    async fn send(&self, account: &Account, mail: Outgoing<'_>) -> Result<SentCopy, SendError> {
        let draft = match mail.provider_draft {
            Some(draft) => draft.to_owned(),
            None => {
                // Made by an attempt whose outcome was not known, if any.
                let found = self
                    .graph
                    .find_draft(mail.message_id)
                    .await
                    .map_err(SendError::NotSent)?;
                let draft = match found {
                    Some(draft) => draft,
                    None => self.graph.create_draft(mail.raw).await?,
                };
                account
                    .observe(SendDrafted {
                        item: mail.item,
                        provider_draft: draft.clone(),
                        provider_thread: None,
                    })
                    .await
                    .map_err(SendError::NotSent)?;
                draft
            }
        };
        self.graph.send_draft(&draft).await?;
        // Graph keeps our Message-ID (SPEC §8.4).
        Ok(SentCopy::OurMessageId)
    }

    /// What IDLE on Exchange's IMAP tells of the Inbox, watched from the
    /// first wait on: Graph's delta of each folder is then taken, as for
    /// a poll. Never, for an account not opted in to IMAP.
    async fn told(&self) -> Told {
        let Some(exchange) = &self.exchange else {
            return std::future::pending().await;
        };
        self.watcher
            .get_or_init(|| {
                Watcher::start(
                    exchange.watched.clone(),
                    exchange.sign_in.clone(),
                    exchange.tls.clone(),
                )
            })
            .told()
            .await
    }
}

impl M365Sync {
    /// The folders uplink shows: all but the Outbox and what it holds.
    async fn folders(&self) -> Result<Vec<Folder>> {
        let mut well_known = HashMap::new();
        for (name, external_id) in WELL_KNOWN {
            let folder = self
                .graph
                .folder(name)
                .await?
                .with_context(|| format!("the mailbox has no {name} folder"))?;
            well_known.insert(folder.id, external_id);
        }
        let outbox = self.graph.folder(OUTBOX).await?.map(|f| f.id);
        let mut folders = vec![];
        let mut parents: Vec<(Option<String>, Option<String>)> = vec![(None, None)];
        while let Some((parent, path)) = parents.pop() {
            for folder in self.graph.folders(parent.as_deref()).await? {
                if Some(&folder.id) == outbox.as_ref() {
                    continue;
                }
                let path = match &path {
                    Some(path) => format!("{path}/{}", folder.display_name),
                    None => folder.display_name,
                };
                if folder.child_folder_count > 0 {
                    parents.push((Some(folder.id.clone()), Some(path.clone())));
                }
                folders.push(match well_known.get(&folder.id) {
                    Some(external_id) => Folder {
                        external_id: (*external_id).to_owned(),
                        name: (*external_id).to_owned(),
                        id: folder.id,
                        well_known: true,
                    },
                    None => Folder {
                        external_id: folder.id.clone(),
                        name: path,
                        id: folder.id,
                        well_known: false,
                    },
                });
            }
        }
        Ok(folders)
    }

    /// The folder's changes from `link`, the delta link it now has, and
    /// whether the changes are all it has (it was listed anew).
    async fn delta(
        &self,
        folder: &Folder,
        link: Option<&String>,
    ) -> Result<(Vec<Delta>, String, bool)> {
        let start = self.graph.delta_start(&folder.id);
        let (mut url, mut full) = match link {
            Some(link) => (link.clone(), false),
            None => (start.clone(), true),
        };
        let mut items = vec![];
        loop {
            let Some(page) = self.graph.delta::<Delta>(&url).await? else {
                if full {
                    bail!("Graph refused to list folder {}", folder.name);
                }
                tracing::info!(
                    folder = folder.name,
                    "delta link expired; listing the folder"
                );
                (url, full) = (start.clone(), true);
                items.clear();
                continue;
            };
            items.extend(page.value);
            match (page.next_link, page.delta_link) {
                (Some(next), _) => url = next,
                (None, Some(link)) => return Ok((items, link, full)),
                (None, None) => bail!("a delta page of folder {} has no link", folder.name),
            }
        }
    }

    /// Where the messages are now; `None` for those gone, or in a folder
    /// uplink doesn't show.
    async fn verify(
        &self,
        folders: &[Folder],
        ids: BTreeSet<String>,
    ) -> Result<Vec<(String, Option<Seen>)>> {
        stream::iter(ids)
            .map(|id| async move {
                let seen = self.graph.message(&id).await?.and_then(|message| {
                    let folder = folders.iter().find(|f| f.id == message.parent_folder_id)?;
                    Some(Seen {
                        folder: folder.external_id.clone(),
                        message,
                    })
                });
                Ok((id, seen))
            })
            .buffered(CONCURRENT_FETCHES)
            .try_collect()
            .await
    }

    /// All the account's labels, if they are not as recorded: the folders,
    /// the state labels, and the categories recorded or `seen`.
    async fn labels(
        &self,
        account: &Account,
        folders: &[Folder],
        seen: impl Iterator<Item = &Seen>,
    ) -> Result<Option<Vec<ObservedLabel>>> {
        let recorded = account.labels().await?;
        let mut categories: BTreeSet<String> = recorded
            .iter()
            .filter(|l| l.external_id.starts_with(CATEGORY))
            .map(|l| l.external_id.clone())
            .collect();
        categories.extend(seen.flat_map(|s| {
            s.message
                .categories
                .iter()
                .map(|c| format!("{CATEGORY}{c}"))
        }));
        let mut labels: Vec<ObservedLabel> = folders
            .iter()
            .map(|f| ObservedLabel {
                external_id: f.external_id.clone(),
                name: f.name.clone(),
                system: f.well_known,
                kind: LabelKind::Folder,
            })
            .chain([UNREAD, STARRED, IMPORTANT].map(|l| ObservedLabel {
                external_id: l.to_owned(),
                name: l.to_owned(),
                system: true,
                kind: LabelKind::Tag,
            }))
            .chain(categories.into_iter().map(|c| ObservedLabel {
                name: c[CATEGORY.len()..].to_owned(),
                external_id: c,
                system: false,
                kind: LabelKind::Tag,
            }))
            .collect();
        labels.sort_by(|a, b| a.external_id.cmp(&b.external_id));
        Ok((labels != recorded).then_some(labels))
    }

    /// Fetches and stores messages new to uplink; skips those deleted
    /// meanwhile.
    async fn fetch_new(&self, account: &Account, batch: &[Seen]) -> Result<Vec<NewEntry>> {
        let entries: Vec<Option<NewEntry>> = stream::iter(batch.to_vec())
            .map(|seen| async move {
                let Some(raw) = self.graph.mime(&seen.message.id).await? else {
                    return anyhow::Ok(None);
                };
                let (variant, parsed, size_bytes) = account.store_raw(raw).await?;
                let received = &seen.message.received_date_time;
                let received_at = mail_parser::DateTime::parse_rfc3339(received)
                    .with_context(|| format!("receivedDateTime {received}"))?
                    .to_timestamp()
                    * 1000;
                Ok(Some(NewEntry {
                    external_id: seen.message.id.clone(),
                    thread_id: seen.message.conversation_id.clone(),
                    received_at,
                    labels: labels_of(&seen),
                    variant,
                    size_bytes,
                    parsed,
                }))
            })
            .buffered(CONCURRENT_FETCHES)
            .try_collect()
            .await?;
        Ok(entries.into_iter().flatten().collect())
    }

    /// Makes the entry's changes claimed at Graph: a folder by moving the
    /// message, the rest by updating it; how each request ended. Once one
    /// fails, the other is not sent.
    async fn push_entry(
        &self,
        account: &Account,
        entry: EntryId,
        pending: &[Pending],
    ) -> Vec<PushEnded> {
        let id = &pending[0].external_id;
        let (moving, updating): (Vec<Pending>, Vec<Pending>) = pending
            .iter()
            .cloned()
            .partition(|p| p.kind == LabelKind::Folder);
        let mut update = MessageUpdate::default();
        let mut categories = false;
        for p in &updating {
            match p.label_external_id.as_str() {
                UNREAD => update.is_read = Some(!p.desired),
                STARRED => {
                    update.flag = Some(Flag {
                        flag_status: if p.desired {
                            FlagStatus::Flagged
                        } else {
                            FlagStatus::NotFlagged
                        },
                    })
                }
                IMPORTANT => {
                    update.importance = Some(if p.desired {
                        Importance::High
                    } else {
                        Importance::Normal
                    })
                }
                l if l.starts_with(CATEGORY) => categories = true,
                l => {
                    return vec![PushEnded::refused(
                        pending,
                        format!("label {l} is not one Microsoft 365 has"),
                    )]
                }
            }
        }
        let mut ended = vec![];
        if !updating.is_empty() {
            if categories {
                // Graph takes them all: as the provider has them, but for
                // those claimed.
                let observed = match account.observed_labels(entry).await {
                    Ok(observed) => observed,
                    Err(e) => return vec![PushEnded::not_sent(pending, e)],
                };
                let mut all: BTreeSet<String> = observed
                    .into_iter()
                    .filter(|l| l.starts_with(CATEGORY))
                    .collect();
                for p in updating
                    .iter()
                    .filter(|p| p.label_external_id.starts_with(CATEGORY))
                {
                    if p.desired {
                        all.insert(p.label_external_id.clone());
                    } else {
                        all.remove(&p.label_external_id);
                    }
                }
                update.categories = Some(
                    all.iter()
                        .map(|l| l.strip_prefix(CATEGORY).expect("a category").to_owned())
                        .collect(),
                );
            }
            let pushed = changed(&updating, self.graph.update(id, &update).await);
            let failed = pushed.error.is_some();
            ended.push(pushed);
            if failed {
                ended.push(PushEnded::of(
                    &moving,
                    Ended::NotApplied { refused: None },
                    None,
                ));
                return ended;
            }
        }
        if let Some(into) = moving.iter().find(|p| p.desired) {
            let folder = WELL_KNOWN
                .iter()
                .find(|(_, external_id)| *external_id == into.label_external_id)
                .map_or(into.label_external_id.as_str(), |(name, _)| name);
            ended.push(changed(&moving, self.graph.move_to(id, folder).await));
        } else if !moving.is_empty() {
            // Out of a folder, but into none: SetLabels puts it somewhere.
            ended.push(PushEnded::refused(
                &moving,
                "a message is in one folder at a time".to_owned(),
            ));
        }
        ended
    }
}

/// How a Graph request pushing `pending` ended. A message gone meanwhile
/// is recorded as removed by the next sync.
fn changed(pending: &[Pending], result: Result<Changed, Failed>) -> PushEnded {
    match result {
        Ok(Changed::Done) => PushEnded::of(pending, Ended::Applied, None),
        Ok(Changed::Gone) => PushEnded::of(pending, Ended::NotApplied { refused: None }, None),
        Ok(Changed::Refused(reason)) => PushEnded::refused(pending, reason),
        Err(Failed::NotDone(e)) => PushEnded::not_sent(pending, e),
        Err(Failed::Unknown(e)) => PushEnded::unknown(pending, e),
    }
}

/// The labels a message has.
fn labels_of(seen: &Seen) -> Vec<String> {
    let message = &seen.message;
    let mut labels = vec![seen.folder.clone()];
    if !message.is_read {
        labels.push(UNREAD.to_owned());
    }
    if message.flag.flag_status == FlagStatus::Flagged {
        labels.push(STARRED.to_owned());
    }
    if message.importance == Importance::High {
        labels.push(IMPORTANT.to_owned());
    }
    labels.extend(message.categories.iter().map(|c| format!("{CATEGORY}{c}")));
    labels
}

/// The account's entries, by external id, and the folder each was last
/// seen in.
async fn known_folders(account: &Account) -> Result<HashMap<String, Option<String>>> {
    let id = account.id.clone();
    account
        .read(move |conn| {
            Ok(conn
                .prepare(
                    "SELECT e.external_id,
                       (SELECT l.external_id FROM entry_label el JOIN label l ON l.id = el.label
                        WHERE el.entry = e.id AND el.observed AND l.kind = 'folder')
                     FROM mail_entry e WHERE e.account_id = ?1",
                )?
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?)
        })
        .await
}

/// An observation of nothing yet, by requests sent after `as_of`.
fn observed(as_of: TxLogId) -> MailboxObserved {
    MailboxObserved {
        account_kind: KIND,
        labels: None,
        rekeyed: vec![],
        added: vec![],
        updated: vec![],
        removed: vec![],
        sync_state: None,
        as_of,
    }
}
