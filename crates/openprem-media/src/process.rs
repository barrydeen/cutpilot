//! Shared ffmpeg/CLI process runner with `-progress` parsing, stderr capture,
//! and cooperative cancellation (child kill on cancel flag).

use crate::MediaError;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Result of a finished process.
pub struct RunResult {
    pub stderr: String,
}

/// Run `program` with `args`, reporting progress as a 0..1 fraction against
/// `total_secs`, and killing the child when `cancel` is set.
pub fn run_with_progress(
    program: &str,
    args: &[String],
    total_secs: f64,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<RunResult, MediaError> {
    let mut child = spawn(program, args)?;
    let result = pump(&mut child, total_secs, progress, cancel)?;
    finish(child, result)
}

/// Run a process in a working directory (used by the HyperFrames CLI).
pub fn run_in_dir(
    program: &str,
    args: &[String],
    cwd: &std::path::Path,
    cancel: Arc<AtomicBool>,
) -> Result<RunResult, MediaError> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = pump(&mut child, 1.0, None, cancel)?;
    finish(child, result)
}

fn spawn(program: &str, args: &[String]) -> Result<Child, MediaError> {
    Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(MediaError::Io)
}

struct Pumped {
    stderr_rx: mpsc::Receiver<String>,
}

fn pump(
    child: &mut Child,
    total_secs: f64,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<Pumped, MediaError> {
    // stderr: capture everything for error reporting.
    let mut stderr = child.stderr.take().expect("stderr piped");
    let (err_tx, err_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut buf);
        let _ = err_tx.send(buf);
    });

    // stdout: parse `-progress` key=value lines.
    let stdout = child.stdout.take().expect("stdout piped");
    let (prog_tx, prog_rx) = mpsc::channel::<f64>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(value) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = value.trim().parse::<f64>() {
                    let _ = prog_tx.send(us / 1_000_000.0);
                }
            } else if let Some(value) = line.strip_prefix("out_time_ms=") {
                if let Ok(ms) = value.trim().parse::<f64>() {
                    let _ = prog_tx.send(ms / 1_000.0);
                }
            }
        }
    });

    let mut cancelled = false;
    loop {
        match prog_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(secs) => {
                if let Some(cb) = &progress {
                    let fraction = if total_secs > 0.0 {
                        (secs / total_secs).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    cb(fraction);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    let _ = child.kill();
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if cancelled {
        return Err(MediaError::Cancelled);
    }
    Ok(Pumped { stderr_rx: err_rx })
}

fn finish(mut child: Child, pumped: Pumped) -> Result<RunResult, MediaError> {
    let status = child.wait()?;
    let stderr = pumped
        .stderr_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_default();
    if !status.success() {
        return Err(MediaError::Ffmpeg {
            message: tail_of(&stderr, 2_000),
        });
    }
    Ok(RunResult { stderr })
}

fn tail_of(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.trim().to_string();
    }
    let start = s.len() - max;
    let slice = &s[start..];
    let cut = slice.find('\n').map(|i| i + 1).unwrap_or(0);
    s[start + cut..].trim().to_string()
}
