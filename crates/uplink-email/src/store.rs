//! Raw messages on disk, content-addressed and never modified (SPEC §4.1,
//! invariant 4).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

/// Lowercase hex SHA-256 of a variant's raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VariantHash(pub String);

impl VariantHash {
    pub fn of(raw: &[u8]) -> Self {
        Self(
            Sha256::digest(raw)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        )
    }
}

/// `mail/` under the data root: `mail/<hash[0:2]>/<hash>.eml`.
#[derive(Debug, Clone)]
pub struct MailStore {
    dir: PathBuf,
}

impl MailStore {
    pub fn new(data_root: &Path) -> Self {
        Self {
            dir: data_root.join("mail"),
        }
    }

    /// Stores `raw` durably, unless it is stored already.
    pub fn put(&self, raw: &[u8]) -> Result<VariantHash> {
        let hash = VariantHash::of(raw);
        let path = self.path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let shard = path.parent().expect("variant paths have a shard directory");
        std::fs::create_dir_all(shard).with_context(|| format!("creating {}", shard.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(shard)
            .with_context(|| format!("creating a temporary file in {}", shard.display()))?;
        file.write_all(raw)?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(&path) {
            Ok(_) => {}
            // Stored concurrently; the content is the same.
            Err(e) if e.error.kind() == ErrorKind::AlreadyExists => return Ok(hash),
            Err(e) => return Err(e.error).with_context(|| format!("storing {}", path.display())),
        }
        File::open(shard)?.sync_all()?;
        Ok(hash)
    }

    pub fn get(&self, hash: &VariantHash) -> Result<Vec<u8>> {
        let path = self.path(hash);
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
    }

    fn path(&self, hash: &VariantHash) -> PathBuf {
        self.dir.join(&hash.0[..2]).join(format!("{}.eml", hash.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = MailStore::new(dir.path());
        let hash = store.put(b"hello").unwrap();
        assert_eq!(
            hash.0,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(dir
            .path()
            .join("mail/2c")
            .join(format!("{}.eml", hash.0))
            .is_file());
        assert_eq!(store.get(&hash).unwrap(), b"hello");
        // Idempotent.
        assert_eq!(store.put(b"hello").unwrap(), hash);
        assert_eq!(
            std::fs::read_dir(dir.path().join("mail/2c"))
                .unwrap()
                .count(),
            1
        );
    }
}
