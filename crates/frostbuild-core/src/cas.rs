use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const CAS_REL: &str = ".frost/cas/objects";

#[derive(Debug, Clone)]
pub struct LocalCas {
    root: PathBuf,
    max_bytes: u64,
}

impl LocalCas {
    pub fn new(workspace_root: &Path, max_bytes: u64) -> Self {
        Self {
            root: workspace_root.join(CAS_REL),
            max_bytes,
        }
    }

    fn object(&self, digest: &str) -> PathBuf {
        self.root.join(&digest[..2.min(digest.len())]).join(digest)
    }

    /// Copy an output into the immutable CAS with temp+rename publication.
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
            Ok(()) => {}
            Err(_) if object.exists() => {
                let _ = std::fs::remove_file(&tmp);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to publish {}", object.display()))
            }
        }
        Ok(())
    }

    /// Restore an output using a hardlink where possible, falling back to copy.
    pub fn materialize(&self, digest: &str, destination: &Path) -> Result<bool> {
        let object = self.object(digest);
        if !object.is_file() {
            return Ok(false);
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(destination);
        if std::fs::hard_link(&object, destination).is_err() {
            std::fs::copy(&object, destination)?;
        }
        Ok(true)
    }

    /// Best-effort oldest-first GC. Objects are immutable, so racing readers
    /// either retain an open inode or report a cache miss and rebuild.
    pub fn gc(&self) -> Result<u64> {
        if self.max_bytes == 0 || !self.root.exists() {
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
        Ok(removed)
    }
}
