use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{ArgValueCompleter, CompletionCandidate};
use frostbuild_core::graph::{ActionKind, BuildGraph, BIN_DIR, LIB_DIR, OBJ_DIR};
use frostbuild_core::graph_store::GraphStore;
use frostbuild_core::journal::Journal;
use frostbuild_core::manifest::{Manifest, TargetKind};
use frostbuild_exec::{
    toolchain_closure_fingerprint_cached, try_fast_noop, BuildOptions, Engine, Outcome,
};
use notify::{RecursiveMode, Watcher};

mod bazel;
mod jar;
mod npm;
mod progress;
mod wheel;

#[derive(Parser)]
#[command(
    name = "frost",
    version,
    about = "frostbuild: correct, fast incremental builds"
)]
struct Cli {
    /// Workspace root (frost.toml for Frost commands; Bazel workspace for bazel-dev)
    #[arg(short = 'C', long = "workspace", default_value = ".", global = true)]
    workspace: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build targets (default: workspace default_targets)
    Build {
        #[arg(add = ArgValueCompleter::new(complete_target))]
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
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        /// Target platform from [platform.<name>] for cross/device builds;
        /// outputs and caches are isolated per platform
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform),
            conflicts_with = "all_platforms"
        )]
        platform: String,
        /// Build host and every declared [platform.*] configuration
        #[arg(long)]
        all_platforms: bool,
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
        /// Disable the interactive terminal UI and print plain progress lines
        #[arg(long)]
        no_tui: bool,
        /// Execute through the per-workspace frostd service
        #[arg(long)]
        daemon: bool,
        #[arg(long, value_enum, default_value = "critical-path")]
        scheduler: SchedulerArg,
        #[arg(long, value_enum, default_value = "journal")]
        estimator: EstimatorArg,
    },
    /// Build one target and execute its native or language artifact
    Run {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        target: Option<String>,
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
        /// Explicit executable prefix for cross/emulated or custom artifacts
        #[arg(long)]
        runner: Option<PathBuf>,
        /// Print the exact direct argv without executing it
        #[arg(long)]
        print: bool,
        /// Arguments passed to the built program (after `--`)
        #[arg(last = true)]
        program_args: Vec<String>,
    },
    /// Rebuild on source/manifest changes and optionally restart a dev process
    Watch {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        targets: Vec<String>,
        /// Number of parallel build jobs (default: number of CPUs)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
        /// Quiet period used to coalesce editor save events
        #[arg(long, default_value_t = 50)]
        debounce_ms: u64,
        /// Direct argv to start after a successful build and restart on success;
        /// place this option last when its arguments begin with '-'
        #[arg(long, num_args = 1.., allow_hyphen_values = true)]
        run: Vec<String>,
    },
    /// Watch one runnable target and restart its inferred artifact on success
    Dev {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        target: Option<String>,
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
        #[arg(long, default_value_t = 50)]
        debounce_ms: u64,
        /// Explicit executable prefix for cross/emulated or custom artifacts
        #[arg(long)]
        runner: Option<PathBuf>,
        /// Arguments passed to the restarted program (after `--`)
        #[arg(last = true)]
        program_args: Vec<String>,
    },
    /// Build one target and launch its native or language debugger
    Debug {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        target: Option<String>,
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
        /// Debugger/runtime executable, or auto for GDB/LLDB, jdb, Node or pdb
        #[arg(long, default_value = "auto")]
        debugger: String,
        /// Print the exact debugger argv without launching it
        #[arg(long)]
        print: bool,
        /// Arguments passed to the program being debugged (after `--`)
        #[arg(last = true)]
        program_args: Vec<String>,
    },
    /// Build one target and generate VS Code build/debug configuration
    Ide {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        target: Option<String>,
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
        /// Workspace-relative VS Code directory
        #[arg(long, default_value = ".vscode")]
        output: PathBuf,
        /// Print the generated file map without writing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Diagnose workspace, required tools and optional developer integrations
    Doctor {
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Build and run test/cc_test targets
    Test {
        #[arg(add = ArgValueCompleter::new(complete_test_target))]
        targets: Vec<String>,
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        #[arg(short = 'k', long)]
        keep_going: bool,
        #[arg(long)]
        affected: bool,
        #[arg(long)]
        predictive: bool,
        #[arg(long, conflicts_with_all = ["affected", "predictive"])]
        all: bool,
        #[arg(long)]
        no_cache: bool,
        #[arg(long)]
        explain: bool,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform),
            conflicts_with = "all_platforms"
        )]
        platform: String,
        /// Test host and every declared [platform.*] configuration
        #[arg(long)]
        all_platforms: bool,
        #[arg(long)]
        sandbox: bool,
        /// Disable the interactive terminal UI and print plain progress lines
        #[arg(long)]
        no_tui: bool,
        #[arg(long)]
        daemon: bool,
        #[arg(long, value_enum, default_value = "critical-path")]
        scheduler: SchedulerArg,
        #[arg(long, value_enum, default_value = "journal")]
        estimator: EstimatorArg,
    },
    /// Show which actions would run and why, without executing anything
    Plan {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        targets: Vec<String>,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
    },
    /// Remove build outputs (--cache also removes the journal and hash cache)
    Clean {
        #[arg(long)]
        cache: bool,
        #[arg(long, add = ArgValueCompleter::new(complete_profile))]
        profile: Option<String>,
        #[arg(long, add = ArgValueCompleter::new(complete_platform))]
        platform: Option<String>,
    },
    /// Print the target dependency graph
    Graph {
        /// Emit Graphviz dot instead of text
        #[arg(long)]
        dot: bool,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
    },
    /// Export JSON Compilation Database for clangd/IDE integrations
    Compdb {
        #[arg(long, default_value = "compile_commands.json")]
        output: PathBuf,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
    },
    /// Explain the most recently recorded decision for a target
    Explain {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        target: String,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
    },
    /// Write a safe native C/C++ or Java starter frost.toml from sources here
    Init {
        /// Print the manifest instead of writing it
        #[arg(long)]
        dry_run: bool,
        /// Source family; omit to auto-detect (mixed families require a choice)
        #[arg(long, value_enum)]
        language: Option<InitLanguage>,
    },
    /// Compare scheduling strategies without building anything
    Simulate {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        targets: Vec<String>,
        /// Worker counts to sweep (default: 1,2,4,8,16 capped at this host)
        #[arg(long, value_delimiter = ',')]
        jobs: Option<Vec<usize>>,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
        #[arg(long)]
        json: bool,
    },
    /// Query the target dependency graph (configuration-free)
    Query {
        #[command(subcommand)]
        function: QueryCmd,
    },
    /// Inspect local content-addressed cache storage and chunk reuse
    Cache {
        #[command(subcommand)]
        command: CacheCmd,
    },
    /// Manage the per-workspace build daemon
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
    /// Import a conservative native C/C++ subset from Bazel query XML
    ImportBazel {
        /// Bazel query expression to import
        #[arg(long, default_value = "//...")]
        query: String,
        /// Bazel or Bazelisk executable (defaults to BAZEL_BIN, bazel, bazelisk)
        #[arg(long)]
        bazel: Option<PathBuf>,
        /// Print every generated manifest without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Import non-interactive npm workspace scripts as cached test gates
    ImportNpm {
        /// Package script to import; repeat or comma-separate
        #[arg(long = "script", value_delimiter = ',')]
        scripts: Vec<String>,
        /// npm executable recorded as a fingerprinted named tool
        #[arg(long, default_value = "npm")]
        npm: PathBuf,
        /// Node executable recorded with npm's toolchain closure
        #[arg(long, default_value = "node")]
        node: PathBuf,
        /// Print the generated root manifest without writing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Watch, incrementally build, and restart a Bazel runnable target
    BazelDev {
        /// Canonical Bazel runnable label, for example //app:server
        target: String,
        /// Bazel or Bazelisk executable (defaults to BAZEL_BIN, bazel, bazelisk)
        #[arg(long)]
        bazel: Option<PathBuf>,
        /// Quiet period used to coalesce editor filesystem events
        #[arg(long, default_value_t = 50)]
        debounce_ms: u64,
        /// Build option forwarded to both `bazel build` and `bazel run`
        #[arg(long = "bazel-arg", allow_hyphen_values = true)]
        bazel_args: Vec<String>,
        /// Arguments passed to the target after `--`
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Pack a directory into a deterministic compressed Java archive
    PackJar {
        /// Workspace-relative directory whose contents become JAR entries
        #[arg(long)]
        input: PathBuf,
        /// Workspace-relative output JAR
        #[arg(long)]
        output: PathBuf,
        /// Optional Java binary name for the Main-Class manifest attribute
        #[arg(long)]
        main_class: Option<String>,
    },
    /// Pack a pure-Python source tree into a deterministic standards-compliant wheel
    PackWheel {
        /// Workspace-relative source root whose contents install into purelib
        #[arg(long)]
        input: PathBuf,
        /// Python distribution name written to wheel metadata
        #[arg(long)]
        distribution: String,
        /// Normalized numeric Python release version (for example 1.2.3)
        #[arg(long)]
        version: String,
        /// Workspace-relative output wheel (must use the standard wheel filename)
        #[arg(long)]
        output: PathBuf,
    },
    /// Generate completion code for a shell
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Select build or test targets interactively with fzf
    Pick {
        /// Select only test targets and run `frost test`
        #[arg(long)]
        tests: bool,
        /// Print selected labels instead of building
        #[arg(long)]
        print: bool,
        #[arg(
            long,
            default_value = "debug",
            add = ArgValueCompleter::new(complete_profile)
        )]
        profile: String,
        #[arg(
            long,
            default_value = frostbuild_core::manifest::HOST_PLATFORM,
            add = ArgValueCompleter::new(complete_platform)
        )]
        platform: String,
    },
}

