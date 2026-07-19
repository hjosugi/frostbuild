//! Measurement for build schedules.
//!
//! A build tool that claims to be fast should be able to say *why*, and to
//! answer "would a different strategy help, and by how much?" without running
//! the build again. Wall-clock A/B runs cannot answer it honestly: each run
//! needs a cleared cache, so the comparison is between two cold builds with
//! different page-cache and disk state, and the difference is buried in noise.
//!
//! This crate answers it by scheduling without executing. The durations come
//! from the journal, the ordering comes from the same [`Schedule`] the engine
//! uses, so a sweep describes the schedulers that would actually run. Results
//! are deterministic, which is what makes them usable as a CI regression gate.
//!
//! What simulation does *not* model: I/O contention, memory bandwidth, CPU
//! frequency scaling, and process startup. It measures the quality of the
//! ordering, not absolute time. Calibrate against one real run before reading
//! absolute numbers ([`Sweep::calibrate`]).

use frostbuild_exec::{Estimator, Schedule, Scheduler, Simulation};

pub use frostbuild_exec::{Estimator as SweepEstimator, Scheduler as SweepScheduler};

/// Every strategy the engine can run, in the order a report should list them.
pub const SCHEDULERS: [Scheduler; 2] = [Scheduler::CriticalPath, Scheduler::Fifo];
pub const ESTIMATORS: [Estimator; 4] = [
    Estimator::Journal,
    Estimator::Learned,
    Estimator::Heuristic,
    Estimator::Static,
];

/// One measured point: a strategy at a worker count.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub scheduler: Scheduler,
    pub estimator: Estimator,
    pub simulation: Simulation,
}

/// A strategy sweep over worker counts.
#[derive(Debug, Clone)]
pub struct Sweep {
    pub points: Vec<Point>,
    pub jobs: Vec<usize>,
    /// Critical path under the journal estimator: the honest lower bound.
    pub critical_path_ms: u64,
    pub work_ms: u64,
    pub actions: usize,
    /// Actions along the critical path with their estimated durations.
    pub critical_path: Vec<(String, u64)>,
}

impl Sweep {
    /// Simulate every (scheduler, estimator) pair at every worker count.
    ///
    /// Every point is scored against **one** set of durations — those of the
    /// journal estimator, the best record of what actions actually cost. An
    /// estimator changes the order work starts in, not how long it takes, so
    /// scoring each estimator against its own guesses would simply rank the
    /// most optimistic guesser first.
    ///
    /// `plan_for` builds the schedule for a given strategy; the caller owns
    /// the graph and journal, so this crate stays free of I/O.
    pub fn run(
        jobs: &[usize],
        schedulers: &[Scheduler],
        estimators: &[Estimator],
        mut plan_for: impl FnMut(Scheduler, Estimator) -> Schedule,
    ) -> Self {
        let reference = plan_for(Scheduler::CriticalPath, Estimator::Journal);
        let durations = reference.duration_ms.clone();
        let critical_path = reference
            .critical_path
            .iter()
            .map(|&local| {
                (
                    format!("#{local}"),
                    reference.duration_ms.get(local).copied().unwrap_or(0),
                )
            })
            .collect();
        let mut points = Vec::new();
        for &scheduler in schedulers {
            for &estimator in estimators {
                let plan = plan_for(scheduler, estimator);
                for &j in jobs {
                    points.push(Point {
                        scheduler,
                        estimator,
                        simulation: plan.simulate_against(j, &durations),
                    });
                }
            }
        }
        Self {
            points,
            jobs: jobs.to_vec(),
            critical_path_ms: reference.critical_path_ms,
            work_ms: reference.work_ms,
            actions: reference.closure.len(),
            critical_path,
        }
    }

    pub fn get(&self, scheduler: Scheduler, estimator: Estimator, jobs: usize) -> Option<&Point> {
        self.points.iter().find(|p| {
            p.scheduler == scheduler && p.estimator == estimator && p.simulation.jobs == jobs
        })
    }

    /// Fastest simulated point. Ties break toward fewer workers, since the
    /// cheaper configuration is the better answer at equal speed.
    pub fn best(&self) -> Option<&Point> {
        self.points
            .iter()
            .min_by_key(|p| (p.simulation.makespan_ms, p.simulation.jobs))
    }

    /// Ratio between an observed makespan and the simulated one at the same
    /// worker count. A value far from 1.0 means the simulation's assumptions
    /// (no contention, journal durations still current) do not hold here, and
    /// absolute simulated times should not be quoted.
    pub fn calibrate(
        &self,
        scheduler: Scheduler,
        estimator: Estimator,
        jobs: usize,
        observed_ms: u64,
    ) -> Option<f64> {
        let p = self.get(scheduler, estimator, jobs)?;
        (p.simulation.makespan_ms > 0).then(|| observed_ms as f64 / p.simulation.makespan_ms as f64)
    }
}

