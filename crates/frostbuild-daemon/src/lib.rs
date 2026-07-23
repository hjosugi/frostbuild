use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
#[cfg(windows)]
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

struct DaemonState {
    dirty: BTreeSet<PathBuf>,
    watcher_trusted: bool,
    event_epoch: u64,
    next_barrier: u64,
    barrier_seen: u64,
    cached_noop: Option<CachedNoop>,
}

struct CachedNoop {
    profile: String,
    platform: String,
    key_env: BTreeMap<String, String>,
    proof: frostbuild_exec::FastNoopWatchProof,
}

impl CachedNoop {
    fn matches(&self, request: &FastNoopRequest) -> bool {
        self.profile == request.profile
            && self.platform == request.platform
            && self.key_env == request.key_env
    }
}

struct DaemonShared {
    state: Mutex<DaemonState>,
    events: Condvar,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status {
        version: u32,
    },
    Run {
        version: u32,
        program: PathBuf,
        args: Vec<String>,
        /// Optional proof-only path for a plain default-target build. Older
        /// clients omit it; older daemons ignore it and retain the child path.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fast_noop: Option<FastNoopRequest>,
    },
    Shutdown {
        version: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FastNoopRequest {
    pub profile: String,
    pub platform: String,
    /// Captured by the invoking client, because a long-lived daemon may have
    /// inherited different output-affecting environment values when started.
    pub key_env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Socket for a workspace's daemon.
///
/// Not inside the workspace: a Unix socket address is capped at roughly 100
/// bytes, so `<workspace>/.frost/frostd.sock` fails outright once the
/// workspace sits a few directories deep — with `SUN_LEN` in the message and
/// no hint that the path is the problem. The address is instead a short,
/// stable name in the user's runtime directory, derived from the workspace
/// path so that each workspace still gets its own daemon.
#[cfg(unix)]
pub fn socket_path(root: &Path) -> PathBuf {
    let key = blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex();
    runtime_dir().join(format!("frostd-{}.sock", &key[..16]))
}

#[cfg(unix)]
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return dir;
        }
    }
    std::env::temp_dir()
}

#[cfg(windows)]
fn endpoint_path(root: &Path) -> PathBuf {
    root.join(".frost").join("frostd.endpoint")
}

#[cfg(unix)]
fn connect(root: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket_path(root)).context("frostd is not running")
}

#[cfg(windows)]
fn connect(root: &Path) -> Result<TcpStream> {
    let endpoint = std::fs::read_to_string(endpoint_path(root))
        .context("frostd is not running (endpoint is missing)")?;
    TcpStream::connect(endpoint.trim()).context("frostd is not running")
}

pub fn request(root: &Path, request: &Request) -> Result<Response> {
    let mut stream = connect(root)?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

pub fn serve(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join(".frost"))?;

    #[cfg(unix)]
    let (listener, cleanup_path) = {
        let socket = socket_path(root);
        // A stale socket from a daemon that was killed rather than shut down
        // would otherwise make bind fail until someone deletes it by hand.
        if UnixStream::connect(&socket).is_ok() {
            anyhow::bail!("frostd is already running for {}", root.display());
        }
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("failed to bind {}", socket.display()))?;
        (listener, socket)
    };

    #[cfg(windows)]
    let (listener, cleanup_path) = {
        if connect(root).is_ok() {
            anyhow::bail!("frostd is already running for {}", root.display());
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("failed to bind frostd loopback endpoint")?;
        let address = listener.local_addr()?.to_string();
        let endpoint = endpoint_path(root);
        publish_windows_endpoint(&endpoint, &address)?;
        (listener, endpoint)
    };

    let state = Arc::new(DaemonShared {
        state: Mutex::new(DaemonState {
            dirty: BTreeSet::new(),
            watcher_trusted: true,
            event_epoch: 0,
            next_barrier: 0,
            barrier_seen: 0,
            cached_noop: None,
        }),
        events: Condvar::new(),
    });
    let event_state = Arc::clone(&state);
    let root_owned = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            let mut state = event_state.state.lock().unwrap();
            state.watcher_trusted = false;
            state.cached_noop = None;
            state.event_epoch = state.event_epoch.wrapping_add(1);
            event_state.events.notify_all();
            return;
        };
        if !matches!(
            event.kind,
            notify::EventKind::Any
                | notify::EventKind::Create(_)
                | notify::EventKind::Modify(_)
                | notify::EventKind::Remove(_)
        ) {
            return;
        }
        let mut state = event_state.state.lock().unwrap();
        for path in event.paths {
            let relative = path.strip_prefix(&root_owned).unwrap_or(&path);
            if let Some(barrier) = watcher_barrier_id(relative) {
                state.barrier_seen = state.barrier_seen.max(barrier);
                event_state.events.notify_all();
                continue;
            }
            if relative.starts_with(".git") {
                continue;
            }
            state.dirty.insert(relative.to_path_buf());
            state.cached_noop = None;
            state.event_epoch = state.event_epoch.wrapping_add(1);
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    for connection in listener.incoming() {
        let mut stream = connection?;
        let request: Request = match read_frame(&mut stream) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let (response, shutdown) = handle(root, request, &state);
        write_frame(&mut stream, &response)?;
        if shutdown {
            break;
        }
    }
    let _ = std::fs::remove_file(cleanup_path);
    Ok(())
}

