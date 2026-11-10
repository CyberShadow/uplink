//! The sync task of a Matrix account (SPEC §9): mirrors rooms and their
//! timelines into uplink, sends queued messages and pushes the read
//! marker.
//!
//! Two loops share the account's client: one long-polls `/sync` (v2,
//! which every homeserver has) and records what it delivers; the other
//! sends and pushes when the user changes something, so that sending does
//! not wait for a long poll to return.

use crate::{
    client,
    membership::{self, MembershipChanged},
    outbox::{self, SendFailed, SendStarted, SendSucceeded},
    rooms::{self, ObservedEvent, ObservedRoom, ReceiptSent, Synced},
};
use anyhow::{Context, Result};
use matrix_sdk::{
    config::{SyncSettings, SyncToken},
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    ruma::{
        api::client::{
            filter::{FilterDefinition, RoomEventFilter},
            receipt::create_receipt::v3::ReceiptType,
            sync::sync_events::v3::Filter,
        },
        events::{receipt::ReceiptThread, room::message::RoomMessageEventContent},
        EventId, OwnedTransactionId, RoomId, UserId,
    },
    sync::SyncResponse,
    Client, Room,
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast::{self, error::RecvError};
use uplink_config::MatrixAccountConfig;
use uplink_protocol::{
    matrix::{Encryption, Membership, MembershipChange},
    LogEntry, Source,
};
use uplink_storage::{rusqlite::Connection, seal::MasterKey};
use uplink_tx_log::{Conflict, Dispatcher, Observation};

/// How long a `/sync` long poll waits for something to happen.
const LONG_POLL: Duration = Duration::from_secs(30);
/// The long poll of the first `/sync`. Without a token there is always
/// something to answer with, every room, so the homeserver does not wait;
/// but building that answer can take it minutes on a large account. The
/// SDK allows a sync request its long poll on top of the usual request
/// timeout (30 seconds), so this is what lets the first sync finish.
const FIRST_SYNC: Duration = Duration::from_secs(600);
/// How often the sender checks for work when nothing wakes it.
const SEND_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

pub struct Sync {
    account: MatrixAccountConfig,
    key: Arc<MasterKey>,
    dispatcher: Dispatcher,
    data_root: std::path::PathBuf,
}

impl Sync {
    pub fn new(
        account: &MatrixAccountConfig,
        key: Arc<MasterKey>,
        dispatcher: Dispatcher,
        data_root: &Path,
    ) -> Self {
        Self {
            account: account.clone(),
            key,
            dispatcher,
            data_root: data_root.to_owned(),
        }
    }

    /// Runs until the process exits, retrying failures with backoff.
    pub async fn run(self) {
        let id = self.account.id.clone();
        let client = retry(&id, "restoring the session", || async {
            client::restore(&self.account, &self.key).await
        })
        .await;
        let reader = |what| {
            uplink_storage::open_reader(&self.data_root)
                .map(|conn| Arc::new(Mutex::new(conn)))
                .unwrap_or_else(|e| panic!("opening a reader for the Matrix {what}: {e:#}"))
        };
        let sender = Sender {
            account: id.clone(),
            client: client.clone(),
            dispatcher: self.dispatcher.clone(),
            reader: reader("sender"),
            wake: self.dispatcher.hub().subscribe(),
        };
        let syncer = Syncer {
            account: id,
            client,
            dispatcher: self.dispatcher,
            reader: reader("sync"),
        };
        tokio::join!(sender.run(), syncer.run());
    }
}

/// Runs `f` until it succeeds, with backoff.
async fn retry<T, F: std::future::Future<Output = Result<T>>>(
    account: &str,
    what: &str,
    mut f: impl FnMut() -> F,
) -> T {
    let mut backoff = Duration::from_secs(1);
    loop {
        match f().await {
            Ok(t) => return t,
            Err(e) => {
                tracing::warn!(account, "{what} failed: {e:#}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn read<R: Send + 'static>(
    reader: &Arc<Mutex<Connection>>,
    f: impl FnOnce(&Connection) -> Result<R> + Send + 'static,
) -> Result<R> {
    let reader = reader.clone();
    match tokio::task::spawn_blocking(move || f(&reader.lock().expect("reader poisoned"))).await {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

// ── receiving ─────────────────────────────────────────────────────────────

struct Syncer {
    account: String,
    client: Client,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
}

impl Syncer {
    async fn run(self) {
        let account = self.account.clone();
        let reader = self.reader.clone();
        let recorded = retry(&self.account, "reading the sync state", || {
            let account = account.clone();
            let reader = reader.clone();
            async move { read(&reader, move |conn| rooms::sync_state(conn, &account)).await }
        })
        .await;
        let mut token = recorded.map(|s| s.next_batch);
        loop {
            token = Some(retry(&self.account, "sync", || self.sync(token.clone())).await);
        }
    }

    /// Syncs once from `token` (from the start if `None`), and records what
    /// changed; returns the token to sync from next.
    async fn sync(&self, token: Option<String>) -> Result<String> {
        let first = token.is_none();
        let settings = SyncSettings::new()
            .filter(filter())
            .token(match token {
                Some(token) => SyncToken::Specific(token),
                None => SyncToken::NoToken,
            })
            .timeout(if first { FIRST_SYNC } else { LONG_POLL });
        let response = self.client.sync_once(settings).await?;
        let next_batch = response.next_batch.clone();
        let rooms = self.observe(response, first).await?;
        if first || !rooms.is_empty() {
            self.dispatcher
                .observe(
                    self.account.clone(),
                    Synced {
                        next_batch: next_batch.clone(),
                        rooms,
                    },
                )
                .await?;
        }
        Ok(next_batch)
    }

    /// The rooms in `response` with changes uplink stores: events, the
    /// account's own receipts, or membership. Others' receipts and room
    /// state changes outside the timeline are not stored, so on their own
    /// they are not changes; but the first sync records every room.
    async fn observe(&self, response: SyncResponse, first: bool) -> Result<Vec<ObservedRoom>> {
        let own = self.client.user_id().context("the client has no user")?;
        let mut observed = vec![];
        for (room_id, update) in response.rooms.joined {
            let receipts = own_receipts(own, &update.ephemeral)?;
            if first || !update.timeline.events.is_empty() || !receipts.is_empty() {
                observed.push(
                    self.room(
                        &room_id,
                        Membership::Joined,
                        update.timeline.events,
                        receipts,
                    )
                    .await?,
                );
            }
        }
        for (room_id, update) in response.rooms.left {
            observed.push(
                self.room(&room_id, Membership::Left, update.timeline.events, vec![])
                    .await?,
            );
        }
        for room_id in response.rooms.invited.into_keys() {
            observed.push(
                self.room(&room_id, Membership::Invited, vec![], vec![])
                    .await?,
            );
        }
        Ok(observed)
    }

    async fn room(
        &self,
        room_id: &RoomId,
        membership: Membership,
        events: Vec<TimelineEvent>,
        own_receipts: Vec<String>,
    ) -> Result<ObservedRoom> {
        let room = self
            .client
            .get_room(room_id)
            .with_context(|| format!("synced room {room_id} is not in the client"))?;
        let mut names = HashMap::new();
        let mut observed = Vec::with_capacity(events.len());
        for event in events {
            observed.push(timeline_event(&room, event, &mut names).await?);
        }
        Ok(ObservedRoom {
            room_id: room_id.to_string(),
            name: room.display_name().await?.to_string(),
            encrypted: room.encryption_state().is_encrypted(),
            membership,
            events: observed,
            own_receipts,
        })
    }
}

/// The sync filter: no presence, and of ephemeral events only receipts.
fn filter() -> Filter {
    let mut filter = FilterDefinition::with_lazy_loading();
    filter.presence = matrix_sdk::ruma::api::client::filter::Filter::ignore_all();
    filter.room.ephemeral = RoomEventFilter::default();
    filter.room.ephemeral.types = Some(vec!["m.receipt".to_owned()]);
    Filter::FilterDefinition(filter)
}

async fn timeline_event(
    room: &Room,
    event: TimelineEvent,
    names: &mut HashMap<String, Option<String>>,
) -> Result<ObservedEvent> {
    let encryption = match &event.kind {
        TimelineEventKind::Decrypted(_) => Encryption::Decrypted,
        TimelineEventKind::UnableToDecrypt { .. } => Encryption::Undecryptable,
        TimelineEventKind::PlainText { .. } => Encryption::None,
    };
    let mut parsed = ObservedEvent::parse(event.raw().json().get(), None, encryption)?;
    if !names.contains_key(&parsed.sender) {
        let user = <&UserId>::try_from(parsed.sender.as_str())
            .with_context(|| format!("event sender {}", parsed.sender))?;
        let name = room
            .get_member_no_sync(user)
            .await?
            .and_then(|member| member.display_name().map(str::to_owned));
        names.insert(parsed.sender.clone(), name);
    }
    parsed.sender_name = names[&parsed.sender].clone();
    Ok(parsed)
}

/// The events `own`'s read receipts among `ephemeral` are for.
fn own_receipts(
    own: &UserId,
    ephemeral: &[matrix_sdk::ruma::serde::Raw<
        matrix_sdk::ruma::events::AnySyncEphemeralRoomEvent,
    >],
) -> Result<Vec<String>> {
    let mut events = vec![];
    for raw in ephemeral {
        let event: serde_json::Value = serde_json::from_str(raw.json().get())?;
        if event["type"] != "m.receipt" {
            continue;
        }
        let content = event["content"]
            .as_object()
            .context("m.receipt without content")?;
        for (event_id, receipts) in content {
            let ours = ["m.read", "m.read.private"]
                .iter()
                .any(|kind| receipts[kind].get(own.as_str()).is_some());
            if ours {
                events.push(event_id.clone());
            }
        }
    }
    Ok(events)
}

// ── sending ───────────────────────────────────────────────────────────────

struct Sender {
    account: String,
    client: Client,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    wake: broadcast::Receiver<Arc<LogEntry>>,
}

impl Sender {
    async fn run(mut self) {
        retry(&self.account, "recovering interrupted sends", || {
            self.recover()
        })
        .await;
        loop {
            retry(&self.account, "sending", || self.step()).await;
            self.wait().await;
        }
    }

    /// Queues again the messages a previous run was interrupted sending:
    /// their transaction ids make sending them again safe.
    async fn recover(&self) -> Result<()> {
        let account = self.account.clone();
        let interrupted = read(&self.reader, move |conn| {
            outbox::interrupted(conn, &account)
        })
        .await?;
        for item in interrupted {
            self.observe(SendFailed {
                item,
                refused: None,
            })
            .await?;
        }
        Ok(())
    }

    async fn step(&self) -> Result<()> {
        self.change_memberships().await?;
        self.send_due().await?;
        self.push_receipts().await
    }

    /// Joins and leaves the rooms the user asked to.
    async fn change_memberships(&self) -> Result<()> {
        let account = self.account.clone();
        let wanted = read(&self.reader, move |conn| membership::wanted(conn, &account)).await?;
        for wanted in wanted {
            let room_id = <&RoomId>::try_from(wanted.room_id.as_str())?;
            let done = match wanted.change {
                MembershipChange::Join => self.client.join_room_by_id(room_id).await.map(drop),
                MembershipChange::Leave => {
                    self.client
                        .get_room(room_id)
                        .with_context(|| format!("room {room_id} is not in the client"))?
                        .leave()
                        .await
                }
            };
            let refused = match &done {
                Ok(()) => None,
                Err(e) => match refusal(e) {
                    Some(refused) => Some(refused),
                    None => return done.context("changing a room's membership"),
                },
            };
            self.observe(MembershipChanged {
                room: wanted.room,
                change: wanted.change,
                refused,
            })
            .await?;
        }
        Ok(())
    }

    /// Until the user changes something, or the interval passes.
    async fn wait(&mut self) {
        let deadline = tokio::time::sleep(SEND_INTERVAL);
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

    /// Sends the queued messages, in order; stops at the first that fails
    /// for a passing reason, so that messages to a room stay in order.
    async fn send_due(&self) -> Result<()> {
        let account = self.account.clone();
        let due = read(&self.reader, move |conn| outbox::due(conn, &account)).await?;
        for item in due {
            match self.observe(SendStarted { item: item.item }).await {
                Ok(()) => {}
                // Cancelled meanwhile.
                Err(e) if e.is::<Conflict>() => continue,
                Err(e) => return Err(e),
            }
            let room_id = <&RoomId>::try_from(item.room_id.as_str())?;
            let Some(room) = self.client.get_room(room_id) else {
                self.observe(SendFailed {
                    item: item.item,
                    refused: Some("uplink is not in the room".into()),
                })
                .await?;
                continue;
            };
            let txn_id: OwnedTransactionId = outbox::transaction_id(item.item).into();
            let sent = room
                .send(RoomMessageEventContent::text_plain(&item.body))
                .with_transaction_id(txn_id)
                .await;
            match sent {
                Ok(result) => {
                    self.observe(SendSucceeded {
                        item: item.item,
                        event_id: result.response.event_id.to_string(),
                    })
                    .await?
                }
                Err(e) => {
                    let refused = refusal(&e);
                    self.observe(SendFailed {
                        item: item.item,
                        refused: refused.clone(),
                    })
                    .await?;
                    if refused.is_none() {
                        return Err(e).context("sending a message");
                    }
                }
            }
        }
        Ok(())
    }

    /// Sends a read receipt for each joined room whose read marker moved
    /// past the latest receipt.
    async fn push_receipts(&self) -> Result<()> {
        let account = self.account.clone();
        let unsent = read(&self.reader, move |conn| {
            rooms::unsent_receipts(conn, &account)
        })
        .await?;
        for receipt in unsent {
            let room_id = <&RoomId>::try_from(receipt.room_id.as_str())?;
            let room = self
                .client
                .get_room(room_id)
                .with_context(|| format!("joined room {room_id} is not in the client"))?;
            room.send_single_receipt(
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                <&EventId>::try_from(receipt.event_id.as_str())?.to_owned(),
            )
            .await?;
            self.observe(ReceiptSent {
                room: receipt.room,
                up_to: receipt.up_to,
            })
            .await?;
        }
        Ok(())
    }

    async fn observe(&self, observation: impl Observation) -> Result<()> {
        self.dispatcher
            .observe(self.account.clone(), observation)
            .await
            .map(drop)
    }
}

/// The homeserver's reason, if it refused the request (a 4xx other than
/// rate limiting); `None` if the failure may pass.
fn refusal(error: &matrix_sdk::Error) -> Option<String> {
    let api = error.as_client_api_error()?;
    let status = api.status_code.as_u16();
    ((400..500).contains(&status) && status != 429).then(|| api.to_string())
}
