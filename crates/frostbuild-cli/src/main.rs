use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use frostbuild_core::graph::{ActionKind, BuildGraph, BIN_DIR, LIB_DIR, OBJ_DIR};
use frostbuild_core::graph_store::GraphStore;
use frostbuild_core::journal::Journal;
use frostbuild_core::manifest::{Manifest, TargetKind};
use frostbuild_exec::{toolchain_closure_fingerprint_cached, BuildOptions, Engine, Outcome};

#[derive(Parser)]
#[command(
    name = "frost",
    version,
    about = "frostbuild: correct, fast incremental builds"
)]
struct Cli {
    /// Workspace root (directory containing frost.toml)
    #[arg(short = 'C', long = "workspace", default_value = ".", global = true)]
    workspace: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build targets (default: workspace default_targets)
    Build {
        targets: Vec<String>,
        /// Number of parallel jobs (default: number of CPUs)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        /// Keep building independent actions after a failure
        #[arg(short = 'k', long)]
        keep_going: bool,
        /// After the build, print why each action ran or was cached
        #[arg(long)]
        explain: bool,
        /// Print full command lines as they run
        #[arg(short, long)]
        verbose: bool,
        /// Build profile; outputs and caches are isolated per profile
        #[arg(long, default_value = "debug")]
        profile: String,
        /// Target platform from [platform.<name>] for cross/device builds;
        /// outputs and caches are isolated per platform
        #[arg(long, default_value = frostbuild_core::manifest::HOST_PLATFORM)]
        platform: String,
        /// Disable successful test-result cache
        #[arg(long)]
        no_cache: bool,
        /// Isolate actions from undeclared workspace files with bubblewrap
        #[arg(long)]
        sandbox: bool,
        /// Execute each selected action twice and compare output digests
        #[arg(long, num_args = 0..=1, default_missing_value = "0", require_equals = true)]
        check_determinism: Option<Option<usize>>,
        /// Write a Chrome/Perfetto trace JSON
        #[arg(long)]
        trace: Option<PathBuf>,
        /// Report scheduling measurements: makespan, worker utilization and
        /// distance from the estimated critical path
        #[arg(long)]
        stats: bool,
        /// Execute through the per-workspace frostd service
        #[arg(long)]
        daemon: bool,
        #[arg(long, value_enum, default_value = "critical-path")]
        scheduler: SchedulerArg,
        #[arg(long, value_enum, default_value = "journal")]
        estimator: EstimatorArg,
    },
    /// Build and run test/cc_test targets
    Test {
        targets: Vec<String>,
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        #[arg(short = 'k', long)]
        keep_going: bool,
        #[arg(long)]
        affected: bool,
        #[arg(long)]
        predictive: bool,
        #[arg(long, conflicts_with = "affected")]
        all: bool,
        #[arg(long)]
        no_cache: bool,
        #[arg(long)]
        explain: bool,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long, default_value = frostbuild_core::manifest::HOST_PLATFORM)]
        platform: String,
        #[arg(long)]
        sandbox: bool,
        #[arg(long)]
        daemon: bool,
        #[arg(long, value_enum, default_value = "critical-path")]
        scheduler: SchedulerArg,
        #[arg(long, value_enum, default_value = "journal")]
        estimator: EstimatorArg,
    },
    /// Show which actions would run and why, without executing anything
    Plan {
        targets: Vec<String>,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long, default_value = frostbuild_core::manifest::HOST_PLATFORM)]
        platform: String,
    },
    /// Remove build outputs (--cache also removes the journal and hash cache)
    Clean {
        #[arg(long)]
        cache: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        platform: Option<String>,
    },
    /// Print the target dependency graph
    Graph {
        /// Emit Graphviz dot instead of text
        #[arg(long)]
        dot: bool,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long, default_value = frostbuild_core::manifest::HOST_PLATFORM)]
        platform: String,
    },
    /// Export JSON Compilation Database for clangd/IDE integrations
    Compdb {
        #[arg(long, default_value = "compile_commands.json")]
        output: PathBuf,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long, default_value = frostbuild_core::manifest::HOST_PLATFORM)]
        platform: String,
    },
    /// Explain the most recently recorded decision for a target
    Explain {
        target: String,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long, default_value = frostbuild_core::manifest::HOST_PLATFORM)]
        platform: String,
    },
    /// Compare scheduling strategies without building anything
    Simulate {
        targets: Vec<String>,
        /// Worker counts to sweep (default: 1,2,4,8,16 capped at this host)
        #[arg(long, value_delimiter = ',')]
        jobs: Option<Vec<usize>>,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long, default_value = frostbuild_core::manifest::HOST_PLATFORM)]
        platform: String,
        #[arg(long)]
        json: bool,
    },
    /// Query the target dependency graph (configuration-free)
    Query {
        #[command(subcommand)]
        function: QueryCmd,
    },
    /// Manage the per-workspace Unix-socket daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCmd,
    },
    /// Convert the supported Ninja rule/build subset to frost.toml
    ImportNinja {
        #[arg(default_value = "build.ninja")]
        ninja: PathBuf,
        #[arg(long, default_value = "frost.toml")]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum QueryCmd {
    /// Transitive dependencies of a target (itself included)
    Deps {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Targets that transitively depend on a target ("what does this affect?")
    Rdeps {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// One dependency path between two targets
    Somepath {
        from: String,
        to: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    Start,
    Status,
    Stop,
    Restart,
    #[command(hide = true)]
    Serve,
}

#[derive(Clone, Copy, ValueEnum)]
enum SchedulerArg {
    CriticalPath,
    Fifo,
}

#[derive(Clone, Copy, ValueEnum)]
enum EstimatorArg {
    Heuristic,
    Journal,
    Static,
    Learned,
}

fn main() {
    if let Err(error) = frostbuild_exec::install_signal_handler() {
        eprintln!("frost: failed to install signal handler: {error:#}");
        std::process::exit(2);
    }
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("frost: error: {err:#}");
            std::process::exit(2);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let root = cli
        .workspace
        .canonicalize()
        .with_context(|| format!("workspace {} not found", cli.workspace.display()))?;

    match cli.command {
        Cmd::Build {
            targets,
            jobs,
            keep_going,
            explain,
            verbose,
            profile,
            platform,
            no_cache,
            sandbox,
            check_determinism,
            trace,
            stats,
            daemon,
            scheduler,
            estimator,
        } => run_build(
            &root,
            BuildRequest {
                targets,
                jobs,
                keep_going,
                explain,
                verbose,
                profile,
                platform,
                no_cache,
                sandbox,
                check_determinism: check_determinism.is_some(),
                trace,
                stats,
                test_mode: false,
                daemon,
                affected: false,
                predictive: false,
                scheduler,
                estimator,
            },
        ),
        Cmd::Test {
            targets,
            jobs,
            keep_going,
            affected,
            predictive,
            all: _,
            no_cache,
            explain,
            profile,
            platform,
            sandbox,
            daemon,
            scheduler,
            estimator,
        } => run_build(
            &root,
            BuildRequest {
                targets,
                jobs,
                keep_going,
                explain,
                verbose: false,
                profile,
                platform,
                no_cache,
                sandbox,
                check_determinism: false,
                trace: None,
                stats: false,
                test_mode: true,
                daemon,
                affected,
                predictive,
                scheduler,
                estimator,
            },
        ),
        Cmd::Plan {
            targets,
            profile,
            platform,
        } => {
            let graph = load_graph(&root, &profile, &platform)?;
            let requested = resolve_targets(&graph, targets)?;
            let closure = graph.action_closure(&requested)?;
            let toolchain = toolchain_closure_fingerprint_cached(&root, &graph.toolchain)?;
            let opts = BuildOptions {
                jobs: default_jobs(),
                keep_going: true,
                dry_run: true,
                verbose: false,
                ..BuildOptions::default()
            };

            let total = closure.len();
            let report = Engine::new(&root, &graph, closure, toolchain, opts).run()?;

            for r in &report.results {
                match &r.outcome {
                    Outcome::WouldRun { reason } => {
                        println!("would run {} :: {reason}", r.id)
                    }
                    Outcome::MayRun { reason } => {
                        println!("may run   {} :: {reason}", r.id)
                    }
                    _ => {}
                }
            }
            let would = report.count(|o| matches!(o, Outcome::WouldRun { .. }));
            let may = report.count(|o| matches!(o, Outcome::MayRun { .. }));
            println!(
                "plan: {} would run, {} may run, {} cached ({} actions)",
                would,
                may,
                report.cached(),
                total
            );
            Ok(0)
        }
        Cmd::Clean {
            cache,
            profile,
            platform,
        } => {
            let active_profile = profile.as_deref().unwrap_or("debug");
            let active_platform = platform
                .as_deref()
                .unwrap_or(frostbuild_core::manifest::HOST_PLATFORM);
            let graph = load_graph(&root, active_profile, active_platform)?;

            // Narrow the removal to the requested platform/profile subtree;
            // with neither given, the whole output trees go.
            let subtree = match (&platform, &profile) {
                (None, None) => None,
                (None, Some(profile)) => Some(profile.clone()),
                (Some(platform), None) => Some(platform.clone()),
                (Some(platform), Some(profile)) => Some(format!("{platform}/{profile}")),
            };
            let mut removed = 0usize;
            for dir in [OBJ_DIR, LIB_DIR, BIN_DIR] {
                let path = subtree
                    .as_ref()
                    .map_or_else(|| root.join(dir), |sub| root.join(dir).join(sub));
                if path.exists() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                    removed += 1;
                }
            }
            // Genrule outputs live outside .frost, wherever the manifest put them.
            for target in graph.targets.values() {
                if target.kind == TargetKind::Genrule {
                    for &out in &target.outputs {
                        let path = root.join(&graph.files[out].path);
                        if path.exists() {
                            std::fs::remove_file(&path)
                                .with_context(|| format!("failed to remove {}", path.display()))?;
                            removed += 1;
                        }
                    }
                }
            }
            if cache {
                for rel in [
                    frostbuild_core::journal::JOURNAL_REL_PATH,
                    ".frost/journal.json",
                    ".frost/hashcache.bin",
                    ".frost/hashcache.json",
                ] {
                    let path = root.join(rel);
                    if path.exists() {
                        std::fs::remove_file(&path)?;
                        removed += 1;
                    }
                }
            }
            println!("frost: cleaned ({removed} entries removed)");
            Ok(0)
        }
        Cmd::Graph {
            dot,
            profile,
            platform,
        } => {
            let graph = load_graph(&root, &profile, &platform)?;
            if dot {
                print!("{}", graph.to_dot());
            } else {
                for target in graph.targets.values() {
                    let deps = if target.deps.is_empty() {
                        String::new()
                    } else {
                        format!(" <- {}", target.deps.join(", "))
                    };
                    println!("{} [{}]{}", target.name, target.kind.as_str(), deps);
                }
            }
            Ok(0)
        }
        Cmd::Compdb {
            output,
            profile,
            platform,
        } => {
            let graph = load_graph(&root, &profile, &platform)?;
            let entries = graph
                .actions
                .iter()
                .filter(|action| action.kind == ActionKind::Compile)
                .map(|action| {
                    let file = action
                        .inputs
                        .first()
                        .map(|&id| graph.files[id].path.clone())
                        .unwrap_or_default();
                    serde_json::json!({
                        "directory": root,
                        "file": file,
                        "arguments": action.argv,
                        "output": action.outputs.first().map(|&id| graph.files[id].path.clone()),
                    })
                })
                .collect::<Vec<_>>();
            let destination = if output.is_absolute() {
                output
            } else {
                root.join(output)
            };
            std::fs::write(&destination, serde_json::to_vec_pretty(&entries)?)?;
            println!(
                "frost: wrote {} entries to {}",
                entries.len(),
                destination.display()
            );
            Ok(0)
        }
        Cmd::Explain {
            target,
            profile,
            platform,
        } => {
            let graph = load_graph(&root, &profile, &platform)?;
            let target = resolve_targets(&graph, vec![target])?.remove(0);
            let closure = graph.action_closure(std::slice::from_ref(&target))?;
            let current = Engine::new(
                &root,
                &graph,
                closure,
                toolchain_closure_fingerprint_cached(&root, &graph.toolchain)?,
                BuildOptions {
                    dry_run: true,
                    keep_going: true,
                    ..BuildOptions::default()
                },
            )
            .run()?;
            if current
                .results
                .iter()
                .all(|result| matches!(result.outcome, Outcome::Cached))
            {
                println!(
                    "frost: no execution required for {target} ({profile}); all actions cached"
                );
                return Ok(0);
            }
            let journal = Journal::load(&root);
            let mut found = 0;
            for action in graph
                .actions
                .iter()
                .filter(|action| action.target == target)
            {
                let id = frostbuild_exec::journal_id(&graph, action);
                if let Some(entry) = journal.actions.get(&id) {
                    println!(
                        "{} :: {} ({} ms)",
                        action.id, entry.reason, entry.duration_ms
                    );
                    found += 1;
                }
            }
            if found == 0 {
                println!("frost: no recorded execution for {target} ({profile})");
            }
            Ok(0)
        }
        Cmd::Simulate {
            targets,
            jobs,
            profile,
            platform,
            json,
        } => run_simulate(&root, targets, jobs, &profile, &platform, json),
        Cmd::Query { function } => {
            // The target-level graph is configuration-free: deps are
            // unconditional, so any profile/platform yields the same shape.
            let graph = load_graph(&root, "debug", frostbuild_core::manifest::HOST_PLATFORM)?;
            let (query, names) = match &function {
                QueryCmd::Deps { target, .. } => {
                    (format!("deps({target})"), graph.deps_closure(target)?)
                }
                QueryCmd::Rdeps { target, .. } => {
                    (format!("rdeps({target})"), graph.rdeps_closure(target)?)
                }
                QueryCmd::Somepath { from, to, .. } => {
                    let path = graph.somepath(from, to)?;
                    let Some(path) = path else {
                        println!("no path from {from} to {to}");
                        return Ok(1);
                    };
                    (format!("somepath({from}, {to})"), path)
                }
            };
            let json = match &function {
                QueryCmd::Deps { json, .. }
                | QueryCmd::Rdeps { json, .. }
                | QueryCmd::Somepath { json, .. } => *json,
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "query": query, "targets": names })
                    )?
                );
            } else {
                for name in &names {
                    println!("{name}");
                }
            }
            Ok(0)
        }
        Cmd::Daemon { command } => daemon_command(&root, command),
        Cmd::ImportNinja { ninja, output } => import_ninja(&root, ninja, output),
    }
}

