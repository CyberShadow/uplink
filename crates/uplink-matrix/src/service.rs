//! Answers the Matrix requests of the WebSocket protocol (SPEC §5.4).

use crate::{
    backfill,
    client::{self, Clients},
    media,
    membership::ChangeMembership,
    preview,
    query::Rooms,
    room_tags::{self, SetRoomTags},
    rooms::{self, Filled, FullyRead, MarkFullyRead, MarkRead},
    security,
    send::{self, CancelMessage, KeepSent, SendMessage},
    space_order::{self, OrderSpaces},
    typing::Typing,
    verification,
};
use matrix_sdk::ruma::events::room::MediaSource as MatrixMediaSource;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uplink_protocol::{
    compose::BlobId,
    matrix::{
        GapId, MatrixAccount, MatrixContent, MatrixSendId, MemberAction, MemberInfo, MemberReceipt,
        MemberState, Membership, MembershipChange, NewRoom, RoomEventId, RoomId, RoomMember,
        RoomPowers, RoomTagChange, SpaceOrderChange, TimelinePage, VerificationAnswer,
    },
    ErrorCode, ServerError, ServerResponse,
};
use uplink_realtime::Answer;
use uplink_storage::{
    rusqlite::{params, Connection, OptionalExtension},
    store::{Store, VariantHash},
};
use uplink_tx_log::{
    answer::{error, internal, Reader, Response},
    Dispatcher,
};

/// How a request that waits on the homeserver is answered: later, by its
/// remote part (SPEC §5.6), unless its local part refuses it.
pub type Remote = Result<Answer, ServerError>;

/// An event's media, as served.
pub struct Media {
    pub mime: String,
    /// The file name the event gives.
    pub name: Option<String>,
    /// Whether the browser may show it, rather than only download it.
    pub inline: bool,
    pub data: Vec<u8>,
}

/// The most events a timeline request may ask for.
pub const MAX_LIMIT: u32 = 500;

/// A configured Matrix account.
#[derive(Debug, Clone)]
pub struct ConfiguredAccount {
    pub id: String,
    pub mxid: String,
}

#[derive(Clone)]
pub struct Matrix {
    dispatcher: Dispatcher,
    reader: Reader,
    accounts: Arc<Vec<ConfiguredAccount>>,
    clients: Clients,
    /// Files uploaded to send.
    blobs: Store,
    typing: Typing,
}

impl Matrix {
    /// `reader` is a read-only connection to the database `dispatcher`
    /// writes; `blobs`, the files uploaded to send.
    pub fn new(
        dispatcher: Dispatcher,
        reader: Connection,
        accounts: Vec<ConfiguredAccount>,
        blobs: Store,
    ) -> Self {
        Self {
            dispatcher,
            reader: Reader::new(reader),
            accounts: Arc::new(accounts),
            clients: Clients::default(),
            blobs,
            typing: Typing::default(),
        }
    }

    /// The accounts' clients, which their sync tasks restore.
    pub fn clients(&self) -> Clients {
        self.clients.clone()
    }

    /// A thumbnail of the room's avatar `mxc`, and its media type: only of
    /// its current one, so that the client cannot have the server fetch
    /// whatever it names.
    pub async fn avatar(
        &self,
        room: RoomId,
        mxc: String,
    ) -> Result<(&'static str, Vec<u8>), ServerError> {
        let account = self
            .reader
            .read({
                let mxc = mxc.clone();
                move |conn| match rooms::room_avatar(conn, room).map_err(internal)? {
                    Some((account, Some(avatar))) if avatar == mxc => Ok(account),
                    Some(_) => Err(error(
                        ErrorCode::NotFound,
                        format!("{mxc} is not the avatar of room {}", room.0),
                    )),
                    None => Err(no_room(room)),
                }
            })
            .await?;
        let client = self.client(&account)?;
        let image = client::avatar_thumbnail(&client, &mxc)
            .await
            .map_err(internal)?;
        let mime = media::image_type(&image).ok_or_else(|| {
            internal(anyhow::anyhow!(
                "the thumbnail of {mxc} is not an image shown"
            ))
        })?;
        Ok((mime, image))
    }

    fn client(&self, account: &str) -> Result<matrix_sdk::Client, ServerError> {
        self.clients.get(account).ok_or_else(|| {
            error(
                ErrorCode::NotFound,
                format!("{account} is not connected to its homeserver yet"),
            )
        })
    }

    /// The media `mxc` that the room's event `event` names, or the
    /// homeserver's `thumbnail` of it (SPEC §9.8).
    pub async fn media(
        &self,
        room: RoomId,
        event: RoomEventId,
        mxc: String,
        thumbnail: bool,
    ) -> Result<Media, ServerError> {
        let (account, named) = self
            .reader
            .read({
                let mxc = mxc.clone();
                move |conn| {
                    let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                    let (account, contents) = rooms::event_contents(&tx, room, event)
                        .map_err(internal)?
                        .ok_or_else(|| no_event(room, event))?;
                    let named = media::named(&contents, &mxc).ok_or_else(|| {
                        error(
                            ErrorCode::NotFound,
                            format!("event {} does not name {mxc}", event.0),
                        )
                    })?;
                    Ok((account, named))
                }
            })
            .await?;
        if thumbnail && !matches!(named.source, MatrixMediaSource::Plain(_)) {
            return Err(error(
                ErrorCode::Invalid,
                format!("{mxc} is encrypted: the homeserver makes no thumbnail of it"),
            ));
        }
        let client = self.client(&account)?;
        let data = client::media(&client, named.source, thumbnail)
            .await
            .map_err(internal)?;
        // Thumbnails are of the homeserver's making; what is not named is
        // only shown if it is an image.
        let mime = match (thumbnail, named.mime) {
            (false, Some(mime)) => mime,
            _ => media::image_type(&data)
                .unwrap_or("application/octet-stream")
                .to_owned(),
        };
        Ok(Media {
            inline: media::shown_inline(&mime),
            mime,
            name: named.name,
            data,
        })
    }

