//! Silence detection via ffmpeg's `silencedetect` audio filter.

use cutpilot_core::{Ticks, TimeRange, TICKS_PER_SEC};
use std::path::Path;
use std::process::Command;

/// Options for silence analysis.
#[derive(Debug, Clone)]
pub struct SilenceOptions {
    /// Noise floor in dB (default -35).
    pub threshold_db: f32,
    /// Minimum silence length in seconds (default 0.6).
    pub min_duration_secs: f64,
    /// Padding kept around each detected range, in seconds (default 0.15).
    pub padding_secs: f64,
}

impl Default for SilenceOptions {
    fn default() -> Self {
        Self {
            threshold_db: -35.0,
            min_duration_secs: 0.6,
            padding_secs: 0.15,
        }
    }
}

/// Run silencedetect and return padded, clamped ranges in ticks.
pub fn detect_silence(
    path: &Path,
    duration_ticks: Ticks,
    opts: &SilenceOptions,
) -> Result<Vec<TimeRange>, String> {
    if !opts.threshold_db.is_finite() || !(-120.0..=0.0).contains(&opts.threshold_db) {
        return Err("threshold_db must be finite and between -120 and 0".into());
    }
    if !opts.min_duration_secs.is_finite() || !(0.01..=3600.0).contains(&opts.min_duration_secs) {
        return Err("min_duration_secs must be finite and between 0.01 and 3600".into());
    }
    if !opts.padding_secs.is_finite() || !(0.0..=60.0).contains(&opts.padding_secs) {
        return Err("padding_secs must be finite and between 0 and 60".into());
    }
    let safe_path = crate::safe_local_input(path).map_err(|e| e.to_string())?;
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostats",
            "-vn",
            "-protocol_whitelist",
            crate::LOCAL_FILE_PROTOCOL_WHITELIST,
            "-i",
        ])
        .arg(&safe_path)
        .arg("-af")
        .arg(format!(
            "silencedetect=noise={:.1}dB:d={:.3}",
            opts.threshold_db, opts.min_duration_secs
        ))
        .args(["-f", "null", "-"])
        .output()
        .map_err(|e| format!("failed to run ffmpeg: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ranges = parse_silence_stderr(&stderr, duration_ticks);
    Ok(apply_padding(ranges, opts.padding_secs, duration_ticks))
}

/// Parse `silencedetect` stderr lines into tick ranges. Pure, unit-tested.
pub fn parse_silence_stderr(stderr: &str, duration_ticks: Ticks) -> Vec<TimeRange> {
    let mut starts: Vec<Ticks> = Vec::new();
    let mut ranges: Vec<TimeRange> = Vec::new();
    for line in stderr.lines() {
        if let Some(rest) = line.split("silence_start:").nth(1) {
            let value = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok());
            if let Some(secs) = value {
                starts.push((secs * TICKS_PER_SEC as f64).round() as Ticks);
            }
        } else if let Some(rest) = line.split("silence_end:").nth(1) {
            let value = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok());
            if let Some(end_secs) = value {
                let end = (end_secs * TICKS_PER_SEC as f64).round() as Ticks;
                if let Some(start) = starts.pop() {
                    if end > start {
                        ranges.push(TimeRange { start, end });
                    }
                }
            }
        }
    }
    // Unterminated trailing silence runs to the end of the file.
    if let Some(start) = starts.pop() {
        if duration_ticks > start {
            ranges.push(TimeRange {
                start,
                end: duration_ticks,
            });
        }
    }
    ranges.sort_by_key(|r| r.start);
    ranges
}

fn apply_padding(
    ranges: Vec<TimeRange>,
    padding_secs: f64,
    duration_ticks: Ticks,
) -> Vec<TimeRange> {
    if padding_secs <= 0.0 {
        return ranges;
    }
    let pad = (padding_secs.clamp(0.0, 60.0) * TICKS_PER_SEC as f64).round() as Ticks;
    let mut padded: Vec<TimeRange> = ranges
        .into_iter()
        .map(|r| TimeRange {
            start: r.start.saturating_sub(pad).max(0),
            end: r.end.saturating_add(pad).min(duration_ticks),
        })
        .collect();
    // Merge ranges that now overlap after padding.
    padded.sort_by_key(|r| r.start);
    let mut merged: Vec<TimeRange> = Vec::new();
    for r in padded {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => {
                last.end = last.end.max(r.end);
            }
            _ => merged.push(r),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[silencedetect @ 0x1] silence_start: -0.023
[silencedetect @ 0x1] silence_end: 1.5 | silence_duration: 1.5
[silencedetect @ 0x1] silence_start: 12
[info] some unrelated line
[silencedetect @ 0x1] silence_end: 14.5 | silence_duration: 2.5
[silencedetect @ 0x1] silence_start: 20
";

    #[test]
    fn parses_pairs_and_trailing_silence() {
        let dur = TICKS_PER_SEC * 30;
        let ranges = parse_silence_stderr(SAMPLE, dur);
        assert_eq!(ranges.len(), 3);
        assert!(ranges[0].start <= 0); // negative start clamps at use time
        assert_eq!(ranges[0].end, TICKS_PER_SEC + TICKS_PER_SEC / 2);
        assert_eq!(ranges[1].start, TICKS_PER_SEC * 12);
        assert_eq!(ranges[1].end, TICKS_PER_SEC * 14 + TICKS_PER_SEC / 2);
        // trailing silence runs to duration
        assert_eq!(ranges[2].start, TICKS_PER_SEC * 20);
        assert_eq!(ranges[2].end, dur);
    }

    #[test]
    fn padding_merges_nearby_ranges() {
        let dur = TICKS_PER_SEC * 30;
        let ranges = vec![
            TimeRange {
                start: TICKS_PER_SEC * 2,
                end: TICKS_PER_SEC * 4,
            },
            TimeRange {
                start: TICKS_PER_SEC * 5,
                end: TICKS_PER_SEC * 6,
            },
        ];
        let merged = apply_padding(ranges, 0.75, dur);
        assert_eq!(merged.len(), 1);
        // 2s - 0.75s padding = 1.25s; 6s + 0.75s = 6.75s.
        assert_eq!(
            merged[0].start,
            TICKS_PER_SEC + TICKS_PER_SEC / 4
        );
        assert_eq!(merged[0].end, (TICKS_PER_SEC * 6) + (TICKS_PER_SEC * 3 / 4));
    }

    #[test]
    fn detect_silence_rejects_non_finite_or_remote_inputs() {
        let bad_opts = SilenceOptions {
            threshold_db: f32::NAN,
            ..Default::default()
        };
        assert!(detect_silence(Path::new("/tmp/a.mp4"), TICKS_PER_SEC, &bad_opts).is_err());
        assert!(detect_silence(
            Path::new("http://example.com/a.mp4"),
            TICKS_PER_SEC,
            &SilenceOptions::default()
        )
        .is_err());
    }
}
