//! The account's matrix-rust-sdk client (SPEC §9.1, §13.5).
//!
//! The SDK keeps its state and crypto stores in the account's
//! `state_dir`, encrypted with a passphrase derived from the master key.
//! It does not keep the session (access token, device id); uplink seals it
//! next to the stores. `uplink matrix pair` logs in, creating the device
//! and session; the server restores it.

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    ruma::{
        api::client::{
            filter::FilterDefinition, media::get_content_thumbnail::v3::Method,
            sync::sync_events::v3::Filter,
        },
        events::room::MediaSource,
        OwnedMxcUri, UInt,
    },
    store::{RoomLoadSettings, StateStoreDataKey},
    Client,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use uplink_config::MatrixAccountConfig;
use uplink_storage::seal::{self, MasterKey};

/// The accounts' clients, by account id, once their sync tasks have
/// restored them: for what else needs to reach the homeserver as the
/// account (its media).
#[derive(Clone, Default)]
pub struct Clients(Arc<Mutex<HashMap<String, Client>>>);

impl Clients {
    /// Of a sync task that starts again, its client replaces the last.
    pub fn insert(&self, account: &str, client: Client) {
        self.0
            .lock()
            .expect("clients poisoned")
            .insert(account.to_owned(), client);
    }

    pub fn get(&self, account: &str) -> Option<Client> {
        self.0
            .lock()
            .expect("clients poisoned")
            .get(account)
            .cloned()
    }
}

/// The side, in pixels, of the avatar thumbnails asked for: twice what the
/// web client shows, for high-density screens.
const AVATAR_SIZE: u32 = 64;

/// A square thumbnail of the image `mxc`, cached in the client's store.
pub async fn avatar_thumbnail(client: &Client, mxc: &str) -> Result<Vec<u8>> {
    let size = UInt::from(AVATAR_SIZE);
    let request = MediaRequestParameters {
        source: MediaSource::Plain(OwnedMxcUri::from(mxc)),
        format: MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
            Method::Crop,
            size,
            size,
        )),
    };
    client
        .media()
        .get_media_content(&request, true)
        .await
        .with_context(|| format!("fetching a thumbnail of {mxc}"))
}

/// The box, in pixels, that thumbnails of events' media fit in, as
/// Element asks for.
const THUMBNAIL: (u32, u32) = (800, 600);

/// An event's media, or a thumbnail of it (only of media not encrypted:
/// the homeserver cannot see those), cached in the client's store.
pub async fn media(client: &Client, source: MediaSource, thumbnail: bool) -> Result<Vec<u8>> {
    let format = if thumbnail {
        ensure!(
            matches!(source, MediaSource::Plain(_)),
            "the homeserver makes no thumbnails of encrypted media"
        );
        MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
            Method::Scale,
            UInt::from(THUMBNAIL.0),
            UInt::from(THUMBNAIL.1),
        ))
    } else {
        MediaFormat::File
    };
    let what = match &source {
        MediaSource::Plain(mxc) => mxc.to_string(),
        MediaSource::Encrypted(file) => file.url.to_string(),
    };
    client
        .media()
        .get_media_content(&MediaRequestParameters { source, format }, true)
        .await
        .with_context(|| format!("fetching {what}"))
}

/// The avatar of `user` in the room, as the client's store has it.
pub async fn member_avatar(client: &Client, room: &str, user: &str) -> Result<Option<String>> {
    let room_id = <&matrix_sdk::ruma::RoomId>::try_from(room)?;
    let room = client
        .get_room(room_id)
        .with_context(|| format!("room {room} is not in the client"))?;
    let user = <&matrix_sdk::ruma::UserId>::try_from(user)?;
    Ok(room
        .get_member_no_sync(user)
        .await?
        .and_then(|member| member.avatar_url().map(ToString::to_string)))
}

/// Where the account's session is sealed.
pub fn session_path(account: &MatrixAccountConfig) -> PathBuf {
    account.state_dir.join("session.sealed")
}

async fn build(account: &MatrixAccountConfig, key: &MasterKey) -> Result<Client> {
    let passphrase = STANDARD.encode(key.derive(&format!("matrix-store:{}", account.id)));
    Client::builder()
        .homeserver_url(&account.homeserver)
        .sqlite_store(&account.state_dir, Some(&passphrase))
        // Keys missing are looked for in the key backup, if the device has
        // its key (SPEC §9.4).
        .with_encryption_settings(EncryptionSettings {
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            ..EncryptionSettings::default()
        })
        .build()
        .await
        .with_context(|| format!("opening the Matrix client of {}", account.id))
}

