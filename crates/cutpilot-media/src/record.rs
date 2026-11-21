//! Live audio input enumeration, studio-grade real-time voiceover DSP, and
//! broadcast mastering for CutPilot voiceovers.
//!
//! Captures 32-bit floating-point PCM (`f32le`) from the host audio daemon
//! (`parecord` / `pw-record` / `arecord` on Linux, `ffmpeg` on macOS &
//! Windows) through a zero-latency studio input strip (80 Hz rumble filter,
//! live input gain, gentle auto-leveling, and transparent soft-knee limiter)
//! so hot microphones never slam into harsh digital clipping. When
//! `studio_voice` is enabled (default), stopping a take automatically runs a
//! fast FFmpeg broadcast voiceover polish pass (noise reduction, vocal clarity
//! EQ, smooth compression, and -16 LUFS loudness normalization).

use crate::MediaError;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_CHANNELS: u16 = 1;
/// Conservative default input gain (~-3.7 dB) so hot USB/laptop mics sit in
/// the clean green/gold sweet spot instead of slamming the red ceiling.
pub const DEFAULT_INPUT_GAIN: f32 = 0.65;
pub const MIN_INPUT_GAIN: f32 = 0.05;
pub const MAX_INPUT_GAIN: f32 = 3.0;
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 192_000;
/// Cap stored live waveform buckets (~20 Hz × 600 s = 12,000 peaks ≈ 48 KB).
const MAX_LIVE_PEAKS: usize = 12_000;
/// Synthetic tone device id used by automated tests without a physical mic.
pub const SYNTHETIC_TEST_DEVICE: &str = "synthetic:tone";

/// One audio capture device discovered on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioInputDevice {
    /// Device identifier passed back to `StartAudioRecording { device }`.
    pub id: String,
    /// Human-readable label shown in the UI and MCP responses.
    pub name: String,
    /// Capture backend associated with this device (`pulse`, `pipewire`, `alsa`, `ffmpeg`, `auto`).
    pub backend: String,
    /// True when this is the OS default recording input.
    pub is_default: bool,
    /// True for desktop loopback / monitor sources rather than physical microphones.
    pub is_monitor: bool,
}

/// Recording capabilities reported by `system_capabilities` and `list_audio_inputs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingCapabilities {
    pub available: bool,
    pub backends: Vec<String>,
}

/// Parameters for starting a recording session.
#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// Device id (`None` or `"default"` uses the OS default microphone).
    pub device: Option<String>,
    /// Sample rate in Hz (default 48,000).
    pub sample_rate: u32,
    /// Channel count: 1 = mono (recommended for voiceover), 2 = stereo.
    pub channels: u16,
    /// Linear input gain multiplier (`0.05..=3.0`, default `0.65`).
    pub input_gain: f32,
    /// Enable studio voice processing (real-time rumble filter + auto-leveling +
    /// soft limiter during capture, plus broadcast denoise/EQ/loudness polish on stop).
    pub studio_voice: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            device: None,
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            input_gain: DEFAULT_INPUT_GAIN,
            studio_voice: true,
        }
    }
}

/// Real-time audio levels and progress updated by the capture worker thread.
#[derive(Debug, Clone, Default)]
pub struct LiveMetrics {
    pub peak: f32,
    pub rms: f32,
    pub db: f32,
    pub clipping: bool,
    /// True when the raw microphone signal (before our soft limiter) was very hot.
    pub input_hot: bool,
    pub frames_written: u64,
    pub elapsed_secs: f64,
    pub waveform_peaks: Vec<f32>,
    pub error: Option<String>,
}

/// Summary returned when a recording stops cleanly.
#[derive(Debug, Clone)]
pub struct RecordingResult {
    pub output_path: PathBuf,
    pub duration_secs: f64,
    pub frames_written: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub backend: String,
    pub studio_polished: bool,
}

