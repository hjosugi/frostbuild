use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use memmap2::Mmap;

use crate::graph::BuildGraph;
use crate::manifest::{Manifest, HOST_PLATFORM};

const MAGIC: &[u8; 8] = b"FRSTGR01";
const VERSION: u32 = 2;

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
        let path = if platform == HOST_PLATFORM {
            root.join(format!(".frost/graph-{profile}.bin"))
        } else {
            root.join(format!(".frost/graph-{platform}-{profile}.bin"))
        };
        if let Ok(graph) = load_graph(&path, &fingerprint) {
            return Ok(graph);
        }
        let graph = BuildGraph::from_manifest_configured(manifest, profile, platform)?;
        save_graph(&path, &fingerprint, &graph)?;
        Ok(graph)
    }

    pub fn validate_bytes(bytes: &[u8]) -> Result<()> {
        anyhow::ensure!(
            bytes.len() >= 44 && &bytes[..8] == MAGIC,
            "invalid graph header"
        );
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        anyhow::ensure!(version == VERSION, "unsupported graph version");
        let _: BuildGraph = postcard::from_bytes(&bytes[44..]).context("corrupt graph store")?;
        Ok(())
    }
}

fn manifest_fingerprint(manifest: &Manifest, profile: &str, platform: &str) -> Result<[u8; 32]> {
    let bytes = postcard::to_allocvec(&(manifest, profile, platform))?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

fn load_graph(path: &Path, fingerprint: &[u8; 32]) -> Result<BuildGraph> {
    let file = File::open(path)?;
    // SAFETY: the mapping is read-only and `file` remains alive until mapping creation.
    let mmap = unsafe { Mmap::map(&file)? };
    if mmap.len() < 44 || &mmap[..8] != MAGIC {
        anyhow::bail!("invalid graph header");
    }
    let version = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
    if version != VERSION || &mmap[12..44] != fingerprint {
        anyhow::bail!("stale graph store");
    }
    postcard::from_bytes(&mmap[44..]).context("corrupt graph store")
}

fn save_graph(path: &Path, fingerprint: &[u8; 32], graph: &BuildGraph) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("bin.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(MAGIC)?;
    file.write_all(&VERSION.to_le_bytes())?;
    file.write_all(fingerprint)?;
    file.write_all(&postcard::to_allocvec(graph)?)?;
    file.flush()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_mismatch_falls_back_to_recompile() {
        let root = std::env::temp_dir().join(format!("frost-graph-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let manifest = Manifest::parse_str("[target.a]\nkind='cc_binary'\nsrcs=['a.c']\n").unwrap();
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
}