/// Render a sweep as an aligned table: strategies down, worker counts across.
pub fn render_table(sweep: &Sweep) -> String {
    let mut out = String::new();
    let label = |s: Scheduler, e: Estimator| format!("{} / {}", s.as_str(), e.as_str());
    let width = sweep
        .points
        .iter()
        .map(|p| label(p.scheduler, p.estimator).len())
        .max()
        .unwrap_or(8)
        .max("strategy".len());

    out.push_str(&format!("  {:<width$}", "strategy", width = width));
    for j in &sweep.jobs {
        out.push_str(&format!("  {:>10}", format!("-j {j}")));
    }
    out.push('\n');

    let mut seen = Vec::new();
    for p in &sweep.points {
        let key = (p.scheduler, p.estimator);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push_str(&format!(
            "  {:<width$}",
            label(p.scheduler, p.estimator),
            width = width
        ));
        for j in &sweep.jobs {
            match sweep.get(p.scheduler, p.estimator, *j) {
                Some(hit) => out.push_str(&format!(
                    "  {:>10}",
                    format!("{} ms", hit.simulation.makespan_ms)
                )),
                None => out.push_str(&format!("  {:>10}", "-")),
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use frostbuild_core::graph::BuildGraph;
    use frostbuild_core::journal::Journal;
    use frostbuild_core::manifest::Manifest;

    /// Two independent chains of unequal length feeding nothing: with one
    /// worker the makespan is the total work; with enough workers it is the
    /// longest chain. Both are checkable by hand.
    fn two_chain_graph() -> BuildGraph {
        let manifest = Manifest::parse_str(
            r#"
            [target.a1]
            kind = "genrule"
            cmd = "true"
            outputs = ["a1.out"]

            [target.a2]
            kind = "genrule"
            cmd = "true"
            outputs = ["a2.out"]
            deps = ["a1"]

            [target.b1]
            kind = "genrule"
            cmd = "true"
            outputs = ["b1.out"]
            "#,
        )
        .unwrap();
        BuildGraph::from_manifest(&manifest).unwrap()
    }

    fn plan(graph: &BuildGraph, s: Scheduler, e: Estimator) -> Schedule {
        let closure = graph
            .action_closure(&["a2".to_string(), "b1".to_string()])
            .unwrap();
        Schedule::plan(graph, closure, &Journal::default(), s, e)
    }

    #[test]
    fn one_worker_serializes_and_many_workers_reach_the_critical_path() {
        let graph = two_chain_graph();
        let p = plan(&graph, Scheduler::CriticalPath, Estimator::Heuristic);
        // Three genrules at the heuristic cost of 10 ms each.
        assert_eq!(p.work_ms, 30);
        assert_eq!(p.critical_path_ms, 20, "a1 -> a2 is the longest chain");

        assert_eq!(
            p.simulate(1).makespan_ms,
            30,
            "one worker pays for all work"
        );
        assert_eq!(
            p.simulate(8).makespan_ms,
            20,
            "spare workers cannot beat the chain"
        );
    }

    #[test]
    fn sweep_reports_every_strategy_and_finds_the_best() {
        let graph = two_chain_graph();
        let sweep = Sweep::run(&[1, 4], &SCHEDULERS, &ESTIMATORS, |s, e| plan(&graph, s, e));
        assert_eq!(sweep.points.len(), 2 * 4 * 2);
        assert_eq!(sweep.actions, 3);
        let best = sweep.best().unwrap();
        assert_eq!(
            best.simulation.makespan_ms, 20,
            "every strategy is scored on the journal clock, so no estimator \
             can look fast by guessing low"
        );
        assert_eq!(
            best.simulation.jobs, 4,
            "the fastest point uses parallelism"
        );
        for p in &sweep.points {
            assert!(
                p.simulation.makespan_ms >= sweep.critical_path_ms,
                "no schedule can beat the critical path: {p:?}"
            );
        }

        let table = render_table(&sweep);
        assert!(table.contains("critical-path / journal"), "{table}");
        assert!(table.contains("-j 4"), "{table}");
    }

    #[test]
    fn calibration_compares_observed_against_simulated() {
        let graph = two_chain_graph();
        let sweep = Sweep::run(&[4], &SCHEDULERS, &ESTIMATORS, |s, e| plan(&graph, s, e));
        let ratio = sweep
            .calibrate(Scheduler::CriticalPath, Estimator::Heuristic, 4, 40)
            .unwrap();
        assert_eq!(ratio, 2.0, "an observed 40 ms against a simulated 20 ms");
    }
}
