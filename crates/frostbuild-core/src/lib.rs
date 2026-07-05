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

        let mut payload = String::new();
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
}

fn write_field(payload: &mut String, key: &str, value: &str) {
    payload.push_str(key);
    payload.push('\0');
    payload.push_str(&value.len().to_string());
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
    }

    #[test]
    fn flag_change_changes_stable_id() {
        let root = Path::new("/repo");
        let release = ActionKey::new("builder", "app", ["compile", "app"], "/repo", "tool")
            .with_env("FROSTBUILD_FLAGS", "--release");
        let debug = ActionKey::new("builder", "app", ["compile", "app"], "/repo", "tool")
            .with_env("FROSTBUILD_FLAGS", "--debug");

        assert_ne!(release.stable_id(root), debug.stable_id(root));
    }
}
