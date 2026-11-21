//! The export engine: builds a single deterministic ffmpeg filtergraph from
//! the project document and runs it on NVIDIA GPU (zero-copy NVDEC → CUDA →
//! NVENC when eligible, or NVDEC + NVENC for complex graphs) with automatic
//! software fallback.
//!
//! Compositing policy:
//! - The bottom-most video track is the base: clips are trimmed, letterboxed
//!   to the project canvas, and concatenated. Gaps between base clips collapse.
//! - Higher video tracks composite on top with alpha and clip transforms.
//! - Audio-track clips mix cleanly; linked video+audio pairs use the audio
//!   track clip as authoritative so volume, mute, and fades apply without
//!   doubling the audio.
//! - Captions burn in via ASS; their times remap through the gap-collapse map.

use crate::{ass, ffprobe, fmt_secs, gpu, process, MediaError};
use cutpilot_core::{AssetKind, Caption, Clip, Easing, Fps, Project, Ticks, TrackKind};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    #[default]
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" | "x264" | "h264_nvenc" => Ok(Self::H264),
            "hevc" | "h265" | "x265" | "hevc_nvenc" => Ok(Self::Hevc),
            "av1" | "av1_nvenc" => Ok(Self::Av1),
            other => Err(format!(
                "unsupported codec `{other}` (expected h264, hevc, or av1)"
            )),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportPreset {
    Fast,
    #[default]
    Balanced,
    Quality,
}

impl ExportPreset {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "fast" | "speed" => Ok(Self::Fast),
            "balanced" | "medium" | "default" => Ok(Self::Balanced),
            "quality" | "hq" | "high" => Ok(Self::Quality),
            other => Err(format!(
                "unsupported preset `{other}` (expected fast, balanced, or quality)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// `None` = auto-detect NVENC, `Some(false)` forces software encoding.
    pub use_nvenc: Option<bool>,
    /// `None` = use CUDA decode when NVENC is used. Forced off on fallback.
    pub hwaccel: Option<bool>,
    /// `None` = allow zero-copy GPU-resident CUDA pipeline when eligible.
    pub gpu_resident: Option<bool>,
    pub video_bitrate_mbps: Option<f64>,
    pub audio_bitrate_kbps: u32,
    pub codec: VideoCodec,
    pub preset: ExportPreset,
    /// Optional output width in pixels (even). `None` uses project width (or scales with `height`).
    pub width: Option<u32>,
    /// Optional output height in pixels (even). `None` uses project height (or scales with `width`).
    pub height: Option<u32>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            use_nvenc: None,
            hwaccel: None,
            gpu_resident: None,
            video_bitrate_mbps: None,
            audio_bitrate_kbps: 256,
            codec: VideoCodec::H264,
            preset: ExportPreset::Balanced,
            width: None,
            height: None,
        }
    }
}

/// A fully resolved ffmpeg invocation.
#[derive(Debug, Clone)]
pub struct ExportPlan {
    pub args: Vec<String>,
    pub expected_duration_ticks: Ticks,
    pub video_codec: &'static str,
    pub uses_hwaccel: bool,
    pub gpu_resident: bool,
    pub width: u32,
    pub height: u32,
}

/// Verified result of a completed export.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExportStats {
    pub output_path: PathBuf,
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
    pub gpu_resident: bool,
    pub fallback_used: bool,
    pub expected_duration_ticks: Ticks,
    pub actual_duration_ticks: Ticks,
}

/// Resolve and validate the output dimensions for an export.
/// - Both `None`: uses the project's canvas size.
/// - Both `Some`: validates both are even and within `16..=7680`.
/// - Only one `Some`: scales the other dimension proportionally to preserve the
///   project's aspect ratio (rounded to an even pixel count).
pub fn resolve_export_dimensions(
    project: &Project,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<(u32, u32), String> {
    let check = |val: u32, name: &str| -> Result<u32, String> {
        if !(16..=7680).contains(&val) {
            return Err(format!("export {name} {val} must be between 16 and 7680"));
        }
        if !val.is_multiple_of(2) {
            return Err(format!("export {name} {val} must be an even number"));
        }
        Ok(val)
    };
    let pw = project.settings.width.max(2) as f64;
    let ph = project.settings.height.max(2) as f64;
    match (width, height) {
        (None, None) => Ok((project.settings.width, project.settings.height)),
        (Some(w), Some(h)) => Ok((check(w, "width")?, check(h, "height")?)),
        (Some(w), None) => {
            let w = check(w, "width")?;
            let h = (((w as f64 * (ph / pw)).round() as u32).clamp(16, 7680)) & !1;
            Ok((w, h))
        }
        (None, Some(h)) => {
            let h = check(h, "height")?;
            let w = (((h as f64 * (pw / ph)).round() as u32).clamp(16, 7680)) & !1;
            Ok((w, h))
        }
    }
}

/// YouTube-ish default bitrate: ~45 Mbps at 4K30, scaled by pixels*fps,
/// clamped to 2..=120 Mbps.
pub fn default_bitrate_mbps_for(width: u32, height: u32, fps: cutpilot_core::Fps) -> f64 {
    let pixels = (width as f64) * (height as f64);
    let base_pixels = 3840.0 * 2160.0;
    let br = 45.0 * (pixels * fps.as_f64()) / (base_pixels * 30.0);
    br.clamp(2.0, 120.0)
}

/// Default bitrate for the project's native canvas resolution.
pub fn default_bitrate_mbps(project: &Project) -> f64 {
    default_bitrate_mbps_for(
        project.settings.width,
        project.settings.height,
        project.settings.fps,
    )
}

enum InputSpec {
    /// Seekable media: `[hwaccel] -ss start -t dur -i path`.
    Media {
        path: PathBuf,
        seek: Option<(f64, f64)>,
        hw_cuda: bool,
        cuda_surfaces: bool,
    },
    /// Still image: `-loop 1 -t dur -i path`.
    Still { path: PathBuf, dur: f64 },
    /// Generated solid black canvas for audio-only timelines.
    Color {
        width: u32,
        height: u32,
        fps: String,
        dur: f64,
    },
    /// Generated silence: `-f lavfi -t dur -i anullsrc=...`.
    Silence { dur: f64, sample_rate: u32 },
}

impl InputSpec {
    fn args(&self) -> Vec<String> {
        match self {
            InputSpec::Media {
                path,
                seek,
                hw_cuda,
                cuda_surfaces,
            } => {
                let mut args = Vec::new();
                if *hw_cuda {
                    args.push("-hwaccel".into());
                    args.push("cuda".into());
                    if *cuda_surfaces {
                        args.push("-hwaccel_output_format".into());
                        args.push("cuda".into());
                    }
                }
                if let Some((start, dur)) = seek {
                    args.push("-ss".into());
                    args.push(fmt_secs(*start));
                    args.push("-t".into());
                    args.push(fmt_secs(*dur));
                }
                args.push("-i".into());
                args.push(path.display().to_string());
                args
            }
            InputSpec::Still { path, dur } => vec![
                "-loop".into(),
                "1".into(),
                "-t".into(),
                fmt_secs(*dur),
                "-i".into(),
                path.display().to_string(),
            ],
            InputSpec::Color {
                width,
                height,
                fps,
                dur,
            } => vec![
                "-f".into(),
                "lavfi".into(),
                "-t".into(),
                fmt_secs(*dur),
                "-i".into(),
                format!("color=c=black:s={width}x{height}:r={fps}"),
            ],
            InputSpec::Silence { dur, sample_rate } => vec![
                "-f".into(),
                "lavfi".into(),
                "-t".into(),
                fmt_secs(*dur),
                "-i".into(),
                format!("anullsrc=r={sample_rate}:cl=stereo"),
            ],
        }
    }
}

struct Builder {
    inputs: Vec<InputSpec>,
    filter_parts: Vec<String>,
    next_label: usize,
}

impl Builder {
    fn new() -> Self {
        Self {
            inputs: Vec::new(),
            filter_parts: Vec::new(),
            next_label: 0,
        }
    }

    fn add_input(&mut self, spec: InputSpec) -> usize {
        let index = self.inputs.len();
        self.inputs.push(spec);
        index
    }

    fn label(&mut self, prefix: &str) -> String {
        let l = format!("{prefix}{}", self.next_label);
        self.next_label += 1;
        l
    }

    fn filter(&mut self, f: String) {
        self.filter_parts.push(f);
    }
}

fn resolve_encoder(codec: VideoCodec, use_nvenc: bool) -> &'static str {
    if use_nvenc {
        let caps = gpu::capabilities();
        match codec {
            VideoCodec::H264 => "h264_nvenc",
            VideoCodec::Hevc => {
                if caps.nvenc_hevc {
                    "hevc_nvenc"
                } else {
                    "h264_nvenc"
                }
            }
            VideoCodec::Av1 => {
                if caps.nvenc_av1 {
                    "av1_nvenc"
                } else if caps.nvenc_hevc {
                    "hevc_nvenc"
                } else {
                    "h264_nvenc"
                }
            }
        }
    } else {
        match codec {
            VideoCodec::H264 => "libx264",
            VideoCodec::Hevc => "libx265",
            VideoCodec::Av1 => "libx264",
        }
    }
}

