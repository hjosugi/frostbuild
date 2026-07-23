use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_SCRIPTS: [&str; 2] = ["test", "typecheck"];
const ROOT_INPUTS: [&str; 5] = [
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    ".npmrc",
    "tsconfig.json",
];
const GENERATED_DIRECTORIES: [&str; 11] = [
    "node_modules",
    "dist",
    "build",
    "coverage",
    "target",
    ".git",
    ".frost",
    ".cache",
    ".turbo",
    ".vite",
    ".next",
];
const NODE_PASS_ENV: [&str; 6] = [
    "NODE_OPTIONS",
    "NODE_PATH",
    "NPM_CONFIG_CACHE",
    "NPM_CONFIG_USERCONFIG",
    "COREPACK_HOME",
    "FORCE_COLOR",
];

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Workspaces {
    Patterns(Vec<String>),
    Object {
        #[serde(default)]
        packages: Vec<String>,
    },
}

impl Workspaces {
    fn patterns(self) -> Vec<String> {
        match self {
            Self::Patterns(patterns) => patterns,
            Self::Object { packages } => packages,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct PackageJson {
    name: Option<String>,
    workspaces: Option<Workspaces>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, Value>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, Value>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, Value>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, Value>,
}

impl PackageJson {
    fn all_dependency_names(&self) -> BTreeSet<String> {
        self.dependencies
            .keys()
            .chain(self.dev_dependencies.keys())
            .chain(self.optional_dependencies.keys())
            .chain(self.peer_dependencies.keys())
            .cloned()
            .collect()
    }

    fn ordered_dependency_names(&self) -> BTreeSet<String> {
        self.dependencies
            .keys()
            .chain(self.optional_dependencies.keys())
            .cloned()
            .collect()
    }
}

#[derive(Debug)]
struct WorkspacePackage {
    name: String,
    scripts: BTreeMap<String, String>,
    all_dependencies: BTreeSet<String>,
    ordered_dependencies: BTreeSet<String>,
    own_inputs: Vec<String>,
}

#[derive(Debug)]
struct ImportPlan {
    manifest: String,
    package_count: usize,
    target_count: usize,
    scripts: Vec<String>,
}

fn read_package_json(path: &Path) -> Result<PackageJson> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))
}

fn normalized_scripts(requested: &[String]) -> Result<Vec<String>> {
    let mut scripts: Vec<String> = if requested.is_empty() {
        DEFAULT_SCRIPTS
            .iter()
            .map(|value| value.to_string())
            .collect()
    } else {
        requested
            .iter()
            .map(|value| value.trim().to_string())
            .collect()
    };
    anyhow::ensure!(
        scripts.iter().all(|script| !script.is_empty()),
        "--script values must not be empty"
    );
    scripts.sort();
    scripts.dedup();
    Ok(scripts)
}

fn validate_workspace_pattern(pattern: &str) -> Result<()> {
    anyhow::ensure!(!pattern.trim().is_empty(), "npm workspace pattern is empty");
    anyhow::ensure!(
        !pattern.starts_with('!'),
        "npm workspace exclusion pattern {pattern:?} is not supported; list the included packages explicitly"
    );
    let path = Path::new(pattern);
    anyhow::ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|component| { matches!(component, Component::Normal(_) | Component::CurDir) }),
        "npm workspace pattern {pattern:?} must stay below the workspace root"
    );
    Ok(())
}

fn workspace_package_files(root: &Path, patterns: Vec<String>) -> Result<Vec<PathBuf>> {
    anyhow::ensure!(
        !patterns.is_empty(),
        "root package.json declares no npm workspace patterns"
    );
    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("failed to resolve workspace {}", root.display()))?;
    let mut files = BTreeSet::new();
    for pattern in patterns {
        validate_workspace_pattern(&pattern)?;
        let package_pattern = canonical_root
            .join(&pattern)
            .join("package.json")
            .to_string_lossy()
            .to_string();
        for entry in glob::glob(&package_pattern)
            .with_context(|| format!("invalid npm workspace pattern {pattern:?}"))?
        {
            let path =
                entry.with_context(|| format!("failed to expand npm workspace {pattern:?}"))?;
            if !path.is_file() {
                continue;
            }
            let package_dir = path
                .parent()
                .context("workspace package.json has no parent directory")?;
            let canonical_dir = std::fs::canonicalize(package_dir).with_context(|| {
                format!("failed to resolve npm workspace {}", package_dir.display())
            })?;
            anyhow::ensure!(
                canonical_dir.starts_with(&canonical_root) && canonical_dir != canonical_root,
                "npm workspace {} escapes or aliases the workspace root",
                package_dir.display()
            );
            files.insert(path);
        }
    }
    anyhow::ensure!(
        !files.is_empty(),
        "npm workspace patterns matched no package.json files"
    );
    Ok(files.into_iter().collect())
}

