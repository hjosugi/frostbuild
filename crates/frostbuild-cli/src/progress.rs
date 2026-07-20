use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use frostbuild_exec::{progress_channel, ProgressEvent, ProgressSender, ProgressState};

pub struct RendererHandle {
    thread: Option<JoinHandle<()>>,
}

impl RendererHandle {
    pub fn finish(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start(no_tui: bool, verbose: bool) -> (ProgressSender, RendererHandle) {
    let (sender, receiver) = progress_channel();
    let live = !no_tui && std::env::var_os("CI").is_none() && io::stdout().is_terminal();
    let interactive_input = io::stdin().is_terminal();
    let thread = thread::spawn(move || {
        if live {
            run_live(receiver, interactive_input);
        } else {
            run_plain(receiver, verbose);
        }
    });
    (
        sender,
        RendererHandle {
            thread: Some(thread),
        },
    )
}

fn run_plain(receiver: Receiver<ProgressEvent>, verbose: bool) {
    let mut commands = HashMap::new();
    let mut outputs = HashMap::new();
    while let Ok(event) = receiver.recv() {
        match event {
            ProgressEvent::ActionStarted { id, command, .. } => {
                commands.insert(id, command);
            }
            ProgressEvent::ActionOutput { id, output } => {
                outputs.insert(id, output);
            }
            ProgressEvent::ActionFinished {
                completed,
                total,
                id,
                desc,
                state: ProgressState::Executed,
                ..
            } => {
                println!("[{completed}/{total}] {desc}");
                let command = commands.remove(&id);
                if verbose {
                    if let Some(command) = command {
                        println!("  $ {command}");
                    }
                }
                let output = outputs.remove(&id).unwrap_or_default();
                let output = output.trim_end();
                if !output.is_empty() {
                    println!("{output}");
                }
            }
            ProgressEvent::ActionFinished {
                id,
                desc,
                state: ProgressState::Failed,
                detail,
                ..
            } => {
                commands.remove(&id);
                outputs.remove(&id);
                println!("FAILED: {desc}");
                let detail = detail.trim_end();
                if !detail.is_empty() {
                    println!("{detail}");
                }
                // A failed command must become visible before other workers
                // finish and before the eventual summary.
                let _ = io::stdout().flush();
            }
            ProgressEvent::ActionFinished { id, .. } => {
                commands.remove(&id);
                outputs.remove(&id);
            }
            _ => {}
        }
    }
}

fn run_live(receiver: Receiver<ProgressEvent>, interactive_input: bool) {
    let mut terminal = match TerminalGuard::enter(interactive_input) {
        Ok(terminal) => terminal,
        Err(_) => {
            run_plain(receiver, false);
            return;
        }
    };
    let mut state = TuiState::default();
    let tick = Duration::from_millis(50);
    loop {
        let mut disconnected = false;
        match receiver.recv_timeout(tick) {
            Ok(event) => state.apply(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => disconnected = true,
        }
        loop {
            match receiver.try_recv() {
                Ok(event) => state.apply(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if interactive_input {
            handle_input(&mut state);
        }
        let _ = state.render(&mut terminal.stdout);
        if disconnected {
            break;
        }
    }
}

struct TerminalGuard {
    stdout: io::Stdout,
    raw: bool,
}

impl TerminalGuard {
    fn enter(raw: bool) -> io::Result<Self> {
        if raw {
            enable_raw_mode()?;
        }
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            if raw {
                let _ = disable_raw_mode();
            }
            return Err(error);
        }
        Ok(Self { stdout, raw })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen);
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

#[derive(Default)]
struct Slot {
    id: String,
    desc: String,
    status: String,
    started: Option<Instant>,
    duration_ms: u64,
    critical: bool,
}

#[derive(Default)]
struct TuiState {
    started: Option<Instant>,
    total: usize,
    completed: usize,
    jobs: usize,
    critical_path_ms: u64,
    critical_path: Vec<String>,
    slots: Vec<Slot>,
    cache_hits: usize,
    cache_misses: usize,
    failures: usize,
    last_failure: Option<String>,
    logs: Vec<String>,
    scroll_from_bottom: usize,
    finished: Option<(bool, u64)>,
}

impl TuiState {
    fn apply(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::BuildStarted {
                total,
                jobs,
                critical_path_ms,
                critical_path,
            } => {
                self.started = Some(Instant::now());
                self.total = total;
                self.jobs = jobs;
                self.critical_path_ms = critical_path_ms;
                self.critical_path = critical_path;
                self.slots.resize_with(jobs, Slot::default);
            }
            ProgressEvent::AllCached { total } => {
                self.total = total;
                self.completed = total;
                self.cache_hits = total;
                for slot in &mut self.slots {
                    slot.status = "cache hit".into();
                    slot.started = None;
                }
                self.push_log(format!("CACHE HIT: all {total} actions"));
            }
            ProgressEvent::ActionStarted {
                slot,
                id,
                desc,
                command,
                critical,
            } => {
                if let Some(target) = self.slots.get_mut(slot) {
                    target.id = id.clone();
                    target.desc = desc;
                    target.status = "checking cache".into();
                    target.started = Some(Instant::now());
                    target.duration_ms = 0;
                    target.critical = critical;
                }
                self.push_log(format!("START {id}: {command}"));
            }
            ProgressEvent::ActionRunning { id } => {
                if let Some(target) = self.slots.iter_mut().find(|slot| slot.id == id) {
                    target.status = "running/miss".into();
                }
                self.cache_misses += 1;
            }
            ProgressEvent::ActionOutput { id, output } => {
                for line in output.trim_end().lines() {
                    self.push_log(format!("{id}: {line}"));
                }
            }
            ProgressEvent::ActionFinished {
                slot,
                completed,
                id,
                desc,
                state,
                duration_ms,
                detail,
                critical,
                ..
            } => {
                self.completed = completed;
                match state {
                    ProgressState::CacheHit => self.cache_hits += 1,
                    ProgressState::Executed => {}
                    ProgressState::Failed => {
                        self.failures += 1;
                        self.last_failure = Some(desc.clone());
                    }
                    ProgressState::Skipped | ProgressState::WouldRun | ProgressState::MayRun => {}
                }
                if let Some(target) = self.slots.get_mut(slot) {
                    target.desc = desc.clone();
                    target.status = state.as_str().into();
                    target.started = None;
                    target.duration_ms = duration_ms;
                    target.critical = critical;
                }
                self.push_log(format!(
                    "{} {id} ({duration_ms} ms): {desc}",
                    state.as_str().to_uppercase()
                ));
                if state == ProgressState::Failed {
                    self.push_log(format!("FAILED: {desc}"));
                    for line in detail.trim_end().lines() {
                        self.push_log(format!("  {line}"));
                    }
                }
            }
            ProgressEvent::BuildFinished {
                success,
                elapsed_ms,
            } => {
                self.finished = Some((success, elapsed_ms));
                self.push_log(format!(
                    "BUILD {} ({elapsed_ms} ms)",
                    if success { "SUCCEEDED" } else { "FAILED" }
                ));
            }
        }
    }

    fn push_log(&mut self, line: String) {
        self.logs.push(line);
        // Bound memory without changing the visible tail.
        if self.logs.len() > 12_000 {
            let removed = self.logs.len() - 10_000;
            self.logs.drain(..removed);
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(removed);
        }
    }

    fn scroll(&mut self, delta: isize) {
        if delta.is_positive() {
            self.scroll_from_bottom = self
                .scroll_from_bottom
                .saturating_add(delta as usize)
                .min(self.logs.len().saturating_sub(1));
        } else {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(delta.unsigned_abs());
        }
    }

    fn render(&self, stdout: &mut io::Stdout) -> io::Result<()> {
        let (width, height) = terminal::size().unwrap_or((100, 30));
        let width = width.max(20) as usize;
        let height = height.max(8) as usize;
        let elapsed_ms = self
            .finished
            .map(|(_, elapsed)| elapsed)
            .or_else(|| {
                self.started
                    .map(|started| started.elapsed().as_millis() as u64)
            })
            .unwrap_or(0);

        let mut lines = Vec::new();
        if let Some(failure) = &self.last_failure {
            lines.push(format!(
                "FAILED: {failure}  {}/{}  elapsed {}",
                self.completed,
                self.total,
                format_duration(elapsed_ms)
            ));
        } else {
            lines.push(format!(
                "frost build  {}/{}  elapsed {}  critical estimate {}",
                self.completed,
                self.total,
                format_duration(elapsed_ms),
                format_duration(self.critical_path_ms)
            ));
        }
        lines.push(format!(
            "cache  {} hit / {} miss    failures {}    jobs {}",
            self.cache_hits, self.cache_misses, self.failures, self.jobs
        ));
        let critical = if self.critical_path.is_empty() {
            "(none)".into()
        } else {
            self.critical_path.join(" -> ")
        };
        lines.push(format!("critical path: {critical}"));
        lines.push("slots".into());
        // Always reserve a header and at least one row for the log pane. Very
        // small pseudo-terminals still show representative worker activity;
        // normal terminals show every slot.
        let slot_rows = height.saturating_sub(lines.len() + 2).max(1);
        let visible_slots = if self.slots.len() > slot_rows {
            slot_rows.saturating_sub(1)
        } else {
            self.slots.len()
        };
        for (index, slot) in self.slots.iter().take(visible_slots).enumerate() {
            let elapsed = slot
                .started
                .map(|started| started.elapsed().as_millis() as u64)
                .unwrap_or(slot.duration_ms);
            let marker = if slot.critical { "*" } else { " " };
            let desc = if slot.desc.is_empty() {
                "idle"
            } else {
                &slot.desc
            };
            let status = if slot.status.is_empty() {
                "idle"
            } else {
                &slot.status
            };
            lines.push(format!(
                " {marker} {:>2} {:<14} {:>8}  {desc}",
                index + 1,
                status,
                format_duration(elapsed)
            ));
        }
        if self.slots.len() > visible_slots {
            lines.push(format!(
                "   … {} more job slots",
                self.slots.len() - visible_slots
            ));
        }
        lines.push("logs (Up/Down/PgUp/PgDn/Home/End to scroll)".into());
        let log_rows = height.saturating_sub(lines.len()).max(1);
        let end = self
            .logs
            .len()
            .saturating_sub(self.scroll_from_bottom.min(self.logs.len()));
        let start = end.saturating_sub(log_rows);
        lines.extend(self.logs[start..end].iter().cloned());

        queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
        for (row, line) in lines.into_iter().take(height).enumerate() {
            queue!(stdout, MoveTo(0, row as u16))?;
            write!(stdout, "{}", truncate(&line, width))?;
        }
        stdout.flush()
    }
}

fn handle_input(state: &mut TuiState) {
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            frostbuild_exec::request_cancellation();
            continue;
        }
        match key.code {
            KeyCode::Up => state.scroll(1),
            KeyCode::Down => state.scroll(-1),
            KeyCode::PageUp => state.scroll(10),
            KeyCode::PageDown => state.scroll(-10),
            KeyCode::Home => state.scroll_from_bottom = state.logs.len().saturating_sub(1),
            KeyCode::End => state.scroll_from_bottom = 0,
            _ => {}
        }
    }
}

fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1_000.0)
    }
}

