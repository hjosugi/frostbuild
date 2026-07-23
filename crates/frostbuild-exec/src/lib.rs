//! Parallel build engine: dependency-counting scheduler, real process
//! execution, and constructive-trace action caching.
//!
//! Rebuild decision: an action is skipped when its action-key digest
//! (command + toolchain + content digests of declared and discovered inputs)
//! matches the journal entry from the last run AND its recorded outputs are
//! intact on disk. Because downstream keys are computed from upstream output
//! *content*, an action that re-runs but reproduces identical outputs stops
//! dirtiness from propagating (early cutoff).

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use frostbuild_core::cas::LocalCas;
use frostbuild_core::depfile;
use frostbuild_core::graph::{ActionId, ActionKind, BuildGraph};
use frostbuild_core::hashcache::HashCache;
use frostbuild_core::journal::{Journal, JournalEntry};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

mod fast_noop;
mod progress;
pub use fast_noop::{FastNoopDaemonHit, FastNoopHit, FastNoopWatchProof};
pub use progress::{progress_channel, ProgressEvent, ProgressSender, ProgressState};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static RUNNING_PROCESS_GROUPS: OnceLock<Mutex<BTreeSet<u32>>> = OnceLock::new();
static SIGNAL_HANDLER: OnceLock<()> = OnceLock::new();

/// Environment an action inherits whose value must not change its output.
/// These name scratch locations and the search path for tools; an action
/// whose result depends on them is not hermetic, which is what `--sandbox`
/// and `--check-determinism` exist to surface. Keying on them would rebuild
/// the world every time a shell exports a different TMPDIR.
///
/// PATH is here rather than in the key for a specific reason: its effect on
/// the compiler is already captured, because the toolchain fingerprint hashes
/// the *resolved* cc/cxx/ar binaries. What it does not capture is a genrule
/// invoking some other tool found on PATH — the same blind spot as any
/// undeclared input.
const ENV_PASSTHROUGH: &[&str] = &["PATH", "HOME", "TMPDIR", "TMP", "TEMP"];

/// Environment that changes what a compiler produces, so it belongs in the
/// action key. `CPATH=/a` and `CPATH=/b` select different headers with an
/// identical command line and identical declared inputs; without these in the
/// key, frost hands back the binary built against the other one.
const ENV_IN_KEY: &[&str] = &[
    "SystemRoot",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "LIBRARY_PATH",
];

pub const DEFAULT_CAS_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;

pub fn key_environment_snapshot() -> BTreeMap<String, String> {
    ENV_IN_KEY
        .iter()
        .filter_map(|name| {
            std::env::var_os(name)
                .map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

pub fn try_fast_noop(root: &Path, profile: &str, platform: &str) -> Result<Option<FastNoopHit>> {
    fast_noop::check(root, profile, platform, &key_environment_snapshot(), true)
}

/// Validate a certificate using key-affecting environment captured by a
/// client process. Arbitrary `pass_env` values are intentionally unavailable
/// to the daemon; certificates that depend on them return a miss and take the
/// normal child-build path.
pub fn try_fast_noop_with_key_environment(
    root: &Path,
    profile: &str,
    platform: &str,
    key_env: &BTreeMap<String, String>,
) -> Result<Option<FastNoopHit>> {
    fast_noop::check(root, profile, platform, key_env, false)
}

/// Fully validate a daemon certificate and return a watcher-cache proof only
/// when every recorded file can be covered by the workspace event stream.
pub fn try_fast_noop_for_daemon(
    root: &Path,
    profile: &str,
    platform: &str,
    key_env: &BTreeMap<String, String>,
) -> Result<Option<FastNoopDaemonHit>> {
    fast_noop::check_for_daemon(root, profile, platform, key_env, false)
}

/// Revalidate the non-workspace portion of a watcher-backed certificate.
pub fn try_fast_noop_from_watch_proof(
    root: &Path,
    profile: &str,
    platform: &str,
    key_env: &BTreeMap<String, String>,
    proof: &FastNoopWatchProof,
) -> Result<Option<FastNoopHit>> {
    fast_noop::check_watch_proof(root, profile, platform, key_env, proof)
}

pub fn install_signal_handler() -> Result<()> {
    if SIGNAL_HANDLER.get().is_some() {
        return Ok(());
    }
    ctrlc::set_handler(request_cancellation)?;
    let _ = SIGNAL_HANDLER.set(());
    Ok(())
}

/// Request the same cancellation performed by SIGINT. Interactive renderers
/// call this when raw terminal mode turns Ctrl-C into a key event.
pub fn request_cancellation() {
    CANCELLED.store(true, Ordering::SeqCst);
    if let Some(groups) = RUNNING_PROCESS_GROUPS.get() {
        for pid in groups.lock().unwrap().iter().copied() {
            terminate_process_tree(pid);
        }
    }
}

#[cfg(unix)]
fn terminate_process_tree(pid: u32) {
    // SAFETY: kill is async-process-safe; negative pid addresses the process
    // group created for this action immediately before it was spawned.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

#[cfg(windows)]
fn terminate_process_tree(pid: u32) {
    // taskkill is part of Windows and `/T` terminates descendants as well as
    // the direct compiler/test process. This keeps cancellation semantics
    // aligned with Unix process groups without requiring child handles to be
    // held behind the scheduler's shared lock.
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
}

pub fn was_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub jobs: usize,
    pub keep_going: bool,
    pub dry_run: bool,
    pub verbose: bool,
    pub no_cache: bool,
    pub sandbox: bool,
    pub check_determinism: bool,
    pub cas_max_bytes: u64,
    /// Persist a whole-closure certificate after the normal path proves that
    /// a plain default-target build is entirely cached.
    pub write_fast_noop: bool,
    pub scheduler: Scheduler,
    pub estimator: Estimator,
    /// Optional structured progress sink. The execution engine never renders
    /// terminal output itself; callers choose a TTY or plain-text renderer.
    pub progress: Option<ProgressSender>,
}

/// Ready-queue ordering. Both schedulers run the same actions and produce the
/// same outputs; they differ only in the order independent work is started,
/// which shows up as makespan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheduler {
    /// Start the action with the longest remaining dependency chain first.
    CriticalPath,
    /// Start whichever became ready first.
    Fifo,
}

/// How the scheduler guesses an action's duration. Only affects ordering, so a
/// bad estimate costs makespan, never correctness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Estimator {
    /// Fixed cost per action kind. No history needed.
    Heuristic,
    /// This action's own last recorded duration; heuristic when unseen.
    Journal,
    /// Every action costs the same, so priority is pure graph depth.
    Static,
    /// This action's own history when present, otherwise the median duration
    /// of the same kind across this workspace's journal. The difference from
    /// `Journal` is entirely in the unseen case — new and changed actions get
    /// a workspace-calibrated estimate instead of a hardcoded constant.
    Learned,
}

impl Scheduler {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheduler::CriticalPath => "critical-path",
            Scheduler::Fifo => "fifo",
        }
    }
}

impl Estimator {
    pub fn as_str(self) -> &'static str {
        match self {
            Estimator::Heuristic => "heuristic",
            Estimator::Journal => "journal",
            Estimator::Static => "static",
            Estimator::Learned => "learned",
        }
    }
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            jobs: std::thread::available_parallelism().map_or(1, |n| n.get()),
            keep_going: false,
            dry_run: false,
            verbose: false,
            no_cache: false,
            sandbox: false,
            check_determinism: false,
            cas_max_bytes: DEFAULT_CAS_MAX_BYTES,
            write_fast_noop: false,
            scheduler: Scheduler::CriticalPath,
            estimator: Estimator::Journal,
            progress: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Ran the command successfully.
    Executed { reason: String, duration_ms: u64 },
    /// Action key and outputs matched the journal; nothing to do.
    Cached,
    /// Dry run: this action would definitely run.
    WouldRun { reason: String },
    /// Dry run: upstream would run, so this action's inputs are unknowable.
    MayRun { reason: String },
    /// The command ran and failed.
    Failed { reason: String, detail: String },
    /// Not run because an upstream action failed or the build aborted.
    Skipped { reason: String },
}

#[derive(Debug, Clone)]
pub struct ActionResult {
    pub id: String,
    pub desc: String,
    pub outcome: Outcome,
}

#[derive(Debug, Default)]
pub struct BuildReport {
    /// One entry per closure action, in deterministic graph order.
    pub results: Vec<ActionResult>,
    /// Scheduling measurements, so two strategies can be compared from a
    /// single run rather than by wall-clock feel.
    pub stats: BuildStats,
}

