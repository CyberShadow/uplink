//! The sync of an IMAP account (SPEC §8.10): folders polled for changes,
//! pushes by storing flags and moving messages, and sending by SMTP, the
//! copy then filed in Sent.
//!
//! Folders are labels of which an entry has exactly one, as Microsoft
//! 365's are; the special-use ones (RFC 6154), or failing that those of
//! the usual names, take the ids of the Gmail system labels that mean the
//! same, so the rest of uplink treats all providers alike. `\Seen` unset
//! is UNREAD, `\Flagged` is STARRED, `$Important` (RFC 8457) is
//! IMPORTANT, and other keywords are labels `kw:<keyword>`.
//!
//! An entry is a message in a folder, keyed by the folder's name, its
//! UIDVALIDITY and the message's UID ([`Key`]); a message moved, by uplink
//! or another client, gets a new UID in its new folder, so a message gone
//! from one folder and of the same Message-ID new in another is the same
//! entry, re-keyed, rather than one removed and one added: what was done
//! to it, and its undo, stay with it.

use super::{
    names,
    session::{self, Connection},
    watch::{Watched, Watcher},
};
use crate::watching::Told;
use crate::{
    account::{Account, Outgoing, Pending, Provider, PushEnded, SendError, SyncTask},
    compose::SentCopy,
    mail_server::{
        self,
        sign_in::{self, SignIn},
        smtp,
    },
    mailbox::{self, EntryLabels, MailboxObserved, NewEntry, ObservedLabel, Rekeyed},
};
use anyhow::{ensure, Context, Result};
use async_imap::{
    error::Error as ImapError,
    types::{Fetch, Flag, Name, NameAttribute},
};
use futures_util::TryStreamExt;
use mail_parser::MessageParser;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use uplink_config::ImapAccountConfig;
use uplink_protocol::{
    mail::{EntryId, LabelKind},
    TxLogId,
};
use uplink_storage::seal::MasterKey;
use uplink_tx_log::{reconcile::Ended, Dispatcher};

pub const KIND: &str = "imap";
/// How often to poll for changes.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Messages fetched in a step: a first sync takes many, between which
/// pushes and sends go.
const BATCH: usize = 50;

const INBOX: &str = "INBOX";
const UNREAD: &str = "UNREAD";
const STARRED: &str = "STARRED";
const IMPORTANT: &str = "IMPORTANT";
const KEYWORD: &str = "kw:";
/// The prefix of the external ids of folders not special-use.
const MAILBOX: &str = "mailbox:";

/// The special-use folders, by external id: their attribute, and the
/// names that stand for them where no folder has it; the first is the one
/// created, if there is none.
const ROLES: [(&str, NameAttribute<'static>, &[&str]); 5] = [
    (
        "SENT",
        NameAttribute::Sent,
        &["Sent", "Sent Items", "Sent Messages", "Sent Mail"],
    ),
    ("DRAFT", NameAttribute::Drafts, &["Drafts"]),
    (
        "TRASH",
        NameAttribute::Trash,
        &["Trash", "Deleted Items", "Deleted Messages"],
    ),
    (
        "SPAM",
        NameAttribute::Junk,
        &["Junk", "Spam", "Junk Mail", "Junk E-mail"],
    ),
    ("ARCHIVE", NameAttribute::Archive, &["Archive", "Archives"]),
];

/// `account.sync_state` of an IMAP account: the folders as last recorded
/// in full.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncState {
    /// Whether the first sync is still listing the mailbox.
    listing: bool,
    /// By folder name.
    folders: BTreeMap<String, FolderState>,
}

/// What the server says of a folder (`STATUS`); a folder whose state is
/// as recorded, by a server with CONDSTORE, has not changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderState {
    uidvalidity: u32,
    uidnext: u32,
    messages: u32,
    highestmodseq: Option<u64>,
}

/// The sync task of `config`'s account, signing in with what `key`
/// unseals, storing mail under `data_root`.
pub fn task(
    config: &ImapAccountConfig,
    key: Arc<MasterKey>,
    http: reqwest::Client,
    dispatcher: Dispatcher,
    data_root: &Path,
) -> Result<SyncTask<ImapSync>> {
    Ok(SyncTask::new(
        ImapSync::new(
            config.clone(),
            SignIn::of(config.login(), key, http),
            sign_in::tls(vec![])?,
        ),
        config.id.clone(),
        config.identities(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        data_root,
        POLL_INTERVAL,
    ))
}

pub struct ImapSync {
    config: ImapAccountConfig,
    sign_in: SignIn,
    tls: TlsConnector,
    /// Kept between steps; dropped when a step fails, as what state it is
    /// in is then not known.
    connection: Mutex<Option<Connection>>,
    /// Begun as the task first waits.
    watcher: OnceLock<Watcher>,
}

/// An entry's external id: where the message is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    uidvalidity: u32,
    uid: u32,
    folder: String,
}