/// Build the full ffmpeg plan without running anything. Pure + unit-tested.
pub fn build_export_plan(
    project: &Project,
    out: &Path,
    ass_path: Option<&Path>,
    opts: &ExportOptions,
) -> Result<ExportPlan, String> {
    let (settings_w, settings_h) = resolve_export_dimensions(project, opts.width, opts.height)?;
    let fps = &project.settings.fps;
    let fps_arg = if fps.den == 1 {
        format!("fps={}", fps.num)
    } else {
        format!("fps={}/{}", fps.num, fps.den)
    };
    let sr = project.settings.audio_sample_rate;

    let base_track = project.base_video_track();
    let base_clips: Vec<&Clip> = base_track
        .map(|t| project.clips_on_track(&t.id))
        .unwrap_or_default();

    let audio_only_end: Ticks = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio && !t.muted && t.volume > 0.0)
        .flat_map(|t| project.clips_on_track(&t.id))
        .filter(|c| c.volume > 0.0)
        .map(|c| c.timeline_end())
        .max()
        .unwrap_or(0);

    if base_clips.is_empty() && audio_only_end <= 0 {
        return Err(
            "the timeline has no clips; insert media before exporting".into(),
        );
    }
    let gap_map = GapMap::new_for_fps(&base_clips, *fps);
    let expected_duration_ticks: Ticks = if base_clips.is_empty() {
        audio_only_end
    } else {
        gap_map.total
    };

    let bitrate = opts
        .video_bitrate_mbps
        .unwrap_or_else(|| default_bitrate_mbps_for(settings_w, settings_h, *fps));
    let nvenc = opts.use_nvenc.unwrap_or_else(detect_nvenc);
    let video_codec = resolve_encoder(opts.codec, nvenc);
    let hwaccel = opts.hwaccel.unwrap_or(nvenc);

    let base_track_id = base_track.map(|t| t.id);
    let overlay_tracks: Vec<_> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video && Some(t.id) != base_track_id && !t.hidden)
        .collect();
    let has_overlays = overlay_tracks
        .iter()
        .any(|t| !project.clips_on_track(&t.id).is_empty());
    let has_transitions = !project.transitions.is_empty();
    let has_captions = ass_path.is_some();

    // Check if every base clip can stay 100% resident on CUDA VRAM (NVDEC →
    // scale_cuda/pad_cuda → concat → NVENC) with zero PCIe readbacks.
    let all_base_cuda_eligible = !base_clips.is_empty()
        && base_clips.iter().all(|clip| {
            !clip.has_video_effects()
                && project
                    .asset(&clip.asset_id)
                    .map(|a| {
                        a.kind == AssetKind::Video
                            && a.probe
                                .as_ref()
                                .map(|p| {
                                    p.rotation_deg == 0
                                        && gpu::nvdec_supported_codec(p.video_codec.as_deref())
                                        && p.fps
                                            .map(|f| (f.as_f64() - fps.as_f64()).abs() < 0.05)
                                            .unwrap_or(false)
                                })
                                .unwrap_or(false)
                    })
                    .unwrap_or(false)
        });

    let want_gpu_resident = opts.gpu_resident.unwrap_or_else(|| gpu::capabilities().cuda_filters);
    let gpu_resident = nvenc
        && hwaccel
        && want_gpu_resident
        && !has_overlays
        && !has_transitions
        && !has_captions
        && all_base_cuda_eligible;

    // Collect all clips on Audio tracks so we never double-mix a base video
    // clip's embedded audio when an Audio-track clip already covers it (either
    // via a shared `link_group`, or when the user unlinked a video/audio pair
    // to move or boost its audio on another track like A4).
    let audio_track_ids: HashSet<Uuid> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .map(|t| t.id)
        .collect();
    let audio_track_clips: Vec<&Clip> = project
        .clips
        .iter()
        .filter(|c| audio_track_ids.contains(&c.track_id))
        .collect();
    let linked_audio_groups: HashSet<Uuid> = audio_track_clips
        .iter()
        .filter_map(|c| c.link_group)
        .collect();

    let mut b = Builder::new();

    // --- base video segments ---
    let mut base_labels: Vec<String> = Vec::new();
    let mut audio_labels: Vec<String> = Vec::new();
    if base_clips.is_empty() {
        // Audio-only timeline: generate a black video canvas + silent audio bed
        // matching the full audio duration so WAV/audio-only timelines export cleanly.
        let dur = cutpilot_core::secs_from_ticks(expected_duration_ticks);
        let color_fps = if fps.den == 1 {
            format!("{}", fps.num)
        } else {
            format!("{}/{}", fps.num, fps.den)
        };
        let idx = b.add_input(InputSpec::Color {
            width: settings_w,
            height: settings_h,
            fps: color_fps,
            dur,
        });
        let label = b.label("v");
        b.filter(format!("[{idx}:v]setsar=1,{fps_arg}[{label}]"));
        base_labels.push(label);

        let sil = b.add_input(InputSpec::Silence {
            dur,
            sample_rate: sr,
        });
        let audio_label = b.label("a");
        b.filter(format!(
            "[{sil}:a]aresample={sr},aformat=sample_fmts=fltp:channel_layouts=stereo[{audio_label}]"
        ));
        audio_labels.push(audio_label);
    }
    let sx = settings_w as f64 / project.settings.width.max(1) as f64;
    let sy = settings_h as f64 / project.settings.height.max(1) as f64;

    for (seg_idx, clip) in base_clips.iter().enumerate() {
        let asset = project
            .asset(&clip.asset_id)
            .ok_or_else(|| format!("clip {} references a missing asset", clip.asset_id))?;
        let clip_frames = gap_map.segment_frames(seg_idx);
        let export_ticks = gap_map.segment_export_ticks(seg_idx);
        let export_secs = cutpilot_core::secs_from_ticks(export_ticks);
        let speed = clip.effective_speed();
        // Give the input demuxer a small cushion (+0.08s) so exact frame/sample
        // trimming in the filtergraph never comes up 1 frame short on VFR/HEVC.
        let src_read_dur = (export_secs * speed as f64) + 0.08;
        let clip_nvdec = hwaccel
            && asset.kind == AssetKind::Video
            && asset
                .probe
                .as_ref()
                .map(|p| gpu::nvdec_supported_codec(p.video_codec.as_deref()))
                .unwrap_or(false);
        let input = match asset.kind {
            AssetKind::Image => InputSpec::Still {
                path: asset.path.clone(),
                dur: export_secs,
            },
            _ => InputSpec::Media {
                path: asset.path.clone(),
                seek: Some((cutpilot_core::secs_from_ticks(clip.source_in), src_read_dur)),
                hw_cuda: clip_nvdec,
                cuda_surfaces: gpu_resident,
            },
        };
        let idx = b.add_input(input);
        let label = b.label("v");

        let exact_canvas_match = clip.crop.is_empty()
            && asset
                .probe
                .as_ref()
                .map(|p| p.oriented_size() == (settings_w, settings_h))
                .unwrap_or(false);

        if gpu_resident {
            // Pure GPU-resident VRAM pipeline: zero CPU copies, exact frame count!
            let chain = if exact_canvas_match {
                format!(
                    "[{idx}:v]scale_cuda={w}:{h}:format=nv12:passthrough=1,\
                     trim=end_frame={clip_frames},setpts=PTS-STARTPTS[{label}]",
                    w = settings_w,
                    h = settings_h,
                )
            } else {
                format!(
                    "[{idx}:v]scale_cuda={w}:{h}:force_original_aspect_ratio=decrease:\
                     force_divisible_by=2:format=nv12,\
                     pad_cuda={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,\
                     trim=end_frame={clip_frames},setpts=PTS-STARTPTS[{label}]",
                    w = settings_w,
                    h = settings_h,
                )
            };
            b.filter(chain);
        } else {
            let mut chain = format!("[{idx}:v]");
            if asset.kind != AssetKind::Image && (speed - 1.0).abs() > 0.001 {
                chain.push_str(&format!("setpts=(PTS-STARTPTS)/{speed:.6}"));
            } else {
                chain.push_str("setpts=PTS-STARTPTS");
            }
            append_crop_flip(&mut chain, clip);
            if exact_canvas_match {
                chain.push_str(",setsar=1");
            } else {
                chain.push_str(&format!(
                    ",scale={w}:{h}:force_original_aspect_ratio=decrease,\
                     pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1",
                    w = settings_w,
                    h = settings_h,
                ));
            }
            if clip.rotation_deg.abs() > 0.01 {
                let rad = (clip.rotation_deg as f64).to_radians();
                chain.push_str(&format!(",rotate={rad:.6}:fillcolor=black"));
            }
            append_static_zoom_pan(&mut chain, clip, settings_w, settings_h, sx, sy);
            chain.push(',');
            chain.push_str(&fps_arg);
            if let Some(motion) = &clip.motion {
                append_motion_zoompan(&mut chain, motion, settings_w, settings_h, *fps, export_secs);
            }
            append_color_grade(&mut chain, clip);
            chain.push_str(&fade_filters_for_clip(project, clip, export_secs));
            // Enforce exact frame count per segment so multi-clip V1 concat
            // never accumulates +10..30 ms/clip lip-sync drift against audio tracks.
            chain.push_str(&format!(
                ",tpad=stop_mode=clone:stop_duration=0.1,trim=end_frame={clip_frames},setpts=PTS-STARTPTS[{label}]"
            ));
            b.filter(chain);
        }
        base_labels.push(label);

        // Segment audio: only pull embedded audio from the base video clip if
        // no Audio-track clip represents it (neither via shared `link_group`
        // nor as an unlinked clip of the same asset overlapping this window).
        let covered_by_audio_track = base_clip_covered_by_audio_track(
            clip,
            &linked_audio_groups,
            &audio_track_clips,
        );
        let has_audio = !covered_by_audio_track
            && asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false);
        let audio_label = b.label("a");
        if has_audio {
            let mut achain = format!("[{idx}:a]asetpts=PTS-STARTPTS");
            let atempo = atempo_chain(speed);
            if !atempo.is_empty() {
                achain.push(',');
                achain.push_str(&atempo);
            }
            achain.push_str(&format!(
                ",aresample={sr},aformat=sample_fmts=fltp:channel_layouts=stereo,\
                 apad,atrim=0:{dur},asetpts=PTS-STARTPTS[{al}]",
                dur = fmt_secs(export_secs),
                al = audio_label
            ));
            b.filter(achain);
        } else {
            let sil = b.add_input(InputSpec::Silence {
                dur: export_secs,
                sample_rate: sr,
            });
            b.filter(format!(
                "[{sil}:a]aresample={sr},aformat=sample_fmts=fltp:channel_layouts=stereo,\
                 apad,atrim=0:{dur},asetpts=PTS-STARTPTS[{al}]",
                dur = fmt_secs(export_secs),
                al = audio_label
            ));
        }
        audio_labels.push(audio_label);
    }

    // Concat base video.
    let base_final = if base_labels.len() == 1 {
        format!("[{}]", base_labels[0])
    } else {
        let joined = base_labels
            .iter()
            .map(|l| format!("[{l}]"))
            .collect::<String>();
        let out_label = b.label("cat");
        b.filter(format!(
            "{joined}concat=n={n}:v=1:a=0[{out_label}]",
            n = base_labels.len(),
        ));
        format!("[{out_label}]")
    };

    // --- overlay tracks ---
    let mut current = base_final;
    for track in overlay_tracks {
        for clip in project.clips_on_track(&track.id) {
            let asset = project
                .asset(&clip.asset_id)
                .ok_or_else(|| format!("clip {} references a missing asset", clip.asset_id))?;
            let src_dur = cutpilot_core::secs_from_ticks(clip.source_duration());
            let tl_dur = cutpilot_core::secs_from_ticks(clip.timeline_duration());
            let speed = clip.effective_speed();
            let (pw, ph) = overlay_dims(project, clip, sx, sy);
            let start = cutpilot_core::secs_from_ticks(gap_map.map(clip.timeline_start));
            let end = cutpilot_core::secs_from_ticks(gap_map.map(clip.timeline_end()));
            let input = match asset.kind {
                AssetKind::Image => InputSpec::Still {
                    path: asset.path.clone(),
                    dur: tl_dur,
                },
                _ => InputSpec::Media {
                    path: asset.path.clone(),
                    seek: Some((cutpilot_core::secs_from_ticks(clip.source_in), src_dur)),
                    hw_cuda: false,
                    cuda_surfaces: false,
                },
            };
            let idx = b.add_input(input);
            let ov_label = b.label("ov");
            let mut chain = format!("[{idx}:v]");
            if asset.kind != AssetKind::Image && (speed - 1.0).abs() > 0.001 {
                chain.push_str(&format!("setpts=(PTS-STARTPTS)/{speed:.6}"));
            } else {
                chain.push_str("setpts=PTS-STARTPTS");
            }
            append_crop_flip(&mut chain, clip);
            chain.push_str(&format!(",scale={pw}:{ph},setsar=1"));
            if clip.rotation_deg.abs() > 0.01 {
                let rad = (clip.rotation_deg as f64).to_radians();
                chain.push_str(&format!(",format=rgba,rotate={rad:.6}:fillcolor=0x00000000"));
            }
            chain.push(',');
            chain.push_str(&fps_arg);
            if let Some(motion) = &clip.motion {
                append_motion_zoompan(&mut chain, motion, pw, ph, *fps, tl_dur);
            }
            append_color_grade(&mut chain, clip);
            chain.push_str(",format=yuva420p");
            if clip.opacity < 1.0 {
                chain.push_str(&format!(
                    ",colorchannelmixer=aa={:.4}",
                    clip.opacity.clamp(0.0, 1.0)
                ));
            }
            if clip.fade_in_ticks > 0 {
                chain.push_str(&format!(
                    ",fade=t=in:st=0:d={}:alpha=1",
                    fmt_secs(cutpilot_core::secs_from_ticks(clip.fade_in_ticks))
                ));
            }
            if clip.fade_out_ticks > 0 {
                chain.push_str(&format!(
                    ",fade=t=out:st={}:d={}:alpha=1",
                    fmt_secs(
                        (tl_dur - cutpilot_core::secs_from_ticks(clip.fade_out_ticks)).max(0.0)
                    ),
                    fmt_secs(cutpilot_core::secs_from_ticks(clip.fade_out_ticks))
                ));
            }
            // Shift overlay PTS to its timeline start so `overlay=enable='between(t,start,end)'`
            // composites the overlay's actual frames at `start..end` instead of discarding them at t=0.
            if start > 0.0 {
                chain.push_str(&format!(",setpts=PTS+{}/TB", fmt_secs(start)));
            }
            chain.push_str(&format!("[{ov_label}]"));
            b.filter(chain);

            let px = ((settings_w as f64 - pw as f64) / 2.0 + clip.x_offset as f64 * sx).round() as i64;
            let py = ((settings_h as f64 - ph as f64) / 2.0 + clip.y_offset as f64 * sy).round() as i64;
            let next = b.label("mix");
            b.filter(format!(
                "{current}[{ov_label}]overlay=x={px}:y={py}:eof_action=pass:\
                 enable='between(t,{start},{end})'[{next}]"
            ));
            current = format!("[{next}]");
        }
    }

    // --- captions / final video pad ---
    let vout = "vout";
    if gpu_resident {
        // Keep CUDA surfaces directly on GPU for NVENC.
        b.filter(format!("{current}null[{vout}]"));
    } else {
        let mut final_chain = format!("{current}format=yuv420p");
        if let Some(path) = ass_path {
            final_chain.push_str(&format!(
                ",ass={}",
                escape_filter_path(&path.display().to_string())
            ));
        }
        b.filter(format!("{final_chain}[{vout}]"));
    }

    // --- audio mix ---
    let cat_label = if audio_labels.len() == 1 {
        format!("[{}]", audio_labels[0])
    } else {
        let joined = audio_labels
            .iter()
            .map(|l| format!("[{l}]"))
            .collect::<String>();
        let out = b.label("acat");
        b.filter(format!(
            "{joined}concat=n={n}:v=0:a=1[{out}]",
            n = audio_labels.len(),
        ));
        format!("[{out}]")
    };

    let mut music_labels: Vec<String> = Vec::new();
    for track in project.tracks.iter().filter(|t| t.kind == TrackKind::Audio) {
        if track.muted || track.volume <= 0.0 {
            continue;
        }
        for clip in project.clips_on_track(&track.id) {
            let asset = project
                .asset(&clip.asset_id)
                .ok_or_else(|| format!("clip {} references a missing asset", clip.asset_id))?;
            let src_dur = cutpilot_core::secs_from_ticks(clip.source_duration());
            let tl_dur = cutpilot_core::secs_from_ticks(clip.timeline_duration());
            let speed = clip.effective_speed();
            let vol = (clip.volume * track.volume).clamp(0.0, 4.0);
            if vol == 0.0 {
                continue;
            }
            let input = match asset.kind {
                AssetKind::Image => {
                    return Err("images cannot be placed on audio tracks".into());
                }
                _ => InputSpec::Media {
                    path: asset.path.clone(),
                    seek: Some((cutpilot_core::secs_from_ticks(clip.source_in), src_dur)),
                    hw_cuda: false,
                    cuda_surfaces: false,
                },
            };
            let idx = b.add_input(input);
            let label = b.label("au");
            let mut chain = format!("[{idx}:a]asetpts=PTS-STARTPTS");
            let atempo = atempo_chain(speed);
            if !atempo.is_empty() {
                chain.push(',');
                chain.push_str(&atempo);
            }
            chain.push_str(&format!(",volume={vol:.4}"));
            if clip.fade_in_ticks > 0 {
                chain.push_str(&format!(
                    ",afade=t=in:st=0:d={}",
                    fmt_secs(cutpilot_core::secs_from_ticks(clip.fade_in_ticks))
                ));
            }
            if clip.fade_out_ticks > 0 {
                chain.push_str(&format!(
                    ",afade=t=out:st={}:d={}",
                    fmt_secs(
                        (tl_dur - cutpilot_core::secs_from_ticks(clip.fade_out_ticks)).max(0.0)
                    ),
                    fmt_secs(cutpilot_core::secs_from_ticks(clip.fade_out_ticks))
                ));
            }
            let delay_ms =
                cutpilot_core::ticks::ms_from_ticks(gap_map.map(clip.timeline_start)).max(0);
            chain.push_str(&format!(
                ",aresample={sr},aformat=sample_fmts=fltp:channel_layouts=stereo,\
                 adelay=delays={delay_ms}:all=1[{label}]"
            ));
            b.filter(chain);
            music_labels.push(label);
        }
    }

    let aout = "aout";
    if music_labels.is_empty() {
        b.filter(format!("{cat_label}anull[{aout}]"));
    } else {
        let joined = std::iter::once(cat_label.clone())
            .chain(music_labels.iter().map(|l| format!("[{l}]")))
            .collect::<String>();
        b.filter(format!(
            "{joined}amix=inputs={n}:duration=first:normalize=0[{aout}]",
            n = music_labels.len() + 1,
        ));
    }

    // --- assemble argv ---
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-loglevel".into(),
        "error".into(),
        "-progress".into(),
        "pipe:1".into(),
    ];
    for input in &b.inputs {
        args.extend(input.args());
    }
    let filter_complex = b.filter_parts.join(";");
    args.push("-filter_complex".into());
    args.push(filter_complex);
    args.push("-map".into());
    args.push("[vout]".into());
    args.push("-map".into());
    args.push("[aout]".into());

    if video_codec.ends_with("_nvenc") {
        let maxrate = bitrate * 1.5;
        let bufsize = bitrate * 2.0;
        let (nv_preset, nv_tune) = match opts.preset {
            ExportPreset::Fast => ("p2", "ull"),
            ExportPreset::Balanced => ("p4", "hq"),
            ExportPreset::Quality => ("p6", "hq"),
        };
        args.extend(
            [
                "-c:v",
                video_codec,
                "-preset",
                nv_preset,
                "-tune",
                nv_tune,
                "-rc",
                "vbr",
            ]
            .map(String::from),
        );
        if opts.preset != ExportPreset::Fast {
            args.extend(["-spatial-aq".into(), "1".into()]);
        }
        if opts.preset == ExportPreset::Quality {
            args.extend(["-rc-lookahead".into(), "20".into()]);
        }
        args.push("-b:v".into());
        args.push(format!("{bitrate:.2}M"));
        args.push("-maxrate".into());
        args.push(format!("{maxrate:.2}M"));
        args.push("-bufsize".into());
        args.push(format!("{bufsize:.2}M"));
    } else {
        let maxrate = bitrate;
        let bufsize = bitrate * 2.0;
        let sw_preset = match opts.preset {
            ExportPreset::Fast => "veryfast",
            ExportPreset::Balanced => "medium",
            ExportPreset::Quality => "slow",
        };
        args.extend(["-c:v", video_codec, "-preset", sw_preset].map(String::from));
        args.push("-crf".into());
        args.push("18".into());
        args.push("-maxrate".into());
        args.push(format!("{maxrate:.2}M"));
        args.push("-bufsize".into());
        args.push(format!("{bufsize:.2}M"));
    }
    if video_codec.contains("hevc") || video_codec.contains("x265") {
        args.extend(["-tag:v".into(), "hvc1".into()]);
    }
    args.extend(["-c:a", "aac"].map(String::from));
    args.push("-b:a".into());
    args.push(format!("{}k", opts.audio_bitrate_kbps));
    args.push("-ar".into());
    args.push(format!("{sr}"));
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push("-y".into());
    args.push(out.display().to_string());

    Ok(ExportPlan {
        args,
        expected_duration_ticks,
        video_codec,
        uses_hwaccel: hwaccel,
        gpu_resident,
        width: settings_w,
        height: settings_h,
    })
}