/// What the chosen scheduler and estimator actually bought.
///
/// `busy_ms / (makespan_ms * jobs)` is the fraction of the available worker
/// time that was spent executing; the gap is idle workers waiting on the
/// dependency graph, which is exactly what a scheduler can improve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub scheduler: &'static str,
    pub estimator: &'static str,
    pub jobs: usize,
    /// Wall time of the execution phase.
    pub makespan_ms: u64,
    /// Sum of executed action durations.
    pub busy_ms: u64,
    /// Estimated longest dependency chain, before execution.
    pub critical_path_ms: u64,
    /// Estimated total work, before execution.
    pub estimated_work_ms: u64,
    pub executed: usize,
}

impl BuildStats {
    /// Executed work over available worker time, in percent.
    pub fn utilization_pct(&self) -> f64 {
        let capacity = self.makespan_ms.saturating_mul(self.jobs as u64);
        if capacity == 0 {
            return 0.0;
        }
        100.0 * self.busy_ms as f64 / capacity as f64
    }

    /// How close the run came to the estimated critical path. A ratio near 1
    /// means the schedule is bounded by the graph, not by the ordering, so a
    /// better scheduler cannot help.
    pub fn critical_path_ratio(&self) -> Option<f64> {
        (self.critical_path_ms > 0).then(|| self.makespan_ms as f64 / self.critical_path_ms as f64)
    }
}

impl BuildReport {
    pub fn count(&self, pred: impl Fn(&Outcome) -> bool) -> usize {
        self.results.iter().filter(|r| pred(&r.outcome)).count()
    }

    pub fn executed(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Executed { .. }))
    }

    pub fn cached(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Cached))
    }

    pub fn failed(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Failed { .. }))
    }

    pub fn success(&self) -> bool {
        self.results.iter().all(|r| {
            matches!(
                r.outcome,
                Outcome::Executed { .. }
                    | Outcome::Cached
                    | Outcome::WouldRun { .. }
                    | Outcome::MayRun { .. }
            )
        })
    }
}

struct Shared {
    ready: BinaryHeap<(u64, Reverse<usize>)>,
    /// Remaining in-closure producer count per local action.
    waiting: Vec<usize>,
    outcomes: Vec<Option<Outcome>>,
    pending: usize,
    abort: bool,
}

struct CommandBatch {
    captured: String,
    failure: Option<(Vec<String>, String)>,
}

pub struct Engine<'a> {
    root: &'a Path,
    graph: &'a BuildGraph,
    /// Closure in deterministic order; all indices below are into this.
    closure: Vec<ActionId>,
    closure_index: HashMap<ActionId, usize>,
    /// Local indices of in-closure dependents, per local action.
    dependents: Vec<Vec<usize>>,
    /// Ready-queue key per local action: estimated longest remaining chain
    /// (zero under the FIFO scheduler).
    priority: Vec<u64>,
    /// Estimated makespan lower bound and total work, for the stats report.
    critical_path_ms: u64,
    critical_path: BTreeSet<usize>,
    critical_path_labels: Vec<String>,
    estimated_work_ms: u64,
    toolchain_hash: String,
    /// Output-affecting environment captured once per invocation. Looking up
    /// the same handful of variables for every action is surprisingly visible
    /// in a 10k-action no-op build.
    key_env: BTreeMap<String, String>,
    command_env: Vec<(OsString, OsString)>,
    opts: BuildOptions,
    cache: HashCache,
    /// Entries recorded by the *previous* build. Immutable for the duration
    /// of this one — an action only ever consults its own entry, and never a
    /// record written by this run — so the check path reads it without a lock.
    previous: Journal,
    /// Records produced by this build, appended under a lock.
    journal: Mutex<Journal>,
    shared: Mutex<Shared>,
    cv: Condvar,
    cas: LocalCas,
}

/// The scheduling decision, separated from execution.
///
/// The engine and any measurement of the engine must agree on what the
/// scheduler would do, so both build their queue from this one type. A
/// simulator that recomputed priorities on its own would be describing a
/// different scheduler than the one that runs.
#[derive(Debug, Clone)]
pub struct Schedule {
    /// Actions in deterministic closure order; every index below is local.
    pub closure: Vec<ActionId>,
    pub closure_index: HashMap<ActionId, usize>,
    /// In-closure dependents of each action.
    pub dependents: Vec<Vec<usize>>,
    /// Unfinished producers each action waits on.
    pub waiting: Vec<usize>,
    /// Estimated duration of each action.
    pub duration_ms: Vec<u64>,
    /// Ready-queue key: estimated longest remaining chain, or zero for FIFO.
    pub priority: Vec<u64>,
    /// Longest chain by estimated duration: the makespan no scheduler can beat.
    pub critical_path_ms: u64,
    /// Local indices along that chain, in execution order.
    pub critical_path: Vec<usize>,
    /// Sum of all estimated durations.
    pub work_ms: u64,
}

impl Schedule {
    pub fn plan(
        graph: &BuildGraph,
        closure: Vec<ActionId>,
        journal: &Journal,
        scheduler: Scheduler,
        estimator: Estimator,
    ) -> Self {
        let closure_index: HashMap<ActionId, usize> =
            closure.iter().enumerate().map(|(i, &a)| (a, i)).collect();

        let mut waiting = vec![0usize; closure.len()];
        let mut dependents = vec![Vec::new(); closure.len()];
        for (local, &action_id) in closure.iter().enumerate() {
            let mut producers = BTreeSet::new();
            for &input in graph.actions[action_id]
                .inputs
                .iter()
                .chain(&graph.actions[action_id].order_only_inputs)
            {
                if let Some(p) = graph.files[input].producer {
                    if let Some(&plocal) = closure_index.get(&p) {
                        producers.insert(plocal);
                    }
                }
            }
            waiting[local] = producers.len();
            for p in producers {
                dependents[p].push(local);
            }
        }

        let estimate = Estimates::new(estimator, graph, journal);
        // Longest remaining chain, computed once in reverse topological order.
        // The same vector is reused when dependents become ready, so priority
        // is consistent for the whole build rather than only the first wave.
        let mut priority = vec![0u64; closure.len()];
        let mut duration_ms = vec![0u64; closure.len()];
        for local in (0..closure.len()).rev() {
            let action = &graph.actions[closure[local]];
            duration_ms[local] = estimate.of(graph, action, journal);
            let tail = dependents[local]
                .iter()
                .map(|&dependent| priority[dependent])
                .max()
                .unwrap_or(0);
            priority[local] = duration_ms[local].saturating_add(tail);
        }
        let critical_path_ms = priority.iter().copied().max().unwrap_or(0);
        let work_ms = duration_ms.iter().sum();

        // Walk the chain that realizes the longest path, so a report can name
        // the actions that actually bound the build.
        let mut critical_path = Vec::new();
        if let Some(mut cur) = (0..closure.len())
            .filter(|&i| waiting[i] == 0)
            .max_by_key(|&i| priority[i])
        {
            loop {
                critical_path.push(cur);
                match dependents[cur].iter().copied().max_by_key(|&d| priority[d]) {
                    Some(next) => cur = next,
                    None => break,
                }
            }
        }

        if scheduler == Scheduler::Fifo {
            priority.fill(0);
        }
        Self {
            closure,
            closure_index,
            dependents,
            waiting,
            duration_ms,
            priority,
            critical_path_ms,
            critical_path,
            work_ms,
        }
    }

    /// Makespan this schedule would reach with `jobs` workers, by list
    /// scheduling over its own estimated durations. Deterministic: no build
    /// runs, no cache is touched, and repeated calls give the same answer.
    pub fn simulate(&self, jobs: usize) -> Simulation {
        self.simulate_against(jobs, &self.duration_ms)
    }

    /// Simulate this schedule's *ordering* against reference durations.
    ///
    /// Comparing two estimators requires one clock. An estimator decides the
    /// order actions start in; it does not change how long they take. Scoring
    /// each estimator against its own guesses would rank the most optimistic
    /// guesser first — `static` calls every action 1 ms and would "win" every
    /// sweep. Pass the best available durations (the journal's recorded ones)
    /// as the reference and the comparison measures ordering quality alone.
    pub fn simulate_against(&self, jobs: usize, durations: &[u64]) -> Simulation {
        let jobs = jobs.max(1);
        let n = self.closure.len();
        assert_eq!(
            durations.len(),
            n,
            "reference durations must cover the closure"
        );
        let mut waiting = self.waiting.clone();
        let mut ready: BinaryHeap<(u64, Reverse<usize>)> = (0..n)
            .filter(|&i| waiting[i] == 0)
            .map(|i| (self.priority[i], Reverse(i)))
            .collect();
        // (completion time, local index), earliest first.
        let mut running: BinaryHeap<(Reverse<u64>, Reverse<usize>)> = BinaryHeap::new();
        let mut now = 0u64;
        let mut busy = 0u64;
        let mut done = 0usize;

        while done < n {
            while running.len() < jobs {
                let Some((_, Reverse(local))) = ready.pop() else {
                    break;
                };
                running.push((Reverse(now + durations[local]), Reverse(local)));
                busy += durations[local];
            }
            let Some((Reverse(finish), Reverse(local))) = running.pop() else {
                // Nothing running and nothing ready: the graph is exhausted or
                // cyclic. The graph builder rejects cycles, so this is the end.
                break;
            };
            now = finish;
            done += 1;
            for &dependent in &self.dependents[local] {
                waiting[dependent] -= 1;
                if waiting[dependent] == 0 {
                    ready.push((self.priority[dependent], Reverse(dependent)));
                }
            }
        }

        Simulation {
            jobs,
            makespan_ms: now,
            busy_ms: busy,
            critical_path_ms: self.critical_path_ms,
            work_ms: self.work_ms,
            actions: n,
        }
    }
}