impl Key {
    fn parse(external_id: &str) -> Result<Self> {
        let mut parts = external_id.splitn(3, ':');
        let mut next = || {
            parts
                .next()
                .with_context(|| format!("entry id {external_id} is not an IMAP one"))
        };
        Ok(Key {
            uidvalidity: next()?.parse()?,
            uid: next()?.parse()?,
            folder: next()?.to_owned(),
        })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.uidvalidity, self.uid, self.folder)
    }
}

/// A folder uplink shows, or would create to file mail in.
#[derive(Debug)]
struct Folder {
    /// As the server names it.
    name: String,
    external_id: String,
    /// Its decoded path, `/`-separated.
    display: String,
    system: bool,
    /// Whether the server has it; a special-use folder it has not is
    /// created when first needed.
    exists: bool,
}

/// An entry as recorded.
struct Known {
    external_id: String,
    key: Key,
    message_id: String,
    /// Its observed labels' external ids.
    labels: BTreeSet<String>,
}

/// A message new to uplink, in a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct New {
    /// In the folder list.
    folder: usize,
    uidvalidity: u32,
    uid: u32,
}

impl Provider for ImapSync {
    const KIND: &'static str = KIND;
    // SMTP leaves filing to the client, which does so only once the
    // message is accepted.
    const FILES_SENT: bool = false;

    async fn sync(&self, account: &Account) -> Result<bool> {
        let mut slot = self.connected().await?;
        let result = self
            .sync_with(slot.as_mut().expect("connected"), account)
            .await;
        forget_if_failed(&mut slot, &result);
        result
    }

    async fn push(&self, account: &Account, claimed: Vec<Pending>) -> Vec<PushEnded> {
        let mut slot = match self.connected().await {
            Ok(slot) => slot,
            Err(e) => return vec![PushEnded::not_sent(&claimed, e)],
        };
        let c = slot.as_mut().expect("connected");
        let ended = self.push_with(c, account, claimed).await;
        // A request failed: the connection may not be good for another.
        if ended.iter().any(|e| e.error.is_some()) {
            *slot = None;
        }
        ended
    }

    /// By fetching their flags where they are, by UID. One no longer
    /// there (moved, or its folder's UIDs no longer valid) is left for the
    /// next sync, which re-keys it.
    async fn read(&self, account: &Account, entries: Vec<(EntryId, String)>) -> Result<()> {
        let as_of = account.as_of().await?;
        let mut slot = self.connected().await?;
        let result = self
            .read_with(slot.as_mut().expect("connected"), account, entries, as_of)
            .await;
        forget_if_failed(&mut slot, &result);
        result
    }

    /// Submits the message by SMTP, then files it in Sent; the copy there
    /// becomes its entry when synced.
    async fn send(&self, _account: &Account, mail: Outgoing<'_>) -> Result<SentCopy, SendError> {
        smtp::send(
            self.config.login(),
            &self.config.smtp,
            &self.sign_in,
            &self.tls,
            mail.from,
            mail.raw,
        )
        .await?;
        // Sent: if it cannot be filed, it stays listed as waiting for its
        // copy, which is so.
        if let Err(e) = self.file_sent(mail.raw).await {
            tracing::warn!(
                account = self.config.id,
                "filing sent mail {} in Sent: {e:#}",
                mail.message_id
            );
        }
        Ok(SentCopy::OurMessageId)
    }

    /// What the server tells of its folders, watched from the first wait
    /// on.
    async fn told(&self) -> Told {
        self.watcher
            .get_or_init(|| {
                Watcher::start(
                    Watched {
                        account: self.config.id.clone(),
                        server: self.config.imap.clone(),
                        user: self.config.login().user.to_owned(),
                        folders: self.config.watch.clone(),
                    },
                    self.sign_in.clone(),
                    self.tls.clone(),
                )
            })
            .told()
            .await
    }
}

impl ImapSync {
    /// Reaching `config`'s servers with `tls`.
    pub fn new(config: ImapAccountConfig, sign_in: SignIn, tls: TlsConnector) -> Self {
        Self {
            config,
            sign_in,
            tls,
            connection: Mutex::new(None),
            watcher: OnceLock::new(),
        }
    }