fn import_ninja(root: &std::path::Path, ninja: PathBuf, output: PathBuf) -> Result<i32> {
    let source = if ninja.is_absolute() {
        ninja
    } else {
        root.join(ninja)
    };
    let text = std::fs::read_to_string(&source)?;
    let mut rules = std::collections::BTreeMap::new();
    let mut current_rule: Option<String> = None;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("rule ") {
            current_rule = Some(name.trim().to_string());
        } else if let Some(command) = line.trim_start().strip_prefix("command = ") {
            if let Some(name) = &current_rule {
                rules.insert(name.clone(), command.to_string());
            }
        } else if !line.starts_with(' ') {
            current_rule = None;
        }
    }
    let mut generated = String::from("[workspace]\n\n");
    let mut producers = std::collections::BTreeMap::new();
    let mut builds = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("build ") else {
            continue;
        };
        let (outputs, rest) = rest
            .split_once(':')
            .context("invalid Ninja build statement")?;
        let mut fields = rest.split_whitespace();
        let rule = fields.next().context("missing Ninja rule")?;
        let inputs = fields
            .filter(|field| *field != "|")
            .map(str::to_string)
            .collect::<Vec<_>>();
        let outputs = outputs
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let name = sanitize_target(outputs.first().context("build has no output")?);
        for output in &outputs {
            producers.insert(output.clone(), name.clone());
        }
        builds.push((name, rule.to_string(), inputs, outputs));
    }
    for (name, rule, inputs, outputs) in builds {
        let command = rules
            .get(&rule)
            .with_context(|| format!("unsupported/unknown Ninja rule {rule:?}"))?;
        let deps = inputs
            .iter()
            .filter_map(|input| producers.get(input).cloned())
            .collect::<Vec<_>>();
        let files = inputs
            .iter()
            .filter(|input| !producers.contains_key(*input))
            .cloned()
            .collect::<Vec<_>>();
        let expanded = command.replace("$in", "${in}").replace("$out", "${outs}");
        generated.push_str(&format!(
            "[target.{name}]\nkind = \"genrule\"\ncmd = {:?}\n",
            expanded
        ));
        generated.push_str(&format!(
            "inputs = {}\noutputs = {}\ndeps = {}\n\n",
            serde_json::to_string(&files)?,
            serde_json::to_string(&outputs)?,
            serde_json::to_string(&deps)?
        ));
    }
    let destination = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };
    std::fs::write(&destination, generated)?;
    println!("frost: imported {}", source.display());
    Ok(0)
}

