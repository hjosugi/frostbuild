use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use frostbuild_core::cas::LocalCas;
use frostbuild_core::graph_store::GraphStore;
use frostbuild_core::manifest::{Toolchain, HOST_PLATFORM};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{toolchain_closure_fingerprint_cached, DEFAULT_CAS_MAX_BYTES};

const MAGIC: &[u8; 8] = b"FRSTNO03";

#[derive(Debug, Clone, Copy)]
pub struct FastNoopHit {
    pub closure_actions: usize,
    pub graph_actions: usize,
}

#[derive(Debug, Clone)]
pub struct FastNoopWatchProof {
    certificate_digest: [u8; 32],
    profile: String,
    platform: String,
    toolchain: Toolchain,
    toolchain_hash: String,
    key_env: BTreeMap<String, String>,
    hit: FastNoopHit,
}

#[derive(Debug, Clone)]
pub struct FastNoopDaemonHit {
    pub hit: FastNoopHit,
    pub watch_proof: Option<FastNoopWatchProof>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileIdentity {
    mtime_ns: i128,
    size_and_mode: u64,
    ino: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileEvidence {
    path: String,
    /// `None` records an input that was expected to be absent.
    identity: Option<FileIdentity>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Certificate {
    profile: String,
    platform: String,
    graph_fingerprint: [u8; 32],
    closure_actions: usize,
    graph_actions: usize,
    toolchain: Toolchain,
    toolchain_hash: String,
    key_env: BTreeMap<String, String>,
    dynamic_env: BTreeMap<String, Option<String>>,
    files: Vec<FileEvidence>,
}

pub(crate) struct CertificateInput<'a> {
    pub root: &'a Path,
    pub profile: &'a str,
    pub platform: &'a str,
    pub closure_actions: usize,
    pub graph_actions: usize,
    pub toolchain: &'a Toolchain,
    pub toolchain_hash: &'a str,
    pub key_env: &'a BTreeMap<String, String>,
    pub dynamic_env: &'a BTreeMap<String, Option<String>>,
    pub paths: &'a [(&'a str, &'a str)],
}

/// Persist evidence for the next plain, default-target build.
///
/// This is written only after the normal graph/journal/action-key path proved
/// that the entire requested closure is cached. The certificate never makes a
/// changed build look current: any mismatch makes the caller use the normal
/// path again.
pub(crate) fn save(
    input: CertificateInput<'_>,
    verify_after_capture: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    let mut files: Vec<FileEvidence> = input
        .paths
        .par_iter()
        .map(|&(path, _digest)| FileEvidence {
            path: path.to_string(),
            identity: file_identity(input.root, path),
        })
        .collect();
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);

    let graph_fingerprint =
        GraphStore::cached_fingerprint(input.root, input.profile, input.platform)
            .context("graph sources changed while capturing the fast no-op certificate")?;
    let certificate = Certificate {
        profile: input.profile.to_string(),
        platform: input.platform.to_string(),
        graph_fingerprint,
        closure_actions: input.closure_actions,
        graph_actions: input.graph_actions,
        toolchain: input.toolchain.clone(),
        toolchain_hash: input.toolchain_hash.to_string(),
        key_env: input.key_env.clone(),
        dynamic_env: input.dynamic_env.clone(),
        files,
    };
    // Identities are captured after the normal digest check. Verify once more
    // after capture so a file changed in that gap cannot make its new stat
    // identity certify bytes that were never checked.
    if !verify_after_capture()? {
        anyhow::bail!("workspace changed while capturing the fast no-op certificate");
    }
    let payload = postcard::to_allocvec(&certificate)?;
    let path = certificate_path(input.root, input.profile, input.platform);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("bin.tmp");
    let mut bytes = Vec::with_capacity(MAGIC.len() + 32 + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(blake3::hash(&payload).as_bytes());
    bytes.extend_from_slice(&payload);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    Ok(())
}

/// Return a hit only when graph sources, toolchain, key environment and every
/// recorded input/output stat identity still match the fully checked build.
pub fn check(
    root: &Path,
    profile: &str,
    platform: &str,
    key_env: &BTreeMap<String, String>,
    read_dynamic_environment: bool,
) -> Result<Option<FastNoopHit>> {
    Ok(
        check_for_daemon(root, profile, platform, key_env, read_dynamic_environment)?
            .map(|validated| validated.hit),
    )
}

/// Fully validate a certificate and, when every recorded file is a normal
/// path below the watched workspace, return a compact proof the daemon may
/// recheck after an event-stream barrier.
pub fn check_for_daemon(
    root: &Path,
    profile: &str,
    platform: &str,
    key_env: &BTreeMap<String, String>,
    read_dynamic_environment: bool,
) -> Result<Option<FastNoopDaemonHit>> {
    if !safe_component(profile) || !safe_component(platform) {
        return Ok(None);
    }
    let path = certificate_path(root, profile, platform);
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(None);
    };
    if bytes.len() < 40 || &bytes[..8] != MAGIC {
        return Ok(None);
    }
    let payload = &bytes[40..];
    let payload_digest = *blake3::hash(payload).as_bytes();
    if payload_digest != bytes[8..40] {
        return Ok(None);
    }
    let Ok(certificate) = postcard::from_bytes::<Certificate>(payload) else {
        return Ok(None);
    };
    if certificate.profile != profile
        || certificate.platform != platform
        || certificate.key_env != *key_env
    {
        return Ok(None);
    }
    if (!read_dynamic_environment && !certificate.dynamic_env.is_empty())
        || (read_dynamic_environment
            && certificate.dynamic_env.iter().any(|(name, expected)| {
                std::env::var_os(name)
                    .map(|value| value.to_string_lossy().into_owned())
                    .as_ref()
                    != expected.as_ref()
            }))
    {
        return Ok(None);
    }
    if GraphStore::cached_fingerprint(root, profile, platform)
        != Some(certificate.graph_fingerprint)
    {
        return Ok(None);
    }
    let current_toolchain = toolchain_closure_fingerprint_cached(root, &certificate.toolchain)?;
    if current_toolchain != certificate.toolchain_hash {
        return Ok(None);
    }
    if !certificate
        .files
        .par_iter()
        .all(|file| file_identity(root, &file.path) == file.identity)
    {
        return Ok(None);
    }

    // Preserve the normal build's bounded CAS maintenance. A certificate is
    // written immediately after that maintenance path, so ordinary fast hits
    // observe the recent GC stamp and do no directory traversal.
    let _ = LocalCas::new(root, DEFAULT_CAS_MAX_BYTES).gc()?;
    let hit = FastNoopHit {
        closure_actions: certificate.closure_actions,
        graph_actions: certificate.graph_actions,
    };
    let watchable = !read_dynamic_environment
        && certificate.dynamic_env.is_empty()
        && certificate
            .files
            .iter()
            .all(|file| watchable_file(root, &file.path));
    let watch_proof = if watchable {
        Some(FastNoopWatchProof {
            certificate_digest: payload_digest,
            profile: certificate.profile,
            platform: certificate.platform,
            toolchain: certificate.toolchain,
            toolchain_hash: certificate.toolchain_hash,
            key_env: certificate.key_env,
            hit,
        })
    } else {
        None
    };
    Ok(Some(FastNoopDaemonHit { hit, watch_proof }))
}

/// Recheck the small portion of a watcher-backed proof that lives outside the
/// workspace event stream. The daemon must place an event barrier after this
/// call and reject the hit if any relevant workspace event was observed.
pub fn check_watch_proof(
    root: &Path,
    profile: &str,
    platform: &str,
    key_env: &BTreeMap<String, String>,
    proof: &FastNoopWatchProof,
) -> Result<Option<FastNoopHit>> {
    if proof.profile != profile || proof.platform != platform || proof.key_env != *key_env {
        return Ok(None);
    }
    let path = certificate_path(root, profile, platform);
    let Ok(mut file) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let mut header = [0u8; 40];
    if file.read_exact(&mut header).is_err()
        || &header[..8] != MAGIC
        || header[8..40] != proof.certificate_digest
    {
        return Ok(None);
    }
    let current_toolchain = toolchain_closure_fingerprint_cached(root, &proof.toolchain)?;
    if current_toolchain != proof.toolchain_hash {
        return Ok(None);
    }
    Ok(Some(proof.hit))
}

fn watchable_file(root: &Path, path: &str) -> bool {
    let Ok(canonical_root) = std::fs::canonicalize(root) else {
        return false;
    };
    let path = Path::new(path);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        if path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return false;
        }
        canonical_root.join(path)
    };
    let Ok(canonical) = std::fs::canonicalize(&candidate) else {
        // Missing expected files still need the ordinary identity check. A
        // watcher cannot prove that a path reached through a missing/symlinked
        // ancestor remains absent.
        return false;
    };
    let Ok(relative) = canonical.strip_prefix(&canonical_root) else {
        return false;
    };
    // Reject symlinks even when they happen to point back into the workspace;
    // notify backends differ in whether they follow a replaced link target.
    candidate == canonical && !relative.starts_with(".git")
}

