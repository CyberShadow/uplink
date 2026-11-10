//! An email account's sync task, whatever its provider: mirrors the
//! mailbox into uplink, pushes the user's label changes back, and sends
//! queued mail (SPEC §6.6, §6.7, §8).
//!
//! One task per account does everything, in turn: fetch changes and
//! record them, push pending changes and record them as pushed, send what
//! is due. Since a fetch is always recorded before the next push starts, a
//! stale fetch can never be recorded over a push and revert it. What
//! differs between providers is behind [`Provider`].

use crate::{
    compose::{
        self, build_message, now_ms, Due, File, ReplyContext, SendFailed, SendNotFound,
        SendOutcome, SendStarted, SendSucceeded,
    },
    mailbox::PushFailing,
    parse::{parse, Parsed},
    store::{Store, VariantHash},
};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::{
    collections::HashSet,
    future::Future,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast::{self, error::RecvError};
use uplink_protocol::{
    mail::{EntryId, LabelId},
    LogEntry, Source,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension};
use uplink_tx_log::{Conflict, Dispatcher, Observation};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long after a send's attempt began a sync must begin to settle its
/// unknown outcome (SPEC §6.7): by then the provider is done with the
/// request, whatever became of the connection.
const SETTLE_AFTER: i64 = 5 * 60 * 1000;

/// What is particular to a provider.
pub trait Provider: Send + Sync + 'static {
    /// `account.kind`.
    const KIND: &'static str;

    /// Records the mailbox's changes since the last sync (all of it, the
    /// first time), or a bounded part of them; `true` if more remain to
    /// record at once. Pushing and sending go between calls, so that
    /// neither waits on a long sync (a first one takes days).
    fn sync(&self, account: &Account) -> impl Future<Output = Result<bool>> + Send;

    /// Pushes label changes not yet in effect at the provider
    /// ([`Account::pending`]), and records them as pushed.
    fn push(&self, account: &Account) -> impl Future<Output = Result<()>> + Send;

    /// Hands the message to the provider to send; `thread` is the
    /// provider's thread of the message it replies to.
    fn send(
        &self,
        raw: &[u8],
        thread: Option<&str>,
    ) -> impl Future<Output = Result<(), SendError>> + Send;
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
    /// The address mail is sent from.
    pub address: String,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    /// Raw messages.
    pub store: Store,
    /// Files attached to drafts.
    pub blobs: Store,
}

/// A label change not yet in effect at the provider.
#[derive(Debug, Clone)]
pub struct Pending {
    pub entry: EntryId,
    /// The entry's id at the provider.
    pub external_id: String,
    pub label: LabelId,
    pub label_external_id: String,
    /// Whether the label is one of the account's folders.
    pub folder: bool,
    pub desired: bool,
}

impl Account {
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

    /// Label changes not yet in effect at the provider, by entry; but for
    /// those it refused, which wait for the user (SPEC §6.6).
    pub async fn pending(&self) -> Result<Vec<Pending>> {
        let account = self.id.clone();
        self.read(move |conn| {
            Ok(conn
                .prepare(
                    "SELECT e.id, e.external_id, l.id, l.external_id, l.folder, el.desired
                     FROM entry_label el
                     JOIN mail_entry e ON e.id = el.entry
                     JOIN label l ON l.id = el.label
                     WHERE e.account_id = ?1 AND el.desired != el.observed
                       AND el.refused IS NULL
                     ORDER BY e.id, l.id",
                )?
                .query_map([account], |r| {
                    Ok(Pending {
                        entry: EntryId(r.get(0)?),
                        external_id: r.get(1)?,
                        label: LabelId(r.get(2)?),
                        label_external_id: r.get(3)?,
                        folder: r.get(4)?,
                        desired: r.get(5)?,
                    })
                })?
                .collect::<Result<_, _>>()?)
        })
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
    /// The task of account `id`, sending from `address`, keeping its mail
    /// under `data_root`; `reader` is a reader connection of its own.
    pub fn new(
        provider: P,
        id: String,
        address: String,
        dispatcher: Dispatcher,
        reader: Connection,
        data_root: &Path,
        poll_interval: Duration,
    ) -> Self {
        Self {
            provider,
            account: Account {
                id,
                address,
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
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT push_error IS NOT NULL FROM account WHERE id = ?1",
                        [account],
                        |r| r.get(0),
                    )
                    .optional()?
                    // Recorded by its first sync, which has not run.
                    .unwrap_or(false))
            })
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
        self.provider.push(&self.account).await?;
        if let Some(since) = *synced_since {
            self.settle_unknown(since).await?;
        }
        Ok((more, self.send_due().await?))
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

    /// Until the poll interval passes, mail is due (unix ms), or the user
    /// changes something.
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
                entry = wake.recv() => match entry {
                    Ok(entry) if entry.source == Source::User => return,
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return,
                    Err(RecvError::Closed) => panic!("the hub outlives sync tasks"),
                },
            }
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
                Ok((variant, parsed, size_bytes)) => {
                    self.account
                        .observe(SendSucceeded {
                            item,
                            variant,
                            size_bytes,
                            parsed,
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
    async fn send(&self, due: Due) -> Result<(VariantHash, Parsed, i64), SendError> {
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
        let raw = build_message(
            &self.account.address,
            &due.draft.content,
            files,
            &due.message_id,
            &reply,
            now_ms() / 1000,
        )
        .map_err(|e| SendError::Rejected(format!("{e:#}")))?;
        self.provider.send(&raw, thread.as_deref()).await?;
        self.account
            .store_raw(raw)
            .await
            // Sent, but the copy could not be stored: the provider's copy
            // will be, once synced.
            .map_err(|e| SendError::Unknown(e.context("storing sent mail")))
    }
}
