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
    rooms::{self, Decrypted, ObservedEvent, ObservedRoom, ReceiptSent, SpaceChild, Synced},
    send::{self, SendFailed, SendStarted, SendSucceeded},
    verification,
};
use anyhow::{Context, Result};
use matrix_sdk::encryption::backups::BackupState;
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::{events::room::encrypted::OriginalSyncRoomEncryptedEvent, serde::Raw};
use matrix_sdk::stream::StreamExt;
use matrix_sdk::{
    attachment::AttachmentConfig,
    config::{SyncSettings, SyncToken},
    deserialized_responses::{RawAnySyncOrStrippedState, TimelineEvent, TimelineEventKind},
    room::reply::{EnforceThread, Reply},
    ruma::{
        api::client::{
            filter::{FilterDefinition, RoomEventFilter},
            receipt::create_receipt::v3::ReceiptType,
            sync::sync_events::v3::Filter,
        },
        events::{
            receipt::ReceiptThread,
            room::message::{AddMentions, FormattedBody},
            StateEventType,
        },
        EventId, OwnedEventId, OwnedTransactionId, RoomId, UserId,
    },
    sync::{State, SyncResponse},
    Client, Room,
};
use serde_json::json;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{
    broadcast::{self, error::RecvError},
    mpsc,
};
use uplink_config::MatrixAccountConfig;
use uplink_protocol::{
    matrix::{
        Encryption, MatrixContent, MemberAction, Membership, MembershipChange, ReceiptsChanged,
        RoomMember, SecurityChanged, Typing,
    },
    LogEntry, Notice, Source,
};
use uplink_storage::{
    rusqlite::Connection,
    seal::MasterKey,
    store::{Store, VariantHash},
};
use uplink_tx_log::{
    push::{self, PushFailing},
    Conflict, Dispatcher, Observation,
};

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
    clients: client::Clients,
    key: Arc<MasterKey>,
    dispatcher: Dispatcher,
    data_root: std::path::PathBuf,
}

