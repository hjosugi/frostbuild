use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::validate_rel_path;

pub const MANIFEST_FILE: &str = "frost.toml";

/// The implicit platform backed by the root `[toolchain]` table. Building for
/// it keeps historical output paths and cache identities unchanged.
pub const HOST_PLATFORM: &str = "host";

/// The profile every workspace has without declaring one.
pub const DEFAULT_PROFILE: &str = "debug";

/// The closest candidate to `input`, when one is close enough to be worth
/// suggesting. Turns "unknown X" into "unknown X, did you mean Y".
pub fn closest<'a>(input: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let distance = edit_distance(input, candidate);
        if best.is_none() || best.is_some_and(|(d, _)| distance < d) {
            best = Some((distance, candidate));
        }
    }
    // One edit per three characters: short names need a near-exact match,
    // longer ones tolerate a typo or two. A suggestion that is not actually
    // similar is worse than no suggestion.
    let budget = 1 + input.chars().count() / 3;
    best.filter(|&(distance, _)| distance <= budget)
        .map(|(_, name)| name)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitute.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    CcBinary,
    CcLibrary,
    CcTest,
    Genrule,
    Test,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::CcBinary => "cc_binary",
            TargetKind::CcLibrary => "cc_library",
            TargetKind::CcTest => "cc_test",
            TargetKind::Genrule => "genrule",
            TargetKind::Test => "test",
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
    cxx: Option<String>,
    ar: Option<String>,
    #[serde(default)]
    arflags: Option<Vec<String>>,
    #[serde(default)]
    cflags: Vec<String>,
    #[serde(default)]
    cxxflags: Vec<String>,
    #[serde(default)]
    ldflags: Vec<String>,
}

/// A named build platform: a toolchain overlay for cross/device builds.
/// Unset drivers inherit from the root `[toolchain]`; flags are appended
/// after the root toolchain's flags; `sysroot` expands to `--sysroot=`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlatform {
    cc: Option<String>,
    cxx: Option<String>,
    ar: Option<String>,
    #[serde(default)]
    arflags: Option<Vec<String>>,
    sysroot: Option<String>,
    #[serde(default)]
    cflags: Vec<String>,
    #[serde(default)]
    cxxflags: Vec<String>,
    #[serde(default)]
    ldflags: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    #[serde(default)]
    cflags: Vec<String>,
    #[serde(default)]
    cxxflags: Vec<String>,
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
    /// Tests may opt out of sandboxing when they intentionally inspect the host.
    sandbox: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[serde(default)]
    workspace: RawWorkspace,
    #[serde(default)]
    toolchain: RawToolchain,
    #[serde(default)]
    platform: BTreeMap<String, RawPlatform>,
    #[serde(default)]
    profile: BTreeMap<String, RawProfile>,
    #[serde(default)]
    target: BTreeMap<String, RawTarget>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Toolchain {
    pub cc: String,
    pub cxx: String,
    pub ar: String,
    pub arflags: Vec<String>,
    pub cflags: Vec<String>,
    pub cxxflags: Vec<String>,
    pub ldflags: Vec<String>,
}

/// Toolchain overlay declared as `[platform.<name>]`; see `RawPlatform`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Platform {
    pub cc: Option<String>,
    pub cxx: Option<String>,
    pub ar: Option<String>,
    pub arflags: Option<Vec<String>>,
    pub sysroot: Option<String>,
    pub cflags: Vec<String>,
    pub cxxflags: Vec<String>,
    pub ldflags: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    pub cflags: Vec<String>,
    pub cxxflags: Vec<String>,
    pub ldflags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub sandbox: bool,
    /// Package directory relative to the workspace root (empty for root).
    pub package: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub default_targets: Vec<String>,
    pub toolchain: Toolchain,
    pub platforms: BTreeMap<String, Platform>,
    pub profiles: BTreeMap<String, Profile>,
    pub targets: BTreeMap<String, Target>,
    /// Manifests which contributed to this workspace, used by graph caching.
    pub manifest_paths: Vec<String>,
}

