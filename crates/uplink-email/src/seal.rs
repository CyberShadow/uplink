//! OAuth refresh tokens sealed at rest with the master key (SPEC §13.4).

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use serde::{Deserialize, Serialize};
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
    pub fn generate() -> String {
        let mut key = [0u8; KEY_BYTES];
        rand::fill(&mut key);
        STANDARD.encode(key)
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

/// What a token file holds, once unsealed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub refresh_token: String,
}

/// Writes `token` to `path`, sealed for `account`: `nonce || ciphertext`.
/// The account id is authenticated, so a token file only opens for the
/// account it was written for. Replaces the file atomically.
pub fn write_token(key: &MasterKey, account: &str, token: &Token, path: &Path) -> Result<()> {
    let mut nonce = [0u8; NONCE_BYTES];
    rand::fill(&mut nonce);
    let plaintext = serde_json::to_vec(token)?;
    let ciphertext = ChaCha20Poly1305::new(&key.0)
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &plaintext,
                aad: account.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("sealing the token failed"))?;
    let dir = path
        .parent()
        .with_context(|| format!("token file {} has no directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // NamedTempFile is created with mode 0600.
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&nonce)?;
    file.write_all(&ciphertext)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("writing token file {}", path.display()))?;
    Ok(())
}

pub fn read_token(key: &MasterKey, account: &str, path: &Path) -> Result<Token> {
    let sealed =
        std::fs::read(path).with_context(|| format!("reading token file {}", path.display()))?;
    ensure!(
        sealed.len() > NONCE_BYTES,
        "token file {} is truncated",
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
            "token file {} does not open with this master key for account {account}",
            path.display()
        );
    };
    serde_json::from_slice(&plaintext)
        .with_context(|| format!("token file {} is malformed", path.display()))
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
        let token = Token {
            refresh_token: "1//secret".into(),
        };
        write_token(&key, "a", &token, &path).unwrap();
        assert_eq!(read_token(&key, "a", &path).unwrap(), token);
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
    fn opens_only_with_its_key_and_account() {
        let dir = tempfile::tempdir().unwrap();
        let key1 = key(dir.path(), "1.key");
        let key2 = key(dir.path(), "2.key");
        let path = dir.path().join("a.token");
        let token = Token {
            refresh_token: "t".into(),
        };
        write_token(&key1, "a", &token, &path).unwrap();
        assert!(read_token(&key2, "a", &path).is_err());
        assert!(read_token(&key1, "b", &path).is_err());
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
