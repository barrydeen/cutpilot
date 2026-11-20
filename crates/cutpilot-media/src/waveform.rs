//! High-resolution audio waveform peaks + RMS energy, streamed through
//! ffmpeg's s16le pipe with bounded memory and fast binary disk caching.

use cutpilot_core::Ticks;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// One waveform bucket: peak envelope (`min`..=`max`, normalized to -1.0..=1.0)
/// plus root-mean-square loudness (`rms`, 0.0..=1.0) so loud peaks and silent
/// regions are immediately distinguishable.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Peak {
    pub min: f32,
    pub max: f32,
    #[serde(default)]
    pub rms: f32,
}

const SAMPLE_RATE: usize = 8_000;
/// Stream-aggregate in 5 ms blocks (40 samples @ 8 kHz) so multi-hour files
/// stay tiny in RAM while retaining sharp transient precision.
const BLOCK_SAMPLES: usize = 40;
/// Cap intermediate blocks before on-the-fly halving (~24 MB max for huge files).
const MAX_RAW_BLOCKS: usize = 1_500_000;

/// Recommended UI waveform bucket count for an audio duration: 50 buckets/sec
/// (20 ms resolution) clamped to `[2_048, 65_536]`.
pub fn recommended_buckets(duration_secs: f64) -> u32 {
    ((duration_secs.max(1.0) * 50.0).round() as u32).clamp(2_048, 65_536)
}

/// Extract up to `buckets` min/max/RMS peaks from the first audio stream.
pub fn waveform_peaks(path: &Path, buckets: u32) -> Result<Vec<Peak>, String> {
    let buckets = buckets.clamp(16, 131_072) as usize;
    let safe_path = crate::safe_local_input(path).map_err(|e| e.to_string())?;
    let mut child = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-protocol_whitelist",
            crate::LOCAL_FILE_PROTOCOL_WHITELIST,
            "-i",
        ])
        .arg(&safe_path)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-ac",
            "1",
            "-ar",
            &SAMPLE_RATE.to_string(),
            "-f",
            "s16le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    let mut blocks: Vec<Peak> = Vec::with_capacity(4_096);
    let mut read_buf = vec![0u8; 64 * 1024];
    let mut stdout = child.stdout.take().expect("stdout piped");

    // Current block accumulator + leftover byte if a chunk splits a 2-byte sample.
    let mut leftover: Option<u8> = None;
    let mut blk_min = 0.0_f32;
    let mut blk_max = 0.0_f32;
    let mut blk_sum_sq = 0.0_f64;
    let mut blk_count = 0usize;

    let flush_block = |blocks: &mut Vec<Peak>,
                       blk_min: &mut f32,
                       blk_max: &mut f32,
                       blk_sum_sq: &mut f64,
                       blk_count: &mut usize| {
        if *blk_count > 0 {
            let rms = (*blk_sum_sq / *blk_count as f64).sqrt() as f32;
            blocks.push(Peak {
                min: *blk_min,
                max: *blk_max,
                rms: rms.clamp(0.0, 1.0),
            });
            *blk_min = 0.0;
            *blk_max = 0.0;
            *blk_sum_sq = 0.0;
            *blk_count = 0;
            if blocks.len() >= MAX_RAW_BLOCKS {
                *blocks = halve_blocks(blocks);
            }
        }
    };

    loop {
        let n = stdout.read(&mut read_buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let mut i = 0usize;
        if let Some(prev) = leftover.take() {
            let s = i16::from_le_bytes([prev, read_buf[0]]) as f32 / 32_768.0;
            blk_min = blk_min.min(s);
            blk_max = blk_max.max(s);
            blk_sum_sq += (s as f64) * (s as f64);
            blk_count += 1;
            if blk_count >= BLOCK_SAMPLES {
                flush_block(
                    &mut blocks,
                    &mut blk_min,
                    &mut blk_max,
                    &mut blk_sum_sq,
                    &mut blk_count,
                );
            }
            i = 1;
        }
        while i + 1 < n {
            let s = i16::from_le_bytes([read_buf[i], read_buf[i + 1]]) as f32 / 32_768.0;
            blk_min = blk_min.min(s);
            blk_max = blk_max.max(s);
            blk_sum_sq += (s as f64) * (s as f64);
            blk_count += 1;
            if blk_count >= BLOCK_SAMPLES {
                flush_block(
                    &mut blocks,
                    &mut blk_min,
                    &mut blk_max,
                    &mut blk_sum_sq,
                    &mut blk_count,
                );
            }
            i += 2;
        }
        if i < n {
            leftover = Some(read_buf[i]);
        }
    }
    flush_block(
        &mut blocks,
        &mut blk_min,
        &mut blk_max,
        &mut blk_sum_sq,
        &mut blk_count,
    );

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
    Ok(downsample(&blocks, buckets))
}

fn halve_blocks(blocks: &[Peak]) -> Vec<Peak> {
    let mut out = Vec::with_capacity(blocks.len().div_ceil(2));
    for chunk in blocks.chunks(2) {
        out.push(merge_chunk(chunk));
    }
    out
}

