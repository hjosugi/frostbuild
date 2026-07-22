use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use memmap2::MmapOptions;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fastcdc::{FastCdc, DEFAULT_AVG, DEFAULT_MAX, DEFAULT_MIN, DEFAULT_NORMALIZATION};

const CAS_REL: &str = ".frost/cas/objects";
const CHUNKS_REL: &str = ".frost/cas/chunks";
const MANIFESTS_REL: &str = ".frost/cas/manifests";
const DELTAS_REL: &str = ".frost/cas/deltas";
const ARTIFACTS_REL: &str = ".frost/cas/artifacts";
const GC_STAMP_REL: &str = ".frost/cas/gc.stamp";
const GC_INTERVAL: Duration = Duration::from_secs(60);
/// Matching production CDC systems avoid paying split/manifest overhead for
/// the overwhelmingly common small-object tail.
pub const CHUNKING_THRESHOLD: u64 = DEFAULT_MAX as u64;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

const CHUNK_MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeltaRef {
    base_sha256: String,
    patch_sha256: String,
    patch_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkRef {
    sha256: String,
    length: u32,
    delta: Option<DeltaRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkManifest {
    version: u32,
    blob_digest: String,
    total_bytes: u64,
    executable: bool,
    average: u32,
    minimum: u32,
    maximum: u32,
    normalization: u8,
    seed: u64,
    chunks: Vec<ChunkRef>,
}

/// Bytes arriving from disk/network are never eligible for CAS publication
/// until their expected digest has promoted them to `VerifiedBytes`.
struct UnverifiedBytes<'a>(&'a [u8]);
struct VerifiedBytes<'a>(&'a [u8]);

impl<'a> UnverifiedBytes<'a> {
    fn verify_sha256(self, expected: &str) -> Option<VerifiedBytes<'a>> {
        (sha256(self.0) == expected).then_some(VerifiedBytes(self.0))
    }

    fn digest_sha256(self) -> (String, VerifiedBytes<'a>) {
        (sha256(self.0), VerifiedBytes(self.0))
    }
}

/// A staged complete blob whose BLAKE3+executable digest was checked. Only
/// this type can cross the final materialization/publication boundary.
struct VerifiedBlob {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct CasStats {
    pub object_count: u64,
    pub object_bytes: u64,
    pub chunk_count: u64,
    pub chunk_bytes: u64,
    pub delta_count: u64,
    pub delta_bytes: u64,
    pub manifest_count: u64,
    pub logical_chunk_count: u64,
    pub logical_chunk_bytes: u64,
    pub reused_chunk_count: u64,
    pub reused_chunk_bytes: u64,
    pub chunk_reuse_ratio: f64,
}

#[derive(Debug)]
pub struct LocalCas {
    root: PathBuf,
    chunks: PathBuf,
    manifests: PathBuf,
    deltas: PathBuf,
    artifacts: PathBuf,
    max_bytes: u64,
    parallel_chunks: bool,
    changed: AtomicBool,
    gc_stamp: PathBuf,
}

impl LocalCas {
    pub fn new(workspace_root: &Path, max_bytes: u64) -> Self {
        Self {
            root: workspace_root.join(CAS_REL),
            chunks: workspace_root.join(CHUNKS_REL),
            manifests: workspace_root.join(MANIFESTS_REL),
            deltas: workspace_root.join(DELTAS_REL),
            artifacts: workspace_root.join(ARTIFACTS_REL),
            max_bytes,
            parallel_chunks: true,
            changed: AtomicBool::new(false),
            gc_stamp: workspace_root.join(GC_STAMP_REL),
        }
    }

    /// Select serial chunk preparation for controlled A/B measurements or
    /// exceptionally memory-constrained hosts. Production defaults to the
    /// bounded global Rayon pool; content and manifest order are identical.
    pub fn with_parallel_chunks(mut self, enabled: bool) -> Self {
        self.parallel_chunks = enabled;
        self
    }

    fn object(&self, digest: &str) -> PathBuf {
        self.root.join(&digest[..2.min(digest.len())]).join(digest)
    }

    fn chunk(&self, digest: &str) -> PathBuf {
        self.chunks
            .join(&digest[..2.min(digest.len())])
            .join(digest)
    }

    fn manifest(&self, digest: &str) -> PathBuf {
        self.manifests
            .join(&digest[..2.min(digest.len())])
            .join(digest)
    }