fn path_string(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("non-UTF-8 npm workspace paths are not supported")?
        .replace('\\', "/"))
}

fn contains_regular_file(path: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_file() {
            return Ok(true);
        }
        if kind.is_dir()
            && !is_generated_directory(&entry.file_name())
            && contains_regular_file(&entry.path())?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_generated_directory(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| GENERATED_DIRECTORIES.contains(&name))
}

fn package_input_patterns(root: &Path, relative: &str) -> Result<Vec<String>> {
    let directory = root.join(relative);
    let escaped = glob::Pattern::escape(relative);
    let mut inputs = vec![format!("{escaped}/*")];
    for entry in std::fs::read_dir(&directory)
        .with_context(|| format!("failed to inspect npm workspace {}", directory.display()))?
    {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() || !kind.is_dir() || is_generated_directory(&entry.file_name()) {
            continue;
        }
        if contains_regular_file(&entry.path())? {
            let name = path_string(Path::new(&entry.file_name()))?;
            inputs.push(format!("{escaped}/{}/**/*", glob::Pattern::escape(&name)));
        }
    }
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

fn discover_packages(root: &Path) -> Result<BTreeMap<String, WorkspacePackage>> {
    let root_package_path = root.join("package.json");
    let root_package = read_package_json(&root_package_path)?;
    let workspaces = root_package.workspaces.with_context(|| {
        format!(
            "{} does not declare npm workspaces",
            root_package_path.display()
        )
    })?;
    let files = workspace_package_files(root, workspaces.patterns())?;
    let canonical_root = std::fs::canonicalize(root)?;
    let mut packages = BTreeMap::new();
    for path in files {
        let package = read_package_json(&path)?;
        let name = package
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .with_context(|| format!("{} has no non-empty package name", path.display()))?
            .to_string();
        let directory = path
            .parent()
            .context("workspace package.json has no parent directory")?;
        let relative = path_string(
            directory
                .strip_prefix(&canonical_root)
                .context("npm workspace escaped the root")?,
        )?;
        let own_inputs = package_input_patterns(&canonical_root, &relative)?;
        let all_dependencies = package.all_dependency_names();
        let ordered_dependencies = package.ordered_dependency_names();
        let workspace = WorkspacePackage {
            name: name.clone(),
            scripts: package.scripts,
            all_dependencies,
            ordered_dependencies,
            own_inputs,
        };
        anyhow::ensure!(
            packages.insert(name.clone(), workspace).is_none(),
            "duplicate npm workspace package name {name:?}"
        );
    }
    Ok(packages)
}

fn target_component(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if !output.is_empty() && !separator {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    output
}

fn target_name(package: &str, script: &str) -> Result<String> {
    let package = target_component(package);
    let script = target_component(script);
    anyhow::ensure!(
        !package.is_empty() && !script.is_empty(),
        "cannot derive a Frost target name from npm package/script"
    );
    Ok(format!("{package}-{script}"))
}

fn workspace_dependency_closure(
    package: &str,
    packages: &BTreeMap<String, WorkspacePackage>,
) -> BTreeSet<String> {
    fn visit(
        name: &str,
        packages: &BTreeMap<String, WorkspacePackage>,
        visited: &mut BTreeSet<String>,
    ) {
        let Some(package) = packages.get(name) else {
            return;
        };
        for dependency in &package.all_dependencies {
            if packages.contains_key(dependency) && visited.insert(dependency.clone()) {
                visit(dependency, packages, visited);
            }
        }
    }
    let mut visited = BTreeSet::new();
    visit(package, packages, &mut visited);
    visited.remove(package);
    visited
}

fn root_inputs(root: &Path) -> Vec<String> {
    let mut inputs = ROOT_INPUTS
        .iter()
        .filter(|path| root.join(path).is_file())
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with("tsconfig") && name.ends_with(".json") {
                inputs.push(name.to_string());
            }
        }
    }
    inputs.sort();
    inputs.dedup();
    inputs
}

