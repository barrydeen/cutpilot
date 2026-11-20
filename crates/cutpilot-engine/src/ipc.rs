//! Live local IPC bridge between `cutpilot-ui` and `cutpilot-mcp`.
//!
//! When the native editor (`cutpilot-ui`) is running, it opens a Unix domain
//! socket and serves its in-memory [`Engine`]. Any MCP agent running
//! `cutpilot-mcp` automatically discovers the live socket and routes its
//! commands through the exact same Engine instance — so the human watches
//! agent edits appear live on the timeline with shared Undo/Redo history.
//!
//! When `cutpilot-ui` is not running (headless mode) or when
//! `CUTPILOT_STANDALONE=1` is set (e.g. in automated tests), `cutpilot-mcp`
//! seamlessly runs against its own local in-process Engine.

use crate::{Engine, EngineError};
use base64::prelude::{Engine as _, BASE64_STANDARD};
use cutpilot_core::{Command, Outcome};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

const LOG_LIMIT: usize = 50;

#[derive(Debug, Clone)]
pub struct IpcLogEntry {
    pub unix_secs: u64,
    pub kind: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct IpcStats {
    pub socket_path: PathBuf,
    pub listening: bool,
    pub total_calls: u64,
    pub last_call_unix_secs: Option<u64>,
    pub recent: VecDeque<IpcLogEntry>,
    pub last_error: Option<String>,
}

impl IpcStats {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            listening: false,
            total_calls: 0,
            last_call_unix_secs: None,
            recent: VecDeque::new(),
            last_error: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct IpcRequest {
    cmd: Command,
    expected_revision: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum IpcResponse {
    Ok {
        outcome: Outcome,
        binary_mime: Option<String>,
        binary_b64: Option<String>,
    },
    Err {
        code: String,
        error: String,
    },
}

/// Resolve the IPC socket path (`CUTPILOT_IPC_SOCKET` overrides the default).
pub fn default_socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("CUTPILOT_IPC_SOCKET") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.trim().is_empty() {
            return PathBuf::from(runtime).join("cutpilot").join("live.sock");
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    std::env::temp_dir()
        .join(format!("cutpilot-{user}"))
        .join("live.sock")
}

/// True when standalone mode is forced via `CUTPILOT_STANDALONE=1`.
pub fn is_standalone_env() -> bool {
    std::env::var("CUTPILOT_STANDALONE")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false)
}

/// Handle to a running IPC bridge server owned by `cutpilot-ui`.
pub struct IpcServer {
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Mutex<IpcStats>>,
}

impl IpcServer {
    /// Start listening on the default (or env-configured) socket path.
    pub fn start(
        engine: Arc<Mutex<Engine>>,
        on_event: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self::start_at(default_socket_path(), engine, on_event)
    }

    /// Start listening on an explicit socket path (useful for tests).
    pub fn start_at(
        socket_path: PathBuf,
        engine: Arc<Mutex<Engine>>,
        on_event: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(IpcStats::new(socket_path.clone())));

        #[cfg(unix)]
        {
            if let Some(parent) = socket_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Remove a stale socket from a previous crashed session.
            if socket_path.exists() {
                let _ = std::fs::remove_file(&socket_path);
            }
            match UnixListener::bind(&socket_path) {
                Ok(listener) => {
                    let _ = listener.set_nonblocking(true);
                    if let Ok(mut s) = stats.lock() {
                        s.listening = true;
                    }
                    let shutdown_flag = shutdown.clone();
                    let stats_ref = stats.clone();
                    std::thread::spawn(move || {
                        accept_loop(listener, engine, stats_ref, shutdown_flag, on_event);
                    });
                }
                Err(e) => {
                    if let Ok(mut s) = stats.lock() {
                        s.listening = false;
                        s.last_error = Some(format!("failed to bind IPC socket: {e}"));
                    }
                }
            }
        }

        Self {
            socket_path,
            shutdown,
            stats,
        }
    }