/// Probe which capture backends are installed on PATH.
pub fn capabilities() -> RecordingCapabilities {
    let mut backends = Vec::new();
    #[cfg(target_os = "linux")]
    {
        for tool in ["parecord", "pw-record", "arecord", "ffmpeg"] {
            if crate::which(tool) {
                backends.push(tool.to_string());
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        if crate::which("ffmpeg") {
            backends.push("ffmpeg".to_string());
        }
    }
    RecordingCapabilities {
        available: !backends.is_empty(),
        backends,
    }
}

/// Enumerate available audio input devices on the host.
///
/// Always includes `"default"` ("System Default Microphone") first so callers
/// can track the OS default mic automatically, followed by concrete hardware
/// microphones (and desktop monitors at the end).
pub fn list_input_devices() -> Vec<AudioInputDevice> {
    let mut out = vec![AudioInputDevice {
        id: "default".into(),
        name: "System Default Microphone".into(),
        backend: "auto".into(),
        is_default: true,
        is_monitor: false,
    }];

    #[cfg(target_os = "linux")]
    {
        let mut discovered = list_pulse_sources();
        if discovered.is_empty() {
            discovered = list_alsa_capture_devices();
        }
        out.extend(discovered);
    }

    out
}

#[cfg(target_os = "linux")]
fn list_pulse_sources() -> Vec<AudioInputDevice> {
    if !crate::which("pactl") {
        return Vec::new();
    }
    let default_source = Command::new("pactl")
        .arg("get-default-source")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let output = match Command::new("pactl")
        .args(["list", "sources"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };
    parse_pactl_sources(&output, default_source.as_deref())
}

#[cfg(target_os = "linux")]
fn list_alsa_capture_devices() -> Vec<AudioInputDevice> {
    if !crate::which("arecord") {
        return Vec::new();
    }
    let output = match Command::new("arecord")
        .arg("-l")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };
    parse_arecord_devices(&output)
}

/// Parse `pactl list sources` output into sorted [`AudioInputDevice`] entries.
/// Physical microphones come before `.monitor` loopback sources, and the default
/// source is placed first within its group.
pub fn parse_pactl_sources(stdout: &str, default_source: Option<&str>) -> Vec<AudioInputDevice> {
    let mut devices = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_desc: Option<String> = None;

    let flush = |devices: &mut Vec<AudioInputDevice>,
                 name: &mut Option<String>,
                 desc: &mut Option<String>| {
        if let Some(id) = name.take() {
            let is_monitor = id.ends_with(".monitor");
            let is_default = default_source.map(|d| d == id).unwrap_or(false);
            let mut label = desc.take().unwrap_or_else(|| id.clone());
            if is_monitor && !label.to_ascii_lowercase().contains("monitor") {
                label.push_str(" (Desktop Monitor)");
            }
            devices.push(AudioInputDevice {
                id,
                name: label,
                backend: "pulse".into(),
                is_default,
                is_monitor,
            });
        } else {
            *desc = None;
        }
    };

    for raw in stdout.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with("Source #") {
            flush(&mut devices, &mut cur_name, &mut cur_desc);
        } else if let Some(rest) = trimmed.strip_prefix("Name:") {
            cur_name = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("Description:") {
            cur_desc = Some(rest.trim().to_string());
        }
    }
    flush(&mut devices, &mut cur_name, &mut cur_desc);

    devices.sort_by_key(|d| (d.is_monitor, !d.is_default, d.name.to_ascii_lowercase()));
    devices
}

/// Parse `arecord -l` output into ALSA `plughw:card,dev` capture devices.
pub fn parse_arecord_devices(stdout: &str) -> Vec<AudioInputDevice> {
    let mut devices = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("card ") {
            continue;
        }
        let Some(card_rest) = trimmed.strip_prefix("card ") else {
            continue;
        };
        let Some((card_num_str, after_card)) = card_rest.split_once(':') else {
            continue;
        };
        let Ok(card_num) = card_num_str.trim().parse::<u32>() else {
            continue;
        };
        let Some((_, dev_part)) = after_card.split_once("device ") else {
            continue;
        };
        let Some((dev_num_str, dev_rest)) = dev_part.split_once(':') else {
            continue;
        };
        let Ok(dev_num) = dev_num_str.trim().parse::<u32>() else {
            continue;
        };
        let card_label = after_card
            .split_once('[')
            .and_then(|(_, r)| r.split_once(']'))
            .map(|(inside, _)| inside.trim())
            .unwrap_or("ALSA");
        let dev_label = dev_rest
            .split_once('[')
            .map(|(before, _)| before.trim())
            .unwrap_or(dev_rest.trim());
        let id = format!("plughw:{card_num},{dev_num}");
        let name = if dev_label.is_empty() {
            format!("{card_label} ({id})")
        } else {
            format!("{card_label} — {dev_label} ({id})")
        };
        devices.push(AudioInputDevice {
            id,
            name,
            backend: "alsa".into(),
            is_default: devices.is_empty(),
            is_monitor: false,
        });
    }
    devices
}

/// Build the 44-byte RIFF/WAVE PCM header for signed 16-bit little-endian audio.
pub fn wav_header_bytes(sample_rate: u32, channels: u16, data_bytes: u32) -> [u8; 44] {
    let bits_per_sample: u16 = 16;
    let block_align: u16 = channels * (bits_per_sample / 8);
    let byte_rate: u32 = sample_rate * block_align as u32;
    let riff_size: u32 = 36u32.saturating_add(data_bytes);

    let mut hdr = [0u8; 44];
    hdr[0..4].copy_from_slice(b"RIFF");
    hdr[4..8].copy_from_slice(&riff_size.to_le_bytes());
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    hdr[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM format = 1
    hdr[22..24].copy_from_slice(&channels.to_le_bytes());
    hdr[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    hdr[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    hdr[32..34].copy_from_slice(&block_align.to_le_bytes());
    hdr[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    hdr
}

/// Compute `(peak, rms, db, clipping)` for a chunk of `s16le` PCM samples.
pub fn analyze_s16le_chunk(bytes: &[u8]) -> (f32, f32, f32, bool) {
    let (chunks, _) = bytes.as_chunks::<2>();
    if chunks.is_empty() {
        return (0.0, 0.0, -96.0, false);
    }
    let mut peak = 0.0_f32;
    let mut sum_sq = 0.0_f64;
    for &pair in chunks {
        let s = i16::from_le_bytes(pair) as f32 / 32_768.0;
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (s as f64) * (s as f64);
    }
    let peak = peak.clamp(0.0, 1.0);
    let rms = ((sum_sq / chunks.len() as f64).sqrt() as f32).clamp(0.0, 1.0);
    let db = if peak > 1e-5 {
        (20.0 * peak.log10()).clamp(-96.0, 0.0)
    } else {
        -96.0
    };
    let clipping = peak >= 0.96;
    (peak, rms, db, clipping)
}

// ---------------------------------------------------------------------------
// Real-time studio input DSP (high-pass rumble filter + auto-level + limiter)
// ---------------------------------------------------------------------------

/// 2nd-order Butterworth high-pass biquad (80 Hz) per channel.
#[derive(Debug, Clone)]
struct BiquadHp {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BiquadHp {
    fn new(sample_rate: u32, cutoff_hz: f64) -> Self {
        let sr = sample_rate.max(8_000) as f64;
        let w0 = 2.0 * std::f64::consts::PI * cutoff_hz / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / std::f64::consts::SQRT_2; // Q = 0.7071 (Butterworth)
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 + cos_w0) * 0.5) / a0,
            b1: -(1.0 + cos_w0) / a0,
            b2: ((1.0 + cos_w0) * 0.5) / a0,
            a1: (-2.0 * cos_w0) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let xd = x as f64;
        let y = self.b0 * xd + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = xd;
        self.y2 = self.y1;
        self.y1 = y;
        y as f32
    }
}

/// Zero-latency real-time broadcast voiceover input strip.
///
/// Applied to every incoming `f32` audio sample before writing to disk:
/// 1. 80 Hz 2nd-order high-pass filter (removes desk thumps, AC hum, plosives)
/// 2. Live user input gain (`input_gain`)
/// 3. Smooth downward auto-leveler (when `studio_voice` is enabled) so hot
///    microphones settle into the clean `-12..-6 dBFS` sweet spot
/// 4. Transparent soft-knee limiter + `-1.5 dBFS` (`0.84`) ceiling so loud
///    peaks NEVER cause harsh digital clipping
pub struct StudioInputDsp {
    hp: Vec<BiquadHp>,
    channels: usize,
    studio_voice: bool,
    /// Envelope follower for the transparent peak limiter.
    lim_env: f32,
    lim_attack: f32,
    lim_release: f32,
    /// Smooth downward auto-leveler gain (<= 1.0) to tame overly hot mics.
    auto_gain: f32,
    auto_attack: f32,
    auto_release: f32,
}

impl StudioInputDsp {
    pub fn new(sample_rate: u32, channels: u16, studio_voice: bool) -> Self {
        let ch = channels.clamp(1, 2) as usize;
        let sr = sample_rate.max(8_000) as f32;
        let hp = (0..ch).map(|_| BiquadHp::new(sample_rate, 80.0)).collect();
        Self {
            hp,
            channels: ch,
            studio_voice,
            lim_env: 0.0,
            lim_attack: (-1.0 / (0.001 * sr)).exp(),    // 1 ms fast catch
            lim_release: (-1.0 / (0.065 * sr)).exp(),   // 65 ms smooth release
            auto_gain: 1.0,
            auto_attack: (-1.0 / (0.015 * sr)).exp(),   // 15 ms duck on hot mic
            auto_release: (-1.0 / (0.600 * sr)).exp(),  // 600 ms gentle recovery
        }
    }

    /// Process interleaved `f32` frames into signed 16-bit PCM (`s16le`) bytes.
    /// Returns `(input_hot, processed_peak, processed_rms, processed_db, clipping)`.
    pub fn process_f32_to_s16le(
        &mut self,
        samples: &[f32],
        input_gain: f32,
        out_s16le: &mut Vec<u8>,
    ) -> (bool, f32, f32, f32, bool) {
        out_s16le.clear();
        out_s16le.reserve(samples.len() * 2);

        let gain = input_gain.clamp(MIN_INPUT_GAIN, MAX_INPUT_GAIN);
        let mut raw_peak = 0.0_f32;
        let mut out_peak = 0.0_f32;
        let mut sum_sq = 0.0_f64;

        // Soft-knee limiter ceiling: -1.5 dBFS (0.84) when Studio Voice is ON,
        // or gentle safety ceiling -0.5 dBFS (0.94) when Studio Voice is OFF.
        let ceiling = if self.studio_voice { 0.84_f32 } else { 0.94_f32 };
        let auto_target = 0.52_f32; // ~ -5.7 dBFS sweet spot for hot mics

        for frame in samples.chunks_exact(self.channels) {
            let mut filtered = [0.0_f32; 2];
            let mut frame_abs = 0.0_f32;
            for c in 0..self.channels {
                let raw = frame[c];
                raw_peak = raw_peak.max(raw.abs());
                let x = if self.studio_voice {
                    self.hp[c].process(raw) * gain
                } else {
                    raw * gain
                };
                filtered[c] = x;
                frame_abs = frame_abs.max(x.abs());
            }

            // Gentle downward auto-leveler when Studio Voice is enabled so
            // overly hot mics ride smoothly into the green/gold sweet spot.
            if self.studio_voice {
                let desired_auto = if frame_abs > auto_target {
                    (auto_target / frame_abs).clamp(0.20, 1.0)
                } else {
                    1.0
                };
                let coeff = if desired_auto < self.auto_gain {
                    self.auto_attack
                } else {
                    self.auto_release
                };
                self.auto_gain = coeff * self.auto_gain + (1.0 - coeff) * desired_auto;
            } else {
                self.auto_gain = 1.0;
            }

            let leveled_abs = frame_abs * self.auto_gain;

            // Fast look-behind peak envelope follower for transparent limiting.
            let lim_coeff = if leveled_abs > self.lim_env {
                self.lim_attack
            } else {
                self.lim_release
            };
            self.lim_env = lim_coeff * self.lim_env + (1.0 - lim_coeff) * leveled_abs;

            let lim_gain = if self.lim_env > ceiling {
                ceiling / self.lim_env
            } else {
                1.0
            };

            let total_scale = self.auto_gain * lim_gain;
            for &s_in in &filtered[..self.channels] {
                let scaled = s_in * total_scale;
                // Smooth tanh soft-clip safety at the ceiling so transient spikes
                // never wrap or produce harsh digital flat-topping.
                let soft = ceiling * (scaled / ceiling).tanh();
                let a = soft.abs();
                if a > out_peak {
                    out_peak = a;
                }
                sum_sq += (soft as f64) * (soft as f64);

                let s16 = (soft * 32_767.0).round().clamp(-32_768.0, 32_767.0) as i16;
                out_s16le.extend_from_slice(&s16.to_le_bytes());
            }
        }

        let count = samples.len().max(1);
        let peak = out_peak.clamp(0.0, 1.0);
        let rms = ((sum_sq / count as f64).sqrt() as f32).clamp(0.0, 1.0);
        let db = if peak > 1e-5 {
            (20.0 * peak.log10()).clamp(-96.0, 0.0)
        } else {
            -96.0
        };
        let input_hot = raw_peak >= 0.92;
        let clipping = peak >= 0.95;
        (input_hot, peak, rms, db, clipping)
    }
}

/// FFmpeg broadcast voiceover mastering filtergraph:
/// - 80 Hz high-pass (sub-bass rumble / plosive thump removal)
/// - Gentle FFT noise reduction (`afftdn`, 10 dB reduction, auto noise tracking)
/// - Warm vocal clarity EQ (cut 240 Hz mud, lift 3.2 kHz presence + 10 kHz air)
/// - Smooth vocal compressor (3:1, soft knee)
/// - EBU R128 broadcast loudness normalization (-16 LUFS, -1.5 dBTP)
/// - Final transparent limiter ceiling (-1.7 dBFS so peaks stay crisp & never red-clip)
pub fn studio_voice_filtergraph() -> &'static str {
    "highpass=f=80:poles=2,\
     afftdn=nr=10:nf=-32:tn=1,\
     equalizer=f=240:t=q:w=1.2:g=-2.0,\
     equalizer=f=3200:t=q:w=1.2:g=2.2,\
     equalizer=f=10000:t=h:w=1.0:g=1.2,\
     acompressor=threshold=-20dB:ratio=3:attack=8:release=120:makeup=2:knee=6,\
     loudnorm=I=-16:TP=-1.5:LRA=11,\
     alimiter=limit=0.82:attack=5:release=50:level=false"
}

/// Apply the FFmpeg broadcast voiceover polish pass in-place on `wav_path`.
/// Falls back gracefully (returning `false`) if FFmpeg is unavailable or fails
/// so a recorded take is never lost.
pub fn polish_voiceover_wav(wav_path: &Path, sample_rate: u32, channels: u16) -> bool {
    if !crate::which("ffmpeg") || !wav_path.is_file() {
        return false;
    }
    let tmp_out = wav_path.with_extension("polished.wav");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
        ])
        .arg(wav_path)
        .args([
            "-af",
            studio_voice_filtergraph(),
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&tmp_out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() && tmp_out.is_file() => {
            if std::fs::rename(&tmp_out, wav_path).is_ok() {
                return true;
            }
            let _ = std::fs::remove_file(&tmp_out);
            false
        }
        _ => {
            let _ = std::fs::remove_file(&tmp_out);
            false
        }
    }
}

/// Raw capture sample encoding returned by a capture subprocess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSampleFormat {
    F32Le,
    S16Le,
}

/// Candidate capture command specification (`(backend_label, binary, args, format)`).
pub fn build_capture_candidates(
    device: Option<&str>,
    sample_rate: u32,
    channels: u16,
) -> Vec<(&'static str, &'static str, Vec<String>, CaptureSampleFormat)> {
    let dev = device
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default");
    let sr = sample_rate.to_string();
    let ch = channels.to_string();

    let mut out: Vec<(&'static str, &'static str, Vec<String>, CaptureSampleFormat)> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        let is_alsa_dev = dev
            .map(|d| d.starts_with("plughw:") || d.starts_with("hw:") || d.starts_with("default:"))
            .unwrap_or(false);

        let parecord_f32_args = || {
            let mut a = vec![
                "--raw".into(),
                "--format=float32le".into(),
                format!("--rate={sr}"),
                format!("--channels={ch}"),
                "--latency-msec=30".into(),
            ];
            if let Some(d) = dev {
                a.push(format!("--device={d}"));
            }
            a
        };

        let pw_record_f32_args = || {
            let mut a = vec![
                "--raw".into(),
                "--format".into(),
                "f32".into(),
                "--rate".into(),
                sr.clone(),
                "--channels".into(),
                ch.clone(),
                "--latency".into(),
                "30ms".into(),
            ];
            if let Some(d) = dev {
                a.push("--target".into());
                a.push(d.to_string());
            }
            a.push("-".into());
            a
        };

        let arecord_s16_args = || {
            let mut a = vec![
                "-q".into(),
                "-t".into(),
                "raw".into(),
                "-f".into(),
                "S16_LE".into(),
                "-r".into(),
                sr.clone(),
                "-c".into(),
                ch.clone(),
            ];
            if let Some(d) = dev {
                a.push("-D".into());
                a.push(d.to_string());
            }
            a
        };

        let ffmpeg_pulse_f32_args = || {
            vec![
                "-hide_banner".into(),
                "-loglevel".into(),
                "error".into(),
                "-f".into(),
                "pulse".into(),
                "-i".into(),
                dev.unwrap_or("default").to_string(),
                "-f".into(),
                "f32le".into(),
                "-acodec".into(),
                "pcm_f32le".into(),
                "-ar".into(),
                sr.clone(),
                "-ac".into(),
                ch.clone(),
                "pipe:1".into(),
            ]
        };

        let ffmpeg_alsa_f32_args = || {
            vec![
                "-hide_banner".into(),
                "-loglevel".into(),
                "error".into(),
                "-f".into(),
                "alsa".into(),
                "-i".into(),
                dev.unwrap_or("default").to_string(),
                "-f".into(),
                "f32le".into(),
                "-acodec".into(),
                "pcm_f32le".into(),
                "-ar".into(),
                sr.clone(),
                "-ac".into(),
                ch.clone(),
                "pipe:1".into(),
            ]
        };

        if is_alsa_dev {
            out.push(("arecord", "arecord", arecord_s16_args(), CaptureSampleFormat::S16Le));
            out.push(("ffmpeg-alsa", "ffmpeg", ffmpeg_alsa_f32_args(), CaptureSampleFormat::F32Le));
        } else {
            out.push(("parecord", "parecord", parecord_f32_args(), CaptureSampleFormat::F32Le));
            out.push(("pw-record", "pw-record", pw_record_f32_args(), CaptureSampleFormat::F32Le));
            out.push(("arecord", "arecord", arecord_s16_args(), CaptureSampleFormat::S16Le));
            out.push(("ffmpeg-pulse", "ffmpeg", ffmpeg_pulse_f32_args(), CaptureSampleFormat::F32Le));
            out.push(("ffmpeg-alsa", "ffmpeg", ffmpeg_alsa_f32_args(), CaptureSampleFormat::F32Le));
        }
    }

    #[cfg(target_os = "macos")]
    {
        let input = match dev {
            Some(d) if d.starts_with(':') => d.to_string(),
            Some(d) => format!(":{d}"),
            None => ":0".to_string(),
        };
        out.push((
            "ffmpeg-avfoundation",
            "ffmpeg",
            vec![
                "-hide_banner".into(),
                "-loglevel".into(),
                "error".into(),
                "-f".into(),
                "avfoundation".into(),
                "-i".into(),
                input,
                "-f".into(),
                "f32le".into(),
                "-acodec".into(),
                "pcm_f32le".into(),
                "-ar".into(),
                sr,
                "-ac".into(),
                ch,
                "pipe:1".into(),
            ],
            CaptureSampleFormat::F32Le,
        ));
    }

    #[cfg(target_os = "windows")]
    {
        let input = format!("audio={}", dev.unwrap_or("default"));
        out.push((
            "ffmpeg-dshow",
            "ffmpeg",
            vec![
                "-hide_banner".into(),
                "-loglevel".into(),
                "error".into(),
                "-f".into(),
                "dshow".into(),
                "-i".into(),
                input,
                "-f".into(),
                "f32le".into(),
                "-acodec".into(),
                "pcm_f32le".into(),
                "-ar".into(),
                sr,
                "-ac".into(),
                ch,
                "pipe:1".into(),
            ],
            CaptureSampleFormat::F32Le,
        ));
    }

    out
}

