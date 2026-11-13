//! Content-addressed object cache.
//!
//! Objects are stored under `cache/objects/<first two hex chars>/<digest>` and
//! are only ever created from verified content, so a path in the cache can be
//! treated as proof of the digest it is named after.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mats_core::MatsHome;
use mats_core::fsutil;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    pub objects: u64,
    pub bytes: u64,
    pub metas: u64,
    pub java_runtimes: u64,
    pub environments: u64,
}

impl CacheStats {
    pub fn total_bytes(&self) -> u64 {
        self.bytes
    }
}

#[derive(Debug, Clone)]
pub struct ObjectCache {
    objects_dir: PathBuf,
    tmp_dir: PathBuf,
}

impl ObjectCache {
    pub fn new(home: &MatsHome) -> Self {
        ObjectCache {
            objects_dir: home.objects_dir(),
            tmp_dir: home.tmp_dir(),
        }
    }

    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    pub fn object_path(&self, digest: &str) -> Result<PathBuf> {
        let digest = digest.trim().to_ascii_lowercase();
        if digest.len() < 3 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("`{digest}` is not a valid content digest");
        }
        Ok(self.objects_dir.join(&digest[..2]).join(&digest))
    }

    pub fn contains(&self, digest: &str) -> bool {
        self.object_path(digest)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    /// Import a file into the cache, verifying it against an expected digest
    /// when one is known.
    pub fn import(&self, source: &Path, expected_sha1: Option<&str>) -> Result<PathBuf> {
        let actual =
            fsutil::sha1_file(source).with_context(|| format!("hashing {}", source.display()))?;
        if let Some(expected) = expected_sha1 {
            if !actual.eq_ignore_ascii_case(expected) {
                bail!(
                    "SHA-1 mismatch for {}: expected {expected}, got {actual}",
                    source.display()
                );
            }
        }
        let target = self.object_path(&actual)?;
        if target.is_file() {
            return Ok(target);
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        // Move when possible (same filesystem), copy otherwise.
        match fs::rename(source, &target) {
            Ok(()) => {}
            Err(_) => {
                fsutil::atomic_copy(source, &target)?;
            }
        }
        Ok(target)
    }

    /// Store in-memory bytes and return the object path.
    pub fn store_bytes(&self, bytes: &[u8]) -> Result<PathBuf> {
        let digest = fsutil::sha1_bytes(bytes);
        let target = self.object_path(&digest)?;
        if !target.is_file() {
            fsutil::atomic_write(&target, bytes)?;
        }
        Ok(target)
    }

    /// Verify a cached object still matches its digest.
    pub fn verify(&self, digest: &str) -> Result<bool> {
        let path = self.object_path(digest)?;
        if !path.is_file() {
            return Ok(false);
        }
        Ok(fsutil::sha1_file(&path)?.eq_ignore_ascii_case(digest))
    }

    /// Path for an in-progress download, keyed by URL so uploads can resume.
    pub fn partial_path(&self, url: &str) -> Result<PathBuf> {
        let digest = fsutil::sha256_bytes(url.as_bytes());
        let dir = self.tmp_dir.join("downloads");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir.join(format!("{}.part", &digest[..32])))
    }

    /// Read bytes of a cached object.
    pub fn read(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.object_path(digest)?;
        let mut file = File::open(&path)
            .with_context(|| format!("opening cached object {}", path.display()))?;
        let mut buffer = Vec::new();
        file.by_ref()
            .take(256 * 1024 * 1024)
            .read_to_end(&mut buffer)
            .with_context(|| format!("reading cached object {}", path.display()))?;
        Ok(buffer)
    }

    pub fn stats(&self) -> CacheStats {
        let (objects, bytes) = count_tree(&self.objects_dir);
        CacheStats {
            objects,
            bytes,
            ..CacheStats::default()
        }
    }

    /// Delete all cached objects. Returns the number of removed files.
    pub fn clear(&self) -> Result<u64> {
        let (objects, _) = count_tree(&self.objects_dir);
        fsutil::remove_dir_if_exists(&self.objects_dir)?;
        fsutil::remove_dir_if_exists(&self.tmp_dir)?;
        fs::create_dir_all(&self.objects_dir)?;
        Ok(objects)
    }
}

fn count_tree(path: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let Ok(entries) = fs::read_dir(path) else {
        return (files, bytes);
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            let (child_files, child_bytes) = count_tree(&entry.path());
            files += child_files;
            bytes += child_bytes;
        } else if metadata.is_file() {
            files += 1;
            bytes += metadata.len();
        }
    }
    (files, bytes)
}

/// Cache entry for downloaded upstream metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedMeta {
    pub url: String,
    pub fetched_at: i64,
    pub body: serde_json::Value,
}

/// Cache entry for text metadata (Maven XML, checksum manifests).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedText {
    pub url: String,
    pub fetched_at: i64,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_and_dedupes_objects() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        home.ensure_layout().unwrap();
        let cache = ObjectCache::new(&home);
        let source = temp.path().join("a.txt");
        fs::write(&source, b"hello").unwrap();
        let digest = fsutil::sha1_bytes(b"hello");
        let path = cache.import(&source, Some(&digest)).unwrap();
        assert!(path.is_file());
        assert!(cache.contains(&digest));
        // Importing again returns the same object even though the source file
        // has been moved.
        let second = cache.import(&path, Some(&digest)).unwrap();
        assert_eq!(second, path);
        assert_eq!(cache.read(&digest).unwrap(), b"hello");
        assert!(cache.verify(&digest).unwrap());
    }

    #[test]
    fn rejects_checksum_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        home.ensure_layout().unwrap();
        let cache = ObjectCache::new(&home);
        let source = temp.path().join("a.txt");
        fs::write(&source, b"hello").unwrap();
        let error = cache
            .import(&source, Some("0000000000000000000000000000000000000000"))
            .unwrap_err();
        assert!(error.to_string().contains("SHA-1 mismatch"));
        assert!(!cache.contains(&fsutil::sha1_bytes(b"hello")));
    }

    #[test]
    fn partial_paths_are_stable_per_url() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        home.ensure_layout().unwrap();
        let cache = ObjectCache::new(&home);
        let first = cache.partial_path("https://example.com/a.jar").unwrap();
        let second = cache.partial_path("https://example.com/a.jar").unwrap();
        let other = cache.partial_path("https://example.com/b.jar").unwrap();
        assert_eq!(first, second);
        assert_ne!(first, other);
    }

    #[test]
    fn stats_count_objects() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        home.ensure_layout().unwrap();
        let cache = ObjectCache::new(&home);
        cache.store_bytes(b"one").unwrap();
        cache.store_bytes(b"two").unwrap();
        let stats = cache.stats();
        assert_eq!(stats.objects, 2);
        assert_eq!(stats.bytes, 6);
    }

    #[test]
    fn rejects_invalid_digests() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        let cache = ObjectCache::new(&home);
        assert!(cache.object_path("not-a-digest").is_err());
        assert!(cache.object_path("../escape").is_err());
    }
}
