//! The account's device and its encryption (SPEC §9.4): whether the
//! device is verified, its key backup and recovery, the account's other
//! devices; recovering with the recovery key, and importing and exporting
//! room keys.

use anyhow::{Context, Result};
use matrix_sdk::{
    encryption::{backups::BackupState, recovery::RecoveryState},
    Client,
};
use std::io::Write;
use uplink_protocol::matrix::{KeyBackup, MatrixDevice, MatrixSecurity, Recovery};

/// The account's security, as its client has it now.
pub async fn security(account: &str, client: &Client) -> Result<MatrixSecurity> {
    let encryption = client.encryption();
    let own_user = client.user_id().context("the client has no user")?;
    let own_device = client.device_id().context("the client has no device")?;
    let cross_signing = encryption
        .cross_signing_status()
        .await
        .context("the client has no crypto store")?;
    let trusted = encryption.get_user_devices(own_user).await?;
    let mut devices: Vec<MatrixDevice> = client
        .devices()
        .await
        .context("listing the account's devices")?
        .devices
        .into_iter()
        .map(|device| MatrixDevice {
            verified: trusted
                .get(&device.device_id)
                .is_some_and(|d| d.is_cross_signed_by_owner()),
            current: device.device_id == own_device,
            device_id: device.device_id.to_string(),
            name: device.display_name,
            last_seen_ts: device.last_seen_ts.map(|ts| i64::from(ts.0)),
            last_seen_ip: device.last_seen_ip,
        })
        .collect();
    devices.sort_by_key(|d| std::cmp::Reverse(d.last_seen_ts));
    Ok(MatrixSecurity {
        account: account.to_owned(),
        device_id: own_device.to_string(),
        verified: devices.iter().any(|d| d.current && d.verified),
        cross_signing: cross_signing.is_complete(),
        backup: match encryption.backups().state() {
            BackupState::Unknown => KeyBackup::Unknown,
            BackupState::Creating => KeyBackup::Creating,
            BackupState::Enabling => KeyBackup::Enabling,
            BackupState::Resuming => KeyBackup::Resuming,
            BackupState::Enabled => KeyBackup::Enabled,
            BackupState::Downloading => KeyBackup::Downloading,
            BackupState::Disabling => KeyBackup::Disabling,
        },
        recovery: match encryption.recovery().state() {
            RecoveryState::Unknown => Recovery::Unknown,
            RecoveryState::Enabled => Recovery::Enabled,
            RecoveryState::Disabled => Recovery::Disabled,
            RecoveryState::Incomplete => Recovery::Incomplete,
        },
        devices,
    })
}

/// Imports room keys from `export`, as clients export them, encrypted
/// with `passphrase`; returns how many were new, of how many.
pub async fn import_room_keys(
    client: &Client,
    export: &[u8],
    passphrase: &str,
) -> Result<(u64, u64)> {
    // The SDK reads exports from files only.
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(export)?;
    let imported = client
        .encryption()
        .import_room_keys(file.path().to_owned(), passphrase)
        .await
        .context("importing the room keys")?;
    Ok((
        imported.imported_count.try_into()?,
        imported.total_count.try_into()?,
    ))
}

/// The account's room keys, as uplink's device has them, exported
/// encrypted with `passphrase`, as other clients import them.
pub async fn export_room_keys(client: &Client, passphrase: &str) -> Result<Vec<u8>> {
    // The SDK writes exports to files only.
    let file = tempfile::NamedTempFile::new()?;
    client
        .encryption()
        .export_room_keys(file.path().to_owned(), passphrase, |_| true)
        .await
        .context("exporting the room keys")?;
    Ok(std::fs::read(file.path())?)
}
