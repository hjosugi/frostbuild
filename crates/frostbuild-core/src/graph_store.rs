use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::graph::BuildGraph;
use crate::manifest::{Manifest, HOST_PLATFORM};

const MAGIC: &[u8; 8] = b"FRSTGR01";
const VERSION: u32 = 3;

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
    /// (workspace-relative dir path, mtime_ns) for every non-ignored dir.
    dirs: Vec<(String, i128)>,
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
    // The workspace root itself: file adds/removes at the top level only
    // show up in the root dir's own mtime.
    let mut dirs = vec![(String::new(), mtime_ns(&std::fs::metadata(root)?))];
    walk_dirs(root, root, &mut dirs)?;
    dirs.sort();
    Ok(SourcesStamp { manifests, dirs })
}

/// Mirrors `discover_package_manifests` skip rules so the stamp covers
/// exactly the tree that package discovery and glob expansion can see.
fn walk_dirs(root: &Path, dir: &Path, out: &mut Vec<(String, i128)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if matches!(
            entry.file_name().to_str(),
            Some(".git" | ".frost" | "target")
        ) {
            continue;
        }
        let ty = entry.file_type()?;
        if ty.is_dir() && !ty.is_symlink() {
            let path = entry.path();
            let meta = entry.metadata()?;
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, mtime_ns(&meta)));
            walk_dirs(root, &path, out)?;
        }
    }
    Ok(())
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
        // Ignore-file entries are re-added by sources_stamp (and may be
        // ABSENT); only real manifests are required to exist.
        let manifest_paths: Vec<String> = parsed
            .stamp
            .manifests
            .iter()
            .map(|(p, _)| p.clone())
            .filter(|p| p != ".gitignore" && p != ".frostignore")
            .collect();
        let current = sources_stamp(root, &manifest_paths)?;
        anyhow::ensure!(current == parsed.stamp, "workspace sources changed");
    }
    postcard::from_bytes(parsed.payload).context("corrupt graph store")
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