#[derive(Subcommand)]
enum QueryCmd {
    /// Transitive dependencies of a target (itself included)
    Deps {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Targets that transitively depend on a target ("what does this affect?")
    Rdeps {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// One dependency path between two targets
    Somepath {
        #[arg(add = ArgValueCompleter::new(complete_target))]
        from: String,
        #[arg(add = ArgValueCompleter::new(complete_target))]
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

#[derive(Subcommand)]
enum CacheCmd {
    /// Report blob/chunk storage and persistent deduplication ratios
    Stats {
        /// Emit one machine-readable JSON object
        #[arg(long)]
        json: bool,
    },
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

#[derive(Clone, Copy, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
    Nushell,
}

#[derive(Clone, Copy, ValueEnum)]
enum InitLanguage {
    Native,
    Java,
}

fn completion_workspace() -> PathBuf {
    let args: Vec<_> = std::env::args_os().collect();
    let mut selected = None;
    for (index, arg) in args.iter().enumerate() {
        if arg == "-C" || arg == "--workspace" {
            selected = args.get(index + 1).map(PathBuf::from);
        } else if let Some(value) = arg
            .to_str()
            .and_then(|arg| arg.strip_prefix("--workspace="))
        {
            selected = Some(PathBuf::from(value));
        } else if let Some(value) = arg
            .to_str()
            .filter(|arg| arg.starts_with("-C") && arg.len() > 2)
            .map(|arg| &arg[2..])
        {
            selected = Some(PathBuf::from(value));
        }
    }
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    selected
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                current.join(path)
            }
        })
        .unwrap_or(current)
}

fn candidates(
    current: &OsStr,
    values: impl IntoIterator<Item = String>,
) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    values
        .into_iter()
        .filter(|value| value.starts_with(current))
        .map(CompletionCandidate::new)
        .collect()
}

fn completion_manifest() -> Option<Manifest> {
    Manifest::load(&completion_workspace()).ok()
}

fn complete_target(current: &OsStr) -> Vec<CompletionCandidate> {
    candidates(
        current,
        completion_manifest()
            .into_iter()
            .flat_map(|manifest| manifest.targets.into_keys()),
    )
}

fn complete_test_target(current: &OsStr) -> Vec<CompletionCandidate> {
    candidates(
        current,
        completion_manifest().into_iter().flat_map(|manifest| {
            manifest.targets.into_values().filter_map(|target| {
                matches!(target.kind, TargetKind::CcTest | TargetKind::Test).then_some(target.name)
            })
        }),
    )
}

fn complete_profile(current: &OsStr) -> Vec<CompletionCandidate> {
    let mut values = vec![frostbuild_core::manifest::DEFAULT_PROFILE.to_string()];
    if let Some(manifest) = completion_manifest() {
        values.extend(manifest.profiles.into_keys());
    }
    values.sort();
    values.dedup();
    candidates(current, values)
}

fn complete_platform(current: &OsStr) -> Vec<CompletionCandidate> {
    let mut values = vec![frostbuild_core::manifest::HOST_PLATFORM.to_string()];
    if let Some(manifest) = completion_manifest() {
        values.extend(manifest.platforms.into_keys());
    }
    values.sort();
    values.dedup();
    candidates(current, values)
}

fn print_completions(shell: CompletionShell) {
    let mut command = Cli::command();
    match shell {
        CompletionShell::Bash => clap_complete::generate(
            clap_complete::Shell::Bash,
            &mut command,
            "frost",
            &mut std::io::stdout(),
        ),
        CompletionShell::Zsh => clap_complete::generate(
            clap_complete::Shell::Zsh,
            &mut command,
            "frost",
            &mut std::io::stdout(),
        ),
        CompletionShell::Fish => clap_complete::generate(
            clap_complete::Shell::Fish,
            &mut command,
            "frost",
            &mut std::io::stdout(),
        ),
        CompletionShell::Powershell => clap_complete::generate(
            clap_complete::Shell::PowerShell,
            &mut command,
            "frost",
            &mut std::io::stdout(),
        ),
        CompletionShell::Elvish => clap_complete::generate(
            clap_complete::Shell::Elvish,
            &mut command,
            "frost",
            &mut std::io::stdout(),
        ),
        CompletionShell::Nushell => clap_complete::generate(
            clap_complete_nushell::Nushell,
            &mut command,
            "frost",
            &mut std::io::stdout(),
        ),
    }
}

