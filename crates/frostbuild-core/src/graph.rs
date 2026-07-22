use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, TargetKind, Toolchain, DEFAULT_PROFILE, HOST_PLATFORM};

pub type FileId = usize;
pub type ActionId = usize;

pub const OBJ_DIR: &str = ".frost/obj";
pub const LIB_DIR: &str = ".frost/lib";
pub const BIN_DIR: &str = ".frost/bin";

/// The interpreter frost runs every genrule and shell test through.
///
/// frost chooses it, so its identity is frost's responsibility in exactly the
/// way the compiler's is: the toolchain fingerprint hashes it alongside the
/// C drivers. Tools the command itself reaches are a different matter — those
/// are undeclared inputs, and no build system can name them for you.
#[cfg(unix)]
pub const SHELL: &str = "/bin/sh";
#[cfg(unix)]
pub const SHELL_ARG: &str = "-c";

#[cfg(windows)]
pub const SHELL: &str = "cmd.exe";
#[cfg(windows)]
pub const SHELL_ARG: &str = "/C";

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
    KofunCompile,
    Command,
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
    /// Additional direct-argv commands executed after `argv`.
    pub followup_argv: Vec<Vec<String>>,
    /// Intermediate workspace directories removed and recreated before each
    /// execution (including determinism reruns).
    pub clean_dirs: Vec<String>,
    /// Retain prior declared outputs while an incremental command reruns.
    #[serde(default)]
    pub preserve_outputs: bool,
    /// Manifest-declared environment values, applied after Frost's baseline.
    pub env: BTreeMap<String, String>,
    /// Host environment names explicitly requested by this action. Their
    /// current values participate in its action key.
    pub pass_env: Vec<String>,
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
    /// Platform-resolved toolchain, embedded so warm invocations can compute
    /// the toolchain fingerprint without re-parsing the manifest.
    pub toolchain: Toolchain,
    /// Workspace default targets, embedded for the same reason.
    pub default_targets: Vec<String>,
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
        // A profile the manifest never declares builds with no profile flags,
        // into its own output tree — silently, so `--profile relase` produces
        // a different binary than `--profile release` and says nothing. Once a
        // workspace declares any profile, an undeclared name is a typo far
        // more often than an intent; declaring an empty section is the way to
        // ask for a bare tree on purpose.
        if !manifest.profiles.is_empty()
            && profile != DEFAULT_PROFILE
            && !manifest.profiles.contains_key(profile)
        {
            let known: Vec<&str> = manifest.profiles.keys().map(String::as_str).collect();
            if let Some(hint) = crate::manifest::closest(profile, known.iter().copied()) {
                bail!("unknown profile {profile:?}. did you mean {hint:?}?");
            }
            bail!(
                "unknown profile {profile:?}. declared profiles: {} \
                 (add an empty [profile.{profile}] section for a bare tree)",
                known.join(", ")
            );
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
            toolchain: toolchain.clone(),
            default_targets: manifest.default_targets.clone(),
            ..BuildGraph::default()
        };
        let profile_flags = manifest.profiles.get(profile).cloned().unwrap_or_default();

        // Transitive exported include dirs, library outputs and genrule
        // outputs, per target — held as structurally shared sets so deep
        // dependency chains stay O(targets + edges) to propagate. Flattening
        // happens only where a flat list is genuinely needed (compile -I
        // flags, order-only generated inputs, link lines) — see #78.
        let mut exported_includes: HashMap<String, Rc<SharedSet>> = HashMap::new();
        let mut exported_libs: HashMap<String, Rc<SharedSet>> = HashMap::new();
        let mut genrule_outputs: HashMap<String, Rc<SharedSet>> = HashMap::new();

        for name in &order {
            let target = &manifest.targets[name];

            let dep_sets = |map: &HashMap<String, Rc<SharedSet>>| -> Vec<Rc<SharedSet>> {
                target.deps.iter().map(|dep| map[dep].clone()).collect()
            };
            let include_set =
                SharedSet::join(target.includes.clone(), dep_sets(&exported_includes));
            let lib_parents = dep_sets(&exported_libs);
            let gen_parents = dep_sets(&genrule_outputs);

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
                        argv: vec![SHELL.into(), SHELL_ARG.into(), expanded],
                        followup_argv: Vec::new(),
                        clean_dirs: Vec::new(),
                        preserve_outputs: false,
                        env: BTreeMap::new(),
                        pass_env: Vec::new(),
                        inputs,
                        order_only_inputs: Vec::new(),
                        outputs: outputs.clone(),
                        depfile: None,
                    })?;
                    target_node.actions.push(action);
                    target_node.outputs = outputs;
                    genrule_outputs.insert(
                        name.clone(),
                        SharedSet::join(target.outputs.clone(), gen_parents),
                    );
                    exported_libs.insert(name.clone(), SharedSet::join(Vec::new(), lib_parents));
                    exported_includes.insert(name.clone(), include_set);
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
                    let (argv, followup_argv, env, pass_env) =
                        if let Some(tool_name) = target.tool.as_deref() {
                            let Some(driver) = toolchain.tools.get(tool_name) else {
                                let configured = toolchain
                                    .tools
                                    .keys()
                                    .map(String::as_str)
                                    .collect::<Vec<_>>();
                                bail!(
                                "test {name:?} uses tool {tool_name:?}, but the active platform \
                                     does not configure [toolchain.tools].{tool_name}{}",
                                if configured.is_empty() {
                                    String::new()
                                } else {
                                    format!(" (configured: {})", configured.join(", "))
                                }
                            );
                            };
                            if driver.contains('/') && !Path::new(driver).is_absolute() {
                                let tool_input = graph.file(driver);
                                if !inputs.contains(&tool_input) {
                                    inputs.push(tool_input);
                                }
                            }
                            let dependency_paths = target
                                .deps
                                .iter()
                                .flat_map(|dep| dep_outputs(&graph, dep))
                                .map(|file| graph.files[file].path.clone())
                                .collect::<Vec<_>>();
                            let argv = expand_test_args(
                                driver,
                                &target.args,
                                &target.inputs,
                                &dependency_paths,
                                &tree,
                                profile,
                                platform,
                            )?;
                            (
                                argv,
                                Vec::new(),
                                target.env.clone(),
                                target.pass_env.clone(),
                            )
                        } else {
                            (
                                vec![
                                    SHELL.into(),
                                    SHELL_ARG.into(),
                                    target
                                        .cmd
                                        .as_deref()
                                        .expect("test validation requires cmd or tool")
                                        .into(),
                                ],
                                Vec::new(),
                                BTreeMap::new(),
                                Vec::new(),
                            )
                        };
                    let action = graph.push_action(ActionNode {
                        id: format!("test:{name}"),
                        desc: format!("TEST {name}"),
                        kind: ActionKind::Test,
                        target: name.clone(),
                        sandbox: target.sandbox,
                        argv,
                        followup_argv,
                        clean_dirs: Vec::new(),
                        preserve_outputs: false,
                        env,
                        pass_env,
                        inputs,
                        order_only_inputs: Vec::new(),
                        outputs: vec![stamp_id],
                        depfile: None,
                    })?;
                    target_node.actions.push(action);
                    target_node.outputs = vec![stamp_id];
                    exported_libs.insert(name.clone(), SharedSet::join(Vec::new(), lib_parents));
                    exported_includes.insert(name.clone(), include_set);
                    genrule_outputs.insert(name.clone(), SharedSet::join(Vec::new(), gen_parents));
                }
                TargetKind::Command => {
                    let tool_name = target
                        .tool
                        .as_deref()
                        .expect("command target validation requires a tool");
                    let Some(driver) = toolchain.tools.get(tool_name) else {
                        let configured = toolchain
                            .tools
                            .keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>();
                        bail!(
                            "target {name:?} uses tool {tool_name:?}, but the active platform \
                             does not configure [toolchain.tools].{tool_name}{}",
                            if configured.is_empty() {
                                String::new()
                            } else {
                                format!(" (configured: {})", configured.join(", "))
                            }
                        );
                    };
                    let mut inputs = target
                        .inputs
                        .iter()
                        .map(|path| graph.file(path))
                        .collect::<Vec<_>>();
                    if driver.contains('/') && !Path::new(driver).is_absolute() {
                        let tool_input = graph.file(driver);
                        if !inputs.contains(&tool_input) {
                            inputs.push(tool_input);
                        }
                    }
                    let mut dependency_inputs = Vec::new();
                    for dep in &target.deps {
                        for output in dep_outputs(&graph, dep) {
                            if !inputs.contains(&output) {
                                inputs.push(output);
                            }
                            dependency_inputs.push(output);
                        }
                    }
                    let input_paths = target.inputs.clone();
                    let dependency_paths = dependency_inputs
                        .iter()
                        .map(|&file| graph.files[file].path.clone())
                        .collect::<Vec<_>>();
                    let outputs = target
                        .outputs
                        .iter()
                        .map(|path| expand_config_template(path, &tree, profile, platform))
                        .collect::<Result<Vec<_>>>()?;
                    let depfile = target
                        .depfile
                        .as_ref()
                        .map(|path| expand_config_template(path, &tree, profile, platform))
                        .transpose()?;
                    let clean_dirs = target
                        .clean_dirs
                        .iter()
                        .map(|path| expand_config_template(path, &tree, profile, platform))
                        .collect::<Result<Vec<_>>>()?;
                    let argv = expand_command_args(
                        driver,
                        &target.args,
                        &input_paths,
                        &dependency_paths,
                        &outputs,
                        &clean_dirs,
                        depfile.as_deref(),
                        &tree,
                        profile,
                        platform,
                    )?;
                    let mut followup_argv = Vec::with_capacity(target.steps.len());
                    for step in &target.steps {
                        let Some(step_driver) = toolchain.tools.get(&step.tool) else {
                            bail!(
                                "target {name:?} uses step tool {:?}, but the active platform \
                                 does not configure [toolchain.tools].{}",
                                step.tool,
                                step.tool
                            );
                        };
                        if step_driver.contains('/') && !Path::new(step_driver).is_absolute() {
                            let tool_input = graph.file(step_driver);
                            if !inputs.contains(&tool_input) {
                                inputs.push(tool_input);
                            }
                        }
                        followup_argv.push(expand_command_args(
                            step_driver,
                            &step.args,
                            &input_paths,
                            &dependency_paths,
                            &outputs,
                            &clean_dirs,
                            depfile.as_deref(),
                            &tree,
                            profile,
                            platform,
                        )?);
                    }
                    let output_ids = outputs
                        .iter()
                        .map(|path| graph.file(path))
                        .collect::<Vec<_>>();
                    let action = graph.push_action(ActionNode {
                        id: format!("command:{name}"),
                        desc: format!("RUN {name} [{tool_name}]"),
                        kind: ActionKind::Command,
                        target: name.clone(),
                        sandbox: target.sandbox,
                        argv,
                        followup_argv,
                        clean_dirs,
                        preserve_outputs: target.preserve_outputs,
                        env: target.env.clone(),
                        pass_env: target.pass_env.clone(),
                        inputs,
                        order_only_inputs: Vec::new(),
                        outputs: output_ids.clone(),
                        depfile,
                    })?;
                    target_node.actions.push(action);
                    target_node.outputs = output_ids;
                    exported_libs.insert(name.clone(), SharedSet::join(Vec::new(), lib_parents));
                    exported_includes.insert(name.clone(), include_set);
                    genrule_outputs.insert(name.clone(), SharedSet::join(outputs, gen_parents));
                }
                TargetKind::KofunBinary => {
                    let Some(driver) = toolchain.kofunc.as_ref() else {
                        bail!(
                            "target {name:?} is a kofun_binary but [toolchain] \
                             does not configure kofunc"
                        );
                    };
                    if target.srcs.len() != 1 {
                        bail!(
                            "target {name:?} is a kofun_binary with {} expanded sources; \
                             exactly one is required",
                            target.srcs.len()
                        );
                    }

                    let source = &target.srcs[0];
                    let bin = format!("{BIN_DIR}/{tree}/{}", path_key(name));
                    let emitted_c = format!("{OBJ_DIR}/{tree}/{}/kofun.c", path_key(name));
                    let mut inputs = vec![graph.file(source)];
                    for dep in &target.deps {
                        for output in dep_outputs(&graph, dep) {
                            if !inputs.contains(&output) {
                                inputs.push(output);
                            }
                        }
                    }
                    let bin_id = graph.file(&bin);
                    let emitted_c_id = graph.file(&emitted_c);
                    let action = graph.push_action(ActionNode {
                        id: format!("kofun:{name}"),
                        desc: format!("KOFUN {source} ({name})"),
                        kind: ActionKind::KofunCompile,
                        target: name.clone(),
                        sandbox: target.sandbox,
                        argv: vec![
                            driver.clone(),
                            "build".into(),
                            source.clone(),
                            "-o".into(),
                            bin.clone(),
                            "--emit-c".into(),
                            emitted_c,
                        ],
                        followup_argv: Vec::new(),
                        clean_dirs: Vec::new(),
                        preserve_outputs: false,
                        env: BTreeMap::new(),
                        pass_env: Vec::new(),
                        inputs,
                        order_only_inputs: Vec::new(),
                        outputs: vec![bin_id, emitted_c_id],
                        depfile: None,
                    })?;
                    target_node.actions.push(action);
                    target_node.outputs = vec![bin_id];
                    exported_libs.insert(name.clone(), SharedSet::join(Vec::new(), lib_parents));
                    exported_includes.insert(name.clone(), include_set);
                    genrule_outputs.insert(name.clone(), SharedSet::join(Vec::new(), gen_parents));
                }
                TargetKind::CcBinary | TargetKind::CcLibrary | TargetKind::CcTest => {
                    let tc = &toolchain;
                    let own_includes = include_set.flatten();
                    let libs = SharedSet::join(Vec::new(), lib_parents.clone()).flatten();
                    let gen_outs = SharedSet::join(Vec::new(), gen_parents.clone()).flatten();
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
                            followup_argv: Vec::new(),
                            clean_dirs: Vec::new(),
                            preserve_outputs: false,
                            env: BTreeMap::new(),
                            pass_env: Vec::new(),
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
                                followup_argv: Vec::new(),
                                clean_dirs: Vec::new(),
                                preserve_outputs: false,
                                env: BTreeMap::new(),
                                pass_env: Vec::new(),
                                inputs: obj_ids.clone(),
                                order_only_inputs: Vec::new(),
                                outputs: vec![lib_id],
                                depfile: None,
                            })?;
                            target_node.actions.push(action);
                            target_node.outputs = vec![lib_id];
                            exported_libs.insert(
                                name.clone(),
                                SharedSet::join(vec![lib.clone()], lib_parents.clone()),
                            );
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
                                followup_argv: Vec::new(),
                                clean_dirs: Vec::new(),
                                preserve_outputs: false,
                                env: BTreeMap::new(),
                                pass_env: Vec::new(),
                                inputs,
                                order_only_inputs: Vec::new(),
                                outputs: vec![bin_id],
                                depfile: None,
                            })?;
                            target_node.actions.push(action);
                            target_node.outputs = vec![bin_id];
                            exported_libs.insert(
                                name.clone(),
                                SharedSet::join(Vec::new(), lib_parents.clone()),
                            );
                            if target.kind == TargetKind::CcTest {
                                let stamp = format!(".frost/test/{tree}/{}/passed", path_key(name));
                                let stamp_id = graph.file(&stamp);
                                let test = graph.push_action(ActionNode {
                                    id: format!("test:{name}"),
                                    desc: format!("TEST {name}"),
                                    kind: ActionKind::Test,
                                    target: name.clone(),
                                    sandbox: target.sandbox,
                                    argv: vec![bin.clone()],
                                    followup_argv: Vec::new(),
                                    clean_dirs: Vec::new(),
                                    preserve_outputs: false,
                                    env: BTreeMap::new(),
                                    pass_env: Vec::new(),
                                    inputs: vec![bin_id],
                                    order_only_inputs: Vec::new(),
                                    outputs: vec![stamp_id],
                                    depfile: None,
                                })?;
                                target_node.actions.push(test);
                                target_node.outputs = vec![stamp_id];
                            }
                        }
                        TargetKind::Genrule
                        | TargetKind::Test
                        | TargetKind::KofunBinary
                        | TargetKind::Command => {
                            unreachable!()
                        }
                    }

                    exported_includes.insert(name.clone(), include_set);
                    genrule_outputs.insert(name.clone(), SharedSet::join(Vec::new(), gen_parents));
                }
            }

            graph.targets.insert(name.clone(), target_node);
        }

        graph.validate_clean_dirs()?;
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

    fn validate_clean_dirs(&self) -> Result<()> {
        let mut claimed: Vec<(&str, &str)> = Vec::new();
        for action in &self.actions {
            for directory in &action.clean_dirs {
                let path = Path::new(directory);
                for &(other_directory, other_action) in &claimed {
                    let other_path = Path::new(other_directory);
                    if path.starts_with(other_path) || other_path.starts_with(path) {
                        bail!(
                            "clean directory {directory:?} for action {:?} overlaps \
                             {other_directory:?} owned by action {other_action:?}",
                            action.id
                        );
                    }
                }
                for file in &self.files {
                    if Path::new(&file.path).starts_with(path) {
                        bail!(
                            "clean directory {directory:?} for action {:?} contains declared \
                             graph path {:?}; clean_dirs may contain only undeclared \
                             intermediates",
                            action.id,
                            file.path
                        );
                    }
                }
                claimed.push((directory, &action.id));
            }
        }
        Ok(())
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
                TargetKind::KofunBinary => "box",
                TargetKind::Command => "folder",
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

fn dep_outputs(graph: &BuildGraph, dep: &str) -> Vec<FileId> {
    graph
        .targets
        .get(dep)
        .map(|t| t.outputs.clone())
        .unwrap_or_default()
}

/// Persistent ordered string set: own entries plus references to parent sets,
/// shared structurally across targets so transitive export propagation costs
/// O(targets + edges) instead of materializing a flat closure per target (#78).
///
/// `flatten` walks own entries first, then parents in declaration order
/// (iterative preorder, first occurrence wins) — exactly the ordering the
/// historical flattened-Vec code produced, so action argv and cache keys are
/// unchanged by the representation.
struct SharedSet {
    own: Vec<String>,
    parents: Vec<Rc<SharedSet>>,
}

impl SharedSet {
    fn join(own: Vec<String>, parents: Vec<Rc<SharedSet>>) -> Rc<Self> {
        Rc::new(Self { own, parents })
    }

    fn flatten(self: &Rc<Self>) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut visited: HashSet<*const SharedSet> = HashSet::new();
        let mut stack: Vec<Rc<SharedSet>> = vec![Rc::clone(self)];
        while let Some(node) = stack.pop() {
            if !visited.insert(Rc::as_ptr(&node)) {
                continue;
            }
            for value in &node.own {
                if seen.insert(value.clone()) {
                    out.push(value.clone());
                }
            }
            for parent in node.parents.iter().rev() {
                stack.push(Rc::clone(parent));
            }
        }
        out
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

fn expand_config_template(
    value: &str,
    config: &str,
    profile: &str,
    platform: &str,
) -> Result<String> {
    let expanded = value
        .replace("${config}", config)
        .replace("${profile}", profile)
        .replace("${platform}", platform);
    if expanded.contains("${") {
        bail!(
            "unknown configuration variable in {value:?} \
             (supported: ${{config}}, ${{profile}}, ${{platform}})"
        );
    }
    Ok(expanded)
}

#[allow(clippy::too_many_arguments)]
fn expand_command_args(
    driver: &str,
    args: &[String],
    inputs: &[String],
    dependency_inputs: &[String],
    outputs: &[String],
    clean_dirs: &[String],
    depfile: Option<&str>,
    config: &str,
    profile: &str,
    platform: &str,
) -> Result<Vec<String>> {
    let mut argv = vec![driver.to_string()];
    for arg in args {
        match arg.as_str() {
            "${in}" => argv.extend(inputs.iter().cloned()),
            "${deps}" => argv.extend(dependency_inputs.iter().cloned()),
            "${outs}" => argv.extend(outputs.iter().cloned()),
            "${clean_dirs}" => argv.extend(clean_dirs.iter().cloned()),
            _ => {
                if arg.contains("${in}")
                    || arg.contains("${deps}")
                    || arg.contains("${outs}")
                    || arg.contains("${clean_dirs}")
                {
                    bail!(
                        "multi-value command variables must occupy one complete argument: {arg:?}"
                    );
                }
                let output_dir = outputs[0]
                    .rsplit_once('/')
                    .map_or(".", |(directory, _)| directory);
                let mut expanded = arg
                    .replace("${out_dir}", output_dir)
                    .replace("${out}", &outputs[0]);
                if expanded.contains("${depfile}") {
                    let Some(depfile) = depfile else {
                        bail!("command arg uses ${{depfile}} but no depfile is configured");
                    };
                    expanded = expanded.replace("${depfile}", depfile);
                }
                if expanded.contains("${clean_dir}") {
                    let Some(clean_dir) = clean_dirs.first() else {
                        bail!("command arg uses ${{clean_dir}} but no clean_dirs are configured");
                    };
                    expanded = expanded.replace("${clean_dir}", clean_dir);
                }
                expanded = expand_config_template(&expanded, config, profile, platform)?;
                if expanded.contains("${") {
                    bail!(
                        "unknown command variable in {arg:?} (supported: ${{in}}, ${{deps}}, \
                         ${{out}}, ${{out_dir}}, ${{outs}}, ${{clean_dir}}, \
                         ${{clean_dirs}}, ${{depfile}}, ${{config}}, ${{profile}}, \
                         ${{platform}})"
                    );
                }
                argv.push(expanded);
            }
        }
    }
    Ok(argv)
}

fn expand_test_args(
    driver: &str,
    args: &[String],
    inputs: &[String],
    dependency_inputs: &[String],
    config: &str,
    profile: &str,
    platform: &str,
) -> Result<Vec<String>> {
    let mut argv = vec![driver.to_string()];
    for arg in args {
        match arg.as_str() {
            "${in}" => argv.extend(inputs.iter().cloned()),
            "${deps}" => argv.extend(dependency_inputs.iter().cloned()),
            _ => {
                if arg.contains("${in}") || arg.contains("${deps}") {
                    bail!("multi-value test variables must occupy one complete argument: {arg:?}");
                }
                let expanded = expand_config_template(arg, config, profile, platform)?;
                argv.push(expanded);
            }
        }
    }
    Ok(argv)
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
    fn kofun_binary_is_one_cacheable_action_with_declared_artifacts() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain]
            kofunc = "tools/kofun"

            [target.generated]
            kind = "genrule"
            cmd = "generate ${out}"
            inputs = ["schema.txt"]
            outputs = ["generated/data.txt"]

            [target.app]
            kind = "kofun_binary"
            srcs = ["src/main.kofun"]
            deps = ["generated"]
            "#,
        )
        .unwrap();
        let graph = BuildGraph::from_manifest(&manifest).unwrap();
        let action = graph.actions.iter().find(|a| a.id == "kofun:app").unwrap();
        assert_eq!(action.kind, ActionKind::KofunCompile);
        assert_eq!(
            action.argv,
            vec![
                "tools/kofun",
                "build",
                "src/main.kofun",
                "-o",
                ".frost/bin/debug/app",
                "--emit-c",
                ".frost/obj/debug/app/kofun.c",
            ]
        );
        assert_eq!(action.depfile, None);
        let inputs = action
            .inputs
            .iter()
            .map(|&id| graph.files[id].path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(inputs, vec!["src/main.kofun", "generated/data.txt"]);
        let outputs = action
            .outputs
            .iter()
            .map(|&id| graph.files[id].path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            outputs,
            vec![".frost/bin/debug/app", ".frost/obj/debug/app/kofun.c"]
        );
        assert_eq!(
            graph.targets["app"]
                .outputs
                .iter()
                .map(|&id| graph.files[id].path.as_str())
                .collect::<Vec<_>>(),
            vec![".frost/bin/debug/app"]
        );
        assert!(
            graph.to_dot().contains("\"app\" [shape=box]"),
            "{}",
            graph.to_dot()
        );
    }

    #[test]
    fn command_target_expands_direct_argv_and_configuration_paths() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain.tools]
            runner = "tools/runner"
            packer = "tools/packer"

            [platform.device.tools]
            runner = "tools/device-runner"
            packer = "tools/device-packer"

            [target.generate]
            kind = "genrule"
            cmd = "generate ${out}"
            inputs = ["schema.txt"]
            outputs = ["generated/data.txt"]

            [target.app]
            kind = "command"
            tool = "runner"
            args = ["--input", "${in}", "--deps", "${deps}", "--output", "${out}",
                    "--output-dir", "${out_dir}", "--depfile", "${depfile}",
                    "--temp", "${clean_dir}", "--platform", "${platform}"]
            inputs = ["src/app.lang"]
            outputs = [".frost/out/${config}/app.bin"]
            depfile = ".frost/out/${config}/app.d"
            clean_dirs = [".frost/tmp/${config}/app"]
            preserve_outputs = true
            steps = [{ tool = "packer", args = ["${out}", "${clean_dirs}", "${config}"] }]
            deps = ["generate"]
            env = { MODE = "release" }
            pass_env = ["LANG_HOME"]
            sandbox = false
            "#,
        )
        .unwrap();
        let graph = BuildGraph::from_manifest_configured(&manifest, "debug", "device").unwrap();
        let action = graph
            .actions
            .iter()
            .find(|action| action.id == "command:app")
            .unwrap();
        assert_eq!(action.kind, ActionKind::Command);
        assert_eq!(
            action.argv,
            vec![
                "tools/device-runner",
                "--input",
                "src/app.lang",
                "--deps",
                "generated/data.txt",
                "--output",
                ".frost/out/device/debug/app.bin",
                "--output-dir",
                ".frost/out/device/debug",
                "--depfile",
                ".frost/out/device/debug/app.d",
                "--temp",
                ".frost/tmp/device/debug/app",
                "--platform",
                "device",
            ]
        );
        assert_eq!(action.env["MODE"], "release");
        assert_eq!(action.pass_env, vec!["LANG_HOME"]);
        assert!(action.preserve_outputs);
        assert_eq!(
            action.followup_argv,
            vec![vec![
                "tools/device-packer",
                ".frost/out/device/debug/app.bin",
                ".frost/tmp/device/debug/app",
                "device/debug",
            ]]
        );
        assert_eq!(action.clean_dirs, vec![".frost/tmp/device/debug/app"]);
        assert_eq!(
            action.depfile.as_deref(),
            Some(".frost/out/device/debug/app.d")
        );
        let inputs = action
            .inputs
            .iter()
            .map(|&id| graph.files[id].path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            inputs,
            vec![
                "src/app.lang",
                "tools/device-runner",
                "generated/data.txt",
                "tools/device-packer"
            ]
        );
    }

    #[test]
    fn direct_test_uses_named_tool_and_executor_owned_success_stamp() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain.tools]
            python = "tools/python"

            [target.generated]
            kind = "genrule"
            cmd = "generate ${out}"
            inputs = ["schema.txt"]
            outputs = ["generated/value.py"]

            [target.unit]
            kind = "test"
            tool = "python"
            args = ["tests/unit.py", "${in}", "${deps}", "${profile}"]
            inputs = ["tests/unit.py"]
            deps = ["generated"]
            env = { PYTHONHASHSEED = "0" }
            pass_env = ["PYTHONPATH"]
            sandbox = false
            "#,
        )
        .unwrap();
        let graph = BuildGraph::from_manifest(&manifest).unwrap();
        let action = graph
            .actions
            .iter()
            .find(|action| action.id == "test:unit")
            .unwrap();
        assert_eq!(action.kind, ActionKind::Test);
        assert_eq!(
            action.argv,
            [
                "tools/python",
                "tests/unit.py",
                "tests/unit.py",
                "generated/value.py",
                "debug"
            ]
        );
        assert_eq!(action.env["PYTHONHASHSEED"], "0");
        assert_eq!(action.pass_env, ["PYTHONPATH"]);
        assert!(action.followup_argv.is_empty());
        assert_eq!(
            graph.files[action.outputs[0]].path,
            ".frost/test/debug/unit/passed"
        );
    }

    #[test]
    fn command_clean_dirs_cannot_overlap_between_actions() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain.tools]
            runner = "runner"

            [target.first]
            kind = "command"
            tool = "runner"
            args = ["${out}"]
            outputs = [".frost/out/${config}/first.bin"]
            clean_dirs = [".frost/tmp/${config}/shared"]

            [target.second]
            kind = "command"
            tool = "runner"
            args = ["${out}"]
            outputs = [".frost/out/${config}/second.bin"]
            clean_dirs = [".frost/tmp/${config}/shared/nested"]
            "#,
        )
        .unwrap();
        let error = BuildGraph::from_manifest(&manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlaps"), "{error}");
        assert!(error.contains("command:first"), "{error}");
        assert!(error.contains("command:second"), "{error}");
    }

    #[test]
    fn command_clean_dirs_cannot_contain_declared_graph_paths() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain.tools]
            runner = "runner"

            [target.app]
            kind = "command"
            tool = "runner"
            args = ["${out}"]
            outputs = [".frost/tmp/${config}/app/final.bin"]
            clean_dirs = [".frost/tmp/${config}/app"]
            "#,
        )
        .unwrap();
        let error = BuildGraph::from_manifest(&manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("contains declared graph path"), "{error}");
        assert!(error.contains("undeclared intermediates"), "{error}");
    }

    #[test]
    fn command_clean_dir_placeholder_requires_an_owned_directory() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain.tools]
            runner = "runner"

            [target.app]
            kind = "command"
            tool = "runner"
            args = ["--temp", "${clean_dir}", "${out}"]
            outputs = [".frost/out/${config}/app.bin"]
            "#,
        )
        .unwrap();
        let error = BuildGraph::from_manifest(&manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no clean_dirs are configured"), "{error}");
    }

    #[test]
    fn kofun_binary_requires_an_explicit_compiler() {
        let manifest =
            Manifest::parse_str("[target.app]\nkind='kofun_binary'\nsrcs=['main.kofun']\n")
                .unwrap();
        let error = BuildGraph::from_manifest(&manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not configure kofunc"), "{error}");
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
    fn platform_selects_kofun_compiler_and_output_tree() {
        let manifest = Manifest::parse_str(
            r#"
            [toolchain]
            kofunc = "host-kofun"

            [platform.device]
            kofunc = "device-kofun"

            [target.app]
            kind = "kofun_binary"
            srcs = ["main.kofun"]
            "#,
        )
        .unwrap();
        let graph = BuildGraph::from_manifest_configured(&manifest, "release", "device").unwrap();
        let action = graph.actions.iter().find(|a| a.id == "kofun:app").unwrap();
        assert_eq!(action.argv[0], "device-kofun");
        assert!(action
            .argv
            .contains(&format!("{BIN_DIR}/device/release/app")));
        assert!(action
            .argv
            .contains(&format!("{OBJ_DIR}/device/release/app/kofun.c")));
    }

    #[test]
    fn an_undeclared_profile_is_a_typo_not_a_silent_new_tree() {
        let manifest = Manifest::parse_str(
            r#"
            [profile.release]
            cflags = ["-O2"]

            [target.app]
            kind = "cc_binary"
            srcs = ["a.c"]
            "#,
        )
        .unwrap();
        let err = BuildGraph::from_manifest_with_profile(&manifest, "relase")
            .unwrap_err()
            .to_string();
        assert!(err.contains("did you mean"), "{err}");
        assert!(err.contains("release"), "{err}");

        // debug always works, declared or not: it is the default.
        assert!(BuildGraph::from_manifest_with_profile(&manifest, "debug").is_ok());
        assert!(BuildGraph::from_manifest_with_profile(&manifest, "release").is_ok());

        // A workspace that declares no profiles keeps naming trees freely.
        let bare = Manifest::parse_str("[target.app]\nkind='cc_binary'\nsrcs=['a.c']\n").unwrap();
        assert!(BuildGraph::from_manifest_with_profile(&bare, "scratch").is_ok());
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