/// Build chained ffmpeg `atempo` filters for any speed in `0.25..=8.0`.
/// Returns an empty string when `speed` is ~1.0.
pub fn atempo_chain(speed: f32) -> String {
    let mut rem = speed.clamp(0.25, 8.0) as f64;
    if (rem - 1.0).abs() <= 0.001 {
        return String::new();
    }
    let mut stages = Vec::new();
    while rem > 2.0 + 1e-6 {
        stages.push("atempo=2.0".to_string());
        rem /= 2.0;
    }
    while rem < 0.5 - 1e-6 {
        stages.push("atempo=0.5".to_string());
        rem /= 0.5;
    }
    if (rem - 1.0).abs() > 1e-4 {
        stages.push(format!("atempo={rem:.6}"));
    }
    stages.join(",")
}

/// Append normalized edge crop and H/V flips to a video filter chain.
pub(crate) fn append_crop_flip(chain: &mut String, clip: &Clip) {
    if !clip.crop.is_empty() {
        let w_frac = (1.0 - clip.crop.left - clip.crop.right).clamp(0.05, 1.0);
        let h_frac = (1.0 - clip.crop.top - clip.crop.bottom).clamp(0.05, 1.0);
        chain.push_str(&format!(
            ",crop=w='trunc(iw*{w_frac:.6}/2)*2':h='trunc(ih*{h_frac:.6}/2)*2':\
             x='trunc(iw*{:.6}/2)*2':y='trunc(ih*{:.6}/2)*2'",
            clip.crop.left, clip.crop.top
        ));
    }
    if clip.flip_h {
        chain.push_str(",hflip");
    }
    if clip.flip_v {
        chain.push_str(",vflip");
    }
}