    fn delta(&self, target_digest: &str, patch_digest: &str) -> PathBuf {
        self.deltas
            .join(&target_digest[..2.min(target_digest.len())])
            .join(format!("{target_digest}-{patch_digest}"))
    }

    fn artifact_head(&self, source: &Path) -> PathBuf {
        let key = blake3::hash(source.to_string_lossy().as_bytes())
            .to_hex()
            .to_string();
        self.artifacts.join(&key[..2]).join(key)
    }

    fn unique_temp(parent: &Path, label: &str) -> PathBuf {
        let temp_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        parent.join(format!(".{label}.{}.{temp_id}.tmp", std::process::id()))
    }

    /// Copy an output into the immutable CAS with temp+rename publication.
    ///
    /// The workspace output must not share an inode with the object: an
    /// external in-place write to that output would otherwise mutate a CAS
    /// object whose name still claims the original digest.
    pub fn put(&self, source: &Path, digest: &str) -> Result<()> {
        let object = self.object(digest);
        let parent = object.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        if !object.is_file() {
            // More than one executor thread/action may publish the same
            // digest. Every writer owns a distinct staging file, verifies the
            // staged bytes, then races only at immutable rename publication.
            let tmp = Self::unique_temp(parent, digest);
            std::fs::copy(source, &tmp)
                .with_context(|| format!("failed to cache {}", source.display()))?;
            let Some(verified) = verify_staged_blob(tmp, digest)? else {
                anyhow::bail!(
                    "output {} changed while entering CAS or had digest {digest:?} incorrectly assigned",
                    source.display()
                );
            };
            match publish_verified_blob(verified, &object) {
                Ok(true) => {
                    self.changed.store(true, Ordering::Relaxed);
                }
                Ok(false) => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to publish {}", object.display()))
                }
            }
        }
        let metadata = std::fs::metadata(&object)?;
        if metadata.len() > CHUNKING_THRESHOLD && !self.manifest(digest).is_file() {
            self.put_chunks(source, &object, digest, &metadata)?;
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
    /// The exact-object path costs one final hash. Chunk/delta fallback also
    /// verifies each component before the final blob hash. Actions whose
    /// outputs are already intact never reach either restore path.
    pub fn materialize(&self, digest: &str, destination: &Path) -> Result<bool> {
        let object = self.object(digest);
        if object.is_file() {
            match self.copy_and_verify(&object, digest, destination)? {
                true => return Ok(true),
                false => {
                    let _ = std::fs::remove_file(&object);
                }
            }
        }
        self.materialize_chunks(digest, destination)
    }

    fn copy_and_verify(&self, source: &Path, digest: &str, destination: &Path) -> Result<bool> {
        let Some(parent) = destination.parent() else {
            anyhow::bail!(
                "materialization destination has no parent: {}",
                destination.display()
            );
        };
        std::fs::create_dir_all(parent)?;
        let tmp = Self::unique_temp(parent, "materialize");
        std::fs::copy(source, &tmp)?;
        let Some(verified) = verify_staged_blob(tmp, digest)? else {
            return Ok(false);
        };
        publish_verified_destination(verified, destination)?;
        Ok(true)
    }

    fn put_chunks(
        &self,
        source: &Path,
        object: &Path,
        blob_digest: &str,
        metadata: &std::fs::Metadata,
    ) -> Result<()> {
        let file = std::fs::File::open(object)?;
        // SAFETY: the CAS object is immutable by contract and remains open for
        // the mapping lifetime. Any external violation is still caught by
        // per-chunk and final blob digest checks during materialization.
        let bytes = unsafe { MmapOptions::new().map(&file)? };
        let previous = self.previous_manifest(source);
        let chunker = FastCdc::default();
        // Boundaries are inherently ordered, but every resulting chunk has
        // independent digest, delta and immutable-publication work. Rayon on
        // this indexed vector retains manifest order while using the same
        // bounded worker pool as hashing and action preparation.
        let boundaries = chunker.chunks(&bytes).collect::<Vec<_>>();
        let prepare =
            |(offset, length)| self.prepare_chunk(&bytes, previous.as_ref(), offset, length);
        let chunks = if self.parallel_chunks {
            boundaries
                .into_par_iter()
                .map(prepare)
                .collect::<Result<Vec<_>>>()?
        } else {
            boundaries
                .into_iter()
                .map(prepare)
                .collect::<Result<Vec<_>>>()?
        };
        let manifest = ChunkManifest {
            version: CHUNK_MANIFEST_VERSION,
            blob_digest: blob_digest.to_string(),
            total_bytes: metadata.len(),
            executable: executable(metadata),
            average: DEFAULT_AVG as u32,
            minimum: DEFAULT_MIN as u32,
            maximum: DEFAULT_MAX as u32,
            normalization: DEFAULT_NORMALIZATION as u8,
            seed: 0,
            chunks,
        };
        self.publish_manifest(&manifest)?;
        self.publish_artifact_head(source, blob_digest)
    }

    fn prepare_chunk(
        &self,
        bytes: &[u8],
        previous: Option<&ChunkManifest>,
        offset: usize,
        length: usize,
    ) -> Result<ChunkRef> {
        let (chunk_digest, verified) =
            UnverifiedBytes(&bytes[offset..offset + length]).digest_sha256();
        let existed = self.chunk(&chunk_digest).is_file();
        let delta = if existed {
            None
        } else {
            self.build_positional_delta(
                previous,
                offset as u64,
                (offset + length) as u64,
                &chunk_digest,
                verified.0,
            )?
        };
        self.publish_chunk(&chunk_digest, verified)?;
        Ok(ChunkRef {
            sha256: chunk_digest,
            length: u32::try_from(length).context("FastCDC chunk exceeds u32")?,
            delta,
        })
    }

    fn publish_chunk(&self, digest: &str, bytes: VerifiedBytes<'_>) -> Result<()> {
        let destination = self.chunk(digest);
        if destination.is_file() {
            return Ok(());
        }
        let parent = destination.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let tmp = Self::unique_temp(parent, digest);
        std::fs::write(&tmp, bytes.0)?;
        match std::fs::rename(&tmp, &destination) {
            Ok(()) => self.changed.store(true, Ordering::Relaxed),
            Err(_) if destination.is_file() => {
                let _ = std::fs::remove_file(tmp);
            }
            Err(error) => return Err(error).context("failed to publish CAS chunk"),
        }
        Ok(())
    }

    fn publish_manifest(&self, manifest: &ChunkManifest) -> Result<()> {
        let destination = self.manifest(&manifest.blob_digest);
        if destination.is_file() {
            return Ok(());
        }
        let parent = destination.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let tmp = Self::unique_temp(parent, &manifest.blob_digest);
        std::fs::write(&tmp, postcard::to_allocvec(manifest)?)?;
        match std::fs::rename(&tmp, &destination) {
            Ok(()) => self.changed.store(true, Ordering::Relaxed),
            Err(_) if destination.is_file() => {
                let _ = std::fs::remove_file(tmp);
            }
            Err(error) => return Err(error).context("failed to publish chunk manifest"),
        }
        Ok(())
    }

    fn previous_manifest(&self, source: &Path) -> Option<ChunkManifest> {
        let digest = std::fs::read_to_string(self.artifact_head(source)).ok()?;
        let digest = digest.trim();
        let bytes = std::fs::read(self.manifest(digest)).ok()?;
        let manifest = postcard::from_bytes::<ChunkManifest>(&bytes).ok()?;
        valid_manifest(&manifest, digest).then_some(manifest)
    }

    fn publish_artifact_head(&self, source: &Path, digest: &str) -> Result<()> {
        let destination = self.artifact_head(source);
        let parent = destination.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let tmp = Self::unique_temp(parent, "artifact-head");
        std::fs::write(&tmp, digest.as_bytes())?;
        let _ = std::fs::remove_file(&destination);
        std::fs::rename(tmp, destination)?;
        Ok(())
    }

    fn build_positional_delta(
        &self,
        previous: Option<&ChunkManifest>,
        target_start: u64,
        target_end: u64,
        target_digest: &str,
        target: &[u8],
    ) -> Result<Option<DeltaRef>> {
        let Some(previous) = previous else {
            return Ok(None);
        };
        let Some(base) = positional_base(previous, target_start, target_end) else {
            return Ok(None);
        };
        let base_path = self.chunk(&base.sha256);
        let Ok(base_bytes) = std::fs::read(&base_path) else {
            return Ok(None);
        };
        let Some(base_bytes) = UnverifiedBytes(&base_bytes).verify_sha256(&base.sha256) else {
            let _ = std::fs::remove_file(base_path);
            return Ok(None);
        };
        let patch = zstd_delta(base_bytes.0, target)?;
        let full = zstd::bulk::compress(target, 3)?;
        if patch.len() >= full.len() {
            return Ok(None);
        }
        let reconstructed = zstd_undelta(base_bytes.0, &patch, target.len())?;
        if UnverifiedBytes(&reconstructed)
            .verify_sha256(target_digest)
            .is_none()
        {
            return Ok(None);
        }
        let (patch_digest, patch) = UnverifiedBytes(&patch).digest_sha256();
        let patch_bytes = u32::try_from(patch.0.len()).context("delta patch exceeds u32")?;
        self.publish_delta(target_digest, &patch_digest, patch)?;
        Ok(Some(DeltaRef {
            base_sha256: base.sha256.clone(),
            patch_sha256: patch_digest,
            patch_bytes,
        }))
    }

    fn publish_delta(
        &self,
        target_digest: &str,
        patch_digest: &str,
        patch: VerifiedBytes<'_>,
    ) -> Result<()> {
        let destination = self.delta(target_digest, patch_digest);
        if destination.is_file() {
            return Ok(());
        }
        let parent = destination.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let tmp = Self::unique_temp(parent, patch_digest);
        std::fs::write(&tmp, patch.0)?;
        match std::fs::rename(&tmp, &destination) {
            Ok(()) => self.changed.store(true, Ordering::Relaxed),
            Err(_) if destination.is_file() => {
                let _ = std::fs::remove_file(tmp);
            }
            Err(error) => return Err(error).context("failed to publish delta patch"),
        }
        Ok(())
    }

    fn materialize_chunks(&self, digest: &str, destination: &Path) -> Result<bool> {
        let manifest_path = self.manifest(digest);
        let Ok(encoded) = std::fs::read(&manifest_path) else {
            return Ok(false);
        };
        let Ok(manifest) = postcard::from_bytes::<ChunkManifest>(&encoded) else {
            let _ = std::fs::remove_file(manifest_path);
            return Ok(false);
        };
        if !valid_manifest(&manifest, digest) {
            let _ = std::fs::remove_file(manifest_path);
            return Ok(false);
        }
        let Some(parent) = destination.parent() else {
            anyhow::bail!(
                "materialization destination has no parent: {}",
                destination.display()
            );
        };
        std::fs::create_dir_all(parent)?;
        let tmp = Self::unique_temp(parent, "splice");
        let result = (|| -> Result<bool> {
            let mut output = std::fs::File::create(&tmp)?;
            output.set_len(manifest.total_bytes)?;
            let mut offset = 0u64;
            let placements = manifest
                .chunks
                .iter()
                .map(|chunk| {
                    let placement = (offset, chunk);
                    offset += u64::from(chunk.length);
                    placement
                })
                .collect::<Vec<_>>();
            let write_one = |(offset, chunk)| self.materialize_chunk_at(&output, offset, chunk);
            let verified = if self.parallel_chunks {
                placements
                    .into_par_iter()
                    .map(write_one)
                    .collect::<Result<Vec<_>>>()?
            } else {
                placements
                    .into_iter()
                    .map(write_one)
                    .collect::<Result<Vec<_>>>()?
            };
            if verified.iter().any(|ready| !ready) {
                return Ok(false);
            }
            output.flush()?;
            drop(output);
            set_executable(&tmp, manifest.executable)?;
            let Some(verified) = verify_staged_blob(tmp.clone(), digest)? else {
                return Ok(false);
            };
            publish_verified_destination(verified, destination)?;
            Ok(true)
        })();
        if !matches!(result, Ok(true)) {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }

    fn materialize_chunk_at(
        &self,
        output: &std::fs::File,
        offset: u64,
        chunk: &ChunkRef,
    ) -> Result<bool> {
        let path = self.chunk(&chunk.sha256);
        let bytes = if let Ok(bytes) = std::fs::read(&path) {
            bytes
        } else if let Some(bytes) = self.reconstruct_delta(chunk)? {
            bytes
        } else {
            return Ok(false);
        };
        let Some(verified) = UnverifiedBytes(&bytes).verify_sha256(&chunk.sha256) else {
            let _ = std::fs::remove_file(path);
            return Ok(false);
        };
        if verified.0.len() != chunk.length as usize {
            return Ok(false);
        }
        write_all_at(output, verified.0, offset)?;
        Ok(true)
    }

    fn reconstruct_delta(&self, chunk: &ChunkRef) -> Result<Option<Vec<u8>>> {
        let Some(delta) = &chunk.delta else {
            return Ok(None);
        };
        let base_path = self.chunk(&delta.base_sha256);
        let Ok(base) = std::fs::read(&base_path) else {
            return Ok(None);
        };
        let Some(base) = UnverifiedBytes(&base).verify_sha256(&delta.base_sha256) else {
            let _ = std::fs::remove_file(base_path);
            return Ok(None);
        };
        let patch_path = self.delta(&chunk.sha256, &delta.patch_sha256);
        let Ok(patch) = std::fs::read(&patch_path) else {
            return Ok(None);
        };
        if patch.len() != delta.patch_bytes as usize {
            return Ok(None);
        }
        let Some(patch) = UnverifiedBytes(&patch).verify_sha256(&delta.patch_sha256) else {
            let _ = std::fs::remove_file(patch_path);
            return Ok(None);
        };
        let Ok(reconstructed) = zstd_undelta(base.0, patch.0, chunk.length as usize) else {
            return Ok(None);
        };
        let Some(verified) = UnverifiedBytes(&reconstructed).verify_sha256(&chunk.sha256) else {
            return Ok(None);
        };
        self.publish_chunk(&chunk.sha256, verified)?;
        Ok(Some(reconstructed))
    }

    /// Best-effort oldest-first GC. Objects are immutable, so racing readers
    /// either retain an open inode or report a cache miss and rebuild.
    pub fn gc(&self) -> Result<u64> {
        if self.max_bytes == 0
            || (!self.root.exists()
                && !self.chunks.exists()
                && !self.manifests.exists()
                && !self.deltas.exists())
        {
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
        for store in [&self.root, &self.chunks, &self.manifests, &self.deltas] {
            if !store.is_dir() {
                continue;
            }
            for shard in std::fs::read_dir(store)? {
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

    /// Persistent deduplication evidence. Reuse is calculated from manifest
    /// references rather than process-local counters, so it survives daemon
    /// restarts and can be inspected in CI.
    pub fn stats(&self) -> Result<CasStats> {
        let (object_count, object_bytes) = count_store(&self.root)?;
        let (chunk_count, chunk_bytes) = count_store(&self.chunks)?;
        let (delta_count, delta_bytes) = count_store(&self.deltas)?;
        let mut manifest_count = 0u64;
        let mut logical_chunk_count = 0u64;
        let mut logical_chunk_bytes = 0u64;
        let mut referenced = BTreeMap::<String, u64>::new();
        for path in store_files(&self.manifests)? {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Ok(manifest) = postcard::from_bytes::<ChunkManifest>(&bytes) else {
                continue;
            };
            if !valid_manifest(&manifest, &manifest.blob_digest) {
                continue;
            }
            manifest_count += 1;
            for chunk in manifest.chunks {
                logical_chunk_count += 1;
                logical_chunk_bytes += u64::from(chunk.length);
                referenced
                    .entry(chunk.sha256)
                    .or_insert(u64::from(chunk.length));
            }
        }
        let unique_referenced_bytes = referenced.values().copied().sum::<u64>();
        let reused_chunk_count = logical_chunk_count.saturating_sub(referenced.len() as u64);
        let reused_chunk_bytes = logical_chunk_bytes.saturating_sub(unique_referenced_bytes);
        let chunk_reuse_ratio = if logical_chunk_bytes == 0 {
            0.0
        } else {
            reused_chunk_bytes as f64 / logical_chunk_bytes as f64
        };
        Ok(CasStats {
            object_count,
            object_bytes,
            chunk_count,
            chunk_bytes,
            delta_count,
            delta_bytes,
            manifest_count,
            logical_chunk_count,
            logical_chunk_bytes,
            reused_chunk_count,
            reused_chunk_bytes,
            chunk_reuse_ratio,
        })
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn write_all_at(file: &std::fs::File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    file.write_all_at(bytes, offset)
}

#[cfg(windows)]
fn write_all_at(file: &std::fs::File, mut bytes: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt as _;
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write complete CAS chunk",
            ));
        }
        bytes = &bytes[written..];
        offset += written as u64;
    }
    Ok(())
}

fn positional_base(
    manifest: &ChunkManifest,
    target_start: u64,
    target_end: u64,
) -> Option<&ChunkRef> {
    let mut start = 0u64;
    manifest
        .chunks
        .iter()
        .filter_map(|chunk| {
            let end = start + u64::from(chunk.length);
            let overlap = end.min(target_end).saturating_sub(start.max(target_start));
            start = end;
            (overlap > 0).then_some((overlap, chunk))
        })
        .max_by_key(|(overlap, _)| *overlap)
        .map(|(_, chunk)| chunk)
}

fn zstd_window_log(length: usize) -> u32 {
    let bits = usize::BITS - length.saturating_sub(1).leading_zeros() + 1;
    bits.clamp(20, 30)
}

fn zstd_delta(base: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    let mut compressor = zstd::bulk::Compressor::with_dictionary(19, base)?;
    compressor.long_distance_matching(true)?;
    compressor.window_log(zstd_window_log(base.len().max(target.len())))?;
    Ok(compressor.compress(target)?)
}

fn zstd_undelta(base: &[u8], patch: &[u8], target_length: usize) -> Result<Vec<u8>> {
    let mut decompressor = zstd::bulk::Decompressor::with_dictionary(base)?;
    decompressor.window_log_max(30)?;
    Ok(decompressor.decompress(patch, target_length)?)
}

fn verify_staged_blob(path: PathBuf, expected: &str) -> Result<Option<VerifiedBlob>> {
    if crate::hashcache::hash_file(&path)? == expected {
        Ok(Some(VerifiedBlob { path }))
    } else {
        let _ = std::fs::remove_file(path);
        Ok(None)
    }
}

/// Publish to the immutable store. `false` means another verified writer won.
fn publish_verified_blob(blob: VerifiedBlob, destination: &Path) -> Result<bool> {
    match std::fs::rename(&blob.path, destination) {
        Ok(()) => Ok(true),
        Err(_) if destination.is_file() => {
            let _ = std::fs::remove_file(blob.path);
            Ok(false)
        }
        Err(error) => {
            let _ = std::fs::remove_file(blob.path);
            Err(error.into())
        }
    }
}

fn publish_verified_destination(blob: VerifiedBlob, destination: &Path) -> Result<()> {
    let _ = std::fs::remove_file(destination);
    std::fs::rename(&blob.path, destination).with_context(|| {
        format!(
            "failed to publish verified materialization {}",
            destination.display()
        )
    })
}

fn valid_manifest(manifest: &ChunkManifest, expected_digest: &str) -> bool {
    manifest.version == CHUNK_MANIFEST_VERSION
        && manifest.blob_digest == expected_digest
        && manifest.average == DEFAULT_AVG as u32
        && manifest.minimum == DEFAULT_MIN as u32
        && manifest.maximum == DEFAULT_MAX as u32
        && manifest.normalization == DEFAULT_NORMALIZATION as u8
        && manifest.seed == 0
        && !manifest.chunks.is_empty()
        && manifest.chunks.iter().all(|chunk| {
            chunk.length > 0
                && chunk.length <= DEFAULT_MAX as u32
                && chunk.sha256.len() == 64
                && chunk.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                && chunk.delta.as_ref().is_none_or(|delta| {
                    delta.base_sha256.len() == 64
                        && delta
                            .base_sha256
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                        && delta.patch_sha256.len() == 64
                        && delta
                            .patch_sha256
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                        && delta.patch_bytes > 0
                })
        })
        && manifest.chunks.iter().try_fold(0u64, |total, chunk| {
            total.checked_add(u64::from(chunk.length))
        }) == Some(manifest.total_bytes)
}

#[cfg(unix)]
fn executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    permissions.set_mode(if executable {
        mode | 0o111
    } else {
        mode & !0o111
    });
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

fn store_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for shard in std::fs::read_dir(root)? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(shard.path())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn count_store(root: &Path) -> Result<(u64, u64)> {
    let mut count = 0u64;
    let mut bytes = 0u64;
    for path in store_files(root)? {
        count += 1;
        bytes = bytes.saturating_add(std::fs::metadata(path)?.len());
    }
    Ok((count, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random_bytes(length: usize) -> Vec<u8> {
        let mut state = 0x4d59_5df4_d0f3_3173u64;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn chunked_fixture(name: &str) -> (PathBuf, LocalCas, PathBuf, String, ChunkManifest) {
        let root = std::env::temp_dir().join(format!(
            "frost-cas-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("large-output");
        std::fs::write(&source, pseudo_random_bytes(8 * 1024 * 1024)).unwrap();
        let digest = crate::hashcache::hash_file(&source).unwrap();
        let cas = LocalCas::new(&root, 128 * 1024 * 1024);
        cas.put(&source, &digest).unwrap();
        let manifest =
            postcard::from_bytes::<ChunkManifest>(&std::fs::read(cas.manifest(&digest)).unwrap())
                .unwrap();
        assert!(manifest.chunks.len() > 4);
        (root, cas, source, digest, manifest)
    }

    #[test]
    fn concurrent_identical_publications_leave_one_valid_object() {
        let root =
            std::env::temp_dir().join(format!("frost-cas-concurrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("output");
        let restored = root.join("restored");
        std::fs::write(&source, vec![b'x'; 2 * 1024 * 1024]).unwrap();
        let digest = crate::hashcache::hash_file(&source).unwrap();
        let cas = std::sync::Arc::new(LocalCas::new(&root, 8 * 1024 * 1024));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));

        let threads = (0..16)
            .map(|_| {
                let cas = cas.clone();
                let barrier = barrier.clone();
                let source = source.clone();
                let digest = digest.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    cas.put(&source, &digest)
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }

        assert!(cas.materialize(&digest, &restored).unwrap());
        assert_eq!(crate::hashcache::hash_file(&restored).unwrap(), digest);
        let object = cas.object(&digest);
        let shard = object.parent().unwrap();
        assert!(
            std::fs::read_dir(shard).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "successful concurrent publication must not leave staging files"
        );

        std::fs::remove_dir_all(root).ok();
    }

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

    #[test]
    fn chunk_store_roundtrips_and_reuses_after_a_one_byte_change() {
        let (root, cas, source, first_digest, first) = chunked_fixture("chunk-reuse");
        std::fs::remove_file(cas.object(&first_digest)).unwrap();
        let restored = root.join("restored");
        assert!(cas.materialize(&first_digest, &restored).unwrap());
        assert_eq!(
            crate::hashcache::hash_file(&restored).unwrap(),
            first_digest
        );

        let mut changed = std::fs::read(&source).unwrap();
        let middle = changed.len() / 2;
        changed[middle] ^= 1;
        std::fs::write(&source, changed).unwrap();
        let second_digest = crate::hashcache::hash_file(&source).unwrap();
        cas.put(&source, &second_digest).unwrap();
        let second = postcard::from_bytes::<ChunkManifest>(
            &std::fs::read(cas.manifest(&second_digest)).unwrap(),
        )
        .unwrap();
        let first_set = first
            .chunks
            .iter()
            .map(|chunk| chunk.sha256.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let reused = second
            .chunks
            .iter()
            .filter(|chunk| first_set.contains(chunk.sha256.as_str()))
            .count();
        assert!(
            reused * 4 >= second.chunks.len() * 3,
            "one byte retained only {reused}/{} chunks",
            second.chunks.len()
        );
        let stats = cas.stats().unwrap();
        assert!(stats.reused_chunk_count > 0);
        assert!(stats.reused_chunk_bytes > 0);
        assert!(stats.chunk_reuse_ratio > 0.35, "{stats:?}");
        assert!(
            stats.delta_count > 0,
            "one-byte residual should have a delta"
        );
        let delta_chunk = second
            .chunks
            .iter()
            .find(|chunk| chunk.delta.is_some())
            .expect("one-byte residual should select its positional base");
        std::fs::remove_file(cas.object(&second_digest)).unwrap();
        std::fs::remove_file(cas.chunk(&delta_chunk.sha256)).unwrap();
        let delta_restored = root.join("delta-restored");
        assert!(cas.materialize(&second_digest, &delta_restored).unwrap());
        assert_eq!(
            crate::hashcache::hash_file(&delta_restored).unwrap(),
            second_digest,
            "delta reconstruction must still pass the complete blob digest"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn materialization_failure_injection_never_publishes_unverified_bytes() {
        #[derive(Clone, Copy, Debug)]
        enum Fault {
            BitFlip,
            MissingChunk,
            WrongChunk,
            ReorderedChunks,
            PartialChunk,
            ParameterMismatch,
            OnlyOneChunkPresent,
        }

        for fault in [
            Fault::BitFlip,
            Fault::MissingChunk,
            Fault::WrongChunk,
            Fault::ReorderedChunks,
            Fault::PartialChunk,
            Fault::ParameterMismatch,
            Fault::OnlyOneChunkPresent,
        ] {
            let (root, cas, _source, digest, mut manifest) =
                chunked_fixture(&format!("fault-{fault:?}"));
            std::fs::remove_file(cas.object(&digest)).unwrap();
            let first = cas.chunk(&manifest.chunks[0].sha256);
            match fault {
                Fault::BitFlip => {
                    let mut bytes = std::fs::read(&first).unwrap();
                    let middle = bytes.len() / 2;
                    bytes[middle] ^= 1;
                    std::fs::write(&first, bytes).unwrap();
                }
                Fault::MissingChunk => {
                    std::fs::remove_file(&first).unwrap();
                }
                Fault::WrongChunk => {
                    let other = cas.chunk(&manifest.chunks[1].sha256);
                    std::fs::copy(other, &first).unwrap();
                }
                Fault::ReorderedChunks => {
                    manifest.chunks.swap(0, 1);
                    std::fs::write(
                        cas.manifest(&digest),
                        postcard::to_allocvec(&manifest).unwrap(),
                    )
                    .unwrap();
                }
                Fault::PartialChunk => {
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&first)
                        .unwrap();
                    file.set_len(u64::from(manifest.chunks[0].length) / 2)
                        .unwrap();
                }
                Fault::ParameterMismatch => {
                    manifest.average /= 2;
                    std::fs::write(
                        cas.manifest(&digest),
                        postcard::to_allocvec(&manifest).unwrap(),
                    )
                    .unwrap();
                }
                Fault::OnlyOneChunkPresent => {
                    for chunk in manifest.chunks.iter().skip(1) {
                        let _ = std::fs::remove_file(cas.chunk(&chunk.sha256));
                    }
                }
            }

            // This sentinel models Bazel #29544's dangerous final-path stream:
            // no individual chunk may truncate or replace it before complete
            // chunk and final-blob verification has succeeded.
            let destination = root.join("published-output");
            std::fs::write(&destination, b"last known good").unwrap();
            assert!(
                !cas.materialize(&digest, &destination).unwrap(),
                "fault {fault:?} was accepted"
            );
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                b"last known good",
                "fault {fault:?} touched the final path before verification"
            );
            std::fs::remove_dir_all(root).ok();
        }

        #[derive(Clone, Copy, Debug)]
        enum DeltaFault {
            PatchBitFlip,
            PatchTruncated,
            BaseMissing,
            WrongBase,
        }
        for fault in [
            DeltaFault::PatchBitFlip,
            DeltaFault::PatchTruncated,
            DeltaFault::BaseMissing,
            DeltaFault::WrongBase,
        ] {
            let (root, cas, source, _first_digest, _first) =
                chunked_fixture(&format!("delta-fault-{fault:?}"));
            let mut bytes = std::fs::read(&source).unwrap();
            let middle = bytes.len() / 2;
            bytes[middle] ^= 1;
            std::fs::write(&source, bytes).unwrap();
            let digest = crate::hashcache::hash_file(&source).unwrap();
            cas.put(&source, &digest).unwrap();
            let manifest = postcard::from_bytes::<ChunkManifest>(
                &std::fs::read(cas.manifest(&digest)).unwrap(),
            )
            .unwrap();
            let delta_chunk = manifest
                .chunks
                .iter()
                .find(|chunk| chunk.delta.is_some())
                .expect("one-bit change should produce a residual delta");
            let delta = delta_chunk.delta.as_ref().unwrap();
            std::fs::remove_file(cas.object(&digest)).unwrap();
            std::fs::remove_file(cas.chunk(&delta_chunk.sha256)).unwrap();
            match fault {
                DeltaFault::PatchBitFlip => {
                    let path = cas.delta(&delta_chunk.sha256, &delta.patch_sha256);
                    let mut patch = std::fs::read(&path).unwrap();
                    let middle = patch.len() / 2;
                    patch[middle] ^= 1;
                    std::fs::write(path, patch).unwrap();
                }
                DeltaFault::PatchTruncated => {
                    let path = cas.delta(&delta_chunk.sha256, &delta.patch_sha256);
                    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                    file.set_len(u64::from(delta.patch_bytes) / 2).unwrap();
                }
                DeltaFault::BaseMissing => {
                    std::fs::remove_file(cas.chunk(&delta.base_sha256)).unwrap();
                }
                DeltaFault::WrongBase => {
                    let wrong = manifest
                        .chunks
                        .iter()
                        .find(|chunk| {
                            chunk.sha256 != delta.base_sha256 && cas.chunk(&chunk.sha256).is_file()
                        })
                        .unwrap();
                    std::fs::copy(cas.chunk(&wrong.sha256), cas.chunk(&delta.base_sha256)).unwrap();
                }
            }
            let destination = root.join("published-delta-output");
            std::fs::write(&destination, b"last known good").unwrap();
            assert!(
                !cas.materialize(&digest, &destination).unwrap(),
                "delta fault {fault:?} was accepted"
            );
            assert_eq!(
                std::fs::read(destination).unwrap(),
                b"last known good",
                "delta fault {fault:?} touched final output"
            );
            std::fs::remove_dir_all(root).ok();
        }
    }
}