    /// The account of the room whose message `event` links `url`, a web
    /// page, but in encrypted rooms: there, as in Element by default,
    /// links are not previewed, which would tell the homeserver of them.
    async fn previewed(
        &self,
        room: RoomId,
        event: RoomEventId,
        url: String,
    ) -> Result<matrix_sdk::Client, ServerError> {
        let account = self
            .reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let (account, contents) = rooms::event_contents(&tx, room, event)
                    .map_err(internal)?
                    .ok_or_else(|| no_event(room, event))?;
                let encrypted: bool = tx
                    .query_row(
                        "SELECT encrypted FROM matrix_room WHERE id = ?1",
                        [room.0],
                        |r| r.get(0),
                    )
                    .map_err(|e| internal(e.into()))?;
                if encrypted {
                    return Err(error(
                        ErrorCode::Invalid,
                        "links in encrypted rooms are not previewed".into(),
                    ));
                }
                if !preview::linked(&contents, &url) {
                    return Err(error(
                        ErrorCode::NotFound,
                        format!("event {} links no {url}", event.0),
                    ));
                }
                Ok(account)
            })
            .await?;
        self.client(&account)
    }

    /// A preview of the page `url` that the room's message `event` links.
    pub async fn preview_url(&self, room: RoomId, event: RoomEventId, url: String) -> Remote {
        let client = self.previewed(room, event, url.clone()).await?;
        Ok(Answer::later(async move {
            let og = preview::fetch(&client, &url).await.map_err(internal)?;
            Ok(ServerResponse::UrlPreview {
                preview: preview::preview(&og),
            })
        }))
    }

    /// A thumbnail of the image `mxc` of the preview of `url`, which the
    /// room's message `event` links, and its media type.
    pub async fn preview_image(
        &self,
        room: RoomId,
        event: RoomEventId,
        url: String,
        mxc: String,
    ) -> Result<(&'static str, Vec<u8>), ServerError> {
        let client = self.previewed(room, event, url.clone()).await?;
        // The homeserver caches its previews.
        let og = preview::fetch(&client, &url).await.map_err(internal)?;
        if preview::preview(&og).and_then(|p| p.image).as_ref() != Some(&mxc) {
            return Err(error(
                ErrorCode::NotFound,
                format!("{mxc} is not the image of {url}"),
            ));
        }
        let image = client::media(&client, MatrixMediaSource::Plain(mxc.as_str().into()), true)
            .await
            .map_err(internal)?;
        let mime = media::image_type(&image).ok_or_else(|| {
            internal(anyhow::anyhow!(
                "the thumbnail of {mxc} is not an image shown"
            ))
        })?;
        Ok((mime, image))
    }

    /// A thumbnail of `user`'s avatar in the room, and its media type;
    /// `None` if the client knows of none.
    pub async fn member_avatar(
        &self,
        room: RoomId,
        user: String,
    ) -> Result<Option<(&'static str, Vec<u8>)>, ServerError> {
        let (account, room_id) = self
            .reader
            .read(move |conn| {
                rooms::room_of(conn, room)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(room))
            })
            .await?;
        let client = self.client(&account)?;
        let Some(mxc) = client::member_avatar(&client, &room_id, &user)
            .await
            .map_err(internal)?
        else {
            return Ok(None);
        };
        let image = client::avatar_thumbnail(&client, &mxc)
            .await
            .map_err(internal)?;
        let mime = media::image_type(&image).ok_or_else(|| {
            internal(anyhow::anyhow!(
                "the thumbnail of {mxc} is not an image shown"
            ))
        })?;
        Ok(Some((mime, image)))
    }

    /// The room's client-side room, of a joined room the client has.
    async fn sdk_room(&self, room: RoomId) -> Result<(String, matrix_sdk::Room), ServerError> {
        let (account, room_id) = self
            .reader
            .read(move |conn| {
                rooms::room_of(conn, room)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(room))
            })
            .await?;
        let client = self.client(&account)?;
        let sdk_room = <&matrix_sdk::ruma::RoomId>::try_from(room_id.as_str())
            .ok()
            .and_then(|id| client.get_room(id))
            .ok_or_else(|| {
                error(
                    ErrorCode::NotFound,
                    format!("{account}'s client does not have room {room_id}"),
                )
            })?;
        Ok((account, sdk_room))
    }

    /// The account's device and its encryption (SPEC §9.4).
    pub async fn get_security(&self, account: &str) -> Remote {
        let client = self.client(account)?;
        Ok(Answer::later(
            self.clone().security(account.to_owned(), client),
        ))
    }

    /// The account's security, which lists its devices on the homeserver.
    async fn security(self, account: String, client: matrix_sdk::Client) -> Response {
        let verifications = self.clients.verifications(&account);
        let security = security::security(&account, &client, &verifications)
            .await
            .map_err(internal)?;
        Ok(ServerResponse::MatrixSecurity { security })
    }

    /// Asks to verify uplink's device with `device`, or with whichever of
    /// the account's answers first.
    pub async fn verify_device(&self, account: &str, device: Option<&str>) -> Remote {
        let client = self.client(account)?;
        let (this, account, device) = (self.clone(), account.to_owned(), device.map(str::to_owned));
        Ok(Answer::later(async move {
            let encryption = client.encryption();
            let own = client.user_id().expect("a restored client has its user");
            let request = match device.as_deref() {
                Some(device) => {
                    let device = <&matrix_sdk::ruma::DeviceId>::from(device);
                    encryption
                        .get_device(own, device)
                        .await
                        .map_err(|e| internal(e.into()))?
                        .ok_or_else(|| {
                            error(
                                ErrorCode::NotFound,
                                format!("{account} has no device {device}"),
                            )
                        })?
                        .request_verification_with_methods(verification::methods())
                        .await
                        .map_err(|e| internal(e.into()))?
                }
                None => encryption
                    .get_user_identity(own)
                    .await
                    .map_err(|e| internal(e.into()))?
                    .ok_or_else(|| {
                        error(
                            ErrorCode::Invalid,
                            format!(
                                "{account} has no cross-signing identity: verify with a device"
                            ),
                        )
                    })?
                    .request_verification_with_methods(verification::methods())
                    .await
                    .map_err(|e| internal(e.into()))?,
            };
            verification::track(&this.clients, &this.dispatcher, &account, request);
            this.security(account, client).await
        }))
    }

    /// Answers the account's verification `flow_id`.
    pub async fn answer_verification(
        &self,
        account: &str,
        flow_id: &str,
        answer: VerificationAnswer,
    ) -> Remote {
        let client = self.client(account)?;
        let request = self
            .clients
            .verifications(account)
            .into_iter()
            .find(|r| r.flow_id() == flow_id)
            .ok_or_else(|| {
                error(
                    ErrorCode::NotFound,
                    format!("{account} has no verification {flow_id}"),
                )
            })?;
        let comparing = || {
            verification::sas(&request).ok_or_else(|| {
                error(
                    ErrorCode::Invalid,
                    format!("verification {flow_id} is not comparing"),
                )
            })
        };
        let sas = match answer {
            VerificationAnswer::Confirm | VerificationAnswer::Mismatch => Some(comparing()?),
            VerificationAnswer::Accept | VerificationAnswer::Cancel => None,
        };
        let (this, account) = (self.clone(), account.to_owned());
        Ok(Answer::later(async move {
            match (answer, sas) {
                (VerificationAnswer::Accept, None) => {
                    request.accept_with_methods(verification::methods()).await
                }
                (VerificationAnswer::Confirm, Some(sas)) => sas.confirm().await,
                (VerificationAnswer::Mismatch, Some(sas)) => sas.mismatch().await,
                (VerificationAnswer::Cancel, None) => request.cancel().await,
                _ => unreachable!("comparing only to confirm or not"),
            }
            .map_err(|e| internal(e.into()))?;
            this.security(account, client).await
        }))
    }

    /// Unlocks Secret Storage with the recovery key or passphrase.
    pub async fn recover(&self, account: &str, recovery: &str) -> Remote {
        let client = self.client(account)?;
        let (this, account, recovery) = (self.clone(), account.to_owned(), recovery.to_owned());
        Ok(Answer::later(async move {
            client
                .encryption()
                .recovery()
                .recover(&recovery)
                .await
                .map_err(|e| {
                    error(
                        ErrorCode::Invalid,
                        format!("could not unlock Secret Storage: {e}"),
                    )
                })?;
            this.security(account, client).await
        }))
    }

    /// Imports room keys from the uploaded export `blob`.
    pub async fn import_room_keys(&self, account: &str, blob: &BlobId, passphrase: &str) -> Remote {
        let client = self.client(account)?;
        let hash = VariantHash::parse(&blob.0)
            .ok_or_else(|| error(ErrorCode::Invalid, format!("not a blob: {:?}", blob.0)))?;
        if self.blobs.size(&hash).map_err(internal)?.is_none() {
            return Err(error(ErrorCode::NotFound, format!("no blob {}", blob.0)));
        }
        let export = self.blobs.get(&hash).map_err(internal)?;
        let passphrase = passphrase.to_owned();
        // Decrypting many keys takes a while, and uploading them to the key
        // backup waits on the homeserver.
        Ok(Answer::later(async move {
            // A wrong passphrase, or a file not an export, is the request's.
            let (imported, total) = security::import_room_keys(&client, &export, &passphrase)
                .await
                .map_err(|e| error(ErrorCode::Invalid, format!("{e:#}")))?;
            Ok(ServerResponse::RoomKeysImported { imported, total })
        }))
    }

    /// The account's room keys, exported encrypted with `passphrase`.
    pub async fn export_room_keys(
        &self,
        account: &str,
        passphrase: &str,
    ) -> Result<Vec<u8>, ServerError> {
        let client = self.client(account)?;
        security::export_room_keys(&client, passphrase)
            .await
            .map_err(internal)
    }

    /// Fetches the homeserver's page of events before the room's gap
    /// `gap`, and stores them (SPEC §9.2).
    pub async fn fill_gap(&self, room: RoomId, gap: GapId) -> Remote {
        let before: String = self
            .reader
            .read(move |conn| {
                conn.query_row(
                    "SELECT e.event_id FROM matrix_gap g JOIN matrix_event e ON e.id = g.before_event
                     WHERE g.id = ?1 AND g.room = ?2",
                    params![gap.0, room.0],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| internal(e.into()))?
                .ok_or_else(|| {
                    error(
                        ErrorCode::NotFound,
                        format!("room {} has no gap {}", room.0, gap.0),
                    )
                })
            })
            .await?;
        let (account, sdk_room) = self.sdk_room(room).await?;
        let dispatcher = self.dispatcher.clone();
        Ok(Answer::later(async move {
            let page = backfill::page(&sdk_room, &before).await.map_err(internal)?;
            dispatcher
                .observe(
                    account,
                    Filled {
                        room,
                        gap,
                        version: page.version,
                        events: page.events,
                        start: page.start,
                        own_receipts: page.own_receipts,
                        fully_read: page.fully_read,
                    },
                )
                .await
                .map_err(internal)?;
            Ok(ServerResponse::Done)
        }))
    }

    /// The account is typing in the room, or has stopped: told the
    /// homeserver in the background.
    pub async fn set_typing(&self, room: RoomId, typing: bool) -> Response {
        let (account, sdk_room) = self.sdk_room(room).await?;
        self.typing.set(
            &self.clients,
            account,
            room,
            sdk_room.room_id().to_owned(),
            typing,
        );
        Ok(ServerResponse::Done)
    }

    /// How far the room's joined members (but the account) have read, of
    /// those the client knows, by their receipts for stored events.
    pub async fn list_receipts(&self, room: RoomId) -> Response {
        use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
        let (_, sdk_room) = self.sdk_room(room).await?;
        let own = sdk_room.own_user_id().to_owned();
        let members = sdk_room
            .members_no_sync(matrix_sdk::RoomMemberships::JOIN)
            .await
            .map_err(|e| internal(e.into()))?;
        let mut read = vec![];
        for member in members.iter().filter(|m| m.user_id() != own) {
            // Unthreaded receipts, else those of the main timeline.
            let mut receipt = None;
            for thread in [ReceiptThread::Unthreaded, ReceiptThread::Main] {
                receipt = sdk_room
                    .load_user_receipt(ReceiptType::Read, &thread, member.user_id())
                    .await
                    .map_err(|e| internal(e.into()))?;
                if receipt.is_some() {
                    break;
                }
            }
            if let Some((event_id, receipt)) = receipt {
                read.push((
                    RoomMember {
                        user: member.user_id().to_string(),
                        name: member.display_name().map(str::to_owned),
                    },
                    event_id.to_string(),
                    receipt.ts.map(|ts| i64::from(ts.0)),
                ));
            }
        }
        self.reader
            .read(move |conn| {
                let ids: Vec<String> = read.iter().map(|(_, event, _)| event.clone()).collect();
                let events = rooms::event_ids(conn, room, &ids).map_err(internal)?;
                let receipts = read
                    .into_iter()
                    .zip(events)
                    .filter_map(|((member, _, ts), event)| {
                        event.map(|(event, pos)| MemberReceipt {
                            member,
                            event,
                            pos,
                            ts,
                        })
                    })
                    .collect();
                Ok(ServerResponse::Receipts { room, receipts })
            })
            .await
    }

    /// The room's members, as the account's client knows them (fetched
    /// first, as sync loads them lazily), and what the account may do.
    pub async fn list_members(&self, room: RoomId) -> Remote {
        let (_, sdk_room) = self.sdk_room(room).await?;
        Ok(Answer::later(members(room, sdk_room)))
    }

    /// Creates the room: named, with its topic, inviting users, maybe
    /// encrypted, private or else public under its address.
    pub async fn create_room(&self, account: &str, room: NewRoom) -> Remote {
        use matrix_sdk::ruma::{
            api::client::room::{
                create_room::v3::{Request, RoomPreset},
                Visibility,
            },
            events::{room::encryption::RoomEncryptionEventContent, InitialStateEvent},
        };
        let client = self.client(account)?;
        let invite = room
            .invite
            .iter()
            .map(|user| user_id(user))
            .collect::<Result<Vec<_>, _>>()?;
        let mut request = Request::new();
        let name = room.name.trim();
        request.name = (!name.is_empty()).then(|| name.to_owned());
        request.topic = room
            .topic
            .as_deref()
            .map(str::trim)
            .filter(|topic| !topic.is_empty())
            .map(str::to_owned);
        request.invite = invite;
        match room.alias {
            Some(alias) => {
                if alias.is_empty()
                    || alias.contains([':', '#'])
                    || alias.contains(char::is_whitespace)
                {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!("not an address's localpart: {alias:?}"),
                    ));
                }
                request.room_alias_name = Some(alias);
                request.visibility = Visibility::Public;
                request.preset = Some(RoomPreset::PublicChat);
            }
            None => request.preset = Some(RoomPreset::PrivateChat),
        }
        if room.encrypted {
            request.initial_state = vec![InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any()];
        }
        Ok(Answer::later(async move {
            let created = client.create_room(request).await.map_err(refused)?;
            Ok(ServerResponse::MatrixRoomJoined {
                room_id: created.room_id().to_string(),
            })
        }))
    }

    /// The account's direct chat with `user` it has joined, else one
    /// created (encrypted), inviting them.
    pub async fn start_direct_chat(&self, account: &str, user: &str) -> Remote {
        let client = self.client(account)?;
        let user = user_id(user)?;
        if client.user_id() == Some(&*user) {
            return Err(error(
                ErrorCode::Invalid,
                "a direct chat is with someone else".into(),
            ));
        }
        let joined = |room: matrix_sdk::Room| ServerResponse::MatrixRoomJoined {
            room_id: room.room_id().to_string(),
        };
        Ok(match client.get_dm_room(&user) {
            Some(room) => Answer::Now(Ok(joined(room))),
            None => {
                Answer::later(
                    async move { Ok(joined(client.create_dm(&user).await.map_err(refused)?)) },
                )
            }
        })
    }

    /// Joins the room by its address or id.
    pub async fn join_room(&self, account: &str, room: &str) -> Remote {
        let client = self.client(account)?;
        let room = <&matrix_sdk::ruma::RoomOrAliasId>::try_from(room.trim()).map_err(|_| {
            error(
                ErrorCode::Invalid,
                format!("not a room's address or id: {room:?}"),
            )
        })?;
        let room = room.to_owned();
        Ok(Answer::later(async move {
            let joined = client
                .join_room_by_id_or_alias(&room, &[])
                .await
                .map_err(refused)?;
            Ok(ServerResponse::MatrixRoomJoined {
                room_id: joined.room_id().to_string(),
            })
        }))
    }

    /// The room the address is of, as the homeserver resolves it.
    pub async fn resolve_alias(&self, account: &str, alias: &str) -> Remote {
        let client = self.client(account)?;
        let alias = <&matrix_sdk::ruma::RoomAliasId>::try_from(alias)
            .map_err(|_| {
                error(
                    ErrorCode::Invalid,
                    format!("not a room's address: {alias:?}"),
                )
            })?
            .to_owned();
        Ok(Answer::later(async move {
            let resolved = client
                .resolve_room_alias(&alias)
                .await
                .map_err(|e| refused(e.into()))?;
            Ok(ServerResponse::RoomAliasResolved {
                room_id: resolved.room_id.to_string(),
            })
        }))
    }

    pub async fn list_rooms(&self) -> Response {
        let configured = self.accounts.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let mut accounts = vec![];
                let mut rooms = vec![];
                for account in configured.iter() {
                    accounts.push(MatrixAccount {
                        id: account.id.clone(),
                        mxid: account.mxid.clone(),
                        synced: rooms::sync_state(&tx, &account.id)
                            .map_err(internal)?
                            .is_some(),
                    });
                    rooms.extend(rooms::rooms(&tx, &account.id, &account.mxid).map_err(internal)?);
                }
                Ok(ServerResponse::Rooms {
                    as_of,
                    accounts,
                    rooms,
                })
            })
            .await
    }

    pub async fn list_timeline(&self, room: RoomId, page: TimelinePage, limit: u32) -> Response {
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(error(
                ErrorCode::Invalid,
                format!("limit must be between 1 and {MAX_LIMIT}"),
            ));
        }
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                if rooms::room_account(&tx, room).map_err(internal)?.is_none() {
                    return Err(no_room(room));
                }
                let (events, more_before, gaps) =
                    rooms::timeline(&tx, room, page, limit).map_err(internal)?;
                // One after an event that is full may have another after it.
                let at_end = match page {
                    TimelinePage::Latest => true,
                    TimelinePage::Before { .. } => false,
                    TimelinePage::After { .. } => events.len() < limit as usize,
                };
                let sends = at_end
                    .then(|| send::sends(&tx, room))
                    .transpose()
                    .map_err(internal)?;
                Ok(ServerResponse::Timeline {
                    as_of,
                    room,
                    events,
                    more_before,
                    gaps,
                    sends,
                })
            })
            .await
    }

    pub async fn search_room(
        &self,
        room: RoomId,
        query: String,
        before: Option<RoomEventId>,
        limit: u32,
    ) -> Response {
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(error(
                ErrorCode::Invalid,
                format!("limit must be between 1 and {MAX_LIMIT}"),
            ));
        }
        if query.trim().is_empty() {
            return Err(error(ErrorCode::Invalid, "the query is empty".into()));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64;
        let query = uplink_query::parse(&query, now).map_err(|e| error(ErrorCode::Invalid, e.0))?;
        let accounts = self.accounts.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let rooms = Rooms::load(&tx, &accounts).map_err(internal)?;
                let Some(listed) = rooms.room(room) else {
                    return Err(no_room(room));
                };
                if let Some(before) = before {
                    if !rooms::event_in_room(&tx, room, before).map_err(internal)? {
                        return Err(no_event(room, before));
                    }
                }
                let (events, more) = rooms
                    .search_room(&tx, listed, &query, before, limit)
                    .map_err(internal)?;
                Ok(ServerResponse::RoomSearch {
                    as_of,
                    room,
                    events,
                    more,
                })
            })
            .await
    }

    pub async fn list_room_events(&self, room: RoomId, events: Vec<RoomEventId>) -> Response {
        if events.len() > MAX_LIMIT as usize {
            return Err(error(
                ErrorCode::Invalid,
                format!("at most {MAX_LIMIT} events at once"),
            ));
        }
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                if rooms::room_account(&tx, room).map_err(internal)?.is_none() {
                    return Err(no_room(room));
                }
                let events = rooms::listed_events(&tx, room, &events).map_err(internal)?;
                Ok(ServerResponse::RoomEvents {
                    as_of,
                    room,
                    events,
                })
            })
            .await
    }

    pub async fn list_edits(&self, room: RoomId, event: RoomEventId) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                if !rooms::event_in_room(&tx, room, event).map_err(internal)? {
                    return Err(no_event(room, event));
                }
                let edits = rooms::edits(&tx, room, event).map_err(internal)?;
                Ok(ServerResponse::Edits {
                    as_of,
                    room,
                    event,
                    edits,
                })
            })
            .await
    }

    pub async fn list_pinned(&self, room: RoomId) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                if rooms::room_account(&tx, room).map_err(internal)?.is_none() {
                    return Err(no_room(room));
                }
                let events = rooms::pinned(&tx, room).map_err(internal)?;
                Ok(ServerResponse::Pinned {
                    as_of,
                    room,
                    events,
                })
            })
            .await
    }

    /// The thread whose root is `root`, which the main timeline lists.
    pub async fn list_thread(&self, room: RoomId, root: RoomEventId) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let (root, replies) = rooms::thread(&tx, room, root)
                    .map_err(internal)?
                    .ok_or_else(|| no_event(room, root))?;
                Ok(ServerResponse::Thread {
                    as_of,
                    room,
                    root: Box::new(root),
                    replies,
                })
            })
            .await
    }

    /// The room's sends not yet reconciled, for the end of its timeline.
    /// Only to joined rooms of configured accounts, and of events stored
    /// in the room: replying to one, editing one of the account's own
    /// messages, reacting to one or redacting one; files only as uploaded.
    pub async fn send_message(&self, room: RoomId, content: MatrixContent) -> Response {
        let invalid = |message: String| Err(error(ErrorCode::Invalid, message));
        match &content {
            MatrixContent::Text { body, .. } if body.trim().is_empty() => {
                return invalid("the message is empty".into());
            }
            MatrixContent::Reaction { key, .. } if key.is_empty() => {
                return invalid("the reaction is empty".into());
            }
            MatrixContent::File {
                blob, name, mime, ..
            } => {
                if name.trim().is_empty() || name.contains(char::is_control) {
                    return invalid(format!("not a file name: {name:?}"));
                }
                if mime.parse::<mime::Mime>().is_err() {
                    return invalid(format!("not a MIME type: {mime:?}"));
                }
                let hash = VariantHash::parse(&blob.0).ok_or_else(|| {
                    error(ErrorCode::Invalid, format!("not a blob: {:?}", blob.0))
                })?;
                if self.blobs.size(&hash).map_err(internal)?.is_none() {
                    return Err(error(ErrorCode::NotFound, format!("no blob {}", blob.0)));
                }
            }
            MatrixContent::Member {
                user,
                action,
                reason,
            } => {
                if <&matrix_sdk::ruma::UserId>::try_from(user.as_str()).is_err() {
                    return invalid(format!("not a Matrix user id: {user:?}"));
                }
                // The homeserver's API takes none.
                if *action == MemberAction::Invite && reason.is_some() {
                    return invalid("an invitation has no reason".into());
                }
            }
            MatrixContent::Pin { event, .. } => {
                if <&matrix_sdk::ruma::EventId>::try_from(event.as_str()).is_err() {
                    return invalid(format!("not a Matrix event id: {event:?}"));
                }
            }
            MatrixContent::Text { .. }
            | MatrixContent::Reaction { .. }
            | MatrixContent::Redaction { .. }
            | MatrixContent::Name { .. }
            | MatrixContent::Topic { .. } => {}
        }
        let accounts = self.accounts.clone();
        let checked = content.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let account = rooms::room_account(&tx, room)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(room))?;
                let configured = accounts.iter().find(|a| a.id == account);
                let joined = rooms::rooms(&tx, &account, configured.map_or("", |a| &a.mxid))
                    .map_err(internal)?
                    .into_iter()
                    .any(|r| r.id == room && r.membership == Membership::Joined);
                let Some(configured) = configured.filter(|_| joined) else {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!(
                            "room {} is not a joined room of a configured account",
                            room.0
                        ),
                    ));
                };
                let event = |event_id: &str| {
                    rooms::stored_event(&tx, room, event_id)
                        .map_err(internal)?
                        .ok_or_else(|| {
                            error(
                                ErrorCode::NotFound,
                                format!("no event {event_id} in room {}", room.0),
                            )
                        })
                };
                match &checked {
                    MatrixContent::Text {
                        reply_to,
                        replaces,
                        thread,
                        ..
                    } => {
                        let replied = reply_to.as_deref().map(event).transpose()?;
                        if let Some(root) = thread {
                            let invalid = |message: String| Err(error(ErrorCode::Invalid, message));
                            if event(root)?.rel_type.as_deref() == Some("m.thread") {
                                return invalid(format!("{root} is of a thread, not its root"));
                            }
                            if replaces.is_some() {
                                return invalid("an edit is of no thread".into());
                            }
                            if let (Some(reply_to), Some(replied)) = (reply_to, &replied) {
                                let of_thread = replied.rel_type.as_deref() == Some("m.thread")
                                    && replied.relates_to.as_ref() == Some(root);
                                if reply_to != root && !of_thread {
                                    return invalid(format!("{reply_to} is not of thread {root}"));
                                }
                            }
                        }
                        if let Some(replaces) = replaces {
                            let edited = event(replaces)?;
                            if edited.sender != configured.mxid
                                || edited.kind != "m.room.message"
                                || edited.redacted
                            {
                                return Err(error(
                                    ErrorCode::Invalid,
                                    format!("{replaces} is not a message of the account's to edit"),
                                ));
                            }
                        }
                    }
                    MatrixContent::Reaction { event: of, .. } => {
                        event(of)?;
                    }
                    MatrixContent::Redaction { event: of } => {
                        if event(of)?.redacted {
                            return Err(error(
                                ErrorCode::Conflict,
                                format!("{of} is redacted already"),
                            ));
                        }
                    }
                    MatrixContent::File { reply_to, .. } => {
                        if let Some(reply_to) = reply_to {
                            event(reply_to)?;
                        }
                    }
                    // Only stored events are pinned; any is unpinned.
                    MatrixContent::Pin {
                        event: of,
                        pinned: true,
                    } => {
                        if event(of)?.redacted {
                            return Err(error(ErrorCode::Conflict, format!("{of} is redacted")));
                        }
                    }
                    // Whether the account may is the homeserver's to say.
                    MatrixContent::Pin { pinned: false, .. }
                    | MatrixContent::Name { .. }
                    | MatrixContent::Topic { .. }
                    | MatrixContent::Member { .. } => {}
                }
                Ok(())
            })
            .await?;
        self.dispatcher.answer(SendMessage { room, content }).await
    }

    pub async fn cancel_send(&self, item: MatrixSendId) -> Response {
        self.dispatcher.answer(CancelMessage { item }).await
    }

    pub async fn keep_sent(&self, item: MatrixSendId) -> Response {
        self.dispatcher.answer(KeepSent { item }).await
    }

    /// Joining only rooms one is invited to; leaving joined rooms, or
    /// declining invitations.
    pub async fn change_membership(&self, room: RoomId, change: MembershipChange) -> Response {
        let accounts = self.accounts.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let account = rooms::room_account(&tx, room)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(room))?;
                let configured = accounts
                    .iter()
                    .find(|a| a.id == account)
                    .ok_or_else(|| no_room(room))?;
                let listed = rooms::rooms(&tx, &account, &configured.mxid)
                    .map_err(internal)?
                    .into_iter()
                    .find(|r| r.id == room)
                    .ok_or_else(|| no_room(room))?;
                let allowed = match change {
                    MembershipChange::Join => listed.membership == Membership::Invited,
                    MembershipChange::Leave => listed.membership != Membership::Left,
                };
                if !allowed {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!("cannot {change:?} room {} as it is", room.0),
                    ));
                }
                Ok(())
            })
            .await?;
        self.dispatcher
            .answer(ChangeMembership { room, change })
            .await
    }

    /// Reordering joined spaces of one account, each to a valid order
    /// other than its own.
    pub async fn order_spaces(&self, changes: Vec<SpaceOrderChange>) -> Response {
        let accounts = self.accounts.clone();
        let asked = changes.clone();
        self.reader
            .read(move |conn| {
                let invalid = |why: String| Err(error(ErrorCode::Invalid, why));
                let Some(first) = asked.first() else {
                    return invalid("no spaces to reorder".to_owned());
                };
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let account = rooms::room_account(&tx, first.space)
                    .map_err(internal)?
                    .ok_or_else(|| no_room(first.space))?;
                let configured = accounts
                    .iter()
                    .find(|a| a.id == account)
                    .ok_or_else(|| no_room(first.space))?;
                let listed = rooms::rooms(&tx, &account, &configured.mxid).map_err(internal)?;
                let mut seen = std::collections::HashSet::new();
                for change in &asked {
                    let room = listed
                        .iter()
                        .find(|r| r.id == change.space)
                        .ok_or_else(|| no_room(change.space))?;
                    if !(room.space && room.membership == Membership::Joined) {
                        return invalid(format!("room {} is not a joined space", room.id.0));
                    }
                    if !seen.insert(change.space) {
                        return invalid(format!("space {} reordered twice", room.id.0));
                    }
                    if change.to.as_deref().is_some_and(|o| !space_order::valid(o)) {
                        return invalid(format!("invalid order {:?}", change.to));
                    }
                    if change.to == change.from {
                        return invalid(format!("space {} is not moved", room.id.0));
                    }
                }
                Ok(())
            })
            .await?;
        self.dispatcher.answer(OrderSpaces { changes }).await
    }

    /// Tagging stored rooms `add` and untagging them `remove`.
    pub async fn modify_room_tags(
        &self,
        rooms: Vec<RoomId>,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> Response {
        let invalid = |why: String| Err(error(ErrorCode::Invalid, why));
        if rooms.is_empty() {
            return invalid("no rooms to tag".to_owned());
        }
        if add.is_empty() && remove.is_empty() {
            return invalid("no tags to change".to_owned());
        }
        if let Some(tag) = add.iter().find(|t| !room_tags::valid(t)) {
            return invalid(format!("invalid tag {tag:?}"));
        }
        if let Some(tag) = add.iter().find(|t| remove.contains(t)) {
            return invalid(format!("tag {tag:?} both added and removed"));
        }
        if [room_tags::FAVOURITE, room_tags::LOW_PRIORITY]
            .iter()
            .all(|t| add.iter().any(|a| a == t))
        {
            return invalid("a favourite is not of low priority".to_owned());
        }
        let asked = rooms.clone();
        self.reader
            .read(move |conn| {
                for &room in &asked {
                    if rooms::room_account(conn, room).map_err(internal)?.is_none() {
                        return Err(no_room(room));
                    }
                }
                Ok(())
            })
            .await?;
        let changes = rooms
            .iter()
            .flat_map(|&room| {
                let added = add.iter().map(move |tag| RoomTagChange {
                    room,
                    tag: tag.clone(),
                    desired: true,
                });
                let removed = remove.iter().map(move |tag| RoomTagChange {
                    room,
                    tag: tag.clone(),
                    desired: false,
                });
                added.chain(removed)
            })
            .collect();
        self.dispatcher.answer(SetRoomTags { changes }).await
    }

    /// Marks the room read up to `event`; if `fully`, caught up on too
    /// (SPEC §12.15).
    pub async fn mark_read(
        &self,
        room: RoomId,
        event: RoomEventId,
        shown: bool,
        fully: bool,
    ) -> Response {
        self.check_event(room, event).await?;
        self.dispatcher
            .answer(MarkRead {
                room,
                up_to: Some(event),
                shown,
                fully_read: fully.then_some(FullyRead { up_to: Some(event) }),
            })
            .await
    }

    /// Moves the room's fully-read marker to `event`, as catching up does,
    /// by the room being shown.
    pub async fn mark_fully_read(&self, room: RoomId, event: RoomEventId) -> Response {
        self.check_event(room, event).await?;
        self.dispatcher
            .answer(MarkFullyRead {
                room,
                up_to: Some(event),
                shown: true,
            })
            .await
    }

    async fn check_event(&self, room: RoomId, event: RoomEventId) -> Result<(), ServerError> {
        self.reader
            .read(move |conn| {
                if rooms::event_in_room(conn, room, event).map_err(internal)? {
                    Ok(())
                } else {
                    Err(no_event(room, event))
                }
            })
            .await
    }
}