#[cfg(windows)]
fn main() {
    // Windows executables default to a 1 MiB main-thread stack.  Constructing
    // the full clap command tree can exceed that in debug builds as the CLI
    // grows, so run the actual entry point on an explicitly sized stack.  Keep
    // this Windows-only to avoid adding thread startup to Unix no-op latency.
    match std::thread::Builder::new()
        .name("frost-main".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(frost_main)
    {
        Ok(worker) => {
            if worker.join().is_err() {
                eprintln!("frost: main thread panicked");
                std::process::exit(2);
            }
        }
        Err(error) => {
            eprintln!("frost: failed to start main thread: {error}");
            std::process::exit(2);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    frost_main();
}

fn frost_main() {
    // Dynamic completion scripts call back into this binary, allowing target,
    // profile and platform candidates to reflect the current frost.toml.
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .bin("frost")
        .complete();
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

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
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
            all_platforms,
            no_cache,
            sandbox,
            check_determinism,
            trace,
            stats,
            no_tui,
            daemon,
            scheduler,
            estimator,
        } => run_build_selected(
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
                no_tui,
                test_mode: false,
                daemon,
                affected: false,
                predictive: false,
                all: false,
                scheduler,
                estimator,
            },
            all_platforms,
        ),
        Cmd::Run {
            target,
            jobs,
            profile,
            platform,
            runner,
            print,
            program_args,
        } => run_target(
            &root,
            target,
            jobs,
            profile,
            platform,
            runner,
            print,
            program_args,
        ),
        Cmd::Watch {
            targets,
            jobs,
            profile,
            platform,
            debounce_ms,
            run,
        } => run_watch(
            &root,
            WatchRequest {
                targets,
                jobs,
                profile,
                platform,
                debounce: Duration::from_millis(debounce_ms),
                run,
                auto_run: None,
            },
        ),
        Cmd::Dev {
            target,
            jobs,
            profile,
            platform,
            debounce_ms,
            runner,
            program_args,
        } => run_dev(
            &root,
            target,
            jobs,
            profile,
            platform,
            Duration::from_millis(debounce_ms),
            runner,
            program_args,
        ),
        Cmd::Debug {
            target,
            jobs,
            profile,
            platform,
            debugger,
            print,
            program_args,
        } => run_debug(
            &root,
            target,
            jobs,
            profile,
            platform,
            debugger,
            print,
            program_args,
        ),
        Cmd::Ide {
            target,
            jobs,
            profile,
            platform,
            output,
            dry_run,
        } => run_ide(&root, target, jobs, profile, platform, output, dry_run),
        Cmd::Doctor {
            profile,
            platform,
            json,
        } => run_doctor(&root, &profile, &platform, json),
        Cmd::Test {
            targets,
            jobs,
            keep_going,
            affected,
            predictive,
            all,
            no_cache,
            explain,
            profile,
            platform,
            all_platforms,
            sandbox,
            no_tui,
            daemon,
            scheduler,
            estimator,
        } => run_build_selected(
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
                no_tui,
                test_mode: true,
                daemon,
                affected,
                predictive,
                all,
                scheduler,
                estimator,
            },
            all_platforms,
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
            // Validate explicitly selected names before touching anything.
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
            // Genrule/command outputs may live outside the native .frost
            // trees. With no selector, or a platform-only selector, expand
            // every configuration whose outputs the native tree removal above
            // covers as well.
            let mut configured_graphs = vec![graph];
            if profile.is_none() {
                let manifest = Manifest::load(&root)?;
                let mut profiles = vec![frostbuild_core::manifest::DEFAULT_PROFILE.to_string()];
                profiles.extend(manifest.profiles.keys().cloned());
                profiles.sort();
                profiles.dedup();
                let mut platforms = if let Some(platform) = &platform {
                    vec![platform.clone()]
                } else {
                    let mut values = vec![frostbuild_core::manifest::HOST_PLATFORM.to_string()];
                    values.extend(manifest.platforms.keys().cloned());
                    values
                };
                platforms.sort();
                platforms.dedup();
                for configured_platform in platforms {
                    for configured_profile in &profiles {
                        if configured_platform == active_platform
                            && configured_profile == active_profile
                        {
                            continue;
                        }
                        configured_graphs.push(load_graph(
                            &root,
                            configured_profile,
                            &configured_platform,
                        )?);
                    }
                }
            }
            let mut external_outputs = std::collections::BTreeSet::new();
            let mut intermediate_dirs = std::collections::BTreeSet::new();
            for graph in &configured_graphs {
                for target in graph.targets.values() {
                    if matches!(target.kind, TargetKind::Genrule | TargetKind::Command) {
                        external_outputs.extend(
                            target
                                .outputs
                                .iter()
                                .map(|&out| graph.files[out].path.clone()),
                        );
                        for &action in &target.actions {
                            intermediate_dirs
                                .extend(graph.actions[action].clean_dirs.iter().cloned());
                        }
                    }
                }
            }
            for output in external_outputs {
                let path = root.join(output);
                if path.exists() {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                    removed += 1;
                }
            }
            for directory in intermediate_dirs {
                let path = root.join(directory);
                if path.exists() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                    removed += 1;
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
                if let Ok(entries) = std::fs::read_dir(root.join(".frost")) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name.starts_with("noop-") && name.ends_with(".bin") {
                            std::fs::remove_file(entry.path())?;
                            removed += 1;
                        }
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
        Cmd::Init { dry_run, language } => run_init(&root, dry_run, language),
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
        Cmd::Cache { command } => match command {
            CacheCmd::Stats { json } => {
                let stats = frostbuild_core::cas::LocalCas::new(
                    &root,
                    frostbuild_exec::DEFAULT_CAS_MAX_BYTES,
                )
                .stats()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&stats)?);
                } else {
                    println!("frost: local CAS");
                    println!(
                        "|-- blobs     {:>8} · {}",
                        stats.object_count,
                        human_bytes(stats.object_bytes)
                    );
                    println!(
                        "|-- chunks    {:>8} · {}",
                        stats.chunk_count,
                        human_bytes(stats.chunk_bytes)
                    );
                    println!(
                        "|-- deltas    {:>8} · {}",
                        stats.delta_count,
                        human_bytes(stats.delta_bytes)
                    );
                    println!("|-- manifests {:>8}", stats.manifest_count);
                    println!(
                        "`-- reuse      {:>7.2}% · {} / {} logical bytes",
                        stats.chunk_reuse_ratio * 100.0,
                        human_bytes(stats.reused_chunk_bytes),
                        human_bytes(stats.logical_chunk_bytes)
                    );
                }
                Ok(0)
            }
        },
        Cmd::Daemon { command } => daemon_command(&root, command),
        Cmd::ImportNinja { ninja, output } => import_ninja(&root, ninja, output),
        Cmd::ImportBazel {
            query,
            bazel,
            dry_run,
        } => bazel::run_import(&root, &query, bazel.as_deref(), dry_run),
        Cmd::ImportNpm {
            scripts,
            npm,
            node,
            dry_run,
        } => npm::run_import(&root, &scripts, &npm, &node, dry_run),
        Cmd::BazelDev {
            target,
            bazel,
            debounce_ms,
            bazel_args,
            args,
        } => bazel::run_dev(
            &root,
            &target,
            bazel.as_deref(),
            Duration::from_millis(debounce_ms),
            &bazel_args,
            &args,
        ),
        Cmd::PackJar {
            input,
            output,
            main_class,
        } => {
            let entries = jar::pack(&root, &input, &output, main_class.as_deref())?;
            println!(
                "frost: packed {entries} files -> {}",
                output.to_string_lossy()
            );
            Ok(0)
        }
        Cmd::PackWheel {
            input,
            distribution,
            version,
            output,
        } => {
            let entries = wheel::pack(&root, &input, &distribution, &version, &output)?;
            println!(
                "frost: packed {entries} files -> {}",
                output.to_string_lossy()
            );
            Ok(0)
        }
        Cmd::Completions { shell } => {
            print_completions(shell);
            Ok(0)
        }
        Cmd::Pick {
            tests,
            print,
            profile,
            platform,
        } => run_pick(&root, tests, print, profile, platform),
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

#[derive(Clone)]
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
    no_tui: bool,
    test_mode: bool,
    daemon: bool,
    affected: bool,
    predictive: bool,
    all: bool,
    scheduler: SchedulerArg,
    estimator: EstimatorArg,
}

struct WatchRequest {
    targets: Vec<String>,
    jobs: Option<usize>,
    profile: String,
    platform: String,
    debounce: Duration,
    run: Vec<String>,
    auto_run: Option<AutoRun>,
}

struct AutoRun {
    target: String,
    runner: Option<PathBuf>,
    program_args: Vec<String>,
}

#[derive(Default)]
struct WatchExclusions {
    outputs: BTreeSet<PathBuf>,
    clean_dirs: Vec<PathBuf>,
}

fn watch_build_request(request: &WatchRequest) -> BuildRequest {
    BuildRequest {
        targets: request.targets.clone(),
        jobs: request.jobs,
        keep_going: true,
        explain: false,
        verbose: false,
        profile: request.profile.clone(),
        platform: request.platform.clone(),
        no_cache: false,
        sandbox: false,
        check_determinism: false,
        trace: None,
        stats: false,
        no_tui: false,
        test_mode: false,
        daemon: false,
        affected: false,
        predictive: false,
        all: false,
        scheduler: SchedulerArg::CriticalPath,
        estimator: EstimatorArg::Journal,
    }
}

fn watch_exclusions(root: &Path, profile: &str, platform: &str) -> WatchExclusions {
    let Ok(graph) = load_graph(root, profile, platform) else {
        return WatchExclusions::default();
    };
    WatchExclusions {
        outputs: graph
            .actions
            .iter()
            .flat_map(|action| action.outputs.iter())
            .map(|&output| PathBuf::from(&graph.files[output].path))
            .collect(),
        clean_dirs: graph
            .actions
            .iter()
            .flat_map(|action| action.clean_dirs.iter())
            .map(PathBuf::from)
            .collect(),
    }
}

