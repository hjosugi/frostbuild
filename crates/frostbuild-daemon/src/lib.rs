use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
#[cfg(windows)]
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Default)]
struct DaemonState {
    dirty: BTreeSet<PathBuf>,
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

#[derive(Debug, Serialize, Deserialize)]
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

    let state = Arc::new(Mutex::new(DaemonState::default()));
    let event_state = Arc::clone(&state);
    let root_owned = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            if !matches!(
                event.kind,
                notify::EventKind::Any
                    | notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            ) {
                return;
            }
            let mut state = event_state.lock().unwrap();
            for path in event.paths {
                let relative = path.strip_prefix(&root_owned).unwrap_or(&path);
                if !relative.starts_with(".frost") && !relative.starts_with(".git") {
                    state.dirty.insert(relative.to_path_buf());
                }
            }
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

fn handle(root: &Path, request: Request, state: &Mutex<DaemonState>) -> (Response, bool) {
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
            let count = state.lock().unwrap().dirty.len();
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
                let started = std::time::Instant::now();
                match frostbuild_exec::try_fast_noop_with_key_environment(
                    root,
                    &fast_noop.profile,
                    &fast_noop.platform,
                    &fast_noop.key_env,
                ) {
                    Ok(Some(hit)) => {
                        state.lock().unwrap().dirty.clear();
                        let scope = if hit.closure_actions < hit.graph_actions {
                            format!("{} of {} actions", hit.closure_actions, hit.graph_actions)
                        } else {
                            format!("{} actions", hit.closure_actions)
                        };
                        return (
                            Response {
                                version,
                                code: 0,
                                stdout: format!(
                                    "frost: up to date · {scope} · {} ms\n",
                                    started.elapsed().as_millis()
                                ),
                                stderr: String::new(),
                            },
                            false,
                        );
                    }
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
                        // The dirty set feeds `daemon status` only. It used to
                        // be cleared after a fixed 20 ms sleep, meant to let
                        // the watcher deliver events for the build's own
                        // output writes first — a delay every build paid to
                        // keep a reported count tidy. Clearing immediately can
                        // leave frost's own writes counted until the next
                        // build, which is cosmetic. Before this set may prune
                        // real work it has to identify self-writes properly.
                        state.lock().unwrap().dirty.clear();
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
