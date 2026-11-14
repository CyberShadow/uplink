//! An email account's sync task, whatever its provider: mirrors the
//! mailbox into uplink, pushes the user's label changes back, and sends
//! queued mail (SPEC §6.6, §6.7, §8).
//!
//! One task per account does everything, in turn: fetch changes and
//! record them, reconcile the labels the user changed (claim their pushes,
//! push them, record how each ended, read back those pushed), send what
//! is due. Since a fetch is always recorded before the next push starts,
//! and asked for after the last was recorded, what the task observes is
//! never older than what it knows (`account.labels_known_at`, V0073). What
//! differs between providers is behind [`Provider`].

use crate::watching::{self, Told, WatchFailing};
use crate::{
    compose::{
        self, build_message, now_ms, Due, File, ReplyContext, SendFailed, SendNotFound,
        SendOutcome, SendStarted, SendSucceeded, SentCopy,
    },
    mailbox::{
        self, kind_at, LabelChange, LabelsClaimed, LabelsPushed, LabelsSettled, ObservedLabel,
        Reconciling,
    },
    parse::{parse, Parsed},
};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::{
    collections::{BTreeSet, HashSet},
    future::Future,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast::{self, error::RecvError};
use uplink_protocol::{
    compose::OutboxId,
    mail::{EntryId, Identity, LabelId, LabelKind},
    LogEntry, ServerEvent, Source, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension};
use uplink_storage::store::{Store, VariantHash};
use uplink_tx_log::{
    push::{self, PushFailing},
    reconcile::{Ended, Outcome},
    Conflict, Dispatcher, Observation,
};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long after a send's attempt began a sync must begin to settle its
/// unknown outcome (SPEC §6.7): by then the provider is done with the
/// request, whatever became of the connection.
const SETTLE_AFTER: i64 = 5 * 60 * 1000;

/// What is particular to a provider.
pub trait Provider: Send + Sync + 'static {
    /// `account.kind`.
    const KIND: &'static str;

    /// Whether the provider files its copy of a message in Sent as it
    /// accepts it, so that a send of unknown outcome is sent again once a
    /// sync has not found the copy (SPEC §6.7): which shows it did not go
    /// out, where the copy keeps our Message-ID; where it does not
    /// (Gmail), sending again sends once only, but one no longer wanted
    /// is taken back as if it had not gone out. Otherwise such a send stays
    /// unknown until its copy turns up, or the user decides.
    const FILES_SENT: bool;

    /// Records the mailbox's changes since the last sync (all of it, the
    /// first time), or a bounded part of them; `true` if more remain to
    /// record at once. Pushing and sending go between calls, so that
    /// neither waits on a long sync (a first one takes days).
    fn sync(&self, account: &Account) -> impl Future<Output = Result<bool>> + Send;

    /// Pushes the label changes claimed (each [`Pending::desired`], the
    /// value to push), by entry; how each request ended, of all of them,
    /// every failure classified (SPEC §6.6.3).
    fn push(
        &self,
        account: &Account,
        claimed: Vec<Pending>,
    ) -> impl Future<Output = Vec<PushEnded>> + Send;

    /// Reads back the labels of `entries` (and their ids at the provider),
    /// recording what it finds as observed by requests sent after
    /// [`Account::as_of`] (in a [`MailboxObserved`](crate::mailbox::MailboxObserved)'s `updated`, or
    /// `removed`, if it can tell that an entry is gone).
    fn read(
        &self,
        account: &Account,
        entries: Vec<(EntryId, String)>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Hands the message to the provider to send; which of its messages
    /// is its copy.
    fn send(
        &self,
        account: &Account,
        mail: Outgoing<'_>,
    ) -> impl Future<Output = Result<SentCopy, SendError>> + Send;

    /// Resolves when the provider's watch tells of a change, so that the
    /// task syncs before the poll interval passes, or of its failing
    /// (SPEC §8.14); for a provider that only polls, never. What is told
    /// while no one waits is not lost: the next call resolves at once.
    fn told(&self) -> impl Future<Output = Told> + Send {
        std::future::pending()
    }
}

/// A message to send, and what is known of earlier attempts.
pub struct Outgoing<'a> {
    pub item: OutboxId,
    /// The address it is from, of an identity of the account's (SPEC
    /// §3.2): the provider sends from it, or refuses to.
    pub from: &'a str,
    /// RFC 5322, as built.
    pub raw: &'a [u8],
    pub message_id: &'a str,
    /// When it was due (unix ms): it went out no earlier.
    pub not_before: i64,
    /// The provider's thread of the message it replies to.
    pub thread: Option<&'a str>,
    /// The provider's draft it is sent as, and the draft's thread (Gmail),
    /// if an earlier attempt made it
    /// ([`SendDrafted`](crate::compose::SendDrafted)).
    pub provider_draft: Option<&'a str>,
    pub provider_thread: Option<&'a str>,
}

