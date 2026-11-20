//! The export engine: builds a single deterministic ffmpeg filtergraph from
//! the project document and runs it with NVENC when available, falling back
//! to libx264 automatically.
//!
//! v1 compositing policy (documented for agents):
//! - The bottom-most video track is the base: clips are trimmed, letterboxed
//!   to the project canvas, and concatenated. Gaps between base clips
//!   collapse (use `remove_ranges` or a transition screen to fill).
//! - Higher video tracks composite on top with alpha and clip transforms.
//! - Audio-track clips mix over the base audio with gain/fades/delay.
//! - Captions burn in via ASS; their times are remapped through the same
//!   gap-collapsing map as overlays and music.

use crate::{ass, ffprobe, fmt_secs, process, MediaError};
use openprem_core::{AssetKind, Caption, Clip, Project, Ticks, TrackKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// `None` = auto-detect NVENC, `Some(false)` forces libx264.
    pub use_nvenc: Option<bool>,
    /// `None` = use CUDA decode when NVENC is used. Forced off on fallback.
    pub hwaccel: Option<bool>,
    pub video_bitrate_mbps: Option<f64>,
    pub audio_bitrate_kbps: u32,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            use_nvenc: None,
            hwaccel: None,
            video_bitrate_mbps: None,
            audio_bitrate_kbps: 256,
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
}

/// Verified result of a completed export.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExportStats {
    pub output_path: PathBuf,
    pub video_codec: String,
    pub fallback_used: bool,
    pub expected_duration_ticks: Ticks,
    pub actual_duration_ticks: Ticks,
}

/// YouTube-ish default bitrate: ~45 Mbps at 4K30, scaled by pixels*fps,
/// clamped to 2..=120 Mbps.
pub fn default_bitrate_mbps(project: &Project) -> f64 {
    let pixels = (project.settings.width * project.settings.height) as f64;
    let fps = project.settings.fps.as_f64();
    let base_pixels = 3840.0 * 2160.0;
    let br = 45.0 * (pixels * fps) / (base_pixels * 30.0);
    br.clamp(2.0, 120.0)
}

enum InputSpec {
    /// Seekable media: `-ss start -t dur -i path`.
    Media {
        path: PathBuf,
        seek: Option<(f64, f64)>,
        hw: bool,
    },
    /// Still image: `-loop 1 -t dur -i path`.
    Still { path: PathBuf, dur: f64 },
    /// Generated silence: `-f lavfi -t dur -i anullsrc=...`.
    Silence { dur: f64, sample_rate: u32 },
}

