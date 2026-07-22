use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use notify::{RecursiveMode, Watcher};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename = "query")]
struct XmlQuery {
    #[serde(rename = "rule", default)]
    rules: Vec<XmlRule>,
}

#[derive(Debug, Deserialize)]
struct XmlRule {
    #[serde(rename = "@class")]
    class: String,
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "list", default)]
    lists: Vec<XmlList>,
    #[serde(rename = "string", default)]
    strings: Vec<XmlValue>,
    #[serde(rename = "int", default)]
    ints: Vec<XmlValue>,
    #[serde(rename = "boolean", default)]
    booleans: Vec<XmlValue>,
}

#[derive(Debug, Deserialize)]
struct XmlList {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "label", default)]
    labels: Vec<XmlValue>,
    #[serde(rename = "string", default)]
    strings: Vec<XmlValue>,
}

#[derive(Debug, Deserialize)]
struct XmlValue {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@value")]
    value: String,
}

#[derive(Debug, Clone)]
struct BazelLabel {
    package: String,
    name: String,
}

#[derive(Debug)]
struct ImportPlan {
    files: BTreeMap<PathBuf, String>,
    rule_count: usize,
    package_count: usize,
}

impl XmlRule {
    fn kind(&self) -> &str {
        self.class.strip_suffix(" rule").unwrap_or(&self.class)
    }

