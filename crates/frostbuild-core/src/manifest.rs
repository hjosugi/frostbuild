use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::paths::validate_rel_path;

pub const MANIFEST_FILE: &str = "frost.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    CcBinary,
    CcLibrary,
    Genrule,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::CcBinary => "cc_binary",
            TargetKind::CcLibrary => "cc_library",
            TargetKind::Genrule => "genrule",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    default_targets: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolchain {
    cc: Option<String>,
    ar: Option<String>,
    #[serde(default)]
    cflags: Vec<String>,
    #[serde(default)]
    ldflags: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    kind: TargetKind,
    #[serde(default)]
    srcs: Vec<String>,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default)]
    cflags: Vec<String>,
    #[serde(default)]
    ldflags: Vec<String>,
    cmd: Option<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[serde(default)]
    workspace: RawWorkspace,
    #[serde(default)]
    toolchain: RawToolchain,
    #[serde(default)]
    target: BTreeMap<String, RawTarget>,
}

#[derive(Debug, Clone)]
pub struct Toolchain {
    pub cc: String,
    pub ar: String,
    pub cflags: Vec<String>,
    pub ldflags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    pub srcs: Vec<String>,
    pub deps: Vec<String>,
    /// Exported include directories, visible to this target and dependents.
    pub includes: Vec<String>,
    pub cflags: Vec<String>,
    pub ldflags: Vec<String>,
    /// Genrule only: shell command with `${in}` / `${out}` / `${outs}`.
    pub cmd: Option<String>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Debug)]
pub struct Manifest {
    pub default_targets: Vec<String>,
    pub toolchain: Toolchain,
    pub targets: BTreeMap<String, Target>,
}

impl Manifest {
    pub fn load(workspace_root: &Path) -> Result<Self> {
        let path = workspace_root.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("missing {} at {}", MANIFEST_FILE, path.display()))?;
        let raw: RawManifest =
            toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        Self::from_raw(raw)
    }

    pub fn parse_str(text: &str) -> Result<Self> {
        let raw: RawManifest = toml::from_str(text).context("failed to parse manifest")?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawManifest) -> Result<Self> {
        if raw.target.is_empty() {
            bail!("manifest declares no [target.*] sections");
        }

        let mut targets = BTreeMap::new();
        for (name, spec) in raw.target {
            let target =
                build_target(&name, spec).with_context(|| format!("invalid target {name:?}"))?;
            targets.insert(name, target);
        }

        for target in targets.values() {
            for dep in &target.deps {
                if dep == &target.name {
                    bail!("target {:?} depends on itself", target.name);
                }
                if !targets.contains_key(dep) {
                    bail!("target {:?} has unknown dep {dep:?}", target.name);
                }
            }
        }

        let default_targets = if raw.workspace.default_targets.is_empty() {
            let binaries: Vec<String> = targets
                .values()
                .filter(|t| t.kind == TargetKind::CcBinary)
                .map(|t| t.name.clone())
                .collect();
            if binaries.is_empty() {
                targets.keys().cloned().collect()
            } else {
                binaries
            }
        } else {
            for name in &raw.workspace.default_targets {
                if !targets.contains_key(name) {
                    bail!("workspace.default_targets names unknown target {name:?}");
                }
            }
            raw.workspace.default_targets
        };

        Ok(Self {
            default_targets,
            toolchain: Toolchain {
                cc: raw.toolchain.cc.unwrap_or_else(|| "cc".to_string()),
                ar: raw.toolchain.ar.unwrap_or_else(|| "ar".to_string()),
                cflags: raw.toolchain.cflags,
                ldflags: raw.toolchain.ldflags,
            },
            targets,
        })
    }
}