/// Publish an ephemeral loopback address without allowing two concurrently
/// starting daemons to silently overwrite one another. A dead daemon can
/// leave a stale endpoint behind; compare its contents again immediately
/// before removal so a new winner is not deleted by a delayed contender.
#[cfg(windows)]
fn publish_windows_endpoint(path: &Path, address: &str) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::ErrorKind;

    for _ in 0..4 {
        if let Ok(existing) = std::fs::read_to_string(path) {
            if !existing.trim().is_empty() && TcpStream::connect(existing.trim()).is_ok() {
                anyhow::bail!("frostd is already running");
            }
            if std::fs::read_to_string(path).is_ok_and(|current| current == existing) {
                let _ = std::fs::remove_file(path);
            }
        }

        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                file.write_all(address.as_bytes())?;
                file.sync_all()?;
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                std::thread::yield_now();
            }
            Err(error) => return Err(error).context("failed to publish frostd endpoint"),
        }
    }
    anyhow::bail!("another frostd instance is starting")
}

const WATCHER_BARRIER_PREFIX: &str = ".frostd-barrier-";

fn watcher_barrier_id(relative: &Path) -> Option<u64> {
    if relative.parent() != Some(Path::new(".frost")) {
        return None;
    }
    relative
        .file_name()?
        .to_str()?
        .strip_prefix(WATCHER_BARRIER_PREFIX)?
        .parse()
        .ok()
}

/// Flush events queued before the marker through the watcher callback. A
/// timeout or backend error permanently disables watcher-backed no-op hits;
/// the ordinary certificate path remains available.
fn watcher_barrier(root: &Path, shared: &DaemonShared) -> bool {
    let id = {
        let mut state = shared.state.lock().unwrap();
        if !state.watcher_trusted {
            return false;
        }
        state.next_barrier = state.next_barrier.wrapping_add(1).max(1);
        state.next_barrier
    };
    let path = root
        .join(".frost")
        .join(format!("{WATCHER_BARRIER_PREFIX}{id}"));
    if std::fs::write(&path, id.to_string()).is_err() {
        let mut state = shared.state.lock().unwrap();
        state.watcher_trusted = false;
        state.cached_noop = None;
        return false;
    }

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut state = shared.state.lock().unwrap();
    while state.watcher_trusted && state.barrier_seen < id {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let (next, timeout) = shared.events.wait_timeout(state, remaining).unwrap();
        state = next;
        if timeout.timed_out() {
            break;
        }
    }
    let observed = state.watcher_trusted && state.barrier_seen >= id;
    if !observed {
        state.watcher_trusted = false;
        state.cached_noop = None;
    }
    drop(state);
    let _ = std::fs::remove_file(path);
    observed
}

fn noop_response(version: u32, hit: frostbuild_exec::FastNoopHit, started: Instant) -> Response {
    let scope = if hit.closure_actions < hit.graph_actions {
        format!("{} of {} actions", hit.closure_actions, hit.graph_actions)
    } else {
        format!("{} actions", hit.closure_actions)
    };
    Response {
        version,
        code: 0,
        stdout: format!(
            "frost: up to date · {scope} · {} ms\n",
            started.elapsed().as_millis()
        ),
        stderr: String::new(),
    }
}

fn try_cached_noop(
    root: &Path,
    request: &FastNoopRequest,
    shared: &DaemonShared,
) -> Result<Option<frostbuild_exec::FastNoopHit>> {
    let proof = {
        let state = shared.state.lock().unwrap();
        if !state.watcher_trusted || !state.dirty.is_empty() {
            return Ok(None);
        }
        state
            .cached_noop
            .as_ref()
            .filter(|cached| cached.matches(request))
            .map(|cached| cached.proof.clone())
    };
    let Some(proof) = proof else {
        return Ok(None);
    };
    let hit = frostbuild_exec::try_fast_noop_from_watch_proof(
        root,
        &request.profile,
        &request.platform,
        &request.key_env,
        &proof,
    )?;
    let Some(hit) = hit else {
        shared.state.lock().unwrap().cached_noop = None;
        return Ok(None);
    };
    if !watcher_barrier(root, shared) {
        return Ok(None);
    }
    let state = shared.state.lock().unwrap();
    Ok(
        (state.watcher_trusted && state.dirty.is_empty() && state.cached_noop.is_some())
            .then_some(hit),
    )
}

