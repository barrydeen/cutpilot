//! Audio waveform peaks, streamed through ffmpeg's s16le pipe.

use openprem_core::Ticks;
use std::path::Path;
use std::process::{Command, Stdio};

/// One min/max peak bucket, normalized to -1.0..=1.0.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Peak {
    pub min: f32,
    pub max: f32,
}

/// Extract up to `buckets` min/max peaks from the first audio stream.
pub fn waveform_peaks(path: &Path, buckets: u32) -> Result<Vec<Peak>, String> {
    let buckets = buckets.clamp(16, 4_096) as usize;
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-ac", "1", "-ar", "8000"])
        .args(["-f", "s16le", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Stream the samples so huge files never buffer fully in RAM.
    let mut peaks: Vec<Peak> = Vec::new();
    let mut read_buf = vec![0u8; 64 * 1024];
    use std::io::Read;
    let mut stdout = child.stdout.take().expect("stdout piped");
    loop {
        let n = stdout.read(&mut read_buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let mut i = 0;
        while i + 1 < n {
            let s = i16::from_le_bytes([read_buf[i], read_buf[i + 1]]) as f32 / 32_768.0;
            peaks.push(Peak {
                min: s.min(0.0),
                max: s.max(0.0),
            });
            i += 2;
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        let stderr = child.stderr.take();
        let msg = stderr
            .map(|mut s| {
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf);
                buf.trim().to_string()
            })
            .unwrap_or_default();
        return Err(format!("waveform extraction failed: {msg}"));
    }
    Ok(downsample(&peaks, buckets))
}

fn downsample(peaks: &[Peak], buckets: usize) -> Vec<Peak> {
    if peaks.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let per = peaks.len().div_ceil(buckets);
    let mut out = Vec::with_capacity(buckets);
    for chunk in peaks.chunks(per) {
        out.push(Peak {
            min: chunk.iter().map(|p| p.min).fold(1.0, f32::min),
            max: chunk.iter().map(|p| p.max).fold(-1.0, f32::max),
        });
    }
    out
}

/// Duration covered by the peaks, for callers that want it alongside.
pub fn audio_duration_ticks(path: &Path) -> Result<Ticks, String> {
    let probe = crate::ffprobe::probe(path).map_err(|e| e.to_string())?;
    Ok(probe.probe.duration_ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_collapses_buckets() {
        let peaks: Vec<Peak> = (0..100)
            .map(|i| Peak {
                min: -0.5,
                max: (i as f32) / 200.0,
            })
            .collect();
        let out = downsample(&peaks, 10);
        assert_eq!(out.len(), 10);
        assert!((out[9].max - 0.4975).abs() < 0.01);
    }
}
