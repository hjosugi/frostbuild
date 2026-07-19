use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, TargetKind, HOST_PLATFORM};

pub type FileId = usize;
pub type ActionId = usize;

pub const OBJ_DIR: &str = ".frost/obj";
pub const LIB_DIR: &str = ".frost/lib";
pub const BIN_DIR: &str = ".frost/bin";

#[derive(Debug, Serialize, Deserialize)]
pub struct FileNode {
    pub path: String,
    pub producer: Option<ActionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionKind {
    Compile,
    Archive,
    Link,
    Genrule,
    Test,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActionNode {
    /// Stable identifier, e.g. `compile:app:src/main.c`. Journal entries are
    /// keyed by this, so it must not depend on hashes or ordering.
    pub id: String,
    /// Short human-readable description, e.g. `CC src/main.c (app)`.
    pub desc: String,
    pub kind: ActionKind,
    pub target: String,
    pub sandbox: bool,
    pub argv: Vec<String>,
    pub inputs: Vec<FileId>,
    /// Enforce producer completion without adding content to the action key.
    pub order_only_inputs: Vec<FileId>,
    pub outputs: Vec<FileId>,
    /// Workspace-relative path of the Makefile-style depfile this action
    /// writes (compile actions only).
    pub depfile: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TargetNode {
    pub name: String,
    pub kind: TargetKind,
    pub deps: Vec<String>,
    pub actions: Vec<ActionId>,
    pub outputs: Vec<FileId>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BuildGraph {
    pub files: Vec<FileNode>,
    pub actions: Vec<ActionNode>,
    pub targets: BTreeMap<String, TargetNode>,
    pub profile: String,
    /// Platform this graph was configured for; `host` uses the root
    /// `[toolchain]` and historical (platform-free) output paths.
    pub platform: String,
    #[serde(skip)]
    file_ids: HashMap<String, FileId>,
}

impl BuildGraph {
    pub fn from_manifest(manifest: &Manifest) -> Result<Self> {
        Self::from_manifest_with_profile(manifest, "debug")
    }

    pub fn from_manifest_with_profile(manifest: &Manifest, profile: &str) -> Result<Self> {
        Self::from_manifest_configured(manifest, profile, HOST_PLATFORM)
    }

    pub fn from_manifest_configured(
        manifest: &Manifest,
        profile: &str,
        platform: &str,
    ) -> Result<Self> {
        if profile.is_empty()
            || !profile
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("invalid profile name {profile:?}");
        }
        let toolchain = manifest.toolchain_for(platform)?;
        // The host platform keeps historical single-segment output trees so
        // existing workspaces, journals and docs stay valid verbatim.
        let tree = if platform == HOST_PLATFORM {
            profile.to_string()
        } else {
            format!("{platform}/{profile}")
        };
        let order = toposort_targets(manifest)?;
        let mut graph = BuildGraph {
            profile: profile.to_string(),
            platform: platform.to_string(),
            ..BuildGraph::default()
        };
        let profile_flags = manifest.profiles.get(profile).cloned().unwrap_or_default();

        // Transitive exported include dirs and library outputs, per target.
        let mut exported_includes: HashMap<String, Vec<String>> = HashMap::new();
        let mut exported_libs: HashMap<String, Vec<String>> = HashMap::new();
        let mut genrule_outputs: HashMap<String, Vec<String>> = HashMap::new();

        for name in &order {
            let target = &manifest.targets[name];

            let mut includes: Vec<String> = Vec::new();
            let mut libs: Vec<String> = Vec::new();
            let mut gen_outs: Vec<String> = Vec::new();
            for dep in &target.deps {
                extend_unique(&mut includes, &exported_includes[dep]);
                extend_unique(&mut libs, &exported_libs[dep]);
                extend_unique(&mut gen_outs, &genrule_outputs[dep]);
            }
            let mut own_includes = target.includes.clone();
            extend_unique(&mut own_includes, &includes);

            let mut target_node = TargetNode {
                name: name.clone(),
                kind: target.kind,
                deps: target.deps.clone(),
                actions: Vec::new(),
                outputs: Vec::new(),
            };

            match target.kind {
                TargetKind::Genrule => {
                    let cmd = target.cmd.as_deref().unwrap();
                    let expanded = expand_genrule_cmd(cmd, &target.inputs, &target.outputs)?;
                    let mut inputs = Vec::new();
                    for p in &target.inputs {
                        inputs.push(graph.file(p));
                    }
                    // Order after dep targets by consuming their outputs.
                    for dep in &target.deps {
                        for out in dep_outputs(&graph, dep) {
                            inputs.push(out);
                        }
                    }
                    let mut outputs = Vec::new();
                    for p in &target.outputs {
                        outputs.push(graph.file(p));
                    }
                    let action = graph.push_action(ActionNode {
                        id: format!("genrule:{name}"),
                        desc: format!("GEN {name}"),
                        kind: ActionKind::Genrule,
                        target: name.clone(),
                        sandbox: target.sandbox,
                        argv: vec!["/bin/sh".into(), "-c".into(), expanded],
                        inputs,
                        order_only_inputs: Vec::new(),
                        outputs: outputs.clone(),
                        depfile: None,
                    })?;
                    target_node.actions.push(action);
                    target_node.outputs = outputs;
                    let mut exp = target.outputs.clone();
                    extend_unique(&mut exp, &gen_outs);
                    genrule_outputs.insert(name.clone(), exp);
                    exported_libs.insert(name.clone(), libs);
                    let mut exp_inc = target.includes.clone();
                    extend_unique(&mut exp_inc, &includes);
                    exported_includes.insert(name.clone(), exp_inc);
                }
                TargetKind::Test => {
                    let mut inputs = target
                        .inputs
                        .iter()
                        .map(|p| graph.file(p))
                        .collect::<Vec<_>>();
                    for dep in &target.deps {
                        inputs.extend(dep_outputs(&graph, dep));
                    }
                    let stamp = format!(".frost/test/{tree}/{}/passed", path_key(name));
                    let stamp_id = graph.file(&stamp);
                    let command = format!(
                        "{} && mkdir -p {} && : > {}",
                        target.cmd.as_deref().unwrap(),
                        shell_quote(&format!(".frost/test/{tree}/{}", path_key(name))),
                        shell_quote(&stamp)
                    );
                    let action = graph.push_action(ActionNode {
                        id: format!("test:{name}"),
                        desc: format!("TEST {name}"),
                        kind: ActionKind::Test,
                        target: name.clone(),
                        sandbox: target.sandbox,
                        argv: vec!["/bin/sh".into(), "-c".into(), command],
                        inputs,
                        order_only_inputs: Vec::new(),
                        outputs: vec![stamp_id],
                        depfile: None,
                    })?;
                    target_node.actions.push(action);
                    target_node.outputs = vec![stamp_id];
                    exported_libs.insert(name.clone(), libs);
                    exported_includes.insert(name.clone(), own_includes);
                    genrule_outputs.insert(name.clone(), gen_outs);
                }
                TargetKind::CcBinary | TargetKind::CcLibrary | TargetKind::CcTest => {
                    let tc = &toolchain;
                    let mut cflags: Vec<String> = tc.cflags.clone();
                    cflags.extend(profile_flags.cflags.iter().cloned());
                    cflags.extend(target.cflags.iter().cloned());
                    let mut include_flags = Vec::new();
                    for dir in &own_includes {
                        include_flags.push(format!("-I{dir}"));
                    }

                    // One compile action per translation unit.
                    let mut objs: Vec<String> = Vec::new();
                    let mut obj_ids: Vec<FileId> = Vec::new();
                    for src in &target.srcs {
                        let is_cxx = is_cxx_source(src);
                        let driver = if is_cxx { &tc.cxx } else { &tc.cc };
                        let obj = format!("{OBJ_DIR}/{tree}/{}/{src}.o", path_key(name));
                        let depfile = format!("{obj}.d");
                        let mut argv = vec![driver.clone()];
                        argv.extend(cflags.iter().cloned());
                        if is_cxx {
                            argv.extend(tc.cxxflags.iter().cloned());
                            argv.extend(profile_flags.cxxflags.iter().cloned());
                        }
                        argv.extend(include_flags.iter().cloned());
                        argv.extend([
                            "-MD".into(),
                            "-MF".into(),
                            depfile.clone(),
                            "-c".into(),
                            src.clone(),
                            "-o".into(),
                            obj.clone(),
                        ]);
                        let inputs = vec![graph.file(src)];
                        // Generated headers from (transitive) genrule deps
                        // must exist before we compile; the depfile narrows
                        // this to the actually-used set on later builds.
                        let order_only_inputs =
                            gen_outs.iter().map(|gen| graph.file(gen)).collect();
                        let obj_id = graph.file(&obj);
                        let action = graph.push_action(ActionNode {
                            id: format!("compile:{name}:{src}"),
                            desc: format!("CC {src} ({name})"),
                            kind: ActionKind::Compile,
                            target: name.clone(),
                            sandbox: target.sandbox,
                            argv,
                            inputs,
                            order_only_inputs,
                            outputs: vec![obj_id],
                            depfile: Some(depfile),
                        })?;
                        target_node.actions.push(action);
                        objs.push(obj);
                        obj_ids.push(obj_id);
                    }

                    match target.kind {
                        TargetKind::CcLibrary => {
                            let lib = format!("{LIB_DIR}/{tree}/lib{}.a", path_key(name));
                            let mut argv = vec![tc.ar.clone()];
                            argv.extend(tc.arflags.iter().cloned());
                            argv.push(lib.clone());
                            argv.extend(objs.iter().cloned());
                            let lib_id = graph.file(&lib);
                            let action = graph.push_action(ActionNode {
                                id: format!("archive:{name}"),
                                desc: format!("AR lib{name}.a"),
                                kind: ActionKind::Archive,
                                target: name.clone(),
                                sandbox: target.sandbox,
                                argv,
                                inputs: obj_ids.clone(),
                                order_only_inputs: Vec::new(),
                                outputs: vec![lib_id],
                                depfile: None,
                            })?;
                            target_node.actions.push(action);
                            target_node.outputs = vec![lib_id];
                            let mut exp = vec![lib.clone()];
                            extend_unique(&mut exp, &libs);
                            exported_libs.insert(name.clone(), exp);
                        }
                        TargetKind::CcBinary | TargetKind::CcTest => {
                            let bin = format!("{BIN_DIR}/{tree}/{}", path_key(name));
                            let link_driver = if target.srcs.iter().any(|s| is_cxx_source(s)) {
                                &tc.cxx
                            } else {
                                &tc.cc
                            };
                            let mut argv = vec![link_driver.clone()];
                            argv.extend(objs.iter().cloned());
                            argv.extend(libs.iter().cloned());
                            argv.extend(tc.ldflags.iter().cloned());
                            argv.extend(profile_flags.ldflags.iter().cloned());
                            argv.extend(target.ldflags.iter().cloned());
                            argv.extend(["-o".into(), bin.clone()]);
                            let mut inputs = obj_ids.clone();
                            for lib in &libs {
                                inputs.push(graph.file(lib));
                            }
                            let bin_id = graph.file(&bin);
                            let action = graph.push_action(ActionNode {
                                id: format!("link:{name}"),
                                desc: format!("LINK {name}"),
                                kind: ActionKind::Link,
                                target: name.clone(),
                                sandbox: target.sandbox,
                                argv,
                                inputs,
                                order_only_inputs: Vec::new(),
                                outputs: vec![bin_id],
                                depfile: None,
                            })?;
                            target_node.actions.push(action);
                            target_node.outputs = vec![bin_id];
                            exported_libs.insert(name.clone(), libs);
                            if target.kind == TargetKind::CcTest {
                                let stamp = format!(".frost/test/{tree}/{}/passed", path_key(name));
                                let stamp_id = graph.file(&stamp);
                                let command = format!(
                                    "{} && mkdir -p {} && : > {}",
                                    shell_quote(&bin),
                                    shell_quote(&format!(".frost/test/{tree}/{}", path_key(name))),
                                    shell_quote(&stamp)
                                );
                                let test = graph.push_action(ActionNode {
                                    id: format!("test:{name}"),
                                    desc: format!("TEST {name}"),
                                    kind: ActionKind::Test,
                                    target: name.clone(),
                                    sandbox: target.sandbox,
                                    argv: vec!["/bin/sh".into(), "-c".into(), command],
                                    inputs: vec![bin_id],
                                    order_only_inputs: Vec::new(),
                                    outputs: vec![stamp_id],
                                    depfile: None,
                                })?;
                                target_node.actions.push(test);
                                target_node.outputs = vec![stamp_id];
                            }
                        }
                        TargetKind::Genrule | TargetKind::Test => unreachable!(),
                    }

                    let mut exp_inc = target.includes.clone();
                    extend_unique(&mut exp_inc, &includes);
                    exported_includes.insert(name.clone(), exp_inc);
                    genrule_outputs.insert(name.clone(), gen_outs);
                }
            }

            graph.targets.insert(name.clone(), target_node);
        }

        Ok(graph)
    }

    fn file(&mut self, path: &str) -> FileId {
        if let Some(&id) = self.file_ids.get(path) {
            return id;
        }
        let id = self.files.len();
        self.files.push(FileNode {
            path: path.to_string(),
            producer: None,
        });
        self.file_ids.insert(path.to_string(), id);
        id
    }

    fn push_action(&mut self, action: ActionNode) -> Result<ActionId> {
        let id = self.actions.len();
        for &out in &action.outputs {
            if let Some(other) = self.files[out].producer {
                bail!(
                    "output {:?} is produced by both {:?} and {:?}",
                    self.files[out].path,
                    self.actions[other].id,
                    action.id
                );
            }
            self.files[out].producer = Some(id);
        }
        self.actions.push(action);
        Ok(id)
    }

    /// All actions needed (transitively) to build the given targets, in a
    /// valid dependency order.
    pub fn action_closure(&self, targets: &[String]) -> Result<Vec<ActionId>> {
        let mut roots: Vec<ActionId> = Vec::new();
        for name in targets {
            let Some(t) = self.targets.get(name) else {
                bail!("unknown target {name:?}");
            };
            roots.extend(t.actions.iter().copied());
        }
        let mut selected = BTreeSet::new();
        let mut stack: Vec<ActionId> = roots;
        while let Some(a) = stack.pop() {
            if !selected.insert(a) {
                continue;
            }
            for &input in self.actions[a]
                .inputs
                .iter()
                .chain(&self.actions[a].order_only_inputs)
            {
                if let Some(producer) = self.files[input].producer {
                    stack.push(producer);
                }
            }
        }
        Ok(selected.into_iter().collect())
    }

    /// Transitive dependency closure of a target, itself included, sorted.
    pub fn deps_closure(&self, root: &str) -> Result<Vec<String>> {
        if !self.targets.contains_key(root) {
            bail!("unknown target {root:?}");
        }
        let mut seen = BTreeSet::new();
        let mut stack = vec![root.to_string()];
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            stack.extend(self.targets[&name].deps.iter().cloned());
        }
        Ok(seen.into_iter().collect())
    }

    /// Transitive reverse-dependency closure: every target that (transitively)
    /// depends on `root`, itself included, sorted. This is the monorepo-CI
    /// primitive ("what does this change affect?").
    pub fn rdeps_closure(&self, root: &str) -> Result<Vec<String>> {
        if !self.targets.contains_key(root) {
            bail!("unknown target {root:?}");
        }
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for target in self.targets.values() {
            for dep in &target.deps {
                dependents.entry(dep).or_default().push(&target.name);
            }
        }
        let mut seen = BTreeSet::new();
        let mut stack = vec![root];
        while let Some(name) = stack.pop() {
            if !seen.insert(name.to_string()) {
                continue;
            }
            if let Some(users) = dependents.get(name) {
                stack.extend(users.iter().copied());
            }
        }
        Ok(seen.into_iter().collect())
    }

    /// One dependency path from `from` down to `to`, or None when `to` is not
    /// in the dependency closure of `from`.
    pub fn somepath(&self, from: &str, to: &str) -> Result<Option<Vec<String>>> {
        for name in [from, to] {
            if !self.targets.contains_key(name) {
                bail!("unknown target {name:?}");
            }
        }
        fn visit<'a>(
            graph: &'a BuildGraph,
            current: &'a str,
            to: &str,
            path: &mut Vec<String>,
            seen: &mut BTreeSet<&'a str>,
        ) -> bool {
            if !seen.insert(current) {
                return false;
            }
            path.push(current.to_string());
            if current == to {
                return true;
            }
            for dep in &graph.targets[current].deps {
                if visit(graph, dep, to, path, seen) {
                    return true;
                }
            }
            path.pop();
            false
        }
        let mut path = Vec::new();
        let mut seen = BTreeSet::new();
        Ok(visit(self, from, to, &mut path, &mut seen).then_some(path))
    }

    pub fn to_dot(&self) -> String {
        let mut out = String::from("digraph frost {\n  rankdir=LR;\n");
        for target in self.targets.values() {
            let shape = match target.kind {
                TargetKind::CcBinary => "box",
                TargetKind::CcLibrary => "ellipse",
                TargetKind::CcTest => "box3d",
                TargetKind::Genrule => "diamond",
                TargetKind::Test => "component",
            };
            out.push_str(&format!("  \"{}\" [shape={shape}];\n", target.name));
            for dep in &target.deps {
                out.push_str(&format!("  \"{}\" -> \"{dep}\";\n", target.name));
            }
        }
        out.push_str("}\n");
        out
    }
}

fn path_key(label: &str) -> String {
    label.trim_start_matches("//").replace([':', '/'], "_")
}

fn is_cxx_source(path: &str) -> bool {
    matches!(
        PathExt::extension(path),
        Some("cc" | "cpp" | "cxx" | "C" | "c++")
    )
}

struct PathExt;
impl PathExt {
    fn extension(path: &str) -> Option<&str> {
        path.rsplit_once('.').map(|(_, ext)| ext)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn dep_outputs(graph: &BuildGraph, dep: &str) -> Vec<FileId> {
    graph
        .targets
        .get(dep)
        .map(|t| t.outputs.clone())
        .unwrap_or_default()
}

fn extend_unique(dst: &mut Vec<String>, src: &[String]) {
    for s in src {
        if !dst.iter().any(|d| d == s) {
            dst.push(s.clone());
        }
    }
}

/// Depth-first topological sort over target deps with cycle reporting.
fn toposort_targets(manifest: &Manifest) -> Result<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unvisited,
        Visiting,
        Done,
    }