fn relevant_watch_path(root: &Path, path: &Path, exclusions: &WatchExclusions) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if relative.as_os_str().is_empty()
        || relative.starts_with(".frost")
        || relative.starts_with(".git")
        || exclusions.outputs.contains(relative)
        || exclusions
            .clean_dirs
            .iter()
            .any(|directory| relative.starts_with(directory))
    {
        return None;
    }
    Some(relative.to_path_buf())
}

fn watch_event_changes_files(kind: &notify::EventKind) -> bool {
    matches!(
        kind,
        notify::EventKind::Any
            | notify::EventKind::Create(_)
            | notify::EventKind::Modify(_)
            | notify::EventKind::Remove(_)
    )
}

fn stop_dev_process(child: &mut Option<Child>) {
    let Some(mut running) = child.take() else {
        return;
    };
    if running.try_wait().ok().flatten().is_some() {
        return;
    }
    let pid = running.id();
    #[cfg(unix)]
    unsafe {
        // The process was placed in its own group immediately before spawn.
        // Terminating the group also stops language servers, web servers and
        // Bazel-run children that would otherwise survive a hot restart.
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    for _ in 0..20 {
        if running.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let _ = running.kill();
    let _ = running.wait();
}

fn configure_dev_command(_command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        _command.process_group(0);
    }
}

fn restart_dev_process(root: &Path, argv: &[String], child: &mut Option<Child>) -> Result<()> {
    if argv.is_empty() {
        return Ok(());
    }
    stop_dev_process(child);
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(root);
    configure_dev_command(&mut command);
    let running = command
        .spawn()
        .with_context(|| format!("failed to start watch process {:?}", argv))?;
    println!("`-- dev process restarted · pid {}", running.id());
    *child = Some(running);
    Ok(())
}

fn watch_run_argv(root: &Path, request: &WatchRequest) -> Result<Vec<String>> {
    if !request.run.is_empty() {
        return Ok(request.run.clone());
    }
    let Some(auto) = &request.auto_run else {
        return Ok(Vec::new());
    };
    let graph = load_graph(root, &request.profile, &request.platform)?;
    let output = root.join(target_runtime_output(&graph, &auto.target)?);
    anyhow::ensure!(
        output.is_file(),
        "dev target output {} was not produced",
        output.display()
    );
    runtime_argv(root, &output, auto.runner.as_deref(), &auto.program_args).map(|(argv, _)| argv)
}

#[allow(clippy::too_many_arguments)]
fn run_dev(
    root: &Path,
    target: Option<String>,
    jobs: Option<usize>,
    profile: String,
    platform: String,
    debounce: Duration,
    runner: Option<PathBuf>,
    program_args: Vec<String>,
) -> Result<i32> {
    let graph = load_graph(root, &profile, &platform)?;
    let targets = resolve_targets(&graph, target.into_iter().collect())?;
    anyhow::ensure!(
        targets.len() == 1,
        "dev requires exactly one target; choose one of: {}",
        targets.join(", ")
    );
    if platform != frostbuild_core::manifest::HOST_PLATFORM && runner.is_none() {
        bail!(
            "cannot execute platform {platform:?} on the host directly; pass --runner for an emulator"
        );
    }
    run_watch(
        root,
        WatchRequest {
            targets: targets.clone(),
            jobs,
            profile,
            platform,
            debounce,
            run: Vec::new(),
            auto_run: Some(AutoRun {
                target: targets[0].clone(),
                runner,
                program_args,
            }),
        },
    )
}

fn run_watch(root: &Path, request: WatchRequest) -> Result<i32> {
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    println!(
        "frost: watch · profile {} · platform {} · debounce {} ms",
        request.profile,
        request.platform,
        request.debounce.as_millis()
    );
    println!("|-- initial build");
    let mut child = None;
    match run_build(root, watch_build_request(&request)) {
        Ok(0) => {
            let argv = watch_run_argv(root, &request);
            if let Err(error) = argv.and_then(|argv| restart_dev_process(root, &argv, &mut child)) {
                eprintln!("|   dev process: {error:#}");
            }
        }
        Ok(code) => eprintln!("|   initial build failed (exit {code}); watching for a fix"),
        Err(error) => eprintln!("|   initial build failed: {error:#}; watching for a fix"),
    }
    println!("`-- ready · Ctrl-C stops");

    let mut exclusions = watch_exclusions(root, &request.profile, &request.platform);
    let mut change_set = 0usize;
    while !frostbuild_exec::was_cancelled() {
        if let Some(running) = child.as_mut() {
            if let Some(status) = running.try_wait()? {
                println!("frost: dev process exited · {status}");
                child = None;
            }
        }

        let first = match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => {
                eprintln!("frost: watch error: {error}");
                continue;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("filesystem watcher stopped"),
        };
        let mut changed = if watch_event_changes_files(&first.kind) {
            first
                .paths
                .iter()
                .filter_map(|path| relevant_watch_path(root, path, &exclusions))
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let mut deadline = Instant::now() + request.debounce;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match receiver.recv_timeout(remaining) {
                Ok(Ok(event)) => {
                    let before = changed.len();
                    if watch_event_changes_files(&event.kind) {
                        changed.extend(
                            event
                                .paths
                                .iter()
                                .filter_map(|path| relevant_watch_path(root, path, &exclusions)),
                        );
                    }
                    if changed.len() > before {
                        deadline = Instant::now() + request.debounce;
                    }
                }
                Ok(Err(error)) => eprintln!("frost: watch error: {error}"),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => bail!("filesystem watcher stopped"),
            }
        }
        if changed.is_empty() {
            continue;
        }

        change_set += 1;
        println!(
            "frost: change #{change_set} · {} path{}",
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

        match run_build(root, watch_build_request(&request)) {
            Ok(0) => {
                exclusions = watch_exclusions(root, &request.profile, &request.platform);
                let argv = watch_run_argv(root, &request);
                if let Err(error) =
                    argv.and_then(|argv| restart_dev_process(root, &argv, &mut child))
                {
                    eprintln!("frost: dev process: {error:#}");
                }
            }
            Ok(code) => eprintln!(
                "frost: build failed (exit {code}); keeping the last successful dev process"
            ),
            Err(error) => {
                eprintln!("frost: build failed: {error:#}; keeping the last successful dev process")
            }
        }
    }
    stop_dev_process(&mut child);
    println!("frost: watch stopped");
    Ok(130)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| is_executable_file(candidate))
    })
}

fn resolve_program(root: &Path, selected: PathBuf, label: &str) -> Result<PathBuf> {
    if selected.is_absolute() || selected.components().count() > 1 {
        let resolved = if selected.is_absolute() {
            selected
        } else {
            root.join(selected)
        };
        anyhow::ensure!(
            is_executable_file(&resolved),
            "{label} {} does not exist",
            resolved.display()
        );
        return Ok(resolved);
    }
    find_on_path(selected.to_string_lossy().as_ref())
        .with_context(|| format!("{label} {:?} was not found on PATH", selected))
}

fn select_debugger(root: &Path, requested: &str) -> Result<PathBuf> {
    let selected = if requested == "auto" {
        if let Some(configured) = std::env::var_os("FROST_DEBUGGER") {
            PathBuf::from(configured)
        } else {
            find_on_path("gdb")
                .or_else(|| find_on_path("lldb"))
                .context("no debugger found; install gdb/lldb or pass --debugger PATH")?
        }
    } else {
        PathBuf::from(requested)
    };
    resolve_program(root, selected, "debugger")
}

fn debugger_argv(debugger: &Path, binary: &Path, program_args: &[String]) -> Vec<String> {
    let name = debugger
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut argv = vec![debugger.display().to_string()];
    if name.contains("lldb") {
        argv.push("--".into());
    } else {
        argv.push("--args".into());
    }
    argv.push(binary.display().to_string());
    argv.extend(program_args.iter().cloned());
    argv
}

