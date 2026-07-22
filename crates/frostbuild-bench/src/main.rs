use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::thread;
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use frostbuild_core::cas::{CasStats, LocalCas, CHUNKING_THRESHOLD};
use frostbuild_core::hashcache::hash_file;
use serde::Serialize;

const MIB: u64 = 1024 * 1024;
const GENERATOR_SEED: u64 = 0x4d59_5df4_d0f3_3173;

#[derive(Debug, Parser)]
#[command(
    name = "frost-bench-rs",
    about = "Low-level, reproducible FrostBuild measurements"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Measure verified whole-blob, FastCDC chunk and DeltaCDC CAS paths.
    Cas(CasArgs),
    /// Compare standalone CLI, daemon CLI and daemon socket warm no-op latency.
    DaemonNoop(DaemonNoopArgs),
}

#[derive(Debug, Args)]
struct CasArgs {
    /// Deterministic fixture size in MiB (must exceed the chunk threshold).
    #[arg(long, default_value_t = 64)]
    size_mib: u64,

    /// Samples per path; reports the median and every raw sample.
    #[arg(long, default_value_t = 7)]
    iterations: usize,

    /// Emit machine-readable JSON instead of the compact AA report.
    #[arg(long)]
    json: bool,

    /// Also write the complete JSON report to this path.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct DaemonNoopArgs {
    /// Release frost executable to measure.
    #[arg(long, default_value = "frost")]
    frost: PathBuf,

    /// Samples per path, rotated so no path always runs first.
    #[arg(long, default_value_t = 31)]
    iterations: usize,

    /// Emit machine-readable JSON instead of the compact AA report.
    #[arg(long)]
    json: bool,

    /// Also write the complete JSON report to this path.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Environment {
    os: &'static str,
    arch: &'static str,
    cpu_count: usize,
    rayon_num_threads_env: Option<String>,
    load_average: Option<[f64; 3]>,
    cpu_governor: Option<String>,
    turbo_disabled: Option<bool>,
    temp_root: PathBuf,
}

#[derive(Debug, Serialize)]
struct Measurement {
    median_ms: f64,
    throughput_mib_s: f64,
    samples_ms: Vec<f64>,
}

#[derive(Debug, Serialize)]
struct LatencyMeasurement {
    median_ms: f64,
    min_ms: f64,
    max_ms: f64,
    samples_ms: Vec<f64>,
}

impl LatencyMeasurement {
    fn new(samples_ms: Vec<f64>) -> Self {
        let mut sorted = samples_ms.clone();
        sorted.sort_by(f64::total_cmp);
        Self {
            median_ms: sorted[sorted.len() / 2],
            min_ms: sorted[0],
            max_ms: *sorted.last().unwrap(),
            samples_ms,
        }
    }
}

impl Measurement {
    fn new(samples_ms: Vec<f64>, size_bytes: u64) -> Self {
        let mut sorted = samples_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let median_ms = sorted[sorted.len() / 2];
        let throughput_mib_s = size_bytes as f64 / MIB as f64 / (median_ms / 1000.0);
        Self {
            median_ms,
            throughput_mib_s,
            samples_ms,
        }
    }
}

#[derive(Debug, Serialize)]
struct CasReport {
    schema: &'static str,
    frostbuild_version: &'static str,
    generated_at_unix_s: u64,
    environment: Environment,
    size_bytes: u64,
    iterations: usize,
    generator_seed: u64,
    chunking_threshold_bytes: u64,
    initial_hash: Measurement,
    serial_cold_put: Measurement,
    cold_put: Measurement,
    cold_put_parallel_speedup: f64,
    exact_materialize: Measurement,
    serial_chunk_materialize: Measurement,
    chunk_materialize: Measurement,
    chunk_materialize_parallel_speedup: f64,
    update_hash: Measurement,
    delta_put: Measurement,
    serial_delta_materialize: Measurement,
    delta_materialize: Measurement,
    delta_materialize_parallel_speedup: f64,
    stats: CasStats,
}

