use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::graph::BuildGraph;
use crate::manifest::{Manifest, HOST_PLATFORM};

const MAGIC: &[u8; 8] = b"FRSTGR01";
const VERSION: u32 = 6;

/// Evidence that the manifest inputs which produced a cached graph are
/// unchanged, checkable without parsing any manifest: exact bytes of every
/// contributing manifest file plus a stat stamp of every workspace directory.
/// Directory mtimes change whenever entries are added/removed/renamed, so an
/// equal stamp implies identical package discovery and glob expansion; file
/// content edits cannot alter either. This makes the warm path sound while
/// skipping TOML parsing entirely.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SourcesStamp {
    /// (workspace-relative path, BLAKE3 of bytes) per contributing manifest.
    manifests: Vec<(String, String)>,
    /// Identity of every non-ignored directory and its immediate entries.
    dirs: Vec<DirStamp>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DirStamp {
    path: String,
    mtime_ns: i128,
    /// BLAKE3 of sorted native entry names plus their filesystem kind.
    /// This is required in addition to mtime: Windows can expose the same
    /// directory timestamp immediately before and after an entry mutation.
    entries_hash: [u8; 32],
}

pub struct GraphStore;

impl GraphStore {
    pub fn load_or_compile(root: &Path, manifest: &Manifest, profile: &str) -> Result<BuildGraph> {
        Self::load_or_compile_configured(root, manifest, profile, HOST_PLATFORM)
    }

    pub fn load_or_compile_configured(
        root: &Path,
        manifest: &Manifest,
        profile: &str,
        platform: &str,
    ) -> Result<BuildGraph> {
        let fingerprint = manifest_fingerprint(manifest, profile, platform)?;
        let path = store_path(root, profile, platform);
        if let Ok(graph) = load_graph(&path, Some(&fingerprint), None) {
            // Keep the warm path viable for workspaces whose builds write
            // outputs into the source tree: a stale sources stamp would
            // otherwise force every future invocation through a full parse.
            if load_graph(&path, None, Some(root)).is_err() {
                save_graph(root, &path, &fingerprint, &manifest.manifest_paths, &graph)?;
            }
            return Ok(graph);
        }
        let graph = BuildGraph::from_manifest_configured(manifest, profile, platform)?;
        save_graph(root, &path, &fingerprint, &manifest.manifest_paths, &graph)?;
        Ok(graph)
    }

    /// Warm fast path: return the cached graph when the sources stamp proves
    /// the manifest inputs are unchanged, without loading the manifest at
    /// all. `None` means the caller must fall back to `Manifest::load` +
    /// [`GraphStore::load_or_compile_configured`].
    pub fn load_cached(root: &Path, profile: &str, platform: &str) -> Option<BuildGraph> {
        let path = store_path(root, profile, platform);
        load_graph(&path, None, Some(root)).ok()
    }

    /// Validate the manifest/package-discovery evidence in a cached graph
    /// without deserializing the graph payload itself.
    ///
    /// A whole-workspace no-op certificate already describes the files that
    /// must remain unchanged. It still needs to prove that the graph
    /// definition is current, but decoding thousands of actions merely to
    /// learn that no action will run defeats that fast path.
    pub fn cached_sources_current(root: &Path, profile: &str, platform: &str) -> bool {
        Self::cached_fingerprint(root, profile, platform).is_some()
    }

    /// Fingerprint of the manifest/profile/platform tuple embedded in a
    /// source-current graph store, without deserializing its graph payload.
    pub fn cached_fingerprint(root: &Path, profile: &str, platform: &str) -> Option<[u8; 32]> {
        let path = store_path(root, profile, platform);
        validate_cached_sources(&path, root).ok()
    }

    pub fn validate_bytes(bytes: &[u8]) -> Result<()> {
        let parsed = parse_header(bytes)?;
        let _: BuildGraph = postcard::from_bytes(parsed.payload).context("corrupt graph store")?;
        Ok(())
    }
}

fn store_path(root: &Path, profile: &str, platform: &str) -> PathBuf {
    if platform == HOST_PLATFORM {
        root.join(format!(".frost/graph-{profile}.bin"))
    } else {
        root.join(format!(".frost/graph-{platform}-{profile}.bin"))
    }
}