    /// The connection, made if there is none.
    async fn connected(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Connection>>> {
        let mut slot = self.connection.lock().await;
        if slot.is_none() {
            let user = self.config.login().user;
            *slot =
                Some(session::connect(&self.config.imap, user, &self.sign_in, &self.tls).await?);
        }
        Ok(slot)
    }

    async fn file_sent(&self, raw: &[u8]) -> Result<()> {
        let mut slot = self.connected().await?;
        let c = slot.as_mut().expect("connected");
        let result = async {
            let folders = folders(c).await?;
            let sent = ensure_exists(c, folder(&folders, "SENT")?).await?;
            c.session
                .append(sent, Some("(\\Seen)"), None, raw)
                .await
                .context("appending to Sent")
        }
        .await;
        forget_if_failed(&mut slot, &result);
        result
    }

    async fn sync_with(&self, c: &mut Connection, account: &Account) -> Result<bool> {
        let as_of = account.as_of().await?;
        let folders = folders(c).await?;
        let state = account.sync_state::<SyncState>().await?;
        let first = state.as_ref().is_none_or(|s| s.listing);
        let state = state.unwrap_or_default();
        let known = known(account).await?;
        let mut by_folder: HashMap<&str, Vec<&Known>> = HashMap::new();
        for k in &known {
            by_folder.entry(&k.key.folder).or_default().push(k);
        }

        let mut vanished: Vec<&Known> = vec![];
        let mut new: Vec<New> = vec![];
        let mut updated: Vec<EntryLabels> = vec![];
        let mut keywords: BTreeSet<String> = BTreeSet::new();
        let mut states = BTreeMap::new();
        let mut total = 0;
        refresh(c).await?;
        for (i, folder) in folders.iter().enumerate().filter(|(_, f)| f.exists) {
            let here = by_folder.remove(folder.name.as_str()).unwrap_or_default();
            let now = status(c, &folder.name).await?;
            total += now.messages;
            let prev = state.folders.get(&folder.name);
            states.insert(folder.name.clone(), now.clone());
            if c.condstore && prev == Some(&now) {
                continue;
            }
            let (valid, stale): (Vec<&Known>, Vec<&Known>) = here
                .into_iter()
                .partition(|k| k.key.uidvalidity == now.uidvalidity);
            vanished.extend(stale);
            let known_here: HashMap<u32, &Known> = valid.iter().map(|k| (k.key.uid, *k)).collect();
            if c.condstore {
                c.session.select_condstore(&folder.name).await?;
            } else {
                c.session.select(&folder.name).await?;
            }
            let current: HashSet<u32> = if now.messages == 0 {
                HashSet::new()
            } else {
                c.session.uid_search("UNDELETED").await?
            };
            vanished.extend(valid.iter().filter(|k| !current.contains(&k.key.uid)));
            let mut fresh: Vec<u32> = current
                .iter()
                .filter(|uid| !known_here.contains_key(uid))
                .copied()
                .collect();
            // Newest first.
            fresh.sort_unstable_by(|a, b| b.cmp(a));
            new.extend(fresh.into_iter().map(|uid| New {
                folder: i,
                uidvalidity: now.uidvalidity,
                uid,
            }));
            if known_here.is_empty() {
                continue;
            }
            let since = match prev {
                Some(prev) if c.condstore && prev.uidvalidity == now.uidvalidity => {
                    prev.highestmodseq
                }
                _ => None,
            };
            let query = match since {
                Some(modseq) => format!("(UID FLAGS) (CHANGEDSINCE {modseq})"),
                None => "(UID FLAGS)".to_owned(),
            };
            for fetch in fetch(c, "1:*", &query).await? {
                let uid = fetch.uid.context("a FETCH without its UID")?;
                let Some(k) = known_here.get(&uid).filter(|_| current.contains(&uid)) else {
                    continue;
                };
                let labels = labels_of(folder, &fetch);
                keywords.extend(labels.iter().filter(|l| l.starts_with(KEYWORD)).cloned());
                if labels.iter().cloned().collect::<BTreeSet<_>>() != k.labels {
                    updated.push(EntryLabels {
                        external_id: k.external_id.clone(),
                        labels,
                    });
                }
            }
        }
        // Of folders gone.
        vanished.extend(by_folder.into_values().flatten());

        // Folders are looked at one by one, so a message moved meanwhile
        // may be seen gone from one but not yet in the other, or the
        // other way round, which would be its removal and another's
        // addition rather than its move. So a step that saw messages come
        // or go stands only if no folder changed while it looked; else it
        // is taken again.
        if !vanished.is_empty() || !new.is_empty() {
            refresh(c).await?;
            for (name, seen) in &states {
                if status(c, name).await? != *seen {
                    return Ok(true);
                }
            }
        }

        // A message gone from one folder and of its Message-ID new in
        // another moved.
        let mut rekeyed = vec![];
        if !vanished.is_empty() && !new.is_empty() {
            let mut by_message_id: HashMap<&str, Vec<&Known>> = HashMap::new();
            for k in &vanished {
                by_message_id.entry(&k.message_id).or_default().push(k);
            }
            let mut moved = HashSet::new();
            for (folder, uids) in by_folder_of(&new) {
                let folder = &folders[folder];
                select(c, &folder.name).await?;
                let query = "(UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])";
                for fetch in fetch(c, &uid_set(uids.iter().map(|n| n.uid)), query).await? {
                    let uid = fetch.uid.context("a FETCH without its UID")?;
                    let Some(n) = uids.iter().find(|n| n.uid == uid) else {
                        continue;
                    };
                    let message_id = fetch.header().and_then(|h| {
                        MessageParser::new()
                            .with_message_ids()
                            .parse_headers(h)?
                            .message_id()
                            .map(str::to_owned)
                    });
                    let Some(was) = message_id
                        .and_then(|id| by_message_id.get_mut(id.as_str()))
                        .and_then(Vec::pop)
                    else {
                        continue;
                    };
                    let to = Key {
                        uidvalidity: n.uidvalidity,
                        uid,
                        folder: folder.name.clone(),
                    }
                    .to_string();
                    let labels = labels_of(folder, &fetch);
                    keywords.extend(labels.iter().filter(|l| l.starts_with(KEYWORD)).cloned());
                    rekeyed.push(Rekeyed {
                        from: was.external_id.clone(),
                        to: to.clone(),
                    });
                    updated.push(EntryLabels {
                        external_id: to,
                        labels,
                    });
                    moved.insert(*n);
                }
            }
            let rekeyed_from: HashSet<&str> = rekeyed.iter().map(|r| r.from.as_str()).collect();
            vanished.retain(|k| !rekeyed_from.contains(k.external_id.as_str()));
            new.retain(|n| !moved.contains(n));
        }
        let removed: Vec<String> = vanished.iter().map(|k| k.external_id.clone()).collect();

        // INBOX first, as the user looks there first.
        new.sort_by_key(|n| (folders[n.folder].external_id != INBOX, n.folder));
        let rest = new.split_off(new.len().min(BATCH));
        let more = !rest.is_empty();
        // A folder not fetched in full is recorded as it was, so that what
        // it has left is listed again.
        for n in &rest {
            let name = &folders[n.folder].name;
            match state.folders.get(name) {
                Some(prev) => states.insert(name.clone(), prev.clone()),
                None => states.remove(name),
            };
        }
        let added = self.fetch_new(c, account, &folders, &new).await?;
        keywords.extend(
            added
                .iter()
                .flat_map(|a| &a.labels)
                .filter(|l| l.starts_with(KEYWORD))
                .cloned(),
        );

        let labels = labels(account, &folders, keywords).await?;
        let sync_state = SyncState {
            listing: first && more,
            folders: states,
        };
        if labels.is_none()
            && rekeyed.is_empty()
            && added.is_empty()
            && updated.is_empty()
            && removed.is_empty()
            && sync_state == state
        {
            return Ok(false);
        }
        let value = serde_json::to_value(&sync_state).expect("sync state serializes");
        account
            .observe(MailboxObserved {
                account_kind: KIND,
                labels,
                rekeyed,
                added,
                updated,
                removed,
                sync_state: Some(if sync_state.listing {
                    mailbox::SyncState::Listing {
                        state: value,
                        total: Some(total),
                    }
                } else {
                    mailbox::SyncState::Synced(value)
                }),
                as_of,
            })
            .await?;
        Ok(more)
    }

    /// Fetches and stores messages new to uplink; skips those expunged
    /// meanwhile.
    async fn fetch_new(
        &self,
        c: &mut Connection,
        account: &Account,
        folders: &[Folder],
        new: &[New],
    ) -> Result<Vec<NewEntry>> {
        let mut entries = vec![];
        for (folder, uids) in by_folder_of(new) {
            let folder = &folders[folder];
            select(c, &folder.name).await?;
            let query = "(UID FLAGS INTERNALDATE BODY.PEEK[])";
            for fetch in fetch(c, &uid_set(uids.iter().map(|n| n.uid)), query).await? {
                let uid = fetch.uid.context("a FETCH without its UID")?;
                let Some(n) = uids.iter().find(|n| n.uid == uid) else {
                    continue;
                };
                let raw = fetch
                    .body()
                    .with_context(|| format!("message {uid} of {} has no body", folder.name))?
                    .to_vec();
                let received_at = fetch
                    .internal_date()
                    .with_context(|| format!("message {uid} of {} has no date", folder.name))?
                    .timestamp_millis();
                let labels = labels_of(folder, &fetch);
                let thread_id = mail_server::thread_of(&raw);
                let (variant, parsed, size_bytes) = account.store_raw(raw).await?;
                entries.push(NewEntry {
                    external_id: Key {
                        uidvalidity: n.uidvalidity,
                        uid,
                        folder: folder.name.clone(),
                    }
                    .to_string(),
                    thread_id,
                    received_at,
                    labels,
                    variant,
                    size_bytes,
                    parsed,
                });
            }
        }
        // Oldest first, so the newest are the last recorded.
        entries.sort_by_key(|e| e.received_at);
        Ok(entries)
    }

    /// Pushes the changes claimed, by entry, in turn; once a request
    /// fails, the rest are not sent.
    async fn push_with(
        &self,
        c: &mut Connection,
        account: &Account,
        claimed: Vec<Pending>,
    ) -> Vec<PushEnded> {
        let folders = match folders(c).await {
            Ok(folders) => folders,
            Err(e) => return vec![PushEnded::not_sent(&claimed, e)],
        };
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
            let pushed = self.push_entry(c, account, &folders, entry, &pending).await;
            failed = pushed.iter().any(|p| p.error.is_some());
            ended.extend(pushed);
        }
        ended
    }