    fn visit(
        name: &str,
        manifest: &Manifest,
        states: &mut BTreeMap<String, State>,
        path: &mut Vec<String>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        match states[name] {
            State::Done => return Ok(()),
            State::Visiting => {
                let start = path.iter().position(|p| p == name).unwrap_or(0);
                let mut cycle = path[start..].to_vec();
                cycle.push(name.to_string());
                bail!("dependency cycle: {}", cycle.join(" -> "));
            }
            State::Unvisited => {}
        }
        states.insert(name.to_string(), State::Visiting);
        path.push(name.to_string());
        for dep in &manifest.targets[name].deps {
            visit(dep, manifest, states, path, order)?;
        }
        path.pop();
        states.insert(name.to_string(), State::Done);
        order.push(name.to_string());
        Ok(())
    }

    let mut states: BTreeMap<String, State> = manifest
        .targets
        .keys()
        .map(|k| (k.clone(), State::Unvisited))
        .collect();
    let mut order = Vec::new();
    let mut path = Vec::new();
    let names: Vec<String> = manifest.targets.keys().cloned().collect();
    for name in names {
        visit(&name, manifest, &mut states, &mut path, &mut order)?;
    }
    Ok(order)
}

fn expand_genrule_cmd(cmd: &str, inputs: &[String], outputs: &[String]) -> Result<String> {
    let expanded = cmd
        .replace("${in}", &inputs.join(" "))
        .replace("${outs}", &outputs.join(" "))
        .replace("${out}", &outputs[0]);
    if expanded.contains("${") {
        bail!(
            "genrule cmd has unknown variable: {cmd:?} (supported: ${{in}}, ${{out}}, ${{outs}})"
        );
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn demo_manifest() -> Manifest {
        Manifest::parse_str(
            r#"
            [toolchain]
            cc = "cc"

            [target.gen]
            kind = "genrule"
            cmd = "sh gen.sh ${out}"
            inputs = ["gen.sh"]
            outputs = ["gen/config.h"]
            includes = ["gen"]

            [target.util]
            kind = "cc_library"
            srcs = ["src/util.c"]
            includes = ["include"]

            [target.app]
            kind = "cc_binary"
            srcs = ["src/main.c"]
            deps = ["util", "gen"]
            "#,
        )
        .unwrap()
    }

    #[test]
    fn builds_expected_actions() {
        let graph = BuildGraph::from_manifest(&demo_manifest()).unwrap();
        let ids: Vec<&str> = graph.actions.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"genrule:gen"));
        assert!(ids.contains(&"compile:util:src/util.c"));
        assert!(ids.contains(&"archive:util"));
        assert!(ids.contains(&"compile:app:src/main.c"));
        assert!(ids.contains(&"link:app"));
    }