fn target_runtime_output(graph: &BuildGraph, target: &str) -> Result<PathBuf> {
    let closure = graph.action_closure(&[target.to_string()])?;
    let link_output = closure
        .iter()
        .map(|&action| &graph.actions[action])
        .find(|action| action.kind == ActionKind::Link)
        .and_then(|action| action.outputs.first())
        .copied();
    let output = link_output
        .or_else(|| graph.targets[target].outputs.first().copied())
        .context("target has no runnable output")?;
    Ok(PathBuf::from(&graph.files[output].path))
}

fn select_language_debugger(
    root: &Path,
    requested: &str,
    environment: &str,
    candidates: &[&str],
) -> Result<PathBuf> {
    if requested != "auto" {
        return select_debugger(root, requested);
    }
    if let Some(configured) = std::env::var_os(environment) {
        return select_debugger(root, Path::new(&configured).to_string_lossy().as_ref());
    }
    candidates
        .iter()
        .find_map(|candidate| find_on_path(candidate))
        .with_context(|| {
            format!(
                "no {} debugger found; install {} or pass --debugger PATH",
                candidates.join("/"),
                candidates.join(" or ")
            )
        })
}

fn jar_main_class(path: &Path) -> Result<String> {
    use std::io::Read as _;

    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open JAR {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("invalid JAR {}", path.display()))?;
    let mut raw = String::new();
    archive
        .by_name("META-INF/MANIFEST.MF")
        .context("JAR has no META-INF/MANIFEST.MF")?
        .read_to_string(&mut raw)
        .context("JAR manifest is not UTF-8")?;
    let mut unfolded = Vec::<String>::new();
    for line in raw.lines() {
        if let Some(continuation) = line.strip_prefix(' ') {
            let previous = unfolded
                .last_mut()
                .context("JAR manifest starts with a continuation line")?;
            previous.push_str(continuation);
        } else {
            unfolded.push(line.trim_end_matches('\r').to_string());
        }
    }
    unfolded
        .into_iter()
        .find_map(|line| line.strip_prefix("Main-Class: ").map(str::to_string))
        .context("JAR has no Main-Class; add pack-jar --main-class or use a direct command")
}

fn language_debug_argv(
    root: &Path,
    requested: &str,
    output: &Path,
    program_args: &[String],
) -> Result<(PathBuf, Vec<String>, &'static str)> {
    let extension = output
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "jar" => {
            let debugger = select_language_debugger(root, requested, "JDB_BIN", &["jdb"])?;
            let main_class = jar_main_class(output)?;
            let mut argv = vec![
                debugger.display().to_string(),
                "-classpath".into(),
                output.display().to_string(),
                main_class,
            ];
            argv.extend(program_args.iter().cloned());
            Ok((debugger, argv, "Java/jdb"))
        }
        "js" | "mjs" | "cjs" => {
            let debugger = select_language_debugger(root, requested, "NODE_BIN", &["node"])?;
            let mut argv = vec![
                debugger.display().to_string(),
                "inspect".into(),
                output.display().to_string(),
            ];
            argv.extend(program_args.iter().cloned());
            Ok((debugger, argv, "JavaScript/Node inspector"))
        }
        "py" | "pyw" => {
            let debugger =
                select_language_debugger(root, requested, "PYTHON_BIN", &["python3", "python"])?;
            let mut argv = vec![
                debugger.display().to_string(),
                "-m".into(),
                "pdb".into(),
                output.display().to_string(),
            ];
            argv.extend(program_args.iter().cloned());
            Ok((debugger, argv, "Python/pdb"))
        }
        _ => {
            let debugger = select_debugger(root, requested)?;
            let argv = debugger_argv(&debugger, output, program_args);
            Ok((debugger, argv, "native"))
        }
    }
}

fn select_runtime(root: &Path, environment: &str, candidates: &[&str]) -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os(environment) {
        return resolve_program(root, PathBuf::from(configured), "runtime");
    }
    candidates
        .iter()
        .find_map(|candidate| find_on_path(candidate))
        .with_context(|| format!("runtime {} was not found on PATH", candidates.join("/")))
}

fn runtime_argv(
    root: &Path,
    output: &Path,
    runner: Option<&Path>,
    program_args: &[String],
) -> Result<(Vec<String>, &'static str)> {
    if let Some(runner) = runner {
        let runner = resolve_program(root, runner.to_path_buf(), "runner")?;
        let mut argv = vec![runner.display().to_string(), output.display().to_string()];
        argv.extend(program_args.iter().cloned());
        return Ok((argv, "explicit runner"));
    }
    let extension = output
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let (mut argv, flavor) = match extension.as_str() {
        "jar" => {
            let java = select_runtime(root, "JAVA_BIN", &["java"])?;
            (
                vec![
                    java.display().to_string(),
                    "-jar".into(),
                    output.display().to_string(),
                ],
                "Java",
            )
        }
        "js" | "mjs" | "cjs" => {
            let node = select_runtime(root, "NODE_BIN", &["node"])?;
            (
                vec![node.display().to_string(), output.display().to_string()],
                "JavaScript",
            )
        }
        "py" | "pyw" => {
            let python = select_runtime(root, "PYTHON_BIN", &["python3", "python"])?;
            (
                vec![python.display().to_string(), output.display().to_string()],
                "Python",
            )
        }
        "whl" => bail!(
            "a wheel is installable, not directly runnable; select a runnable target or pass --runner"
        ),
        _ => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(output)?.permissions().mode();
                anyhow::ensure!(
                    mode & 0o111 != 0,
                    "output {} is not executable; use --runner for a custom artifact",
                    output.display()
                );
            }
            (vec![output.display().to_string()], "native")
        }
    };
    argv.extend(program_args.iter().cloned());
    Ok((argv, flavor))
}

#[allow(clippy::too_many_arguments)]
fn run_target(
    root: &Path,
    target: Option<String>,
    jobs: Option<usize>,
    profile: String,
    platform: String,
    runner: Option<PathBuf>,
    print: bool,
    program_args: Vec<String>,
) -> Result<i32> {
    let graph = load_graph(root, &profile, &platform)?;
    let targets = resolve_targets(&graph, target.into_iter().collect())?;
    anyhow::ensure!(
        targets.len() == 1,
        "run requires exactly one target; choose one of: {}",
        targets.join(", ")
    );
    let target = targets[0].clone();
    if platform != frostbuild_core::manifest::HOST_PLATFORM && runner.is_none() {
        bail!(
            "cannot execute platform {platform:?} on the host directly; pass --runner for an emulator"
        );
    }
    let build_code = run_build(
        root,
        BuildRequest {
            targets: vec![target.clone()],
            jobs,
            keep_going: false,
            explain: false,
            verbose: false,
            profile: profile.clone(),
            platform: platform.clone(),
            no_cache: false,
            sandbox: false,
            check_determinism: false,
            trace: None,
            stats: false,
            no_tui: false,
            test_mode: false,
            daemon: false,
            affected: false,
            predictive: false,
            all: false,
            scheduler: SchedulerArg::CriticalPath,
            estimator: EstimatorArg::Journal,
        },
    )?;
    if build_code != 0 {
        return Ok(build_code);
    }
    let graph = load_graph(root, &profile, &platform)?;
    let output = root.join(target_runtime_output(&graph, &target)?);
    anyhow::ensure!(
        output.is_file(),
        "run target output {} was not produced",
        output.display()
    );
    let (argv, flavor) = runtime_argv(root, &output, runner.as_deref(), &program_args)?;
    println!("frost: run");
    println!("|-- target    {target}");
    println!("|-- artifact  {}", output.display());
    println!("|-- profile   {profile} / {platform}");
    println!("`-- runtime   {flavor}");
    if print {
        println!("{}", serde_json::to_string(&argv)?);
        return Ok(0);
    }
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(root)
        .status()
        .context("failed to run built artifact")?;
    Ok(status.code().unwrap_or(1))
}