    async fn read_with(
        &self,
        c: &mut Connection,
        account: &Account,
        entries: Vec<(EntryId, String)>,
        as_of: TxLogId,
    ) -> Result<()> {
        let folders = folders(c).await?;
        let mut by_folder: BTreeMap<String, Vec<(Key, String)>> = BTreeMap::new();
        for (_, external_id) in entries {
            let key = Key::parse(&external_id)?;
            by_folder
                .entry(key.folder.clone())
                .or_default()
                .push((key, external_id));
        }
        let mut updated = vec![];
        let mut keywords = BTreeSet::new();
        for (name, keys) in by_folder {
            let Some(folder) = folders.iter().find(|f| f.name == name && f.exists) else {
                continue;
            };
            let uidvalidity = match select(c, &name).await {
                Ok(uidvalidity) => uidvalidity,
                // Gone meanwhile.
                Err(ImapError::No(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            let valid: Vec<&(Key, String)> = keys
                .iter()
                .filter(|(k, _)| k.uidvalidity == uidvalidity)
                .collect();
            if valid.is_empty() {
                continue;
            }
            let uids = uid_set(valid.iter().map(|(k, _)| k.uid));
            for fetch in fetch(c, &uids, "(UID FLAGS)").await? {
                let uid = fetch.uid.context("a FETCH without its UID")?;
                let Some((_, external_id)) = valid.iter().find(|(k, _)| k.uid == uid) else {
                    continue;
                };
                let labels = labels_of(folder, &fetch);
                keywords.extend(labels.iter().filter(|l| l.starts_with(KEYWORD)).cloned());
                updated.push(EntryLabels {
                    external_id: external_id.clone(),
                    labels,
                });
            }
        }
        if updated.is_empty() {
            return Ok(());
        }
        let labels = labels(account, &folders, keywords).await?;
        account
            .observe(MailboxObserved {
                account_kind: KIND,
                labels,
                rekeyed: vec![],
                added: vec![],
                updated,
                removed: vec![],
                sync_state: None,
                as_of,
            })
            .await
    }

    /// Makes the entry's changes claimed at the server: its flags stored,
    /// then the message moved, if its folder is to change; how each
    /// ended. Once one fails, the other is not sent. A message whose key no
    /// longer names it (its folder gone, or its UIDs no longer valid) is
    /// not changed: the next sync re-keys or removes it.
    async fn push_entry(
        &self,
        c: &mut Connection,
        account: &Account,
        folders: &[Folder],
        entry: EntryId,
        pending: &[Pending],
    ) -> Vec<PushEnded> {
        let (moving, storing): (Vec<Pending>, Vec<Pending>) = pending
            .iter()
            .cloned()
            .partition(|p| p.kind == LabelKind::Folder);
        let key = match Key::parse(&pending[0].external_id) {
            Ok(key) => key,
            Err(e) => return vec![PushEnded::not_sent(pending, e)],
        };
        match select(c, &key.folder).await {
            Ok(uidvalidity) if uidvalidity == key.uidvalidity => {}
            Ok(_) | Err(ImapError::No(_)) => {
                return vec![PushEnded::of(
                    pending,
                    Ended::NotApplied { refused: None },
                    None,
                )]
            }
            Err(e) => return vec![PushEnded::not_sent(pending, e.into())],
        }
        let uid = key.uid.to_string();
        let mut ended = vec![];
        if !storing.is_empty() {
            let stored = store(c, &uid, &storing).await;
            let failed = stored.iter().any(|p| p.error.is_some());
            ended.extend(stored);
            if failed {
                ended.push(PushEnded::of(
                    &moving,
                    Ended::NotApplied { refused: None },
                    None,
                ));
                return ended;
            }
        }
        let Some(into) = moving.iter().find(|p| p.desired) else {
            if !moving.is_empty() {
                // Out of a folder, but into none: SetLabels puts it somewhere.
                ended.push(PushEnded::refused(
                    &moving,
                    "a message is in one folder at a time".to_owned(),
                ));
            }
            return ended;
        };
        let target = match folder(folders, &into.label_external_id) {
            Ok(target) => target,
            Err(e) => {
                ended.push(PushEnded::refused(&moving, format!("{e:#}")));
                return ended;
            }
        };
        let name = match ensure_exists(c, target).await {
            Ok(name) => name.to_owned(),
            Err(e) => {
                ended.push(PushEnded::not_sent(&moving, e));
                return ended;
            }
        };
        let moved = if c.moves {
            copy_uid(c, &format!("UID MOVE {uid} {}", quoted(&name))).await
        } else if c.uidplus {
            move_by_copy(c, &uid, &name).await
        } else {
            ended.push(PushEnded::refused(
                &moving,
                "the server can move messages only by expunging all deleted mail".into(),
            ));
            return ended;
        };
        ended.push(match moved {
            Ok(copied) => {
                let to = copied.map(|(uidvalidity, uid)| {
                    Key {
                        uidvalidity,
                        uid,
                        folder: name.clone(),
                    }
                    .to_string()
                });
                tracing::debug!(account = account.id, ?entry, "moved {key} to {to:?}");
                PushEnded::rekeyed(&moving, to)
            }
            Err(ImapError::No(reason)) => PushEnded::refused(&moving, reason),
            Err(e) => PushEnded::unknown(&moving, e.into()),
        });
        ended
    }
}

/// Stores the flags of the changes claimed of the message selected, by
/// UID: those added, then those removed, each a request of its own; how
/// each ended. Once one fails, the other is not sent.
async fn store(c: &mut Connection, uid: &str, storing: &[Pending]) -> Vec<PushEnded> {
    let mut add: (Vec<Pending>, Vec<String>) = (vec![], vec![]);
    let mut remove: (Vec<Pending>, Vec<String>) = (vec![], vec![]);
    for p in storing {
        let id = p.label_external_id.as_str();
        let (flag, set) = match id {
            UNREAD => ("\\Seen".to_owned(), !p.desired),
            STARRED => ("\\Flagged".to_owned(), p.desired),
            IMPORTANT => ("$Important".to_owned(), p.desired),
            _ => match id.strip_prefix(KEYWORD) {
                Some(keyword) => (keyword.to_owned(), p.desired),
                None => {
                    return vec![PushEnded::refused(
                        storing,
                        format!("label {id} is not one an IMAP server has"),
                    )]
                }
            },
        };
        let (pending, flags) = if set { &mut add } else { &mut remove };
        pending.push(p.clone());
        flags.push(flag);
    }
    let mut ended = vec![];
    let mut failed = false;
    for (sign, (pending, flags)) in [("+", add), ("-", remove)] {
        if pending.is_empty() {
            continue;
        }
        if failed {
            ended.push(PushEnded::of(
                &pending,
                Ended::NotApplied { refused: None },
                None,
            ));
            continue;
        }
        let query = format!("{sign}FLAGS.SILENT ({})", flags.join(" "));
        let stored = match c.session.uid_store(uid, &query).await {
            Ok(stream) => stream.try_collect::<Vec<_>>().await.map(drop),
            Err(e) => Err(e),
        };
        ended.push(match stored {
            Ok(()) => PushEnded::of(&pending, Ended::Applied, None),
            Err(ImapError::No(reason)) => PushEnded::refused(&pending, reason),
            Err(e) => {
                failed = true;
                PushEnded::unknown(&pending, e.into())
            }
        });
    }
    ended
}

/// Runs `command`, a COPY or MOVE of one message, and returns where it
/// went by the COPYUID it answers with (RFC 4315; MOVE's, RFC 6851 §4.3):
/// the target's UIDVALIDITY and the copy's UID. `None` if it does not
/// say.
async fn copy_uid(c: &mut Connection, command: &str) -> Result<Option<(u32, u32)>, ImapError> {
    use async_imap::imap_proto::{rfc4315::UidSetMember, Outcome, Response, ResponseCode, Status};
    let of = |outcome: &Outcome| match &outcome.code {
        Some(ResponseCode::CopyUid(uidvalidity, _, to)) => match &to[..] {
            [UidSetMember::Uid(uid)] => Some((*uidvalidity, *uid)),
            [UidSetMember::UidRange(uids)] if uids.start() == uids.end() => {
                Some((*uidvalidity, *uids.start()))
            }
            _ => None,
        },
        _ => None,
    };
    let tag = c.session.run_command(command).await?;
    let mut copied = None;
    loop {
        let response = c
            .session
            .read_response()
            .await?
            .ok_or(ImapError::ConnectionLost)?;
        match response.parsed() {
            Response::Data {
                status: Status::Ok,
                outcome,
            } => copied = copied.or(of(outcome)),
            Response::Done {
                tag: done,
                status,
                outcome,
            } if *done == tag => {
                let information = outcome
                    .information
                    .as_deref()
                    .unwrap_or_default()
                    .to_owned();
                return match status {
                    Status::Ok => Ok(copied.or(of(outcome))),
                    Status::No => Err(ImapError::No(information)),
                    _ => Err(ImapError::Bad(information)),
                };
            }
            _ => {}
        }
    }
}

/// A mailbox name as an IMAP string.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// MOVE, without the extension: a copy, and the message expunged; where
/// it went, as [`copy_uid`] tells.
async fn move_by_copy(
    c: &mut Connection,
    uid: &str,
    target: &str,
) -> Result<Option<(u32, u32)>, ImapError> {
    let copied = copy_uid(c, &format!("UID COPY {uid} {}", quoted(target))).await?;
    c.session
        .uid_store(uid, "+FLAGS.SILENT (\\Deleted)")
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    c.session
        .uid_expunge(uid)
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    Ok(copied)
}

fn forget_if_failed<T>(slot: &mut Option<Connection>, result: &Result<T>) {
    if result.is_err() {
        *slot = None;
    }
}

/// A listed mailbox's path, as uplink names a folder: its name decoded,
/// `/` between levels.
pub(super) fn path(name: &Name) -> Result<String> {
    let path = names::decode(name.name())?;
    Ok(match name.delimiter() {
        Some(delimiter) if delimiter != "/" => path.replace(delimiter, "/"),
        _ => path,
    })
}

/// The folders uplink shows: all that can hold mail, but for the virtual
/// ones that show what others hold (`\All`, `\Flagged`); and the
/// special-use ones the server lacks, to be created when needed.
async fn folders(c: &mut Connection) -> Result<Vec<Folder>> {
    let names: Vec<_> = c
        .session
        .list(Some(""), Some("*"))
        .await?
        .try_collect()
        .await?;
    let mut listed = vec![];
    for name in &names {
        let attributes = name.attributes();
        if attributes.iter().any(|a| {
            matches!(
                a,
                NameAttribute::NoSelect | NameAttribute::All | NameAttribute::Flagged
            ) || matches!(a, NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\NonExistent"))
        }) {
            continue;
        }
        listed.push((name.name().to_owned(), path(name)?, attributes.to_vec()));
    }
    // A role is the folder with its attribute, else the first of its names.
    let mut roles: HashMap<String, &str> = HashMap::new();
    for (id, attribute, _) in &ROLES {
        if let Some((name, ..)) = listed.iter().find(|(_, _, a)| a.contains(attribute)) {
            roles.entry(name.clone()).or_insert(id);
        }
    }
    for (id, attribute, fallbacks) in &ROLES {
        if roles.values().any(|r| r == id) || listed.iter().any(|(_, _, a)| a.contains(attribute)) {
            continue;
        }
        let found = fallbacks.iter().find_map(|fallback| {
            listed
                .iter()
                .find(|(name, ..)| name.eq_ignore_ascii_case(fallback) && !roles.contains_key(name))
        });
        if let Some((name, ..)) = found {
            roles.insert(name.clone(), id);
        }
    }
    let mut folders: Vec<Folder> = listed
        .into_iter()
        .map(|(name, display, _)| {
            let role = if name.eq_ignore_ascii_case(INBOX) {
                Some(INBOX)
            } else {
                roles.get(&name).copied()
            };
            match role {
                Some(id) => Folder {
                    external_id: id.to_owned(),
                    display: id.to_owned(),
                    system: true,
                    exists: true,
                    name,
                },
                None => Folder {
                    external_id: format!("{MAILBOX}{name}"),
                    display,
                    system: false,
                    exists: true,
                    name,
                },
            }
        })
        .collect();
    for (id, _, fallbacks) in &ROLES {
        if !folders.iter().any(|f| f.external_id == *id) {
            folders.push(Folder {
                name: fallbacks[0].to_owned(),
                external_id: (*id).to_owned(),
                display: (*id).to_owned(),
                system: true,
                exists: false,
            });
        }
    }
    ensure!(
        folders.iter().any(|f| f.external_id == INBOX),
        "the server lists no INBOX"
    );
    Ok(folders)
}

/// The folder of `external_id`.
fn folder<'a>(folders: &'a [Folder], external_id: &str) -> Result<&'a Folder> {
    folders
        .iter()
        .find(|f| f.external_id == external_id)
        .with_context(|| format!("no folder {external_id}"))
}

/// The folder's name, made at the server if it has not got it.
async fn ensure_exists<'a>(c: &mut Connection, folder: &'a Folder) -> Result<&'a str> {
    if !folder.exists {
        c.session
            .create(&folder.name)
            .await
            .with_context(|| format!("creating folder {}", folder.name))?;
    }
    Ok(&folder.name)
}