impl Manifest {
    pub fn load(workspace_root: &Path) -> Result<Self> {
        let path = workspace_root.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&path).map_err(|_| {
            anyhow::anyhow!(
                "no {MANIFEST_FILE} in {}. run `frost init` to write one, \
                 or `-C <dir>` to build somewhere else",
                workspace_root.display()
            )
        })?;
        let raw: RawManifest =
            toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        let root_has_workspace = toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|v| v.get("workspace").cloned())
            .is_some();
        let mut manifest = Self::from_raw_unvalidated(raw)?;
        for target in manifest.targets.values_mut() {
            target.deps = target
                .deps
                .iter()
                .map(|dep| dep.strip_prefix("//:").unwrap_or(dep).to_string())
                .collect();
        }
        manifest.default_targets = manifest
            .default_targets
            .iter()
            .map(|name| name.strip_prefix("//:").unwrap_or(name).to_string())
            .collect();
        manifest.manifest_paths.push(MANIFEST_FILE.to_string());
        expand_manifest_paths(&mut manifest, workspace_root, "")?;

        if root_has_workspace {
            let mut packages = discover_package_manifests(workspace_root)?;
            packages.sort();
            for rel in packages {
                let package = rel
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_str()
                    .context("non-UTF-8 package path is not supported")?
                    .replace('\\', "/");
                if package.is_empty() {
                    continue;
                }
                let package_text = std::fs::read_to_string(workspace_root.join(&rel))
                    .with_context(|| format!("failed to read {}", rel.display()))?;
                let package_raw: RawManifest = toml::from_str(&package_text)
                    .with_context(|| format!("failed to parse {}", rel.display()))?;
                let mut child = Self::from_raw_unvalidated(package_raw)?;
                expand_manifest_paths(&mut child, workspace_root, &package)?;
                for (local, mut target) in child.targets {
                    let canonical = format!("//{package}:{local}");
                    target.name = canonical.clone();
                    target.package = package.clone();
                    target.deps = target
                        .deps
                        .iter()
                        .map(|dep| resolve_label(dep, &package))
                        .collect();
                    if manifest.targets.insert(canonical.clone(), target).is_some() {
                        bail!("duplicate target label {canonical:?}");
                    }
                }
                manifest.manifest_paths.push(
                    rel.to_str()
                        .context("non-UTF-8 manifest path is not supported")?
                        .replace('\\', "/"),
                );
            }
        }
        validate_dependencies(&manifest.targets)?;
        validate_default_targets(&manifest)?;
        Ok(manifest)
    }

    pub fn parse_str(text: &str) -> Result<Self> {
        let raw: RawManifest = toml::from_str(text).context("failed to parse manifest")?;
        Self::from_raw(raw)
    }

    /// Resolves the effective toolchain for a platform: the root `[toolchain]`
    /// for `host`, otherwise that toolchain with the `[platform.<name>]`
    /// overlay applied (driver overrides, appended flags, sysroot expansion).
    pub fn toolchain_for(&self, platform: &str) -> Result<Toolchain> {
        if platform == HOST_PLATFORM {
            return Ok(self.toolchain.clone());
        }
        let Some(spec) = self.platforms.get(platform) else {
            let known: Vec<&str> = self.platforms.keys().map(String::as_str).collect();
            if let Some(hint) = closest(platform, known.iter().copied()) {
                bail!("unknown platform {platform:?}. did you mean {hint:?}?");
            }
            bail!(
                "unknown platform {platform:?}{}",
                if known.is_empty() {
                    ". this workspace declares no [platform.*] sections".to_string()
                } else {
                    format!(". declared platforms: {}", known.join(", "))
                }
            );
        };
        let base = &self.toolchain;
        let mut resolved = Toolchain {
            cc: spec.cc.clone().unwrap_or_else(|| base.cc.clone()),
            cxx: spec.cxx.clone().unwrap_or_else(|| base.cxx.clone()),
            ar: spec.ar.clone().unwrap_or_else(|| base.ar.clone()),
            arflags: spec.arflags.clone().unwrap_or_else(|| base.arflags.clone()),
            cflags: base.cflags.clone(),
            cxxflags: base.cxxflags.clone(),
            ldflags: base.ldflags.clone(),
        };
        if let Some(sysroot) = &spec.sysroot {
            let flag = format!("--sysroot={sysroot}");
            resolved.cflags.push(flag.clone());
            resolved.ldflags.push(flag);
        }
        resolved.cflags.extend(spec.cflags.iter().cloned());
        resolved.cxxflags.extend(spec.cxxflags.iter().cloned());
        resolved.ldflags.extend(spec.ldflags.iter().cloned());
        Ok(resolved)
    }

    fn from_raw(raw: RawManifest) -> Result<Self> {
        let manifest = Self::from_raw_unvalidated(raw)?;
        if manifest.targets.is_empty() {
            bail!("manifest declares no [target.*] sections");
        }
        validate_dependencies(&manifest.targets)?;
        validate_default_targets(&manifest)?;
        Ok(manifest)
    }

    fn from_raw_unvalidated(raw: RawManifest) -> Result<Self> {
        let mut targets = BTreeMap::new();
        for (name, spec) in raw.target {
            let target =
                build_target(&name, spec).with_context(|| format!("invalid target {name:?}"))?;
            targets.insert(name, target);
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
            raw.workspace.default_targets
        };

        let mut platforms = BTreeMap::new();
        for (name, spec) in raw.platform {
            if name == HOST_PLATFORM {
                bail!("platform name {HOST_PLATFORM:?} is reserved for the root [toolchain]");
            }
            if !valid_target_name(&name) {
                bail!("platform name must match [A-Za-z0-9_-]+, got {name:?}");
            }
            platforms.insert(
                name,
                Platform {
                    cc: spec.cc,
                    cxx: spec.cxx,
                    ar: spec.ar,
                    arflags: spec.arflags,
                    sysroot: spec.sysroot,
                    cflags: spec.cflags,
                    cxxflags: spec.cxxflags,
                    ldflags: spec.ldflags,
                },
            );
        }

        Ok(Self {
            default_targets,
            toolchain: Toolchain {
                cc: raw.toolchain.cc.unwrap_or_else(|| "cc".to_string()),
                cxx: raw.toolchain.cxx.unwrap_or_else(|| "c++".to_string()),
                ar: raw.toolchain.ar.unwrap_or_else(|| "ar".to_string()),
                arflags: raw
                    .toolchain
                    .arflags
                    .unwrap_or_else(|| vec!["rcsD".to_string()]),
                cflags: raw.toolchain.cflags,
                cxxflags: raw.toolchain.cxxflags,
                ldflags: raw.toolchain.ldflags,
            },
            platforms,
            profiles: raw
                .profile
                .into_iter()
                .map(|(name, p)| {
                    (
                        name,
                        Profile {
                            cflags: p.cflags,
                            cxxflags: p.cxxflags,
                            ldflags: p.ldflags,
                        },
                    )
                })
                .collect(),
            targets,
            manifest_paths: Vec::new(),
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
        TargetKind::CcBinary | TargetKind::CcLibrary | TargetKind::CcTest => {
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
        TargetKind::Genrule | TargetKind::Test => {
            if spec.cmd.as_deref().map(str::trim).unwrap_or("").is_empty() {
                bail!("genrule requires a non-empty cmd");
            }
            if spec.kind == TargetKind::Genrule && outputs.is_empty() {
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
        sandbox: spec.sandbox.unwrap_or(true),
        package: String::new(),
    })
}

fn discover_package_manifests(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(name.to_str(), Some(".git" | ".frost" | "target")) {
                continue;
            }
            let ty = entry.file_type()?;
            if ty.is_dir() && !ty.is_symlink() {
                walk(root, &entry.path(), out)?;
            } else if ty.is_file() && name == MANIFEST_FILE {
                out.push(entry.path().strip_prefix(root).unwrap().to_path_buf());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

fn resolve_label(raw: &str, package: &str) -> String {
    if let Some(root) = raw.strip_prefix("//:") {
        root.to_string()
    } else if raw.starts_with("//") {
        raw.to_string()
    } else {
        format!("//{package}:{}", raw.trim_start_matches(':'))
    }
}

fn prefix_path(package: &str, path: &str) -> String {
    if package.is_empty() {
        path.to_string()
    } else {
        format!("{package}/{path}")
    }
}

fn has_glob(path: &str) -> bool {
    path.bytes().any(|b| matches!(b, b'*' | b'?' | b'['))
}

fn expand_paths(root: &Path, package: &str, paths: &[String]) -> Result<Vec<String>> {
    let mut expanded = Vec::new();
    let mut ignore_builder = ignore::gitignore::GitignoreBuilder::new(root);
    for file in [".gitignore", ".frostignore"] {
        let path = root.join(file);
        if path.exists() {
            ignore_builder.add(path);
        }
    }
    let ignored = ignore_builder.build()?;
    for path in paths {
        let rel = prefix_path(package, path);
        if !has_glob(path) {
            expanded.push(rel);
            continue;
        }
        let pattern = root.join(&rel).to_string_lossy().to_string();
        let matches = glob::glob(&pattern).with_context(|| format!("invalid glob {path:?}"))?;
        let before = expanded.len();
        for item in matches {
            let item = item.with_context(|| format!("failed to expand glob {path:?}"))?;
            if !item.is_file() {
                continue;
            }
            let relative = item
                .strip_prefix(root)
                .context("glob escaped workspace")?
                .to_str()
                .context("non-UTF-8 source path is not supported")?
                .replace('\\', "/");
            if !relative.starts_with(".frost/")
                && !relative.starts_with(".git/")
                && !ignored
                    .matched_path_or_any_parents(&item, false)
                    .is_ignore()
            {
                expanded.push(relative);
            }
        }
        // A pattern that matches nothing is a typo far more often than an
        // intent, and the damage shows up somewhere else: a cc_library whose
        // srcs vanished still archives, and the build fails at the link with
        // a message about symbols rather than about the glob. Say it here,
        // where the cause is.
        if expanded.len() == before {
            bail!("{path:?} matched no files");
        }
    }
    expanded.sort();
    expanded.dedup();
    Ok(expanded)
}

fn expand_manifest_paths(manifest: &mut Manifest, root: &Path, package: &str) -> Result<()> {
    for (name, target) in manifest.targets.iter_mut() {
        target.package = package.to_string();
        target.srcs = expand_paths(root, package, &target.srcs)
            .with_context(|| format!("target {name:?} srcs"))?;
        target.inputs = expand_paths(root, package, &target.inputs)
            .with_context(|| format!("target {name:?} inputs"))?;
        target.includes = target
            .includes
            .iter()
            .map(|p| prefix_path(package, p))
            .collect();
        target.outputs = target
            .outputs
            .iter()
            .map(|p| prefix_path(package, p))
            .collect();
    }
    Ok(())
}

fn validate_dependencies(targets: &BTreeMap<String, Target>) -> Result<()> {
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
    Ok(())
}

fn validate_default_targets(manifest: &Manifest) -> Result<()> {
    for name in &manifest.default_targets {
        if !manifest.targets.contains_key(name) {
            bail!("workspace.default_targets names unknown target {name:?}");
        }
    }
    Ok(())
}

fn validate_paths(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for p in raw {
        out.push(validate_rel_path(p)?);
    }
    Ok(out)
}

/// A starter manifest for a directory that has C or C++ sources but no
/// `frost.toml` yet.
///
/// Deliberately shallow: it reports what it found and writes the smallest
/// manifest that builds it, rather than inferring a target layout the author
/// did not ask for. Anything beyond one binary and one library is a decision
/// the author should make in the file, where it is visible.
pub struct Scaffold {
    pub manifest: String,
    /// What the scan saw, for the caller to print.
    pub summary: Vec<String>,
}

const SOURCE_EXTENSIONS: [&str; 6] = ["c", "cc", "cpp", "cxx", "C", "c++"];

pub fn scaffold(root: &Path) -> Result<Scaffold> {
    let mut sources: Vec<String> = Vec::new();
    collect_sources(root, root, &mut sources, 0)?;
    sources.sort();
    if sources.is_empty() {
        bail!(
            "no C or C++ sources under {}. frost builds C and C++; write \
             {MANIFEST_FILE} by hand for anything else",
            root.display()
        );
    }

    let has_include = root.join("include").is_dir();
    // A file defining main is the binary; everything else is library code.
    let entry = sources
        .iter()
        .find(|path| defines_main(&root.join(path)))
        .cloned();

    let mut summary = vec![format!("{} source file(s)", sources.len())];
    let mut manifest = String::from("[workspace]\n");

    let (binary_srcs, library_srcs): (Vec<String>, Vec<String>) = match &entry {
        Some(entry) => {
            summary.push(format!("entry point: {entry}"));
            (
                vec![entry.clone()],
                sources.iter().filter(|s| *s != entry).cloned().collect(),
            )
        }
        None => {
            summary.push("no main() found, so everything becomes a library".into());
            (Vec::new(), sources.clone())
        }
    };
    if has_include {
        summary.push("include/ used as the exported header directory".into());
    }

    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize_name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "app".to_string());
    let lib_name = format!("{name}_lib");

    let default_target = if binary_srcs.is_empty() {
        // Nothing defines main, so a library is the only honest default.
        name.clone()
    } else {
        name.clone()
    };
    manifest.push_str(&format!("default_targets = [\"{default_target}\"]\n\n"));
    manifest.push_str("[toolchain]\ncc = \"cc\"\ncxx = \"c++\"\ncflags = [\"-Wall\"]\n\n");

    if binary_srcs.is_empty() {
        manifest.push_str(&format!("[target.{name}]\nkind = \"cc_library\"\n"));
        manifest.push_str(&format!("srcs = {}\n", toml_array(&library_srcs)));
        if has_include {
            manifest.push_str("includes = [\"include\"]\n");
        }
    } else {
        if !library_srcs.is_empty() {
            manifest.push_str(&format!("[target.{lib_name}]\nkind = \"cc_library\"\n"));
            manifest.push_str(&format!("srcs = {}\n", toml_array(&library_srcs)));
            if has_include {
                manifest.push_str("includes = [\"include\"]\n");
            }
            manifest.push('\n');
        }
        manifest.push_str(&format!("[target.{name}]\nkind = \"cc_binary\"\n"));
        manifest.push_str(&format!("srcs = {}\n", toml_array(&binary_srcs)));
        if !library_srcs.is_empty() {
            manifest.push_str(&format!("deps = [\"{lib_name}\"]\n"));
        } else if has_include {
            manifest.push_str("includes = [\"include\"]\n");
        }
    }

    Ok(Scaffold { manifest, summary })
}

fn collect_sources(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) -> Result<()> {
    if depth > 8 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || matches!(name.as_ref(), "target" | "build" | "node_modules") {
            continue;
        }
        let ty = entry.file_type()?;
        let path = entry.path();
        if ty.is_dir() && !ty.is_symlink() {
            collect_sources(root, &path, out, depth + 1)?;
        } else if ty.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e))
        {
            if let Ok(relative) = path.strip_prefix(root) {
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

/// Whether a file looks like it defines `main`. A scaffold is allowed to be
/// wrong here — the author reads the file it wrote — so this stays a textual
/// check rather than pulling in a parser.
fn defines_main(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines()
        .map(str::trim_start)
        .filter(|line| !line.starts_with("//") && !line.starts_with('*'))
        .any(|line| line.contains("main(") && (line.contains("int ") || line.starts_with("main(")))
}

fn sanitize_name(raw: &str) -> String {
    let name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    name.trim_matches('-').to_string()
}

fn toml_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
    format!("[{}]", items.join(", "))
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
    fn platform_overlay_resolves_toolchain() {
        let text = r#"
            [toolchain]
            cc = "gcc"
            cxx = "g++"
            cflags = ["-O2"]

            [platform.aarch64]
            cc = "aarch64-linux-gnu-gcc"
            sysroot = "sysroots/aarch64"
            cflags = ["-mcpu=cortex-a53"]
            ldflags = ["-static"]

            [target.app]
            kind = "cc_binary"
            srcs = ["a.c"]
        "#;
        let m = Manifest::parse_str(text).unwrap();

        let host = m.toolchain_for(HOST_PLATFORM).unwrap();
        assert_eq!(host.cc, "gcc");
        assert_eq!(host.cflags, vec!["-O2"]);
        assert_eq!(host.arflags, vec!["rcsD"]);

        let cross = m.toolchain_for("aarch64").unwrap();
        assert_eq!(cross.cc, "aarch64-linux-gnu-gcc");
        assert_eq!(cross.cxx, "g++", "unset drivers inherit the root toolchain");
        assert_eq!(
            cross.cflags,
            vec!["-O2", "--sysroot=sysroots/aarch64", "-mcpu=cortex-a53"]
        );
        assert_eq!(cross.ldflags, vec!["--sysroot=sysroots/aarch64", "-static"]);
    }

    #[test]
    fn unknown_platform_errors_with_candidates() {
        let m = Manifest::parse_str(
            r#"
            [platform.rv64]
            cc = "riscv64-elf-gcc"

            [target.app]
            kind = "cc_binary"
            srcs = ["a.c"]
            "#,
        )
        .unwrap();
        let err = m.toolchain_for("nope").unwrap_err().to_string();
        assert!(err.contains("unknown platform"), "{err}");
        assert!(err.contains("rv64"), "{err}");
    }

    #[test]
    fn suggests_only_genuinely_close_names() {
        assert_eq!(closest("relase", ["debug", "release"]), Some("release"));
        assert_eq!(closest("aarch65", ["aarch64", "riscv"]), Some("aarch64"));
        assert_eq!(closest("ap", ["app", "lib"]), Some("app"));
        // A short name that resembles nothing gets no suggestion: a wrong
        // hint sends the reader down the wrong path.
        assert_eq!(closest("zzz", ["debug", "release"]), None);
        assert_eq!(closest("windows", ["aarch64"]), None);
        assert_eq!(closest("anything", []), None);
    }

    #[test]
    fn rejects_reserved_host_platform() {
        let text = r#"
            [platform.host]
            cc = "gcc"

            [target.app]
            kind = "cc_binary"
            srcs = ["a.c"]
        "#;
        let err = Manifest::parse_str(text).unwrap_err().to_string();
        assert!(err.contains("reserved"), "{err}");
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
