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
    ruma::api::client::{filter::FilterDefinition, sync::sync_events::v3::Filter},
    store::RoomLoadSettings,
    Client,
};
use std::{path::PathBuf, time::Duration};
use uplink_config::MatrixAccountConfig;
use uplink_storage::seal::{self, MasterKey};

/// Where the account's session is sealed.
pub fn session_path(account: &MatrixAccountConfig) -> PathBuf {
    account.state_dir.join("session.sealed")
}

async fn build(account: &MatrixAccountConfig, key: &MasterKey) -> Result<Client> {
    let passphrase = STANDARD.encode(key.derive(&format!("matrix-store:{}", account.id)));
    Client::builder()
        .homeserver_url(&account.homeserver)
        .sqlite_store(&account.state_dir, Some(&passphrase))
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
