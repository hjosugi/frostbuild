pub mod cas;
pub mod depfile;
pub mod fastcdc;
pub mod graph;
pub mod graph_store;
pub mod hashcache;
pub mod journal;
pub mod manifest;
pub mod paths;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionKey {
    pub builder: String,
    pub target: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub inputs: BTreeMap<String, String>,
    pub toolchain_hash: String,
}

impl ActionKey {
    pub fn new(
        builder: impl Into<String>,
        target: impl Into<String>,
        argv: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
        toolchain_hash: impl Into<String>,
    ) -> Self {
        Self {
            builder: builder.into(),
            target: target.into(),
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            env: BTreeMap::new(),
            inputs: BTreeMap::new(),
            toolchain_hash: toolchain_hash.into(),
        }
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn with_input(mut self, path: impl Into<String>, digest: impl Into<String>) -> Self {
        self.inputs.insert(path.into(), digest.into());
        self
    }

    pub fn canonical_payload(&self, workspace_root: &Path) -> String {
        let cwd = self
            .cwd
            .strip_prefix(workspace_root)
            .map(|path| {
                if path.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    path.to_string_lossy().replace('\\', "/")
                }
            })
            .unwrap_or_else(|_| self.cwd.to_string_lossy().replace('\\', "/"));

        // One allocation for the whole payload rather than a handful of
        // doublings: every field contributes its key, its length and a few
        // separators on top of the value.
        let capacity = 128
            + self.builder.len()
            + self.target.len()
            + cwd.len()
            + self.toolchain_hash.len()
            + self.argv.iter().map(|a| a.len() + 16).sum::<usize>()
            + self
                .env
                .iter()
                .map(|(k, v)| k.len() + v.len() + 32)
                .sum::<usize>()
            + self
                .inputs
                .iter()
                .map(|(p, d)| p.len() + d.len() + 32)
                .sum::<usize>();
        let mut payload = String::with_capacity(capacity);
        write_field(&mut payload, "schema", "frost-action-key-v2");
        write_field(&mut payload, "builder", &self.builder);
        write_field(&mut payload, "target", &self.target);
        write_field(&mut payload, "cwd", &cwd);
        write_field(&mut payload, "toolchain", &self.toolchain_hash);
        for arg in &self.argv {
            write_field(&mut payload, "argv", arg);
        }
        for (key, value) in &self.env {
            write_field(&mut payload, "env", key);
            write_field(&mut payload, "env", value);
        }
        for (path, digest) in &self.inputs {
            write_field(&mut payload, "input", path);
            write_field(&mut payload, "input", digest);
        }
        payload
    }

    pub fn stable_id(&self, workspace_root: &Path) -> String {
        let payload = self.canonical_payload(workspace_root);
        format!("{:016x}", fnv1a64(payload.as_bytes()))
    }

    /// Full-strength content digest of the canonical payload. This is the
    /// action cache key: any change to command, environment, toolchain, or
    /// input digests changes this value.
    pub fn digest(&self, workspace_root: &Path) -> String {
        let payload = self.canonical_payload(workspace_root);
        blake3::hash(payload.as_bytes()).to_hex().to_string()
    }
}

/// Appends one length-prefixed field. Byte-for-byte identical to writing
/// `key\0len\0value\0`; the length is formatted in place because
/// `len().to_string()` heap-allocates once per field, and an action key writes
/// a field per argv entry and two per input.
fn write_field(payload: &mut String, key: &str, value: &str) {
    use std::fmt::Write;
    payload.push_str(key);
    payload.push('\0');
    let _ = write!(payload, "{}", value.len());
    payload.push('\0');
    payload.push_str(value);
    payload.push('\0');
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn canonical_payload_sorts_env_and_inputs() {
        let root = Path::new("/repo");
        let a = ActionKey::new("builder", "app", ["compile", "app"], "/repo", "tool")
            .with_env("B", "2")
            .with_env("A", "1")
            .with_input("src/b", "bbb")
            .with_input("src/a", "aaa");
        let b = ActionKey::new("builder", "app", ["compile", "app"], "/repo/.", "tool")
            .with_env("A", "1")
            .with_env("B", "2")
            .with_input("src/a", "aaa")
            .with_input("src/b", "bbb");

        assert_eq!(a.canonical_payload(root), b.canonical_payload(root));
        assert_eq!(a.stable_id(root), b.stable_id(root));
        assert_eq!(a.digest(root), b.digest(root));
    }

    #[test]
    fn flag_change_changes_stable_id() {
        let root = Path::new("/repo");
        let release = ActionKey::new("builder", "app", ["compile", "app"], "/repo", "tool")
            .with_env("FROSTBUILD_FLAGS", "--release");
        let debug = ActionKey::new("builder", "app", ["compile", "app"], "/repo", "tool")
            .with_env("FROSTBUILD_FLAGS", "--debug");

        assert_ne!(release.stable_id(root), debug.stable_id(root));
        assert_ne!(release.digest(root), debug.digest(root));
    }

    #[test]
    fn input_digest_change_changes_digest() {
        let root = Path::new("/repo");
        let a =
            ActionKey::new("builder", "app", ["cc"], "/repo", "tool").with_input("src/a.c", "aaa");
        let b =
            ActionKey::new("builder", "app", ["cc"], "/repo", "tool").with_input("src/a.c", "bbb");
        assert_ne!(a.digest(root), b.digest(root));
    }

    proptest! {
        #[test]
        fn action_key_is_invariant_to_input_insertion_order(
            entries in prop::collection::btree_map("[a-z]{1,8}", "[0-9a-f]{1,16}", 0..32)
        ) {
            let root = Path::new("/repo");
            let mut forward = ActionKey::new("builder", "target", ["cc"], root, "tool");
            let mut reverse = ActionKey::new("builder", "target", ["cc"], root, "tool");
            for (path, digest) in &entries { forward = forward.with_input(path, digest); }
            for (path, digest) in entries.iter().rev() { reverse = reverse.with_input(path, digest); }
            prop_assert_eq!(forward.digest(root), reverse.digest(root));
        }
    }
}