fn sanitize_target(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn daemon_command(root: &std::path::Path, command: DaemonCmd) -> Result<i32> {
    use frostbuild_daemon::{Request, PROTOCOL_VERSION};
    match command {
        DaemonCmd::Serve => {
            frostbuild_daemon::serve(root)?;
            Ok(0)
        }
        DaemonCmd::Start => {
            if frostbuild_daemon::request(
                root,
                &Request::Status {
                    version: PROTOCOL_VERSION,
                },
            )
            .is_ok()
            {
                println!("frostd: already running");
                return Ok(0);
            }
            let executable = std::env::current_exe()?;
            std::process::Command::new(executable)
                .arg("-C")
                .arg(root)
                .args(["daemon", "serve"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            for _ in 0..50 {
                if frostbuild_daemon::request(
                    root,
                    &Request::Status {
                        version: PROTOCOL_VERSION,
                    },
                )
                .is_ok()
                {
                    println!("frostd: started");
                    return Ok(0);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            bail!("frostd did not become ready");
        }
        DaemonCmd::Status => {
            let response = frostbuild_daemon::request(
                root,
                &Request::Status {
                    version: PROTOCOL_VERSION,
                },
            )?;
            println!(
                "frostd: {} (protocol {})",
                response.stdout, response.version
            );
            Ok(response.code)
        }
        DaemonCmd::Stop => {
            let response = frostbuild_daemon::request(
                root,
                &Request::Shutdown {
                    version: PROTOCOL_VERSION,
                },
            )?;
            println!("frostd: {}", response.stdout);
            Ok(response.code)
        }
        DaemonCmd::Restart => {
            let _ = frostbuild_daemon::request(
                root,
                &Request::Shutdown {
                    version: PROTOCOL_VERSION,
                },
            );
            daemon_command(root, DaemonCmd::Start)
        }
    }
}

struct BuildRequest {
    targets: Vec<String>,
    jobs: Option<usize>,
    keep_going: bool,
    explain: bool,
    verbose: bool,
    profile: String,
    platform: String,
    no_cache: bool,
    sandbox: bool,
    check_determinism: bool,
    trace: Option<PathBuf>,
    stats: bool,
    test_mode: bool,
    daemon: bool,
    affected: bool,
    predictive: bool,
    scheduler: SchedulerArg,
    estimator: EstimatorArg,
}

fn run_build_via_daemon(root: &std::path::Path, request: &BuildRequest) -> Result<i32> {
    use frostbuild_daemon::{Request, PROTOCOL_VERSION};
    let mut args = vec![
        "-C".to_string(),
        root.to_string_lossy().into_owned(),
        if request.test_mode { "test" } else { "build" }.to_string(),
    ];
    args.extend(request.targets.iter().cloned());
    if let Some(jobs) = request.jobs {
        args.extend(["--jobs".into(), jobs.to_string()]);
    }
    if request.keep_going {
        args.push("--keep-going".into());
    }
    if request.explain {
        args.push("--explain".into());
    }
    if request.verbose {
        args.push("--verbose".into());
    }
    if request.no_cache {
        args.push("--no-cache".into());
    }
    if request.affected {
        args.push("--affected".into());
    }
    if request.predictive {
        args.push("--predictive".into());
    }
    if request.sandbox {
        args.push("--sandbox".into());
    }
    if request.check_determinism {
        args.push("--check-determinism".into());
    }
    args.extend([
        "--scheduler".into(),
        match request.scheduler {
            SchedulerArg::CriticalPath => "critical-path",
            SchedulerArg::Fifo => "fifo",
        }
        .into(),
    ]);
    args.extend([
        "--estimator".into(),
        match request.estimator {
            EstimatorArg::Heuristic => "heuristic",
            EstimatorArg::Journal => "journal",
            EstimatorArg::Static => "static",
            EstimatorArg::Learned => "learned",
        }
        .into(),
    ]);
    args.extend(["--profile".into(), request.profile.clone()]);
    args.extend(["--platform".into(), request.platform.clone()]);
    if request.stats {
        args.push("--stats".into());
    }
    if let Some(trace) = &request.trace {
        args.extend(["--trace".into(), trace.to_string_lossy().into_owned()]);
    }
    let request_message = Request::Run {
        version: PROTOCOL_VERSION,
        program: std::env::current_exe()?,
        args,
    };
    let response = match frostbuild_daemon::request(root, &request_message) {
        Ok(response) => response,
        Err(_) => {
            daemon_command(root, DaemonCmd::Start)?;
            frostbuild_daemon::request(root, &request_message)?
        }
    };
    print!("{}", response.stdout);
    eprint!("{}", response.stderr);
    Ok(response.code)
}

fn run_build(root: &std::path::Path, request: BuildRequest) -> Result<i32> {
    if request.daemon {
        return run_build_via_daemon(root, &request);
    }
    let graph = load_graph(root, &request.profile, &request.platform)?;
    let toolchain = toolchain_closure_fingerprint_cached(root, &graph.toolchain)?;
    let mut requested = if request.test_mode && request.targets.is_empty() {
        graph
            .targets
            .values()
            .filter(|target| matches!(target.kind, TargetKind::CcTest | TargetKind::Test))
            .map(|target| target.name.clone())
            .collect::<Vec<_>>()
    } else {
        resolve_targets(&graph, request.targets)?
    };
    if request.test_mode && requested.is_empty() {
        bail!("workspace declares no cc_test or test targets");
    }
    for name in &requested {
        if request.test_mode
            && !matches!(
                graph.targets[name].kind,
                TargetKind::CcTest | TargetKind::Test
            )
        {
            bail!("{name:?} is not a test target");
        }
    }
    if request.test_mode && (request.affected || request.predictive) {
        let all_closure = graph.action_closure(&requested)?;
        let plan = Engine::new(
            root,
            &graph,
            all_closure,
            toolchain.clone(),
            BuildOptions {
                jobs: request.jobs.unwrap_or_else(default_jobs),
                keep_going: true,
                dry_run: true,
                ..BuildOptions::default()
            },
        )
        .run()?;
        let affected = plan
            .results
            .iter()
            .filter(|result| {
                result.id.starts_with("test:")
                    && matches!(
                        result.outcome,
                        Outcome::WouldRun { .. } | Outcome::MayRun { .. }
                    )
            })
            .map(|result| result.id.trim_start_matches("test:").to_string())
            .collect::<std::collections::BTreeSet<_>>();
        requested.retain(|target| affected.contains(target));
        if requested.is_empty() {
            println!("tests: 0 passed, 0 failed, 0 cached (no affected tests)");
            return Ok(0);
        }
        if request.explain {
            println!("affected tests: {}", requested.join(", "));
        }
    }
    let closure = graph.action_closure(&requested)?;
    let opts = BuildOptions {
        jobs: request.jobs.unwrap_or_else(default_jobs),
        keep_going: request.keep_going,
        dry_run: false,
        verbose: request.verbose,
        no_cache: request.no_cache,
        sandbox: request.sandbox,
        check_determinism: request.check_determinism,
        scheduler: match request.scheduler {
            SchedulerArg::CriticalPath => frostbuild_exec::Scheduler::CriticalPath,
            SchedulerArg::Fifo => frostbuild_exec::Scheduler::Fifo,
        },
        estimator: match request.estimator {
            EstimatorArg::Heuristic => frostbuild_exec::Estimator::Heuristic,
            EstimatorArg::Journal => frostbuild_exec::Estimator::Journal,
            EstimatorArg::Static => frostbuild_exec::Estimator::Static,
            EstimatorArg::Learned => frostbuild_exec::Estimator::Learned,
        },
        ..BuildOptions::default()
    };

    let started = Instant::now();
    let total = closure.len();
    let report = Engine::new(root, &graph, closure, toolchain, opts).run()?;
    let elapsed = started.elapsed().as_millis();

    if request.explain {
        println!("explain:");
        for result in &report.results {
            match &result.outcome {
                Outcome::Executed { reason, .. } => println!("  ran {} :: {reason}", result.id),
                Outcome::Cached => println!("  cached {}", result.id),
                Outcome::Failed { reason, .. } => println!("  failed {} :: {reason}", result.id),
                Outcome::Skipped { reason } => println!("  skipped {} :: {reason}", result.id),
                Outcome::WouldRun { .. } | Outcome::MayRun { .. } => {}
            }
        }
    }

    if let Some(trace) = request.trace {
        write_trace(root, trace, &report)?;
    }

    let failed = report.failed();
    let skipped = report.count(|outcome| matches!(outcome, Outcome::Skipped { .. }));
    println!(
        "{}",
        summarize(
            report.executed(),
            report.cached(),
            failed,
            skipped,
            total,
            graph.actions.len(),
            elapsed,
        )
    );
    if request.stats {
        let st = &report.stats;
        println!(
            "  strategy    {} / {}  (-j {})",
            st.scheduler, st.estimator, st.jobs
        );
        // Scheduling statistics describe how work was spread across workers.
        // A run that executed nothing has none to describe, and printing
        // "0 ms, 0.0%, 0.00x" reads like something went wrong.
        if st.executed == 0 {
            println!("  scheduling  nothing ran, so there was nothing to schedule");
        } else {
            println!(
                "  makespan    {} ms, {} ms of work across {} actions",
                st.makespan_ms, st.busy_ms, st.executed
            );
            println!(
                "  utilization {:.0}% of worker capacity",
                st.utilization_pct()
            );
            // makespan / critical path. Near 1 the graph is the limit; above
            // it there is ordering to win back; below it the estimate simply
            // over-predicted, and saying anything about scheduling on that
            // basis would be a claim the numbers do not support.
            match st.critical_path_ratio() {
                Some(ratio) if ratio < 0.95 => println!(
                    "  critical    {} ms estimated, longer than the run itself — \
                     the recorded durations are stale, so run again to compare",
                    st.critical_path_ms
                ),
                Some(ratio) if ratio <= 1.05 => println!(
                    "  critical    {} ms estimated — the dependency graph bounds \
                     this build, so no scheduler can improve it",
                    st.critical_path_ms
                ),
                Some(ratio) => println!(
                    "  critical    {} ms estimated, {:.1}x under the run — that \
                     gap is what a better schedule could win",
                    st.critical_path_ms, ratio
                ),
                None => {}
            }
        }
    }
    if failed > 0 {
        println!("failure summary (first 10):");
        for result in report
            .results
            .iter()
            .filter(|result| matches!(result.outcome, Outcome::Failed { .. }))
            .take(10)
        {
            if let Outcome::Failed { detail, .. } = &result.outcome {
                println!(
                    "  {}: {}",
                    result.id,
                    detail.lines().next().unwrap_or("failed")
                );
            }
        }
    }
    if request.test_mode {
        let tests = report
            .results
            .iter()
            .filter(|result| result.id.starts_with("test:"));
        let (mut passed, mut test_failed, mut cached) = (0, 0, 0);
        for test in tests {
            match test.outcome {
                Outcome::Executed { .. } => passed += 1,
                Outcome::Cached => cached += 1,
                Outcome::Failed { .. } | Outcome::Skipped { .. } => test_failed += 1,
                Outcome::WouldRun { .. } | Outcome::MayRun { .. } => {}
            }
        }
        println!("tests: {passed} passed, {test_failed} failed, {cached} cached");
    }
    Ok(if frostbuild_exec::was_cancelled() {
        130
    } else if report.success() {
        0
    } else {
        1
    })
}

fn write_trace(
    root: &std::path::Path,
    destination: PathBuf,
    report: &frostbuild_exec::BuildReport,
) -> Result<()> {
    let mut timestamp = 0u64;
    let mut events = Vec::new();
    for result in &report.results {
        if let Outcome::Executed { duration_ms, .. } = result.outcome {
            events.push(serde_json::json!({
                "name": result.desc,
                "cat": "action",
                "ph": "X",
                "pid": 1,
                "tid": 1,
                "ts": timestamp,
                "dur": duration_ms * 1000,
                "args": { "id": result.id },
            }));
            timestamp += duration_ms * 1000;
        }
    }
    let path = if destination.is_absolute() {
        destination
    } else {
        root.join(destination)
    };
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({ "traceEvents": events }))?,
    )?;
    Ok(())
}

/// Load the configured graph, taking the manifest-free warm path when the
/// sources stamp proves the workspace inputs are unchanged; otherwise fall
/// back to a full manifest load and (re)compile.
fn load_graph(root: &std::path::Path, profile: &str, platform: &str) -> Result<BuildGraph> {
    if let Some(graph) = GraphStore::load_cached(root, profile, platform) {
        return Ok(graph);
    }
    let manifest = Manifest::load(root)?;
    GraphStore::load_or_compile_configured(root, &manifest, profile, platform)
}

fn resolve_targets(graph: &BuildGraph, requested: Vec<String>) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(graph.default_targets.clone());
    }
    for name in &requested {
        if !graph.targets.contains_key(name) {
            let known: Vec<&str> = graph.targets.keys().map(String::as_str).collect();
            if let Some(hint) = frostbuild_core::manifest::closest(name, known.iter().copied()) {
                bail!("unknown target {name:?}. did you mean {hint:?}?");
            }
            bail!(
                "unknown target {name:?}. known targets: {}",
                known.join(", ")
            );
        }
    }
    Ok(requested)
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Compare scheduling strategies by planning them, not by running them.
///
/// Every strategy is scored against the durations recorded in the journal, so
/// the numbers are deterministic and no cache is touched. Simulation models
/// ordering, not contention: treat it as "which strategy orders this graph
/// best", and calibrate absolute times against a real `build --stats` run.
fn run_simulate(
    root: &std::path::Path,
    targets: Vec<String>,
    jobs: Option<Vec<usize>>,
    profile: &str,
    platform: &str,
    json: bool,
) -> Result<i32> {
    use frostbuild_bench::{render_table, Sweep, ESTIMATORS, SCHEDULERS};
    use frostbuild_exec::Schedule;

    let graph = load_graph(root, profile, platform)?;
    let requested = resolve_targets(&graph, targets)?;
    let closure = graph.action_closure(&requested)?;
    if closure.is_empty() {
        bail!("nothing to simulate: the requested targets have no actions");
    }
    let journal = Journal::load(root);
    let host = default_jobs();
    let jobs = jobs.unwrap_or_else(|| {
        [1, 2, 4, 8, 16]
            .into_iter()
            .filter(|&j| j <= host.max(1))
            .collect()
    });
    let jobs = if jobs.is_empty() { vec![1] } else { jobs };

    let sweep = Sweep::run(&jobs, &SCHEDULERS, &ESTIMATORS, |scheduler, estimator| {
        Schedule::plan(&graph, closure.clone(), &journal, scheduler, estimator)
    });

    let recorded = graph
        .actions
        .iter()
        .filter(|a| {
            journal
                .actions
                .get(&frostbuild_exec::journal_id(&graph, a))
                .is_some_and(|e| e.duration_ms > 0)
        })
        .count();

    if json {
        let points: Vec<_> = sweep
            .points
            .iter()
            .map(|p| {
                serde_json::json!({
                    "scheduler": p.scheduler.as_str(),
                    "estimator": p.estimator.as_str(),
                    "jobs": p.simulation.jobs,
                    "makespan_ms": p.simulation.makespan_ms,
                    "utilization_pct": p.simulation.utilization_pct(),
                    "over_critical_path_pct": p.simulation.over_critical_path_pct(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "actions": sweep.actions,
                "actions_with_recorded_duration": recorded,
                "critical_path_ms": sweep.critical_path_ms,
                "work_ms": sweep.work_ms,
                "points": points,
            }))?
        );
        return Ok(0);
    }

    println!(
        "frost: simulating {} actions from the journal (no build, no cache writes)",
        sweep.actions
    );
    if recorded < sweep.actions {
        println!(
            "  note: {} of {} actions have no recorded duration; those fall back to estimates",
            sweep.actions - recorded,
            sweep.actions
        );
    }
    println!();
    println!(
        "  critical path  {} ms   total work  {} ms",
        sweep.critical_path_ms, sweep.work_ms
    );
    println!("  no schedule can finish faster than the critical path.");
    println!();
    print!("{}", render_table(&sweep));
    println!();
    if let Some(best) = sweep.best() {
        let over = best
            .simulation
            .over_critical_path_pct()
            .map(|p| format!("{p:.0}% above the critical path"))
            .unwrap_or_else(|| "critical path unknown".to_string());
        println!(
            "  fastest: {} / {} at -j {} -> {} ms ({}, {:.0}% worker utilization)",
            best.scheduler.as_str(),
            best.estimator.as_str(),
            best.simulation.jobs,
            best.simulation.makespan_ms,
            over,
            best.simulation.utilization_pct()
        );
    }
    println!("  compare against a real run: frost build --stats -j <n>");
    Ok(0)
}

/// The line every build ends with, and the one people actually read.
///
/// It leads with what happened rather than a fixed set of counters, and drops
/// every term that is zero: a build where nothing needed doing says so in
/// three words instead of reporting four zeroes. The action count and the
/// share of the graph left out of this build appear only when they say
/// something — a full build of everything does not need to be told it built
/// everything.
fn summarize(
    executed: usize,
    cached: usize,
    failed: usize,
    skipped: usize,
    selected: usize,
    total_in_graph: usize,
    elapsed_ms: u128,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if executed > 0 {
        parts.push(format!("{executed} built"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    // A run where everything was already current is the common case and
    // deserves words, not a row of counters.
    let headline = if parts.is_empty() && cached > 0 {
        "up to date".to_string()
    } else {
        if cached > 0 {
            parts.push(format!("{cached} cached"));
        }
        if parts.is_empty() {
            "nothing to do".to_string()
        } else {
            parts.join(", ")
        }
    };

    let pruned = total_in_graph.saturating_sub(selected);
    let scope = if pruned > 0 {
        format!("{selected} of {total_in_graph} actions")
    } else {
        format!("{selected} actions")
    };
    format!("frost: {headline} · {scope} · {elapsed_ms} ms")
}

#[cfg(test)]
mod summary_tests {
    use super::summarize;

    #[test]
    fn says_what_happened_and_omits_what_did_not() {
        assert_eq!(
            summarize(0, 5, 0, 0, 5, 5, 12),
            "frost: up to date · 5 actions · 12 ms"
        );
        assert_eq!(
            summarize(5, 0, 0, 0, 5, 5, 70),
            "frost: 5 built · 5 actions · 70 ms"
        );
        assert_eq!(
            summarize(2, 3, 0, 0, 5, 5, 40),
            "frost: 2 built, 3 cached · 5 actions · 40 ms"
        );
        // Failures lead, because that is what the reader needs first.
        assert_eq!(
            summarize(0, 3, 1, 1, 5, 5, 20),
            "frost: 1 failed, 1 skipped, 3 cached · 5 actions · 20 ms"
        );
        // Building a subset is worth saying; building everything is not.
        assert_eq!(
            summarize(0, 2, 0, 0, 2, 9, 5),
            "frost: up to date · 2 of 9 actions · 5 ms"
        );
        assert_eq!(
            summarize(0, 0, 0, 0, 0, 0, 1),
            "frost: nothing to do · 0 actions · 1 ms"
        );
    }
}