fn certificate_path(root: &Path, profile: &str, platform: &str) -> PathBuf {
    if platform == HOST_PLATFORM {
        root.join(format!(".frost/noop-{profile}.bin"))
    } else {
        root.join(format!(".frost/noop-{platform}-{profile}.bin"))
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn resolve(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn file_identity(root: &Path, path: &str) -> Option<FileIdentity> {
    let metadata = std::fs::metadata(resolve(root, path)).ok()?;
    Some(metadata_identity(&metadata))
}

#[cfg(unix)]
fn metadata_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
        // Match HashCache: executable mode changes affect the content digest.
        size_and_mode: metadata.size() ^ ((metadata.mode() as u64 & 0o111) << 40),
        ino: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn metadata_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
        .unwrap_or(0);
    FileIdentity {
        mtime_ns,
        size_and_mode: metadata.len(),
        ino: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_identity_detects_content_and_executable_mode_changes() {
        let root =
            std::env::temp_dir().join(format!("frost-fast-noop-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("input");
        std::fs::write(&path, b"one").unwrap();
        let first = file_identity(&root, "input");
        std::fs::write(&path, b"different length").unwrap();
        assert_ne!(file_identity(&root, "input"), first);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let before = file_identity(&root, "input");
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).unwrap();
            assert_ne!(file_identity(&root, "input"), before);
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn unsafe_profile_components_never_escape_the_workspace() {
        assert!(safe_component("release-1"));
        assert!(!safe_component("../release"));
        assert!(!safe_component(""));
    }

    #[test]
    fn daemon_snapshot_never_assumes_arbitrary_pass_env_values() {
        let root = std::env::temp_dir().join(format!(
            "frost-fast-noop-dynamic-env-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".frost")).unwrap();
        let certificate = Certificate {
            profile: "debug".into(),
            platform: HOST_PLATFORM.into(),
            graph_fingerprint: [0; 32],
            closure_actions: 1,
            graph_actions: 1,
            toolchain: Toolchain::default(),
            toolchain_hash: String::new(),
            key_env: BTreeMap::new(),
            dynamic_env: BTreeMap::from([("FROST_TEST_DYNAMIC".into(), Some("one".into()))]),
            files: Vec::new(),
        };
        let payload = postcard::to_allocvec(&certificate).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(blake3::hash(&payload).as_bytes());
        bytes.extend_from_slice(&payload);
        std::fs::write(certificate_path(&root, "debug", HOST_PLATFORM), bytes).unwrap();

        let hit = check(&root, "debug", HOST_PLATFORM, &BTreeMap::new(), false).unwrap();
        assert!(hit.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn watcher_proofs_cover_only_normal_files_inside_the_workspace() {
        let root =
            std::env::temp_dir().join(format!("frost-fast-noop-watchable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/input"), b"inside").unwrap();
        assert!(watchable_file(&root, "src/input"));
        assert!(!watchable_file(&root, "../outside"));
        assert!(!watchable_file(&root, ".git/index"));

        let outside = root.with_extension("outside");
        std::fs::write(&outside, b"outside").unwrap();
        assert!(!watchable_file(&root, outside.to_str().unwrap()));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("src/link")).unwrap();
            assert!(!watchable_file(&root, "src/link"));
        }
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_file(outside).ok();
    }
}