/// Selects the folder; returns its UIDVALIDITY.
async fn select(c: &mut Connection, name: &str) -> Result<u32, ImapError> {
    let mailbox = c.session.select(name).await?;
    mailbox
        .uid_validity
        .ok_or_else(|| ImapError::Bad(format!("{name} has no UIDVALIDITY")))
}

/// Brings what the connection sees of the folder selected, if any, up to
/// date, so that STATUS tells of it as it is: of the folder selected, a
/// server may answer as the connection last saw it (Dovecot does; RFC 9051
/// §6.3.11 asks clients not to ask).
async fn refresh(c: &mut Connection) -> Result<()> {
    c.session.noop().await?;
    Ok(())
}

async fn status(c: &mut Connection, name: &str) -> Result<FolderState> {
    let items = if c.condstore {
        "(MESSAGES UIDNEXT UIDVALIDITY HIGHESTMODSEQ)"
    } else {
        "(MESSAGES UIDNEXT UIDVALIDITY)"
    };
    let mailbox = c
        .session
        .status(name, items)
        .await
        .with_context(|| format!("STATUS of {name}"))?;
    Ok(FolderState {
        uidvalidity: mailbox
            .uid_validity
            .with_context(|| format!("{name} has no UIDVALIDITY"))?,
        uidnext: mailbox
            .uid_next
            .with_context(|| format!("{name} has no UIDNEXT"))?,
        messages: mailbox.exists,
        highestmodseq: mailbox.highest_modseq,
    })
}

