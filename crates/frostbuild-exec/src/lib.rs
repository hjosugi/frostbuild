//! Parallel build engine: dependency-counting scheduler, real process
//! execution, and constructive-trace action caching.
//!
//! Rebuild decision: an action is skipped when its action-key digest
//! (command + toolchain + content digests of declared and discovered inputs)
//! matches the journal entry from the last run AND its recorded outputs are
//! intact on disk. Because downstream keys are computed from upstream output
//! *content*, an action that re-runs but reproduces identical outputs stops
//! dirtiness from propagating (early cutoff).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use frostbuild_core::cas::LocalCas;
use frostbuild_core::graph::{ActionId, ActionKind, BuildGraph};
use frostbuild_core::hashcache::HashCache;
use frostbuild_core::journal::{Journal, JournalEntry};
use frostbuild_core::{depfile, ActionKey};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static RUNNING_PROCESS_GROUPS: OnceLock<Mutex<BTreeSet<i32>>> = OnceLock::new();
static SIGNAL_HANDLER: OnceLock<()> = OnceLock::new();

pub fn install_signal_handler() -> Result<()> {
    if SIGNAL_HANDLER.get().is_some() {
        return Ok(());
    }
    ctrlc::set_handler(|| {
        CANCELLED.store(true, Ordering::SeqCst);
        if let Some(groups) = RUNNING_PROCESS_GROUPS.get() {
            for pid in groups.lock().unwrap().iter().copied() {
                // SAFETY: kill is async-process-safe; negative pid addresses the process group.
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                }
            }
        }
    })?;
    let _ = SIGNAL_HANDLER.set(());
    Ok(())
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
    pub scheduler: Scheduler,
    pub estimator: Estimator,
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
            cas_max_bytes: 10 * 1024 * 1024 * 1024,
            scheduler: Scheduler::CriticalPath,
            estimator: Estimator::Journal,
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
    printed: usize,
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
    estimated_work_ms: u64,
    toolchain_hash: String,
    opts: BuildOptions,
    cache: Mutex<HashCache>,
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
        let journal = Journal::load(root);
        let plan = Schedule::plan(graph, closure, &journal, opts.scheduler, opts.estimator);
        let Schedule {
            closure,
            closure_index,
            dependents,
            waiting,
            priority,
            critical_path_ms,
            work_ms: estimated_work_ms,
            ..
        } = plan;
        let ready = waiting
            .iter()
            .enumerate()
            .filter(|(_, &w)| w == 0)
            .map(|(i, _)| (priority[i], Reverse(i)))
            .collect();

        let n = closure.len();
        let cas_max_bytes = opts.cas_max_bytes;
        Self {
            root,
            graph,
            closure,
            closure_index,
            dependents,
            priority,
            critical_path_ms,
            estimated_work_ms,
            toolchain_hash,
            opts,
            cache: Mutex::new(HashCache::load(root)),
            journal: Mutex::new(journal),
            shared: Mutex::new(Shared {
                ready,
                waiting,
                outcomes: vec![None; n],
                pending: n,
                abort: false,
                printed: 0,
            }),
            cv: Condvar::new(),
            cas: LocalCas::new(root, cas_max_bytes),
        }
    }

    pub fn run(self) -> Result<BuildReport> {
        let workers = self.opts.jobs.max(1).min(self.closure.len().max(1));
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| self.worker());
            }
        });
        let makespan_ms = started.elapsed().as_millis() as u64;

        let shared = self.shared.into_inner().unwrap();
        if !self.opts.dry_run {
            let journal = self.journal.into_inner().unwrap();
            let journal_path = self.root.join(frostbuild_core::journal::JOURNAL_REL_PATH);
            if std::fs::metadata(journal_path).is_ok_and(|m| m.len() > 32 * 1024 * 1024) {
                journal.save(self.root)?;
            }
            let _ = self.cas.gc()?;
        }
        self.cache.into_inner().unwrap().save(self.root)?;

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
        Ok(BuildReport { results, stats })
    }

    fn worker(&self) {
        loop {
            let local = {
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

            let outcome = self.process(local);

            let mut s = self.shared.lock().unwrap();
            let failed = matches!(outcome, Outcome::Failed { .. });
            s.outcomes[local] = Some(outcome);
            s.pending -= 1;
            if failed && !self.opts.keep_going {
                s.abort = true;
                s.ready.clear();
            }
            if !s.abort {
                for &dep in &self.dependents[local] {
                    s.waiting[dep] -= 1;
                    if s.waiting[dep] == 0 {
                        let priority = self.priority(dep);
                        s.ready.push((priority, Reverse(dep)));
                    }
                }
            }
            self.cv.notify_all();
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

        let previous = {
            let journal = self.journal.lock().unwrap();
            journal
                .actions
                .get(&journal_id(self.graph, action))
                .cloned()
        };

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
        if self.opts.dry_run {
            return Outcome::WouldRun { reason };
        }

        if let Err(err) = self.prepare_output_dirs(action) {
            return Outcome::Failed {
                reason,
                detail: format!("{err:#}"),
            };
        }

        {
            let mut cache = self.cache.lock().unwrap();
            for &out in &action.outputs {
                let path = &self.graph.files[out].path;
                cache.invalidate(path);
                let _ = std::fs::remove_file(self.root.join(path));
            }
        }

        let started = Instant::now();
        let mut cmd = match self.command_for(action, &inputs) {
            Ok(command) => command,
            Err(err) => {
                return Outcome::Failed {
                    reason,
                    detail: format!("{err:#}"),
                }
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let detail = format!("failed to spawn {:?}: {err}", action.argv[0]);
                self.print_failure(action, &detail);
                return Outcome::Failed { reason, detail };
            }
        };
        let pid = child.id() as i32;
        RUNNING_PROCESS_GROUPS
            .get_or_init(|| Mutex::new(BTreeSet::new()))
            .lock()
            .unwrap()
            .insert(pid);
        let output = child.wait_with_output();
        RUNNING_PROCESS_GROUPS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&pid);
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                return Outcome::Failed {
                    reason,
                    detail: format!("failed waiting for {}: {err}", action.id),
                }
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;

        let captured = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            self.remove_partial_outputs(action);
            let detail = format!(
                "command: {}\nexit: {}\n{}",
                shell_join(&action.argv),
                describe_exit(&output.status),
                captured.trim_end()
            );
            self.print_failure(action, &detail);
            return Outcome::Failed { reason, detail };
        }

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
                        self.print_failure(action, &detail);
                        return Outcome::Failed { reason, detail };
                    }
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
            self.print_failure(action, &detail);
            return Outcome::Failed { reason, detail };
        }

        for (path, digest) in &outputs {
            if let Err(err) = self.cas.put(&self.root.join(path), digest) {
                return Outcome::Failed {
                    reason,
                    detail: format!("failed to store output in CAS: {err:#}"),
                };
            }
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
                self.print_failure(action, &detail);
                return Outcome::Failed {
                    reason: "determinism check failed".into(),
                    detail,
                };
            }
            let first = outputs.clone();
            let mut second = match self.command_for(action, &inputs) {
                Ok(command) => command,
                Err(err) => {
                    return Outcome::Failed {
                        reason,
                        detail: format!("{err:#}"),
                    }
                }
            };
            match second.output() {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    return Outcome::Failed {
                        reason,
                        detail: format!("determinism rerun failed: {}", describe_exit(&out.status)),
                    }
                }
                Err(err) => {
                    return Outcome::Failed {
                        reason,
                        detail: format!("determinism rerun failed: {err}"),
                    }
                }
            }
            for path in &output_paths {
                self.cache.lock().unwrap().invalidate(path);
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
                self.print_failure(action, &detail);
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

        self.print_progress(local, action, &captured);
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
        let mut key = ActionKey::new(
            "frost-engine-v1",
            &action.id,
            action.argv.iter().cloned(),
            self.root,
            &self.toolchain_hash,
        );
        for (path, digest) in inputs {
            key = key.with_input(path.clone(), digest.clone());
        }
        key.digest(self.root)
    }

    fn digest_all(&self, paths: &[String]) -> Result<BTreeMap<String, String>> {
        let mut cache = self.cache.lock().unwrap();
        cache.digest_many(self.root, paths)
    }

    /// Returns Ok(None) when all recorded outputs are on disk with matching
    /// digests, or Ok(Some(path)) naming the first stale output.
    fn outputs_intact(&self, prev: &JournalEntry) -> Result<Option<String>> {
        let mut cache = self.cache.lock().unwrap();
        for (path, recorded) in &prev.outputs {
            let current = cache.digest(self.root, path)?;
            if &current != recorded {
                return Ok(Some(path.clone()));
            }
        }
        Ok(None)
    }

    fn prepare_output_dirs(&self, action: &frostbuild_core::graph::ActionNode) -> Result<()> {
        for &out in &action.outputs {
            let path = self.root.join(&self.graph.files[out].path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
        }
        if let Some(dep) = &action.depfile {
            let path = self.root.join(dep);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Ok(())
    }

    fn restore_outputs(&self, prev: &JournalEntry) -> Result<bool> {
        for (path, digest) in &prev.outputs {
            if !self.cas.materialize(digest, &self.root.join(path))? {
                return Ok(false);
            }
            self.cache.lock().unwrap().invalidate(path);
        }
        Ok(true)
    }

    fn remove_partial_outputs(&self, action: &frostbuild_core::graph::ActionNode) {
        for &output in &action.outputs {
            let _ = std::fs::remove_file(self.root.join(&self.graph.files[output].path));
        }
    }

    fn command_for(
        &self,
        action: &frostbuild_core::graph::ActionNode,
        inputs: &BTreeMap<String, String>,
    ) -> Result<Command> {
        let mut command = if self.opts.sandbox && action.sandbox {
            sandbox_command(self.root, self.graph, action, inputs)?
        } else {
            let mut command = Command::new(&action.argv[0]);
            command.args(&action.argv[1..]).current_dir(self.root);
            command
        };
        let whitelist = [
            "PATH",
            "HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "SystemRoot",
            "SDKROOT",
            "MACOSX_DEPLOYMENT_TARGET",
            "CPATH",
            "C_INCLUDE_PATH",
            "CPLUS_INCLUDE_PATH",
            "LIBRARY_PATH",
        ];
        let env = whitelist
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (key, value)))
            .collect::<Vec<_>>();
        command
            .env_clear()
            .envs(env)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            // Actions never read from the terminal. Inheriting stdin lets a
            // command that expects input (`cat > out` when ${in} expanded to
            // nothing, an accidental interactive prompt) block forever with no
            // output and no diagnostic, which looks exactly like a slow build.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        Ok(command)
    }

    fn priority(&self, local: usize) -> u64 {
        self.priority[local]
    }

    fn print_progress(
        &self,
        _local: usize,
        action: &frostbuild_core::graph::ActionNode,
        captured: &str,
    ) {
        let mut s = self.shared.lock().unwrap();
        s.printed += 1;
        let line = format!("[{}/{}] {}", s.printed, self.closure.len(), action.desc);
        drop(s);
        if self.opts.verbose {
            println!("{line}\n  $ {}", shell_join(&action.argv));
        } else {
            println!("{line}");
        }
        let trimmed = captured.trim_end();
        if !trimmed.is_empty() {
            println!("{trimmed}");
        }
    }

    fn print_failure(&self, action: &frostbuild_core::graph::ActionNode, detail: &str) {
        println!("FAILED: {}\n{detail}", action.desc);
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
    }
}