impl Sync {
    /// `clients`: where to put the account's client once restored.
    pub fn new(
        account: &MatrixAccountConfig,
        clients: client::Clients,
        key: Arc<MasterKey>,
        dispatcher: Dispatcher,
        data_root: &Path,
    ) -> Self {
        Self {
            account: account.clone(),
            clients,
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
        self.clients.insert(&id, client.clone());
        // Verifications other devices of the account ask for.
        let (clients, dispatcher, account) =
            (self.clients.clone(), self.dispatcher.clone(), id.clone());
        client.add_event_handler(
            move |event: ToDeviceKeyVerificationRequestEvent, client: Client| {
                let (clients, dispatcher, account) =
                    (clients.clone(), dispatcher.clone(), account.clone());
                async move {
                    if Some(&*event.sender) != client.user_id() {
                        return;
                    }
                    let Some(request) = client
                        .encryption()
                        .get_verification_request(&event.sender, &event.content.transaction_id)
                        .await
                    else {
                        return;
                    };
                    verification::track(&clients, &dispatcher, &account, request);
                }
            },
        );
        let reader = |what| {
            uplink_storage::open_reader(&self.data_root)
                .map(|conn| Arc::new(Mutex::new(conn)))
                .unwrap_or_else(|e| panic!("opening a reader for the Matrix {what}: {e:#}"))
        };
        let sender = Sender {
            account: id.clone(),
            client: client.clone(),
            blobs: Store::blobs(&self.data_root),
            dispatcher: self.dispatcher.clone(),
            reader: reader("sender"),
            wake: self.dispatcher.hub().subscribe(),
        };
        let (undecryptable, received) = mpsc::unbounded_channel();
        let (id_for_security, client_for_security) = (id.clone(), client.clone());
        let dispatcher = self.dispatcher.clone();
        let redecryptor = Redecryptor {
            account: id.clone(),
            client: client.clone(),
            dispatcher: self.dispatcher.clone(),
            reader: reader("redecryptor"),
            undecryptable: received,
        };
        let syncer = Syncer {
            account: id,
            client,
            dispatcher: self.dispatcher,
            reader: reader("sync"),
            undecryptable,
        };
        let security = watch_security(id_for_security, client_for_security, dispatcher);
        tokio::join!(sender.run(), syncer.run(), redecryptor.run(), security);
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
    /// Where events recorded undecryptable go, to be decrypted later.
    undecryptable: mpsc::UnboundedSender<Vec<(String, String)>>,
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
        let moment = self.moment(&response).await?;
        let rooms = self.observe(response, first).await?;
        let undecryptable: Vec<(String, String)> = rooms
            .iter()
            .flat_map(|room| {
                room.events
                    .iter()
                    .filter(|e| e.encryption == Encryption::Undecryptable)
                    .map(|e| (room.room_id.clone(), e.raw.clone()))
            })
            .collect();
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
        self.notify(moment).await?;
        if !undecryptable.is_empty() {
            self.undecryptable
                .send(undecryptable)
                .expect("the redecryptor runs as long as the syncer");
        }
        Ok(next_batch)
    }

    /// What the response says of the moment, by joined room: who is
    /// typing, if that changed, and whether others sent read receipts.
    async fn moment(&self, response: &SyncResponse) -> Result<Vec<Moment>> {
        let own = self.client.user_id().context("the client has no user")?;
        let mut moments = vec![];
        for (room_id, update) in &response.rooms.joined {
            let mut moment = Moment {
                room_id: room_id.to_string(),
                typing: None,
                receipts: false,
            };
            for raw in &update.ephemeral {
                let event: serde_json::Value = serde_json::from_str(raw.json().get())?;
                match event["type"].as_str() {
                    Some("m.typing") => {
                        let room = self.client.get_room(room_id).with_context(|| {
                            format!("synced room {room_id} is not in the client")
                        })?;
                        let mut users = vec![];
                        for user in event["content"]["user_ids"]
                            .as_array()
                            .into_iter()
                            .flatten()
                        {
                            let user = <&UserId>::try_from(
                                user.as_str()
                                    .context("m.typing with a user id not a string")?,
                            )?;
                            if user == own {
                                continue;
                            }
                            let name = room
                                .get_member_no_sync(user)
                                .await?
                                .and_then(|member| member.display_name().map(str::to_owned));
                            users.push(RoomMember {
                                user: user.to_string(),
                                name,
                            });
                        }
                        moment.typing = Some(users);
                    }
                    Some("m.receipt") => {
                        moment.receipts |= receipts_of_others(own, &event);
                    }
                    _ => {}
                }
            }
            if moment.typing.is_some() || moment.receipts {
                moments.push(moment);
            }
        }
        Ok(moments)
    }

    /// Tells connected clients of the moment, in rooms recorded.
    async fn notify(&self, moments: Vec<Moment>) -> Result<()> {
        if moments.is_empty() {
            return Ok(());
        }
        let account = self.account.clone();
        let ids: Vec<String> = moments.iter().map(|m| m.room_id.clone()).collect();
        let rooms = read(&self.reader, move |conn| {
            rooms::room_ids(conn, &account, &ids)
        })
        .await?;
        let hub = self.dispatcher.hub();
        for (moment, room) in moments.into_iter().zip(rooms) {
            let Some(room) = room else { continue };
            if let Some(users) = moment.typing {
                hub.notify(Notice::Typing(Typing { room, users }));
            }
            if moment.receipts {
                hub.notify(Notice::ReceiptsChanged(ReceiptsChanged { room }));
            }
        }
        Ok(())
    }

    /// The rooms in `response` with changes uplink may store: events, the
    /// account's own receipts, state (a name, an avatar, a space's
    /// children), or membership. Others' receipts are not stored, so on
    /// their own they are not changes; but the first sync records every
    /// room.
    async fn observe(&self, response: SyncResponse, first: bool) -> Result<Vec<ObservedRoom>> {
        let own = self.client.user_id().context("the client has no user")?;
        let mut observed = vec![];
        for (room_id, update) in response.rooms.joined {
            let receipts = own_receipts(own, &update.ephemeral)?;
            let state = match &update.state {
                State::Before(events) | State::After(events) => events,
            };
            if first
                || !update.timeline.events.is_empty()
                || !state.is_empty()
                || !receipts.is_empty()
            {
                observed.push(
                    self.room(
                        &room_id,
                        Membership::Joined,
                        update.timeline.events,
                        update.timeline.limited,
                        receipts,
                    )
                    .await?,
                );
            }
        }
        for (room_id, update) in response.rooms.left {
            observed.push(
                self.room(
                    &room_id,
                    Membership::Left,
                    update.timeline.events,
                    update.timeline.limited,
                    vec![],
                )
                .await?,
            );
        }
        for room_id in response.rooms.invited.into_keys() {
            observed.push(
                self.room(&room_id, Membership::Invited, vec![], false, vec![])
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
        limited: bool,
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
            space: room.is_space(),
            avatar: avatar(&room).await?,
            children: space_children(&room).await?,
            topic: room.topic().filter(|t| !t.trim().is_empty()),
            joined_members: room.joined_members_count(),
            version: room
                .version()
                .unwrap_or(matrix_sdk::ROOM_VERSION_FALLBACK)
                .to_string(),
            events: observed,
            limited,
            own_receipts,
        })
    }
}

/// The room's avatar or, of a direct chat without one, the other user's,
/// as Element shows.
async fn avatar(room: &Room) -> Result<Option<String>> {
    if let Some(url) = room.avatar_url() {
        return Ok(Some(url.to_string()));
    }
    let targets = room.direct_targets();
    let [target] = targets.iter().collect::<Vec<_>>()[..] else {
        return Ok(None);
    };
    let Some(user) = target.as_user_id() else {
        return Ok(None);
    };
    Ok(room
        .get_member_no_sync(user)
        .await?
        .and_then(|member| member.avatar_url().map(ToString::to_string)))
}

/// The rooms a space lists (none, of a room that is not a space).
async fn space_children(room: &Room) -> Result<Vec<SpaceChild>> {
    let mut children = vec![];
    for event in room.get_state_events(StateEventType::SpaceChild).await? {
        let raw = match &event {
            RawAnySyncOrStrippedState::Sync(raw) => raw.json().get(),
            RawAnySyncOrStrippedState::Stripped(raw) => raw.json().get(),
        };
        children.extend(SpaceChild::parse(raw)?);
    }
    Ok(children)
}

/// The sync filter: no presence, and of ephemeral events receipts and
/// typing.
fn filter() -> Filter {
    let mut filter = FilterDefinition::with_lazy_loading();
    filter.presence = matrix_sdk::ruma::api::client::filter::Filter::ignore_all();
    filter.room.ephemeral = RoomEventFilter::default();
    filter.room.ephemeral.types = Some(vec!["m.receipt".to_owned(), "m.typing".to_owned()]);
    Filter::FilterDefinition(filter)
}

/// The event as stored, with its sender's name in the room, which `names`
/// caches.
pub(crate) async fn timeline_event(
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

/// What a sync says of a joined room's moment.
struct Moment {
    room_id: String,
    /// Who is typing now, if the sync says.
    typing: Option<Vec<RoomMember>>,
    /// Whether others sent read receipts.
    receipts: bool,
}

/// Whether the `m.receipt` event has receipts of others than `own`.
fn receipts_of_others(own: &UserId, event: &serde_json::Value) -> bool {
    event["content"]
        .as_object()
        .into_iter()
        .flatten()
        .any(|(_, receipts)| {
            receipts
                .as_object()
                .into_iter()
                .flatten()
                .any(|(_, users)| {
                    users
                        .as_object()
                        .is_some_and(|users| users.keys().any(|user| user != own.as_str()))
                })
        })
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

// ── decrypting late ───────────────────────────────────────────────────────

/// Decrypts events recorded undecryptable once their keys come: from
/// another device, from the key backup (which a failed decryption asks,
/// if the device has the backup's key), or imported (SPEC §9.2).
struct Redecryptor {
    account: String,
    client: Client,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    /// Events the syncer recorded undecryptable, by room id, as JSON.
    undecryptable: mpsc::UnboundedReceiver<Vec<(String, String)>>,
}

impl Redecryptor {
    async fn run(mut self) {
        let mut keys = self
            .client
            .encryption()
            .room_keys_received_stream()
            .await
            .expect("a restored client has its crypto store");
        let mut backup = self.client.encryption().backups().state_stream();
        // Keys may have come while uplink was not running.
        retry(&self.account, "decrypting stored events", || {
            self.decrypt_stored(None)
        })
        .await;
        loop {
            tokio::select! {
                received = keys.next() => match received {
                    Some(Ok(keys)) => {
                        for key in keys {
                            let session = (key.room_id.to_string(), key.session_id);
                            retry(&self.account, "decrypting stored events", || {
                                self.decrypt_stored(Some(session.clone()))
                            })
                            .await;
                        }
                    }
                    // Some went by unseen: try them all.
                    Some(Err(_lagged)) => {
                        retry(&self.account, "decrypting stored events", || {
                            self.decrypt_stored(None)
                        })
                        .await;
                    }
                    None => panic!("the room key stream ends with the client"),
                },
                // A backup usable now (as on recovering) has keys of
                // events stored: trying them asks it for those.
                state = backup.next() => match state {
                    Some(Ok(BackupState::Enabled)) | Some(Err(_)) => {
                        retry(&self.account, "decrypting stored events", || {
                            self.decrypt_stored(None)
                        })
                        .await;
                    }
                    Some(Ok(_)) => {}
                    None => panic!("the backup state stream ends with the client"),
                },
                recorded = self.undecryptable.recv() => {
                    let recorded: Vec<(String, String)> = recorded.expect("the syncer runs as long as the redecryptor");
                    // Trying asks the key backup for their keys.
                    retry(&self.account, "decrypting new events", || {
                        self.decrypt(recorded.clone())
                    })
                    .await;
                }
            }
        }
    }

    /// Decrypts the stored events still undecryptable, of a session only,
    /// if given: a room's Matrix id and a Megolm session id.
    async fn decrypt_stored(&self, session: Option<(String, String)>) -> Result<()> {
        let account = self.account.clone();
        let stored = read(&self.reader, move |conn| {
            rooms::undecryptable(
                conn,
                &account,
                session.as_ref().map(|(r, s)| (r.as_str(), s.as_str())),
            )
        })
        .await?;
        self.decrypt(stored).await
    }

    /// Decrypts `events` (by room id, as JSON), recording those it can.
    async fn decrypt(&self, events: Vec<(String, String)>) -> Result<()> {
        let mut decrypted: Vec<(String, Vec<ObservedEvent>)> = vec![];
        for (room_id, raw) in events {
            let Some(room) = self.client.get_room(<&RoomId>::try_from(room_id.as_str())?) else {
                continue;
            };
            let encrypted: Raw<OriginalSyncRoomEncryptedEvent> = serde_json::from_str(&raw)?;
            let event = room.decrypt_event(&encrypted, None).await?;
            if !matches!(event.kind, TimelineEventKind::Decrypted(_)) {
                continue;
            }
            let event =
                ObservedEvent::parse(event.raw().json().get(), None, Encryption::Decrypted)?;
            match decrypted.last_mut() {
                Some((last, events)) if *last == room_id => events.push(event),
                _ => decrypted.push((room_id, vec![event])),
            }
        }
        for (room_id, events) in decrypted {
            self.dispatcher
                .observe(self.account.clone(), Decrypted { room_id, events })
                .await?;
        }
        Ok(())
    }
}

/// Tells connected clients when the account's devices, their trust, its
/// key backup or recovery change (SPEC §9.4).
async fn watch_security(account: String, client: Client, dispatcher: Dispatcher) {
    let encryption = client.encryption();
    let mut backup = encryption.backups().state_stream();
    let mut recovery = encryption.recovery().state_stream();
    let mut devices = encryption
        .devices_stream()
        .await
        .expect("a restored client has its crypto store");
    let mut identities = encryption
        .user_identities_stream()
        .await
        .expect("a restored client has its crypto store");
    loop {
        tokio::select! {
            changed = backup.next() => changed.map(drop),
            changed = recovery.next() => changed.map(drop),
            changed = devices.next() => changed.map(drop),
            changed = identities.next() => changed.map(drop),
        }
        .expect("the client's streams end with it");
        dispatcher
            .hub()
            .notify(Notice::SecurityChanged(SecurityChanged {
                account: account.clone(),
            }));
    }
}

// ── sending ───────────────────────────────────────────────────────────────

struct Sender {
    account: String,
    client: Client,
    /// The files uploaded to send.
    blobs: Store,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    wake: broadcast::Receiver<Arc<LogEntry>>,
}

impl Sender {
    /// Pushes until the process exits, retrying failures with backoff. A
    /// failure holds up the account's pushes, as they go in turn, so it is
    /// recorded for the outbox as they start and stop failing, not at each
    /// retry (SPEC §6.6).
    async fn run(mut self) {
        retry(&self.account, "recovering interrupted sends", || {
            self.recover()
        })
        .await;
        let mut failing = retry(&self.account, "reading whether failing", || async {
            let account = self.account.clone();
            read(&self.reader, move |conn| push::is_failing(conn, &account)).await
        })
        .await;
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.step().await {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                    if failing {
                        self.record_failing(None).await;
                        failing = false;
                    }
                    self.wait().await;
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    tracing::warn!(account = self.account, "sending failed: {error}");
                    if !failing {
                        self.record_failing(Some(error)).await;
                        failing = true;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    /// Records that the account's changes are not reaching the homeserver,
    /// for `error`, or are again. If that cannot be recorded either, the
    /// database is failing too, which the next step reports.
    async fn record_failing(&self, error: Option<String>) {
        let at = send::now_ms();
        if let Err(e) = self.observe(PushFailing { error, at }).await {
            tracing::warn!(account = self.account, "recording the failure: {e:#}");
        }
    }

    /// Queues again the messages a previous run was interrupted sending:
    /// their transaction ids make sending them again safe.
    async fn recover(&self) -> Result<()> {
        let account = self.account.clone();
        let interrupted = read(&self.reader, move |conn| send::interrupted(conn, &account)).await?;
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
        let due = read(&self.reader, move |conn| send::due(conn, &account)).await?;
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
            let txn_id: OwnedTransactionId = send::transaction_id(item.item).into();
            let sent = self.send(&room, &item.content, txn_id).await?;
            match sent {
                Ok(event_id) => {
                    self.observe(SendSucceeded {
                        item: item.item,
                        event_id: event_id.map(|id| id.to_string()),
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

    /// Sends `content` to the room as transaction `txn_id`; the event id
    /// the homeserver gave it (but for changes to members, of which the
    /// homeserver says none), or why it did not. Fails if the file to
    /// send could not be read.
    async fn send(
        &self,
        room: &Room,
        content: &MatrixContent,
        txn_id: OwnedTransactionId,
    ) -> Result<Result<Option<OwnedEventId>, matrix_sdk::Error>> {
        let sent = match content {
            MatrixContent::Text {
                body,
                emote,
                reply_to,
                replaces,
            } => {
                let content = text_content(body, *emote, reply_to.as_deref(), replaces.as_deref());
                room.send_raw("m.room.message", content)
                    .with_transaction_id(&txn_id)
                    .await
                    .map(|sent| sent.response.event_id)
            }
            MatrixContent::Reaction { event, key } => room
                .send_raw(
                    "m.reaction",
                    json!({ "m.relates_to": {
                        "rel_type": "m.annotation", "event_id": event, "key": key,
                    } }),
                )
                .with_transaction_id(&txn_id)
                .await
                .map(|sent| sent.response.event_id),
            MatrixContent::Redaction { event } => room
                .redact(<&EventId>::try_from(event.as_str())?, None, Some(txn_id))
                .await
                .map(|redacted| redacted.event_id)
                .map_err(matrix_sdk::Error::from),
            MatrixContent::File {
                blob,
                name,
                mime,
                reply_to,
            } => {
                let blobs = self.blobs.clone();
                let hash = VariantHash(blob.0.clone());
                let data = tokio::task::spawn_blocking(move || blobs.get(&hash))
                    .await
                    .expect("reading a blob does not panic")
                    .with_context(|| format!("reading blob {}", blob.0))?;
                let mime: mime::Mime = mime.parse().context("the send's MIME type")?;
                let reply = reply_to
                    .as_deref()
                    .map(|event| -> Result<_> {
                        Ok(Reply {
                            event_id: <&EventId>::try_from(event)?.to_owned(),
                            enforce_thread: EnforceThread::MaybeThreaded,
                            add_mentions: AddMentions::Yes,
                        })
                    })
                    .transpose()?;
                room.send_attachment(
                    name,
                    &mime,
                    data,
                    AttachmentConfig::new().txn_id(txn_id).reply(reply),
                )
                .await
                .map(|sent| sent.event_id)
            }
            MatrixContent::Name { name } => room
                .send_state_event_raw("m.room.name", "", json!({ "name": name }))
                .await
                .map(|sent| sent.event_id),
            MatrixContent::Topic { topic } => room
                .send_state_event_raw("m.room.topic", "", json!({ "topic": topic }))
                .await
                .map(|sent| sent.event_id),
            MatrixContent::Member {
                user,
                action,
                reason,
            } => {
                // Unlike events, these have no transaction id: sent again
                // after an interrupted send that went out, the homeserver
                // may refuse them as made already.
                let user = <&UserId>::try_from(user.as_str())?;
                let reason = reason.as_deref();
                return Ok(match action {
                    MemberAction::Invite => room.invite_user_by_id(user).await,
                    MemberAction::Kick => room.kick_user(user, reason).await,
                    MemberAction::Ban => room.ban_user(user, reason).await,
                    MemberAction::Unban => room.unban_user(user, reason).await,
                }
                .map(|()| None));
            }
        };
        Ok(sent.map(Some))
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

/// A message's content: `body` in Markdown, formatted as HTML if it has
/// formatting (as Element does), replying to `reply_to` or replacing the
/// account's own message `replaces`. Replies carry no fallback (the
/// quote of what they answer), which the spec no longer asks for.
pub fn text_content(
    body: &str,
    emote: bool,
    reply_to: Option<&str>,
    replaces: Option<&str>,
) -> serde_json::Value {
    let message = |body: &str| {
        let mut content = json!({
            "msgtype": if emote { "m.emote" } else { "m.text" },
            "body": body,
        });
        if let Some(html) = FormattedBody::markdown(body) {
            content["format"] = json!("org.matrix.custom.html");
            content["formatted_body"] = json!(html.body);
        }
        content
    };
    match replaces {
        // An edit: its own body, for clients that do not know edits, is the
        // new one marked as such.
        Some(replaces) => {
            let mut content = message(&format!("* {body}"));
            content["m.new_content"] = message(body);
            content["m.relates_to"] = json!({ "rel_type": "m.replace", "event_id": replaces });
            content
        }
        None => {
            let mut content = message(body);
            if let Some(reply_to) = reply_to {
                content["m.relates_to"] = json!({ "m.in_reply_to": { "event_id": reply_to } });
            }
            content
        }
    }
}

/// The homeserver's reason, if it refused the request (a 4xx other than
/// rate limiting); `None` if the failure may pass.
pub(crate) fn refusal(error: &matrix_sdk::Error) -> Option<String> {
    let api = error.as_client_api_error()?;
    let status = api.status_code.as_u16();
    ((400..500).contains(&status) && status != 429).then(|| api.to_string())
}