fn manifest_fingerprint(manifest: &Manifest, profile: &str, platform: &str) -> Result<[u8; 32]> {
    let bytes = postcard::to_allocvec(&(manifest, profile, platform))?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

fn sources_stamp(root: &Path, manifest_paths: &[String]) -> Result<SourcesStamp> {
    let mut manifests = Vec::with_capacity(manifest_paths.len() + 2);
    for rel in manifest_paths {
        let bytes =
            std::fs::read(root.join(rel)).with_context(|| format!("missing manifest {rel}"))?;
        manifests.push((rel.clone(), blake3::hash(&bytes).to_hex().to_string()));
    }
    // Root ignore files gate glob expansion, so their content (or absence)
    // is part of the stamp even though it never touches a dir mtime.
    for ignore in [".gitignore", ".frostignore"] {
        if manifest_paths.iter().any(|p| p == ignore) {
            continue;
        }
        let digest = match std::fs::read(root.join(ignore)) {
            Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
            Err(_) => "ABSENT".to_string(),
        };
        manifests.push((ignore.to_string(), digest));
    }
    let mut dirs = Vec::new();
    walk_dirs(root, root, &mut dirs)?;
    dirs.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(SourcesStamp { manifests, dirs })
}

/// Mirrors `discover_package_manifests` skip rules so the stamp covers
/// exactly the tree that package discovery and glob expansion can see.
fn walk_dirs(root: &Path, dir: &Path, out: &mut Vec<DirStamp>) -> Result<()> {
    let mut entries = Vec::new();
    let mut child_dirs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if matches!(
            entry.file_name().to_str(),
            Some(".git" | ".frost" | "target")
        ) {
            continue;
        }
        let ty = entry.file_type()?;
        let kind = if ty.is_symlink() {
            b'l'
        } else if ty.is_dir() {
            b'd'
        } else if ty.is_file() {
            b'f'
        } else {
            b'o'
        };
        entries.push((entry.file_name(), kind));
        if ty.is_dir() && !ty.is_symlink() {
            child_dirs.push(entry.path());
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    let mut hasher = blake3::Hasher::new();
    for (name, kind) in entries {
        hash_os_str(&mut hasher, &name);
        hasher.update(&[kind]);
    }
    let path = dir
        .strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    out.push(DirStamp {
        path,
        mtime_ns: mtime_ns(&std::fs::metadata(dir)?),
        entries_hash: *hasher.finalize().as_bytes(),
    });
    child_dirs.sort();
    for child in child_dirs {
        walk_dirs(root, &child, out)?;
    }
    Ok(())
}

#[cfg(unix)]
fn hash_os_str(hasher: &mut blake3::Hasher, value: &OsStr) {
    use std::os::unix::ffi::OsStrExt;
    let bytes = value.as_bytes();
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(windows)]
fn hash_os_str(hasher: &mut blake3::Hasher, value: &OsStr) {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = value.encode_wide().collect();
    hasher.update(&(units.len() as u64).to_le_bytes());
    for unit in units {
        hasher.update(&unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_os_str(hasher: &mut blake3::Hasher, value: &OsStr) {
    let value = value.to_string_lossy();
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(unix)]
fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
}

#[cfg(not(unix))]
fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

struct ParsedStore<'a> {
    fingerprint: &'a [u8],
    stamp: SourcesStamp,
    payload: &'a [u8],
}

fn parse_header(bytes: &[u8]) -> Result<ParsedStore<'_>> {
    anyhow::ensure!(
        bytes.len() >= 48 && &bytes[..8] == MAGIC,
        "invalid graph header"
    );
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    anyhow::ensure!(version == VERSION, "unsupported graph version");
    let stamp_len = u32::from_le_bytes(bytes[44..48].try_into().unwrap()) as usize;
    anyhow::ensure!(bytes.len() >= 48 + stamp_len, "truncated graph header");
    let stamp: SourcesStamp =
        postcard::from_bytes(&bytes[48..48 + stamp_len]).context("corrupt sources stamp")?;
    Ok(ParsedStore {
        fingerprint: &bytes[12..44],
        stamp,
        payload: &bytes[48 + stamp_len..],
    })
}

/// Loads a stored graph, validated either against a manifest fingerprint
/// (fallback path, manifest already parsed) or against a freshly computed
/// sources stamp (warm path, no manifest parse).
fn load_graph(
    path: &Path,
    fingerprint: Option<&[u8; 32]>,
    stamp_root: Option<&Path>,
) -> Result<BuildGraph> {
    let file = File::open(path)?;
    // SAFETY: the mapping is read-only and `file` remains alive until mapping creation.
    let mmap = unsafe { Mmap::map(&file)? };
    let parsed = parse_header(&mmap)?;
    if let Some(fingerprint) = fingerprint {
        anyhow::ensure!(parsed.fingerprint == fingerprint, "stale graph store");
    }
    if let Some(root) = stamp_root {
        ensure_sources_current(root, &parsed.stamp)?;
    }
    postcard::from_bytes(parsed.payload).context("corrupt graph store")
}

fn validate_cached_sources(path: &Path, root: &Path) -> Result<[u8; 32]> {
    let file = File::open(path)?;
    // SAFETY: the mapping is read-only and `file` remains alive until mapping creation.
    let mmap = unsafe { Mmap::map(&file)? };
    let parsed = parse_header(&mmap)?;
    ensure_sources_current(root, &parsed.stamp)?;
    Ok(parsed
        .fingerprint
        .try_into()
        .expect("graph header fingerprint is always 32 bytes"))
}

fn ensure_sources_current(root: &Path, stamp: &SourcesStamp) -> Result<()> {
    // Ignore-file entries are re-added by sources_stamp (and may be ABSENT);
    // only real manifests are required to exist.
    let manifest_paths: Vec<String> = stamp
        .manifests
        .iter()
        .map(|(p, _)| p.clone())
        .filter(|p| p != ".gitignore" && p != ".frostignore")
        .collect();
    let current = sources_stamp(root, &manifest_paths)?;
    anyhow::ensure!(current == *stamp, "workspace sources changed");
    Ok(())
}

fn save_graph(
    root: &Path,
    path: &Path,
    fingerprint: &[u8; 32],
    manifest_paths: &[String],
    graph: &BuildGraph,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stamp = sources_stamp(root, manifest_paths)?;
    let stamp_bytes = postcard::to_allocvec(&stamp)?;
    let tmp = path.with_extension("bin.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(MAGIC)?;
    file.write_all(&VERSION.to_le_bytes())?;
    file.write_all(fingerprint)?;
    file.write_all(&(stamp_bytes.len() as u32).to_le_bytes())?;
    file.write_all(&stamp_bytes)?;
    file.write_all(&postcard::to_allocvec(graph)?)?;
    file.flush()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MANIFEST_FILE;

    fn workspace(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("frost-graph-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn version_mismatch_falls_back_to_recompile() {
        let root = workspace("ver");
        std::fs::write(
            root.join(MANIFEST_FILE),
            "[target.a]\nkind='cc_binary'\nsrcs=['a.c']\n",
        )
        .unwrap();
        let manifest = Manifest::load(&root).unwrap();
        let graph = GraphStore::load_or_compile(&root, &manifest, "debug").unwrap();
        assert_eq!(graph.actions.len(), 2);
        std::fs::write(root.join(".frost/graph-debug.bin"), b"bad").unwrap();
        assert_eq!(
            GraphStore::load_or_compile(&root, &manifest, "debug")
                .unwrap()
                .actions
                .len(),
            2
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn warm_path_hits_without_manifest_and_misses_on_change() {
        let root = workspace("warm");
        std::fs::write(
            root.join(MANIFEST_FILE),
            "[target.a]\nkind='cc_binary'\nsrcs=['a.c']\n",
        )
        .unwrap();
        assert!(
            GraphStore::load_cached(&root, "debug", HOST_PLATFORM).is_none(),
            "no store yet"
        );
        let manifest = Manifest::load(&root).unwrap();
        GraphStore::load_or_compile(&root, &manifest, "debug").unwrap();

        assert!(GraphStore::cached_sources_current(
            &root,
            "debug",
            HOST_PLATFORM
        ));
        let cached =
            GraphStore::load_cached(&root, "debug", HOST_PLATFORM).expect("warm hit after save");
        assert_eq!(cached.actions.len(), 2);
        assert_eq!(cached.toolchain.cc, "cc");

        // Manifest edit invalidates the warm path.
        std::fs::write(
            root.join(MANIFEST_FILE),
            "[target.a]\nkind='cc_binary'\nsrcs=['a.c']\ncflags=['-O2']\n",
        )
        .unwrap();
        assert!(!GraphStore::cached_sources_current(
            &root,
            "debug",
            HOST_PLATFORM
        ));
        assert!(GraphStore::load_cached(&root, "debug", HOST_PLATFORM).is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn warm_path_misses_when_directories_change() {
        let root = workspace("dirs");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join(MANIFEST_FILE),
            "[target.a]\nkind='cc_binary'\nsrcs=['a.c']\n",
        )
        .unwrap();
        let manifest = Manifest::load(&root).unwrap();
        GraphStore::load_or_compile(&root, &manifest, "debug").unwrap();
        assert!(GraphStore::load_cached(&root, "debug", HOST_PLATFORM).is_some());

        // Adding a file changes the parent dir mtime → warm miss (globs and
        // package discovery may see a different tree).
        std::fs::write(root.join("src/new.c"), "int x;").unwrap();
        assert!(GraphStore::load_cached(&root, "debug", HOST_PLATFORM).is_none());
        std::fs::remove_dir_all(root).ok();
    }
}