    #[test]
    fn compile_gets_dep_includes_and_gen_inputs() {
        let graph = BuildGraph::from_manifest(&demo_manifest()).unwrap();
        let compile = graph
            .actions
            .iter()
            .find(|a| a.id == "compile:app:src/main.c")
            .unwrap();
        assert!(compile.argv.contains(&"-Iinclude".to_string()));
        assert!(compile.argv.contains(&"-Igen".to_string()));
        let input_paths: Vec<&str> = compile
            .order_only_inputs
            .iter()
            .map(|&f| graph.files[f].path.as_str())
            .collect();
        assert!(input_paths.contains(&"gen/config.h"));
    }

    #[test]
    fn link_orders_after_archive() {
        let graph = BuildGraph::from_manifest(&demo_manifest()).unwrap();
        let link = graph.actions.iter().find(|a| a.id == "link:app").unwrap();
        let lib = format!("{LIB_DIR}/debug/libutil.a");
        assert!(link.argv.contains(&lib));
        let input_paths: Vec<&str> = link
            .inputs
            .iter()
            .map(|&f| graph.files[f].path.as_str())
            .collect();
        assert!(input_paths.contains(&lib.as_str()));
    }

    #[test]
    fn closure_selects_only_needed_actions() {
        let graph = BuildGraph::from_manifest(&demo_manifest()).unwrap();
        let closure = graph.action_closure(&["util".to_string()]).unwrap();
        let ids: Vec<&str> = closure
            .iter()
            .map(|&a| graph.actions[a].id.as_str())
            .collect();
        assert!(ids.contains(&"compile:util:src/util.c"));
        assert!(ids.contains(&"archive:util"));
        assert!(!ids.contains(&"link:app"));
        assert!(!ids.contains(&"genrule:gen"));
    }

