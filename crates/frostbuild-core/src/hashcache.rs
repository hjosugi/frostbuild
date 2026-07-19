use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Digest recorded for an input path that does not exist on disk. Missing
/// files still participate in action keys so that deleting an input forces a
/// re-run (which then surfaces the real error from the tool).
pub const MISSING: &str = "MISSING";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    mtime_ns: i128,
    size: u64,
    ino: u64,
    hash: String,
}

/// Content-hash cache keyed by workspace-relative (or absolute, for system
/// headers) path, validated by a (mtime, size, inode) stat triple. Avoids
/// re-hashing unchanged files across builds.
#[derive(Debug, Default)]
pub struct HashCache {
    entries: HashMap<String, Entry>,
    dirty: bool,
}

pub const CACHE_REL_PATH: &str = ".frost/hashcache.bin";
/// Pre-0.2 JSON cache location, removed opportunistically on save.
pub const LEGACY_CACHE_REL_PATH: &str = ".frost/hashcache.json";
const CACHE_MAGIC: &[u8; 8] = b"FRSTHC01";

impl HashCache {
    pub fn load(workspace_root: &Path) -> Self {
        let path = workspace_root.join(CACHE_REL_PATH);
        let entries = std::fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() >= 8 && &bytes[..8] == CACHE_MAGIC)
            .and_then(|bytes| postcard::from_bytes(&bytes[8..]).ok())
            .unwrap_or_default();
        Self {
            entries,
            dirty: false,
        }
    }

    pub fn save(&self, workspace_root: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let path = workspace_root.join(CACHE_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("bin.tmp");
        let mut bytes = CACHE_MAGIC.to_vec();
        bytes.extend(postcard::to_allocvec(&self.entries)?);
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("failed to persist {}", path.display()))?;
        let _ = std::fs::remove_file(workspace_root.join(LEGACY_CACHE_REL_PATH));
        Ok(())
    }

    /// Digest for `rel` (workspace-relative, or absolute e.g. a system
    /// header). Returns [`MISSING`] when the file does not exist.
    pub fn digest(&mut self, workspace_root: &Path, rel: &str) -> Result<String> {
        let full = resolve(workspace_root, rel);
        let Ok(meta) = std::fs::metadata(&full) else {
            self.dirty |= self.entries.remove(rel).is_some();
            return Ok(MISSING.to_string());
        };
        let stat = stat_triple(&meta);
        if let Some(entry) = self.entries.get(rel) {
            if (entry.mtime_ns, entry.size, entry.ino) == stat {
                return Ok(entry.hash.clone());
            }
        }
        let hash =
            hash_file(&full).with_context(|| format!("failed to hash {}", full.display()))?;
        self.entries.insert(
            rel.to_string(),
            Entry {
                mtime_ns: stat.0,
                size: stat.1,
                ino: stat.2,
                hash: hash.clone(),
            },
        );
        self.dirty = true;
        Ok(hash)
    }

    /// Resolve a fileset with cached stat checks and hash misses in parallel.
    pub fn digest_many(
        &mut self,
        workspace_root: &Path,
        paths: &[String],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let mut ready = std::collections::BTreeMap::new();
        let mut misses = Vec::new();
        for rel in paths {
            let full = resolve(workspace_root, rel);
            let Ok(meta) = std::fs::metadata(&full) else {
                self.dirty |= self.entries.remove(rel).is_some();
                ready.insert(rel.clone(), MISSING.to_string());
                continue;
            };
            let stat = stat_triple(&meta);
            if let Some(entry) = self.entries.get(rel) {
                if (entry.mtime_ns, entry.size, entry.ino) == stat {
                    ready.insert(rel.clone(), entry.hash.clone());
                    continue;
                }
            }
            misses.push((rel.clone(), full, stat));
        }
        let hashed: Result<Vec<_>> = misses
            .into_par_iter()
            .map(|(rel, full, stat)| {
                hash_file(&full)
                    .with_context(|| format!("failed to hash {}", full.display()))
                    .map(|hash| (rel, stat, hash))
            })
            .collect();
        for (rel, stat, hash) in hashed? {
            self.entries.insert(
                rel.clone(),
                Entry {
                    mtime_ns: stat.0,
                    size: stat.1,
                    ino: stat.2,
                    hash: hash.clone(),
                },
            );
            ready.insert(rel, hash);
            self.dirty = true;
        }
        Ok(ready)
    }

    /// Drop the cached stat for a path we just (re)wrote, forcing a re-hash.
    /// Needed for action outputs: a fast rewrite can land in the same mtime
    /// granule with the same size.
    pub fn invalidate(&mut self, rel: &str) {
        self.dirty |= self.entries.remove(rel).is_some();
    }
}

fn resolve(workspace_root: &Path, rel: &str) -> std::path::PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace_root.join(p)
    }
}

#[cfg(unix)]
fn stat_triple(meta: &std::fs::Metadata) -> (i128, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (
        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        meta.size(),
        meta.ino(),
    )
}

#[cfg(not(unix))]
fn stat_triple(meta: &std::fs::Metadata) -> (i128, u64, u64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    (mtime, meta.len(), 0)
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if n >= 128 * 1024 {
            hasher.update_rayon(&buf[..n]);
        } else {
            hasher.update(&buf[..n]);
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_digests_as_missing() {
        let dir = std::env::temp_dir().join("frost-hashcache-test-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let mut cache = HashCache::default();
        assert_eq!(cache.digest(&dir, "no/such/file").unwrap(), MISSING);
    }

    #[test]
    fn caches_and_detects_content_change() {
        let dir = std::env::temp_dir().join(format!("frost-hashcache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");

        std::fs::write(&file, "one").unwrap();
        let mut cache = HashCache::default();
        let h1 = cache.digest(&dir, "a.txt").unwrap();
        let h1b = cache.digest(&dir, "a.txt").unwrap();
        assert_eq!(h1, h1b);

        std::fs::write(&file, "two-longer").unwrap();
        let h2 = cache.digest(&dir, "a.txt").unwrap();
        assert_ne!(h1, h2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("frost-hashcache-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "content").unwrap();

        let mut cache = HashCache::load(&dir);
        let h = cache.digest(&dir, "a.txt").unwrap();
        cache.save(&dir).unwrap();

        let mut reloaded = HashCache::load(&dir);
        assert_eq!(reloaded.digest(&dir, "a.txt").unwrap(), h);

        std::fs::remove_dir_all(&dir).ok();
    }
}