fn vscode_files(
    root: &Path,
    graph: &BuildGraph,
    target: &str,
    profile: &str,
    platform: &str,
    artifact: &Path,
) -> Result<(serde_json::Value, serde_json::Value, &'static str)> {
    let relative = artifact
        .strip_prefix(root)
        .with_context(|| format!("artifact {} is outside the workspace", artifact.display()))?;
    let artifact_variable = format!(
        "${{workspaceFolder}}/{}",
        relative.to_string_lossy().replace('\\', "/")
    );
    let task_label = format!("frost: build {target}");
    let extension = artifact
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let launch = match extension.as_str() {
        "jar" => serde_json::json!({
            "name": format!("Frost: debug {target}"),
            "type": "java",
            "request": "launch",
            "mainClass": jar_main_class(artifact)?,
            "classPaths": [artifact_variable],
            "cwd": "${workspaceFolder}",
            "preLaunchTask": task_label,
            "args": []
        }),
        "js" | "mjs" | "cjs" => {
            let closure = graph.action_closure(&[target.to_string()])?;
            let source_maps = closure.iter().any(|&action| {
                graph.actions[action].outputs.iter().any(|&output| {
                    graph.files[output]
                        .path
                        .to_ascii_lowercase()
                        .ends_with(".map")
                })
            });
            serde_json::json!({
                "name": format!("Frost: debug {target}"),
                "type": "node",
                "request": "launch",
                "program": artifact_variable,
                "cwd": "${workspaceFolder}",
                "preLaunchTask": task_label,
                "sourceMaps": source_maps,
                "args": []
            })
        }
        "py" | "pyw" => serde_json::json!({
            "name": format!("Frost: debug {target}"),
            "type": "debugpy",
            "request": "launch",
            "program": artifact_variable,
            "cwd": "${workspaceFolder}",
            "preLaunchTask": task_label,
            "args": []
        }),
        "whl" => bail!("a wheel has no direct IDE launch configuration; choose a runnable target"),
        _ => serde_json::json!({
            "name": format!("Frost: debug {target}"),
            "type": "cppdbg",
            "request": "launch",
            "program": artifact_variable,
            "cwd": "${workspaceFolder}",
            "preLaunchTask": task_label,
            "MIMode": if cfg!(target_os = "macos") { "lldb" } else { "gdb" },
            "args": [],
            "stopAtEntry": false
        }),
    };
    let problem_matcher = if matches!(
        extension.as_str(),
        "jar" | "js" | "mjs" | "cjs" | "py" | "pyw"
    ) {
        serde_json::json!([])
    } else {
        serde_json::json!(["$gcc"])
    };
    let tasks = serde_json::json!({
        "version": "2.0.0",
        "tasks": [{
            "label": task_label,
            "type": "process",
            "command": "frost",
            "args": [
                "-C", "${workspaceFolder}", "build", target,
                "--profile", profile, "--platform", platform, "--no-tui"
            ],
            "options": { "cwd": "${workspaceFolder}" },
            "problemMatcher": problem_matcher,
            "group": { "kind": "build", "isDefault": true }
        }]
    });
    let launches = serde_json::json!({
        "version": "0.2.0",
        "configurations": [launch]
    });
    let flavor = match extension.as_str() {
        "jar" => "Java",
        "js" | "mjs" | "cjs" => "JavaScript",
        "py" | "pyw" => "Python",
        _ => "native",
    };
    Ok((tasks, launches, flavor))
}

fn run_ide(
    root: &Path,
    target: Option<String>,
    jobs: Option<usize>,
    profile: String,
    platform: String,
    output: PathBuf,
    dry_run: bool,
) -> Result<i32> {
    let graph = load_graph(root, &profile, &platform)?;
    let targets = resolve_targets(&graph, target.into_iter().collect())?;
    anyhow::ensure!(
        targets.len() == 1,
        "ide requires exactly one target; choose one of: {}",
        targets.join(", ")
    );
    let target = targets[0].clone();
    let build_code = run_build(
        root,
        BuildRequest {
            targets: vec![target.clone()],
            jobs,
            keep_going: false,
            explain: false,
            verbose: false,
            profile: profile.clone(),
            platform: platform.clone(),
            no_cache: false,
            sandbox: false,
            check_determinism: false,
            trace: None,
            stats: false,
            no_tui: false,
            test_mode: false,
            daemon: false,
            affected: false,
            predictive: false,
            all: false,
            scheduler: SchedulerArg::CriticalPath,
            estimator: EstimatorArg::Journal,
        },
    )?;
    if build_code != 0 {
        return Ok(build_code);
    }
    let graph = load_graph(root, &profile, &platform)?;
    let artifact = root.join(target_runtime_output(&graph, &target)?);
    anyhow::ensure!(
        artifact.is_file(),
        "IDE artifact {} was not produced",
        artifact.display()
    );
    let (tasks, launch, flavor) =
        vscode_files(root, &graph, &target, &profile, &platform, &artifact)?;
    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "tasks.json": tasks,
                "launch.json": launch,
            }))?
        );
        return Ok(0);
    }
    let output_text = output
        .to_str()
        .context("non-UTF-8 IDE output path is not supported")?;
    let relative = frostbuild_core::paths::validate_rel_path(output_text)
        .context("IDE output must be a workspace-relative directory")?;
    let directory = root.join(relative);
    let tasks_path = directory.join("tasks.json");
    let launch_path = directory.join("launch.json");
    for path in [&tasks_path, &launch_path] {
        anyhow::ensure!(
            !path.exists(),
            "{} already exists; use --dry-run and merge the Frost entry instead of overwriting it",
            path.display()
        );
    }
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    std::fs::write(&tasks_path, serde_json::to_vec_pretty(&tasks)?)?;
    std::fs::write(&launch_path, serde_json::to_vec_pretty(&launch)?)?;
    println!("frost: IDE configuration");
    println!("|-- target   {target} ({flavor})");
    println!("|-- task     {}", tasks_path.display());
    println!("`-- launch   {}", launch_path.display());
    Ok(0)
}

#[derive(serde::Serialize)]
struct DoctorTool {
    name: String,
    configured: String,
    resolved: Option<String>,
    available: bool,
    required: bool,
}

fn inspect_tool(root: &Path, name: &str, configured: &str, required: bool) -> DoctorTool {
    let selected = PathBuf::from(configured);
    let resolved = if selected.is_absolute() || selected.components().count() > 1 {
        let candidate = if selected.is_absolute() {
            selected
        } else {
            root.join(selected)
        };
        is_executable_file(&candidate).then_some(candidate)
    } else {
        find_on_path(configured)
    };
    DoctorTool {
        name: name.to_string(),
        configured: configured.to_string(),
        available: resolved.is_some(),
        resolved: resolved.map(|path| path.display().to_string()),
        required,
    }
}