fn truncate(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let mut result = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 1 {
        result.pop();
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_pane_scrolls_and_returns_to_live_tail() {
        let mut state = TuiState::default();
        for index in 0..20 {
            state.push_log(format!("line {index}"));
        }
        state.scroll(5);
        assert_eq!(state.scroll_from_bottom, 5);
        state.scroll(-2);
        assert_eq!(state.scroll_from_bottom, 3);
        state.scroll_from_bottom = 0;
        assert_eq!(state.scroll_from_bottom, 0);
    }

    #[test]
    fn renderer_overhead_gate_stays_below_20us_per_action() {
        let mut state = TuiState::default();
        state.apply(ProgressEvent::BuildStarted {
            total: 50_000,
            jobs: 16,
            critical_path_ms: 500,
            critical_path: vec!["link".into()],
        });
        let started = Instant::now();
        for completed in 1..=50_000 {
            let slot = completed % 16;
            state.apply(ProgressEvent::ActionStarted {
                slot,
                id: format!("a{completed}"),
                desc: "compile".into(),
                command: "cc -c source.c".into(),
                critical: false,
            });
            state.apply(ProgressEvent::ActionFinished {
                slot,
                completed,
                total: 50_000,
                id: format!("a{completed}"),
                desc: "compile".into(),
                state: ProgressState::CacheHit,
                duration_ms: 0,
                detail: String::new(),
                critical: false,
            });
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "progress model took {elapsed:?} for 50k actions"
        );
    }

    #[test]
    fn all_cached_fast_path_is_one_aggregate_event() {
        let mut state = TuiState::default();
        state.apply(ProgressEvent::BuildStarted {
            total: 50_000,
            jobs: 16,
            critical_path_ms: 0,
            critical_path: Vec::new(),
        });
        state.apply(ProgressEvent::AllCached { total: 50_000 });
        assert_eq!(state.completed, 50_000);
        assert_eq!(state.cache_hits, 50_000);
        assert_eq!(state.logs.len(), 1);
    }
}
