//! The sync of a JMAP account (SPEC §8.13): changes by state, pushes by
//! `Email/set`, and sending by `EmailSubmission`.
//!
//! Mailboxes are labels, of which an email may have several; but Trash,
//! Junk and the Archive are folders, an email in one of them in nothing
//! else, as RFC 8621 trashes (§4.6). The ones of a role take the ids of the
//! Gmail system labels that mean the same, so the rest of uplink treats
//! all providers alike. `$seen` unset is UNREAD, `$flagged` is STARRED, and
//! other keywords are labels `kw:<keyword>`.
//!
//! Entries are emails, by their ids, which JMAP keeps as they move.

use super::{
    api::{
        Changes, Email, Got, Identity, Imported, Jmap, Mailbox, MethodError, Queried, Request, Set,
        EMAIL_PROPERTIES,
    },
    watch::Watcher,
};
use crate::{
    account::{Account, Outgoing, Pending, Provider, SendError, SyncTask},
    compose::{SendDrafted, SentCopy},
    mail_server::sign_in::SignIn,
    mailbox::{
        EntryLabels, LabelChange, LabelsPushed, LabelsRefused, MailboxObserved, NewEntry,
        ObservedLabel, SyncState,
    },
};
use anyhow::{bail, ensure, Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use uplink_config::JmapAccountConfig;
use uplink_protocol::mail::{EntryId, LabelKind};
use uplink_storage::seal::MasterKey;
use uplink_tx_log::Dispatcher;

pub const KIND: &str = "jmap";
/// How often to poll for changes; the event source tells of them sooner.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Emails fetched, or changes recorded, in a step: a first sync takes
/// many, between which pushes and sends go.
const BATCH: usize = 50;
/// Emails listed at once, at most.
const PAGE: usize = 500;
/// Blobs downloaded at once.
const CONCURRENT_FETCHES: usize = 4;

const INBOX: &str = "INBOX";
const UNREAD: &str = "UNREAD";
const STARRED: &str = "STARRED";
const KEYWORD: &str = "kw:";
/// The prefix of the external ids of mailboxes of no role uplink knows.
const MAILBOX: &str = "mailbox:";

/// A role (RFC 8621 §2, from the IMAP special-use registry) uplink knows.
struct Role {
    role: &'static str,
    /// The external id of its label.
    label: &'static str,
    kind: LabelKind,
    /// The name of the mailbox made for it, if the server has none.
    name: &'static str,
}

/// The roles uplink knows; each but the Inbox's is listed as a label
/// whether or not the server has its mailbox, which is made as an email is
/// first filed in it.
const ROLES: [Role; 6] = [
    Role {
        role: "inbox",
        label: INBOX,
        kind: LabelKind::Mailbox,
        name: "Inbox",
    },
    Role {
        role: "sent",
        label: "SENT",
        kind: LabelKind::Mailbox,
        name: "Sent",
    },
    Role {
        role: "drafts",
        label: "DRAFT",
        kind: LabelKind::Mailbox,
        name: "Drafts",
    },
    Role {
        role: "trash",
        label: "TRASH",
        kind: LabelKind::Folder,
        name: "Trash",
    },
    Role {
        role: "junk",
        label: "SPAM",
        kind: LabelKind::Folder,
        name: "Junk",
    },
    Role {
        role: "archive",
        label: "ARCHIVE",
        kind: LabelKind::Folder,
        name: "Archive",
    },
];
/// Roles of virtual mailboxes, which show what others hold.
const VIRTUAL: [&str; 2] = ["all", "flagged"];

/// `account.sync_state` of a JMAP account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    /// The emails' changes are recorded up to this state; while listing,
    /// as the listing began, then as the polls between its steps went on.
    /// The listing records the emails as they were; the polls, what
    /// changed since.
    emails: String,
    /// While listing: the last entry recorded before the listing began.
    /// Those after it were fetched by the listing, so the polls cover
    /// their changes; those up to it may have changed unseen, and their
    /// labels are taken anew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    listing_before: Option<i64>,
}