#[derive(Debug, Serialize)]
struct DaemonNoopReport {
    schema: &'static str,
    frostbuild_version: &'static str,
    generated_at_unix_s: u64,
    environment_before: Environment,
    environment_after: Environment,
    frost_executable: PathBuf,
    frost_version: String,
    iterations: usize,
    workspace_contract: &'static str,
    standalone_cli: LatencyMeasurement,
    daemon_cli: LatencyMeasurement,
    daemon_socket_roundtrip: LatencyMeasurement,
    daemon_cli_speedup: f64,
    daemon_socket_speedup: f64,
    daemon_server_sub_5ms: bool,
    end_to_end_sub_5ms: bool,
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("frost-cas-bench-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

fn environment() -> Environment {
    let load_average = read_trimmed("/proc/loadavg").and_then(|value| {
        let values = value
            .split_whitespace()
            .take(3)
            .map(str::parse::<f64>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?;
        (values.len() == 3).then(|| [values[0], values[1], values[2]])
    });
    Environment {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpu_count: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        rayon_num_threads_env: std::env::var("RAYON_NUM_THREADS").ok(),
        load_average,
        cpu_governor: read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        turbo_disabled: read_trimmed("/sys/devices/system/cpu/intel_pstate/no_turbo").and_then(
            |value| match value.as_str() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            },
        ),
        temp_root: std::env::temp_dir(),
    }
}

fn resolve_program(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() || path.components().count() > 1 {
        return std::fs::canonicalize(path)
            .with_context(|| format!("executable {} was not found", path.display()));
    }
    let search = std::env::var_os("PATH").context("PATH is not set")?;
    std::env::split_paths(&search)
        .map(|directory| directory.join(path))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("executable {} was not found on PATH", path.display()))
}

fn frost_command(frost: &Path, root: &Path, args: &[&str]) -> Result<std::process::Output> {
    ProcessCommand::new(frost)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("failed to start {}", frost.display()))
}

