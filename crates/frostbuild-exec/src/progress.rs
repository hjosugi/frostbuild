use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug, Clone)]
pub struct ProgressSender(Sender<ProgressEvent>);

impl ProgressSender {
    pub(crate) fn emit(&self, event: ProgressEvent) {
        // Rendering is best-effort and must never make a build fail.
        let _ = self.0.send(event);
    }
}

pub fn progress_channel() -> (ProgressSender, Receiver<ProgressEvent>) {
    let (sender, receiver) = mpsc::channel();
    (ProgressSender(sender), receiver)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    CacheHit,
    Executed,
    Failed,
    Skipped,
    WouldRun,
    MayRun,
}

impl ProgressState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CacheHit => "cache hit",
            Self::Executed => "cache miss",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::WouldRun => "would run",
            Self::MayRun => "may run",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProgressEvent {
    BuildStarted {
        total: usize,
        jobs: usize,
        critical_path_ms: u64,
        critical_path: Vec<String>,
    },
    /// The cache preflight validated the complete closure. This single event
    /// preserves the O(1) renderer cost of the all-cached fast path.
    AllCached {
        total: usize,
    },
    ActionStarted {
        slot: usize,
        id: String,
        desc: String,
        command: String,
        critical: bool,
    },
    ActionRunning {
        id: String,
    },
    ActionOutput {
        id: String,
        output: String,
    },
    ActionFinished {
        slot: usize,
        completed: usize,
        total: usize,
        id: String,
        desc: String,
        state: ProgressState,
        duration_ms: u64,
        detail: String,
        critical: bool,
    },
    BuildFinished {
        success: bool,
        elapsed_ms: u64,
    },
}