fn try_full_noop(
    root: &Path,
    request: &FastNoopRequest,
    shared: &DaemonShared,
) -> Result<Option<frostbuild_exec::FastNoopHit>> {
    let barrier_ready = watcher_barrier(root, shared);
    let baseline_epoch = {
        let mut state = shared.state.lock().unwrap();
        if barrier_ready && state.watcher_trusted {
            state.dirty.clear();
        }
        state.event_epoch
    };
    let validated = frostbuild_exec::try_fast_noop_for_daemon(
        root,
        &request.profile,
        &request.platform,
        &request.key_env,
    )?;
    let Some(validated) = validated else {
        return Ok(None);
    };

    let stable = barrier_ready && watcher_barrier(root, shared);
    let mut state = shared.state.lock().unwrap();
    let unchanged = stable && state.watcher_trusted && state.event_epoch == baseline_epoch;
    if unchanged {
        state.dirty.clear();
        state.cached_noop = validated.watch_proof.map(|proof| CachedNoop {
            profile: request.profile.clone(),
            platform: request.platform.clone(),
            key_env: request.key_env.clone(),
            proof,
        });
    } else {
        state.cached_noop = None;
    }
    // If a trusted watcher observed a change during validation, the identity
    // scan may have passed that path before it changed. Reject this response
    // and let the child build settle the workspace. When no watcher barrier
    // was available at the start, preserve the ordinary certificate path.
    Ok((!barrier_ready || unchanged).then_some(validated.hit))
}

fn handle(root: &Path, request: Request, shared: &DaemonShared) -> (Response, bool) {
    let version = match &request {
        Request::Status { version }
        | Request::Run { version, .. }
        | Request::Shutdown { version } => *version,
    };
    if version != PROTOCOL_VERSION {
        return (
            Response {
                version: PROTOCOL_VERSION,
                code: 2,
                stdout: String::new(),
                stderr: "protocol version mismatch".into(),
            },
            false,
        );
    }
    match request {
        Request::Status { .. } => {
            let count = shared.state.lock().unwrap().dirty.len();
            (
                Response {
                    version,
                    code: 0,
                    stdout: format!("running ({count} dirty paths)"),
                    stderr: String::new(),
                },
                false,
            )
        }
        Request::Shutdown { .. } => (
            Response {
                version,
                code: 0,
                stdout: "stopped".into(),
                stderr: String::new(),
            },
            true,
        ),
        Request::Run {
            program,
            args,
            fast_noop,
            ..
        } => {
            if let Some(fast_noop) = fast_noop {
                let started = Instant::now();
                match try_cached_noop(root, &fast_noop, shared).and_then(|hit| match hit {
                    Some(hit) => Ok(Some(hit)),
                    None => try_full_noop(root, &fast_noop, shared),
                }) {
                    Ok(Some(hit)) => return (noop_response(version, hit, started), false),
                    Ok(None) => {}
                    Err(error) => {
                        return (
                            Response {
                                version,
                                code: 2,
                                stdout: String::new(),
                                stderr: format!("frost: error: {error:#}\n"),
                            },
                            false,
                        );
                    }
                }
            }
            match std::process::Command::new(program)
                .args(&args)
                .current_dir(root)
                .output()
            {
                Ok(output) => {
                    if output.status.success() {
                        // Drain the child build's writes before clearing its
                        // dirty paths. No proof is installed here: the next
                        // request still performs one complete certificate
                        // validation before watcher-backed hits are possible.
                        let _ = watcher_barrier(root, shared);
                        let mut state = shared.state.lock().unwrap();
                        state.dirty.clear();
                        state.cached_noop = None;
                    }
                    (
                        Response {
                            version,
                            code: output.status.code().unwrap_or(1),
                            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        },
                        false,
                    )
                }
                Err(err) => (
                    Response {
                        version,
                        code: 2,
                        stdout: String::new(),
                        stderr: err.to_string(),
                    },
                    false,
                ),
            }
        }
    }
}

pub fn write_frame<T: Serialize>(stream: &mut impl Write, value: &T) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    stream.write_all(&(payload.len() as u32).to_be_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()?;
    Ok(())
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut impl Read) -> Result<T> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(length <= 64 * 1024 * 1024, "daemon frame too large");
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_barriers_are_distinct_from_workspace_changes() {
        assert_eq!(
            watcher_barrier_id(Path::new(".frost/.frostd-barrier-42")),
            Some(42)
        );
        assert_eq!(
            watcher_barrier_id(Path::new("src/.frostd-barrier-42")),
            None
        );
        assert_eq!(watcher_barrier_id(Path::new(".frost/output")), None);
    }

    #[test]
    fn workspace_daemon_serves_status_and_shutdown() {
        let root = std::env::temp_dir().join(format!(
            "frostd-transport-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let server_root = root.clone();
        let server = std::thread::spawn(move || serve(&server_root));

        let status = (0..100).find_map(|_| {
            let response = request(
                &root,
                &Request::Status {
                    version: PROTOCOL_VERSION,
                },
            )
            .ok();
            if response.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            response
        });
        let status = status.expect("daemon did not publish its endpoint");
        assert_eq!(status.code, 0);
        assert!(status.stdout.starts_with("running ("));

        let stopped = request(
            &root,
            &Request::Shutdown {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert_eq!(stopped.code, 0);
        assert_eq!(stopped.stdout, "stopped");
        server.join().unwrap().unwrap();
        std::fs::remove_dir_all(root).ok();
    }
}