/// Why sending did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The message was not sent, but may be on a later attempt.
    #[error("{0:#}")]
    NotSent(anyhow::Error),
    /// The provider refused the message.
    #[error("{0}")]
    Rejected(String),
    /// The request failed after it went out: the message may have been
    /// sent.
    #[error("{0:#}; it may have been sent")]
    Unknown(anyhow::Error),
}

/// The account as its provider's sync sees it: uplink's side.
pub struct Account {
    pub id: String,
    /// The addresses mail is sent from, the account's own first (SPEC
    /// §3.2).
    pub identities: Vec<Identity>,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    /// Raw messages.
    pub store: Store,
    /// Files attached to drafts.
    pub blobs: Store,
}

/// How a provider's request pushing `changes` ended; and why, if it
/// failed for a reason that may pass.
#[derive(Debug)]
pub struct PushEnded {
    pub changes: Vec<LabelChange>,
    pub ended: Ended,
    pub error: Option<anyhow::Error>,
}

impl PushEnded {
    /// All of `pending`'s changes, which ended so.
    pub fn of(pending: &[Pending], ended: Ended, error: Option<anyhow::Error>) -> Self {
        Self {
            changes: pending.iter().map(Pending::change).collect(),
            ended,
            error,
        }
    }

    /// Not applied, for `error`, which may pass.
    pub fn not_sent(pending: &[Pending], error: anyhow::Error) -> Self {
        Self::of(pending, Ended::NotApplied { refused: None }, Some(error))
    }

    /// Of unknown outcome, as of now, for `error`.
    pub fn unknown(pending: &[Pending], error: anyhow::Error) -> Self {
        Self::of(pending, Ended::Unknown { at_ms: now_ms() }, Some(error))
    }

    pub fn refused(pending: &[Pending], reason: String) -> Self {
        Self::of(
            pending,
            Ended::NotApplied {
                refused: Some(reason),
            },
            None,
        )
    }
}

/// A label change claimed, to push.
#[derive(Debug, Clone)]
pub struct Pending {
    pub entry: EntryId,
    /// The entry's id at the provider.
    pub external_id: String,
    pub label: LabelId,
    pub label_external_id: String,
    pub kind: LabelKind,
    /// The value to push.
    pub desired: bool,
}

impl Pending {
    fn change(&self) -> LabelChange {
        LabelChange {
            entry: self.entry,
            label: self.label,
            desired: self.desired,
        }
    }
}

impl Account {
    /// The log's last entry: requests sent after this observe what it
    /// knows (SPEC §6.6.2).
    pub async fn as_of(&self) -> Result<TxLogId> {
        self.read(uplink_tx_log::as_of).await
    }

    /// Records `observation` as the account's.
    pub async fn observe(&self, observation: impl Observation) -> Result<()> {
        self.dispatcher
            .observe(self.id.clone(), observation)
            .await
            .map(drop)
    }

