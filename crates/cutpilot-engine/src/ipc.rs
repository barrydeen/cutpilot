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
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

const LOG_LIMIT: usize = 50;
/// Maximum IPC request size (16 MiB) to prevent unbounded `read_line` OOM.
const MAX_IPC_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum IPC response size (64 MiB, accommodates base64 preview images).
const MAX_IPC_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum concurrent IPC client handler threads.
const MAX_IPC_CONCURRENT_CLIENTS: usize = 16;

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
    #[serde(default)]
    token: String,
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

/// Companion per-session authentication token path next to the IPC socket.
fn token_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("token")
}

/// Constant-time byte slice equality for bearer token verification.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(unix)]
fn read_bounded_line<R: Read>(reader: R, max_bytes: u64) -> Result<Option<String>, String> {
    let mut limited = BufReader::new(reader.take(max_bytes.saturating_add(1)));
    let mut buf = Vec::new();
    let n = limited
        .read_until(b'\n', &mut buf)
        .map_err(|e| format!("IPC read error: {e}"))?;
    if n == 0 {
        return Ok(None);
    }
    if (buf.len() as u64) > max_bytes {
        return Err(format!("IPC message exceeds maximum size ({max_bytes} bytes)"));
    }
    let s = String::from_utf8(buf).map_err(|_| "IPC message is not valid UTF-8".to_string())?;
    Ok(Some(s))
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
    if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
        if !xdg_cache.trim().is_empty() {
            return PathBuf::from(xdg_cache)
                .join("cutpilot")
                .join("run")
                .join("live.sock");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join(".cache")
                .join("cutpilot")
                .join("run")
                .join("live.sock");
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
    token_file: PathBuf,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Mutex<IpcStats>>,
}

#[cfg(unix)]
fn prepare_private_socket_dir(parent: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::symlink_metadata(parent) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to bind IPC socket inside symlink directory `{}`",
                parent.display()
            ));
        }
    }
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("failed to create IPC socket directory: {e}"))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("failed to set 0700 permissions on IPC directory: {e}"))?;
    Ok(())
}

#[cfg(unix)]
fn write_private_token(path: &Path, token: &str) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to write IPC token through symlink `{}`",
                path.display()
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("failed to create IPC token file: {e}"))?;
    file.write_all(token.as_bytes())
        .map_err(|e| format!("failed to write IPC token: {e}"))?;
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    Ok(())
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
        let token_file = token_path(&socket_path);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let setup_res: Result<UnixListener, String> = (|| {
                if let Some(parent) = socket_path.parent() {
                    prepare_private_socket_dir(parent)?;
                }
                if let Ok(meta) = std::fs::symlink_metadata(&socket_path) {
                    if meta.file_type().is_symlink() {
                        return Err(format!(
                            "refusing to remove symlink at IPC socket path `{}`",
                            socket_path.display()
                        ));
                    }
                    let _ = std::fs::remove_file(&socket_path);
                }
                write_private_token(&token_file, &token)?;
                let listener = UnixListener::bind(&socket_path)
                    .map_err(|e| format!("failed to bind IPC socket: {e}"))?;
                std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| format!("failed to set 0600 permissions on IPC socket: {e}"))?;
                Ok(listener)
            })();

            match setup_res {
                Ok(listener) => {
                    let _ = listener.set_nonblocking(true);
                    if let Ok(mut s) = stats.lock() {
                        s.listening = true;
                    }
                    let shutdown_flag = shutdown.clone();
                    let stats_ref = stats.clone();
                    let expected_token = Arc::new(token);
                    std::thread::spawn(move || {
                        accept_loop(
                            listener,
                            engine,
                            stats_ref,
                            shutdown_flag,
                            expected_token,
                            on_event,
                        );
                    });
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&token_file);
                    if let Ok(mut s) = stats.lock() {
                        s.listening = false;
                        s.last_error = Some(e);
                    }
                }
            }
        }

        Self {
            socket_path,
            token_file,
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
        let _ = std::fs::remove_file(&self.token_file);
    }
}