/// Append static zoom (`clip.scale`) and pan (`clip.x_offset`, `clip.y_offset`)
/// onto a base-track canvas of `out_w x out_h`, safely supporting both zoom-in
/// and zoom-out with any offset.
pub(crate) fn append_static_zoom_pan(
    chain: &mut String,
    clip: &Clip,
    out_w: u32,
    out_h: u32,
    sx: f64,
    sy: f64,
) {
    let has_scale = (clip.scale - 1.0).abs() > 0.001;
    let dx = (clip.x_offset as f64 * sx).round() as i64;
    let dy = (clip.y_offset as f64 * sy).round() as i64;
    if !has_scale && dx == 0 && dy == 0 {
        return;
    }
    let scale = clip.scale.clamp(0.05, 20.0) as f64;
    let sw = (((out_w as f64 * scale).round() as u32).max(2)) & !1;
    let sh = (((out_h as f64 * scale).round() as u32).max(2)) & !1;
    if sw != out_w || sh != out_h {
        chain.push_str(&format!(",scale={sw}:{sh}:flags=bicubic"));
    }
    let margin_x = ((out_w as i64 - sw as i64).max(0) + 1) / 2 + dx.abs() + 4;
    let margin_y = ((out_h as i64 - sh as i64).max(0) + 1) / 2 + dy.abs() + 4;
    let pad_w = sw as i64 + 2 * margin_x;
    let pad_h = sh as i64 + 2 * margin_y;
    let cx = margin_x + (sw as i64 - out_w as i64) / 2 - dx;
    let cy = margin_y + (sh as i64 - out_h as i64) / 2 - dy;
    chain.push_str(&format!(
        ",pad={pad_w}:{pad_h}:{margin_x}:{margin_y}:color=black,\
         crop={out_w}:{out_h}:{cx}:{cy},setsar=1"
    ));
}