fn no_room(room: RoomId) -> uplink_protocol::ServerError {
    error(ErrorCode::NotFound, format!("no room {}", room.0))
}

fn no_event(room: RoomId, event: RoomEventId) -> ServerError {
    error(
        ErrorCode::NotFound,
        format!("no event {} in room {}", event.0, room.0),
    )
}

/// A Matrix user id, or else the client's mistake.
fn user_id(user: &str) -> Result<matrix_sdk::ruma::OwnedUserId, ServerError> {
    matrix_sdk::ruma::UserId::parse(user.trim()).map_err(|_| {
        error(
            ErrorCode::Invalid,
            format!("not a Matrix user id: {user:?}"),
        )
    })
}

/// What the homeserver said of a request: its refusal is the user's to
/// know (as an address that does not exist), anything else the server's.
fn refused(e: matrix_sdk::Error) -> ServerError {
    match crate::sync::refusal(&e) {
        Some(reason) => error(
            ErrorCode::Invalid,
            format!("the homeserver refused: {reason}"),
        ),
        None => internal(e.into()),
    }
}

/// The room's members, as the account's client knows them (fetched
/// first, as sync loads them lazily), and what the account may do.
async fn members(room: RoomId, sdk_room: matrix_sdk::Room) -> Response {
    use matrix_sdk::ruma::events::room::member::MembershipState;
    use matrix_sdk::ruma::events::{room::power_levels::UserPowerLevel, StateEventType};
    let own = sdk_room.own_user_id().to_owned();
    let levels = sdk_room
        .power_levels()
        .await
        .map_err(|e| internal(e.into()))?;
    let members = sdk_room
        .members(matrix_sdk::RoomMemberships::empty())
        .await
        .map_err(|e| internal(e.into()))?;
    let members = members
        .iter()
        .filter_map(|member| {
            let membership = match member.membership() {
                MembershipState::Join => MemberState::Joined,
                MembershipState::Invite => MemberState::Invited,
                MembershipState::Knock => MemberState::Knocking,
                MembershipState::Leave => MemberState::Left,
                MembershipState::Ban => MemberState::Banned,
                // Not of the spec: not a member it can say anything of.
                _ => return None,
            };
            let user = member.user_id();
            Some(MemberInfo {
                user: user.to_string(),
                name: member.display_name().map(str::to_owned),
                membership,
                power: match levels.for_user(user) {
                    UserPowerLevel::Int(level) => Some(level.into()),
                    // Infinite (the enum is non-exhaustive).
                    _ => None,
                },
                kickable: levels.user_can_kick_user(&own, user),
                bannable: levels.user_can_ban_user(&own, user),
            })
        })
        .collect();
    Ok(ServerResponse::Members {
        room,
        members,
        powers: RoomPowers {
            invite: levels.user_can_invite(&own),
            name: levels.user_can_send_state(&own, StateEventType::RoomName),
            topic: levels.user_can_send_state(&own, StateEventType::RoomTopic),
        },
    })
}
