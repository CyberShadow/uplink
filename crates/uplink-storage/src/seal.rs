//! Secrets sealed at rest with the master key (SPEC §13.4): OAuth refresh
//! tokens, Matrix sessions, and keys derived for stores that encrypt
//! themselves.

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

/// The master key: 32 random bytes, stored base64-encoded.
pub struct MasterKey(Key);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

impl MasterKey {
    /// A new key, in the form [`MasterKey::load`] reads.
    fn generate() -> String {
        let mut key = [0u8; KEY_BYTES];
        rand::fill(&mut key);
        STANDARD.encode(key)
    }

    /// The key at `path`, made there first if there is none: readable by
    /// its owner only, and never replacing one made meanwhile.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            let dir = path
                .parent()
                .with_context(|| format!("master key {} has no directory", path.display()))?;
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            // NamedTempFile is created with mode 0600.
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            file.write_all(Self::generate().as_bytes())?;
            file.as_file().sync_all()?;
            match file.persist_noclobber(path) {
                Ok(_) => {}
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(e.error)
                        .with_context(|| format!("writing master key {}", path.display()))
                }
            }
        }
        Self::load(path)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading master key {}", path.display()))?;
        let bytes = STANDARD
            .decode(text.trim())
            .with_context(|| format!("master key {} is not base64", path.display()))?;
        let key: [u8; KEY_BYTES] = bytes.try_into().map_err(|b: Vec<u8>| {
            anyhow::anyhow!(
                "master key {} has {} bytes, not {KEY_BYTES}",
                path.display(),
                b.len()
            )
        })?;
        Ok(Self(key.into()))
    }
}

impl MasterKey {
    /// A key for `purpose` (e.g. a store's passphrase), derived from the
    /// master key, so that it needs no storing of its own.
    pub fn derive(&self, purpose: &str) -> [u8; KEY_BYTES] {
        Sha256::new()
            .chain_update(b"uplink key derivation\0")
            .chain_update(self.0)
            .chain_update(purpose.as_bytes())
            .finalize()
            .into()
    }
}

/// Writes `secret` to `path`, sealed for `account`: `nonce || ciphertext`.
/// The account id is authenticated, so a file only opens for the account
/// it was written for. Replaces the file atomically, with mode 0600.
pub fn write<T: Serialize>(key: &MasterKey, account: &str, secret: &T, path: &Path) -> Result<()> {
    let mut nonce = [0u8; NONCE_BYTES];
    rand::fill(&mut nonce);
    let plaintext = serde_json::to_vec(secret)?;
    let ciphertext = ChaCha20Poly1305::new(&key.0)
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &plaintext,
                aad: account.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("sealing failed"))?;
    let dir = path
        .parent()
        .with_context(|| format!("sealed file {} has no directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // NamedTempFile is created with mode 0600.
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&nonce)?;
    file.write_all(&ciphertext)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("writing sealed file {}", path.display()))?;
    Ok(())
}

pub fn read<T: DeserializeOwned>(key: &MasterKey, account: &str, path: &Path) -> Result<T> {
    let sealed =
        std::fs::read(path).with_context(|| format!("reading sealed file {}", path.display()))?;
    ensure!(
        sealed.len() > NONCE_BYTES,
        "sealed file {} is truncated",
        path.display()
    );
    let (nonce, ciphertext) = sealed.split_at(NONCE_BYTES);
    let nonce: [u8; NONCE_BYTES] = nonce.try_into().expect("split at the nonce length");
    let Ok(plaintext) = ChaCha20Poly1305::new(&key.0).decrypt(
        &Nonce::from(nonce),
        Payload {
            msg: ciphertext,
            aad: account.as_bytes(),
        },
    ) else {
        bail!(
            "{} does not open with this master key for account {account}",
            path.display()
        );
    };
    serde_json::from_slice(&plaintext)
        .with_context(|| format!("sealed file {} is malformed", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(dir: &Path, name: &str) -> MasterKey {
        let path = dir.join(name);
        std::fs::write(&path, MasterKey::generate() + "\n").unwrap();
        MasterKey::load(&path).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = key(dir.path(), "master.key");
        let path = dir.path().join("accounts/a.token");
        let token = "1//secret".to_owned();
        write(&key, "a", &token, &path).unwrap();
        assert_eq!(read::<String>(&key, "a", &path).unwrap(), token);
        assert!(!std::fs::read(&path)
            .unwrap()
            .windows(6)
            .any(|w| w == b"secret"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_master_key_is_made_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data/master.key");
        let made = MasterKey::open(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let token = dir.path().join("a.token");
        write(&made, "a", &"t", &token).unwrap();
        // Opened again, it is the same key.
        let opened = MasterKey::open(&path).unwrap();
        assert_eq!(read::<String>(&opened, "a", &token).unwrap(), "t");
    }

    #[test]
    fn opens_only_with_its_key_and_account() {
        let dir = tempfile::tempdir().unwrap();
        let key1 = key(dir.path(), "1.key");
        let key2 = key(dir.path(), "2.key");
        let path = dir.path().join("a.token");
        write(&key1, "a", &"t", &path).unwrap();
        assert!(read::<String>(&key2, "a", &path).is_err());
        assert!(read::<String>(&key1, "b", &path).is_err());
    }

    #[test]
    fn derived_keys_differ_by_purpose_and_master_key() {
        let dir = tempfile::tempdir().unwrap();
        let (key1, key2) = (key(dir.path(), "1.key"), key(dir.path(), "2.key"));
        assert_eq!(key1.derive("a"), key1.derive("a"));
        assert_ne!(key1.derive("a"), key1.derive("b"));
        assert_ne!(key1.derive("a"), key2.derive("a"));
    }

    #[test]
    fn rejects_malformed_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        std::fs::write(&path, STANDARD.encode([0u8; 16])).unwrap();
        assert!(MasterKey::load(&path)
            .unwrap_err()
            .to_string()
            .contains("16 bytes"));
        std::fs::write(&path, "not base64!").unwrap();
        assert!(MasterKey::load(&path).is_err());
    }
}