    #[test]
    fn query_closures_and_somepath() {
        let graph = BuildGraph::from_manifest(&demo_manifest()).unwrap();
        assert_eq!(
            graph.deps_closure("app").unwrap(),
            vec!["app", "gen", "util"]
        );
        assert_eq!(graph.deps_closure("util").unwrap(), vec!["util"]);
        assert_eq!(graph.rdeps_closure("util").unwrap(), vec!["app", "util"]);
        assert_eq!(
            graph.somepath("app", "gen").unwrap(),
            Some(vec!["app".to_string(), "gen".to_string()])
        );
        assert_eq!(graph.somepath("util", "gen").unwrap(), None);
        assert!(graph.deps_closure("nope").is_err());
    }

    #[test]
    fn platform_isolates_paths_and_selects_toolchain() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain]
            cc = "cc"

            [platform.cross]
            cc = "cross-gcc"
            ar = "cross-ar"
            arflags = ["rcs"]

            [target.util]
            kind = "cc_library"
            srcs = ["src/util.c"]

            [target.app]
            kind = "cc_binary"
            srcs = ["src/main.c"]
            deps = ["util"]
            "#,
        )
        .unwrap();
        let graph = BuildGraph::from_manifest_configured(&manifest, "debug", "cross").unwrap();
        assert_eq!(graph.platform, "cross");

        let compile = graph
            .actions
            .iter()
            .find(|a| a.id == "compile:app:src/main.c")
            .unwrap();
        assert_eq!(compile.argv[0], "cross-gcc");
        let obj = format!("{OBJ_DIR}/cross/debug/app/src/main.c.o");
        assert!(compile.argv.contains(&obj), "argv: {:?}", compile.argv);

        let archive = graph
            .actions
            .iter()
            .find(|a| a.id == "archive:util")
            .unwrap();
        assert_eq!(archive.argv[0], "cross-ar");
        assert_eq!(archive.argv[1], "rcs");
        assert!(archive
            .argv
            .contains(&format!("{LIB_DIR}/cross/debug/libutil.a")));

        let link = graph.actions.iter().find(|a| a.id == "link:app").unwrap();
        assert!(link.argv.contains(&format!("{BIN_DIR}/cross/debug/app")));

        // The host graph keeps historical platform-free paths.
        let host = BuildGraph::from_manifest_with_profile(&manifest, "debug").unwrap();
        let host_link = host.actions.iter().find(|a| a.id == "link:app").unwrap();
        assert!(host_link.argv.contains(&format!("{BIN_DIR}/debug/app")));
    }

    #[test]
    fn detects_dependency_cycle() {
        let manifest = Manifest::parse_str(
            r#"
            [target.a]
            kind = "cc_library"
            srcs = ["a.c"]
            deps = ["b"]

            [target.b]
            kind = "cc_library"
            srcs = ["b.c"]
            deps = ["a"]
            "#,
        )
        .unwrap();
        let err = BuildGraph::from_manifest(&manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("dependency cycle"), "{err}");
    }

    #[test]
    fn rejects_duplicate_outputs() {
        let manifest = Manifest::parse_str(
            r#"
            [target.g1]
            kind = "genrule"
            cmd = "true"
            outputs = ["gen/same.h"]

            [target.g2]
            kind = "genrule"
            cmd = "true"
            outputs = ["gen/same.h"]
            "#,
        )
        .unwrap();
        let err = BuildGraph::from_manifest(&manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("produced by both"), "{err}");
    }

    #[test]
    fn unknown_target_in_closure_errors() {
        let graph = BuildGraph::from_manifest(&demo_manifest()).unwrap();
        assert!(graph.action_closure(&["nope".to_string()]).is_err());
    }
}
