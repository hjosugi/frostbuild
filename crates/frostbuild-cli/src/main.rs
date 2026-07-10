use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use frostbuild_core::graph::{BuildGraph, BIN_DIR, LIB_DIR, OBJ_DIR};
use frostbuild_core::manifest::{Manifest, TargetKind};
use frostbuild_exec::{toolchain_fingerprint, BuildOptions, Engine, Outcome};

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
    },
    /// Show which actions would run and why, without executing anything
    Plan { targets: Vec<String> },
    /// Remove build outputs (--cache also removes the journal and hash cache)
    Clean {
        #[arg(long)]
        cache: bool,
    },
    /// Print the target dependency graph
    Graph {
        /// Emit Graphviz dot instead of text
        #[arg(long)]
        dot: bool,
    },
}

fn main() {
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
        } => {
            let manifest = Manifest::load(&root)?;
            let graph = BuildGraph::from_manifest(&manifest)?;
            let requested = resolve_targets(&manifest, targets)?;
            let closure = graph.action_closure(&requested)?;
            let toolchain = toolchain_fingerprint(&manifest.toolchain.cc)?;
            let opts = BuildOptions {
                jobs: jobs.unwrap_or_else(default_jobs),
                keep_going,
                dry_run: false,
                verbose,
            };

            let started = Instant::now();
            let total = closure.len();
            let report = Engine::new(&root, &graph, closure, toolchain, opts).run()?;
            let elapsed = started.elapsed().as_millis();

            if explain {
                println!("explain:");
                for r in &report.results {
                    match &r.outcome {
                        Outcome::Executed { reason, .. } => {
                            println!("  ran {} :: {reason}", r.id)
                        }
                        Outcome::Cached => println!("  cached {}", r.id),
                        Outcome::Failed { reason, .. } => {
                            println!("  failed {} :: {reason}", r.id)
                        }
                        Outcome::Skipped { reason } => {
                            println!("  skipped {} :: {reason}", r.id)
                        }
                        Outcome::WouldRun { .. } | Outcome::MayRun { .. } => {}
                    }
                }
            }

            let failed = report.failed();
            let skipped = report.count(|o| matches!(o, Outcome::Skipped { .. }));
            let mut summary = format!(
                "frost: {} executed, {} cached",
                report.executed(),
                report.cached()
            );
            if failed > 0 || skipped > 0 {
                summary.push_str(&format!(", {failed} failed, {skipped} skipped"));
            }
            summary.push_str(&format!(" ({total} actions) in {elapsed} ms"));
            println!("{summary}");

            Ok(if report.success() { 0 } else { 1 })
        }
        Cmd::Plan { targets } => {
            let manifest = Manifest::load(&root)?;
            let graph = BuildGraph::from_manifest(&manifest)?;
            let requested = resolve_targets(&manifest, targets)?;
            let closure = graph.action_closure(&requested)?;
            let toolchain = toolchain_fingerprint(&manifest.toolchain.cc)?;
            let opts = BuildOptions {
                jobs: default_jobs(),
                keep_going: true,
                dry_run: true,
                verbose: false,
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
        Cmd::Clean { cache } => {
            let manifest = Manifest::load(&root)?;
            let graph = BuildGraph::from_manifest(&manifest)?;

            let mut removed = 0usize;
            for dir in [OBJ_DIR, LIB_DIR, BIN_DIR] {
                let path = root.join(dir);
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
                for rel in [".frost/journal.json", ".frost/hashcache.json"] {
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
        Cmd::Graph { dot } => {
            let manifest = Manifest::load(&root)?;
            let graph = BuildGraph::from_manifest(&manifest)?;
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
    }
}

fn resolve_targets(manifest: &Manifest, requested: Vec<String>) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(manifest.default_targets.clone());
    }
    for name in &requested {
        if !manifest.targets.contains_key(name) {
            let known: Vec<&str> = manifest.targets.keys().map(String::as_str).collect();
            bail!(
                "unknown target {name:?} (known targets: {})",
                known.join(", ")
            );
        }
    }
    Ok(requested)
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}
