//! Parallel build engine: dependency-counting scheduler, real process
//! execution, and constructive-trace action caching.
//!
//! Rebuild decision: an action is skipped when its action-key digest
//! (command + toolchain + content digests of declared and discovered inputs)
//! matches the journal entry from the last run AND its recorded outputs are
//! intact on disk. Because downstream keys are computed from upstream output
//! *content*, an action that re-runs but reproduces identical outputs stops
//! dirtiness from propagating (early cutoff).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use frostbuild_core::graph::{ActionId, BuildGraph};
use frostbuild_core::hashcache::HashCache;
use frostbuild_core::journal::{Journal, JournalEntry};
use frostbuild_core::{depfile, ActionKey};

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub jobs: usize,
    pub keep_going: bool,
    pub dry_run: bool,
    pub verbose: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            jobs: std::thread::available_parallelism().map_or(1, |n| n.get()),
            keep_going: false,
            dry_run: false,
            verbose: false,
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
    ready: std::collections::VecDeque<usize>,
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
    toolchain_hash: String,
    opts: BuildOptions,
    cache: Mutex<HashCache>,
    journal: Mutex<Journal>,
    shared: Mutex<Shared>,
    cv: Condvar,
}

impl<'a> Engine<'a> {
    pub fn new(
        root: &'a Path,
        graph: &'a BuildGraph,
        closure: Vec<ActionId>,
        toolchain_hash: String,
        opts: BuildOptions,
    ) -> Self {
        let closure_index: HashMap<ActionId, usize> =
            closure.iter().enumerate().map(|(i, &a)| (a, i)).collect();

        let mut waiting = vec![0usize; closure.len()];
        let mut dependents = vec![Vec::new(); closure.len()];
        for (local, &action_id) in closure.iter().enumerate() {
            let mut producers = BTreeSet::new();
            for &input in &graph.actions[action_id].inputs {
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

        let ready = waiting
            .iter()
            .enumerate()
            .filter(|(_, &w)| w == 0)
            .map(|(i, _)| i)
            .collect();

        let n = closure.len();
        Self {
            root,
            graph,
            closure,
            closure_index,
            dependents,
            toolchain_hash,
            opts,
            cache: Mutex::new(HashCache::load(root)),
            journal: Mutex::new(Journal::load(root)),
            shared: Mutex::new(Shared {
                ready,
                waiting,
                outcomes: vec![None; n],
                pending: n,
                abort: false,
                printed: 0,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn run(self) -> Result<BuildReport> {
        let workers = self.opts.jobs.max(1).min(self.closure.len().max(1));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| self.worker());
            }
        });

        let shared = self.shared.into_inner().unwrap();
        if !self.opts.dry_run {
            self.journal.into_inner().unwrap().save(self.root)?;
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
        Ok(BuildReport { results })
    }

    fn worker(&self) {
        loop {
            let local = {
                let mut s = self.shared.lock().unwrap();
                loop {
                    if s.abort && s.ready.is_empty() {
                        return;
                    }
                    if let Some(i) = s.ready.pop_front() {
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
                        s.ready.push_back(dep);
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
            for &input in &action.inputs {
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
            journal.actions.get(&action.id).cloned()
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

        if let Some(prev) = &previous {
            if prev.key == key {
                match self.outputs_intact(prev) {
                    Ok(None) => return Outcome::Cached,
                    Ok(Some(bad)) => {
                        return self.execute(
                            local,
                            action,
                            inputs,
                            format!("output missing or modified: {bad}"),
                        )
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
                cache.invalidate(&self.graph.files[out].path);
            }
        }

        let started = Instant::now();
        let mut cmd = Command::new(&action.argv[0]);
        cmd.args(&action.argv[1..]).current_dir(self.root);
        let output = match cmd.output() {
            Ok(o) => o,
            Err(err) => {
                let detail = format!("failed to spawn {:?}: {err}", action.argv[0]);
                self.print_failure(action, &detail);
                return Outcome::Failed { reason, detail };
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;

        let captured = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
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

        let key = self.action_key(action, &inputs);
        {
            let mut journal = self.journal.lock().unwrap();
            journal.actions.insert(
                action.id.clone(),
                JournalEntry {
                    key,
                    inputs,
                    discovered,
                    outputs,
                    duration_ms,
                },
            );
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
        let mut map = BTreeMap::new();
        let mut cache = self.cache.lock().unwrap();
        for path in paths {
            let digest = cache.digest(self.root, path)?;
            map.insert(path.clone(), digest);
        }
        Ok(map)
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
    let meta = std::fs::metadata(&resolved)
        .with_context(|| format!("compiler {} not accessible", resolved.display()))?;
    let stamp = format!(
        "{}\0{}\0{:?}",
        resolved.display(),
        meta.len(),
        meta.modified().ok()
    );
    Ok(blake3::hash(stamp.as_bytes()).to_hex().to_string())
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