fn run_doctor(root: &Path, profile: &str, platform: &str, json: bool) -> Result<i32> {
    let graph = load_graph(root, profile, platform)?;
    let mut required = vec![
        inspect_tool(root, "C compiler", &graph.toolchain.cc, true),
        inspect_tool(root, "C++ compiler", &graph.toolchain.cxx, true),
        inspect_tool(root, "archiver", &graph.toolchain.ar, true),
        inspect_tool(root, "shell", frostbuild_core::graph::SHELL, true),
    ];
    if let Some(kofunc) = &graph.toolchain.kofunc {
        required.push(inspect_tool(root, "Kofun compiler", kofunc, true));
    }
    required.extend(
        graph
            .toolchain
            .tools
            .iter()
            .map(|(name, tool)| inspect_tool(root, &format!("tool:{name}"), tool, true)),
    );
    let extras = [
        ("fuzzy target picker", "fzf"),
        ("native debugger (GDB)", "gdb"),
        ("native debugger (LLDB)", "lldb"),
        ("Java debugger", "jdb"),
        ("Java runtime", "java"),
        ("Node runtime/debugger", "node"),
        ("Python runtime/debugger", "python3"),
        ("Linux sandbox", "bwrap"),
        ("Graphviz rendering", "dot"),
    ]
    .into_iter()
    .map(|(name, tool)| inspect_tool(root, name, tool, false))
    .collect::<Vec<_>>();
    let ready = required.iter().all(|tool| tool.available);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": if ready { "ready" } else { "blocked" },
                "workspace": root,
                "profile": profile,
                "platform": platform,
                "targets": graph.targets.len(),
                "actions": graph.actions.len(),
                "required_tools": required,
                "optional_integrations": extras,
            }))?
        );
        return Ok(if ready { 0 } else { 1 });
    }

    println!(
        "frost: doctor · {}",
        if ready { "ready" } else { "action required" }
    );
    println!("|-- workspace  {}", root.display());
    println!("|-- config     {profile} / {platform}");
    println!(
        "|-- graph      {} targets / {} actions",
        graph.targets.len(),
        graph.actions.len()
    );
    println!("|-- required tools");
    for (index, tool) in required.iter().enumerate() {
        let branch = if index + 1 == required.len() {
            "|   `--"
        } else {
            "|   |--"
        };
        let location = tool.resolved.as_deref().unwrap_or(&tool.configured);
        println!(
            "{branch} {:<20} {:<7} {location}",
            tool.name,
            if tool.available { "ok" } else { "missing" }
        );
    }
    println!("|-- optional integrations");
    for (index, tool) in extras.iter().enumerate() {
        let branch = if index + 1 == extras.len() {
            "|   `--"
        } else {
            "|   |--"
        };
        println!(
            "{branch} {:<24} {}",
            tool.name,
            if tool.available {
                "available"
            } else {
                "not installed"
            }
        );
    }
    println!(
        "`-- result     {}",
        if ready {
            "build prerequisites are ready"
        } else {
            "install or correct every missing required tool"
        }
    );
    Ok(if ready { 0 } else { 1 })
}

#[allow(clippy::too_many_arguments)]
fn run_debug(
    root: &Path,
    target: Option<String>,
    jobs: Option<usize>,
    profile: String,
    platform: String,
    debugger: String,
    print: bool,
    program_args: Vec<String>,
) -> Result<i32> {
    let graph = load_graph(root, &profile, &platform)?;
    let targets = resolve_targets(&graph, target.into_iter().collect())?;
    anyhow::ensure!(
        targets.len() == 1,
        "debug requires exactly one target; choose one of: {}",
        targets.join(", ")
    );
    let target = targets[0].clone();
    let closure = graph.action_closure(std::slice::from_ref(&target))?;
    let compile_actions = closure
        .iter()
        .map(|&action| &graph.actions[action])
        .filter(|action| action.kind == ActionKind::Compile)
        .collect::<Vec<_>>();
    if !compile_actions.is_empty()
        && compile_actions.iter().any(|action| {
            !action
                .argv
                .iter()
                .any(|arg| arg == "/Zi" || arg == "/Z7" || arg == "-ggdb" || arg.starts_with("-g"))
        })
    {
        bail!(
            "target {target:?} is not compiled with debug symbols in profile {profile:?}; \
             add [profile.{profile}] cflags = [\"-O0\", \"-g\"]"
        );
    }

    let build_code = run_build(
        root,
        BuildRequest {
            targets: vec![target.clone()],
            jobs,
            keep_going: false,
            explain: false,
            verbose: false,
            profile: profile.clone(),
            platform: platform.clone(),
            no_cache: false,
            sandbox: false,
            check_determinism: false,
            trace: None,
            stats: false,
            no_tui: false,
            test_mode: false,
            daemon: false,
            affected: false,
            predictive: false,
            all: false,
            scheduler: SchedulerArg::CriticalPath,
            estimator: EstimatorArg::Journal,
        },
    )?;
    if build_code != 0 {
        return Ok(build_code);
    }

    let graph = load_graph(root, &profile, &platform)?;
    let binary = root.join(target_runtime_output(&graph, &target)?);
    anyhow::ensure!(
        binary.is_file(),
        "debug target output {} was not produced",
        binary.display()
    );
    let (debugger, argv, flavor) = language_debug_argv(root, &debugger, &binary, &program_args)?;
    println!("frost: debug");
    println!("|-- target    {target}");
    println!("|-- binary    {}", binary.display());
    println!("|-- profile   {profile} / {platform}");
    println!("|-- mode      {flavor}");
    println!("`-- debugger  {}", debugger.display());
    if print {
        println!("{}", serde_json::to_string(&argv)?);
        return Ok(0);
    }
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(root)
        .status()
        .context("failed to launch debugger")?;
    Ok(status.code().unwrap_or(1))
}

fn run_build_selected(
    root: &std::path::Path,
    request: BuildRequest,
    all_platforms: bool,
) -> Result<i32> {
    if !all_platforms {
        return run_build(root, request);
    }

    let manifest = Manifest::load(root)?;
    let mut platforms = vec![frostbuild_core::manifest::HOST_PLATFORM.to_string()];
    platforms.extend(manifest.platforms.into_keys());
    println!(
        "frost: multi-platform build ({} platforms, profile {})",
        platforms.len(),
        request.profile
    );

    let mut results = Vec::with_capacity(platforms.len());
    for platform in platforms {
        println!("+-- {platform}");
        let mut configured = request.clone();
        configured.platform = platform.clone();
        match run_build(root, configured) {
            Ok(code) => results.push((platform, code)),
            Err(error) => {
                eprintln!("|   error: {error:#}");
                results.push((platform, 2));
            }
        }
    }

    println!("frost: platform summary");
    for (index, (platform, code)) in results.iter().enumerate() {
        let branch = if index + 1 == results.len() {
            "`--"
        } else {
            "|--"
        };
        println!(
            "{branch} {platform:<16} {}",
            if *code == 0 { "ok" } else { "failed" }
        );
    }
    Ok(results.iter().map(|(_, code)| *code).max().unwrap_or(0))
}

