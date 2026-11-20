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

enum ProgressMsg {
    TimeSecs(f64),
    Ended,
}

/// Parse an ffmpeg `-progress pipe:1` line into output timestamp seconds.
/// Note: ffmpeg's `out_time_ms` is famously reported in **microseconds**
/// (identical to `out_time_us`), NOT milliseconds.
fn parse_progress_line(line: &str) -> Option<ProgressMsg> {
    if let Some(value) = line.strip_prefix("out_time_us=") {
        let us = value.trim().parse::<f64>().ok()?;
        if us.is_finite() && us >= 0.0 {
            return Some(ProgressMsg::TimeSecs(us / 1_000_000.0));
        }
    } else if let Some(value) = line.strip_prefix("out_time_ms=") {
        let us = value.trim().parse::<f64>().ok()?;
        if us.is_finite() && us >= 0.0 {
            return Some(ProgressMsg::TimeSecs(us / 1_000_000.0));
        }
    } else if line.trim() == "progress=end" {
        return Some(ProgressMsg::Ended);
    }
    None
}

fn pump(
    child: &mut Child,
    total_secs: f64,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<Pumped, MediaError> {
    // stderr: capture bounded diagnostic output (cap at 256 KiB, draining any
    // excess so the child never blocks on a full pipe and RAM stays bounded).
    let mut stderr = child.stderr.take().expect("stderr piped");
    let (err_tx, err_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::Read;
        const MAX_STDERR_BYTES: usize = 256 * 1024;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if raw.len() < MAX_STDERR_BYTES {
                        let take = (MAX_STDERR_BYTES - raw.len()).min(n);
                        raw.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
        let _ = err_tx.send(String::from_utf8_lossy(&raw).into_owned());
    });

    // stdout: parse `-progress` key=value lines.
    let stdout = child.stdout.take().expect("stdout piped");
    let (prog_tx, prog_rx) = mpsc::channel::<ProgressMsg>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(msg) = parse_progress_line(&line) {
                let _ = prog_tx.send(msg);
            }
        }
    });

    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            let _ = child.kill();
            break;
        }
        match prog_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ProgressMsg::TimeSecs(secs)) => {
                if let Some(cb) = &progress {
                    let fraction = if total_secs > 0.0 {
                        // Cap at 0.99 until `progress=end` so +faststart/verify
                        // doesn't appear stuck at 100% before completion.
                        (secs / total_secs).clamp(0.0, 0.99)
                    } else {
                        0.0
                    };
                    cb(fraction);
                }
            }
            Ok(ProgressMsg::Ended) => {
                if let Some(cb) = &progress {
                    cb(1.0);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_microsecond_out_time_us_and_out_time_ms() {
        match parse_progress_line("out_time_us=2500000") {
            Some(ProgressMsg::TimeSecs(s)) => assert!((s - 2.5).abs() < 1e-6),
            _ => panic!("expected 2.5s from out_time_us"),
        }
        // ffmpeg reports out_time_ms in microseconds too (not milliseconds!).
        match parse_progress_line("out_time_ms=2500000") {
            Some(ProgressMsg::TimeSecs(s)) => assert!((s - 2.5).abs() < 1e-6),
            _ => panic!("expected 2.5s from out_time_ms"),
        }
        assert!(parse_progress_line("out_time_us=N/A").is_none());
        assert!(parse_progress_line("out_time_us=-500").is_none());
        assert!(matches!(
            parse_progress_line("progress=end"),
            Some(ProgressMsg::Ended)
        ));
    }
}
