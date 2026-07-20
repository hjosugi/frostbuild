use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

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
///
/// One instance covers one build, and treats that build as a single point in
/// time: a path digested once is not stat'd again unless [`Self::invalidate`]
/// says frost rewrote it. Declared outputs always invalidate, so the guarantee
/// holds for every path frost is responsible for; a write frost was never told
/// about is an undeclared side effect, which the build model does not admit
/// and `--sandbox` exists to catch. The next build starts from a fresh
/// instance and re-stats everything.
///
/// Split into an immutable snapshot loaded from disk and the changes made by
/// this build. Every worker reads the snapshot without synchronizing; a lock
/// is taken only once something has actually changed. A no-op build changes
/// nothing, so its stat path — the one that decides whether frost has any
/// work at all — never contends.
#[derive(Debug, Default)]
pub struct HashCache {
    /// Loaded from disk; never mutated.
    snapshot: HashMap<String, Entry>,
    /// Entries this build recomputed. Reads consult it only after
    /// [`Self::changed`] flips, which a no-op build never does.
    updates: RwLock<HashMap<String, Entry>>,
    changed: AtomicBool,
    /// Digests already established during this build, so a path that is both
    /// one action's output and the next action's input is stat'd once rather
    /// than twice. A build is a single point in time: a path is re-stat'd
    /// only after frost itself writes it, which clears the entry.
    settled: RwLock<HashMap<String, String>>,
}

pub const CACHE_REL_PATH: &str = ".frost/hashcache.bin";
/// Pre-0.2 JSON cache location, removed opportunistically on save.
pub const LEGACY_CACHE_REL_PATH: &str = ".frost/hashcache.json";
const CACHE_MAGIC: &[u8; 8] = b"FRSTHC02";