/// Result of scheduling without executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Simulation {
    pub jobs: usize,
    pub makespan_ms: u64,
    pub busy_ms: u64,
    pub critical_path_ms: u64,
    pub work_ms: u64,
    pub actions: usize,
}

impl Simulation {
    pub fn utilization_pct(&self) -> f64 {
        let capacity = self.makespan_ms.saturating_mul(self.jobs as u64);
        if capacity == 0 {
            return 0.0;
        }
        100.0 * self.busy_ms as f64 / capacity as f64
    }

    /// How far above the unbeatable lower bound this schedule lands.
    pub fn over_critical_path_pct(&self) -> Option<f64> {
        (self.critical_path_ms > 0).then(|| {
            100.0 * (self.makespan_ms as f64 - self.critical_path_ms as f64)
                / self.critical_path_ms as f64
        })
    }
}

impl<'a> Engine<'a> {
    pub fn new(
        root: &'a Path,
        graph: &'a BuildGraph,
        closure: Vec<ActionId>,
        toolchain_hash: String,
        opts: BuildOptions,
    ) -> Self {
        // Neither store depends on the other. Loading them concurrently keeps
        // the warm path bounded by the larger decode instead of their sum.
        let (journal, cache) = std::thread::scope(|scope| {
            let journal = scope.spawn(|| Journal::load(root));
            let cache = HashCache::load(root);
            (
                journal.join().expect("journal loader should not panic"),
                cache,
            )
        });
        let n = closure.len();
        let cas_max_bytes = opts.cas_max_bytes;
        let key_env = key_environment_snapshot();
        let command_env = ENV_PASSTHROUGH
            .iter()
            .chain(ENV_IN_KEY)
            .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
            .collect();
        Self {
            root,
            graph,
            closure,
            closure_index: HashMap::new(),
            dependents: Vec::new(),
            priority: Vec::new(),
            critical_path_ms: 0,
            critical_path: BTreeSet::new(),
            critical_path_labels: Vec::new(),
            estimated_work_ms: 0,
            toolchain_hash,
            key_env,
            command_env,
            opts,
            cache,
            previous: journal,
            journal: Mutex::new(Journal::default()),
            shared: Mutex::new(Shared {
                ready: BinaryHeap::new(),
                waiting: vec![0; n],
                outcomes: vec![None; n],
                pending: n,
                abort: false,
            }),
            cv: Condvar::new(),
            cas: LocalCas::new(root, cas_max_bytes),
        }
    }

    pub fn run(mut self) -> Result<BuildReport> {
        let workers = self.opts.jobs.max(1).min(self.closure.len().max(1));
        let progress = self.opts.progress.clone();
        let started = std::time::Instant::now();
        if self.all_cached().unwrap_or(false) {
            if let Some(progress) = &progress {
                progress.emit(ProgressEvent::BuildStarted {
                    total: self.closure.len(),
                    jobs: workers,
                    critical_path_ms: 0,
                    critical_path: Vec::new(),
                });
            }
            {
                let mut shared = self.shared.lock().unwrap();
                shared.outcomes.fill(Some(Outcome::Cached));
                shared.pending = 0;
                shared.ready.clear();
            }
            if let Some(progress) = &progress {
                progress.emit(ProgressEvent::AllCached {
                    total: self.closure.len(),
                });
            }
        } else {
            if !self.opts.dry_run {
                self.prepare_output_dirs()?;
            }
            self.prepare_schedule();
            if let Some(progress) = &progress {
                progress.emit(ProgressEvent::BuildStarted {
                    total: self.closure.len(),
                    jobs: workers,
                    critical_path_ms: self.critical_path_ms,
                    critical_path: std::mem::take(&mut self.critical_path_labels),
                });
            }
            std::thread::scope(|scope| {
                let engine = &self;
                for slot in 0..workers {
                    scope.spawn(move || engine.worker(slot));
                }
            });
        }
        let makespan_ms = started.elapsed().as_millis() as u64;

        let shared = self.shared.into_inner().unwrap();
        if !self.opts.dry_run {
            let recorded = self.journal.into_inner().unwrap();
            let journal_path = self.root.join(frostbuild_core::journal::JOURNAL_REL_PATH);
            if std::fs::metadata(journal_path).is_ok_and(|m| m.len() > 32 * 1024 * 1024) {
                // Compaction rewrites the whole file, so it must carry the
                // entries this build did not touch as well as the new ones.
                let mut compacted = self.previous;
                compacted.actions.extend(recorded.actions);
                compacted.save(self.root)?;
            }
            let _ = self.cas.gc()?;
        }
        self.cache.save(self.root)?;

        let mut results = Vec::with_capacity(self.closure.len());
        for (local, &action_id) in self.closure.iter().enumerate() {
            let action = &self.graph.actions[action_id];
            let outcome = shared.outcomes[local].clone().unwrap_or(Outcome::Skipped {
                reason: "not run (earlier failure aborted the build)".into(),
            });
            results.push(ActionResult {
                id: action.id.clone(),
                desc: action.desc.clone(),
                outcome,
            });
        }
        let (busy_ms, executed) =
            results
                .iter()
                .fold((0u64, 0usize), |(b, n), r| match r.outcome {
                    Outcome::Executed { duration_ms, .. } => (b + duration_ms, n + 1),
                    _ => (b, n),
                });
        let stats = BuildStats {
            scheduler: self.opts.scheduler.as_str(),
            estimator: self.opts.estimator.as_str(),
            jobs: workers,
            makespan_ms,
            busy_ms,
            critical_path_ms: self.critical_path_ms,
            estimated_work_ms: self.estimated_work_ms,
            executed,
        };
        let report = BuildReport { results, stats };
        if let Some(progress) = progress {
            progress.emit(ProgressEvent::BuildFinished {
                success: report.success(),
                elapsed_ms: makespan_ms,
            });
        }
        Ok(report)
    }

    /// Scheduling data is irrelevant when a whole closure is cached. Delay
    /// its O(actions + edges) allocation until the cache preflight finds work.
    fn prepare_schedule(&mut self) {
        let plan = Schedule::plan(
            self.graph,
            self.closure.clone(),
            &self.previous,
            self.opts.scheduler,
            self.opts.estimator,
        );
        if self.opts.progress.is_some() {
            self.critical_path = plan.critical_path.iter().copied().collect();
            self.critical_path_labels = plan
                .critical_path
                .iter()
                .map(|&local| self.graph.actions[self.closure[local]].desc.clone())
                .collect();
        } else {
            self.critical_path.clear();
            self.critical_path_labels.clear();
        }
        self.closure_index = plan.closure_index;
        self.dependents = plan.dependents;
        self.priority = plan.priority;
        self.critical_path_ms = plan.critical_path_ms;
        self.estimated_work_ms = plan.work_ms;
        let mut shared = self.shared.lock().unwrap();
        shared.waiting = plan.waiting;
        shared.ready = shared
            .waiting
            .iter()
            .enumerate()
            .filter(|(_, &waiting)| waiting == 0)
            .map(|(local, _)| (self.priority[local], Reverse(local)))
            .collect();
    }