fn valid_target_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn build_target(name: &str, spec: RawTarget) -> Result<Target> {
    if !valid_target_name(name) {
        bail!("target name must match [A-Za-z0-9_-]+");
    }

    let srcs = validate_paths(&spec.srcs).context("srcs")?;
    let includes = validate_paths(&spec.includes).context("includes")?;
    let inputs = validate_paths(&spec.inputs).context("inputs")?;
    let outputs = validate_paths(&spec.outputs).context("outputs")?;

    match spec.kind {
        TargetKind::CcBinary | TargetKind::CcLibrary => {
            if srcs.is_empty() {
                bail!("{} requires non-empty srcs", spec.kind.as_str());
            }
            if spec.cmd.is_some() || !inputs.is_empty() || !outputs.is_empty() {
                bail!(
                    "{} must not set genrule fields (cmd/inputs/outputs)",
                    spec.kind.as_str()
                );
            }
        }
        TargetKind::Genrule => {
            if spec.cmd.as_deref().map(str::trim).unwrap_or("").is_empty() {
                bail!("genrule requires a non-empty cmd");
            }
            if outputs.is_empty() {
                bail!("genrule requires non-empty outputs");
            }
            if !srcs.is_empty() {
                bail!("genrule uses inputs, not srcs");
            }
            if !spec.cflags.is_empty() || !spec.ldflags.is_empty() {
                bail!("genrule must not set cflags/ldflags");
            }
        }
    }

    Ok(Target {
        name: name.to_string(),
        kind: spec.kind,
        srcs,
        deps: spec.deps,
        includes,
        cflags: spec.cflags,
        ldflags: spec.ldflags,
        cmd: spec.cmd,
        inputs,
        outputs,
    })
}

fn validate_paths(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for p in raw {
        out.push(validate_rel_path(p)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
        [workspace]
        default_targets = ["app"]

        [toolchain]
        cc = "gcc"
        cflags = ["-O2"]

        [target.util]
        kind = "cc_library"
        srcs = ["src/util.c"]
        includes = ["include"]

        [target.app]
        kind = "cc_binary"
        srcs = ["src/main.c"]
        deps = ["util"]

        [target.gen]
        kind = "genrule"
        cmd = "sh gen.sh ${out}"
        inputs = ["gen.sh"]
        outputs = ["gen/config.h"]
    "#;

    #[test]
    fn parses_valid_manifest() {
        let m = Manifest::parse_str(OK).unwrap();
        assert_eq!(m.default_targets, vec!["app"]);
        assert_eq!(m.toolchain.cc, "gcc");
        assert_eq!(m.targets.len(), 3);
        assert_eq!(m.targets["app"].deps, vec!["util"]);
    }

    #[test]
    fn rejects_unknown_dep() {
        let text = r#"
            [target.app]
            kind = "cc_binary"
            srcs = ["a.c"]
            deps = ["nope"]
        "#;
        let err = Manifest::parse_str(text).unwrap_err().to_string();
        assert!(err.contains("unknown dep"), "{err}");
    }

    #[test]
    fn rejects_self_dep() {
        let text = r#"
            [target.app]
            kind = "cc_binary"
            srcs = ["a.c"]
            deps = ["app"]
        "#;
        let err = Manifest::parse_str(text).unwrap_err().to_string();
        assert!(err.contains("depends on itself"), "{err}");
    }

    #[test]
    fn rejects_absolute_src() {
        let text = r#"
            [target.app]
            kind = "cc_binary"
            srcs = ["/etc/passwd"]
        "#;
        assert!(Manifest::parse_str(text).is_err());
    }

    #[test]
    fn rejects_genrule_without_outputs() {
        let text = r#"
            [target.g]
            kind = "genrule"
            cmd = "true"
        "#;
        assert!(Manifest::parse_str(text).is_err());
    }

    #[test]
    fn rejects_unknown_field() {
        let text = r#"
            [target.app]
            kind = "cc_binary"
            srcs = ["a.c"]
            cost_ms = 30
        "#;
        assert!(Manifest::parse_str(text).is_err());
    }

    #[test]
    fn default_targets_fall_back_to_binaries() {
        let text = r#"
            [target.lib]
            kind = "cc_library"
            srcs = ["l.c"]

            [target.tool]
            kind = "cc_binary"
            srcs = ["t.c"]
        "#;
        let m = Manifest::parse_str(text).unwrap();
        assert_eq!(m.default_targets, vec!["tool"]);
    }
}