    fn list(&self, name: &str) -> Vec<String> {
        self.lists
            .iter()
            .find(|list| list.name == name)
            .map(|list| {
                list.labels
                    .iter()
                    .chain(&list.strings)
                    .map(|value| value.value.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn scalar(&self, name: &str) -> Option<&str> {
        self.strings
            .iter()
            .chain(&self.ints)
            .chain(&self.booleans)
            .find(|value| value.name == name)
            .map(|value| value.value.as_str())
    }

    fn truthy(&self, name: &str) -> bool {
        matches!(self.scalar(name), Some("1" | "true" | "True"))
    }
}

fn parse_label(value: &str) -> Result<BazelLabel> {
    if value.starts_with('@') {
        bail!("external repository label {value:?} is not importable; vendor or wrap it first");
    }
    let body = value
        .strip_prefix("//")
        .with_context(|| format!("unsupported non-canonical Bazel label {value:?}"))?;
    let (package, name) = if let Some((package, name)) = body.split_once(':') {
        (package, name)
    } else {
        let name = body.rsplit('/').next().unwrap_or(body);
        (body, name)
    };
    anyhow::ensure!(!name.is_empty(), "Bazel label {value:?} has no target name");
    Ok(BazelLabel {
        package: package.to_string(),
        name: name.to_string(),
    })
}

fn frost_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn frost_label(label: &BazelLabel, names: &BTreeMap<String, String>) -> Result<String> {
    let canonical = format!("//{}:{}", label.package, label.name);
    let name = names
        .get(&canonical)
        .with_context(|| format!("dependency {canonical:?} is not a supported imported rule"))?;
    Ok(if label.package.is_empty() {
        name.clone()
    } else {
        format!("//{}:{name}", label.package)
    })
}

fn source_path(value: &str, package: &str, all_rules: &BTreeSet<String>) -> Result<String> {
    anyhow::ensure!(
        !all_rules.contains(value),
        "srcs entry {value:?} is another rule (filegroup/generated sources are not yet importable)"
    );
    let label = parse_label(value)?;
    anyhow::ensure!(
        label.package == package,
        "source {value:?} is outside package //{package}; cross-package source files are unsupported"
    );
    let supported = ["c", "cc", "cpp", "cxx", "C", "c++"];
    let extension = Path::new(&label.name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    anyhow::ensure!(
        supported.contains(&extension),
        "source {value:?} has unsupported extension {extension:?}; Frost's Bazel importer currently accepts native C/C++ only"
    );
    Ok(label.name)
}

fn ensure_supported_attributes(rule: &XmlRule) -> Result<()> {
    for name in [
        "additional_linker_inputs",
        "data",
        "defines",
        "include_prefix",
        "nocopts",
        "strip_include_prefix",
        "win_def_file",
    ] {
        if !rule.list(name).is_empty()
            || rule
                .scalar(name)
                .is_some_and(|value| !value.is_empty() && value != "0" && value != "false")
        {
            bail!(
                "{} {} uses unsupported attribute {name:?}; import stopped before writing partial semantics",
                rule.kind(),
                rule.name
            );
        }
    }
    if rule.truthy("alwayslink") || rule.truthy("linkshared") {
        bail!(
            "{} {} uses alwayslink/linkshared semantics that Frost's native subset cannot preserve",
            rule.kind(),
            rule.name
        );
    }
    if rule.list("copts").iter().any(|flag| flag.contains("$("))
        || rule.list("linkopts").iter().any(|flag| flag.contains("$("))
    {
        bail!(
            "{} {} uses Bazel make-variable expansion in flags; resolve it before importing",
            rule.kind(),
            rule.name
        );
    }
    Ok(())
}

fn toml_array(values: &[String]) -> Result<String> {
    Ok(serde_json::to_string(values)?)
}

fn render_rule(
    rule: &XmlRule,
    names: &BTreeMap<String, String>,
    all_rules: &BTreeSet<String>,
) -> Result<(String, String, String)> {
    ensure_supported_attributes(rule)?;
    let label = parse_label(&rule.name)?;
    let name = names[&rule.name].clone();
    let kind = match rule.kind() {
        "cc_library" => "cc_library",
        "cc_binary" => "cc_binary",
        "cc_test" => "cc_test",
        other => bail!("unsupported Bazel rule {other:?}"),
    };
    let sources = rule
        .list("srcs")
        .iter()
        .map(|source| source_path(source, &label.package, all_rules))
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        !sources.is_empty(),
        "{} {} is header/dep-only; Frost native C/C++ targets currently require a source",
        rule.kind(),
        rule.name
    );
    let mut dependency_values = rule.list("deps");
    dependency_values.extend(rule.list("implementation_deps"));
    dependency_values.sort();
    dependency_values.dedup();
    let dependencies = dependency_values
        .iter()
        .map(|dependency| parse_label(dependency))
        .map(|dependency| dependency.and_then(|label| frost_label(&label, names)))
        .collect::<Result<Vec<_>>>()?;
    let includes = rule.list("includes");
    let mut cflags = rule.list("copts");
    cflags.extend(
        rule.list("local_defines")
            .into_iter()
            .map(|define| format!("-D{define}")),
    );
    let linkopts = rule.list("linkopts");
    if kind == "cc_library" && !linkopts.is_empty() {
        bail!(
            "cc_library {} has linkopts, which Frost cannot yet export transitively",
            rule.name
        );
    }

    let mut rendered = format!(
        "[target.{name}]\nkind = {kind:?}\nsrcs = {}\n",
        toml_array(&sources)?
    );
    if !dependencies.is_empty() {
        rendered.push_str(&format!("deps = {}\n", toml_array(&dependencies)?));
    }
    if !includes.is_empty() {
        rendered.push_str(&format!("includes = {}\n", toml_array(&includes)?));
    }
    if !cflags.is_empty() {
        rendered.push_str(&format!("cflags = {}\n", toml_array(&cflags)?));
    }
    if !linkopts.is_empty() {
        rendered.push_str(&format!("ldflags = {}\n", toml_array(&linkopts)?));
    }
    rendered.push('\n');
    Ok((label.package, name, rendered))
}

fn generate_plan(xml: &str, bazel_version: &str) -> Result<ImportPlan> {
    let query: XmlQuery = quick_xml::de::from_str(xml).context("invalid Bazel query XML")?;
    let supported = query
        .rules
        .iter()
        .filter(|rule| matches!(rule.kind(), "cc_library" | "cc_binary" | "cc_test"))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !supported.is_empty(),
        "query contains no supported cc_library, cc_binary or cc_test rules"
    );
    let all_rules = query
        .rules
        .iter()
        .map(|rule| rule.name.clone())
        .collect::<BTreeSet<_>>();
    let mut names = BTreeMap::new();
    let mut package_names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for rule in &supported {
        let label = parse_label(&rule.name)?;
        let name = frost_name(&label.name);
        anyhow::ensure!(
            !name.is_empty(),
            "cannot sanitize Bazel target {}",
            rule.name
        );
        anyhow::ensure!(
            package_names
                .entry(label.package.clone())
                .or_default()
                .insert(name.clone()),
            "Bazel target names collide after Frost sanitization in //{}: {:?}",
            label.package,
            label.name
        );
        names.insert(rule.name.clone(), name);
    }

    let mut packages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut defaults = Vec::new();
    for rule in supported {
        let is_default = matches!(rule.kind(), "cc_binary" | "cc_test");
        let (package, name, rendered) = render_rule(rule, &names, &all_rules)?;
        if is_default {
            defaults.push(if package.is_empty() {
                name.clone()
            } else {
                format!("//{package}:{name}")
            });
        }
        packages.entry(package).or_default().push(rendered);
    }
    if defaults.is_empty() {
        defaults.extend(names.values().cloned());
    }
    defaults.sort();

    let mut files = BTreeMap::new();
    let root_rules = packages.remove("").unwrap_or_default();
    let mut root_manifest = format!(
        "# Generated by `frost import-bazel` from {bazel_version}.\n\
         # Review toolchain flags and every noted unsupported Bazel feature before deleting BUILD files.\n\n\
         [workspace]\ndefault_targets = {}\n\n\
         [toolchain]\ncc = \"cc\"\ncxx = \"c++\"\ncflags = [\"-Wall\"]\n\n\
         [profile.debug]\ncflags = [\"-O0\", \"-g\"]\n\n\
         [profile.release]\ncflags = [\"-O3\", \"-DNDEBUG\"]\n\n",
        toml_array(&defaults)?
    );
    root_manifest.push_str(&root_rules.concat());
    files.insert(PathBuf::from("frost.toml"), root_manifest);
    for (package, rules) in packages {
        files.insert(
            PathBuf::from(package).join("frost.toml"),
            format!(
                "# Generated by `frost import-bazel` from {bazel_version}. Review before use.\n\n{}",
                rules.concat()
            ),
        );
    }
    Ok(ImportPlan {
        package_count: files.len(),
        rule_count: names.len(),
        files,
    })
}

fn command_output(root: &Path, bazel: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(bazel)
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to execute {}", bazel.display()))?;
    if !output.status.success() {
        bail!(
            "{} {} failed ({})\n{}",
            bazel.display(),
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ensure_configuration_free(expanded_build: &str) -> Result<()> {
    if expanded_build.contains("select(") {
        bail!(
            "Bazel query contains select(); import is configuration-dependent and was stopped before writing files"
        );
    }
    Ok(())
}

fn resolve_bazel(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_path_buf());
    }
    if let Some(configured) = std::env::var_os("BAZEL_BIN") {
        return Ok(PathBuf::from(configured));
    }
    super::find_on_path("bazel")
        .or_else(|| super::find_on_path("bazelisk"))
        .context("Bazel was not found; pass --bazel PATH or set BAZEL_BIN")
}

fn bazel_build(root: &Path, bazel: &Path, target: &str, bazel_args: &[String]) -> Result<bool> {
    let status = Command::new(bazel)
        .arg("build")
        .args(bazel_args)
        .arg(target)
        .current_dir(root)
        .status()
        .with_context(|| format!("failed to execute {} build", bazel.display()))?;
    Ok(status.success())
}

fn spawn_bazel_run(
    root: &Path,
    bazel: &Path,
    target: &str,
    bazel_args: &[String],
    program_args: &[String],
) -> Result<Child> {
    let mut command = Command::new(bazel);
    command
        .arg("run")
        .args(bazel_args)
        .arg(target)
        .arg("--")
        .args(program_args)
        .current_dir(root);
    super::configure_dev_command(&mut command);
    command
        .spawn()
        .with_context(|| format!("failed to execute {} run", bazel.display()))
}

fn relevant_bazel_watch_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let first = relative.components().next()?.as_os_str().to_string_lossy();
    if relative.as_os_str().is_empty()
        || first == ".git"
        || first == ".frost"
        || first.starts_with("bazel-")
    {
        return None;
    }
    Some(relative.to_path_buf())
}

/// Keep the last successfully launched Bazel target alive while an
/// incremental rebuild is attempted, replacing it only after Bazel reports a
/// successful build. This deliberately uses Bazel's own graph/server/cache;
/// no BUILD-file semantics are reimplemented here.
pub fn run_dev(
    root: &Path,
    target: &str,
    bazel: Option<&Path>,
    debounce: Duration,
    bazel_args: &[String],
    program_args: &[String],
) -> Result<i32> {
    anyhow::ensure!(!target.trim().is_empty(), "Bazel target must not be empty");
    let bazel = resolve_bazel(bazel)?;
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    println!(
        "frost: bazel dev · {target} · debounce {} ms",
        debounce.as_millis()
    );
    println!("|-- initial bazel build");
    let mut child = None;
    if bazel_build(root, &bazel, target, bazel_args)? {
        let running = spawn_bazel_run(root, &bazel, target, bazel_args, program_args)?;
        println!("|   `-- target started · pid {}", running.id());
        child = Some(running);
    } else {
        eprintln!("|   `-- build failed; watching for a fix");
    }
    println!("`-- ready · Ctrl-C stops");

    let mut change_set = 0usize;
    while !frostbuild_exec::was_cancelled() {
        if let Some(running) = child.as_mut() {
            if let Some(status) = running.try_wait()? {
                println!("frost: Bazel target exited · {status}");
                child = None;
            }
        }

        let first = match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => {
                eprintln!("frost: Bazel watch error: {error}");
                continue;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("Bazel filesystem watcher stopped"),
        };
        let mut changed = if super::watch_event_changes_files(&first.kind) {
            first
                .paths
                .iter()
                .filter_map(|path| relevant_bazel_watch_path(root, path))
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let mut deadline = Instant::now() + debounce;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match receiver.recv_timeout(remaining) {
                Ok(Ok(event)) => {
                    let before = changed.len();
                    if super::watch_event_changes_files(&event.kind) {
                        changed.extend(
                            event
                                .paths
                                .iter()
                                .filter_map(|path| relevant_bazel_watch_path(root, path)),
                        );
                    }
                    if changed.len() > before {
                        deadline = Instant::now() + debounce;
                    }
                }
                Ok(Err(error)) => eprintln!("frost: Bazel watch error: {error}"),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("Bazel filesystem watcher stopped")
                }
            }
        }
        if changed.is_empty() {
            continue;
        }

        change_set += 1;
        println!(
            "frost: Bazel change #{change_set} · {} path{}",
            changed.len(),
            if changed.len() == 1 { "" } else { "s" }
        );
        for (index, path) in changed.iter().take(4).enumerate() {
            let last = index + 1 == changed.len().min(4);
            println!("{} {}", if last { "`--" } else { "|--" }, path.display());
        }
        if changed.len() > 4 {
            println!("    … and {} more", changed.len() - 4);
        }

        if bazel_build(root, &bazel, target, bazel_args)? {
            super::stop_dev_process(&mut child);
            match spawn_bazel_run(root, &bazel, target, bazel_args, program_args) {
                Ok(running) => {
                    println!("`-- Bazel target restarted · pid {}", running.id());
                    child = Some(running);
                }
                Err(error) => eprintln!("`-- Bazel target restart failed: {error:#}"),
            }
        } else {
            eprintln!("`-- Bazel build failed; keeping the last successful target process");
        }
    }
    super::stop_dev_process(&mut child);
    println!("frost: bazel dev stopped");
    Ok(130)
}

pub fn run_import(
    root: &Path,
    expression: &str,
    bazel: Option<&Path>,
    dry_run: bool,
) -> Result<i32> {
    let bazel = resolve_bazel(bazel)?;
    let version = command_output(root, &bazel, &["--version"])?
        .trim()
        .to_string();
    let expanded = command_output(
        root,
        &bazel,
        &["query", "--noshow_progress", "--output=build", expression],
    )?;
    ensure_configuration_free(&expanded)?;
    let xml = command_output(
        root,
        &bazel,
        &[
            "query",
            "--noshow_progress",
            "--noimplicit_deps",
            "--noxml:default_values",
            "--output=xml",
            expression,
        ],
    )?;
    let plan = generate_plan(&xml, &version)?;
    if dry_run {
        for (path, manifest) in &plan.files {
            println!("# === {} ===", path.display());
            print!("{manifest}");
        }
        eprintln!(
            "frost: Bazel import preview · {} rules · {} packages",
            plan.rule_count, plan.package_count
        );
        return Ok(0);
    }
    let collisions = plan
        .files
        .keys()
        .filter(|path| root.join(path).exists())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        collisions.is_empty(),
        "refusing to overwrite existing manifest(s): {}",
        collisions.join(", ")
    );
    for (path, manifest) in &plan.files {
        let destination = root.join(path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&destination, manifest)
            .with_context(|| format!("failed to write {}", destination.display()))?;
    }
    println!("frost: imported Bazel native C/C++ subset");
    println!("|-- {} rules", plan.rule_count);
    println!("|-- {} package manifests", plan.package_count);
    println!("`-- review toolchain/profile flags, then run: frost build");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<query version="2">
  <rule class="cc_library rule" name="//lib:math-core">
    <list name="srcs"><label value="//lib:math.cc"/></list>
    <list name="hdrs"><label value="//lib:math.h"/></list>
    <list name="includes"><string value="include"/></list>
    <list name="local_defines"><string value="LOCAL=1"/></list>
  </rule>
  <rule class="cc_binary rule" name="//app:runner">
    <list name="srcs"><label value="//app:main.cc"/></list>
    <list name="deps"><label value="//lib:math-core"/></list>
    <list name="copts"><string value="-Wextra"/></list>
    <list name="linkopts"><string value="-pthread"/></list>
  </rule>
</query>"#;

    #[test]
    fn query_xml_becomes_multi_package_frost_manifests() {
        let plan = generate_plan(XML, "bazel 9.1.0").unwrap();
        assert_eq!(plan.rule_count, 2);
        assert_eq!(plan.package_count, 3);
        let root = &plan.files[Path::new("frost.toml")];
        assert!(root.contains("//app:runner"), "{root}");
        let library = &plan.files[Path::new("lib/frost.toml")];
        assert!(library.contains("[target.math-core]"), "{library}");
        assert!(library.contains("cflags = [\"-DLOCAL=1\"]"), "{library}");
        let app = &plan.files[Path::new("app/frost.toml")];
        assert!(app.contains("deps = [\"//lib:math-core\"]"), "{app}");
        assert!(app.contains("ldflags = [\"-pthread\"]"), "{app}");
    }

    #[test]
    fn unsupported_semantics_fail_the_whole_plan() {
        let external = XML.replace("//lib:math-core", "@outside//lib:math");
        let error = generate_plan(&external, "bazel").unwrap_err().to_string();
        assert!(error.contains("external repository"), "{error}");

        let defines = XML.replace("<list name=\"local_defines\">", "<list name=\"defines\">");
        let error = generate_plan(&defines, "bazel").unwrap_err().to_string();
        assert!(error.contains("unsupported attribute"), "{error}");

        let error = ensure_configuration_free(
            "cc_binary(name = 'app', srcs = select({'//conditions:default': ['a.cc']}))",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("configuration-dependent"), "{error}");
    }
}