fn checked_frost(frost: &Path, root: &Path, args: &[&str]) -> Result<String> {
    let output = frost_command(frost, root, args)?;
    anyhow::ensure!(
        output.status.success(),
        "frost {} failed:\n{}{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct DaemonThread {
    root: PathBuf,
    handle: Option<thread::JoinHandle<Result<()>>>,
}

impl DaemonThread {
    fn start(root: &Path) -> Result<Self> {
        let server_root = root.to_path_buf();
        let handle = thread::spawn(move || frostbuild_daemon::serve(&server_root));
        for _ in 0..100 {
            if frostbuild_daemon::request(
                root,
                &frostbuild_daemon::Request::Status {
                    version: frostbuild_daemon::PROTOCOL_VERSION,
                },
            )
            .is_ok_and(|response| response.code == 0)
            {
                return Ok(Self {
                    root: root.to_path_buf(),
                    handle: Some(handle),
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = frostbuild_daemon::request(
            root,
            &frostbuild_daemon::Request::Shutdown {
                version: frostbuild_daemon::PROTOCOL_VERSION,
            },
        );
        let _ = handle.join();
        anyhow::bail!("frostd did not become ready")
    }
}

impl Drop for DaemonThread {
    fn drop(&mut self) {
        let _ = frostbuild_daemon::request(
            &self.root,
            &frostbuild_daemon::Request::Shutdown {
                version: frostbuild_daemon::PROTOCOL_VERSION,
            },
        );
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn measure_frost(frost: &Path, root: &Path, daemon: bool) -> Result<f64> {
    let mut args = vec!["build"];
    if daemon {
        args.push("--daemon");
    }
    args.push("--no-tui");
    let started = Instant::now();
    let stdout = checked_frost(frost, root, &args)?;
    anyhow::ensure!(
        stdout.contains("up to date"),
        "unexpected no-op output: {stdout}"
    );
    Ok(elapsed_ms(started))
}

fn measure_daemon_socket(root: &Path) -> Result<f64> {
    let request = frostbuild_daemon::Request::Run {
        version: frostbuild_daemon::PROTOCOL_VERSION,
        program: root.join("deliberately-missing-frost"),
        args: Vec::new(),
        fast_noop: Some(frostbuild_daemon::FastNoopRequest {
            profile: "debug".into(),
            platform: frostbuild_core::manifest::HOST_PLATFORM.into(),
            key_env: frostbuild_exec::key_environment_snapshot(),
        }),
    };
    let started = Instant::now();
    let response = frostbuild_daemon::request(root, &request)?;
    anyhow::ensure!(
        response.code == 0 && response.stdout.contains("up to date"),
        "daemon certificate path missed: {}{}",
        response.stdout,
        response.stderr
    );
    Ok(elapsed_ms(started))
}

fn run_daemon_noop(args: DaemonNoopArgs) -> Result<DaemonNoopReport> {
    anyhow::ensure!(
        args.iterations > 0,
        "--iterations must be greater than zero"
    );
    let frost = resolve_program(&args.frost)?;
    let version = ProcessCommand::new(&frost)
        .arg("--version")
        .output()
        .context("failed to read frost version")?;
    anyhow::ensure!(version.status.success(), "frost --version failed");
    let frost_version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    let environment_before = environment();
    let scratch = Scratch::new()?;
    let root = scratch.path.join("daemon-workspace");
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("src/main.c"),
        "#include <stdio.h>\nint main(void) { puts(\"daemon-ok\"); return 0; }\n",
    )?;
    std::fs::write(
        root.join("frost.toml"),
        "[workspace]\ndefault_targets = [\"app\"]\n\n\
         [target.app]\nkind = \"cc_binary\"\nsrcs = [\"src/main.c\"]\n",
    )?;
    checked_frost(&frost, &root, &["build", "--no-tui"])?;
    let _daemon = DaemonThread::start(&root)?;

    // Warm process pages, the certificate and the daemon's Rayon pool before
    // measurement. The nonexistent fallback in the direct request proves the
    // certificate was accepted inside the server.
    measure_frost(&frost, &root, false)?;
    measure_frost(&frost, &root, true)?;
    measure_daemon_socket(&root)?;

    let mut standalone = Vec::with_capacity(args.iterations);
    let mut daemon_cli = Vec::with_capacity(args.iterations);
    let mut daemon_socket = Vec::with_capacity(args.iterations);
    for iteration in 0..args.iterations {
        let order = match iteration % 3 {
            0 => [0, 1, 2],
            1 => [1, 2, 0],
            _ => [2, 0, 1],
        };
        for path in order {
            match path {
                0 => standalone.push(measure_frost(&frost, &root, false)?),
                1 => daemon_cli.push(measure_frost(&frost, &root, true)?),
                _ => daemon_socket.push(measure_daemon_socket(&root)?),
            }
        }
    }
    let standalone_cli = LatencyMeasurement::new(standalone);
    let daemon_cli = LatencyMeasurement::new(daemon_cli);
    let daemon_socket_roundtrip = LatencyMeasurement::new(daemon_socket);
    Ok(DaemonNoopReport {
        schema: "frost-daemon-noop-v1",
        frostbuild_version: env!("CARGO_PKG_VERSION"),
        generated_at_unix_s: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        environment_before,
        environment_after: environment(),
        frost_executable: frost,
        frost_version,
        iterations: args.iterations,
        workspace_contract: "one native C compile/link target; warm default-target certificate",
        daemon_cli_speedup: standalone_cli.median_ms / daemon_cli.median_ms,
        daemon_socket_speedup: standalone_cli.median_ms / daemon_socket_roundtrip.median_ms,
        daemon_server_sub_5ms: daemon_socket_roundtrip.median_ms < 5.0,
        end_to_end_sub_5ms: daemon_cli.median_ms < 5.0,
        standalone_cli,
        daemon_cli,
        daemon_socket_roundtrip,
    })
}

fn write_fixture(path: &Path, size_bytes: u64) -> Result<()> {
    let mut output = File::create(path)?;
    let mut state = GENERATOR_SEED;
    let mut remaining = size_bytes;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let length = remaining.min(buffer.len() as u64) as usize;
        for byte in &mut buffer[..length] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        output.write_all(&buffer[..length])?;
        remaining -= length as u64;
    }
    output.flush()?;
    Ok(())
}

fn mutate_byte(path: &Path, offset: u64) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    byte[0] ^= 1;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&byte)?;
    file.flush()?;
    Ok(())
}

fn object_path(root: &Path, digest: &str) -> PathBuf {
    root.join(".frost/cas/objects")
        .join(&digest[..2])
        .join(digest)
}

fn store_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    if !root.is_dir() {
        return Ok(files);
    }
    for shard in std::fs::read_dir(root)? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(shard.path())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.insert(entry.path());
            }
        }
    }
    Ok(files)
}

