//! ffprobe wrapper: JSON metadata extraction for any importable media file.

use crate::{MediaError, FMT};
use cutpilot_core::{AssetKind, Fps, MediaProbe, Ticks, TICKS_PER_SEC};
use std::path::Path;
use std::process::Command;

/// Probe result plus the resolved asset kind.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub probe: MediaProbe,
    pub kind: AssetKind,
}

#[derive(serde::Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<Stream>,
    #[serde(default)]
    format: Option<Format>,
}

#[derive(serde::Deserialize)]
struct Stream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    duration: Option<String>,
    duration_ts: Option<u64>,
    sample_rate: Option<String>,
    tags: Option<StreamTags>,
    side_data_list: Option<Vec<SideData>>,
    disposition: Option<StreamDisposition>,
}

#[derive(serde::Deserialize)]
struct StreamDisposition {
    #[serde(default)]
    attached_pic: Option<u8>,
}

#[derive(serde::Deserialize)]
struct StreamTags {
    rotate: Option<String>,
}

#[derive(serde::Deserialize)]
struct SideData {
    rotation: Option<f64>,
}

#[derive(serde::Deserialize)]
struct Format {
    duration: Option<String>,
    format_name: Option<String>,
}

const IMAGE_CODECS: &[&str] = &[
    "mjpeg", "mjpegb", "png", "bmp", "gif", "tiff", "webp", "jpeg", "jpg", "jpegls",
];

