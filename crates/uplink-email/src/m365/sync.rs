//! The sync of a Microsoft 365 account (SPEC §8.3–8.4): folder delta
//! polling, pushes by moving and updating messages, and sending.
//!
//! Folders are labels of which an entry has exactly one; the well-known
//! ones take the ids of the Gmail system labels that mean the same, so
//! the rest of uplink treats both alike. Read, flagged and high importance
//! are the labels UNREAD, STARRED and IMPORTANT, and categories are labels
//! `cat:<name>`.

use super::{
    api::{Delta, Flag, FlagStatus, Graph, Importance, Message, MessageUpdate},
    oauth,
};
use crate::{
    account::{Account, Pending, Provider, SendError, SyncTask},
    mailbox::{EntryLabels, LabelChange, LabelsPushed, MailboxObserved, NewEntry, ObservedLabel},
    oauth::Credentials,
};
use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
    time::Duration,
};
use uplink_config::M365AccountConfig;
use uplink_protocol::mail::EntryId;
use uplink_storage::seal::MasterKey;
use uplink_tx_log::Dispatcher;

pub const KIND: &str = "m365";
/// How often to poll for changes (SPEC §8.3).
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
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

/// The sync task of `config`'s account, storing mail under `data_root`.
pub fn task(
    config: &M365AccountConfig,
    key: Arc<MasterKey>,
    http: reqwest::Client,
    dispatcher: Dispatcher,
    data_root: &Path,
) -> Result<SyncTask<M365Sync>> {
    let credentials = Credentials::new(http.clone(), oauth::grant(config), key);
    Ok(SyncTask::new(
        M365Sync {
            graph: Graph::new(http, &config.oauth.resource, credentials),
        },
        config.id.clone(),
        config.address.clone(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        data_root,
        POLL_INTERVAL,
    ))
}

pub struct M365Sync {
    pub graph: Graph,
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

    async fn sync(&self, account: &Account) -> Result<()> {
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
            return Ok(());
        }

        let mut labels = labels;
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
                    ..observed()
                })
                .await?;
        }
        account
            .observe(MailboxObserved {
                labels,
                updated,
                removed,
                sync_state: Some(serde_json::to_value(sync_state).expect("sync state serializes")),
                ..observed()
            })
            .await
    }

    async fn push(&self, account: &Account) -> Result<()> {
        let pending = account.pending().await?;
        if pending.is_empty() {
            return Ok(());
        }
        let mut per_entry: BTreeMap<EntryId, Vec<Pending>> = BTreeMap::new();
        for p in pending {
            per_entry.entry(p.entry).or_default().push(p);
        }
        let mut changes = vec![];
        for (entry, pending) in per_entry {
            self.push_entry(account, entry, &pending).await?;
            changes.extend(pending.into_iter().map(|p| LabelChange {
                entry: p.entry,
                label: p.label,
                desired: p.desired,
            }));
        }
        account.observe(LabelsPushed { changes }).await
    }

    async fn send(&self, raw: &[u8], _thread: Option<&str>) -> Result<(), SendError> {
        // Graph threads a message by its headers.
        self.graph.send_mail(raw).await
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
            let Some(page) = self.graph.delta(&url).await? else {
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
        let recorded = recorded_labels(account).await?;
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
                folder: true,
            })
            .chain([UNREAD, STARRED, IMPORTANT].map(|l| ObservedLabel {
                external_id: l.to_owned(),
                name: l.to_owned(),
                system: true,
                folder: false,
            }))
            .chain(categories.into_iter().map(|c| ObservedLabel {
                name: c[CATEGORY.len()..].to_owned(),
                external_id: c,
                system: false,
                folder: false,
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

    /// Makes the entry's pending changes at Graph: a folder by moving the
    /// message, the rest by updating it.
    async fn push_entry(
        &self,
        account: &Account,
        entry: EntryId,
        pending: &[Pending],
    ) -> Result<()> {
        let id = &pending[0].external_id;
        let mut update = MessageUpdate::default();
        let mut categories = false;
        let mut into = None;
        for p in pending {
            match p.label_external_id.as_str() {
                _ if p.folder => {
                    if p.desired {
                        into = Some(&p.label_external_id);
                    }
                }
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
                l => bail!("label {l} is not one Microsoft 365 has"),
            }
        }
        if categories {
            update.categories = Some(
                account
                    .desired_labels(entry)
                    .await?
                    .into_iter()
                    .filter_map(|l| l.strip_prefix(CATEGORY).map(str::to_owned))
                    .collect(),
            );
        }
        // A message deleted meanwhile is recorded as removed by the next
        // sync.
        if !update.is_empty() && !self.graph.update(id, &update).await? {
            return Ok(());
        }
        if let Some(into) = into {
            let folder = WELL_KNOWN
                .iter()
                .find(|(_, external_id)| external_id == into)
                .map_or(into.as_str(), |(name, _)| name);
            self.graph.move_to(id, folder).await?;
        }
        Ok(())
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
                        WHERE el.entry = e.id AND el.observed AND l.folder)
                     FROM mail_entry e WHERE e.account_id = ?1",
                )?
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?)
        })
        .await
}

/// The account's labels as recorded, by external id.
async fn recorded_labels(account: &Account) -> Result<Vec<ObservedLabel>> {
    let id = account.id.clone();
    account
        .read(move |conn| {
            Ok(conn
                .prepare(
                    "SELECT external_id, name, system, folder FROM label
                     WHERE account_id = ?1 ORDER BY external_id",
                )?
                .query_map([id], |r| {
                    Ok(ObservedLabel {
                        external_id: r.get(0)?,
                        name: r.get(1)?,
                        system: r.get(2)?,
                        folder: r.get(3)?,
                    })
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
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