async fn fetch(c: &mut Connection, uids: &str, query: &str) -> Result<Vec<Fetch>> {
    Ok(c.session
        .uid_fetch(uids, query)
        .await?
        .try_collect()
        .await?)
}

/// `new`, by folder.
fn by_folder_of(new: &[New]) -> BTreeMap<usize, Vec<New>> {
    let mut by_folder: BTreeMap<usize, Vec<New>> = BTreeMap::new();
    for n in new {
        by_folder.entry(n.folder).or_default().push(*n);
    }
    by_folder
}

/// A UID set of `uids`, as ranges.
fn uid_set(uids: impl Iterator<Item = u32>) -> String {
    let mut uids: Vec<u32> = uids.collect();
    uids.sort_unstable();
    let mut ranges: Vec<(u32, u32)> = vec![];
    for uid in uids {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == uid => *end = uid,
            _ => ranges.push((uid, uid)),
        }
    }
    ranges
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}:{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The labels of a message in `folder` with the flags `fetch` reports.
fn labels_of(folder: &Folder, fetch: &Fetch) -> Vec<String> {
    let mut labels = vec![folder.external_id.clone()];
    let mut seen = false;
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => seen = true,
            Flag::Flagged => labels.push(STARRED.to_owned()),
            // Flags are of any case (RFC 9051 §2.3.2). Stalwart stores
            // `$Important` and reports it as `\Important`, the name of the
            // mailbox attribute of the same RFC.
            Flag::Custom(keyword)
                if keyword.eq_ignore_ascii_case("$Important")
                    || keyword.eq_ignore_ascii_case("\\Important") =>
            {
                labels.push(IMPORTANT.to_owned())
            }
            // Keywords of clients' conventions ($Forwarded, $Junk, …) are
            // theirs.
            Flag::Custom(keyword) if !keyword.starts_with(['$', '\\']) => {
                labels.push(format!("{KEYWORD}{keyword}"))
            }
            _ => {}
        }
    }
    if !seen {
        labels.push(UNREAD.to_owned());
    }
    labels
}

