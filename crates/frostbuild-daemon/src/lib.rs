use std::collections::BTreeSet;
use std::io::{Read, Write};
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
    },
    Shutdown {
        version: u32,
    },
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
pub fn socket_path(root: &Path) -> PathBuf {
    let key = blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex();
    runtime_dir().join(format!("frostd-{}.sock", &key[..16]))
}

fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return dir;
        }
    }
    std::env::temp_dir()
}

pub fn request(root: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path(root)).context("frostd is not running")?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

pub fn serve(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join(".frost"))?;
    let socket = socket_path(root);
    // A stale socket from a daemon that was killed rather than shut down
    // would otherwise make bind fail until someone deletes it by hand.
    if UnixStream::connect(&socket).is_ok() {
        anyhow::bail!("frostd is already running for {}", root.display());
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind {}", socket.display()))?;
    let state = Arc::new(Mutex::new(DaemonState::default()));
    let event_state = Arc::clone(&state);
    let root_owned = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
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
    let _ = std::fs::remove_file(socket);
    Ok(())
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
        Request::Run { program, args, .. } => {
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