fn run_cas(args: CasArgs) -> Result<CasReport> {
    anyhow::ensure!(
        args.iterations > 0,
        "--iterations must be greater than zero"
    );
    let size_bytes = args
        .size_mib
        .checked_mul(MIB)
        .context("--size-mib is too large")?;
    anyhow::ensure!(
        size_bytes > CHUNKING_THRESHOLD,
        "--size-mib must produce more than {} bytes to exercise FastCDC",
        CHUNKING_THRESHOLD
    );

    // Capture host load before fixture generation and measured I/O begin.
    let environment = environment();
    let scratch = Scratch::new()?;
    let source = scratch.path.join("large-output.bin");
    let destination = scratch.path.join("materialized.bin");
    write_fixture(&source, size_bytes)?;

    let mut hash_samples = Vec::with_capacity(args.iterations);
    let mut initial_digest = String::new();
    for _ in 0..args.iterations {
        let start = Instant::now();
        initial_digest = hash_file(&source)?;
        hash_samples.push(elapsed_ms(start));
    }

    let mut serial_cold_put_samples = Vec::with_capacity(args.iterations);
    let mut cold_put_samples = Vec::with_capacity(args.iterations);
    for iteration in 0..args.iterations {
        // Reverse order every sample so neither mode systematically owns a
        // warmer page cache or cooler CPU.
        for parallel in if iteration % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let _ = std::fs::remove_dir_all(scratch.path.join(".frost"));
            let cas = LocalCas::new(&scratch.path, u64::MAX).with_parallel_chunks(parallel);
            let start = Instant::now();
            cas.put(&source, &initial_digest)?;
            if parallel {
                cold_put_samples.push(elapsed_ms(start));
            } else {
                serial_cold_put_samples.push(elapsed_ms(start));
            }
        }
    }

    let _ = std::fs::remove_dir_all(scratch.path.join(".frost"));
    let cas = LocalCas::new(&scratch.path, u64::MAX);
    let serial_cas = LocalCas::new(&scratch.path, u64::MAX).with_parallel_chunks(false);
    cas.put(&source, &initial_digest)?;

    let mut exact_samples = Vec::with_capacity(args.iterations);
    for _ in 0..args.iterations {
        let start = Instant::now();
        anyhow::ensure!(cas.materialize(&initial_digest, &destination)?);
        exact_samples.push(elapsed_ms(start));
        anyhow::ensure!(hash_file(&destination)? == initial_digest);
        std::fs::remove_file(&destination)?;
    }

    std::fs::remove_file(object_path(&scratch.path, &initial_digest))?;
    let mut serial_chunk_samples = Vec::with_capacity(args.iterations);
    let mut chunk_samples = Vec::with_capacity(args.iterations);
    for iteration in 0..args.iterations {
        for parallel in if iteration % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let selected = if parallel { &cas } else { &serial_cas };
            let start = Instant::now();
            anyhow::ensure!(selected.materialize(&initial_digest, &destination)?);
            if parallel {
                chunk_samples.push(elapsed_ms(start));
            } else {
                serial_chunk_samples.push(elapsed_ms(start));
            }
            anyhow::ensure!(hash_file(&destination)? == initial_digest);
            std::fs::remove_file(&destination)?;
        }
    }

    let chunks_root = scratch.path.join(".frost/cas/chunks");
    let mut update_hash_samples = Vec::with_capacity(args.iterations);
    let mut delta_put_samples = Vec::with_capacity(args.iterations);
    let mut serial_delta_materialize_samples = Vec::with_capacity(args.iterations);
    let mut delta_materialize_samples = Vec::with_capacity(args.iterations);
    for iteration in 0..args.iterations {
        let offset = (size_bytes / 2 + iteration as u64 * 4096) % size_bytes;
        mutate_byte(&source, offset)?;

        let start = Instant::now();
        let digest = hash_file(&source)?;
        update_hash_samples.push(elapsed_ms(start));

        let before = store_files(&chunks_root)?;
        let start = Instant::now();
        cas.put(&source, &digest)?;
        delta_put_samples.push(elapsed_ms(start));
        let after = store_files(&chunks_root)?;
        let new_chunks = after.difference(&before).cloned().collect::<Vec<_>>();
        anyhow::ensure!(
            !new_chunks.is_empty(),
            "one-byte mutation unexpectedly produced no new chunk"
        );

        std::fs::remove_file(object_path(&scratch.path, &digest))?;
        for parallel in if iteration % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            for chunk in &new_chunks {
                let _ = std::fs::remove_file(chunk);
            }
            let selected = if parallel { &cas } else { &serial_cas };
            let start = Instant::now();
            anyhow::ensure!(selected.materialize(&digest, &destination)?);
            if parallel {
                delta_materialize_samples.push(elapsed_ms(start));
            } else {
                serial_delta_materialize_samples.push(elapsed_ms(start));
            }
            anyhow::ensure!(hash_file(&destination)? == digest);
            std::fs::remove_file(&destination)?;
        }
    }

    let stats = cas.stats()?;
    anyhow::ensure!(stats.delta_count > 0, "benchmark did not exercise DeltaCDC");
    let mut report = CasReport {
        schema: "frost-cas-benchmark-v3",
        frostbuild_version: env!("CARGO_PKG_VERSION"),
        generated_at_unix_s: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        environment,
        size_bytes,
        iterations: args.iterations,
        generator_seed: GENERATOR_SEED,
        chunking_threshold_bytes: CHUNKING_THRESHOLD,
        initial_hash: Measurement::new(hash_samples, size_bytes),
        serial_cold_put: Measurement::new(serial_cold_put_samples, size_bytes),
        cold_put: Measurement::new(cold_put_samples, size_bytes),
        cold_put_parallel_speedup: 0.0,
        exact_materialize: Measurement::new(exact_samples, size_bytes),
        serial_chunk_materialize: Measurement::new(serial_chunk_samples, size_bytes),
        chunk_materialize: Measurement::new(chunk_samples, size_bytes),
        chunk_materialize_parallel_speedup: 0.0,
        update_hash: Measurement::new(update_hash_samples, size_bytes),
        delta_put: Measurement::new(delta_put_samples, size_bytes),
        serial_delta_materialize: Measurement::new(serial_delta_materialize_samples, size_bytes),
        delta_materialize: Measurement::new(delta_materialize_samples, size_bytes),
        delta_materialize_parallel_speedup: 0.0,
        stats,
    };
    report.cold_put_parallel_speedup = report.serial_cold_put.median_ms / report.cold_put.median_ms;
    report.chunk_materialize_parallel_speedup =
        report.serial_chunk_materialize.median_ms / report.chunk_materialize.median_ms;
    report.delta_materialize_parallel_speedup =
        report.serial_delta_materialize.median_ms / report.delta_materialize.median_ms;
    Ok(report)
}