#[cfg(unix)]
struct ActiveClientGuard(Arc<AtomicUsize>);

#[cfg(unix)]
impl Drop for ActiveClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(unix)]
fn accept_loop(
    listener: UnixListener,
    engine: Arc<Mutex<Engine>>,
    stats: Arc<Mutex<IpcStats>>,
    shutdown: Arc<AtomicBool>,
    expected_token: Arc<String>,
    on_event: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let active_clients = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let prev = active_clients.fetch_add(1, Ordering::Relaxed);
                if prev >= MAX_IPC_CONCURRENT_CLIENTS {
                    active_clients.fetch_sub(1, Ordering::Relaxed);
                    let resp = IpcResponse::Err {
                        code: "busy".into(),
                        error: "too many concurrent IPC connections".into(),
                    };
                    write_response(&mut stream, &resp);
                    continue;
                }
                let guard = ActiveClientGuard(active_clients.clone());
                let eng = engine.clone();
                let st = stats.clone();
                let tok = expected_token.clone();
                let cb = on_event.clone();
                std::thread::spawn(move || {
                    let _guard = guard;
                    handle_client(stream, eng, st, &tok, cb);
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
    expected_token: &str,
    on_event: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let line = match read_bounded_line(read_stream, MAX_IPC_REQUEST_BYTES) {
        Ok(Some(l)) => l,
        Ok(None) => return,
        Err(e) => {
            let resp = IpcResponse::Err {
                code: "validation".into(),
                error: e,
            };
            write_response(&mut stream, &resp);
            return;
        }
    };

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

    if !ct_eq(req.token.as_bytes(), expected_token.as_bytes()) {
        let resp = IpcResponse::Err {
            code: "unauthorized".into(),
            error: "invalid or missing IPC authentication token".into(),
        };
        write_response(&mut stream, &resp);
        return;
    }

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
        let token = std::fs::read_to_string(token_path(socket_path))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        let mut stream = UnixStream::connect(socket_path).ok()?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

        let req = IpcRequest {
            token,
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

        let line = match read_bounded_line(stream, MAX_IPC_RESPONSE_BYTES) {
            Ok(Some(l)) => l,
            Ok(None) => {
                return Some(Err(EngineError::Validation(
                    "live CutPilot UI closed the IPC connection unexpectedly".into(),
                )));
            }
            Err(e) => {
                return Some(Err(EngineError::Validation(format!(
                    "failed to read IPC response from CutPilot UI: {e}"
                ))));
            }
        };

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
        assert!(!token_path(&sock).exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn ipc_bridge_enforces_permissions_and_rejects_unauthenticated_clients() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = std::env::temp_dir().join(format!("cutpilot-ipc-sec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("live.sock");

        let engine = Arc::new(Mutex::new(Engine::new()));
        let server = IpcServer::start_at(sock.clone(), engine, None);

        let dir_mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        let tok_mode = std::fs::metadata(token_path(&sock))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(sock_mode, 0o600);
        assert_eq!(tok_mode, 0o600);

        // Send a request with a bogus token directly over the Unix socket.
        let mut raw = UnixStream::connect(&sock).unwrap();
        let bad_req = serde_json::json!({
            "token": "wrong-token",
            "cmd": { "kind": "system_capabilities" },
            "expected_revision": null
        });
        raw.write_all(bad_req.to_string().as_bytes()).unwrap();
        raw.write_all(b"\n").unwrap();
        raw.flush().unwrap();

        let resp_line = read_bounded_line(raw, 4096).unwrap().unwrap();
        let resp: serde_json::Value = serde_json::from_str(resp_line.trim()).unwrap();
        assert_eq!(resp["status"], "err");
        assert_eq!(resp["code"], "unauthorized");

        drop(server);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