    /// Validate a fully cached closure in two passes instead of sending every
    /// action through the scheduler. The normal path stats the same output as
    /// one action's output and the next action's input, and takes the shared
    /// scheduler lock for every cached node. A workspace-wide pass stats each
    /// unique path once, then verifies the exact same action keys and output
    /// digests before declaring the closure cached.
    fn all_cached(&self) -> Result<bool> {
        if self.opts.dry_run {
            return Ok(false);
        }

        let mut expected_by_file = vec![None; self.graph.files.len()];
        let mut discovered_expected = HashMap::new();
        for &action_id in &self.closure {
            let action = &self.graph.actions[action_id];
            if self.opts.no_cache && action.kind == ActionKind::Test {
                return Ok(false);
            }
            let Some(previous) = self.previous.actions.get(&journal_id(self.graph, action)) else {
                return Ok(false);
            };

            // Reusing `previous.inputs` for the key is valid only when the
            // current declared-input set is identical. Discovered inputs are
            // explicitly recorded, so they can be separated without changing
            // the journal format.
            if previous.discovered.is_empty() {
                if action.inputs.len() != previous.inputs.len()
                    || action
                        .inputs
                        .iter()
                        .any(|&file| !previous.inputs.contains_key(&self.graph.files[file].path))
                {
                    return Ok(false);
                }
            } else {
                let discovered: BTreeSet<&str> =
                    previous.discovered.iter().map(String::as_str).collect();
                let previous_declared: BTreeSet<&str> = previous
                    .inputs
                    .keys()
                    .map(String::as_str)
                    .filter(|path| !discovered.contains(path))
                    .collect();
                let current_declared: BTreeSet<&str> = action
                    .inputs
                    .iter()
                    .map(|&file| self.graph.files[file].path.as_str())
                    .collect();
                if current_declared != previous_declared {
                    return Ok(false);
                }
            }

            for &file in &action.inputs {
                let path = &self.graph.files[file].path;
                let Some(digest) = previous.inputs.get(path) else {
                    return Ok(false);
                };
                if expected_by_file[file]
                    .replace(digest.as_str())
                    .is_some_and(|other| other != digest)
                {
                    return Ok(false);
                }
            }
            for path in &previous.discovered {
                let Some(digest) = previous.inputs.get(path) else {
                    return Ok(false);
                };
                if discovered_expected
                    .insert(path.as_str(), digest.as_str())
                    .is_some_and(|other| other != digest)
                {
                    return Ok(false);
                }
            }
            if action.outputs.len() != previous.outputs.len() {
                return Ok(false);
            }
            for &file in &action.outputs {
                let path = &self.graph.files[file].path;
                let Some(digest) = previous.outputs.get(path) else {
                    return Ok(false);
                };
                if expected_by_file[file]
                    .replace(digest.as_str())
                    .is_some_and(|other| other != digest)
                {
                    return Ok(false);
                }
            }
        }

        let mut expected = Vec::with_capacity(
            expected_by_file
                .len()
                .saturating_add(discovered_expected.len()),
        );
        expected.extend(
            self.graph
                .files
                .iter()
                .zip(expected_by_file)
                .filter_map(|(file, digest)| digest.map(|digest| (file.path.as_str(), digest))),
        );
        expected.extend(discovered_expected);
        let (files_match, keys_match) = rayon::join(
            || self.cache.matches_many(self.root, &expected),
            || {
                self.closure.par_iter().all(|&action_id| {
                    let action = &self.graph.actions[action_id];
                    let previous = &self.previous.actions[&journal_id(self.graph, action)];
                    self.action_key(action, &previous.inputs) == previous.key
                })
            },
        );
        let files_match = files_match?;
        let cached = files_match && keys_match;
        if cached && self.opts.write_fast_noop {
            let dynamic_env = self
                .closure
                .iter()
                .flat_map(|&action_id| self.graph.actions[action_id].pass_env.iter())
                .map(|name| {
                    (
                        name.clone(),
                        std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            let _ = fast_noop::save(
                fast_noop::CertificateInput {
                    root: self.root,
                    profile: &self.graph.profile,
                    platform: &self.graph.platform,
                    closure_actions: self.closure.len(),
                    graph_actions: self.graph.actions.len(),
                    toolchain: &self.graph.toolchain,
                    toolchain_hash: &self.toolchain_hash,
                    key_env: &self.key_env,
                    dynamic_env: &dynamic_env,
                    paths: &expected,
                },
                || self.cache.matches_many(self.root, &expected),
            );
        }
        Ok(cached)
    }

    fn worker(&self, slot: usize) {
        let mut continuation = None;
        loop {
            let local = if let Some(local) = continuation.take() {
                local
            } else {
                let mut s = self.shared.lock().unwrap();
                loop {
                    if s.abort && s.ready.is_empty() {
                        return;
                    }
                    if let Some((_, Reverse(i))) = s.ready.pop() {
                        break i;
                    }
                    if s.pending == 0 {
                        return;
                    }
                    s = self.cv.wait(s).unwrap();
                }
            };

            let action = &self.graph.actions[self.closure[local]];
            let critical = self.opts.progress.is_some() && self.critical_path.contains(&local);
            if let Some(progress) = &self.opts.progress {
                progress.emit(ProgressEvent::ActionStarted {
                    slot,
                    id: action.id.clone(),
                    desc: action.desc.clone(),
                    command: shell_join(&action.argv),
                    critical,
                });
            }
            let action_started = self.opts.progress.as_ref().map(|_| Instant::now());
            let outcome = self.process(local);
            let elapsed_ms = action_started
                .map(|started| started.elapsed().as_millis() as u64)
                .unwrap_or(0);
            let progress_result = self.opts.progress.as_ref().map(|_| match &outcome {
                Outcome::Executed { duration_ms, .. } => {
                    (ProgressState::Executed, *duration_ms, String::new())
                }
                Outcome::Cached => (ProgressState::CacheHit, elapsed_ms, String::new()),
                Outcome::Failed { detail, .. } => {
                    (ProgressState::Failed, elapsed_ms, detail.clone())
                }
                Outcome::Skipped { reason } => (ProgressState::Skipped, elapsed_ms, reason.clone()),
                Outcome::WouldRun { reason } => {
                    (ProgressState::WouldRun, elapsed_ms, reason.clone())
                }
                Outcome::MayRun { reason } => (ProgressState::MayRun, elapsed_ms, reason.clone()),
            });

            let mut s = self.shared.lock().unwrap();
            let failed = matches!(outcome, Outcome::Failed { .. });
            s.outcomes[local] = Some(outcome);
            s.pending -= 1;
            let completed = self.closure.len() - s.pending;
            if failed && !self.opts.keep_going {
                s.abort = true;
                s.ready.clear();
            }
            let mut unlocked = 0usize;
            if !s.abort {
                for &dep in &self.dependents[local] {
                    s.waiting[dep] -= 1;
                    if s.waiting[dep] == 0 {
                        let priority = self.priority(dep);
                        s.ready.push((priority, Reverse(dep)));
                        unlocked += 1;
                    }
                }
            }
            let finished = s.pending == 0 || s.abort;
            // The worker that just made an action ready is already awake.
            // Let it claim the highest-priority next action while holding the
            // scheduler lock. On a dependency chain this avoids 10k kernel
            // wakeups and ready-heap push/pop handoffs between workers.
            if !finished {
                continuation = s.ready.pop().map(|(_, Reverse(local))| local);
            }
            let claimed = usize::from(continuation.is_some());
            drop(s);
            if let (Some(progress), Some((state, duration_ms, detail))) =
                (&self.opts.progress, progress_result)
            {
                progress.emit(ProgressEvent::ActionFinished {
                    slot,
                    completed,
                    total: self.closure.len(),
                    id: action.id.clone(),
                    desc: action.desc.clone(),
                    state,
                    duration_ms,
                    detail,
                    critical,
                });
            }
            if finished {
                // Everyone must wake to observe the end and return.
                self.cv.notify_all();
            } else {
                // Wake one worker per action that became runnable. Waking all
                // of them would send every idle worker to an empty queue: a
                // dependency chain unlocks one action at a time, so on a chain
                // of N actions `notify_all` costs N * jobs wakeups to do N
                // units of work.
                for _ in 0..unlocked.saturating_sub(claimed) {
                    self.cv.notify_one();
                }
            }
        }
    }

    fn process(&self, local: usize) -> Outcome {
        let action = &self.graph.actions[self.closure[local]];

        // Upstream state: producers finished before we became ready.
        let mut upstream_dirty: Option<String> = None;
        {
            let s = self.shared.lock().unwrap();
            for &input in action.inputs.iter().chain(&action.order_only_inputs) {
                let Some(p) = self.graph.files[input].producer else {
                    continue;
                };
                let Some(&plocal) = self.closure_index.get(&p) else {
                    continue;
                };
                match &s.outcomes[plocal] {
                    Some(Outcome::Failed { .. }) | Some(Outcome::Skipped { .. }) => {
                        return Outcome::Skipped {
                            reason: format!(
                                "upstream failed: {}",
                                self.graph.actions[self.closure[plocal]].id
                            ),
                        };
                    }
                    Some(Outcome::WouldRun { .. }) | Some(Outcome::MayRun { .. }) => {
                        upstream_dirty = Some(self.graph.actions[self.closure[plocal]].id.clone());
                    }
                    _ => {}
                }
            }
        }
        if let Some(upstream) = upstream_dirty {
            // Dry run only: inputs on disk are stale, so no honest key exists.
            return Outcome::MayRun {
                reason: format!("depends on output of {upstream}, which would run"),
            };
        }

        let previous = self.previous.actions.get(&journal_id(self.graph, action));

        // Declared inputs + inputs discovered by the previous run's depfile.
        let mut input_paths: Vec<String> = action
            .inputs
            .iter()
            .map(|&f| self.graph.files[f].path.clone())
            .collect();
        if let Some(prev) = &previous {
            for d in &prev.discovered {
                if !input_paths.contains(d) {
                    input_paths.push(d.clone());
                }
            }
        }

        let inputs = match self.digest_all(&input_paths) {
            Ok(m) => m,
            Err(err) => {
                return Outcome::Failed {
                    reason: "failed to hash inputs".into(),
                    detail: format!("{err:#}"),
                }
            }
        };
        let key = self.action_key(action, &inputs);

        if self.opts.no_cache && action.kind == ActionKind::Test {
            return self.execute(local, action, inputs, "test cache disabled".into());
        }

        if let Some(prev) = &previous {
            if prev.key == key {
                match self.outputs_intact(prev) {
                    Ok(None) => return Outcome::Cached,
                    Ok(Some(bad)) => {
                        if self.restore_outputs(prev).unwrap_or(false) {
                            return Outcome::Cached;
                        }
                        return self.execute(
                            local,
                            action,
                            inputs,
                            format!("output missing or modified: {bad}"),
                        );
                    }
                    Err(err) => {
                        return Outcome::Failed {
                            reason: "failed to hash outputs".into(),
                            detail: format!("{err:#}"),
                        }
                    }
                }
            }
            let reason = explain_key_change(prev, &inputs);
            return self.execute(local, action, inputs, reason);
        }

        self.execute(local, action, inputs, "not built before".into())
    }

    fn execute(
        &self,
        local: usize,
        action: &frostbuild_core::graph::ActionNode,
        mut inputs: BTreeMap<String, String>,
        reason: String,
    ) -> Outcome {
        let _ = local;
        if self.opts.dry_run {
            return Outcome::WouldRun { reason };
        }
        // Raw terminal mode turns Ctrl-C into an input event instead of a
        // signal. That event can arrive after scheduling but before this
        // action has spawned; do not delete outputs or start new work once
        // cancellation has already been requested.
        if was_cancelled() {
            return Outcome::Failed {
                reason: "build cancelled".into(),
                detail: "cancelled before action start".into(),
            };
        }
        if let Some(progress) = &self.opts.progress {
            progress.emit(ProgressEvent::ActionRunning {
                id: action.id.clone(),
            });
        }

        for &out in &action.outputs {
            let path = &self.graph.files[out].path;
            self.cache.invalidate(path);
            if !action.preserve_outputs {
                let _ = std::fs::remove_file(self.root.join(path));
            }
        }
        if let Err(err) = self.reset_clean_dirs(action) {
            return Outcome::Failed {
                reason,
                detail: format!("failed to reset command intermediates: {err:#}"),
            };
        }

        let started = Instant::now();
        let batch = match self.run_action_commands(action, &inputs) {
            Ok(batch) => batch,
            Err(err) => {
                return Outcome::Failed {
                    reason,
                    detail: err,
                }
            }
        };

        if let Some((argv, exit)) = batch.failure {
            self.remove_partial_outputs(action);
            let detail = format!(
                "command: {}\nexit: {}\n{}",
                shell_join(&argv),
                exit,
                batch.captured.trim_end()
            );
            return Outcome::Failed { reason, detail };
        }
        if action.kind == ActionKind::Test {
            if let Err(err) = self.write_test_success_outputs(action) {
                self.remove_partial_outputs(action);
                return Outcome::Failed {
                    reason,
                    detail: format!("failed to record test success: {err:#}"),
                };
            }
        }
        let duration_ms = started.elapsed().as_millis() as u64;
        let captured = batch.captured;

        // Ingest the depfile: replace previous discovered deps with fresh
        // ones and fold their digests into the recorded key.
        let mut discovered = Vec::new();
        if let Some(dep_rel) = &action.depfile {
            let dep_path = self.root.join(dep_rel);
            if let Ok(text) = std::fs::read_to_string(&dep_path) {
                match depfile::parse(&text, self.root) {
                    Ok(deps) => discovered = deps,
                    Err(err) => {
                        let detail = format!("failed to parse depfile {dep_rel}: {err:#}");
                        return Outcome::Failed { reason, detail };
                    }
                }
            }
            let declared: BTreeSet<String> = action
                .inputs
                .iter()
                .map(|&f| self.graph.files[f].path.clone())
                .collect();
            discovered.retain(|d| !declared.contains(d));
            inputs.retain(|path, _| declared.contains(path));
            match self.digest_all(&discovered) {
                Ok(extra) => inputs.extend(extra),
                Err(err) => {
                    return Outcome::Failed {
                        reason,
                        detail: format!("failed to hash discovered deps: {err:#}"),
                    }
                }
            }
        };

        let output_paths: Vec<String> = action
            .outputs
            .iter()
            .map(|&f| self.graph.files[f].path.clone())
            .collect();
        let outputs = match self.digest_all(&output_paths) {
            Ok(m) => m,
            Err(err) => {
                return Outcome::Failed {
                    reason,
                    detail: format!("failed to hash outputs: {err:#}"),
                }
            }
        };
        if let Some(missing) = outputs
            .iter()
            .find(|(_, h)| h.as_str() == frostbuild_core::hashcache::MISSING)
        {
            let detail = format!(
                "command succeeded but declared output {} was not created",
                missing.0
            );
            return Outcome::Failed { reason, detail };
        }

        // A compiler/code generator may publish hundreds of small outputs.
        // CAS objects are independent, so serial copy+rename publication makes
        // post-processing dominate the action itself. Deduplicate by digest
        // first (parallel writers for identical bytes would share a temp name),
        // then publish distinct immutable objects concurrently.
        let unique_outputs: BTreeMap<&str, &str> = outputs
            .iter()
            .map(|(path, digest)| (digest.as_str(), path.as_str()))
            .collect();
        if let Err(err) = unique_outputs
            .par_iter()
            .try_for_each(|(digest, path)| self.cas.put(&self.root.join(path), digest))
        {
            return Outcome::Failed {
                reason,
                detail: format!("failed to store output in CAS: {err:#}"),
            };
        }

        if self.opts.check_determinism {
            if let Some(path) = inputs.keys().find(|path| {
                std::fs::read_to_string(self.root.join(path))
                    .is_ok_and(|text| text.contains("__TIME__") || text.contains("__DATE__"))
            }) {
                let detail = format!(
                    "non-deterministic action {}: {} uses __DATE__/__TIME__; outputs: {}",
                    action.id,
                    path,
                    output_paths.join(", ")
                );
                return Outcome::Failed {
                    reason: "determinism check failed".into(),
                    detail,
                };
            }
            let first = outputs.clone();
            if let Err(err) = self.reset_clean_dirs(action) {
                return Outcome::Failed {
                    reason,
                    detail: format!("determinism rerun setup failed: {err:#}"),
                };
            }
            if action.kind == ActionKind::Test {
                self.remove_partial_outputs(action);
            }
            let second = match self.run_action_commands(action, &inputs) {
                Ok(batch) => batch,
                Err(err) => {
                    return Outcome::Failed {
                        reason,
                        detail: format!("determinism rerun failed: {err}"),
                    }
                }
            };
            if let Some((argv, exit)) = second.failure {
                return Outcome::Failed {
                    reason,
                    detail: format!(
                        "determinism rerun failed: {} ({exit})\n{}",
                        shell_join(&argv),
                        second.captured.trim_end()
                    ),
                };
            }
            if action.kind == ActionKind::Test {
                if let Err(err) = self.write_test_success_outputs(action) {
                    return Outcome::Failed {
                        reason,
                        detail: format!("determinism rerun success record failed: {err:#}"),
                    };
                }
            }
            for path in &output_paths {
                self.cache.invalidate(path);
            }
            let second_outputs = match self.digest_all(&output_paths) {
                Ok(value) => value,
                Err(err) => {
                    return Outcome::Failed {
                        reason,
                        detail: format!("determinism output hash failed: {err:#}"),
                    }
                }
            };
            if first != second_outputs {
                let changed = first
                    .iter()
                    .filter_map(|(path, hash)| {
                        (second_outputs.get(path) != Some(hash)).then_some(path.clone())
                    })
                    .collect::<Vec<_>>();
                let detail = format!(
                    "non-deterministic action {} produced different output: {}",
                    action.id,
                    changed.join(", ")
                );
                return Outcome::Failed {
                    reason: "determinism check failed".into(),
                    detail,
                };
            }
        }

        let key = self.action_key(action, &inputs);
        {
            let mut journal = self.journal.lock().unwrap();
            if let Err(err) = journal.record(
                self.root,
                journal_id(self.graph, action),
                JournalEntry {
                    key,
                    inputs,
                    discovered,
                    outputs,
                    duration_ms,
                    reason: reason.clone(),
                },
            ) {
                return Outcome::Failed {
                    reason,
                    detail: format!("failed to flush journal: {err:#}"),
                };
            }
        }

        if let Some(progress) = &self.opts.progress {
            progress.emit(ProgressEvent::ActionOutput {
                id: action.id.clone(),
                output: captured,
            });
        }
        Outcome::Executed {
            reason,
            duration_ms,
        }
    }

    fn action_key(
        &self,
        action: &frostbuild_core::graph::ActionNode,
        inputs: &BTreeMap<String, String>,
    ) -> String {
        let mut action_environment = None;
        if !action.env.is_empty() || !action.pass_env.is_empty() {
            let mut environment = self.key_env.clone();
            environment.extend(action.env.clone());
            for name in &action.pass_env {
                if let Some(value) = std::env::var_os(name) {
                    environment.insert(name.clone(), value.to_string_lossy().into_owned());
                } else {
                    environment.remove(name);
                }
            }
            action_environment = Some(environment);
        }
        let environment = action_environment.as_ref().unwrap_or(&self.key_env);
        let argv = action_key_argv(action);
        streamed_action_key(
            "frost-engine-v1",
            &action.id,
            argv.as_ref(),
            ".",
            &self.toolchain_hash,
            environment,
            inputs,
        )
    }

    fn digest_all(&self, paths: &[String]) -> Result<BTreeMap<String, String>> {
        self.cache.digest_many(self.root, paths)
    }

    /// Returns Ok(None) when all recorded outputs are on disk with matching
    /// digests, or Ok(Some(path)) naming the first stale output.
    fn outputs_intact(&self, prev: &JournalEntry) -> Result<Option<String>> {
        for (path, recorded) in &prev.outputs {
            let current = self.cache.digest(self.root, path)?;
            if &current != recorded {
                return Ok(Some(path.clone()));
            }
        }
        Ok(None)
    }

    fn prepare_output_dirs(&self) -> Result<()> {
        let mut directories = BTreeSet::new();
        for &action_id in &self.closure {
            let action = &self.graph.actions[action_id];
            for &out in &action.outputs {
                let path = self.root.join(&self.graph.files[out].path);
                if let Some(parent) = path.parent() {
                    directories.insert(parent.to_path_buf());
                }
            }
            if let Some(dep) = &action.depfile {
                let path = self.root.join(dep);
                if let Some(parent) = path.parent() {
                    directories.insert(parent.to_path_buf());
                }
            }
        }
        for parent in directories {
            std::fs::create_dir_all(&parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        Ok(())
    }

    fn restore_outputs(&self, prev: &JournalEntry) -> Result<bool> {
        for (path, digest) in &prev.outputs {
            if !self.cas.materialize(digest, &self.root.join(path))? {
                return Ok(false);
            }
            self.cache.invalidate(path);
        }
        Ok(true)
    }

    fn remove_partial_outputs(&self, action: &frostbuild_core::graph::ActionNode) {
        for &output in &action.outputs {
            let _ = std::fs::remove_file(self.root.join(&self.graph.files[output].path));
        }
    }

    /// Test outputs are Frost-owned success stamps, not files the test
    /// process is expected to manufacture. Keeping this outside the command
    /// removes a POSIX-shell dependency and guarantees a stamp exists only
    /// after every command in the test action has succeeded.
    fn write_test_success_outputs(
        &self,
        action: &frostbuild_core::graph::ActionNode,
    ) -> Result<()> {
        for &output in &action.outputs {
            let relative = &self.graph.files[output].path;
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::write(&path, b"")
                .with_context(|| format!("failed to write {}", path.display()))?;
            self.cache.invalidate(relative);
        }
        Ok(())
    }

    fn reset_clean_dirs(&self, action: &frostbuild_core::graph::ActionNode) -> Result<()> {
        for directory in &action.clean_dirs {
            let path = self.root.join(directory);
            if path.exists() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            std::fs::create_dir_all(&path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        Ok(())
    }

    fn run_action_commands(
        &self,
        action: &frostbuild_core::graph::ActionNode,
        inputs: &BTreeMap<String, String>,
    ) -> std::result::Result<CommandBatch, String> {
        let mut captured = String::new();
        for argv in std::iter::once(&action.argv).chain(&action.followup_argv) {
            let mut command = self
                .command_for_argv(action, inputs, argv)
                .map_err(|error| format!("{error:#}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let child = command
                .spawn()
                .map_err(|error| format!("failed to spawn {:?}: {error}", argv[0]))?;
            let pid = child.id();
            {
                let mut groups = RUNNING_PROCESS_GROUPS
                    .get_or_init(|| Mutex::new(BTreeSet::new()))
                    .lock()
                    .unwrap();
                groups.insert(pid);
                // Close the spawn/registration race: request_cancellation may
                // have run after `spawn` but before this process group became
                // visible. It sets the flag before taking the same mutex, so
                // either that call kills us or this check does.
                if was_cancelled() {
                    terminate_process_tree(pid);
                }
            }
            let output = child.wait_with_output();
            RUNNING_PROCESS_GROUPS
                .get()
                .unwrap()
                .lock()
                .unwrap()
                .remove(&pid);
            let output =
                output.map_err(|error| format!("failed waiting for {}: {error}", action.id))?;
            captured.push_str(&String::from_utf8_lossy(&output.stdout));
            captured.push_str(&String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                return Ok(CommandBatch {
                    captured,
                    failure: Some((argv.clone(), describe_exit(&output.status))),
                });
            }
        }
        Ok(CommandBatch {
            captured,
            failure: None,
        })
    }

    fn command_for_argv(
        &self,
        action: &frostbuild_core::graph::ActionNode,
        inputs: &BTreeMap<String, String>,
        argv: &[String],
    ) -> Result<Command> {
        let mut command = if self.opts.sandbox && action.sandbox {
            sandbox_command(self.root, self.graph, action, inputs, argv)?
        } else {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).current_dir(self.root);
            command
        };
        command
            .env_clear()
            .envs(self.command_env.iter().map(|(key, value)| (key, value)))
            .envs(&action.env)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            // Actions never read from the terminal. Inheriting stdin lets a
            // command that expects input (`cat > out` when ${in} expanded to
            // nothing, an accidental interactive prompt) block forever with no
            // output and no diagnostic, which looks exactly like a slow build.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for name in &action.pass_env {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        Ok(command)
    }

    fn priority(&self, local: usize) -> u64 {
        self.priority[local]
    }
}

/// Journal namespace for an action: host builds keep the historical
/// `id@profile` form; platform builds add the platform segment so each
/// (platform, profile) pair has an independent cache identity.
pub fn journal_id(graph: &BuildGraph, action: &frostbuild_core::graph::ActionNode) -> String {
    if graph.platform == frostbuild_core::manifest::HOST_PLATFORM {
        format!("{}@{}", action.id, graph.profile)
    } else {
        format!("{}@{}@{}", action.id, graph.platform, graph.profile)
    }
}

fn default_duration(kind: ActionKind) -> u64 {
    match kind {
        ActionKind::Link => 100,
        ActionKind::Archive => 30,
        ActionKind::Compile => 20,
        ActionKind::Genrule => 10,
        ActionKind::Test => 50,
        ActionKind::KofunCompile => 30,
        ActionKind::Command => 40,
    }
}

/// Duration estimates for scheduling. Estimates only order work, so an
/// inaccurate model costs makespan and never correctness.
struct Estimates {
    kind: Estimator,
    /// Median observed duration per action kind, learned from this
    /// workspace's journal. Empty unless the learned estimator is selected.
    learned: BTreeMap<u8, u64>,
}

impl Estimates {
    fn new(kind: Estimator, graph: &BuildGraph, journal: &Journal) -> Self {
        let mut learned = BTreeMap::new();
        if kind == Estimator::Learned {
            let mut by_kind: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
            let mut kind_of: HashMap<&str, ActionKind> = HashMap::new();
            for action in &graph.actions {
                kind_of.insert(action.id.as_str(), action.kind);
            }
            for (id, entry) in &journal.actions {
                // Journal ids are `action@profile[@platform]`; the action id
                // is the prefix before the first '@'.
                let action_id = id.split('@').next().unwrap_or(id);
                if let Some(&k) = kind_of.get(action_id) {
                    if entry.duration_ms > 0 {
                        by_kind
                            .entry(kind_code(k))
                            .or_default()
                            .push(entry.duration_ms);
                    }
                }
            }
            for (k, mut samples) in by_kind {
                samples.sort_unstable();
                learned.insert(k, samples[samples.len() / 2].max(1));
            }
        }
        Self { kind, learned }
    }

    fn of(
        &self,
        graph: &BuildGraph,
        action: &frostbuild_core::graph::ActionNode,
        journal: &Journal,
    ) -> u64 {
        let recorded = || {
            journal
                .actions
                .get(&journal_id(graph, action))
                .map(|e| e.duration_ms)
                .filter(|&d| d > 0)
        };
        match self.kind {
            Estimator::Static => 1,
            Estimator::Heuristic => default_duration(action.kind),
            Estimator::Journal => recorded().unwrap_or_else(|| default_duration(action.kind)),
            Estimator::Learned => recorded().unwrap_or_else(|| {
                self.learned
                    .get(&kind_code(action.kind))
                    .copied()
                    .unwrap_or_else(|| default_duration(action.kind))
            }),
        }
    }
}

fn kind_code(kind: ActionKind) -> u8 {
    match kind {
        ActionKind::Compile => 0,
        ActionKind::Archive => 1,
        ActionKind::Link => 2,
        ActionKind::Genrule => 3,
        ActionKind::Test => 4,
        ActionKind::KofunCompile => 5,
        ActionKind::Command => 6,
    }
}

fn sandbox_command(
    root: &Path,
    graph: &BuildGraph,
    action: &frostbuild_core::graph::ActionNode,
    inputs: &BTreeMap<String, String>,
    argv: &[String],
) -> Result<Command> {
    let bwrap = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("bwrap"))
                .find(|candidate| candidate.is_file())
        })
        .context("--sandbox requires bubblewrap (bwrap) on Linux")?;
    let mut command = Command::new(bwrap);
    command.args([
        "--die-with-parent",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
    ]);
    command.arg(root);

    let mut readonly_dirs = BTreeSet::new();
    for &file in &action.inputs {
        let relative = &graph.files[file].path;
        if !Path::new(relative).is_absolute() {
            if let Some(parent) = root.join(relative).parent() {
                readonly_dirs.insert(parent.to_path_buf());
            }
        }
    }
    for argv in std::iter::once(&action.argv).chain(&action.followup_argv) {
        let mut args = argv.iter().peekable();
        while let Some(arg) = args.next() {
            let include = if arg == "-I" {
                args.next().map(String::as_str)
            } else {
                arg.strip_prefix("-I").filter(|value| !value.is_empty())
            };
            if let Some(include) = include {
                let path = Path::new(include);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    root.join(path)
                };
                if path.starts_with(root) && path.is_dir() {
                    readonly_dirs.insert(path);
                }
            }
        }
    }
    let mut allowed = inputs.keys().cloned().collect::<BTreeSet<_>>();
    for &file in &action.order_only_inputs {
        allowed.insert(graph.files[file].path.clone());
    }
    let mut made_dirs = BTreeSet::new();
    for directory in readonly_dirs {
        add_sandbox_dirs(&mut command, root, directory.parent(), &mut made_dirs);
        command.arg("--ro-bind").arg(&directory).arg(&directory);
    }
    for rel in allowed {
        let source = Path::new(&rel);
        if source.is_absolute() {
            continue;
        }
        let source = root.join(&rel);
        if !source.exists() {
            continue;
        }
        let destination = root.join(&rel);
        add_sandbox_dirs(&mut command, root, destination.parent(), &mut made_dirs);
        command.arg("--ro-bind").arg(&source).arg(&destination);
    }

    let mut writable = BTreeSet::new();
    for &file in &action.outputs {
        if let Some(parent) = root.join(&graph.files[file].path).parent() {
            writable.insert(parent.to_path_buf());
        }
    }
    if let Some(depfile) = &action.depfile {
        if let Some(parent) = root.join(depfile).parent() {
            writable.insert(parent.to_path_buf());
        }
    }
    for directory in &action.clean_dirs {
        writable.insert(root.join(directory));
    }
    for directory in writable {
        std::fs::create_dir_all(&directory)?;
        add_sandbox_dirs(&mut command, root, directory.parent(), &mut made_dirs);
        command.arg("--bind").arg(&directory).arg(&directory);
    }
    command.arg("--chdir").arg(root).arg("--").args(argv);
    Ok(command)
}

fn add_sandbox_dirs(
    command: &mut Command,
    root: &Path,
    parent: Option<&Path>,
    made: &mut BTreeSet<PathBuf>,
) {
    let Some(parent) = parent else { return };
    let Ok(relative) = parent.strip_prefix(root) else {
        return;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if made.insert(current.clone()) {
            command.arg("--dir").arg(&current);
        }
    }
}

fn explain_key_change(prev: &JournalEntry, inputs: &BTreeMap<String, String>) -> String {
    for (path, digest) in inputs {
        match prev.inputs.get(path) {
            Some(old) if old != digest => return format!("input changed: {path}"),
            None => return format!("new input: {path}"),
            _ => {}
        }
    }
    for path in prev.inputs.keys() {
        if !inputs.contains_key(path) {
            return format!("input removed: {path}");
        }
    }
    "command or toolchain changed".into()
}

fn describe_exit(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("signal {sig}");
        }
    }
    match status.code() {
        Some(code) => format!("code {code}"),
        None => "unknown".into(),
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty()
                || a.contains(|c: char| c.is_whitespace() || "'\"$&|;<>()`\\".contains(c))
            {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Hash the same length-prefixed payload as `ActionKey::digest`, but feed it
/// directly to BLAKE3. This avoids cloning argv/input maps and allocating a
/// complete canonical string for every cache check.
fn streamed_action_key(
    builder: &str,
    target: &str,
    argv: &[String],
    cwd: &str,
    toolchain_hash: &str,
    env: &BTreeMap<String, String>,
    inputs: &BTreeMap<String, String>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    update_key_field(&mut hasher, "schema", "frost-action-key-v2");
    update_key_field(&mut hasher, "builder", builder);
    update_key_field(&mut hasher, "target", target);
    update_key_field(&mut hasher, "cwd", cwd);
    update_key_field(&mut hasher, "toolchain", toolchain_hash);
    for arg in argv {
        update_key_field(&mut hasher, "argv", arg);
    }
    for (key, value) in env {
        update_key_field(&mut hasher, "env", key);
        update_key_field(&mut hasher, "env", value);
    }
    for (path, digest) in inputs {
        update_key_field(&mut hasher, "input", path);
        update_key_field(&mut hasher, "input", digest);
    }
    hasher.finalize().to_hex().to_string()
}

fn action_key_argv(action: &frostbuild_core::graph::ActionNode) -> Cow<'_, [String]> {
    if action.followup_argv.is_empty() && action.clean_dirs.is_empty() && !action.preserve_outputs {
        return Cow::Borrowed(&action.argv);
    }
    let mut argv = action.argv.clone();
    if action.preserve_outputs {
        argv.push("\0frost-preserve-outputs".into());
    }
    for directory in &action.clean_dirs {
        // NUL cannot occur in an OS argument, making these internal boundary
        // tags unambiguous in the canonical key payload.
        argv.push("\0frost-clean-dir".into());
        argv.push(directory.clone());
    }
    for command in &action.followup_argv {
        argv.push("\0frost-next-command".into());
        argv.extend(command.iter().cloned());
    }
    Cow::Owned(argv)
}

fn update_key_field(hasher: &mut blake3::Hasher, key: &str, value: &str) {
    hasher.update(key.as_bytes());
    hasher.update(b"\0");
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    let mut length = value.len();
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + (length % 10) as u8;
        length /= 10;
        if length == 0 {
            break;
        }
    }
    hasher.update(&digits[cursor..]);
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    hasher.update(b"\0");
}

/// Hash identifying the compiler binary so a toolchain swap invalidates the
/// cache (a lightweight stand-in for the closure hashing planned in #28).
pub fn toolchain_fingerprint(cc: &str) -> Result<String> {
    let resolved: PathBuf = if cc.contains('/') {
        PathBuf::from(cc)
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join(cc))
            .find(|candidate| candidate.is_file())
            .with_context(|| format!("compiler {cc:?} not found in PATH"))?
    };
    frostbuild_core::hashcache::hash_file(&resolved)
        .with_context(|| format!("compiler {} not accessible", resolved.display()))
}

/// Stat identity of the toolchain binaries that produced a fingerprint.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct ToolchainStamp {
    tools: Vec<(String, i128, u64, u64)>,
    fingerprint: String,
}

const TOOLCHAIN_STAMP_PATH: &str = ".frost/toolchain.bin";

/// Fingerprint of the compiler closure, cached by the stat identity of the
/// configured driver binaries.
///
/// A second function used to exist that also mixed in `cc --print-sysroot`,
/// but nothing called it, so the fingerprint frost actually used was the
/// weaker of the two. Rather than pay a process spawn per build to reconcile
/// them, note why the sysroot needs no separate treatment: an explicit
/// `--sysroot=` reaches the action key through argv, a default sysroot is a
/// property of the driver binary whose contents are hashed here, and the
/// headers actually read from it arrive as depfile-discovered inputs.
///
/// This used to load the workspace-wide content cache — megabytes covering
/// every source file — to digest a handful of executables. It now keeps its
/// own stamp: a few stats on the warm path, and the binaries are re-hashed
/// only when one of them actually changed.
pub fn toolchain_closure_fingerprint_cached(
    root: &Path,
    toolchain: &frostbuild_core::manifest::Toolchain,
) -> Result<String> {
    let shell = frostbuild_core::graph::SHELL.to_string();
    // The shell is in here because frost picks it, the same reason the C
    // drivers are: every genrule and shell test runs through it, and a
    // different /bin/sh can produce different bytes from the same command.
    let mut all = vec![&toolchain.cc, &toolchain.cxx, &toolchain.ar];
    if let Some(kofunc) = &toolchain.kofunc {
        all.push(kofunc);
    }
    all.extend(toolchain.tools.values());
    all.push(&shell);
    let mut tools = Vec::with_capacity(all.len());
    let mut resolved_paths = Vec::with_capacity(all.len());
    for tool in all {
        // A manifest may name a driver by a workspace-relative path (a
        // wrapper script for a cross toolchain, say), which only resolves
        // against the workspace root, not the process working directory.
        let resolved = resolve_executable(tool)?;
        let resolved = if resolved.is_absolute() {
            resolved
        } else {
            root.join(resolved)
        };
        let stat = std::fs::metadata(&resolved)
            .map(|m| stat_identity(&m))
            .unwrap_or((0, 0, 0));
        tools.push((
            resolved.to_string_lossy().into_owned(),
            stat.0,
            stat.1,
            stat.2,
        ));
        resolved_paths.push((tool.clone(), resolved));
    }

    let stamp_path = root.join(TOOLCHAIN_STAMP_PATH);
    if let Some(stamp) = std::fs::read(&stamp_path)
        .ok()
        .and_then(|b| postcard::from_bytes::<ToolchainStamp>(&b).ok())
    {
        if stamp.tools == tools {
            return Ok(stamp.fingerprint);
        }
    }

    let mut hasher = blake3::Hasher::new();
    for (tool, resolved) in &resolved_paths {
        hasher.update(tool.as_bytes());
        hasher.update(b"\0");
        hasher.update(
            frostbuild_core::hashcache::hash_file(resolved)
                .with_context(|| format!("compiler {} not accessible", resolved.display()))?
                .as_bytes(),
        );
        hasher.update(b"\0");
    }
    let fingerprint = hasher.finalize().to_hex().to_string();
    if let Some(parent) = stamp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stamp = ToolchainStamp {
        tools,
        fingerprint: fingerprint.clone(),
    };
    let tmp = stamp_path.with_extension("bin.tmp");
    std::fs::write(&tmp, postcard::to_allocvec(&stamp)?)?;
    std::fs::rename(&tmp, &stamp_path)?;
    Ok(fingerprint)
}

#[cfg(unix)]
fn stat_identity(meta: &std::fs::Metadata) -> (i128, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (
        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        meta.size(),
        meta.ino(),
    )
}

#[cfg(not(unix))]
fn stat_identity(meta: &std::fs::Metadata) -> (i128, u64, u64) {
    (0, meta.len(), 0)
}

fn resolve_executable(tool: &str) -> Result<PathBuf> {
    if tool.contains('/') {
        return Ok(PathBuf::from(tool));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("tool {tool:?} not found in PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes_specials() {
        let argv = vec!["cc".to_string(), "a b".to_string(), "plain".to_string()];
        assert_eq!(shell_join(&argv), "cc 'a b' plain");
    }

    #[test]
    fn streamed_action_key_matches_the_canonical_core_key() {
        let root = Path::new("/workspace");
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "cp $in $out".to_string(),
        ];
        let env = BTreeMap::from([
            ("CPATH".to_string(), "/headers".to_string()),
            ("SDKROOT".to_string(), "/sdk".to_string()),
        ]);
        let inputs = BTreeMap::from([
            ("include/a.h".to_string(), "abc".to_string()),
            ("src/main.c".to_string(), "def".to_string()),
        ]);
        let mut canonical = frostbuild_core::ActionKey::new(
            "frost-engine-v1",
            "compile:main",
            argv.clone(),
            root,
            "toolchain",
        );
        for (key, value) in &env {
            canonical = canonical.with_env(key, value);
        }
        for (path, digest) in &inputs {
            canonical = canonical.with_input(path, digest);
        }
        assert_eq!(
            streamed_action_key(
                "frost-engine-v1",
                "compile:main",
                &argv,
                ".",
                "toolchain",
                &env,
                &inputs,
            ),
            canonical.digest(root)
        );
    }

    #[test]
    fn multi_step_commands_and_clean_dirs_are_unambiguous_key_material() {
        fn action(
            followup_argv: Vec<Vec<String>>,
            clean_dirs: Vec<String>,
            preserve_outputs: bool,
        ) -> frostbuild_core::graph::ActionNode {
            frostbuild_core::graph::ActionNode {
                id: "command:java".into(),
                desc: "RUN java [javac]".into(),
                kind: ActionKind::Command,
                target: "java".into(),
                sandbox: false,
                argv: vec!["javac".into(), "Hello.java".into()],
                followup_argv,
                clean_dirs,
                preserve_outputs,
                env: BTreeMap::new(),
                pass_env: Vec::new(),
                inputs: Vec::new(),
                order_only_inputs: Vec::new(),
                outputs: Vec::new(),
                depfile: None,
            }
        }
        let digest = |action: &frostbuild_core::graph::ActionNode| {
            streamed_action_key(
                "frost-engine-v1",
                &action.id,
                action_key_argv(action).as_ref(),
                ".",
                "toolchain",
                &BTreeMap::new(),
                &BTreeMap::new(),
            )
        };

        let primary_only = action(Vec::new(), Vec::new(), false);
        assert!(matches!(action_key_argv(&primary_only), Cow::Borrowed(_)));
        let jar = action(
            vec![vec!["jar".into(), "classes".into()]],
            vec![".frost/tmp/debug/java".into()],
            false,
        );
        let differently_segmented = action(
            vec![vec!["jar".into()], vec!["classes".into()]],
            vec![".frost/tmp/debug/java".into()],
            false,
        );
        let different_clean_dir = action(
            vec![vec!["jar".into(), "classes".into()]],
            vec![".frost/tmp/debug/java-v2".into()],
            false,
        );
        let preserving = action(Vec::new(), Vec::new(), true);

        assert_ne!(digest(&primary_only), digest(&jar));
        assert_ne!(digest(&jar), digest(&differently_segmented));
        assert_ne!(digest(&jar), digest(&different_clean_dir));
        assert_ne!(digest(&primary_only), digest(&preserving));
    }

    #[test]
    fn the_fingerprint_covers_the_shell_frost_chooses() {
        // Every genrule and shell test runs through this interpreter, and the
        // manifest has no way to name it, so leaving it out would make it the
        // one tool frost picks and does not account for.
        let dir = std::env::temp_dir().join(format!("frost-tc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools/kofun"), b"kofun compiler v1\n").unwrap();
        std::fs::write(dir.join("tools/language"), b"language adapter v1\n").unwrap();
        let mut named_tools = BTreeMap::new();
        named_tools.insert("language".into(), "tools/language".into());
        let toolchain = frostbuild_core::manifest::Toolchain {
            cc: frostbuild_core::graph::SHELL.into(),
            cxx: frostbuild_core::graph::SHELL.into(),
            ar: frostbuild_core::graph::SHELL.into(),
            kofunc: Some("tools/kofun".into()),
            tools: named_tools,
            arflags: vec!["rcsD".into()],
            cflags: Vec::new(),
            cxxflags: Vec::new(),
            ldflags: Vec::new(),
        };
        let first = toolchain_closure_fingerprint_cached(&dir, &toolchain).unwrap();
        assert_eq!(
            toolchain_closure_fingerprint_cached(&dir, &toolchain).unwrap(),
            first,
            "an unchanged toolchain keeps its fingerprint"
        );
        let stamp = std::fs::read(dir.join(TOOLCHAIN_STAMP_PATH)).unwrap();
        let stamp: ToolchainStamp = postcard::from_bytes(&stamp).unwrap();
        assert!(
            stamp
                .tools
                .iter()
                .any(|(path, ..)| path.ends_with(frostbuild_core::graph::SHELL)),
            "the shell must be one of the hashed tools: {:?}",
            stamp.tools
        );
        assert!(
            stamp
                .tools
                .iter()
                .any(|(path, ..)| path.ends_with("tools/language")),
            "named command tools must be hashed: {:?}",
            stamp.tools
        );
        assert!(
            stamp
                .tools
                .iter()
                .any(|(path, ..)| path.ends_with("tools/kofun")),
            "the configured Kofun compiler must be hashed: {:?}",
            stamp.tools
        );
        std::fs::write(dir.join("tools/kofun"), b"kofun compiler v2 changed\n").unwrap();
        assert_ne!(
            toolchain_closure_fingerprint_cached(&dir, &toolchain).unwrap(),
            first,
            "changing kofunc must invalidate the toolchain fingerprint"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn toolchain_fingerprint_is_stable_and_errors_on_missing() {
        let a = toolchain_fingerprint(frostbuild_core::graph::SHELL).unwrap();
        let b = toolchain_fingerprint(frostbuild_core::graph::SHELL).unwrap();
        assert_eq!(a, b);
        assert!(toolchain_fingerprint("definitely-not-a-compiler-xyz").is_err());
    }
}