    /// Runs `f` on the task's reader connection.
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let reader = self.reader.clone();
        tokio::task::spawn_blocking(move || f(&reader.lock().expect("reader poisoned"))).await?
    }

    /// `account.sync_state`, as the provider recorded it.
    pub async fn sync_state<T: DeserializeOwned + Send + 'static>(&self) -> Result<Option<T>> {
        let account = self.id.clone();
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
    pub async fn known(&self) -> Result<HashSet<String>> {
        self.known_up_to(i64::MAX).await
    }

    /// External ids of the account's entries up to entry id `last`.
    pub async fn known_up_to(&self, last: i64) -> Result<HashSet<String>> {
        let account = self.id.clone();
        self.read(move |conn| {
            Ok(conn
                .prepare("SELECT external_id FROM mail_entry WHERE account_id = ?1 AND id <= ?2")?
                .query_map(params![account, last], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// The id of the last entry recorded, of any account; entries recorded
    /// later have greater ids (`AUTOINCREMENT`).
    pub async fn last_entry(&self) -> Result<i64> {
        self.read(|conn| {
            Ok(
                conn.query_row("SELECT coalesce(max(id), 0) FROM mail_entry", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
    }

    /// The account's labels its reconciler has to do with.
    async fn reconciling(&self) -> Result<Vec<Reconciling>> {
        let account = self.id.clone();
        self.read(move |conn| mailbox::reconciling(conn, &account))
            .await
    }

    /// External ids of the labels the entry should have.
    pub async fn desired_labels(&self, entry: EntryId) -> Result<Vec<String>> {
        self.read(move |conn| {
            Ok(conn
                .prepare(
                    "SELECT l.external_id FROM entry_label el JOIN label l ON l.id = el.label
                     WHERE el.entry = ?1 AND el.desired ORDER BY l.id",
                )?
                .query_map([entry.0], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// External ids of the labels the entry has at the provider, as last
    /// observed.
    pub async fn observed_labels(&self, entry: EntryId) -> Result<Vec<String>> {
        self.read(move |conn| {
            Ok(conn
                .prepare(
                    "SELECT l.external_id FROM entry_label el JOIN label l ON l.id = el.label
                     WHERE el.entry = ?1 AND el.observed ORDER BY l.id",
                )?
                .query_map([entry.0], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// The account's labels as recorded, by external id.
    pub async fn labels(&self) -> Result<Vec<ObservedLabel>> {
        let id = self.id.clone();
        self.read(move |conn| {
            Ok(conn
                .prepare(
                    "SELECT external_id, name, system, kind FROM label
                     WHERE account_id = ?1 ORDER BY external_id",
                )?
                .query_map([id], |r| {
                    Ok(ObservedLabel {
                        external_id: r.get(0)?,
                        name: r.get(1)?,
                        system: r.get(2)?,
                        kind: kind_at(r, 3)?,
                    })
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Stores a raw message; returns its hash, its parse, and its size.
    pub async fn store_raw(&self, raw: Vec<u8>) -> Result<(VariantHash, Parsed, i64)> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((store.put(&raw)?, parse(&raw), raw.len().try_into()?))
        })
        .await?
    }
}

/// The sync task of an account at `P`.
pub struct SyncTask<P> {
    pub provider: P,
    pub account: Account,
    pub poll_interval: Duration,
}

impl<P: Provider> SyncTask<P> {
    /// The task of account `id`, sending from `identities`, keeping its mail
    /// under `data_root`; `reader` is a reader connection of its own.
    pub fn new(
        provider: P,
        id: String,
        identities: Vec<Identity>,
        dispatcher: Dispatcher,
        reader: Connection,
        data_root: &Path,
        poll_interval: Duration,
    ) -> Self {
        Self {
            provider,
            account: Account {
                id,
                identities,
                dispatcher,
                reader: Arc::new(Mutex::new(reader)),
                store: Store::mail(data_root),
                blobs: Store::blobs(data_root),
            },
            poll_interval,
        }
    }

    /// Runs until the process exits, retrying failures with backoff. A
    /// failure holds up the account's pushes and sends, as the steps go in
    /// turn, so it is recorded for the outbox as they start and stop
    /// failing, not at each retry (SPEC §6.6).
    pub async fn run(self) {
        let mut wake = self.account.dispatcher.hub().subscribe();
        let mut backoff = Duration::from_secs(1);
        let mut recovered = false;
        // Whether the last run left the account failing: not known until
        // read, which may fail too.
        let mut failing = None;
        // When the last sync that recorded all there was began (unix ms).
        let mut synced_since = None;
        loop {
            let step = async {
                if failing.is_none() {
                    failing = Some(self.failing().await?);
                }
                if !recovered {
                    self.recover().await?;
                    recovered = true;
                }
                self.step(&mut synced_since).await
            };
            match step.await {
                Ok((more, next_due)) => {
                    backoff = Duration::from_secs(1);
                    if failing == Some(true) {
                        self.record_failing(None).await;
                        failing = Some(false);
                    }
                    if !more {
                        self.wait(&mut wake, next_due).await;
                    }
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    tracing::warn!(account = self.account.id, "sync failed: {error}");
                    if failing == Some(false) {
                        self.record_failing(Some(error)).await;
                        failing = Some(true);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    /// Whether the account is recorded as failing.
    async fn failing(&self) -> Result<bool> {
        let account = self.account.id.clone();
        self.account
            .read(move |conn| push::is_failing(conn, &account))
            .await
    }

    /// Records that the account's changes are not reaching the provider,
    /// for `error`, or are again. If that cannot be recorded either, the
    /// database is failing too, which the next step reports.
    async fn record_failing(&self, error: Option<String>) {
        let at = now_ms();
        if let Err(e) = self.account.observe(PushFailing { error, at }).await {
            tracing::warn!(account = self.account.id, "recording the failure: {e:#}");
        }
    }

    /// Syncs (a step), pushes and sends; returns whether the sync has more
    /// to do at once, and when the next queued mail is due. `synced_since`:
    /// when the last sync that recorded all there was began.
    async fn step(&self, synced_since: &mut Option<i64>) -> Result<(bool, Option<i64>)> {
        let began = now_ms();
        let more = self.provider.sync(&self.account).await?;
        if !more {
            *synced_since = Some(began);
        }
        self.reconcile().await?;
        if let (true, Some(since)) = (P::FILES_SENT, *synced_since) {
            self.settle_unknown(since).await?;
        }
        Ok((more, self.send_due().await?))
    }

    /// Pushes the label changes the user made, and reads back what that is
    /// to be confirmed by (SPEC §6.6): until nothing is left to push or
    /// read, but for what waits on the next sync (a push not applied, an
    /// entry its read did not find). Every push claimed ends recorded,
    /// though some fail; then the first failure, that may pass, is
    /// returned, for the step to be retried with backoff.
    async fn reconcile(&self) -> Result<()> {
        // Each round pushes or confirms; few are needed.
        const ROUNDS: usize = 8;
        let mut later = BTreeSet::new();
        for _ in 0..ROUNDS {
            let fields = self.account.reconciling().await?;
            let now = now_ms();
            let in_flight = fields.iter().any(|f| {
                f.field
                    .attempt
                    .as_ref()
                    .is_some_and(|a| a.outcome == Outcome::InFlight)
            });
            // One push at a time: one in flight stopped without its
            // outcome (the step failed, or the process stopped).
            if in_flight || fields.iter().any(|f| f.field.expiring(now)) {
                self.account
                    .observe(LabelsSettled {
                        now_ms: now,
                        interrupted: in_flight,
                    })
                    .await?;
                continue;
            }
            let wanted: Vec<(EntryId, LabelId)> = fields
                .iter()
                .filter(|f| !later.contains(&f.entry) && f.field.wanted().is_some())
                .map(|f| (f.entry, f.label))
                .collect();
            if !wanted.is_empty() {
                self.account
                    .observe(LabelsClaimed { changes: wanted })
                    .await?;
                let claimed: Vec<Pending> = self
                    .account
                    .reconciling()
                    .await?
                    .into_iter()
                    .filter_map(|f| {
                        let attempt = f.field.attempt.filter(|a| a.outcome == Outcome::InFlight)?;
                        Some(Pending {
                            entry: f.entry,
                            external_id: f.external_id,
                            label: f.label,
                            label_external_id: f.label_external_id,
                            kind: f.kind,
                            desired: attempt.value,
                        })
                    })
                    .collect();
                let mut failed = None;
                for pushed in self.provider.push(&self.account, claimed).await {
                    // Not applied, or maybe: pushed again after the next
                    // sync, rather than at once.
                    if !matches!(
                        pushed.ended,
                        Ended::Applied | Ended::NotApplied { refused: Some(_) }
                    ) {
                        later.extend(pushed.changes.iter().map(|c| c.entry));
                    }
                    if let Some(e) = pushed.error {
                        failed.get_or_insert(e.context("pushing label changes"));
                    }
                    self.account
                        .observe(LabelsPushed {
                            changes: pushed.changes,
                            ended: pushed.ended,
                        })
                        .await?;
                }
                if let Some(e) = failed {
                    return Err(e);
                }
                continue;
            }
            let mut read: Vec<(EntryId, String)> = fields
                .iter()
                .filter(|f| !later.contains(&f.entry) && f.field.read_back())
                .map(|f| (f.entry, f.external_id.clone()))
                .collect();
            read.dedup();
            if read.is_empty() {
                return Ok(());
            }
            let entries: BTreeSet<EntryId> = read.iter().map(|(e, _)| *e).collect();
            self.provider.read(&self.account, read).await?;
            // Not found, as an entry moved is not: the next sync tells.
            for f in self.account.reconciling().await? {
                if entries.contains(&f.entry) && f.field.read_back() {
                    later.insert(f.entry);
                }
            }
        }
        anyhow::bail!("label changes still to push or read after {ROUNDS} rounds")
    }

    /// Settles the sends whose outcome was unknown, attempted long enough
    /// before a sync that began at `synced_since` and recorded all there
    /// was: had they gone out, it would have seen their copy, and they
    /// would be sent.
    async fn settle_unknown(&self, synced_since: i64) -> Result<()> {
        let account = self.account.id.clone();
        let unknown = self
            .account
            .read(move |conn| compose::unknown_before(conn, &account, synced_since - SETTLE_AFTER))
            .await?;
        for item in unknown {
            self.account.observe(SendNotFound { item }).await?;
        }
        Ok(())
    }

    /// Until the poll interval passes, mail is due (unix ms), the user
    /// changes something of the account's to push (its outbox), or the
    /// provider tells of a change.
    async fn wait(&self, wake: &mut broadcast::Receiver<Arc<LogEntry>>, next_due: Option<i64>) {
        let mut interval = self.poll_interval;
        if let Some(due) = next_due {
            let until_due = Duration::from_millis((due - now_ms()).max(0).try_into().expect("≥ 0"));
            interval = interval.min(until_due);
        }
        let deadline = tokio::time::sleep(interval);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return,
                told = self.provider.told() => match told {
                    Told::Changed => return,
                    Told::Failing(error) => self.record_watch(error).await,
                },
                entry = wake.recv() => match entry {
                    Ok(entry) if self.pushes(&entry) => return,
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return,
                    Err(RecvError::Closed) => panic!("the hub outlives sync tasks"),
                },
            }
        }
    }

    /// Whether `entry` is the user's, of something of the account's to
    /// push: its own observations change its outbox too, but are what it
    /// pushed or synced.
    fn pushes(&self, entry: &LogEntry) -> bool {
        entry.source == Source::User
            && entry.events.iter().any(|event| match event {
                ServerEvent::OutboxChanged(changed) => changed.accounts.contains(&self.account.id),
                _ => false,
            })
    }

    /// Records that the provider's watch is failing, for `error`, or is
    /// not, where that is not what is recorded: as failures start and
    /// end, not at each retry. If that cannot be done, the database is
    /// failing, which the next step reports.
    async fn record_watch(&self, error: Option<String>) {
        let account = self.account.id.clone();
        let recorded = self
            .account
            .read(move |conn| watching::is_failing(conn, &account))
            .await;
        let result = match recorded {
            Ok(failing) if failing == error.is_some() => return,
            Ok(_) => {
                let at = now_ms();
                self.account.observe(WatchFailing { error, at }).await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            tracing::warn!(account = self.account.id, "recording the watch: {e:#}");
        }
    }

    /// Records the outcome of mail a previous run of the task was
    /// interrupted sending as unknown.
    async fn recover(&self) -> Result<()> {
        let account = self.account.id.clone();
        let interrupted = self
            .account
            .read(move |conn| compose::interrupted(conn, &account))
            .await?;
        for item in interrupted {
            self.account
                .observe(SendFailed {
                    item,
                    outcome: SendOutcome::Unknown("interrupted while sending".into()),
                })
                .await?;
        }
        Ok(())
    }

    /// Sends the queued mail that is due; returns when the next is due.
    async fn send_due(&self) -> Result<Option<i64>> {
        let account = self.account.id.clone();
        let (due, next) = self
            .account
            .read(move |conn| compose::due(conn, &account, now_ms()))
            .await?;
        for due in due {
            match self.account.observe(SendStarted { item: due.item }).await {
                // Cancelled meanwhile.
                Err(e) if e.is::<Conflict>() => continue,
                result => result?,
            }
            let item = due.item;
            match self.send(due).await {
                Ok((variant, parsed, size_bytes, copy)) => {
                    self.account
                        .observe(SendSucceeded {
                            item,
                            variant,
                            size_bytes,
                            parsed,
                            copy,
                        })
                        .await?
                }
                Err(SendError::NotSent(e)) => {
                    self.account
                        .observe(SendFailed {
                            item,
                            outcome: SendOutcome::NotSent,
                        })
                        .await?;
                    return Err(e.context("sending mail"));
                }
                Err(SendError::Rejected(reason)) => {
                    self.account
                        .observe(SendFailed {
                            item,
                            outcome: SendOutcome::Refused(reason),
                        })
                        .await?
                }
                Err(SendError::Unknown(e)) => {
                    self.account
                        .observe(SendFailed {
                            item,
                            outcome: SendOutcome::Unknown(format!("{e:#}")),
                        })
                        .await?
                }
            }
        }
        Ok(next)
    }

    /// Sends a claimed item, and stores the message as sent.
    async fn send(&self, due: Due) -> Result<(VariantHash, Parsed, i64, SentCopy), SendError> {
        let account = self.account.id.clone();
        let store = self.account.store.clone();
        let blobs = self.account.blobs.clone();
        let attachments = due.draft.attachments.clone();
        let in_reply_to = due.draft.in_reply_to;
        // The message replied to: its headers, and its thread here.
        let (files, reply, thread) = self
            .account
            .read(move |conn| {
                let files = File::read_all(&blobs, &attachments)?;
                let Some(message) = in_reply_to else {
                    return Ok((files, ReplyContext::default(), None));
                };
                let (message_id, variant) = crate::query::preferred_variant(conn, message)?
                    .context("the message replied to is gone")?;
                let raw = store.get(&VariantHash(variant))?;
                let thread: Option<String> = conn
                    .query_row(
                        "SELECT thread_id FROM mail_entry WHERE message = ?1 AND account_id = ?2
                         ORDER BY id LIMIT 1",
                        params![message.0, account],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                Ok((files, ReplyContext::of(&message_id, &raw), thread))
            })
            .await
            .map_err(|e| SendError::Rejected(format!("{e:#}")))?;
        // The server is the boundary: mail goes from an identity only.
        let from = compose::sender(&due.draft.content, &self.account.identities)
            .map_err(|e| SendError::Rejected(format!("{e:#}")))?;
        let raw = build_message(
            &from,
            &due.draft.content,
            files,
            &due.message_id,
            &reply,
            now_ms() / 1000,
        )
        .map_err(|e| SendError::Rejected(format!("{e:#}")))?;
        let copy = self
            .provider
            .send(
                &self.account,
                Outgoing {
                    item: due.item,
                    from: &from.address,
                    raw: &raw,
                    message_id: &due.message_id,
                    not_before: due.not_before,
                    thread: thread.as_deref(),
                    provider_draft: due.provider_draft.as_deref(),
                    provider_thread: due.provider_thread.as_deref(),
                },
            )
            .await?;
        let (variant, parsed, size_bytes) = self
            .account
            .store_raw(raw)
            .await
            // Sent, but the copy could not be stored: the provider's copy
            // will be, once synced.
            .map_err(|e| SendError::Unknown(e.context("storing sent mail")))?;
        Ok((variant, parsed, size_bytes, copy))
    }
}