fn run_pick(
    root: &std::path::Path,
    tests: bool,
    print: bool,
    profile: String,
    platform: String,
) -> Result<i32> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let manifest = Manifest::load(root)?;
    let rows: Vec<String> = manifest
        .targets
        .values()
        .filter(|target| !tests || matches!(target.kind, TargetKind::CcTest | TargetKind::Test))
        .map(|target| {
            let deps = if target.deps.is_empty() {
                "-".to_string()
            } else {
                target.deps.join(",")
            };
            format!("{}\t{}\t{}", target.name, target.kind.as_str(), deps)
        })
        .collect();
    if rows.is_empty() {
        bail!(
            "this workspace has no {}targets to select",
            if tests { "test " } else { "" }
        );
    }

    let prompt = if tests {
        "frost test > "
    } else {
        "frost build > "
    };
    let mut child = Command::new("fzf")
        .args([
            "--multi",
            "--height=70%",
            "--layout=reverse",
            "--border=rounded",
            "--delimiter=\t",
            "--with-nth=1,2,3",
            "--header=TAB: multi-select  ENTER: confirm  ESC: cancel",
            "--prompt",
            prompt,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context(
            "fzf was not found. install fzf, or use shell completion and pass target names directly",
        )?;
    {
        let stdin = child.stdin.as_mut().context("failed to open fzf input")?;
        for row in &rows {
            writeln!(stdin, "{row}")?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        // fzf uses 1 for no match and 130 for an interactive cancel. Neither
        // should turn an intentional escape into a scary Frost error.
        return Ok(0);
    }
    let selected: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split('\t').next())
        .filter(|target| !target.is_empty())
        .map(str::to_string)
        .collect();
    if selected.is_empty() {
        return Ok(0);
    }
    if print {
        for target in selected {
            println!("{target}");
        }
        return Ok(0);
    }

    run_build(
        root,
        BuildRequest {
            targets: selected,
            jobs: None,
            keep_going: false,
            explain: false,
            verbose: false,
            profile,
            platform,
            no_cache: false,
            sandbox: false,
            check_determinism: false,
            trace: None,
            stats: false,
            no_tui: false,
            test_mode: tests,
            daemon: false,
            affected: false,
            predictive: false,
            all: false,
            scheduler: SchedulerArg::CriticalPath,
            estimator: EstimatorArg::Journal,
        },
    )
}

fn run_build_via_daemon(
    root: &std::path::Path,
    request: &BuildRequest,
    enable_fast_noop: bool,
) -> Result<i32> {
    use frostbuild_daemon::{FastNoopRequest, Request, PROTOCOL_VERSION};
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
    if request.no_tui {
        args.push("--no-tui".into());
    }
    if request.affected {
        args.push("--affected".into());
    }
    if request.predictive {
        args.push("--predictive".into());
    }
    if request.all {
        args.push("--all".into());
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
        fast_noop: enable_fast_noop.then(|| FastNoopRequest {
            profile: request.profile.clone(),
            platform: request.platform.clone(),
            key_env: frostbuild_exec::key_environment_snapshot(),
        }),
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
    let enable_fast_noop = !request.test_mode
        && request.targets.is_empty()
        && !request.keep_going
        && !request.explain
        && !request.verbose
        && !request.no_cache
        && !request.sandbox
        && !request.check_determinism
        && request.trace.is_none()
        && !request.stats
        && !request.affected
        && !request.predictive
        && !request.all;
    if request.daemon {
        return run_build_via_daemon(root, &request, enable_fast_noop);
    }
    if enable_fast_noop {
        let started = Instant::now();
        if let Some(hit) = try_fast_noop(root, &request.profile, &request.platform)? {
            println!(
                "{}",
                summarize(
                    0,
                    hit.closure_actions,
                    0,
                    0,
                    hit.closure_actions,
                    hit.graph_actions,
                    started.elapsed().as_millis(),
                )
            );
            return Ok(0);
        }
    }
    let graph = load_graph(root, &request.profile, &request.platform)?;
    let toolchain = toolchain_closure_fingerprint_cached(root, &graph.toolchain)?;
    let mut requested = if request.test_mode && (request.all || request.targets.is_empty()) {
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
    let (progress, renderer) = progress::start(request.no_tui, request.verbose);
    let opts = BuildOptions {
        jobs: request.jobs.unwrap_or_else(default_jobs),
        keep_going: request.keep_going,
        dry_run: false,
        verbose: request.verbose,
        no_cache: request.no_cache,
        sandbox: request.sandbox,
        check_determinism: request.check_determinism,
        write_fast_noop: enable_fast_noop,
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
        progress: Some(progress),
        ..BuildOptions::default()
    };

    let started = Instant::now();
    let total = closure.len();
    let report = Engine::new(root, &graph, closure, toolchain, opts).run();
    renderer.finish();
    let report = report?;
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

/// Write a starter manifest for a directory that has sources but no
/// `frost.toml`, so the first thing a newcomer runs is not a dead end.
fn run_init(root: &std::path::Path, dry_run: bool, language: Option<InitLanguage>) -> Result<i32> {
    let manifest_path = root.join(frostbuild_core::manifest::MANIFEST_FILE);
    if manifest_path.exists() && !dry_run {
        bail!(
            "{} already exists. delete it first, or use --dry-run to see what \
             init would write",
            manifest_path.display()
        );
    }
    let scaffold = match language {
        Some(InitLanguage::Native) => frostbuild_core::manifest::scaffold_for(
            root,
            frostbuild_core::manifest::ScaffoldLanguage::Native,
        )?,
        Some(InitLanguage::Java) => frostbuild_core::manifest::scaffold_for(
            root,
            frostbuild_core::manifest::ScaffoldLanguage::Java,
        )?,
        None => frostbuild_core::manifest::scaffold(root)?,
    };
    if dry_run {
        print!("{}", scaffold.manifest);
        return Ok(0);
    }
    std::fs::write(&manifest_path, &scaffold.manifest)?;
    println!("frost: wrote {}", manifest_path.display());
    for line in &scaffold.summary {
        println!("  {line}");
    }
    println!();
    println!("  read it before trusting it, then: frost build");
    Ok(0)
}

#[cfg(test)]
mod summary_tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use super::{
        jar, language_debug_argv, relevant_watch_path, summarize, watch_event_changes_files, Cli,
        Cmd, WatchExclusions,
    };

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

    #[test]
    fn watch_ignores_self_writes_but_keeps_sources_and_manifests() {
        let root = Path::new("/workspace");
        let exclusions = WatchExclusions {
            outputs: BTreeSet::from([PathBuf::from("dist/app.js")]),
            clean_dirs: vec![PathBuf::from("tmp/generated")],
        };
        for ignored in [
            ".frost/out/debug/app",
            ".git/index",
            "dist/app.js",
            "tmp/generated/member.js",
        ] {
            assert!(
                relevant_watch_path(root, &root.join(ignored), &exclusions).is_none(),
                "{ignored}"
            );
        }
        for watched in ["src/app.ts", "frost.toml", "dist/source.ts"] {
            assert_eq!(
                relevant_watch_path(root, &root.join(watched), &exclusions),
                Some(PathBuf::from(watched))
            );
        }
    }

    #[test]
    fn watch_ignores_read_access_but_keeps_content_events() {
        use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};
        use notify::EventKind;

        assert!(!watch_event_changes_files(&EventKind::Access(
            AccessKind::Any
        )));
        assert!(watch_event_changes_files(&EventKind::Create(
            CreateKind::Any
        )));
        assert!(watch_event_changes_files(&EventKind::Modify(
            ModifyKind::Any
        )));
        assert!(watch_event_changes_files(&EventKind::Remove(
            RemoveKind::Any
        )));
    }

    #[test]
    fn watch_parses_a_direct_dev_process_argv() {
        let cli = Cli::try_parse_from([
            "frost",
            "watch",
            "app",
            "--debounce-ms",
            "25",
            "--run",
            "node",
            "dist/app.js",
        ])
        .unwrap();
        let Cmd::Watch {
            targets,
            debounce_ms,
            run,
            ..
        } = cli.command
        else {
            panic!("watch command was not parsed")
        };
        assert_eq!(targets, vec!["app"]);
        assert_eq!(debounce_ms, 25);
        assert_eq!(run, vec!["node", "dist/app.js"]);
    }

    #[test]
    fn debug_selects_language_native_argv_without_a_shell() {
        let root = Path::new("/");
        let executable = std::env::current_exe().unwrap();
        let debugger = executable.to_string_lossy().into_owned();
        let (debugger, javascript, flavor) = language_debug_argv(
            root,
            &debugger,
            Path::new("/workspace/app.js"),
            &["--port".into(), "3000".into()],
        )
        .unwrap();
        assert_eq!(debugger, executable);
        assert_eq!(
            javascript,
            [
                debugger.to_string_lossy().as_ref(),
                "inspect",
                "/workspace/app.js",
                "--port",
                "3000"
            ]
        );
        assert_eq!(flavor, "JavaScript/Node inspector");

        let (_, python, flavor) = language_debug_argv(
            root,
            debugger.to_string_lossy().as_ref(),
            Path::new("/workspace/app.py"),
            &[],
        )
        .unwrap();
        assert_eq!(
            python,
            [
                debugger.to_string_lossy().as_ref(),
                "-m",
                "pdb",
                "/workspace/app.py"
            ]
        );
        assert_eq!(flavor, "Python/pdb");
    }

    #[test]
    fn debug_reads_an_executable_jars_main_class() {
        let root = std::env::temp_dir().join(format!("frost-debug-jar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("classes/pkg")).unwrap();
        std::fs::write(root.join("classes/pkg/Main.class"), b"class").unwrap();
        jar::pack(
            &root,
            Path::new("classes"),
            Path::new("out/app.jar"),
            Some("pkg.Main"),
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let debugger = executable.to_string_lossy();
        let (_, argv, flavor) = language_debug_argv(
            &root,
            &debugger,
            &root.join("out/app.jar"),
            &["argument".into()],
        )
        .unwrap();
        assert_eq!(
            argv,
            [
                debugger.as_ref(),
                "-classpath",
                root.join("out/app.jar").to_str().unwrap(),
                "pkg.Main",
                "argument"
            ]
        );
        assert_eq!(flavor, "Java/jdb");
        std::fs::remove_dir_all(root).ok();
    }
}