/// Active live audio recording session writing a 16-bit PCM WAV file.
pub struct RecordingSession {
    output_path: PathBuf,
    sample_rate: u32,
    channels: u16,
    studio_voice: bool,
    backend: String,
    stop: Arc<AtomicBool>,
    live_gain: Arc<AtomicU32>,
    child_pid: Arc<Mutex<Option<u32>>>,
    metrics: Arc<Mutex<LiveMetrics>>,
    worker: Option<JoinHandle<Result<u64, String>>>,
    finalized: bool,
}

impl RecordingSession {
    /// Start recording live audio to `output_path` (`.wav`).
    pub fn start(output_path: PathBuf, opts: RecordOptions) -> Result<Self, MediaError> {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&opts.sample_rate) {
            return Err(MediaError::BadData(format!(
                "sample_rate {} out of range ({MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE})",
                opts.sample_rate
            )));
        }
        if opts.channels == 0 || opts.channels > 2 {
            return Err(MediaError::BadData(format!(
                "channels {} unsupported (expected 1 or 2)",
                opts.channels
            )));
        }
        if !opts.input_gain.is_finite()
            || !(MIN_INPUT_GAIN..=MAX_INPUT_GAIN).contains(&opts.input_gain)
        {
            return Err(MediaError::BadData(format!(
                "input_gain {} out of range ({MIN_INPUT_GAIN}..={MAX_INPUT_GAIN})",
                opts.input_gain
            )));
        }
        if let Some(parent) = output_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let live_gain = Arc::new(AtomicU32::new(opts.input_gain.to_bits()));
        let metrics = Arc::new(Mutex::new(LiveMetrics {
            db: -96.0,
            ..Default::default()
        }));
        let child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        if opts.device.as_deref() == Some(SYNTHETIC_TEST_DEVICE) {
            let mut file = File::create(&output_path)?;
            file.write_all(&wav_header_bytes(opts.sample_rate, opts.channels, 0))?;
            let sr = opts.sample_rate;
            let ch = opts.channels;
            let studio = opts.studio_voice;
            let stop_w = stop.clone();
            let gain_w = live_gain.clone();
            let metrics_w = metrics.clone();
            let worker = std::thread::spawn(move || {
                synthetic_capture_loop(file, sr, ch, studio, stop_w, gain_w, metrics_w)
            });
            return Ok(Self {
                output_path,
                sample_rate: opts.sample_rate,
                channels: opts.channels,
                studio_voice: opts.studio_voice,
                backend: "synthetic".into(),
                stop,
                live_gain,
                child_pid,
                metrics,
                worker: Some(worker),
                finalized: false,
            });
        }