/// The account's client, logged in as its paired device.
pub async fn restore(account: &MatrixAccountConfig, key: &MasterKey) -> Result<Client> {
    let path = session_path(account);
    let session: MatrixSession = seal::read(key, &account.id, &path).with_context(|| {
        format!(
            "reading the session of {}; pair it with `uplink matrix pair --account {}`",
            account.id, account.id
        )
    })?;
    check_user(account, &session)?;
    let client = build(account, key).await?;
    // uplink keeps where it synced to (`account.sync_state`), and names the
    // token in every sync. The SDK's own copy would only have it skip the
    // response of a first sync that ends where it last synced to (nothing
    // happened since), taking it for one it had processed: and with it
    // every room, which a first sync records.
    client
        .state_store()
        .remove_kv_data(StateStoreDataKey::SyncToken)
        .await
        .context("forgetting the SDK's sync token")?;
    client
        .matrix_auth()
        .restore_session(session, RoomLoadSettings::default())
        .await
        .with_context(|| format!("restoring the Matrix session of {}", account.id))?;
    Ok(client)
}

/// Pairs the account: logs in with `password`, creating a device named
/// `device_name`, uploads the device's keys, so that others encrypt to it
/// from now on, and seals the session. Run before [`import_secrets`].
///
/// The account must have no state yet: its stores belong to one device. A
/// pairing that fails logs its device out and removes the state it made,
/// so that it can be retried.
pub async fn pair(
    account: &MatrixAccountConfig,
    key: &MasterKey,
    password: &str,
    device_name: &str,
) -> Result<Client> {
    ensure!(
        !account.state_dir.exists(),
        "{} has state in {} already: it is paired, or its pairing was interrupted; \
         to pair it again, delete that first",
        account.id,
        account.state_dir.display()
    );
    let client = build(account, key).await?;
    match set_up(account, key, &client, password, device_name).await {
        Ok(()) => Ok(client),
        Err(e) => match undo(account, client).await {
            Ok(()) => Err(e),
            Err(undo) => Err(e.context(format!(
                "pairing failed, and undoing it failed too ({undo:#})"
            ))),
        },
    }
}

async fn set_up(
    account: &MatrixAccountConfig,
    key: &MasterKey,
    client: &Client,
    password: &str,
    device_name: &str,
) -> Result<()> {
    client
        .matrix_auth()
        .login_username(&account.mxid, password)
        .initial_device_display_name(device_name)
        .await
        .with_context(|| format!("logging in as {}", account.mxid))?;
    let session = client
        .matrix_auth()
        .session()
        .context("logged in, but the client has no session")?;
    check_user(account, &session)?;
    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;
    // A sync sends the outgoing key uploads. One without rooms returns
    // quickly however many the account has; the server's sync task starts
    // afresh, so it leaves nothing unrecorded.
    client
        .sync_once(
            SyncSettings::new()
                .filter(Filter::FilterDefinition(FilterDefinition::ignore_all()))
                .timeout(Duration::ZERO),
        )
        .await
        .context("syncing to upload the device keys")?;
    seal::write(key, &account.id, &session, &session_path(account))
}

/// Logs out the device of a failed pairing, if it got one, and removes
/// the account's state.
async fn undo(account: &MatrixAccountConfig, client: Client) -> Result<()> {
    if client.matrix_auth().logged_in() {
        client.logout().await.context("logging the device out")?;
    }
    drop(client);
    std::fs::remove_dir_all(&account.state_dir)
        .with_context(|| format!("removing {}", account.state_dir.display()))
}

/// Imports the account's cross-signing keys (and key backup key) from
/// Secret Storage, unlocked with `recovery` (the recovery key or
/// passphrase), so that the device signs itself; returns whether the
/// device is now verified.
pub async fn import_secrets(client: &Client, recovery: &str) -> Result<bool> {
    client
        .encryption()
        .recovery()
        .recover(recovery)
        .await
        .context("unlocking Secret Storage")?;
    let device = client
        .encryption()
        .get_own_device()
        .await?
        .context("the client has no device")?;
    Ok(device.is_cross_signed_by_owner())
}

fn check_user(account: &MatrixAccountConfig, session: &MatrixSession) -> Result<()> {
    ensure!(
        session.meta.user_id == account.mxid,
        "the session of {} is of {}, not the configured {}",
        account.id,
        session.meta.user_id,
        account.mxid
    );
    Ok(())
}