fn merge_chunk(chunk: &[Peak]) -> Peak {
    let mut min = 0.0_f32;
    let mut max = 0.0_f32;
    let mut rms_sq = 0.0_f64;
    for p in chunk {
        min = min.min(p.min);
        max = max.max(p.max);
        rms_sq += (p.rms as f64) * (p.rms as f64);
    }
    let rms = if chunk.is_empty() {
        0.0
    } else {
        (rms_sq / chunk.len() as f64).sqrt() as f32
    };
    Peak {
        min,
        max,
        rms: rms.clamp(0.0, 1.0),
    }
}

fn downsample(peaks: &[Peak], buckets: usize) -> Vec<Peak> {
    if peaks.is_empty() || buckets == 0 {
        return Vec::new();
    }
    if peaks.len() <= buckets {
        return peaks.to_vec();
    }
    let len = peaks.len();
    let mut out = Vec::with_capacity(buckets);
    for i in 0..buckets {
        let start = (i * len) / buckets;
        let end = (((i + 1) * len) / buckets).max(start + 1).min(len);
        out.push(merge_chunk(&peaks[start..end]));
    }
    out
}

const WF_MAGIC: &[u8; 8] = b"CPWF2\0\0\0";

/// Save waveform peaks to a compact binary cache file (12 bytes per bucket).
pub fn save_peaks_bin(path: &Path, peaks: &[Peak]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = Vec::with_capacity(12 + peaks.len() * 12);
    buf.extend_from_slice(WF_MAGIC);
    buf.extend_from_slice(&(peaks.len() as u32).to_le_bytes());
    for p in peaks {
        buf.extend_from_slice(&p.min.to_le_bytes());
        buf.extend_from_slice(&p.max.to_le_bytes());
        buf.extend_from_slice(&p.rms.to_le_bytes());
    }
    let tmp = path.with_extension("wf2.tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load waveform peaks from a binary cache file written by [`save_peaks_bin`].
pub fn load_peaks_bin(path: &Path) -> Option<Vec<Peak>> {
    const MAX_CACHED_PEAKS: usize = 262_144;
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 12 || &bytes[0..8] != WF_MAGIC {
        return None;
    }
    let count = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if count > MAX_CACHED_PEAKS {
        return None;
    }
    let expected = count.checked_mul(12)?.checked_add(12)?;
    if bytes.len() != expected {
        return None;
    }
    let mut peaks = Vec::with_capacity(count);
    let mut off = 12;
    for _ in 0..count {
        let min = f32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        let max = f32::from_le_bytes([
            bytes[off + 4],
            bytes[off + 5],
            bytes[off + 6],
            bytes[off + 7],
        ]);
        let rms = f32::from_le_bytes([
            bytes[off + 8],
            bytes[off + 9],
            bytes[off + 10],
            bytes[off + 11],
        ]);
        if !min.is_finite() || !max.is_finite() || !rms.is_finite() {
            return None;
        }
        peaks.push(Peak {
            min: min.clamp(-1.0, 1.0),
            max: max.clamp(-1.0, 1.0),
            rms: rms.clamp(0.0, 1.0),
        });
        off += 12;
    }
    Some(peaks)
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
    fn downsample_collapses_buckets_preserving_peaks_and_rms() {
        let peaks: Vec<Peak> = (0..100)
            .map(|i| Peak {
                min: -0.5,
                max: (i as f32) / 200.0,
                rms: 0.25,
            })
            .collect();
        let out = downsample(&peaks, 10);
        assert_eq!(out.len(), 10);
        assert!((out[9].max - 0.495).abs() < 0.01);
        assert!((out[9].min - (-0.5)).abs() < 0.001);
        assert!((out[9].rms - 0.25).abs() < 0.001);
    }

    #[test]
    fn recommended_buckets_scales_with_duration() {
        assert_eq!(recommended_buckets(10.0), 2_048);
        assert_eq!(recommended_buckets(600.0), 30_000);
        assert_eq!(recommended_buckets(7200.0), 65_536);
    }

    #[test]
    fn binary_cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.wf2");
        let peaks = vec![
            Peak { min: -0.8, max: 0.9, rms: 0.45 },
            Peak { min: -0.01, max: 0.01, rms: 0.004 },
        ];
        save_peaks_bin(&file, &peaks).unwrap();
        let loaded = load_peaks_bin(&file).unwrap();
        assert_eq!(loaded, peaks);
    }

    #[test]
    fn binary_cache_rejects_overflow_and_non_finite_peaks() {
        let dir = tempfile::tempdir().unwrap();
        let overflow = dir.path().join("overflow.wf2");
        let mut raw = Vec::new();
        raw.extend_from_slice(WF_MAGIC);
        raw.extend_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&overflow, &raw).unwrap();
        assert!(load_peaks_bin(&overflow).is_none());

        let nan_file = dir.path().join("nan.wf2");
        let mut nan_raw = Vec::new();
        nan_raw.extend_from_slice(WF_MAGIC);
        nan_raw.extend_from_slice(&1u32.to_le_bytes());
        nan_raw.extend_from_slice(&f32::NAN.to_le_bytes());
        nan_raw.extend_from_slice(&0.5f32.to_le_bytes());
        nan_raw.extend_from_slice(&0.2f32.to_le_bytes());
        std::fs::write(&nan_file, &nan_raw).unwrap();
        assert!(load_peaks_bin(&nan_file).is_none());
    }
}