        let (backend, mut child, sample_fmt) = spawn_live_capture(&opts)?;
        if let Ok(mut slot) = child_pid.lock() {
            *slot = Some(child.id());
        }
        let mut file = File::create(&output_path)?;
        file.write_all(&wav_header_bytes(opts.sample_rate, opts.channels, 0))?;

        let sr = opts.sample_rate;
        let ch = opts.channels;
        let studio = opts.studio_voice;
        let stop_w = stop.clone();
        let gain_w = live_gain.clone();
        let metrics_w = metrics.clone();
        let pid_w = child_pid.clone();
        let stdout = child.stdout.take().ok_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            MediaError::BadData("capture backend did not expose stdout pipe".into())
        })?;

        let worker = std::thread::spawn(move || {
            let res = subprocess_capture_loop(
                file, stdout, sr, ch, sample_fmt, studio, stop_w, gain_w, metrics_w,
            );
            let _ = child.kill();
            let _ = child.wait();
            if let Ok(mut slot) = pid_w.lock() {
                *slot = None;
            }
            res
        });

        Ok(Self {
            output_path,
            sample_rate: opts.sample_rate,
            channels: opts.channels,
            studio_voice: opts.studio_voice,
            backend: backend.to_string(),
            stop,
            live_gain,
            child_pid,
            metrics,
            worker: Some(worker),
            finalized: false,
        })
    }

    pub fn backend(&self) -> &str {
        &self.backend
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Cheap clone of the shared live metrics handle (for 60fps UI polling).
    pub fn metrics_handle(&self) -> Arc<Mutex<LiveMetrics>> {
        self.metrics.clone()
    }

    /// Shared atomic input gain so the UI can nudge Mic Level live during capture.
    pub fn live_gain_handle(&self) -> Arc<AtomicU32> {
        self.live_gain.clone()
    }

    /// Snapshot current VU levels and elapsed duration.
    pub fn metrics(&self) -> LiveMetrics {
        self.metrics
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    /// Stop recording, finalize the WAV header, optionally run Studio Voice
    /// mastering, and return recording metadata.
    pub fn stop(mut self) -> Result<RecordingResult, MediaError> {
        self.finalized = true;
        self.stop.store(true, Ordering::Relaxed);
        self.terminate_child();
        let frames_written = match self.worker.take() {
            Some(handle) => match handle.join() {
                Ok(Ok(frames)) => frames,
                Ok(Err(msg)) => return Err(MediaError::BadData(msg)),
                Err(_) => {
                    return Err(MediaError::BadData(
                        "audio capture worker panicked".into(),
                    ))
                }
            },
            None => 0,
        };

        let studio_polished = if self.studio_voice && frames_written > 0 {
            polish_voiceover_wav(&self.output_path, self.sample_rate, self.channels)
        } else {
            false
        };

        let duration_secs = frames_written as f64 / self.sample_rate.max(1) as f64;
        Ok(RecordingResult {
            output_path: self.output_path.clone(),
            duration_secs,
            frames_written,
            sample_rate: self.sample_rate,
            channels: self.channels,
            backend: self.backend.clone(),
            studio_polished,
        })
    }

    /// Stop recording and delete the temporary `.wav` file.
    pub fn discard(mut self) {
        self.finalized = true;
        self.stop.store(true, Ordering::Relaxed);
        self.terminate_child();
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.output_path);
    }

    fn terminate_child(&self) {
        #[cfg(unix)]
        {
            if let Ok(slot) = self.child_pid.lock() {
                if let Some(pid) = *slot {
                    // Send SIGTERM so the capture process closes stdout cleanly
                    // and unblocks any pending read() in the worker thread.
                    let _ = Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
    }
}

impl Drop for RecordingSession {
    fn drop(&mut self) {
        if !self.finalized {
            self.stop.store(true, Ordering::Relaxed);
            self.terminate_child();
            if let Some(handle) = self.worker.take() {
                let _ = handle.join();
            }
        }
    }
}

fn spawn_live_capture(
    opts: &RecordOptions,
) -> Result<(&'static str, Child, CaptureSampleFormat), MediaError> {
    let candidates =
        build_capture_candidates(opts.device.as_deref(), opts.sample_rate, opts.channels);
    let mut errors: Vec<String> = Vec::new();

    for (label, bin, args, fmt) in candidates {
        if !crate::which(bin) {
            continue;
        }
        let mut child = match Command::new(bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{label}: {e}"));
                continue;
            }
        };

        // Give the backend a short window to fail fast (e.g. missing PulseAudio
        // socket or busy ALSA device) so we can fall back cleanly to the next one.
        std::thread::sleep(Duration::from_millis(90));
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut err_txt = String::new();
                if let Some(mut se) = child.stderr.take() {
                    let _ = se.read_to_string(&mut err_txt);
                }
                let detail = err_txt
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("exited immediately");
                errors.push(format!("{label} ({status}): {detail}"));
                continue;
            }
            Ok(None) => {
                return Ok((label, child, fmt));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                errors.push(format!("{label}: {e}"));
            }
        }
    }

    if errors.is_empty() {
        Err(MediaError::BadData(
            "no audio recording backend found on PATH (install PipeWire/PulseAudio `parecord`/`pw-record`, ALSA `arecord`, or `ffmpeg`)".into(),
        ))
    } else {
        Err(MediaError::BadData(format!(
            "failed to start audio recording ({})",
            errors.join("; ")
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn subprocess_capture_loop<R: Read>(
    file: File,
    mut reader: R,
    sample_rate: u32,
    channels: u16,
    sample_fmt: CaptureSampleFormat,
    studio_voice: bool,
    stop: Arc<AtomicBool>,
    live_gain: Arc<AtomicU32>,
    metrics: Arc<Mutex<LiveMetrics>>,
) -> Result<u64, String> {
    let in_bytes_per_sample = match sample_fmt {
        CaptureSampleFormat::F32Le => 4usize,
        CaptureSampleFormat::S16Le => 2usize,
    };
    let in_bytes_per_frame = (channels as usize) * in_bytes_per_sample;
    let out_bytes_per_frame = (channels as usize) * 2;
    // ~50ms chunks (20 Hz meter & waveform update rate).
    let chunk_frames = ((sample_rate as usize) / 20).max(256);
    let chunk_bytes = chunk_frames * in_bytes_per_frame;

    let mut dsp = StudioInputDsp::new(sample_rate, channels, studio_voice);
    let mut writer = BufWriter::new(file);
    let mut buf = vec![0u8; chunk_bytes];
    let mut carry: Vec<u8> = Vec::with_capacity(in_bytes_per_frame);
    let mut f32_samples: Vec<f32> = Vec::with_capacity(chunk_frames * channels as usize);
    let mut s16_out: Vec<u8> = Vec::with_capacity(chunk_frames * out_bytes_per_frame);
    let mut total_out_bytes: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut combined = Vec::with_capacity(carry.len() + n);
                combined.extend_from_slice(&carry);
                combined.extend_from_slice(&buf[..n]);
                carry.clear();

                let rem = combined.len() % in_bytes_per_frame;
                let usable = combined.len() - rem;
                if rem > 0 {
                    carry.extend_from_slice(&combined[usable..]);
                }
                if usable == 0 {
                    continue;
                }

                let slice = &combined[..usable];
                f32_samples.clear();
                match sample_fmt {
                    CaptureSampleFormat::F32Le => {
                        let (chunks, _) = slice.as_chunks::<4>();
                        for &b in chunks {
                            let v = f32::from_le_bytes(b);
                            f32_samples.push(if v.is_finite() { v } else { 0.0 });
                        }
                    }
                    CaptureSampleFormat::S16Le => {
                        let (chunks, _) = slice.as_chunks::<2>();
                        for &b in chunks {
                            f32_samples.push(i16::from_le_bytes(b) as f32 / 32_768.0);
                        }
                    }
                }

                let gain = f32::from_bits(live_gain.load(Ordering::Relaxed));
                let (input_hot, peak, rms, db, clipping) =
                    dsp.process_f32_to_s16le(&f32_samples, gain, &mut s16_out);

                if let Err(e) = writer.write_all(&s16_out) {
                    let msg = format!("failed writing recorded WAV: {e}");
                    if let Ok(mut m) = metrics.lock() {
                        m.error = Some(msg.clone());
                    }
                    return Err(msg);
                }
                total_out_bytes = total_out_bytes.saturating_add(s16_out.len() as u64);
                let frames_written = total_out_bytes / out_bytes_per_frame as u64;
                if let Ok(mut m) = metrics.lock() {
                    m.peak = peak;
                    m.rms = rms;
                    m.db = db;
                    m.clipping = clipping;
                    m.input_hot = input_hot;
                    m.frames_written = frames_written;
                    m.elapsed_secs = frames_written as f64 / sample_rate as f64;
                    if m.waveform_peaks.len() < MAX_LIVE_PEAKS {
                        m.waveform_peaks.push(peak);
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let msg = format!("audio capture read error: {e}");
                if let Ok(mut m) = metrics.lock() {
                    m.error = Some(msg.clone());
                }
                break;
            }
        }
    }

    finalize_wav_writer(writer, sample_rate, channels, total_out_bytes)?;
    Ok(total_out_bytes / out_bytes_per_frame as u64)
}

fn synthetic_capture_loop(
    file: File,
    sample_rate: u32,
    channels: u16,
    studio_voice: bool,
    stop: Arc<AtomicBool>,
    live_gain: Arc<AtomicU32>,
    metrics: Arc<Mutex<LiveMetrics>>,
) -> Result<u64, String> {
    let out_bytes_per_frame = (channels as usize) * 2;
    let chunk_frames = ((sample_rate as usize) / 25).max(256); // 40ms chunks
    let chunk_dur = Duration::from_secs_f64(chunk_frames as f64 / sample_rate as f64);

    let mut dsp = StudioInputDsp::new(sample_rate, channels, studio_voice);
    let mut writer = BufWriter::new(file);
    let mut f32_samples = Vec::with_capacity(chunk_frames * channels as usize);
    let mut s16_out = Vec::with_capacity(chunk_frames * out_bytes_per_frame);
    let mut total_frames: u64 = 0;
    let started = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        f32_samples.clear();
        for i in 0..chunk_frames {
            let t = (total_frames + i as u64) as f64 / sample_rate as f64;
            let sample = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.45) as f32;
            for _ in 0..channels {
                f32_samples.push(sample);
            }
        }
        let gain = f32::from_bits(live_gain.load(Ordering::Relaxed));
        let (input_hot, peak, rms, db, clipping) =
            dsp.process_f32_to_s16le(&f32_samples, gain, &mut s16_out);
        writer
            .write_all(&s16_out)
            .map_err(|e| format!("failed writing synthetic WAV: {e}"))?;
        total_frames += chunk_frames as u64;
        if let Ok(mut m) = metrics.lock() {
            m.peak = peak;
            m.rms = rms;
            m.db = db;
            m.clipping = clipping;
            m.input_hot = input_hot;
            m.frames_written = total_frames;
            m.elapsed_secs = total_frames as f64 / sample_rate as f64;
            if m.waveform_peaks.len() < MAX_LIVE_PEAKS {
                m.waveform_peaks.push(peak);
            }
        }

        let target_elapsed = Duration::from_secs_f64(total_frames as f64 / sample_rate as f64);
        if let Some(sleep_for) = target_elapsed.checked_sub(started.elapsed()) {
            std::thread::sleep(sleep_for.min(chunk_dur));
        }
    }

    let total_data_bytes = total_frames * out_bytes_per_frame as u64;
    finalize_wav_writer(writer, sample_rate, channels, total_data_bytes)?;
    Ok(total_frames)
}

fn finalize_wav_writer(
    mut writer: BufWriter<File>,
    sample_rate: u32,
    channels: u16,
    total_data_bytes: u64,
) -> Result<(), String> {
    writer
        .flush()
        .map_err(|e| format!("flush WAV failed: {e}"))?;
    let mut file = writer
        .into_inner()
        .map_err(|e| format!("WAV finalize failed: {e}"))?;
    let clamped_bytes = total_data_bytes.min(u32::MAX as u64 - 36) as u32;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("WAV header seek failed: {e}"))?;
    file.write_all(&wav_header_bytes(sample_rate, channels, clamped_bytes))
        .map_err(|e| format!("WAV header write failed: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("WAV sync failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_valid_riff_pcm() {
        let hdr = wav_header_bytes(48_000, 1, 96_000);
        assert_eq!(&hdr[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(hdr[4..8].try_into().unwrap()), 96_036);
        assert_eq!(&hdr[8..12], b"WAVE");
        assert_eq!(&hdr[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes(hdr[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(hdr[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(hdr[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(hdr[28..32].try_into().unwrap()), 96_000);
        assert_eq!(u16::from_le_bytes(hdr[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(hdr[34..36].try_into().unwrap()), 16);
        assert_eq!(&hdr[36..40], b"data");
        assert_eq!(u32::from_le_bytes(hdr[40..44].try_into().unwrap()), 96_000);
    }

    #[test]
    fn analyze_s16le_chunk_detects_silence_level_and_clipping() {
        let silent = vec![0u8; 64];
        let (peak, rms, db, clip) = analyze_s16le_chunk(&silent);
        assert_eq!(peak, 0.0);
        assert_eq!(rms, 0.0);
        assert_eq!(db, -96.0);
        assert!(!clip);

        let mut hot = Vec::new();
        for _ in 0..32 {
            hot.extend_from_slice(&32_767i16.to_le_bytes());
        }
        let (peak, rms, db, clip) = analyze_s16le_chunk(&hot);
        assert!(peak > 0.99);
        assert!(rms > 0.99);
        assert!(db > -0.1);
        assert!(clip);
    }

    #[test]
    fn studio_dsp_tames_overdriven_hot_mic_below_red_clipping() {
        let mut dsp = StudioInputDsp::new(48_000, 1, true);
        // Simulate a screaming hot microphone (0.98 amplitude 440 Hz tone).
        let samples: Vec<f32> = (0..4_800)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.98) as f32
            })
            .collect();
        let mut out = Vec::new();
        let (input_hot, peak, _rms, _db, clipping) =
            dsp.process_f32_to_s16le(&samples, 1.0, &mut out);
        assert!(input_hot, "should flag raw mic signal as hot");
        assert!(!clipping, "soft limiter must prevent harsh red clipping");
        assert!(
            peak <= 0.85,
            "studio ceiling should keep peak below 0.85, got {peak}"
        );
    }

    #[test]
    fn studio_dsp_highpass_attenuates_low_frequency_rumble() {
        let mut dsp = StudioInputDsp::new(48_000, 1, true);
        // Feed 25 Hz desk rumble (well below 80 Hz cutoff) after settling.
        let rumble: Vec<f32> = (0..24_000)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                ((2.0 * std::f64::consts::PI * 25.0 * t).sin() * 0.5) as f32
            })
            .collect();
        let mut out = Vec::new();
        let _ = dsp.process_f32_to_s16le(&rumble[..12_000], 1.0, &mut out);
        let (_, peak, _, _, _) = dsp.process_f32_to_s16le(&rumble[12_000..], 1.0, &mut out);
        assert!(
            peak < 0.08,
            "25 Hz rumble should be heavily attenuated by 80 Hz HPF, got {peak}"
        );
    }

    #[test]
    fn parse_pactl_sources_orders_default_mic_first_and_monitors_last() {
        let sample = "\
Source #41
\tState: SUSPENDED
\tName: alsa_output.pci.analog-stereo.monitor
\tDescription: Monitor of Built-in Audio Analog Stereo
Source #42
\tState: IDLE
\tName: alsa_input.usb-Rode_NT-USB-00.mono-fallback
\tDescription: RODE NT-USB Mono
Source #43
\tState: SUSPENDED
\tName: alsa_input.pci.analog-stereo
\tDescription: Built-in Microphone
";
        let devs = parse_pactl_sources(sample, Some("alsa_input.usb-Rode_NT-USB-00.mono-fallback"));
        assert_eq!(devs.len(), 3);
        assert_eq!(devs[0].id, "alsa_input.usb-Rode_NT-USB-00.mono-fallback");
        assert!(devs[0].is_default);
        assert!(!devs[0].is_monitor);
        assert_eq!(devs[1].id, "alsa_input.pci.analog-stereo");
        assert!(!devs[1].is_default);
        assert!(!devs[1].is_monitor);
        assert_eq!(devs[2].id, "alsa_output.pci.analog-stereo.monitor");
        assert!(devs[2].is_monitor);
    }

    #[test]
    fn parse_arecord_devices_extracts_plughw_ids() {
        let sample = "\
**** List of CAPTURE Hardware Devices ****
card 1: sofhdadsp [sof-hda-dsp], device 0: HDA Analog (*) []
  Subdevices: 1/1
card 1: sofhdadsp [sof-hda-dsp], device 6: DMIC (*) []
  Subdevices: 1/1
";
        let devs = parse_arecord_devices(sample);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].id, "plughw:1,0");
        assert!(devs[0].is_default);
        assert_eq!(devs[1].id, "plughw:1,6");
        assert!(!devs[1].is_default);
    }

    #[test]
    fn synthetic_recording_session_produces_valid_probed_polished_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = tmp.path().join("vo-take.wav");
        let session = RecordingSession::start(
            wav.clone(),
            RecordOptions {
                device: Some(SYNTHETIC_TEST_DEVICE.into()),
                sample_rate: 48_000,
                channels: 1,
                input_gain: DEFAULT_INPUT_GAIN,
                studio_voice: true,
            },
        )
        .expect("start synthetic recording");

        std::thread::sleep(Duration::from_millis(240));
        let live = session.metrics();
        assert!(live.frames_written > 0);
        assert!(live.peak > 0.15 && live.peak <= 0.85);
        assert!(!live.clipping);
        assert!(!live.waveform_peaks.is_empty());

        let result = session.stop().expect("stop recording");
        assert_eq!(result.output_path, wav);
        assert!(result.frames_written > 4_000);
        assert!(result.duration_secs > 0.1);
        assert!(result.studio_polished);
        assert!(wav.is_file());

        let probed = crate::ffprobe::probe(&wav).expect("ffprobe recorded wav");
        assert_eq!(probed.kind, cutpilot_core::AssetKind::Audio);
        assert!(probed.probe.has_audio);
        assert_eq!(probed.probe.sample_rate, Some(48_000));
        assert!(probed.probe.duration_ticks > 0);
    }

    #[test]
    fn discard_removes_recorded_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = tmp.path().join("discarded.wav");
        let session = RecordingSession::start(
            wav.clone(),
            RecordOptions {
                device: Some(SYNTHETIC_TEST_DEVICE.into()),
                sample_rate: 48_000,
                channels: 1,
                input_gain: DEFAULT_INPUT_GAIN,
                studio_voice: true,
            },
        )
        .expect("start synthetic recording");
        std::thread::sleep(Duration::from_millis(80));
        session.discard();
        assert!(!wav.exists());
    }
}