impl HashCache {
    pub fn load(workspace_root: &Path) -> Self {
        let path = workspace_root.join(CACHE_REL_PATH);
        let snapshot = std::fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() >= 8 && &bytes[..8] == CACHE_MAGIC)
            .and_then(|bytes| postcard::from_bytes(&bytes[8..]).ok())
            .unwrap_or_default();
        Self {
            snapshot,
            ..Self::default()
        }
    }

    /// Cached entry for `rel`, newest first. Lock-free until this build has
    /// changed something.
    fn lookup(&self, rel: &str) -> Option<Entry> {
        if self.changed.load(Ordering::Relaxed) {
            if let Some(hit) = self.updates.read().unwrap().get(rel) {
                return Some(hit.clone());
            }
        }
        self.snapshot.get(rel).cloned()
    }

    fn store(&self, rel: &str, entry: Entry) {
        self.updates.write().unwrap().insert(rel.to_string(), entry);
        self.changed.store(true, Ordering::Relaxed);
    }

    /// Digest already established during this build, if any.
    fn settled(&self, rel: &str) -> Option<String> {
        self.settled.read().unwrap().get(rel).cloned()
    }

    fn settle(&self, rel: &str, hash: &str) {
        self.settled
            .write()
            .unwrap()
            .insert(rel.to_string(), hash.to_string());
    }

    fn forget(&self, rel: &str) {
        // A removed path must not fall back to the snapshot, so remember the
        // removal explicitly rather than deleting an entry that only exists
        // in the read-only half.
        self.updates.write().unwrap().insert(
            rel.to_string(),
            Entry {
                mtime_ns: i128::MIN,
                size: 0,
                ino: 0,
                hash: String::new(),
            },
        );
        self.settled.write().unwrap().remove(rel);
        self.changed.store(true, Ordering::Relaxed);
    }

    pub fn save(&self, workspace_root: &Path) -> Result<()> {
        if !self.changed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut merged = self.snapshot.clone();
        for (path, entry) in self.updates.read().unwrap().iter() {
            if entry.hash.is_empty() {
                merged.remove(path);
            } else {
                merged.insert(path.clone(), entry.clone());
            }
        }
        let path = workspace_root.join(CACHE_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("bin.tmp");
        let mut bytes = CACHE_MAGIC.to_vec();
        bytes.extend(postcard::to_allocvec(&merged)?);
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("failed to persist {}", path.display()))?;
        let _ = std::fs::remove_file(workspace_root.join(LEGACY_CACHE_REL_PATH));
        Ok(())
    }

    /// Digest for `rel` (workspace-relative, or absolute e.g. a system
    /// header). Returns [`MISSING`] when the file does not exist.
    pub fn digest(&self, workspace_root: &Path, rel: &str) -> Result<String> {
        if let Some(hit) = self.settled(rel) {
            return Ok(hit);
        }
        let full = resolve(workspace_root, rel);
        let Ok(meta) = std::fs::metadata(&full) else {
            if self.lookup(rel).is_some_and(|e| !e.hash.is_empty()) {
                self.forget(rel);
            }
            return Ok(MISSING.to_string());
        };
        let stat = stat_triple(&meta);
        if let Some(entry) = self.lookup(rel) {
            if !entry.hash.is_empty() && (entry.mtime_ns, entry.size, entry.ino) == stat {
                self.settle(rel, &entry.hash);
                return Ok(entry.hash);
            }
        }
        let hash =
            hash_file(&full).with_context(|| format!("failed to hash {}", full.display()))?;
        self.settle(rel, &hash);
        self.store(
            rel,
            Entry {
                mtime_ns: stat.0,
                size: stat.1,
                ino: stat.2,
                hash: hash.clone(),
            },
        );
        Ok(hash)
    }

    /// Resolve a fileset with cached stat checks and hash misses in parallel.
    pub fn digest_many(
        &self,
        workspace_root: &Path,
        paths: &[String],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let mut ready = std::collections::BTreeMap::new();
        let mut misses = Vec::new();
        for rel in paths {
            if let Some(hit) = self.settled(rel) {
                ready.insert(rel.clone(), hit);
                continue;
            }
            let full = resolve(workspace_root, rel);
            let Ok(meta) = std::fs::metadata(&full) else {
                if self.lookup(rel).is_some_and(|e| !e.hash.is_empty()) {
                    self.forget(rel);
                }
                ready.insert(rel.clone(), MISSING.to_string());
                continue;
            };
            let stat = stat_triple(&meta);
            if let Some(entry) = self.lookup(rel) {
                if !entry.hash.is_empty() && (entry.mtime_ns, entry.size, entry.ino) == stat {
                    self.settle(rel, &entry.hash);
                    ready.insert(rel.clone(), entry.hash);
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
            self.settle(&rel, &hash);
            self.store(
                &rel,
                Entry {
                    mtime_ns: stat.0,
                    size: stat.1,
                    ino: stat.2,
                    hash: hash.clone(),
                },
            );
            ready.insert(rel, hash);
        }
        Ok(ready)
    }

    /// Drop the cached stat for a path we just (re)wrote, forcing a re-hash.
    /// Needed for action outputs: a fast rewrite can land in the same mtime
    /// granule with the same size.
    pub fn invalidate(&self, rel: &str) {
        self.forget(rel);
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
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
        // chmod updates ctime, not mtime, so a mode change would otherwise
        // reuse the cached digest and never reach the hash above.
        meta.size() ^ ((meta.mode() as u64 & 0o111) << 40),
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

/// Content digest of a file, including whether it is executable.
///
/// The mode is part of the digest rather than a separate field because it is
/// part of what the file *is*: `chmod -x` on a script a genrule runs leaves
/// the bytes untouched, so a content-only digest reports the build as current
/// while a clean build of the same tree fails. Mixing one bit into the hash
/// costs nothing and closes that gap; the CAS then stores the two modes as
/// distinct objects, which is also what restoring them correctly requires.
pub fn hash_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(if is_executable(&file.metadata()?) {
        b"x"
    } else {
        b"-"
    });
    let mut file = file;
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
        let cache = HashCache::default();
        assert_eq!(cache.digest(&dir, "no/such/file").unwrap(), MISSING);
    }

    #[test]
    fn caches_and_detects_content_change() {
        let dir = std::env::temp_dir().join(format!("frost-hashcache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");

        std::fs::write(&file, "one").unwrap();
        let cache = HashCache::default();
        let h1 = cache.digest(&dir, "a.txt").unwrap();
        assert_eq!(cache.digest(&dir, "a.txt").unwrap(), h1, "repeat is stable");

        // A cache instance covers one build, and a build is one point in
        // time: a path already digested is not re-stat'd. This is what makes
        // a file that is one action's output and the next action's input cost
        // a single stat instead of two.
        std::fs::write(&file, "two-longer").unwrap();
        assert_eq!(
            cache.digest(&dir, "a.txt").unwrap(),
            h1,
            "a write frost did not make is not observed mid-build"
        );

        // Whenever frost writes a path it says so, and the next digest is
        // fresh. Every engine path that produces outputs calls this.
        cache.invalidate("a.txt");
        let h2 = cache.digest(&dir, "a.txt").unwrap();
        assert_ne!(h1, h2, "invalidate restores freshness");

        // A new build sees the current content with no invalidation needed.
        assert_eq!(HashCache::default().digest(&dir, "a.txt").unwrap(), h2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("frost-hashcache-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "content").unwrap();

        let cache = HashCache::load(&dir);
        let h = cache.digest(&dir, "a.txt").unwrap();
        cache.save(&dir).unwrap();

        let reloaded = HashCache::load(&dir);
        assert_eq!(reloaded.digest(&dir, "a.txt").unwrap(), h);

        std::fs::remove_dir_all(&dir).ok();
    }
}