/// Append ffmpeg `zoompan` for animated Ken Burns / zoom & pan across a clip.
fn append_motion_zoompan(
    chain: &mut String,
    motion: &cutpilot_core::ClipMotion,
    out_w: u32,
    out_h: u32,
    fps: Fps,
    tl_dur_secs: f64,
) {
    let total_frames = (tl_dur_secs * fps.as_f64()).round().max(1.0) as u64;
    let denom = total_frames.saturating_sub(1).max(1);
    let u = format!("min(1\\,in/{denom})");
    let p = match motion.easing {
        Easing::Linear => u.clone(),
        Easing::EaseIn => format!("({u}*{u})"),
        Easing::EaseOut => format!("(1-(1-{u})*(1-{u}))"),
        Easing::EaseInOut => format!("(({u}*{u})*(3-2*{u}))"),
    };
    let ds = motion.end_scale - motion.start_scale;
    let dpx = motion.end_pan_x - motion.start_pan_x;
    let dpy = motion.end_pan_y - motion.start_pan_y;
    let z_expr = if ds.abs() < 1e-4 {
        format!("{:.4}", motion.start_scale)
    } else {
        format!("{:.4}+({ds:.4})*{p}", motion.start_scale)
    };
    let px_expr = if dpx.abs() < 1e-4 {
        format!("{:.4}", motion.start_pan_x)
    } else {
        format!("({:.4}+({dpx:.4})*{p})", motion.start_pan_x)
    };
    let py_expr = if dpy.abs() < 1e-4 {
        format!("{:.4}", motion.start_pan_y)
    } else {
        format!("({:.4}+({dpy:.4})*{p})", motion.start_pan_y)
    };
    let fps_val = if fps.den == 1 {
        format!("{}", fps.num)
    } else {
        format!("{}/{}", fps.num, fps.den)
    };
    // Oversample 2x up to 1080p for sub-pixel-smooth panning without jitter.
    if out_w <= 1920 && out_h <= 1080 {
        chain.push_str(&format!(
            ",scale={}:{}:flags=bicubic",
            out_w * 2,
            out_h * 2
        ));
    }
    chain.push_str(&format!(
        ",zoompan=z='{z_expr}':x='(iw-iw/zoom)*(0.5+0.5*{px_expr})':\
         y='(ih-ih/zoom)*(0.5+0.5*{py_expr})':d=1:s={out_w}x{out_h}:fps={fps_val},setsar=1"
    ));
}

/// Append color correction (`eq`), gaussian blur (`gblur`), and `vignette`.
pub(crate) fn append_color_grade(chain: &mut String, clip: &Clip) {
    let c = &clip.color;
    if c.has_eq() {
        chain.push_str(&format!(
            ",eq=brightness={:.4}:contrast={:.4}:saturation={:.4}:gamma={:.4}",
            c.brightness, c.contrast, c.saturation, c.gamma
        ));
    }
    if c.blur > 0.01 {
        chain.push_str(&format!(",gblur=sigma={:.3}", c.blur.clamp(0.01, 20.0)));
    }
    if c.vignette > 0.01 {
        let angle = (c.vignette * 1.1).clamp(0.05, 1.2);
        chain.push_str(&format!(",vignette=angle={angle:.4}"));
    }
}

/// Fade filters for a base clip, from transitions anchored at its edges.
fn fade_filters_for_clip(project: &Project, clip: &Clip, video_len: f64) -> String {
    let mut out = String::new();
    for tr in &project.transitions {
        let d = cutpilot_core::secs_from_ticks(tr.duration);
        if d <= 0.0 {
            continue;
        }
        let color = crate::hex_to_ffmpeg_color(&tr.color).unwrap_or_else(|_| "black".into());
        if tr.at == clip.timeline_start {
            out.push_str(&format!(",fade=t=in:st=0:d={}:c={color}", fmt_secs(d)));
        } else if tr.at == clip.timeline_end() {
            out.push_str(&format!(
                ",fade=t=out:st={}:d={}:c={color}",
                fmt_secs((video_len - d).max(0.0)),
                fmt_secs(d)
            ));
        }
    }
    out
}

fn overlay_dims(project: &Project, clip: &Clip, sx: f64, sy: f64) -> (u32, u32) {
    let asset = project.asset(&clip.asset_id);
    let (w, h) = asset
        .and_then(|a| a.probe.as_ref())
        .map(|p| p.oriented_size())
        .unwrap_or((project.settings.width, project.settings.height));
    let scale = clip.scale.clamp(0.05, 20.0) as f64;
    let ow = ((w as f64 * scale * sx).round() as u32).max(2) & !1;
    let oh = ((h as f64 * scale * sy).round() as u32).max(2) & !1;
    (ow, oh)
}