fn sandbox_command(
    root: &Path,
    graph: &BuildGraph,
    action: &frostbuild_core::graph::ActionNode,
    inputs: &BTreeMap<String, String>,
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
    let mut args = action.argv.iter().peekable();
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
    for directory in writable {
        std::fs::create_dir_all(&directory)?;
        add_sandbox_dirs(&mut command, root, directory.parent(), &mut made_dirs);
        command.arg("--bind").arg(&directory).arg(&directory);
    }
    command
        .arg("--chdir")
        .arg(root)
        .arg("--")
        .args(&action.argv);
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

pub fn toolchain_closure_fingerprint(
    toolchain: &frostbuild_core::manifest::Toolchain,
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    for tool in [&toolchain.cc, &toolchain.cxx, &toolchain.ar] {
        hasher.update(tool.as_bytes());
        hasher.update(b"\0");
        hasher.update(toolchain_fingerprint(tool)?.as_bytes());
        hasher.update(b"\0");
    }
    if let Ok(output) = Command::new(&toolchain.cc).arg("--print-sysroot").output() {
        if output.status.success() {
            hasher.update(&output.stdout);
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn toolchain_closure_fingerprint_cached(
    root: &Path,
    toolchain: &frostbuild_core::manifest::Toolchain,
) -> Result<String> {
    let mut cache = HashCache::load(root);
    let mut hasher = blake3::Hasher::new();
    for tool in [&toolchain.cc, &toolchain.cxx, &toolchain.ar] {
        let resolved = resolve_executable(tool)?;
        let path = resolved.to_string_lossy().into_owned();
        hasher.update(tool.as_bytes());
        hasher.update(b"\0");
        hasher.update(cache.digest(root, &path)?.as_bytes());
        hasher.update(b"\0");
    }
    cache.save(root)?;
    Ok(hasher.finalize().to_hex().to_string())
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
    fn toolchain_fingerprint_is_stable_and_errors_on_missing() {
        let a = toolchain_fingerprint("sh").unwrap();
        let b = toolchain_fingerprint("sh").unwrap();
        assert_eq!(a, b);
        assert!(toolchain_fingerprint("definitely-not-a-compiler-xyz").is_err());
    }
}