fn print_cas(report: &CasReport) {
    println!("Frost CAS benchmark · {} MiB", report.size_bytes / MIB);
    println!("|");
    println!(
        "+-- hash output ........ {:>8.3} ms  {:>8.1} MiB/s",
        report.initial_hash.median_ms, report.initial_hash.throughput_mib_s
    );
    println!(
        "+-- cold publish serial  {:>8.3} ms  {:>8.1} MiB/s",
        report.serial_cold_put.median_ms, report.serial_cold_put.throughput_mib_s
    );
    println!(
        "+-- cold publish -j auto {:>8.3} ms  {:>8.1} MiB/s  {:>4.2}x",
        report.cold_put.median_ms,
        report.cold_put.throughput_mib_s,
        report.cold_put_parallel_speedup
    );
    println!(
        "+-- exact restore ...... {:>8.3} ms  {:>8.1} MiB/s",
        report.exact_materialize.median_ms, report.exact_materialize.throughput_mib_s
    );
    println!(
        "+-- chunk restore serial {:>8.3} ms  {:>8.1} MiB/s",
        report.serial_chunk_materialize.median_ms, report.serial_chunk_materialize.throughput_mib_s
    );
    println!(
        "+-- chunk restore -j auto{:>8.3} ms  {:>8.1} MiB/s  {:>4.2}x",
        report.chunk_materialize.median_ms,
        report.chunk_materialize.throughput_mib_s,
        report.chunk_materialize_parallel_speedup
    );
    println!(
        "+-- one-byte delta put . {:>8.3} ms  {:>8.1} MiB/s",
        report.delta_put.median_ms, report.delta_put.throughput_mib_s
    );
    println!(
        "+-- delta restore serial {:>8.3} ms  {:>8.1} MiB/s",
        report.serial_delta_materialize.median_ms, report.serial_delta_materialize.throughput_mib_s
    );
    println!(
        "`-- delta restore -j auto{:>8.3} ms  {:>8.1} MiB/s  {:>4.2}x",
        report.delta_materialize.median_ms,
        report.delta_materialize.throughput_mib_s,
        report.delta_materialize_parallel_speedup
    );
    println!(
        "    reuse {:>5.1}% · {} chunks · {} deltas · median of {}",
        report.stats.chunk_reuse_ratio * 100.0,
        report.stats.chunk_count,
        report.stats.delta_count,
        report.iterations
    );
}