fn escape_filter_path(path: &str) -> String {
    path.replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

/// True when a base video clip's audio is already represented by a clip on an
/// Audio track — either via a shared `link_group` OR as an unlinked clip of
/// the same asset overlapping the video clip's timeline range.
fn base_clip_covered_by_audio_track(
    base_clip: &Clip,
    linked_audio_groups: &HashSet<Uuid>,
    audio_track_clips: &[&Clip],
) -> bool {
    if base_clip
        .link_group
        .map(|g| linked_audio_groups.contains(&g))
        .unwrap_or(false)
    {
        return true;
    }
    let v_start = base_clip.timeline_start;
    let v_end = base_clip.timeline_end();
    audio_track_clips.iter().any(|ac| {
        ac.asset_id == base_clip.asset_id && ac.timeline_start < v_end && v_start < ac.timeline_end()
    })
}

/// Maps timeline ticks to exact frame-quantized export ticks when base-track
/// gaps collapse. Every base segment's exported duration is an exact integer
/// multiple of `fps.frame_duration_ticks()`, using cumulative error diffusion
/// so multi-clip exports never accumulate lip-sync drift.
struct GapMap {
    /// `(timeline_start, timeline_end, export_start, export_dur, frames)`
    segments: Vec<(Ticks, Ticks, Ticks, Ticks, u64)>,
    total: Ticks,
}

impl GapMap {
    #[cfg(test)]
    fn new(base_clips: &[&Clip]) -> Self {
        Self::new_for_fps(base_clips, Fps::from_int(30))
    }

    fn new_for_fps(base_clips: &[&Clip], fps: Fps) -> Self {
        let frame_ticks = fps.frame_duration_ticks().max(1);
        let mut segments = Vec::with_capacity(base_clips.len());
        let mut ideal_cursor: Ticks = 0;
        let mut quantized_frames: u64 = 0;
        for clip in base_clips {
            let export_start = quantized_frames as Ticks * frame_ticks;
            ideal_cursor += clip.timeline_duration();
            let target_frames = ((ideal_cursor as f64 / frame_ticks as f64).round() as u64)
                .max(quantized_frames + 1);
            let seg_frames = (target_frames - quantized_frames).max(1);
            let export_dur = seg_frames as Ticks * frame_ticks;
            segments.push((
                clip.timeline_start,
                clip.timeline_end(),
                export_start,
                export_dur,
                seg_frames,
            ));
            quantized_frames += seg_frames;
        }
        Self {
            segments,
            total: quantized_frames as Ticks * frame_ticks,
        }
    }

    fn segment_frames(&self, idx: usize) -> u64 {
        self.segments.get(idx).map(|s| s.4).unwrap_or(1)
    }

    fn segment_export_ticks(&self, idx: usize) -> Ticks {
        self.segments.get(idx).map(|s| s.3).unwrap_or(0)
    }

    fn map(&self, t: Ticks) -> Ticks {
        if self.segments.is_empty() {
            return t;
        }
        for &(start, end, export_start, export_dur, _) in &self.segments {
            if t < start {
                return export_start;
            }
            if t < end {
                return export_start + (t - start).min(export_dur);
            }
        }
        self.total
    }
}

/// Remap captions into export time for the ASS burn-in.
pub fn remapped_captions(project: &Project, base_clips: &[&Clip]) -> Vec<Caption> {
    let map = GapMap::new_for_fps(base_clips, project.settings.fps);
    project
        .captions
        .iter()
        .map(|c| {
            let mut c = c.clone();
            c.start = map.map(c.start);
            c.end = map.map(c.end).max(map.map(c.start));
            c
        })
        .collect()
}

/// True when NVIDIA `h264_nvenc` hardware encoding is verified available (cached).
pub fn detect_nvenc() -> bool {
    gpu::capabilities().nvenc_h264
}

/// Run the export end to end: write the ASS sidecar, run ffmpeg with
/// progress/cancel, cascade through GPU fallbacks cleanly, and verify the
/// output against the plan.
pub fn export_project(
    project: &Project,
    out: &Path,
    opts: &ExportOptions,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<ExportStats, MediaError> {
    let base_clips: Vec<&Clip> = project
        .base_video_track()
        .map(|t| project.clips_on_track(&t.id))
        .unwrap_or_default();

    let mut ass_path: Option<PathBuf> = None;
    if !project.captions.is_empty() {
        let dir = out
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("cutpilot-ass-{}.ass", uuid::Uuid::new_v4()));
        let ass_doc = ass::generate_ass(
            &project.caption_style,
            &remapped_captions(project, &base_clips),
            project.settings.width,
            project.settings.height,
        )
        .map_err(MediaError::BadData)?;
        std::fs::write(&path, ass_doc)?;
        ass_path = Some(path);
    }
    let result = run_export_with_sidecar(
        project,
        out,
        ass_path.as_deref(),
        opts,
        progress,
        cancel,
    );
    if let Some(p) = &ass_path {
        let _ = std::fs::remove_file(p);
    }
    result
}

fn run_export_with_sidecar(
    project: &Project,
    out: &Path,
    ass_path: Option<&Path>,
    opts: &ExportOptions,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<ExportStats, MediaError> {
    let plan =
        build_export_plan(project, out, ass_path, opts).map_err(MediaError::BadData)?;
    let total_secs = cutpilot_core::secs_from_ticks(plan.expected_duration_ticks);

    let run = |plan: &ExportPlan| {
        process::run_with_progress(
            "ffmpeg",
            &plan.args,
            total_secs,
            progress.clone(),
            cancel.clone(),
        )
    };

    match run(&plan) {
        Ok(_) => finish_stats(out, &plan, false),
        Err(MediaError::Cancelled) => Err(MediaError::Cancelled),
        Err(first_error) => {
            // Tier 2: if zero-copy CUDA surfaces failed (e.g., 10-bit/HDR stream),
            // retry with NVENC encoding + CPU filtergraph (still GPU-accelerated!).
            if plan.gpu_resident {
                let hybrid_opts = ExportOptions {
                    gpu_resident: Some(false),
                    hwaccel: Some(false),
                    ..opts.clone()
                };
                if let Ok(hybrid) = build_export_plan(project, out, ass_path, &hybrid_opts) {
                    if run(&hybrid).is_ok() {
                        return finish_stats(out, &hybrid, false);
                    }
                }
            }
            // Tier 3: software fallback if NVENC/CUDA failed.
            if plan.video_codec.ends_with("_nvenc") || plan.uses_hwaccel {
                let fallback_opts = ExportOptions {
                    use_nvenc: Some(false),
                    hwaccel: Some(false),
                    gpu_resident: Some(false),
                    codec: VideoCodec::H264,
                    ..opts.clone()
                };
                let fallback = build_export_plan(project, out, ass_path, &fallback_opts)
                    .map_err(MediaError::BadData)?;
                if run(&fallback).is_ok() {
                    return finish_stats(out, &fallback, true);
                }
            }
            Err(first_error)
        }
    }
}

fn finish_stats(
    out: &Path,
    plan: &ExportPlan,
    fallback_used: bool,
) -> Result<ExportStats, MediaError> {
    let actual = ffprobe::verify_output(
        out,
        plan.expected_duration_ticks,
        (plan.width, plan.height),
    )?;
    Ok(ExportStats {
        output_path: out.to_path_buf(),
        video_codec: plan.video_codec.to_string(),
        width: plan.width,
        height: plan.height,
        gpu_resident: plan.gpu_resident,
        fallback_used,
        expected_duration_ticks: plan.expected_duration_ticks,
        actual_duration_ticks: actual,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutpilot_core::TransitionKind;
    use cutpilot_core::{
        Asset, Caption, CaptionStyle, Clip, MediaProbe, ProjectSettings, Transition,
    };

    fn asset(name: &str, dur_secs: f64, has_audio: bool) -> Asset {
        Asset {
            id: uuid::Uuid::new_v4(),
            kind: AssetKind::Video,
            name: name.into(),
            path: format!("/tmp/{name}.mp4").into(),
            probe: Some(MediaProbe {
                duration_ticks: cutpilot_core::ticks_from_secs(dur_secs),
                width: 1920,
                height: 1080,
                fps: Some(cutpilot_core::Fps::from_int(30)),
                has_audio,
                rotation_deg: 0,
                video_codec: Some("h264".into()),
                audio_codec: if has_audio { Some("aac".into()) } else { None },
                sample_rate: Some(48_000),
            }),
            generated: None,
        }
    }

    fn base_clip(project: &Project, asset_id: uuid::Uuid, start_s: i64, dur_s: i64) -> Clip {
        Clip {
            id: uuid::Uuid::new_v4(),
            asset_id,
            track_id: project.tracks[0].id,
            timeline_start: cutpilot_core::ticks_from_secs(start_s as f64),
            source_in: 0,
            source_out: cutpilot_core::ticks_from_secs(dur_s as f64),
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: cutpilot_core::CropRect::default(),
            motion: None,
            color: cutpilot_core::ColorGrade::default(),
        }
    }

    #[test]
    fn plan_has_concat_fades_ass_and_amix() {
        let mut p = Project::new("t", ProjectSettings::default());
        let a1 = asset("one", 10.0, true);
        let a2 = asset("two", 10.0, false);
        let id1 = a1.id;
        let id2 = a2.id;
        p.assets.extend([a1, a2]);
        p.clips.push(base_clip(&p, id1, 0, 10));
        p.clips.push(base_clip(&p, id2, 10, 10));
        p.transitions.push(Transition {
            id: uuid::Uuid::new_v4(),
            at: cutpilot_core::ticks_from_secs(10.0),
            duration: cutpilot_core::ticks_from_secs(0.5),
            kind: TransitionKind::FadeThroughColor,
            color: "#000000".into(),
        });
        let music = asset("song", 30.0, false);
        let music_id = music.id;
        p.assets.push(music);
        p.clips.push(Clip {
            id: uuid::Uuid::new_v4(),
            asset_id: music_id,
            track_id: p.tracks[1].id,
            timeline_start: 0,
            source_in: 0,
            source_out: cutpilot_core::ticks_from_secs(5.0),
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 0.5,
            fade_in_ticks: 0,
            fade_out_ticks: cutpilot_core::ticks_from_secs(1.0),
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: cutpilot_core::CropRect::default(),
            motion: None,
            color: cutpilot_core::ColorGrade::default(),
        });
        p.captions.push(Caption {
            id: uuid::Uuid::new_v4(),
            start: 0,
            end: cutpilot_core::ticks_from_secs(2.0),
            text: "hello".into(),
        });
        p.caption_style = CaptionStyle::default();

        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(
            &p,
            Path::new("/tmp/out.mp4"),
            Some(Path::new("/tmp/c.ass")),
            &opts,
        )
        .unwrap();
        let joined = plan.args.join(" ");
        assert!(joined.contains("-filter_complex"), "missing filter graph");
        assert!(joined.contains("concat=n=2:v=1:a=0"), "missing video concat: {joined}");
        assert!(joined.contains("concat=n=2:v=0:a=1"), "missing audio concat");
        assert!(joined.contains("fade=t=in"), "missing fade in");
        assert!(joined.contains("fade=t=out"), "missing fade out");
        assert!(joined.contains("ass=/tmp/c.ass"), "missing ass burn");
        assert!(joined.contains("amix=inputs=2:duration=first:normalize=0"));
        assert!(joined.contains("afade=t=out"));
        assert!(joined.contains("volume=0.5000"));
        assert!(joined.contains("anullsrc=r=48000:cl=stereo"), "silent audio bed missing");
        assert!(joined.contains("libx264"));
        assert!(!joined.contains("h264_nvenc"));
        assert!(joined.contains("+faststart"));
        assert_eq!(
            plan.expected_duration_ticks,
            cutpilot_core::ticks_from_secs(20.0)
        );
    }

    #[test]
    fn plan_gpu_resident_zero_copy_cuda_pipeline() {
        let mut p = Project::new(
            "t",
            ProjectSettings {
                width: 1920,
                height: 1080,
                fps: cutpilot_core::Fps::from_int(30),
                audio_sample_rate: 48_000,
            },
        );
        let a1 = asset("clip1", 10.0, true);
        let id1 = a1.id;
        p.assets.push(a1);
        p.clips.push(base_clip(&p, id1, 0, 5));
        p.clips.push(base_clip(&p, id1, 5, 5));

        let opts = ExportOptions {
            use_nvenc: Some(true),
            hwaccel: Some(true),
            gpu_resident: Some(true),
            preset: ExportPreset::Fast,
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/out.mp4"), None, &opts).unwrap();
        let joined = plan.args.join(" ");
        assert!(plan.gpu_resident);
        assert!(joined.contains("-hwaccel cuda -hwaccel_output_format cuda"));
        assert!(joined.contains("scale_cuda=1920:1080:format=nv12:passthrough=1"));
        assert!(joined.contains("h264_nvenc -preset p2 -tune ull"));
    }

    #[test]
    fn linked_audio_is_not_double_mixed() {
        let mut p = Project::new("t", ProjectSettings::default());
        let a1 = asset("phone", 10.0, true);
        let id1 = a1.id;
        p.assets.push(a1);
        let grp = Uuid::new_v4();
        let mut v = base_clip(&p, id1, 0, 10);
        v.link_group = Some(grp);
        let mut a = base_clip(&p, id1, 0, 10);
        a.track_id = p.tracks[1].id;
        a.link_group = Some(grp);
        a.volume = 0.7;
        p.clips.extend([v, a]);

        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/out.mp4"), None, &opts).unwrap();
        let joined = plan.args.join(" ");
        // Base clip uses silent bed because A1's linked clip is authoritative.
        assert!(joined.contains("anullsrc=r=48000:cl=stereo"));
        assert!(joined.contains("volume=0.7000"));
        assert!(!joined.contains("[0:a]"));
    }

    #[test]
    fn unlinked_video_with_same_asset_on_audio_track_is_not_double_mixed() {
        let mut p = Project::new("t", ProjectSettings::default());
        let a1 = asset("talking_head", 30.0, true);
        let id1 = a1.id;
        p.assets.push(a1);
        // Video on V1 with link_group: None; unlinked audio moved to an Audio track with volume=2.0.
        let v = base_clip(&p, id1, 10, 15);
        let mut a = base_clip(&p, id1, 10, 15);
        a.track_id = p.tracks[1].id;
        a.link_group = None;
        a.volume = 2.0;
        p.clips.extend([v, a]);

        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/out.mp4"), None, &opts).unwrap();
        let joined = plan.args.join(" ");
        // V1 embedded audio [0:a] MUST NOT be pulled when an audio clip of the
        // same asset overlaps on an Audio track (avoids double-mixed echo!).
        assert!(!joined.contains("[0:a]"), "unlinked audio double-mixed: {joined}");
        assert!(joined.contains("volume=2.0000"));
    }

    #[test]
    fn fractional_clip_durations_stay_frame_quantized_and_synced_across_many_cuts() {
        let mut p = Project::new("sync", ProjectSettings::default()); // 30 fps
        let a = asset("broll", 30.0, true);
        let id = a.id;
        p.assets.push(a);

        // 21 clips with fractional non-frame-aligned durations (e.g. 2.143s each),
        // followed by a talking-head clip whose audio is on A1.
        let mut cursor: Ticks = 0;
        let frac_dur = cutpilot_core::ticks_from_secs(2.143);
        for _ in 0..21 {
            let mut v = base_clip(&p, id, 0, 1);
            v.timeline_start = cursor;
            v.source_in = 0;
            v.source_out = frac_dur;
            p.clips.push(v);
            cursor += frac_dur;
        }
        let talking_start = cursor;
        let talking_dur = cutpilot_core::ticks_from_secs(10.0);
        let mut tv = base_clip(&p, id, 0, 10);
        tv.timeline_start = talking_start;
        tv.source_in = 0;
        tv.source_out = talking_dur;
        let mut ta = tv.clone();
        ta.id = Uuid::new_v4();
        ta.track_id = p.tracks[1].id;
        p.clips.extend([tv, ta]);

        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/out.mp4"), None, &opts).unwrap();
        let joined = plan.args.join(" ");

        // Every base clip must enforce exact frame count (`trim=end_frame=`)
        // and exact sample-aligned audio bed (`apad,atrim=0:`), and cumulative
        // error diffusion keeps total drift <= half a frame (~17 ms)!
        assert!(joined.contains("trim=end_frame="));
        assert!(joined.contains("apad,atrim=0:"));
        let base_clips = p.clips_on_track(&p.tracks[0].id);
        let map = GapMap::new_for_fps(&base_clips, p.settings.fps);
        let mapped_talking = map.map(talking_start);
        let drift_ticks = (mapped_talking - talking_start).abs();
        let half_frame = p.settings.fps.frame_duration_ticks() / 2;
        assert!(
            drift_ticks <= half_frame,
            "cumulative sync drift {drift_ticks} ticks exceeded half a frame ({half_frame})"
        );
    }

    #[test]
    fn plan_overlays_track_with_alpha_enable_window() {
        let mut p = Project::new("t", ProjectSettings::default());
        let base = asset("base", 10.0, true);
        let base_id = base.id;
        p.assets.push(base);
        p.clips.push(base_clip(&p, base_id, 0, 10));
        p.tracks.insert(
            1,
            cutpilot_core::Track {
                id: uuid::Uuid::new_v4(),
                kind: TrackKind::Video,
                name: "V2".into(),
                muted: false,
                hidden: false,
                locked: false,
                volume: 1.0,
            },
        );
        let overlay = asset("overlay", 3.0, false);
        let overlay_id = overlay.id;
        p.assets.push(overlay);
        p.clips.push(Clip {
            id: uuid::Uuid::new_v4(),
            asset_id: overlay_id,
            track_id: p.tracks[1].id,
            timeline_start: cutpilot_core::ticks_from_secs(2.0),
            source_in: 0,
            source_out: cutpilot_core::ticks_from_secs(3.0),
            link_group: None,
            opacity: 0.8,
            scale: 1.0,
            x_offset: 10.0,
            y_offset: -5.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: cutpilot_core::CropRect::default(),
            motion: None,
            color: cutpilot_core::ColorGrade::default(),
        });
        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/out.mp4"), None, &opts).unwrap();
        let joined = plan.args.join(" ");
        assert!(joined.contains("format=yuva420p"), "overlay must keep alpha");
        assert!(joined.contains("colorchannelmixer=aa=0.8"));
        assert!(
            joined.contains("setpts=PTS+2.000000/TB"),
            "overlay must shift PTS to timeline start: {joined}"
        );
        assert!(
            joined.contains("enable='between(t,2,5)'"),
            "enable window: {joined}"
        );
        assert!(joined.contains("overlay=x="));
    }

    #[test]
    fn gap_map_collapses_gaps() {
        let mut p = Project::new("t", ProjectSettings::default());
        let a = asset("a", 10.0, true);
        let id = a.id;
        p.assets.push(a);
        p.clips.push(base_clip(&p, id, 0, 5));
        p.clips.push(base_clip(&p, id, 10, 5));
        let base_clips = p.clips_on_track(&p.tracks[0].id);
        let map = GapMap::new(&base_clips);
        assert_eq!(map.map(cutpilot_core::ticks_from_secs(0.0)), 0);
        assert_eq!(
            map.map(cutpilot_core::ticks_from_secs(4.9)),
            cutpilot_core::ticks_from_secs(4.9)
        );
        assert_eq!(
            map.map(cutpilot_core::ticks_from_secs(7.0)),
            cutpilot_core::ticks_from_secs(5.0)
        );
        assert_eq!(
            map.map(cutpilot_core::ticks_from_secs(12.0)),
            cutpilot_core::ticks_from_secs(7.0)
        );
        assert_eq!(map.total, cutpilot_core::ticks_from_secs(10.0));
    }

    #[test]
    fn empty_timeline_rejected() {
        let p = Project::new("t", ProjectSettings::default());
        let err = build_export_plan(&p, Path::new("/tmp/x.mp4"), None, &ExportOptions::default())
            .unwrap_err();
        assert!(err.contains("no clips"));
    }

    #[test]
    fn wav_audio_on_second_track_mixes_with_trimmed_source() {
        let mut p = Project::new("t", ProjectSettings::default());
        let vid = asset("video", 10.0, true);
        let vid_id = vid.id;
        let wav = Asset {
            id: uuid::Uuid::new_v4(),
            kind: AssetKind::Audio,
            name: "voiceover".into(),
            path: "/tmp/voiceover.wav".into(),
            probe: Some(MediaProbe {
                duration_ticks: cutpilot_core::ticks_from_secs(6.0),
                width: 0,
                height: 0,
                fps: None,
                has_audio: true,
                rotation_deg: 0,
                video_codec: None,
                audio_codec: Some("pcm_s16le".into()),
                sample_rate: Some(44_100),
            }),
            generated: None,
        };
        let wav_id = wav.id;
        p.assets.extend([vid, wav]);
        p.clips.push(base_clip(&p, vid_id, 0, 10));

        let a2 = cutpilot_core::Track {
            id: uuid::Uuid::new_v4(),
            kind: TrackKind::Audio,
            name: "A2".into(),
            muted: false,
            hidden: false,
            locked: false,
            volume: 1.0,
        };
        let a2_id = a2.id;
        p.tracks.push(a2);
        p.clips.push(Clip {
            id: uuid::Uuid::new_v4(),
            asset_id: wav_id,
            track_id: a2_id,
            timeline_start: cutpilot_core::ticks_from_secs(2.0),
            source_in: cutpilot_core::ticks_from_secs(1.0),
            source_out: cutpilot_core::ticks_from_secs(5.0),
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 0.8,
            fade_in_ticks: cutpilot_core::ticks_from_secs(0.5),
            fade_out_ticks: cutpilot_core::ticks_from_secs(0.5),
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: cutpilot_core::CropRect::default(),
            motion: None,
            color: cutpilot_core::ColorGrade::default(),
        });

        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/out.mp4"), None, &opts).unwrap();
        let joined = plan.args.join(" ");
        assert!(joined.contains("/tmp/voiceover.wav"));
        assert!(joined.contains("asetpts=PTS-STARTPTS,volume=0.8000"));
        assert!(joined.contains("adelay=delays=2000:all=1"));
        assert!(joined.contains("amix=inputs=2:duration=first:normalize=0"));
    }

    #[test]
    fn audio_only_wav_timeline_builds_black_canvas_plan() {
        let mut p = Project::new("t", ProjectSettings::default());
        let wav = Asset {
            id: uuid::Uuid::new_v4(),
            kind: AssetKind::Audio,
            name: "podcast".into(),
            path: "/tmp/podcast.wav".into(),
            probe: Some(MediaProbe {
                duration_ticks: cutpilot_core::ticks_from_secs(4.0),
                width: 0,
                height: 0,
                fps: None,
                has_audio: true,
                rotation_deg: 0,
                video_codec: None,
                audio_codec: Some("pcm_s16le".into()),
                sample_rate: Some(48_000),
            }),
            generated: None,
        };
        let wav_id = wav.id;
        p.assets.push(wav);
        p.clips.push(Clip {
            id: uuid::Uuid::new_v4(),
            asset_id: wav_id,
            track_id: p.tracks[1].id,
            timeline_start: 0,
            source_in: 0,
            source_out: cutpilot_core::ticks_from_secs(4.0),
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            speed: 1.0,
            rotation_deg: 0.0,
            flip_h: false,
            flip_v: false,
            crop: cutpilot_core::CropRect::default(),
            motion: None,
            color: cutpilot_core::ColorGrade::default(),
        });

        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/audio-only.mp4"), None, &opts).unwrap();
        let joined = plan.args.join(" ");
        assert!(!plan.gpu_resident);
        assert!(joined.contains("color=c=black:s=3840x2160:r=30"));
        assert!(joined.contains("/tmp/podcast.wav"));
        assert_eq!(plan.expected_duration_ticks, cutpilot_core::ticks_from_secs(4.0));
    }

    #[test]
    fn custom_export_resolution_scales_canvas_overlays_and_bitrate() {
        let mut p = Project::new("4k", ProjectSettings::default()); // 3840x2160
        let base = asset("base", 6.0, true);
        let base_id = base.id;
        p.assets.push(base);
        p.clips.push(base_clip(&p, base_id, 0, 6));

        // Explicit 1280x720 override on a 4K project.
        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            width: Some(1280),
            height: Some(720),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/720p.mp4"), None, &opts).unwrap();
        assert_eq!((plan.width, plan.height), (1280, 720));
        let joined = plan.args.join(" ");
        assert!(joined.contains("scale=1280:720"), "expected 720p scale: {joined}");

        // Height-only override preserves aspect ratio (4K 16:9 -> 1920x1080).
        let (w, h) = resolve_export_dimensions(&p, None, Some(1080)).unwrap();
        assert_eq!((w, h), (1920, 1080));

        // Odd dimension rejected cleanly.
        assert!(resolve_export_dimensions(&p, Some(1921), Some(1080)).is_err());
    }

    #[test]
    fn zoom_pan_speed_color_and_transform_build_valid_filtergraph() {
        let mut p = Project::new(
            "fx",
            ProjectSettings {
                width: 1920,
                height: 1080,
                fps: cutpilot_core::Fps::from_int(30),
                audio_sample_rate: 48_000,
            },
        );
        let a = asset("a", 8.0, true);
        let aid = a.id;
        p.assets.push(a);
        let mut c = base_clip(&p, aid, 0, 8);
        c.speed = 2.0; // 8s source -> 4s timeline!
        c.flip_h = true;
        c.rotation_deg = 15.0;
        c.crop = cutpilot_core::CropRect {
            left: 0.1,
            right: 0.1,
            top: 0.0,
            bottom: 0.0,
        };
        c.motion = cutpilot_core::ClipMotion::preset("zoom_in").unwrap();
        c.color = cutpilot_core::ColorGrade {
            brightness: 0.08,
            contrast: 1.15,
            saturation: 1.2,
            gamma: 1.0,
            blur: 1.5,
            vignette: 0.4,
        };
        p.clips.push(c);

        let opts = ExportOptions {
            use_nvenc: Some(true),
            hwaccel: Some(true),
            gpu_resident: Some(true),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/fx.mp4"), None, &opts).unwrap();
        // Effects must bypass zero-copy CUDA passthrough.
        assert!(!plan.gpu_resident);
        assert_eq!(
            plan.expected_duration_ticks,
            cutpilot_core::ticks_from_secs(4.0)
        );
        let joined = plan.args.join(" ");
        assert!(joined.contains("setpts=(PTS-STARTPTS)/2.000000"), "{joined}");
        assert!(joined.contains("atempo=2.000000"), "{joined}");
        assert!(joined.contains("crop=w="), "{joined}");
        assert!(joined.contains("hflip"), "{joined}");
        assert!(joined.contains("rotate="), "{joined}");
        assert!(joined.contains("zoompan=z="), "{joined}");
        assert!(joined.contains("eq=brightness=0.0800"), "{joined}");
        assert!(joined.contains("gblur=sigma=1.500"), "{joined}");
        assert!(joined.contains("vignette=angle="), "{joined}");
    }

    #[test]
    fn atempo_chain_handles_wide_speed_range() {
        assert_eq!(atempo_chain(1.0), "");
        assert_eq!(atempo_chain(2.0), "atempo=2.000000");
        assert_eq!(atempo_chain(4.0), "atempo=2.0,atempo=2.000000");
        assert_eq!(atempo_chain(0.25), "atempo=0.5,atempo=0.500000");
    }
}