fn toml_array(values: &[String]) -> Result<String> {
    Ok(serde_json::to_string(values)?)
}

fn generate_plan(root: &Path, requested: &[String], npm: &Path, node: &Path) -> Result<ImportPlan> {
    let scripts = normalized_scripts(requested)?;
    let packages = discover_packages(root)?;
    let mut target_names = BTreeMap::new();
    let mut used_names = BTreeSet::new();
    for package in packages.values() {
        for script in &scripts {
            if !package.scripts.contains_key(script) {
                continue;
            }
            let name = target_name(&package.name, script)?;
            anyhow::ensure!(
                used_names.insert(name.clone()),
                "npm package/script target names collide after sanitization: {name:?}"
            );
            target_names.insert((package.name.clone(), script.clone()), name);
        }
    }
    anyhow::ensure!(
        !target_names.is_empty(),
        "none of the requested scripts ({}) exist in the discovered npm workspaces",
        scripts.join(", ")
    );

    let npm = npm
        .to_str()
        .context("non-UTF-8 npm executable paths are not supported")?;
    anyhow::ensure!(!npm.trim().is_empty(), "--npm must not be empty");
    let node = node
        .to_str()
        .context("non-UTF-8 Node executable paths are not supported")?;
    anyhow::ensure!(!node.trim().is_empty(), "--node must not be empty");
    let mut manifest = format!(
        "# Generated by `frost import-npm` from npm workspace metadata.\n\
         # Only non-interactive script gates are imported. Build/dev scripts and dynamic\n\
         # output trees remain npm/Vite-owned until an explicit artifact boundary exists.\n\n\
         [toolchain.tools]\nnpm = {}\nnode = {}\n\n",
        serde_json::to_string(npm)?,
        serde_json::to_string(node)?,
    );
    let common_inputs = root_inputs(root);
    for ((package_name, script), name) in &target_names {
        let package = &packages[package_name];
        let mut inputs = common_inputs.clone();
        inputs.extend(package.own_inputs.clone());
        for dependency in workspace_dependency_closure(package_name, &packages) {
            inputs.extend(packages[&dependency].own_inputs.clone());
        }
        inputs.sort();
        inputs.dedup();

        let mut dependencies = package
            .ordered_dependencies
            .iter()
            .filter_map(|dependency| {
                target_names
                    .get(&(dependency.clone(), script.clone()))
                    .cloned()
            })
            .collect::<Vec<_>>();
        dependencies.sort();
        dependencies.dedup();

        manifest.push_str(&format!(
            "[target.{name}]\n\
             kind = \"test\"\n\
             tool = \"npm\"\n\
             args = {}\n\
             inputs = {}\n",
            toml_array(&[
                "run".to_string(),
                script.clone(),
                "--workspace".to_string(),
                package.name.clone(),
            ])?,
            toml_array(&inputs)?,
        ));
        if !dependencies.is_empty() {
            manifest.push_str(&format!("deps = {}\n", toml_array(&dependencies)?));
        }
        manifest.push_str(&format!(
            "env = {{ CI = \"true\" }}\npass_env = {}\nsandbox = false\n\n",
            toml_array(
                &NODE_PASS_ENV
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
            )?
        ));
    }

    let parsed = frostbuild_core::manifest::Manifest::parse_str(&manifest)
        .context("generated npm manifest is invalid")?;
    frostbuild_core::graph::BuildGraph::from_manifest(&parsed)
        .context("generated npm target graph is invalid")?;

    Ok(ImportPlan {
        manifest,
        package_count: packages.len(),
        target_count: target_names.len(),
        scripts,
    })
}

