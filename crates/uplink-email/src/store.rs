//! Content-addressed files on disk, never modified: raw messages (SPEC
//! §4.1, invariant 4), and files uploaded to attach to drafts (§7.4).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

/// Lowercase hex SHA-256 of a file's bytes: a variant's raw message, or
/// a blob.
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

    /// `hash` if it is one: 64 lowercase hex digits.
    pub fn parse(hash: &str) -> Option<Self> {
        (hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(hash.to_owned()))
    }
}

/// A directory under the data root, of `<hash[0:2]>/<hash><extension>`.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    extension: &'static str,
}

impl Store {
    /// Raw messages: `mail/<hash[0:2]>/<hash>.eml`.
    pub fn mail(data_root: &Path) -> Self {
        Self {
            dir: data_root.join("mail"),
            extension: ".eml",
        }
    }

    /// Uploaded files: `blobs/<hash[0:2]>/<hash>`.
    pub fn blobs(data_root: &Path) -> Self {
        Self {
            dir: data_root.join("blobs"),
            extension: "",
        }
    }

    /// Stores `raw` durably, unless it is stored already.
    pub fn put(&self, raw: &[u8]) -> Result<VariantHash> {
        let hash = VariantHash::of(raw);
        let path = self.path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let shard = path.parent().expect("paths have a shard directory");
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

    /// The stored file's size; `None` if there is none.
    pub fn size(&self, hash: &VariantHash) -> Result<Option<u64>> {
        let path = self.path(hash);
        match std::fs::metadata(&path) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn path(&self, hash: &VariantHash) -> PathBuf {
        self.dir
            .join(&hash.0[..2])
            .join(format!("{}{}", hash.0, self.extension))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::mail(dir.path());
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
        assert_eq!(store.size(&hash).unwrap(), Some(5));
        assert_eq!(store.size(&VariantHash::of(b"other")).unwrap(), None);
    }

    #[test]
    fn blobs_have_no_extension() {
        let dir = tempfile::tempdir().unwrap();
        let hash = Store::blobs(dir.path()).put(b"hello").unwrap();
        assert!(dir.path().join("blobs/2c").join(&hash.0).is_file());
    }

    #[test]
    fn only_hashes_parse() {
        let hash = VariantHash::of(b"hello");
        assert_eq!(VariantHash::parse(&hash.0), Some(hash.clone()));
        assert_eq!(VariantHash::parse(&hash.0.to_uppercase()), None);
        assert_eq!(VariantHash::parse(&hash.0[1..]), None);
        assert_eq!(VariantHash::parse("../../../../etc/passwd"), None);
    }
}