fn print_daemon_noop(report: &DaemonNoopReport) {
    println!("Frost warm no-op · median of {}", report.iterations);
    println!("|");
    println!(
        "+-- standalone CLI .... {:>8.3} ms",
        report.standalone_cli.median_ms
    );
    println!(
        "+-- daemon CLI ........ {:>8.3} ms  {:>4.2}x",
        report.daemon_cli.median_ms, report.daemon_cli_speedup
    );
    println!(
        "`-- daemon socket ..... {:>8.3} ms  {:>4.2}x",
        report.daemon_socket_roundtrip.median_ms, report.daemon_socket_speedup
    );
    println!(
        "    server <5 ms {} · end-to-end <5 ms {}",
        if report.daemon_server_sub_5ms {
            "PASS"
        } else {
            "FAIL"
        },
        if report.end_to_end_sub_5ms {
            "PASS"
        } else {
            "FAIL"
        }
    );
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Cas(args)) => {
            let json = args.json;
            let out = args.out.clone();
            let report = run_cas(args)?;
            let encoded = serde_json::to_string_pretty(&report)?;
            if let Some(path) = &out {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, format!("{encoded}\n"))?;
            }
            if json {
                println!("{encoded}");
            } else {
                print_cas(&report);
                if let Some(path) = out {
                    println!("    report {}", path.display());
                }
            }
        }
        Some(Command::DaemonNoop(args)) => {
            let json = args.json;
            let out = args.out.clone();
            let report = run_daemon_noop(args)?;
            let encoded = serde_json::to_string_pretty(&report)?;
            if let Some(path) = &out {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, format!("{encoded}\n"))?;
            }
            if json {
                println!("{encoded}");
            } else {
                print_daemon_noop(&report);
                if let Some(path) = out {
                    println!("    report {}", path.display());
                }
            }
        }
        None => {
            println!("Use ./frost-bench for frontend comparisons, or run:");
            println!(
                "  cargo run --release -p frostbuild-bench --bin frost-bench-rs -- \
                 <cas|daemon-noop> --help"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_uses_middle_sample_and_byte_throughput() {
        let measurement = Measurement::new(vec![300.0, 100.0, 200.0], 10 * MIB);
        assert_eq!(measurement.median_ms, 200.0);
        assert_eq!(measurement.throughput_mib_s, 50.0);
        assert_eq!(measurement.samples_ms, vec![300.0, 100.0, 200.0]);
    }

    #[test]
    fn latency_measurement_keeps_raw_samples_and_extrema() {
        let measurement = LatencyMeasurement::new(vec![3.0, 1.0, 2.0]);
        assert_eq!(measurement.median_ms, 2.0);
        assert_eq!(measurement.min_ms, 1.0);
        assert_eq!(measurement.max_ms, 3.0);
        assert_eq!(measurement.samples_ms, vec![3.0, 1.0, 2.0]);
    }
}