/// Run ffprobe and parse the result.
pub fn probe(path: &Path) -> Result<ProbeResult, MediaError> {
    let safe_path = crate::safe_local_input(path)?;
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-protocol_whitelist",
            crate::LOCAL_FILE_PROTOCOL_WHITELIST,
            "-i",
        ])
        .arg(&safe_path)
        .output()
        .map_err(|e| MediaError::Probe {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(MediaError::Probe {
            path: path.display().to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout).map_err(|e| {
        MediaError::Probe {
            path: path.display().to_string(),
            message: format!("unparseable ffprobe output: {e}"),
        }
    })?;
    parse_probe(path, parsed)
}

fn parse_probe(path: &Path, parsed: FfprobeOutput) -> Result<ProbeResult, MediaError> {
    let bad = |msg: String| MediaError::Probe {
        path: path.display().to_string(),
        message: msg,
    };
    // Ignore embedded album art / cover pictures (`attached_pic == 1`) so audio
    // files (WAV/MP3/FLAC/M4A) with cover art are not misclassified as images.
    let video = parsed.streams.iter().find(|s| {
        s.codec_type.as_deref() == Some("video")
            && s.disposition.as_ref().and_then(|d| d.attached_pic) != Some(1)
    });
    let audio = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    let format_duration = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0);

    let kind = match (video, audio) {
        (None, Some(_)) => AssetKind::Audio,
        (Some(v), _) => {
            let codec = v.codec_name.as_deref().unwrap_or("");
            let format_is_image = parsed
                .format
                .as_ref()
                .and_then(|f| f.format_name.as_deref())
                .map(|n| n.contains("image2") || n.contains("pipe"))
                .unwrap_or(false);
            let stream_duration = v.duration.as_deref().and_then(|d| d.parse::<f64>().ok());
            let looks_still = IMAGE_CODECS.contains(&codec)
                && (format_is_image || stream_duration.is_none());
            if looks_still {
                AssetKind::Image
            } else {
                AssetKind::Video
            }
        }
        (None, None) => {
            return Err(bad("no audio or video streams found".into()));
        }
    };

    let sample_rate: Option<u32> = audio
        .and_then(|a| a.sample_rate.as_deref())
        .and_then(|s| s.parse().ok());

    let duration_secs = match kind {
        AssetKind::Image => None,
        _ => format_duration
            .or_else(|| {
                video
                    .and_then(|v| v.duration.as_deref())
                    .and_then(|d| d.parse::<f64>().ok())
                    .filter(|d| d.is_finite() && *d > 0.0)
            })
            .or_else(|| {
                audio
                    .and_then(|a| a.duration.as_deref())
                    .and_then(|d| d.parse::<f64>().ok())
                    .filter(|d| d.is_finite() && *d > 0.0)
            })
            .or_else(|| {
                let ts = audio.and_then(|a| a.duration_ts)?;
                let sr = sample_rate?;
                (sr > 0 && ts > 0).then(|| ts as f64 / sr as f64)
            }),
    };
    let duration_ticks = duration_secs
        .map(|d| (d * TICKS_PER_SEC as f64).round() as Ticks)
        .unwrap_or(0);

    let (width, height) = match video {
        Some(v) => (v.width.unwrap_or(0), v.height.unwrap_or(0)),
        None => (0, 0),
    };

    let fps = video
        .and_then(|v| v.r_frame_rate.as_deref())
        .and_then(parse_fraction)
        .filter(|f| f.as_f64() > 1.0);

    let rotation_deg = video
        .map(rotation_from_stream)
        .unwrap_or(0);

    let probe = MediaProbe {
        duration_ticks,
        width,
        height,
        fps,
        has_audio: audio.is_some(),
        rotation_deg,
        video_codec: video.and_then(|v| v.codec_name.clone()),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
        sample_rate,
    };
    Ok(ProbeResult { probe, kind })
}

fn rotation_from_stream(v: &Stream) -> u32 {
    if let Some(side) = v.side_data_list.as_deref() {
        for sd in side {
            if let Some(rot) = sd.rotation {
                return normalize_rotation(rot);
            }
        }
    }
    if let Some(tags) = v.tags.as_ref() {
        if let Some(rot) = tags.rotate.as_deref() {
            if let Ok(r) = rot.parse::<f64>() {
                return normalize_rotation(r);
            }
        }
    }
    0
}

fn normalize_rotation(deg: f64) -> u32 {
    let d = deg.round() as i64;
    let d = ((d % 360) + 360) % 360;
    d as u32
}

fn parse_fraction(s: &str) -> Option<Fps> {
    let mut parts = s.splitn(2, '/');
    let num: u32 = parts.next()?.parse().ok()?;
    let den: u32 = match parts.next() {
        Some(d) => d.parse().unwrap_or(1),
        None => 1,
    };
    if num == 0 || den == 0 {
        return None;
    }
    Some(Fps::new(num, den))
}

/// Duration of a file in ticks, or an error.
pub fn duration_ticks(path: &Path) -> Result<Ticks, MediaError> {
    Ok(probe(path)?.probe.duration_ticks)
}

/// Validate an exported file: duration within tolerance, expected size, and
/// an audio stream present.
pub fn verify_output(
    path: &Path,
    expected_ticks: Ticks,
    expected_size: (u32, u32),
) -> Result<Ticks, MediaError> {
    let result = probe(path)?;
    let probe = result.probe;
    if probe.width != expected_size.0 || probe.height != expected_size.1 {
        return Err(MediaError::Ffmpeg {
            message: format!(
                "output is {}x{} but project is {}x{}",
                probe.width, probe.height, expected_size.0, expected_size.1
            ),
        });
    }
    if !probe.has_audio {
        return Err(MediaError::Ffmpeg {
            message: "output has no audio stream".into(),
        });
    }
    let tolerance = TICKS_PER_SEC / 4; // 250 ms
    if expected_ticks > 0 && (probe.duration_ticks - expected_ticks).abs() > tolerance {
        return Err(MediaError::Ffmpeg {
            message: format!(
                "output duration is {} but timeline duration is {}",
                FMT(probe.duration_ticks),
                FMT(expected_ticks)
            ),
        });
    }
    Ok(probe.duration_ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wav_as_audio() {
        let json = r#"{
            "streams": [{
                "codec_type": "audio",
                "codec_name": "pcm_s16le",
                "sample_rate": "44100",
                "duration": "3.500000",
                "duration_ts": 154350
            }],
            "format": {
                "format_name": "wav",
                "duration": "3.500000"
            }
        }"#;
        let parsed: FfprobeOutput = serde_json::from_str(json).unwrap();
        let res = parse_probe(Path::new("/tmp/voice.wav"), parsed).unwrap();
        assert_eq!(res.kind, AssetKind::Audio);
        assert!(res.probe.has_audio);
        assert_eq!(res.probe.width, 0);
        assert_eq!(res.probe.height, 0);
        assert_eq!(res.probe.audio_codec.as_deref(), Some("pcm_s16le"));
        assert_eq!(res.probe.sample_rate, Some(44_100));
        assert_eq!(res.probe.duration_ticks, cutpilot_core::ticks_from_secs(3.5));
    }

    #[test]
    fn audio_with_cover_art_stays_audio() {
        let json = r#"{
            "streams": [
                {
                    "codec_type": "audio",
                    "codec_name": "pcm_s24le",
                    "sample_rate": "48000",
                    "duration": "10.000000"
                },
                {
                    "codec_type": "video",
                    "codec_name": "mjpeg",
                    "width": 500,
                    "height": 500,
                    "disposition": { "attached_pic": 1 }
                }
            ],
            "format": { "format_name": "wav", "duration": "10.000000" }
        }"#;
        let parsed: FfprobeOutput = serde_json::from_str(json).unwrap();
        let res = parse_probe(Path::new("/tmp/track.wav"), parsed).unwrap();
        assert_eq!(res.kind, AssetKind::Audio);
        assert!(res.probe.has_audio);
        assert_eq!(res.probe.width, 0);
        assert_eq!(res.probe.height, 0);
    }

    #[test]
    fn wav_duration_falls_back_to_duration_ts() {
        let json = r#"{
            "streams": [{
                "codec_type": "audio",
                "codec_name": "pcm_f32le",
                "sample_rate": "48000",
                "duration_ts": 96000
            }],
            "format": { "format_name": "wav" }
        }"#;
        let parsed: FfprobeOutput = serde_json::from_str(json).unwrap();
        let res = parse_probe(Path::new("/tmp/float.wav"), parsed).unwrap();
        assert_eq!(res.kind, AssetKind::Audio);
        assert_eq!(res.probe.duration_ticks, cutpilot_core::ticks_from_secs(2.0));
    }
}
