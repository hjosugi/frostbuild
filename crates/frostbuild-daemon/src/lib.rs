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

pub fn socket_path(root: &Path) -> PathBuf {
    root.join(".frost/frostd.sock")
}

pub fn request(root: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path(root)).context("frostd is not running")?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

pub fn serve(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join(".frost"))?;
    let socket = socket_path(root);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
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
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        let mut state = state.lock().unwrap();
                        state.dirty.clear();
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