/// The sync task of `config`'s account, signing in with what `key`
/// unseals, storing mail under `data_root`.
pub fn task(
    config: &JmapAccountConfig,
    key: Arc<MasterKey>,
    http: reqwest::Client,
    dispatcher: Dispatcher,
    data_root: &Path,
) -> Result<SyncTask<JmapSync>> {
    let sign_in = SignIn::of(config.login(), key, http.clone());
    let jmap = Jmap::new(
        http,
        config.session_url.as_str(),
        config.login().user,
        sign_in,
    );
    Ok(SyncTask::new(
        JmapSync::new(config.id.clone(), jmap),
        config.id.clone(),
        config.identities(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        data_root,
        POLL_INTERVAL,
    ))
}

pub struct JmapSync {
    account: String,
    jmap: Arc<Jmap>,
    /// The listing under way, if one is: where it has got, between steps.
    /// Only in memory: a listing resumed after a restart lists anew,
    /// fetching only what it has not.
    cursor: tokio::sync::Mutex<Option<Cursor>>,
    /// Begun as the task first waits.
    watcher: OnceLock<Watcher>,
}

/// A listing's progress through the emails, newest first.
struct Cursor {
    state: State,
    /// How many emails there were as it began, if the server says, for
    /// the progress shown.
    total: Option<u32>,
    /// What the account has: what it had as the listing began, and what
    /// the polls since added, less what they removed.
    known: HashSet<String>,
    /// What it had before the listing first began.
    earlier: HashSet<String>,
    /// Every email listed so far, or added by a poll: what is known but
    /// not this, at the end, has gone.
    listed: HashSet<String>,
    next: Next,
    /// Of the pages listed, the new emails yet to fetch, in order.
    unfetched: Vec<Email>,
}

impl Cursor {
    /// The account's sync state while listing, its changes recorded up to
    /// `emails`.
    fn sync_state(&self, emails: &str) -> SyncState {
        let state = State {
            emails: emails.to_owned(),
            ..self.state.clone()
        };
        SyncState::Listing {
            state: serde_json::to_value(&state).expect("serializes"),
            total: self.total,
        }
    }

    /// Takes in what a poll recorded, up to `emails`: what it added is
    /// not fetched again, nor what it removed taken as gone again.
    fn polled(&mut self, emails: String, added: &[String], removed: &[String]) {
        self.state.emails = emails;
        for id in removed {
            self.known.remove(id);
        }
        self.known.extend(added.iter().cloned());
        self.listed.extend(added.iter().cloned());
        self.unfetched.retain(|e| !added.contains(&e.id));
    }
}

enum Next {
    First,
    /// The page after this email.
    After(String),
    Done,
}

/// The account's mailboxes, as uplink shows them.
#[derive(Debug)]
struct Mailboxes {
    /// The label each is; none for a virtual one.
    by_id: HashMap<String, Option<String>>,
    /// The labels they are, and those of the roles the server has no
    /// mailbox of.
    labels: Vec<ObservedLabel>,
    /// By label external id.
    by_label: HashMap<String, String>,
}

impl Mailboxes {
    fn of(list: Vec<Mailbox>) -> Result<Self> {
        let names: HashMap<&str, (&str, Option<&str>)> = list
            .iter()
            .map(|m| (m.id.as_str(), (m.name.as_str(), m.parent_id.as_deref())))
            .collect();
        // A role is the first mailbox of it.
        let mut roles: HashMap<&str, &str> = HashMap::new();
        for m in &list {
            if let Some(role) = &m.role {
                roles.entry(role.as_str()).or_insert(&m.id);
            }
        }
        let mut mailboxes = Mailboxes {
            by_id: HashMap::new(),
            labels: vec![],
            by_label: HashMap::new(),
        };
        for m in &list {
            let role = m.role.as_deref().filter(|r| roles[r] == m.id);
            let label = match (role, ROLES.iter().find(|r| Some(r.role) == role)) {
                (_, Some(_)) => continue,
                (Some(role), None) if VIRTUAL.contains(&role) => None,
                _ => {
                    let label = format!("{MAILBOX}{}", m.id);
                    mailboxes.labels.push(ObservedLabel {
                        external_id: label.clone(),
                        name: path(&names, &m.id)?,
                        system: false,
                        kind: LabelKind::Mailbox,
                    });
                    Some(label)
                }
            };
            if let Some(label) = &label {
                mailboxes.by_label.insert(label.clone(), m.id.clone());
            }
            mailboxes.by_id.insert(m.id.clone(), label);
        }
        ensure!(roles.contains_key("inbox"), "the server has no Inbox");
        for role in &ROLES {
            mailboxes.labels.push(ObservedLabel {
                external_id: role.label.to_owned(),
                name: role.label.to_owned(),
                system: true,
                kind: role.kind,
            });
            if let Some(&id) = roles.get(role.role) {
                mailboxes.add(role.label, id.to_owned());
            }
        }
        Ok(mailboxes)
    }

    /// Records that mailbox `id` is `label`'s.
    fn add(&mut self, label: &str, id: String) {
        self.by_label.insert(label.to_owned(), id.clone());
        self.by_id.insert(id, Some(label.to_owned()));
    }

    /// The labels of `email`.
    fn labels_of(&self, email: &Email) -> Result<Vec<String>> {
        let mut labels = vec![];
        for (id, _) in email.mailbox_ids.iter().filter(|(_, &is)| is) {
            let label = self
                .by_id
                .get(id)
                .with_context(|| format!("email {} is in unknown mailbox {id}", email.id))?;
            labels.extend(label.iter().cloned());
        }
        let has = |keyword: &str| email.keywords.get(keyword).copied().unwrap_or(false);
        if !has("$seen") {
            labels.push(UNREAD.to_owned());
        }
        if has("$flagged") {
            labels.push(STARRED.to_owned());
        }
        // Keywords of clients' conventions ($answered, $forwarded, …) are
        // theirs.
        labels.extend(
            email
                .keywords
                .iter()
                .filter(|(k, &is)| is && !k.starts_with('$'))
                .map(|(k, _)| format!("{KEYWORD}{k}")),
        );
        labels.sort();
        Ok(labels)
    }
}

/// A mailbox's name, under its parents', `/`-separated.
fn path(names: &HashMap<&str, (&str, Option<&str>)>, id: &str) -> Result<String> {
    let mut parts = vec![];
    let mut at = Some(id);
    while let Some(id) = at {
        ensure!(parts.len() < names.len(), "mailbox {id}'s parents loop");
        let (name, parent) = names
            .get(id)
            .with_context(|| format!("mailbox {id}'s parent is not listed"))?;
        parts.push(*name);
        at = *parent;
    }
    parts.reverse();
    Ok(parts.join("/"))
}

/// `Mailbox/get` of all, for [`Mailboxes::of`].
fn get_mailboxes(request: &mut Request) -> super::api::Call {
    request.call(
        "Mailbox/get",
        json!({ "ids": null, "properties": ["id", "name", "parentId", "role"] }),
    )
}

impl Provider for JmapSync {
    const KIND: &'static str = KIND;
    // The copy, imported before the submission, is filed in Sent as the
    // server accepts it (`onSuccessUpdateEmail`).
    const FILES_SENT: bool = true;

    /// Polls; and, listing the emails, takes one step: one page listed, or
    /// one batch of its emails fetched.
    async fn sync(&self, account: &Account) -> Result<bool> {
        let mut cursor = self.cursor.lock().await;
        if let Some(listing) = cursor.as_mut() {
            // The polls go on while the listing does, so that it holds up
            // no new mail.
            let from = listing.state.emails.clone();
            match self.poll(account, &from, Some(listing)).await? {
                None => {
                    *cursor = Some(self.start_listing(account, None).await?);
                    return Ok(true);
                }
                Some(true) => return Ok(true),
                Some(false) => {}
            }
            return match self.list_step(account, listing).await {
                Ok(more) => {
                    if !more {
                        *cursor = None;
                    }
                    Ok(more)
                }
                // The email the next page is after is gone: the listing
                // resumes anew, fetching only what it has not.
                Err(e) if method_error(&e) == Some("anchorNotFound") => {
                    *cursor = None;
                    Ok(true)
                }
                Err(e) => Err(e),
            };
        }
        let resumed = match account.sync_state::<State>().await? {
            Some(state) if state.listing_before.is_none() => {
                if let Some(more) = self.poll(account, &state.emails, None).await? {
                    return Ok(more);
                }
                None
            }
            Some(state) => {
                tracing::info!(account = account.id, "resuming the listing of the emails");
                Some(state)
            }
            None => None,
        };
        *cursor = Some(self.start_listing(account, resumed).await?);
        Ok(true)
    }

    async fn push(&self, account: &Account) -> Result<()> {
        let pending = account.pending().await?;
        if pending.is_empty() {
            return Ok(());
        }
        self.push_labels(account, pending).await
    }

    /// In two steps: the message is imported into Drafts, recorded, then
    /// submitted, which files it in Sent; it is its copy. Before submitting
    /// a draft again, its submissions are asked for: a draft submitted is
    /// not submitted twice.
    async fn send(&self, account: &Account, mail: Outgoing<'_>) -> Result<SentCopy, SendError> {
        let draft = match mail.provider_draft {
            Some(draft) => draft.to_owned(),
            None => {
                let draft = self.import_draft(mail.raw).await?;
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
        self.submit(mail.from, &draft).await?;
        Ok(SentCopy::Id(draft))
    }

    /// What the event source tells, watched from the first wait on.
    async fn changed(&self) {
        self.watcher
            .get_or_init(|| Watcher::start(self.account.clone(), self.jmap.clone()))
            .changed()
            .await
    }
}

/// The type of the method error `e` is, if it is one.
fn method_error(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<MethodError>().map(|e| e.kind.as_str())
}

impl JmapSync {
    /// `account`'s sync, at `jmap`.
    pub fn new(account: String, jmap: Jmap) -> Self {
        Self {
            account,
            jmap: Arc::new(jmap),
            cursor: tokio::sync::Mutex::new(None),
            watcher: OnceLock::new(),
        }
    }

    /// Records the changes since `from`, a batch of them; whether more
    /// remain, or `None` if the server cannot tell them from there. A
    /// `listing` under way is told what was recorded.
    async fn poll(
        &self,
        account: &Account,
        from: &str,
        listing: Option<&mut Cursor>,
    ) -> Result<Option<bool>> {
        let mut request = Request::default();
        let mailboxes = get_mailboxes(&mut request);
        let changes = request.call(
            "Email/changes",
            json!({ "sinceState": from, "maxChanges": BATCH }),
        );
        let properties = EMAIL_PROPERTIES;
        let created = request.call(
            "Email/get",
            json!({ "#ids": request.result_of(changes, "/created"), "properties": properties }),
        );
        let updated = request.call(
            "Email/get",
            json!({ "#ids": request.result_of(changes, "/updated"), "properties": properties }),
        );
        let mut responses = self.jmap.call(request).await?;
        let changes: Changes = match responses.take(changes) {
            Err(e) if method_error(&e) == Some("cannotCalculateChanges") => {
                tracing::info!(account = account.id, "changes are lost; syncing fully");
                return Ok(None);
            }
            changes => changes?,
        };
        let mailboxes = Mailboxes::of(responses.take::<Got<Mailbox>>(mailboxes)?.list)?;
        let created: Got<Email> = responses.take(created)?;
        let updated: Got<Email> = responses.take(updated)?;
        let known = account.known().await?;
        // Created or updated, an email new to uplink is fetched, and one it
        // has is relabelled: which it is, the server may say either way.
        let (old, new): (Vec<Email>, Vec<Email>) = created
            .list
            .into_iter()
            .chain(updated.list)
            .partition(|e| known.contains(&e.id));
        let mut keywords = BTreeSet::new();
        let updated = old
            .iter()
            .map(|email| {
                let labels = mailboxes.labels_of(email)?;
                keywords.extend(keywords_of(&labels));
                Ok(EntryLabels {
                    external_id: email.id.clone(),
                    labels,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let added = self.fetch_new(account, &mailboxes, new).await?;
        keywords.extend(added.iter().flat_map(|a| keywords_of(&a.labels)));
        // Of what was destroyed, what uplink had: not one created and
        // destroyed since.
        let removed: Vec<String> = changes
            .destroyed
            .into_iter()
            .filter(|id| known.contains(id))
            .collect();
        let labels = labels(account, &mailboxes, keywords).await?;
        if labels.is_none()
            && added.is_empty()
            && updated.is_empty()
            && removed.is_empty()
            && changes.new_state == from
        {
            return Ok(Some(false));
        }
        let added_ids: Vec<String> = added.iter().map(|e| e.external_id.clone()).collect();
        let sync_state = match &listing {
            Some(cursor) => cursor.sync_state(&changes.new_state),
            None => synced(changes.new_state.clone()),
        };
        account
            .observe(MailboxObserved {
                labels,
                added,
                updated,
                removed: removed.clone(),
                sync_state: Some(sync_state),
                ..observed()
            })
            .await?;
        if let Some(cursor) = listing {
            cursor.polled(changes.new_state, &added_ids, &removed);
        }
        Ok(Some(changes.has_more_changes))
    }

    /// Begins recording all the emails, as of the state read as the
    /// listing began; continues `resumed` if given, listing anew but
    /// fetching only what it has not.
    async fn start_listing(&self, account: &Account, resumed: Option<State>) -> Result<Cursor> {
        let mut request = Request::default();
        let mailboxes = get_mailboxes(&mut request);
        let state = request.call("Email/get", json!({ "ids": [] }));
        // Its total, for the progress shown, is as of now.
        let total = request.call("Email/query", json!({ "calculateTotal": true, "limit": 0 }));
        let mut responses = self.jmap.call(request).await?;
        let mailboxes = Mailboxes::of(responses.take::<Got<Mailbox>>(mailboxes)?.list)?;
        let state = match resumed {
            Some(state) => state,
            None => State {
                emails: responses.take::<Got<Email>>(state)?.state,
                listing_before: Some(account.last_entry().await?),
            },
        };
        let before = state.listing_before.expect("a listing's state");
        let cursor = Cursor {
            known: account.known().await?,
            earlier: account.known_up_to(before).await?,
            state,
            total: responses.take::<Queried>(total)?.total,
            listed: HashSet::new(),
            next: Next::First,
            unfetched: vec![],
        };
        account
            .observe(MailboxObserved {
                labels: labels(account, &mailboxes, BTreeSet::new()).await?,
                sync_state: Some(cursor.sync_state(&cursor.state.emails)),
                ..observed()
            })
            .await?;
        Ok(cursor)
    }

    /// Takes the listing a step: fetches a batch of the new emails listed,
    /// or lists the next page, or, at the end, records what has gone and
    /// that the emails are synced. `true` if there is more. The cursor
    /// moves only once a step is recorded, so a failed one is retried.
    async fn list_step(&self, account: &Account, cursor: &mut Cursor) -> Result<bool> {
        let mut request = Request::default();
        let mailboxes = get_mailboxes(&mut request);
        if !cursor.unfetched.is_empty() {
            let mut responses = self.jmap.call(request).await?;
            let mailboxes = Mailboxes::of(responses.take::<Got<Mailbox>>(mailboxes)?.list)?;
            // Stored a few at a time: so that they show, and pushes and
            // sends go between.
            let n = cursor.unfetched.len().min(BATCH);
            let batch = cursor.unfetched[..n].to_vec();
            let added = self.fetch_new(account, &mailboxes, batch).await?;
            let keywords = added.iter().flat_map(|a| keywords_of(&a.labels)).collect();
            account
                .observe(MailboxObserved {
                    labels: labels(account, &mailboxes, keywords).await?,
                    added,
                    ..observed()
                })
                .await?;
            cursor.unfetched.drain(..n);
            return Ok(true);
        }
        let anchor = match &cursor.next {
            Next::First => None,
            Next::After(id) => Some(id.clone()),
            Next::Done => {
                account
                    .observe(MailboxObserved {
                        removed: cursor.known.difference(&cursor.listed).cloned().collect(),
                        sync_state: Some(synced(cursor.state.emails.clone())),
                        ..observed()
                    })
                    .await?;
                return Ok(false);
            }
        };
        let page = PAGE.min(self.jmap.session().await?.max_objects_in_get);
        let mut query = json!({
            "sort": [{ "property": "receivedAt", "isAscending": false }],
            "limit": page,
        });
        if let Some(anchor) = anchor {
            query["anchor"] = anchor.into();
            query["anchorOffset"] = 1.into();
        }
        let query = request.call("Email/query", query);
        let got = request.call(
            "Email/get",
            json!({ "#ids": request.result_of(query, "/ids"), "properties": EMAIL_PROPERTIES }),
        );
        let mut responses = self.jmap.call(request).await?;
        let mailboxes = Mailboxes::of(responses.take::<Got<Mailbox>>(mailboxes)?.list)?;
        let ids = responses.take::<Queried>(query)?.ids;
        // Emails destroyed since they were listed are recorded as removed
        // by the polls.
        let mut by_id: HashMap<String, Email> = responses
            .take::<Got<Email>>(got)?
            .list
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();
        let mut keywords = BTreeSet::new();
        let mut updated = vec![];
        let mut unfetched = vec![];
        for id in &ids {
            let Some(email) = by_id.remove(id) else {
                continue;
            };
            if cursor.earlier.contains(id) {
                let labels = mailboxes.labels_of(&email)?;
                keywords.extend(keywords_of(&labels));
                updated.push(EntryLabels {
                    external_id: email.id,
                    labels,
                });
            } else if !cursor.known.contains(id) {
                unfetched.push(email);
            }
        }
        let labels = labels(account, &mailboxes, keywords).await?;
        if labels.is_some() || !updated.is_empty() {
            account
                .observe(MailboxObserved {
                    labels,
                    updated,
                    ..observed()
                })
                .await?;
        }
        cursor.unfetched = unfetched;
        cursor.next = ids.last().cloned().map_or(Next::Done, Next::After);
        cursor.listed.extend(ids);
        Ok(true)
    }

    /// Downloads and stores emails new to uplink; skips those destroyed
    /// meanwhile. Oldest first, so the newest are the last recorded.
    async fn fetch_new(
        &self,
        account: &Account,
        mailboxes: &Mailboxes,
        emails: Vec<Email>,
    ) -> Result<Vec<NewEntry>> {
        let mut entries: Vec<NewEntry> = stream::iter(emails)
            .map(|email| async move {
                let labels = mailboxes.labels_of(&email)?;
                let received_at = mail_parser::DateTime::parse_rfc3339(&email.received_at)
                    .with_context(|| format!("receivedAt {}", email.received_at))?
                    .to_timestamp()
                    * 1000;
                let raw = self.jmap.download(&email.blob_id).await?;
                let (variant, parsed, size_bytes) = account.store_raw(raw).await?;
                Ok::<_, anyhow::Error>(NewEntry {
                    external_id: email.id,
                    thread_id: Some(email.thread_id),
                    received_at,
                    labels,
                    variant,
                    size_bytes,
                    parsed,
                })
            })
            .buffered(CONCURRENT_FETCHES)
            .try_collect()
            .await?;
        entries.sort_by_key(|e| e.received_at);
        Ok(entries)
    }

    /// Pushes label changes not yet in effect at the server: each email's
    /// keywords and mailboxes patched, a role's mailbox made if the server
    /// has none.
    async fn push_labels(&self, account: &Account, pending: Vec<Pending>) -> Result<()> {
        let mut per_entry: BTreeMap<EntryId, Vec<Pending>> = BTreeMap::new();
        for p in pending {
            per_entry.entry(p.entry).or_default().push(p);
        }
        let chunk = self.jmap.session().await?.max_objects_in_set;
        let entries: Vec<(EntryId, Vec<Pending>)> = per_entry.into_iter().collect();
        let mut pushed = vec![];
        let mut refused: BTreeMap<String, Vec<LabelChange>> = BTreeMap::new();
        for entries in entries.chunks(chunk) {
            let outcomes = self.push_chunk(entries).await?;
            for ((_, pending), outcome) in entries.iter().zip(outcomes) {
                let changes = pending.iter().map(|p| LabelChange {
                    entry: p.entry,
                    label: p.label,
                    desired: p.desired,
                });
                match outcome {
                    Pushed::Done => pushed.extend(changes),
                    Pushed::Refused(reason) => refused.entry(reason).or_default().extend(changes),
                    // Left for the next sync to record as removed.
                    Pushed::Gone => {}
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

    /// Pushes the changes of `entries`; what came of each, in order.
    async fn push_chunk(&self, entries: &[(EntryId, Vec<Pending>)]) -> Result<Vec<Pushed>> {
        let mut request = Request::default();
        let mailboxes = get_mailboxes(&mut request);
        let mut responses = self.jmap.call(request).await?;
        let mut mailboxes = Mailboxes::of(responses.take::<Got<Mailbox>>(mailboxes)?.list)?;
        let filed: BTreeSet<&str> = entries
            .iter()
            .flat_map(|(_, pending)| pending)
            .filter(|p| p.desired && p.kind != LabelKind::Tag)
            .map(|p| p.label_external_id.as_str())
            .collect();
        for label in filed {
            if ROLES.iter().any(|r| r.label == label) {
                self.mailbox(&mut mailboxes, label).await?;
            }
        }
        let patches: Vec<Result<Patch, Pushed>> = entries
            .iter()
            .map(|(_, pending)| patch(&mailboxes, pending))
            .collect();
        let mut update = Map::new();
        for p in patches.iter().flatten() {
            update.insert(p.id.clone(), p.set.clone().into());
        }
        let mut set: Option<Set> = None;
        if !update.is_empty() {
            let mut request = Request::default();
            let call = request.call("Email/set", json!({ "update": update }));
            set = Some(self.jmap.call(request).await?.take(call)?);
        }
        Ok(patches
            .into_iter()
            .map(|p| {
                let p = p?;
                let set = set.as_ref().expect("set, as there was a patch");
                match set.not_updated.get(&p.id) {
                    Some(e) if e.kind == "notFound" => Err(Pushed::Gone),
                    Some(e) => Err(Pushed::Refused(e.to_string())),
                    None if set.updated.contains_key(&p.id) => Ok(Pushed::Done),
                    None => Err(Pushed::Refused("the server did not say".into())),
                }
            })
            .map(|p| p.unwrap_or_else(|e| e))
            .collect())
    }

    /// The mailbox of a role's `label`, made if the server has none.
    async fn mailbox(&self, mailboxes: &mut Mailboxes, label: &str) -> Result<String> {
        if let Some(id) = mailboxes.by_label.get(label) {
            return Ok(id.clone());
        }
        let role = ROLES
            .iter()
            .find(|r| r.label == label)
            .with_context(|| format!("{label} is of no role"))?;
        let mut request = Request::default();
        let call = request.call(
            "Mailbox/set",
            json!({ "create": { "made": { "name": role.name, "role": role.role, "parentId": null } } }),
        );
        let mut set: Set = self.jmap.call(request).await?.take(call)?;
        if let Some(e) = set.not_created.get("made") {
            bail!("making the {} mailbox: {e}", role.name);
        }
        let created = set
            .created
            .remove("made")
            .with_context(|| format!("the server did not make the {} mailbox", role.name))?;
        let id = created["id"]
            .as_str()
            .with_context(|| format!("the {} mailbox made has no id", role.name))?
            .to_owned();
        mailboxes.add(label, id.clone());
        Ok(id)
    }

    /// Imports `raw` into Drafts, as a draft: its id.
    async fn import_draft(&self, raw: &[u8]) -> Result<String, SendError> {
        let not_sent = SendError::NotSent;
        let mut request = Request::default();
        let mailboxes = get_mailboxes(&mut request);
        let mut responses = self.jmap.call(request).await.map_err(not_sent)?;
        let mut mailboxes = Mailboxes::of(
            responses
                .take::<Got<Mailbox>>(mailboxes)
                .map_err(not_sent)?
                .list,
        )
        .map_err(not_sent)?;
        let drafts = self
            .mailbox(&mut mailboxes, "DRAFT")
            .await
            .map_err(not_sent)?;
        let blob = self
            .jmap
            .upload(raw, "message/rfc822")
            .await
            .map_err(not_sent)?;
        let mut request = Request::default();
        let call = request.call(
            "Email/import",
            json!({ "emails": { "draft": {
                "blobId": blob,
                "mailboxIds": { drafts: true },
                "keywords": { "$draft": true, "$seen": true },
            } } }),
        );
        let mut imported: Imported = self
            .jmap
            .call(request)
            .await
            .and_then(|mut r| r.take(call))
            .map_err(not_sent)?;
        if let Some(e) = imported.not_created.remove("draft") {
            return match e.existing_id {
                Some(id) => Ok(id),
                None => Err(SendError::Rejected(e.to_string())),
            };
        }
        Ok(imported
            .created
            .remove("draft")
            .ok_or_else(|| not_sent(anyhow::anyhow!("the server did not import the draft")))?
            .id)
    }

    /// Submits the draft, from `address`, unless it was; it is filed in
    /// Sent as the server accepts it.
    async fn submit(&self, address: &str, draft: &str) -> Result<(), SendError> {
        let not_sent = SendError::NotSent;
        let mut request = Request::default();
        let mailboxes = get_mailboxes(&mut request);
        let identities = request.call("Identity/get", json!({ "ids": null }));
        let submitted = request.call(
            "EmailSubmission/query",
            json!({ "filter": { "emailIds": [draft] } }),
        );
        let email = request.call("Email/get", json!({ "ids": [draft], "properties": ["id"] }));
        let mut responses = self.jmap.call(request).await.map_err(not_sent)?;
        let mut mailboxes = Mailboxes::of(
            responses
                .take::<Got<Mailbox>>(mailboxes)
                .map_err(not_sent)?
                .list,
        )
        .map_err(not_sent)?;
        if !responses
            .take::<Queried>(submitted)
            .map_err(not_sent)?
            .ids
            .is_empty()
        {
            return Ok(());
        }
        if responses
            .take::<Got<Value>>(email)
            .map_err(not_sent)?
            .list
            .is_empty()
        {
            return Err(SendError::Rejected(
                "its draft is gone from the server".into(),
            ));
        }
        let identities: Got<Identity> = responses.take(identities).map_err(not_sent)?;
        let identity = identity_of(&identities.list, address).ok_or_else(|| {
            SendError::Rejected(format!(
                "the server has no identity of {address}, nor at its domain"
            ))
        })?;
        let drafts = self
            .mailbox(&mut mailboxes, "DRAFT")
            .await
            .map_err(not_sent)?;
        let sent = self
            .mailbox(&mut mailboxes, "SENT")
            .await
            .map_err(not_sent)?;
        let mut request = Request::default();
        let call = request.call(
            "EmailSubmission/set",
            json!({
                "create": { "send": { "identityId": identity, "emailId": draft } },
                "onSuccessUpdateEmail": { "#send": {
                    format!("mailboxIds/{drafts}"): null,
                    format!("mailboxIds/{sent}"): true,
                    "keywords/$draft": null,
                } },
            }),
        );
        // Once the request is made, it may have been submitted, whatever
        // became of the answer.
        let mut set: Set = self
            .jmap
            .call(request)
            .await
            .and_then(|mut r| r.take(call))
            .map_err(SendError::Unknown)?;
        match set.not_created.remove("send") {
            Some(e) => Err(SendError::Rejected(e.to_string())),
            None if set.created.contains_key("send") => Ok(()),
            None => Err(SendError::Unknown(anyhow::anyhow!(
                "the server did not say whether it submitted the draft"
            ))),
        }
    }
}

/// What came of pushing an entry's changes.
#[derive(Debug)]
enum Pushed {
    Done,
    Refused(String),
    /// The email is gone: the next sync records it removed.
    Gone,
}

/// An email's patch.
#[derive(Debug)]
struct Patch {
    id: String,
    set: Map<String, Value>,
}

/// The patch making an email's `pending` changes, by `mailboxes`, which
/// have every mailbox it is filed in.
fn patch(mailboxes: &Mailboxes, pending: &[Pending]) -> Result<Patch, Pushed> {
    let mut set = Map::new();
    for p in pending {
        let label = p.label_external_id.as_str();
        let (path, on) = match label {
            UNREAD => ("keywords/$seen".to_owned(), !p.desired),
            STARRED => ("keywords/$flagged".to_owned(), p.desired),
            _ => match (label.strip_prefix(KEYWORD), mailboxes.by_label.get(label)) {
                (Some(keyword), _) => (format!("keywords/{keyword}"), p.desired),
                (None, Some(mailbox)) => (format!("mailboxIds/{mailbox}"), p.desired),
                (None, None) => {
                    return Err(Pushed::Refused(format!(
                        "label {label} is not one the server has"
                    )))
                }
            },
        };
        set.insert(path, on.then_some(true).into());
    }
    Ok(Patch {
        id: pending[0].external_id.clone(),
        set,
    })
}

/// The identity mail from `address` is submitted as: of the address,
/// else of its domain (`*@domain`, as RFC 8621 has wildcards), else any
/// at its domain. Stalwart has identities of the account's addresses
/// only, a catch-all of a domain not among them, so mail from an address
/// of that domain (an identity of uplink's, SPEC §3.2) is submitted as
/// another at it: the message is from the address, its envelope (where
/// bounces go) from the identity's, of the same domain, so DMARC holds.
fn identity_of<'a>(identities: &'a [Identity], address: &str) -> Option<&'a str> {
    let (_, domain) = address.rsplit_once('@')?;
    let at = |email: &str| {
        email
            .rsplit_once('@')
            .is_some_and(|(_, d)| d.eq_ignore_ascii_case(domain))
    };
    let of = |email: &str| email.eq_ignore_ascii_case(address);
    let of_domain = |email: &str| {
        email
            .strip_prefix("*@")
            .is_some_and(|d| d.eq_ignore_ascii_case(domain))
    };
    identities
        .iter()
        .find(|i| of(&i.email))
        .or_else(|| identities.iter().find(|i| of_domain(&i.email)))
        .or_else(|| identities.iter().find(|i| at(&i.email)))
        .map(|i| i.id.as_str())
}

fn keywords_of(labels: &[String]) -> impl Iterator<Item = String> + '_ {
    labels.iter().filter(|l| l.starts_with(KEYWORD)).cloned()
}

/// All the account's labels, if they are not as recorded: the mailboxes,
/// the state labels, and the keywords recorded or `seen`.
async fn labels(
    account: &Account,
    mailboxes: &Mailboxes,
    seen: BTreeSet<String>,
) -> Result<Option<Vec<ObservedLabel>>> {
    let recorded = account.labels().await?;
    let mut keywords: BTreeSet<String> = recorded
        .iter()
        .filter(|l| l.external_id.starts_with(KEYWORD))
        .map(|l| l.external_id.clone())
        .collect();
    keywords.extend(seen);
    let mut labels: Vec<ObservedLabel> = mailboxes
        .labels
        .iter()
        .cloned()
        .chain([UNREAD, STARRED].map(|l| ObservedLabel {
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

fn synced(emails: String) -> SyncState {
    SyncState::Synced(
        serde_json::to_value(State {
            emails,
            listing_before: None,
        })
        .expect("serializes"),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use uplink_protocol::mail::LabelId;

    fn mailbox(id: &str, name: &str, parent: Option<&str>, role: Option<&str>) -> Mailbox {
        Mailbox {
            id: id.into(),
            name: name.into(),
            parent_id: parent.map(Into::into),
            role: role.map(Into::into),
        }
    }

    fn mailboxes() -> Mailboxes {
        Mailboxes::of(vec![
            mailbox("a", "Inbox", None, Some("inbox")),
            mailbox("b", "Archive", None, Some("archive")),
            mailbox("c", "Lists", None, None),
            mailbox("d", "Rust", Some("c"), None),
            mailbox("e", "Sent Items", None, Some("sent")),
            mailbox("f", "All Mail", None, Some("all")),
        ])
        .unwrap()
    }

    #[test]
    fn mailboxes_are_labels_and_trash_junk_and_archive_folders() {
        let m = mailboxes();
        let labels: Vec<(&str, &str, bool, LabelKind)> = m
            .labels
            .iter()
            .map(|l| (l.external_id.as_str(), l.name.as_str(), l.system, l.kind))
            .collect();
        use LabelKind::{Folder, Mailbox};
        assert_eq!(
            labels,
            [
                ("mailbox:c", "Lists", false, Mailbox),
                ("mailbox:d", "Lists/Rust", false, Mailbox),
                ("INBOX", "INBOX", true, Mailbox),
                ("SENT", "SENT", true, Mailbox),
                ("DRAFT", "DRAFT", true, Mailbox),
                ("TRASH", "TRASH", true, Folder),
                ("SPAM", "SPAM", true, Folder),
                ("ARCHIVE", "ARCHIVE", true, Folder),
            ]
        );
        // Those the server has not are made as filed in.
        assert_eq!(m.by_label.get("TRASH"), None);
        assert_eq!(m.by_label["ARCHIVE"], "b");
        let email = Email {
            id: "m".into(),
            thread_id: "t".into(),
            mailbox_ids: [("b", true), ("d", true), ("f", true)]
                .map(|(k, v)| (k.to_owned(), v))
                .into(),
            keywords: [("$flagged", true), ("$answered", true), ("todo", true)]
                .map(|(k, v)| (k.to_owned(), v))
                .into(),
            received_at: "2026-11-13T23:30:51Z".into(),
            blob_id: "x".into(),
        };
        assert_eq!(
            m.labels_of(&email).unwrap(),
            ["ARCHIVE", "STARRED", "UNREAD", "kw:todo", "mailbox:d"]
        );
    }

    #[test]
    fn changes_patch_mailboxes_and_keywords() {
        let m = mailboxes();
        let pending = |label: &str, desired| Pending {
            entry: EntryId(1),
            external_id: "m".into(),
            label: LabelId(1),
            label_external_id: label.into(),
            kind: LabelKind::Tag,
            desired,
        };
        let p = patch(
            &m,
            &[
                pending("INBOX", false),
                pending("ARCHIVE", true),
                pending("UNREAD", false),
                pending("STARRED", true),
                pending("kw:x", true),
            ],
        )
        .unwrap();
        assert_eq!(
            p.set,
            json!({
                "mailboxIds/a": null,
                "mailboxIds/b": true,
                "keywords/$seen": true,
                "keywords/$flagged": true,
                "keywords/x": true,
            })
            .as_object()
            .unwrap()
            .clone()
        );
        assert!(matches!(
            patch(&m, &[pending("IMPORTANT", true)]),
            Err(Pushed::Refused(_))
        ));
    }

    #[test]
    fn identities_are_of_the_address_or_its_domain() {
        let identity = |id: &str, email: &str| Identity {
            id: id.into(),
            email: email.into(),
        };
        let all = [
            identity("1", "carol@example.com"),
            identity("2", "*@example.org"),
            identity("3", "Alice@Example.org"),
            identity("4", "bob@example.com"),
        ];
        assert_eq!(identity_of(&all, "alice@example.org"), Some("3"));
        assert_eq!(identity_of(&all, "bob@example.org"), Some("2"));
        assert_eq!(identity_of(&all, "bob@example.com"), Some("4"));
        // At a domain of identities but none of its own: as another there.
        assert_eq!(identity_of(&all, "shop@Example.COM"), Some("1"));
        assert_eq!(identity_of(&all, "bob@example.net"), None);
    }
}