    pub fn stats(&self) -> Arc<Mutex<IpcStats>> {
        self.stats.clone()
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
fn accept_loop(
    listener: UnixListener,
    engine: Arc<Mutex<Engine>>,
    stats: Arc<Mutex<IpcStats>>,
    shutdown: Arc<AtomicBool>,
    on_event: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let eng = engine.clone();
                let st = stats.clone();
                let cb = on_event.clone();
                std::thread::spawn(move || {
                    handle_client(stream, eng, st, cb);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(unix)]
fn handle_client(
    mut stream: UnixStream,
    engine: Arc<Mutex<Engine>>,
    stats: Arc<Mutex<IpcStats>>,
    on_event: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }

    let req: IpcRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let resp = IpcResponse::Err {
                code: "validation".into(),
                error: format!("invalid IPC request: {e}"),
            };
            write_response(&mut stream, &resp);
            return;
        }
    };

    let cmd_kind = req.cmd.kind().to_string();
    let result = {
        match engine.lock() {
            Ok(mut guard) => guard.execute_checked(req.cmd, req.expected_revision),
            Err(e) => Err(EngineError::Validation(format!("engine lock poisoned: {e}"))),
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let resp = match result {
        Ok(mut outcome) => {
            let bin = outcome.binary.take();
            let (binary_mime, binary_b64) = match bin {
                Some(b) => (Some(b.mime), Some(BASE64_STANDARD.encode(&b.bytes))),
                None => (None, None),
            };
            record_log(&stats, now, cmd_kind, true, outcome.message.clone());
            IpcResponse::Ok {
                outcome,
                binary_mime,
                binary_b64,
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let code = e.code().to_string();
            record_log(&stats, now, cmd_kind, false, msg.clone());
            IpcResponse::Err { code, error: msg }
        }
    };

    write_response(&mut stream, &resp);
    if let Some(cb) = on_event {
        cb();
    }
}

fn record_log(stats: &Arc<Mutex<IpcStats>>, unix_secs: u64, kind: String, ok: bool, message: String) {
    if let Ok(mut s) = stats.lock() {
        s.total_calls += 1;
        s.last_call_unix_secs = Some(unix_secs);
        s.recent.push_back(IpcLogEntry {
            unix_secs,
            kind,
            ok,
            message,
        });
        while s.recent.len() > LOG_LIMIT {
            s.recent.pop_front();
        }
    }
}

#[cfg(unix)]
fn write_response(stream: &mut UnixStream, resp: &IpcResponse) {
    if let Ok(json) = serde_json::to_string(resp) {
        let _ = stream.write_all(json.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
}

/// Try to execute `cmd` against a live `cutpilot-ui` instance over IPC.
/// Returns `None` if standalone mode is active or no live UI is listening.
pub fn try_remote_execute(
    cmd: &Command,
    expected_revision: Option<u64>,
) -> Option<Result<Outcome, EngineError>> {
    if is_standalone_env() {
        return None;
    }
    try_remote_execute_at(&default_socket_path(), cmd, expected_revision)
}

/// Connect to a specific socket path and run one command.
pub fn try_remote_execute_at(
    socket_path: &Path,
    cmd: &Command,
    expected_revision: Option<u64>,
) -> Option<Result<Outcome, EngineError>> {
    #[cfg(unix)]
    {
        let mut stream = UnixStream::connect(socket_path).ok()?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

        let req = IpcRequest {
            cmd: cmd.clone(),
            expected_revision,
        };
        let payload = match serde_json::to_string(&req) {
            Ok(s) => s,
            Err(e) => return Some(Err(EngineError::Validation(e.to_string()))),
        };
        if stream
            .write_all(payload.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .and_then(|_| stream.flush())
            .is_err()
        {
            return None;
        }

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return Some(Err(EngineError::Validation(
                "live CutPilot UI closed the IPC connection unexpectedly".into(),
            )));
        }

        let resp: IpcResponse = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                return Some(Err(EngineError::Validation(format!(
                    "malformed IPC response from CutPilot UI: {e}"
                ))));
            }
        };

        match resp {
            IpcResponse::Ok {
                mut outcome,
                binary_mime,
                binary_b64,
            } => {
                if let (Some(mime), Some(b64)) = (binary_mime, binary_b64) {
                    if let Ok(bytes) = BASE64_STANDARD.decode(b64) {
                        outcome = outcome.with_binary(&mime, bytes);
                    }
                }
                Some(Ok(outcome))
            }
            IpcResponse::Err { error, .. } => Some(Err(EngineError::Validation(error))),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (socket_path, cmd, expected_revision);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_bridge_executes_commands_live() {
        let tmp = std::env::temp_dir().join(format!("cutpilot-ipc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("live.sock");

        let engine = Arc::new(Mutex::new(Engine::new()));
        let woken = Arc::new(AtomicBool::new(false));
        let woken_clone = woken.clone();
        let server = IpcServer::start_at(
            sock.clone(),
            engine.clone(),
            Some(Arc::new(move || {
                woken_clone.store(true, Ordering::Relaxed);
            })),
        );

        let caps = try_remote_execute_at(&sock, &Command::SystemCapabilities {}, None)
            .expect("should connect to live server")
            .expect("capabilities should succeed");
        assert_eq!(caps.kind, "system_capabilities");
        assert!(woken.load(Ordering::Relaxed));

        let stats = server.stats().lock().unwrap().clone();
        assert!(stats.listening);
        assert_eq!(stats.total_calls, 1);
        assert_eq!(stats.recent.len(), 1);
        assert_eq!(stats.recent[0].kind, "system_capabilities");

        drop(server);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
