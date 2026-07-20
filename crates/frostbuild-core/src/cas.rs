use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

const CAS_REL: &str = ".frost/cas/objects";
const GC_STAMP_REL: &str = ".frost/cas/gc.stamp";
const GC_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct LocalCas {
    root: PathBuf,
    max_bytes: u64,
    changed: AtomicBool,
    gc_stamp: PathBuf,
}

impl LocalCas {
    pub fn new(workspace_root: &Path, max_bytes: u64) -> Self {
        Self {
            root: workspace_root.join(CAS_REL),
            max_bytes,
            changed: AtomicBool::new(false),
            gc_stamp: workspace_root.join(GC_STAMP_REL),
        }
    }

    fn object(&self, digest: &str) -> PathBuf {
        self.root.join(&digest[..2.min(digest.len())]).join(digest)
    }

    /// Copy an output into the immutable CAS with temp+rename publication.
    ///
    /// The workspace output must not share an inode with the object: an
    /// external in-place write to that output would otherwise mutate a CAS
    /// object whose name still claims the original digest.
    pub fn put(&self, source: &Path, digest: &str) -> Result<()> {
        let object = self.object(digest);
        if object.exists() {
            return Ok(());
        }
        let parent = object.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".{digest}.{}.tmp", std::process::id()));
        std::fs::copy(source, &tmp)
            .with_context(|| format!("failed to cache {}", source.display()))?;
        match std::fs::rename(&tmp, &object) {
            Ok(()) => {
                self.changed.store(true, Ordering::Relaxed);
            }
            Err(_) if object.exists() => {
                let _ = std::fs::remove_file(&tmp);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to publish {}", object.display()))
            }
        }
        Ok(())
    }

    /// Restore an output without sharing the CAS object's inode.
    ///
    /// The object is verified against the digest that names it before it is
    /// published. A content-addressed store whose object no longer hashes to
    /// its own address is corrupt — bit rot, a truncated write, an editor
    /// pointed at the wrong directory — and restoring it would hand back an
    /// artifact that never existed while reporting the build as current. The
    /// bad object is removed and this returns `false`, which the caller reads
    /// as a miss and re-runs the action.
    ///
    /// The cost is one hash, and only on the restore path: an action whose
    /// outputs are already intact never reaches here.
    pub fn materialize(&self, digest: &str, destination: &Path) -> Result<bool> {
        let object = self.object(digest);
        if !object.is_file() {
            return Ok(false);
        }
        match crate::hashcache::hash_file(&object) {
            Ok(actual) if actual == digest => {}
            _ => {
                let _ = std::fs::remove_file(&object);
                return Ok(false);
            }
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(destination);
        std::fs::copy(&object, destination)?;
        Ok(true)
    }

    /// Best-effort oldest-first GC. Objects are immutable, so racing readers
    /// either retain an open inode or report a cache miss and rebuild.
    pub fn gc(&self) -> Result<u64> {
        if self.max_bytes == 0 || !self.root.exists() {
            return Ok(0);
        }
        // A no-op build should not traverse a large object store every time.
        // The bounded stamp still guarantees recovery after a prior process
        // crashed or another writer exceeded the cap: an unchanged process
        // skips only a recent successful scan, never indefinitely.
        if !self.changed.load(Ordering::Relaxed)
            && std::fs::metadata(&self.gc_stamp)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age < GC_INTERVAL)
        {
            return Ok(0);
        }
        let mut objects = Vec::new();
        let mut total = 0u64;
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                let meta = entry.metadata()?;
                if meta.is_file() {
                    total = total.saturating_add(meta.len());
                    objects.push((meta.modified().ok(), meta.len(), entry.path()));
                }
            }
        }
        objects.sort_by_key(|(mtime, _, path)| (*mtime, path.clone()));
        let mut removed = 0;
        for (_, size, path) in objects {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total = total.saturating_sub(size);
                removed += size;
            }
        }
        if let Some(parent) = self.gc_stamp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.gc_stamp, b"frost-cas-gc-v1\n")?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_mutation_cannot_modify_an_immutable_object() {
        let root = std::env::temp_dir().join(format!("frost-cas-immutable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("output");
        let restored = root.join("restored");
        std::fs::write(&source, b"original bytes").unwrap();
        let digest = crate::hashcache::hash_file(&source).unwrap();
        let cas = LocalCas::new(&root, 1024 * 1024);

        cas.put(&source, &digest).unwrap();
        std::fs::write(&source, b"mutated output").unwrap();
        assert!(cas.materialize(&digest, &restored).unwrap());
        assert_eq!(std::fs::read(&restored).unwrap(), b"original bytes");

        std::fs::write(&restored, b"mutated restore").unwrap();
        assert_eq!(
            crate::hashcache::hash_file(&cas.object(&digest)).unwrap(),
            digest,
            "mutating a restored workspace file must not mutate the CAS"
        );
        assert!(cas.materialize(&digest, &restored).unwrap());
        assert_eq!(std::fs::read(&restored).unwrap(), b"original bytes");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn restarted_process_repairs_an_existing_over_budget_store() {
        let root =
            std::env::temp_dir().join(format!("frost-cas-over-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("output");
        std::fs::write(&source, b"more than five bytes").unwrap();
        let digest = crate::hashcache::hash_file(&source).unwrap();
        LocalCas::new(&root, 5).put(&source, &digest).unwrap();

        let restarted = LocalCas::new(&root, 5);
        assert!(
            restarted.gc().unwrap() > 0,
            "a fresh process must not mistake an unscanned store for under-budget"
        );
        assert!(!restarted.object(&digest).exists());

        std::fs::remove_dir_all(root).ok();
    }
}