fn write_manifest_without_overwrite(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("generated manifest path has no parent directory")?;
    let mut temporary = None;
    for attempt in 0..1000 {
        let candidate = parent.join(format!(
            ".frost.toml.import-{}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(output) => {
                temporary = Some((candidate, output));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create temporary manifest in {}",
                        parent.display()
                    )
                });
            }
        }
    }

    let (temporary_path, mut output) =
        temporary.context("could not allocate a temporary manifest filename")?;
    if let Err(error) = output.write_all(contents).and_then(|()| output.sync_all()) {
        drop(output);
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error)
            .with_context(|| format!("failed to write generated manifest {}", path.display()));
    }
    drop(output);

    let publish_result = std::fs::hard_link(&temporary_path, path);
    let _ = std::fs::remove_file(&temporary_path);
    publish_result.with_context(|| {
        format!(
            "{} already exists or cannot be created; use --dry-run to inspect without overwriting",
            path.display()
        )
    })
}

pub fn run_import(
    root: &Path,
    scripts: &[String],
    npm: &Path,
    node: &Path,
    dry_run: bool,
) -> Result<i32> {
    let plan = generate_plan(root, scripts, npm, node)?;
    if dry_run {
        print!("{}", plan.manifest);
        return Ok(0);
    }
    let path = root.join("frost.toml");
    write_manifest_without_overwrite(&path, plan.manifest.as_bytes())?;
    println!(
        "frost: wrote {} · {} npm workspaces · {} test gates ({})",
        path.display(),
        plan.package_count,
        plan.target_count,
        plan.scripts.join(", ")
    );
    println!("  dynamic build outputs were not guessed; review inputs, then: frost test --all");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "frost-import-npm-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn imports_workspace_scripts_and_transitive_inputs() {
        let root = fixture("plan");
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces":["packages/*","apps/*"]}"#,
        )
        .unwrap();
        std::fs::write(root.join("package-lock.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("packages/core/src")).unwrap();
        std::fs::write(
            root.join("packages/core/package.json"),
            r#"{"name":"@demo/core","scripts":{"test":"vitest","typecheck":"tsc"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("packages/core/src/index.ts"),
            "export const n = 1;",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("apps/web/src")).unwrap();
        std::fs::write(
            root.join("apps/web/package.json"),
            r#"{"name":"@demo/web","scripts":{"typecheck":"tsc"},"dependencies":{"@demo/core":"*"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("apps/web/src/main.ts"), "import '@demo/core';").unwrap();

        let plan = generate_plan(&root, &[], Path::new("npm"), Path::new("node")).unwrap();
        assert_eq!(plan.package_count, 2);
        assert_eq!(plan.target_count, 3);
        assert!(plan.manifest.contains("[target.demo-web-typecheck]"));
        assert!(plan.manifest.contains("deps = [\"demo-core-typecheck\"]"));
        let web = plan
            .manifest
            .split("[target.demo-web-typecheck]")
            .nth(1)
            .unwrap();
        assert!(web.contains("packages/core/src/**/*"));
        assert!(web.contains("args = [\"run\",\"typecheck\",\"--workspace\",\"@demo/web\"]"));
        assert!(!plan.manifest.contains("kind = \"command\""));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn accepts_object_workspace_syntax_and_selected_scripts() {
        let root = fixture("object");
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces":{"packages":["modules/*"]}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("modules/a/src")).unwrap();
        std::fs::write(
            root.join("modules/a/package.json"),
            r#"{"name":"module-a","scripts":{"lint":"eslint","build":"vite build"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("modules/a/src/index.ts"), "export {};").unwrap();

        let plan = generate_plan(
            &root,
            &["lint".to_string()],
            Path::new("tools/npm"),
            Path::new("tools/node"),
        )
        .unwrap();
        assert_eq!(plan.target_count, 1);
        assert!(plan.manifest.contains("npm = \"tools/npm\""));
        assert!(plan.manifest.contains("node = \"tools/node\""));
        assert!(plan.manifest.contains("[target.module-a-lint]"));
        assert!(!plan.manifest.contains("vite build"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_patterns_that_escape_or_match_nothing() {
        let root = fixture("invalid");
        std::fs::write(root.join("package.json"), r#"{"workspaces":["../*"]}"#).unwrap();
        let error = generate_plan(&root, &[], Path::new("npm"), Path::new("node")).unwrap_err();
        assert!(error.to_string().contains("must stay below"), "{error:#}");
        std::fs::remove_dir_all(root).ok();
    }
}