/// The account's entries as recorded.
async fn known(account: &Account) -> Result<Vec<Known>> {
    let id = account.id.clone();
    let rows: Vec<(String, String, Option<String>)> = account
        .read(move |conn| {
            Ok(conn
                .prepare(
                    "SELECT e.external_id, m.message_id,
                       (SELECT group_concat(l.external_id, char(10))
                        FROM entry_label el JOIN label l ON l.id = el.label
                        WHERE el.entry = e.id AND el.observed)
                     FROM mail_entry e JOIN message m ON m.id = e.message
                     WHERE e.account_id = ?1",
                )?
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?)
        })
        .await?;
    rows.into_iter()
        .map(|(external_id, message_id, labels)| {
            Ok(Known {
                key: Key::parse(&external_id)?,
                external_id,
                message_id,
                labels: labels
                    .iter()
                    .flat_map(|l| l.split('\n'))
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect()
}

/// All the account's labels, if they are not as recorded: the folders,
/// the state labels, and the keywords recorded or `seen`.
async fn labels(
    account: &Account,
    folders: &[Folder],
    seen: BTreeSet<String>,
) -> Result<Option<Vec<ObservedLabel>>> {
    let recorded = account.labels().await?;
    let mut keywords: BTreeSet<String> = recorded
        .iter()
        .filter(|l| l.external_id.starts_with(KEYWORD))
        .map(|l| l.external_id.clone())
        .collect();
    keywords.extend(seen);
    let mut labels: Vec<ObservedLabel> = folders
        .iter()
        .map(|f| ObservedLabel {
            external_id: f.external_id.clone(),
            name: f.display.clone(),
            system: f.system,
            kind: LabelKind::Folder,
        })
        .chain([UNREAD, STARRED, IMPORTANT].map(|l| ObservedLabel {
            external_id: l.to_owned(),
            name: l.to_owned(),
            system: true,
            kind: LabelKind::Tag,
        }))
        .chain(keywords.into_iter().map(|k| ObservedLabel {
            name: k[KEYWORD.len()..].to_owned(),
            external_id: k,
            system: false,
            kind: LabelKind::Tag,
        }))
        .collect();
    labels.sort_by(|a, b| a.external_id.cmp(&b.external_id));
    Ok((labels != recorded).then_some(labels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        let key = Key {
            uidvalidity: 7,
            uid: 42,
            folder: "Lists:Rust/Users".into(),
        };
        assert_eq!(key.to_string(), "7:42:Lists:Rust/Users");
        assert_eq!(Key::parse(&key.to_string()).unwrap(), key);
        assert!(Key::parse("not-imap").is_err());
    }

    #[test]
    fn uid_sets_are_ranges() {
        assert_eq!(uid_set([5, 1, 2, 3, 9, 10].into_iter()), "1:3,5,9:10");
        assert_eq!(uid_set([4].into_iter()), "4");
    }
}