impl InputSpec {
    fn args(&self) -> Vec<String> {
        match self {
            InputSpec::Media { path, seek, hw } => {
                let mut args = Vec::new();
                if *hw {
                    args.push("-hwaccel".into());
                    args.push("cuda".into());
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
            InputSpec::Still { path, dur } => {
                vec![
                    "-loop".into(),
                    "1".into(),
                    "-t".into(),
                    fmt_secs(*dur),
                    "-i".into(),
                    path.display().to_string(),
                ]
            }
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

/// Build the full ffmpeg plan without running anything. Pure + unit-tested.
pub fn build_export_plan(
    project: &Project,
    out: &Path,
    ass_path: Option<&Path>,
    opts: &ExportOptions,
) -> Result<ExportPlan, String> {
    let (settings_w, settings_h) = (project.settings.width, project.settings.height);
    let fps = &project.settings.fps;
    let fps_arg = if fps.den == 1 {
        format!("fps={}", fps.num)
    } else {
        format!("fps={}/{}", fps.num, fps.den)
    };
    let sr = project.settings.audio_sample_rate;

    let base_track = project
        .base_video_track()
        .ok_or("project has no video track")?;
    let base_clips = project.clips_on_track(&base_track.id);
    if base_clips.is_empty() {
        return Err(
            "the base video track has no clips; insert media before exporting".into(),
        );
    }
    let expected_duration_ticks: Ticks = base_clips.iter().map(|c| c.source_duration()).sum();

    // Decide codecs up front so the plan is deterministic.
    let bitrate = opts.video_bitrate_mbps.unwrap_or_else(|| default_bitrate_mbps(project));
    let nvenc = opts.use_nvenc.unwrap_or_else(detect_nvenc);
    let video_codec = if nvenc { "h264_nvenc" } else { "libx264" };
    let hwaccel = opts.hwaccel.unwrap_or(nvenc);

    let mut b = Builder::new();
    let gap_map = GapMap::new(&base_clips);

    // --- base video segments ---
    let mut base_labels: Vec<String> = Vec::new();
    let mut audio_labels: Vec<String> = Vec::new();
    for clip in &base_clips {
        let asset = project
            .asset(&clip.asset_id)
            .ok_or_else(|| format!("clip {} references a missing asset", clip.asset_id))?;
        let dur = openprem_core::secs_from_ticks(clip.source_duration());
        let input = match asset.kind {
            AssetKind::Image => InputSpec::Still {
                path: asset.path.clone(),
                dur,
            },
            _ => InputSpec::Media {
                path: asset.path.clone(),
                seek: Some((
                    openprem_core::secs_from_ticks(clip.source_in),
                    dur,
                )),
                hw: hwaccel,
            },
        };
        let idx = b.add_input(input);
        let label = b.label("v");
        let mut chain = format!(
            "[{idx}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1",
            w = settings_w,
            h = settings_h,
        );
        chain.push_str(&fade_filters_for_clip(project, clip, dur));
        chain.push(',');
        chain.push_str(&fps_arg);
        chain.push_str(&format!("[{label}]"));
        b.filter(chain);
        base_labels.push(label);

        // Segment audio: file audio or matching silence.
        let has_audio = asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false);
        let audio_label = b.label("a");
        if has_audio {
            b.filter(format!(
                "[{idx}:a]aresample={sr},aformat=sample_fmts=fltp:channel_layouts=stereo[{al}]",
                al = audio_label
            ));
        } else {
            let sil = b.add_input(InputSpec::Silence { dur, sample_rate: sr });
            b.filter(format!(
                "[{sil}:a]aresample={sr},aformat=sample_fmts=fltp:channel_layouts=stereo[{al}]",
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
    let overlay_tracks: Vec<_> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video && t.id != base_track.id && !t.hidden)
        .collect();
    for track in overlay_tracks {
        for clip in project.clips_on_track(&track.id) {
            let asset = project
                .asset(&clip.asset_id)
                .ok_or_else(|| format!("clip {} references a missing asset", clip.asset_id))?;
            let dur = openprem_core::secs_from_ticks(clip.source_duration());
            let (pw, ph) = overlay_dims(project, clip);
            let start = openprem_core::secs_from_ticks(gap_map.map(clip.timeline_start));
            let end = openprem_core::secs_from_ticks(gap_map.map(clip.timeline_end()));
            let input = match asset.kind {
                AssetKind::Image => InputSpec::Still {
                    path: asset.path.clone(),
                    dur,
                },
                _ => InputSpec::Media {
                    path: asset.path.clone(),
                    seek: Some((
                        openprem_core::secs_from_ticks(clip.source_in),
                        dur,
                    )),
                    hw: false,
                },
            };
            let idx = b.add_input(input);
            let ov_label = b.label("ov");
            let mut chain = format!(
                "[{idx}:v]scale={pw}:{ph},setsar=1,format=yuva420p"
            );
            if clip.opacity < 1.0 {
                chain.push_str(&format!(
                    ",colorchannelmixer=aa={:.4}",
                    clip.opacity.clamp(0.0, 1.0)
                ));
            }
            // Alpha fades for overlay clips.
            if clip.fade_in_ticks > 0 {
                chain.push_str(&format!(
                    ",fade=t=in:st=0:d={}:alpha=1",
                    fmt_secs(openprem_core::secs_from_ticks(clip.fade_in_ticks))
                ));
            }
            if clip.fade_out_ticks > 0 {
                chain.push_str(&format!(
                    ",fade=t=out:st={}:d={}:alpha=1",
                    fmt_secs((dur - openprem_core::secs_from_ticks(clip.fade_out_ticks)).max(0.0)),
                    fmt_secs(openprem_core::secs_from_ticks(clip.fade_out_ticks))
                ));
            }
            chain.push(',');
            chain.push_str(&fps_arg);
            chain.push_str(&format!("[{ov_label}]"));
            b.filter(chain);

            let px = ((settings_w as f64 - pw as f64) / 2.0 + clip.x_offset as f64).round() as i64;
            let py = ((settings_h as f64 - ph as f64) / 2.0 + clip.y_offset as f64).round() as i64;
            let next = b.label("mix");
            b.filter(format!(
                "{current}[{ov_label}]overlay=x={px}:y={py}:eof_action=pass:\
                 enable='between(t,{start},{end})'[{next}]"
            ));
            current = format!("[{next}]");
        }
    }

    // --- captions --- (burn at the end of the video chain)
    let mut final_chain = format!("{current}format=yuv420p");
    if let Some(path) = ass_path {
        final_chain.push_str(&format!(",ass={}", escape_filter_path(&path.display().to_string())));
    }
    let vout = "vout";
    b.filter(format!("{final_chain}[{vout}]"));

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
            let dur = openprem_core::secs_from_ticks(clip.source_duration());
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
                    seek: Some((
                        openprem_core::secs_from_ticks(clip.source_in),
                        dur,
                    )),
                    hw: false,
                },
            };
            let idx = b.add_input(input);
            let label = b.label("au");
            let mut chain = format!(
                "[{idx}:a]volume={vol:.4}"
            );
            if clip.fade_in_ticks > 0 {
                chain.push_str(&format!(
                    ",afade=t=in:st=0:d={}",
                    fmt_secs(openprem_core::secs_from_ticks(clip.fade_in_ticks))
                ));
            }
            if clip.fade_out_ticks > 0 {
                chain.push_str(&format!(
                    ",afade=t=out:st={}:d={}",
                    fmt_secs(
                        (dur - openprem_core::secs_from_ticks(clip.fade_out_ticks)).max(0.0)
                    ),
                    fmt_secs(openprem_core::secs_from_ticks(clip.fade_out_ticks))
                ));
            }
            let delay_ms =
                openprem_core::ticks::ms_from_ticks(gap_map.map(clip.timeline_start)).max(0);
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
    if video_codec == "h264_nvenc" {
        let maxrate = bitrate * 1.5;
        let bufsize = bitrate * 2.0;
        args.extend(
            [
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p5",
                "-tune",
                "hq",
                "-rc",
                "vbr",
            ]
            .map(String::from),
        );
        args.push("-b:v".into());
        args.push(format!("{bitrate:.2}M"));
        args.push("-maxrate".into());
        args.push(format!("{maxrate:.2}M"));
        args.push("-bufsize".into());
        args.push(format!("{bufsize:.2}M"));
    } else {
        let maxrate = bitrate;
        let bufsize = bitrate * 2.0;
        args.extend(["-c:v", "libx264", "-preset", "medium"].map(String::from));
        args.push("-crf".into());
        args.push("18".into());
        args.push("-maxrate".into());
        args.push(format!("{maxrate:.2}M"));
        args.push("-bufsize".into());
        args.push(format!("{bufsize:.2}M"));
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
    })
}

/// Fade filters for a base clip, from transitions anchored at its edges.
fn fade_filters_for_clip(project: &Project, clip: &Clip, video_len: f64) -> String {
    let mut out = String::new();
    for tr in &project.transitions {
        let d = openprem_core::secs_from_ticks(tr.duration);
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

fn overlay_dims(project: &Project, clip: &Clip) -> (u32, u32) {
    let asset = project.asset(&clip.asset_id);
    let (w, h) = asset
        .and_then(|a| a.probe.as_ref())
        .map(|p| p.oriented_size())
        .unwrap_or((project.settings.width, project.settings.height));
    let scale = clip.scale.clamp(0.05, 20.0) as f64;
    let ow = ((w as f64 * scale).round() as u32).max(2) & !1;
    let oh = ((h as f64 * scale).round() as u32).max(2) & !1;
    (ow, oh)
}

fn escape_filter_path(path: &str) -> String {
    path.replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

/// Maps timeline ticks to export seconds when base-track gaps collapse.
struct GapMap {
    segments: Vec<(Ticks, Ticks, Ticks)>, // (tl_start, tl_end, export_start)
    total: Ticks,
}

impl GapMap {
    fn new(base_clips: &[&Clip]) -> Self {
        let mut segments = Vec::new();
        let mut cursor: Ticks = 0;
        for clip in base_clips {
            segments.push((
                clip.timeline_start,
                clip.timeline_end(),
                cursor,
            ));
            cursor += clip.source_duration();
        }
        Self {
            segments,
            total: cursor,
        }
    }

    fn map(&self, t: Ticks) -> Ticks {
        for (start, end, export_start) in &self.segments {
            if t < *start {
                return *export_start; // in a gap before this segment
            }
            if t < *end {
                return export_start + (t - start);
            }
        }
        self.total
    }
}

/// Remap captions into export time for the ASS burn-in.
pub fn remapped_captions(project: &Project, base_clips: &[&Clip]) -> Vec<Caption> {
    let map = GapMap::new(base_clips);
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

/// True when the local ffmpeg reports an `h264_nvenc` encoder.
pub fn detect_nvenc() -> bool {
    let Ok(output) = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
    else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|l| l.contains("h264_nvenc"))
}

/// Run the export end to end: write the ASS sidecar, run ffmpeg with
/// progress/cancel, retry once without NVENC/CUDA on failure, and verify
/// the output against the plan.
pub fn export_project(
    project: &Project,
    out: &Path,
    opts: &ExportOptions,
    progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
    cancel: Arc<AtomicBool>,
) -> Result<ExportStats, MediaError> {
    let base_track = project
        .base_video_track()
        .ok_or_else(|| MediaError::BadData("project has no video track".into()))?;
    let base_clips = project.clips_on_track(&base_track.id);

    // Caption sidecar (only when captions exist).
    let mut ass_path: Option<PathBuf> = None;
    if !project.captions.is_empty() {
        let dir = out
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("openprem-ass-{}.ass", uuid::Uuid::new_v4()));
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
    let plan = build_export_plan(project, out, ass_path, opts)
        .map_err(MediaError::BadData)?;
    let total_secs = openprem_core::secs_from_ticks(plan.expected_duration_ticks);

    let run = |plan: &ExportPlan| {
        process::run_with_progress("ffmpeg", &plan.args, total_secs, progress.clone(), cancel.clone())
    };

    match run(&plan) {
        Ok(_) => finish_stats(project, out, &plan, false),
        Err(first_error) => {
            if plan.video_codec == "h264_nvenc" || plan.uses_hwaccel {
                // Retry once on software with hardware fully disabled.
                let fallback_opts = ExportOptions {
                    use_nvenc: Some(false),
                    hwaccel: Some(false),
                    ..opts.clone()
                };
                let fallback = build_export_plan(project, out, ass_path, &fallback_opts)
                    .map_err(MediaError::BadData)?;
                if run(&fallback).is_ok() {
                    return finish_stats(project, out, &fallback, true);
                }
            }
            Err(first_error)
        }
    }
}

fn finish_stats(
    project: &Project,
    out: &Path,
    plan: &ExportPlan,
    fallback_used: bool,
) -> Result<ExportStats, MediaError> {
    let actual = ffprobe::verify_output(
        out,
        plan.expected_duration_ticks,
        (project.settings.width, project.settings.height),
    )?;
    Ok(ExportStats {
        output_path: out.to_path_buf(),
        video_codec: plan.video_codec.to_string(),
        fallback_used,
        expected_duration_ticks: plan.expected_duration_ticks,
        actual_duration_ticks: actual,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openprem_core::TransitionKind;
    use openprem_core::{
        Asset, Caption, CaptionStyle, Clip, MediaProbe, ProjectSettings, Transition,
    };

    fn asset(name: &str, dur_secs: f64, has_audio: bool) -> Asset {
        Asset {
            id: uuid::Uuid::new_v4(),
            kind: AssetKind::Video,
            name: name.into(),
            path: format!("/tmp/{name}.mp4").into(),
            probe: Some(MediaProbe {
                duration_ticks: openprem_core::ticks_from_secs(dur_secs),
                width: 1920,
                height: 1080,
                fps: Some(openprem_core::Fps::from_int(30)),
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
            timeline_start: openprem_core::ticks_from_secs(start_s as f64),
            source_in: 0,
            source_out: openprem_core::ticks_from_secs(dur_s as f64),
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
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
        // transition at the cut
        p.transitions.push(Transition {
            id: uuid::Uuid::new_v4(),
            at: openprem_core::ticks_from_secs(10.0),
            duration: openprem_core::ticks_from_secs(0.5),
            kind: TransitionKind::FadeThroughColor,
            color: "#000000".into(),
        });
        // music on A1
        let music = asset("song", 30.0, false);
        let music_id = music.id;
        p.assets.push(music);
        p.clips.push(Clip {
            id: uuid::Uuid::new_v4(),
            asset_id: music_id,
            track_id: p.tracks[1].id,
            timeline_start: 0,
            source_in: 0,
            source_out: openprem_core::ticks_from_secs(5.0),
            link_group: None,
            opacity: 1.0,
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            volume: 0.5,
            fade_in_ticks: 0,
            fade_out_ticks: openprem_core::ticks_from_secs(1.0),
        });
        // captions
        p.captions.push(Caption {
            id: uuid::Uuid::new_v4(),
            start: 0,
            end: openprem_core::ticks_from_secs(2.0),
            text: "hello".into(),
        });
        p.caption_style = CaptionStyle::default();

        let opts = ExportOptions {
            use_nvenc: Some(false),
            hwaccel: Some(false),
            ..Default::default()
        };
        let plan = build_export_plan(&p, Path::new("/tmp/out.mp4"), Some(Path::new("/tmp/c.ass")), &opts)
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
            openprem_core::ticks_from_secs(20.0)
        );
    }

    #[test]
    fn plan_overlays_track_with_alpha_enable_window() {
        let mut p = Project::new("t", ProjectSettings::default());
        let base = asset("base", 10.0, true);
        let base_id = base.id;
        p.assets.push(base);
        p.clips.push(base_clip(&p, base_id, 0, 10));
        // overlay track V2
        p.tracks.insert(
            1,
            openprem_core::Track {
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
            timeline_start: openprem_core::ticks_from_secs(2.0),
            source_in: 0,
            source_out: openprem_core::ticks_from_secs(3.0),
            link_group: None,
            opacity: 0.8,
            scale: 1.0,
            x_offset: 10.0,
            y_offset: -5.0,
            volume: 1.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
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
        // clip 0: 0..5s, clip 1: 10..15s (5s gap between)
        p.clips.push(base_clip(&p, id, 0, 5));
        p.clips.push(base_clip(&p, id, 10, 5));
        let base_clips = p.clips_on_track(&p.tracks[0].id);
        let map = GapMap::new(&base_clips);
        assert_eq!(map.map(openprem_core::ticks_from_secs(0.0)), 0);
        assert_eq!(map.map(openprem_core::ticks_from_secs(4.9)), openprem_core::ticks_from_secs(4.9));
        // inside the gap (7s) collapses to the next segment's export start (5s)
        assert_eq!(map.map(openprem_core::ticks_from_secs(7.0)), openprem_core::ticks_from_secs(5.0));
        assert_eq!(map.map(openprem_core::ticks_from_secs(12.0)), openprem_core::ticks_from_secs(7.0));
        assert_eq!(map.total, openprem_core::ticks_from_secs(10.0));
    }

    #[test]
    fn empty_timeline_rejected() {
        let p = Project::new("t", ProjectSettings::default());
        let err = build_export_plan(&p, Path::new("/tmp/x.mp4"), None, &ExportOptions::default())
            .unwrap_err();
        assert!(err.contains("no clips"));
    }
}
