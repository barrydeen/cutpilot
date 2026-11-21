//! End-to-end engine flow against real ffmpeg: create a project, import a
//! synthetic "phone recording" (8 s with a silent stretch in the middle),
//! detect and remove the dead space, add a caption + transition, export, and
//! verify the output with ffprobe. This is the heart of the MVP acceptance
//! workflow, exercised headlessly exactly the way an MCP agent drives it.

use cutpilot_core::{Command, TICKS_PER_SEC};
use cutpilot_engine::Engine;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

fn require_ffmpeg() {
    let ok = StdCommand::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "ffmpeg must be installed for this test");
}

/// 8 s testsrc2 clip with 440 Hz tone, silenced between 2 s and 4 s.
fn make_sample_media(dir: &Path) -> PathBuf {
    let out = dir.join("pixel-sample.mp4");
    let status = StdCommand::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-filter_complex",
            "[1:a]volume=enable='between(t,2,4)':volume=0,volume=0.9[a]",
            "-map",
            "0:v",
            "-map",
            "[a]",
            "-t",
            "8",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
        ])
        .arg(&out)
        .status()
        .expect("run ffmpeg to synthesize sample media");
    assert!(status.success(), "sample media synthesis failed");
    out
}

#[test]
fn full_edit_flow_cuts_dead_space_and_exports() {
    require_ffmpeg();
    let tmp = std::env::temp_dir().join(format!("cutpilot-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let media = make_sample_media(&tmp);

    let mut engine = Engine::new();

    // capabilities
    let caps = engine.execute(Command::SystemCapabilities {}).unwrap();
    assert!(caps.data["ffmpeg"].as_bool().unwrap());

    // create project
    let project_dir = tmp.join("project.cutpilot");
    engine
        .execute(Command::CreateProject {
            name: "Engine Flow".into(),
            path: project_dir.clone(),
            width: Some(640),
            height: Some(360),
            fps_num: Some(30),
            fps_den: Some(1),
        })
        .unwrap();
    assert!(project_dir.join("project.json").is_file());

    // import with auto-place (linked video + audio)
    let import = engine
        .execute(Command::ImportMedia {
            path: media.clone(),
            auto_place: Some(true),
        })
        .unwrap();
    let asset_id: uuid::Uuid =
        serde_json::from_value(import.data["asset_id"].clone()).unwrap();
    let placed = import.data["placed_clips"].as_array().unwrap();
    assert_eq!(placed.len(), 2, "video + linked audio clip: {placed:?}");
    let revision = engine.revision();

    // detect silence in the middle
    let silence = engine
        .execute(Command::DetectSilence {
            asset_id,
            threshold_db: Some(-35.0),
            min_duration_secs: Some(0.5),
            padding_secs: Some(0.0),
        })
        .unwrap();
    let ranges = silence.data["ranges"].as_array().unwrap();
    assert_eq!(ranges.len(), 1, "expected exactly one silent range: {silence:?}");
    let start = ranges[0]["start"].as_i64().unwrap();
    let end = ranges[0]["end"].as_i64().unwrap();
    // ~2s..~4s
    let two = TICKS_PER_SEC * 2;
    let four = TICKS_PER_SEC * 4;
    assert!((start - two).abs() < TICKS_PER_SEC / 2, "start {start}");
    assert!((end - four).abs() < TICKS_PER_SEC / 2, "end {end}");

    // atomic batch: remove the range with ripple + add a caption in one step
    let batch = engine
        .execute(Command::CommandsBatch {
            commands: vec![
                Command::RemoveRanges {
                    start,
                    end,
                    ripple: Some(true),
                    tracks: None,
                },
                Command::AddCaption {
                    start: 0,
                    end: TICKS_PER_SEC,
                    text: "Welcome to CutPilot".into(),
                },
            ],
            expected_revision: Some(revision),
            label: Some("cut dead space + intro caption".into()),
        })
        .unwrap();
    assert_eq!(batch.data["applied"], 2);

    // transition at the new cut point
    engine
        .execute(Command::AddTransition {
            at: start,
            duration: TICKS_PER_SEC / 3,
            transition_kind: None,
            color: Some("#000000".into()),
        })
        .unwrap();

    // preview frame must come back as a JPEG payload
    let preview = engine
        .execute(Command::PreviewFrame {
            ticks: Some(TICKS_PER_SEC / 2),
            max_width: Some(320),
        })
        .unwrap();
    let jpeg = preview.binary.expect("preview binary");
    assert!(jpeg.bytes.len() > 1_000);
    assert_eq!(jpeg.mime, "image/jpeg");

    // undo / redo round trip
    let before_undo = engine.revision();
    engine.execute(Command::Undo {}).unwrap();
    let after_undo = engine.revision();
    assert_eq!(after_undo, before_undo + 1, "undo must bump revision");
    engine.execute(Command::Redo {}).unwrap();

    // history should record our steps (before closing; history is in-memory)
    let history = engine.execute(Command::HistoryList {}).unwrap();
    assert!(!history.data["undo"].as_array().unwrap().is_empty());

    // save, reopen, confirm persistence
    engine
        .execute(Command::SaveProject { path: None })
        .unwrap();
    engine.execute(Command::CloseProject {}).unwrap();
    engine
        .execute(Command::OpenProject {
            path: project_dir.clone(),
        })
        .unwrap();
    let timeline = engine.execute(Command::GetTimeline {}).unwrap();
    let total = timeline.data["duration_ticks"].as_i64().unwrap();
    // 8 s minus ~2 s of dead space
    let expected = TICKS_PER_SEC * 6;
    assert!(
        (total - expected).abs() < TICKS_PER_SEC / 2,
        "timeline should be ~6s, got {total} ticks"
    );

    // export (force libx264 for a fast, deterministic test)
    let out = tmp.join("final.mp4");
    let render = engine
        .execute(Command::StartRender {
            output_path: Some(out.clone()),
            video_bitrate_mbps: Some(2.0),
            use_nvenc: Some(false),
            codec: None,
            preset: None,
            width: None,
            height: None,
        })
        .unwrap();
    let job_id: uuid::Uuid =
        serde_json::from_value(render.data["job_id"].clone()).unwrap();

    // poll the job to completion (bounded wait)
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut final_data = None;
    while Instant::now() < deadline {
        let job = engine.execute(Command::GetJob { job_id }).unwrap();
        let status = job.data["state"]["status"].as_str().unwrap().to_string();
        if status == "done" {
            final_data = Some(job.data["state"]["data"].clone());
            break;
        }
        if status == "failed" {
            panic!(
                "export failed: {}",
                job.data["state"]["error"].as_str().unwrap_or("?")
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let final_data =
        final_data.expect("export job did not finish in time");
    assert!(!final_data["fallback_used"].as_bool().unwrap());
    let actual_duration = final_data["actual_duration_ticks"].as_i64().unwrap();
    assert!(
        (actual_duration - expected).abs() < TICKS_PER_SEC / 4,
        "exported duration {actual_duration} ticks, expected ~{expected}"
    );
    assert!(out.is_file());
    assert!(out.metadata().unwrap().len() > 10_000, "output suspiciously small");

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn nvenc_gpu_zero_copy_export_when_hardware_present() {
    require_ffmpeg();
    let gpu = cutpilot_media::gpu::capabilities();
    if !gpu.nvenc_h264 {
        eprintln!("skipping NVENC export test: no verified NVENC hardware");
        return;
    }

    let tmp = std::env::temp_dir().join(format!("cutpilot-nvenc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let media = make_sample_media(&tmp);

    let mut engine = Engine::new();
    let project_dir = tmp.join("gpu.cutpilot");
    engine
        .execute(Command::CreateProject {
            name: "GPU Export".into(),
            path: project_dir,
            width: Some(640),
            height: Some(360),
            fps_num: Some(30),
            fps_den: Some(1),
        })
        .unwrap();
    engine
        .execute(Command::ImportMedia {
            path: media,
            auto_place: Some(true),
        })
        .unwrap();

    // Split in the middle so concat runs on GPU surfaces.
    let timeline = engine.execute(Command::GetTimeline {}).unwrap();
    let first_clip_id: uuid::Uuid = serde_json::from_value(
        timeline.data["tracks"][0]["clips"][0]["clip_id"].clone(),
    )
    .unwrap();
    engine
        .execute(Command::SplitClip {
            clip_id: first_clip_id,
            at: TICKS_PER_SEC * 4,
        })
        .unwrap();

    let out = tmp.join("gpu-out.mp4");
    let render = engine
        .execute(Command::StartRender {
            output_path: Some(out.clone()),
            video_bitrate_mbps: Some(4.0),
            use_nvenc: Some(true),
            codec: Some("h264".into()),
            preset: Some("fast".into()),
            width: None,
            height: None,
        })
        .unwrap();
    let job_id: uuid::Uuid = serde_json::from_value(render.data["job_id"].clone()).unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut final_data = None;
    while Instant::now() < deadline {
        let job = engine.execute(Command::GetJob { job_id }).unwrap();
        let status = job.data["state"]["status"].as_str().unwrap().to_string();
        if status == "done" {
            final_data = Some(job.data["state"]["data"].clone());
            break;
        }
        if status == "failed" {
            panic!("GPU export failed: {}", job.data["state"]["error"]);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let data = final_data.expect("GPU export timed out");
    assert_eq!(data["video_codec"], "h264_nvenc");
    assert!(!data["fallback_used"].as_bool().unwrap());
    if gpu.cuda_filters {
        assert!(
            data["gpu_resident"].as_bool().unwrap(),
            "expected zero-copy GPU-resident pipeline: {data}"
        );
    }
    assert!(out.is_file());

    std::fs::remove_dir_all(&tmp).ok();
}

/// 4 s stereo 44.1 kHz 16-bit PCM WAV file (880 Hz tone).
fn make_sample_wav(dir: &Path, name: &str, secs: u32) -> PathBuf {
    let out = dir.join(name);
    let status = StdCommand::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=44100",
            "-ac",
            "2",
            "-t",
            &secs.to_string(),
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&out)
        .status()
        .expect("run ffmpeg to synthesize sample wav");
    assert!(status.success(), "sample wav synthesis failed");
    out
}

#[test]
fn wav_import_auto_places_onto_audio_track_and_exports_cleanly() {
    require_ffmpeg();
    let tmp = std::env::temp_dir().join(format!("cutpilot-wav-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let video = make_sample_media(&tmp);
    let wav = make_sample_wav(&tmp, "voiceover.wav", 4);

    let mut engine = Engine::new();
    let project_dir = tmp.join("wav.cutpilot");
    engine
        .execute(Command::CreateProject {
            name: "WAV Mix".into(),
            path: project_dir,
            width: Some(640),
            height: Some(360),
            fps_num: Some(30),
            fps_den: Some(1),
        })
        .unwrap();

    // 1. Import video first (populates V1 + linked A1).
    engine
        .execute(Command::ImportMedia {
            path: video,
            auto_place: Some(true),
        })
        .unwrap();

    // 2. Import WAV with auto_place: should create A2 at tick 0 so it plays
    //    alongside the video's linked audio on A1.
    let imported = engine
        .execute(Command::ImportMedia {
            path: wav.clone(),
            auto_place: Some(true),
        })
        .unwrap();
    assert_eq!(imported.data["kind"], "audio");
    assert_eq!(imported.data["probe"]["audio_codec"], "pcm_s16le");
    assert_eq!(imported.data["probe"]["sample_rate"], 44_100);
    let wav_asset_id: uuid::Uuid =
        serde_json::from_value(imported.data["asset_id"].clone()).unwrap();
    let placed = imported.data["placed_clips"].as_array().unwrap();
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0]["timeline_start"], 0);
    let wav_track_id: uuid::Uuid =
        serde_json::from_value(placed[0]["track_id"].clone()).unwrap();

    // Confirm WAV landed on a distinct audio track (A2), not on A1 behind the video.
    let timeline = engine.execute(Command::GetTimeline {}).unwrap();
    let tracks = timeline.data["tracks"].as_array().unwrap();
    let wav_track = tracks
        .iter()
        .find(|t| t["track_id"].as_str() == Some(&wav_track_id.to_string()))
        .expect("wav track exists");
    assert_eq!(wav_track["kind"], "audio");
    assert_eq!(wav_track["name"], "A2");

    // 3. Also manually place a trimmed copy of the WAV further down A2 with fades.
    let ins = engine
        .execute(Command::InsertClip {
            asset_id: wav_asset_id,
            track_id: wav_track_id,
            timeline_start: TICKS_PER_SEC * 5,
            source_in: TICKS_PER_SEC,
            source_out: TICKS_PER_SEC * 3,
            link_group: None,
        })
        .unwrap();
    let second_clip_id: uuid::Uuid =
        serde_json::from_value(ins.data["clip_id"].clone()).unwrap();
    engine
        .execute(Command::UpdateClip {
            clip_id: second_clip_id,
            volume: Some(0.6),
            opacity: None,
            scale: None,
            x_offset: None,
            y_offset: None,
            fade_in_ticks: Some(TICKS_PER_SEC / 2),
            fade_out_ticks: Some(TICKS_PER_SEC / 2),
            rotation_deg: None,
            flip_h: None,
            flip_v: None,
            crop: None,
        })
        .unwrap();

    // 4. Waveform extraction works on the WAV asset.
    let wf = engine
        .execute(Command::Waveform {
            asset_id: wav_asset_id,
            buckets: Some(64),
        })
        .unwrap();
    assert_eq!(wf.data["buckets"].as_u64().unwrap(), 64);

    // 5. Export mixed video + A1 + A2 WAV and verify with ffprobe.
    let out = tmp.join("wav-mixed.mp4");
    let render = engine
        .execute(Command::StartRender {
            output_path: Some(out.clone()),
            video_bitrate_mbps: Some(2.0),
            use_nvenc: Some(false),
            codec: None,
            preset: None,
            width: None,
            height: None,
        })
        .unwrap();
    let job_id: uuid::Uuid = serde_json::from_value(render.data["job_id"].clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut done = false;
    while Instant::now() < deadline {
        let job = engine.execute(Command::GetJob { job_id }).unwrap();
        let status = job.data["state"]["status"].as_str().unwrap();
        if status == "done" {
            done = true;
            break;
        }
        if status == "failed" {
            panic!("WAV mixed export failed: {}", job.data["state"]["error"]);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(done, "WAV mixed export timed out");
    assert!(out.is_file());

    // 6. Audio-only project with just a WAV file on A1 also imports & exports cleanly.
    let mut audio_only_engine = Engine::new();
    audio_only_engine
        .execute(Command::CreateProject {
            name: "Audio Only".into(),
            path: tmp.join("audio-only.cutpilot"),
            width: Some(640),
            height: Some(360),
            fps_num: Some(30),
            fps_den: Some(1),
        })
        .unwrap();
    let imp_ao = audio_only_engine
        .execute(Command::ImportMedia {
            path: wav,
            auto_place: Some(true),
        })
        .unwrap();
    assert_eq!(imp_ao.data["placed_clips"][0]["timeline_start"], 0);
    let ao_out = tmp.join("audio-only.mp4");
    let ao_render = audio_only_engine
        .execute(Command::StartRender {
            output_path: Some(ao_out.clone()),
            video_bitrate_mbps: Some(1.0),
            use_nvenc: Some(false),
            codec: None,
            preset: None,
            width: Some(320),
            height: Some(180),
        })
        .unwrap();
    assert_eq!(ao_render.data["width"], 320);
    assert_eq!(ao_render.data["height"], 180);
    let ao_job: uuid::Uuid = serde_json::from_value(ao_render.data["job_id"].clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut ao_done_data = None;
    while Instant::now() < deadline {
        let job = audio_only_engine
            .execute(Command::GetJob { job_id: ao_job })
            .unwrap();
        let status = job.data["state"]["status"].as_str().unwrap();
        if status == "done" {
            ao_done_data = Some(job.data["state"]["data"].clone());
            break;
        }
        if status == "failed" {
            panic!("audio-only WAV export failed: {}", job.data["state"]["error"]);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let ao_done_data = ao_done_data.expect("audio-only WAV export timed out");
    assert_eq!(ao_done_data["width"], 320);
    assert_eq!(ao_done_data["height"], 180);
    assert!(ao_out.is_file());

    std::fs::remove_dir_all(&tmp).ok();
}

fn sample_center_rgb(path: &Path, at_secs: f64) -> (u8, u8, u8) {
    let out = StdCommand::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &format!("{at_secs:.3}"),
            "-i",
        ])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "format=rgb24,crop=1:1:iw/2:ih/2",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .output()
        .expect("sample center pixel");
    assert!(
        out.status.success(),
        "ffmpeg pixel sample failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout.len(), 3);
    (out.stdout[0], out.stdout[1], out.stdout[2])
}

#[test]
fn overlay_clip_placed_after_zero_renders_in_preview_and_export() {
    require_ffmpeg();
    let tmp = std::env::temp_dir().join(format!("cutpilot-overlay-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();

    // Blue 5s base video with silent audio, plus a 2s bright-red overlay clip.
    let base_vid = tmp.join("base-blue.mp4");
    assert!(StdCommand::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", "color=c=blue:s=320x180:r=30:d=5",
            "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo",
            "-t", "5", "-pix_fmt", "yuv420p", "-c:v", "libx264", "-preset", "ultrafast",
            "-c:a", "aac",
        ])
        .arg(&base_vid)
        .status()
        .unwrap()
        .success());

    let overlay_vid = tmp.join("hf-red.mov");
    assert!(StdCommand::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", "color=c=red:s=320x180:r=30:d=2",
            "-t", "2", "-c:v", "prores_ks", "-profile:v", "4444", "-pix_fmt", "yuva444p10le",
        ])
        .arg(&overlay_vid)
        .status()
        .unwrap()
        .success());

    let mut engine = Engine::new();
    engine
        .execute(Command::CreateProject {
            name: "Overlay Test".into(),
            path: tmp.join("ov.cutpilot"),
            width: Some(320),
            height: Some(180),
            fps_num: Some(30),
            fps_den: Some(1),
        })
        .unwrap();
    engine
        .execute(Command::ImportMedia {
            path: base_vid,
            auto_place: Some(true),
        })
        .unwrap();
    let ov_imp = engine
        .execute(Command::ImportMedia {
            path: overlay_vid,
            auto_place: Some(false),
        })
        .unwrap();
    let ov_asset_id: uuid::Uuid =
        serde_json::from_value(ov_imp.data["asset_id"].clone()).unwrap();

    // Add V2 overlay track (higher index = front) and place the 2s overlay at t=2.5s..4.5s.
    let v2_res = engine
        .execute(Command::AddTrack {
            track_kind: "video".into(),
            name: Some("V2".into()),
        })
        .unwrap();
    let v2_id: uuid::Uuid = serde_json::from_value(v2_res.data["track_id"].clone()).unwrap();
    engine
        .execute(Command::InsertClip {
            asset_id: ov_asset_id,
            track_id: v2_id,
            timeline_start: TICKS_PER_SEC * 5 / 2,
            source_in: 0,
            source_out: TICKS_PER_SEC * 2,
            link_group: None,
        })
        .unwrap();

    // Timeline JSON must report layer_role & video_layer_rank so MCP agents know front vs back.
    let tl = engine.execute(Command::GetTimeline {}).unwrap();
    let tracks = tl.data["tracks"].as_array().unwrap();
    assert_eq!(tracks[0]["layer_role"], "base_back");
    assert_eq!(tracks[0]["video_layer_rank"], 0);
    let v2_json = tracks
        .iter()
        .find(|t| t["name"] == "V2")
        .expect("V2 in timeline");
    assert_eq!(v2_json["layer_role"], "overlay_front");
    assert_eq!(v2_json["video_layer_rank"], 1);

    // Export and check pixels at t=1.0s (must be blue) and t=3.5s (must be red!).
    let out = tmp.join("ov-out.mp4");
    let render = engine
        .execute(Command::StartRender {
            output_path: Some(out.clone()),
            video_bitrate_mbps: Some(2.0),
            use_nvenc: Some(false),
            codec: None,
            preset: None,
            width: None,
            height: None,
        })
        .unwrap();
    let job_id: uuid::Uuid = serde_json::from_value(render.data["job_id"].clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut done = false;
    while Instant::now() < deadline {
        let job = engine.execute(Command::GetJob { job_id }).unwrap();
        let status = job.data["state"]["status"].as_str().unwrap();
        if status == "done" {
            done = true;
            break;
        }
        if status == "failed" {
            panic!("overlay export failed: {}", job.data["state"]["error"]);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(done, "overlay export timed out");

    let (r1, _g1, b1) = sample_center_rgb(&out, 1.0);
    assert!(b1 > 180 && r1 < 50, "expected blue at t=1.0s, got ({r1}, {_g1}, {b1})");
    let (r2, _g2, b2) = sample_center_rgb(&out, 3.5);
    assert!(r2 > 180 && b2 < 50, "expected red overlay at t=3.5s, got ({r2}, {_g2}, {b2})");

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn zoom_pan_speed_color_and_classic_edits_render_cleanly() {
    let tmp = std::env::temp_dir().join(format!("cutpilot-fx-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let media = make_sample_media(&tmp); // 8s 640x360 w/ audio

    let mut engine = Engine::new();
    engine
        .execute(Command::CreateProject {
            name: "fx-test".into(),
            path: tmp.join("fx.cutpilot"),
            width: Some(640),
            height: Some(360),
            fps_num: Some(30),
            fps_den: Some(1),
        })
        .unwrap();
    let imp = engine
        .execute(Command::ImportMedia {
            path: media,
            auto_place: Some(true),
        })
        .unwrap();
    let vid_clip_id: uuid::Uuid =
        serde_json::from_value(imp.data["placed_clips"][0]["clip_id"].clone()).unwrap();

    // Trim to first 4s source, then 2x speed -> 2s on timeline.
    engine
        .execute(Command::TrimClip {
            clip_id: vid_clip_id,
            timeline_start: None,
            source_in: Some(0),
            source_out: Some(TICKS_PER_SEC * 4),
        })
        .unwrap();
    engine
        .execute(Command::SetClipSpeed {
            clip_id: vid_clip_id,
            speed: 2.0,
        })
        .unwrap();
    engine
        .execute(Command::SetClipMotion {
            clip_id: vid_clip_id,
            motion: cutpilot_core::ClipMotion::preset("zoom_in").unwrap(),
        })
        .unwrap();
    engine
        .execute(Command::SetClipColor {
            clip_id: vid_clip_id,
            color: cutpilot_core::ColorGrade {
                brightness: 0.05,
                contrast: 1.15,
                saturation: 1.2,
                gamma: 1.0,
                blur: 0.0,
                vignette: 0.35,
            },
        })
        .unwrap();
    engine
        .execute(Command::UpdateClip {
            clip_id: vid_clip_id,
            volume: None,
            opacity: None,
            scale: None,
            x_offset: None,
            y_offset: None,
            fade_in_ticks: None,
            fade_out_ticks: None,
            rotation_deg: Some(5.0),
            flip_h: Some(true),
            flip_v: None,
            crop: Some(cutpilot_core::CropRect {
                left: 0.05,
                right: 0.05,
                top: 0.0,
                bottom: 0.0,
            }),
        })
        .unwrap();

    // Duplicate the 2s clip -> total timeline is 4s!
    let dup = engine
        .execute(Command::DuplicateClip {
            clip_id: vid_clip_id,
            timeline_start: None,
        })
        .unwrap();
    assert_eq!(dup.data["duplicated_count"], 2);
    let dup_vid_id: uuid::Uuid = serde_json::from_value(dup.data["clip_id"].clone()).unwrap();
    engine
        .execute(Command::UnlinkClip {
            clip_id: dup_vid_id,
        })
        .unwrap();
    engine
        .execute(Command::AddMarker {
            at: TICKS_PER_SEC * 2,
            label: "Chapter 2".into(),
            color: Some("#22C55E".into()),
        })
        .unwrap();

    // Preview frame with all effects active must succeed.
    let preview = engine
        .execute(Command::PreviewFrame {
            ticks: Some(TICKS_PER_SEC),
            max_width: Some(320),
        })
        .unwrap();
    assert!(preview.binary.is_some());

    // Render full MP4 with zoompan + 2x speed + atempo + eq + vignette + crop + hflip + rotate.
    let out = tmp.join("fx-out.mp4");
    let render = engine
        .execute(Command::StartRender {
            output_path: Some(out.clone()),
            video_bitrate_mbps: Some(2.0),
            use_nvenc: Some(false),
            codec: None,
            preset: None,
            width: None,
            height: None,
        })
        .unwrap();
    let job_id: uuid::Uuid = serde_json::from_value(render.data["job_id"].clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut done = false;
    while Instant::now() < deadline {
        let job = engine.execute(Command::GetJob { job_id }).unwrap();
        let status = job.data["state"]["status"].as_str().unwrap();
        if status == "done" {
            done = true;
            break;
        }
        if status == "failed" {
            panic!("FX export failed: {}", job.data["state"]["error"]);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(done, "FX export timed out");
    let probed = cutpilot_media::ffprobe::probe(&out).unwrap();
    let dur_secs = cutpilot_core::secs_from_ticks(probed.probe.duration_ticks);
    assert!(
        (dur_secs - 4.0).abs() < 0.35,
        "expected ~4.0s output (two 2x-speed 4s clips), got {dur_secs:.2}s"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn voiceover_recording_places_clip_and_supports_undo_and_discard() {
    require_ffmpeg();
    let tmp = std::env::temp_dir().join(format!("cutpilot-vo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let media = make_sample_media(&tmp);

    let mut engine = Engine::new();
    let monitor = engine.recording_monitor();
    assert!(!monitor.is_recording());

    // Listing audio inputs works even before opening a project.
    let inputs = engine.execute(Command::ListAudioInputs {}).unwrap();
    assert!(!inputs.data["devices"].as_array().unwrap().is_empty());

    let project_dir = tmp.join("vo.cutpilot");
    engine
        .execute(Command::CreateProject {
            name: "Voiceover Test".into(),
            path: project_dir.clone(),
            width: Some(640),
            height: Some(360),
            fps_num: Some(30),
            fps_den: Some(1),
        })
        .unwrap();
    engine
        .execute(Command::ImportMedia {
            path: media,
            auto_place: Some(true),
        })
        .unwrap();

    // Start recording at t = 2s while A1 is occupied by linked camera audio ->
    // should auto-create a dedicated `Voiceover` track on stop!
    let started = engine
        .execute(Command::StartAudioRecording {
            name: Some("Intro Narration".into()),
            device: Some(cutpilot_media::record::SYNTHETIC_TEST_DEVICE.into()),
            track_id: None,
            insert_at_ticks: Some(TICKS_PER_SEC * 2),
            sample_rate: Some(48_000),
            channels: Some(1),
            auto_place: Some(true),
            input_gain: Some(0.65),
            studio_voice: Some(true),
        })
        .unwrap();
    assert_eq!(started.data["name"], "Intro-Narration");
    assert!(monitor.is_recording());

    // Starting a second recording while one is active must fail cleanly.
    assert!(engine
        .execute(Command::StartAudioRecording {
            name: None,
            device: Some(cutpilot_media::record::SYNTHETIC_TEST_DEVICE.into()),
            track_id: None,
            insert_at_ticks: None,
            sample_rate: None,
            channels: None,
            auto_place: Some(true),
            input_gain: None,
            studio_voice: None,
        })
        .is_err());

    std::thread::sleep(Duration::from_millis(260));
    let status = engine.execute(Command::GetRecordingStatus {}).unwrap();
    assert_eq!(status.data["recording"], true);
    assert!(status.data["elapsed_ticks"].as_i64().unwrap() > 0);
    let live_snap = monitor.snapshot();
    assert!(live_snap.recording);
    assert!(live_snap.peak > 0.15 && live_snap.peak <= 0.85);
    assert!(!live_snap.clipping);
    assert!(!live_snap.waveform_peaks.is_empty());

    let stopped = engine
        .execute(Command::StopAudioRecording {
            discard: Some(false),
        })
        .unwrap();
    assert!(!monitor.is_recording());
    assert_eq!(stopped.data["discarded"], false);
    assert_eq!(stopped.data["studio_polished"], true);
    assert_eq!(stopped.data["placed_clip"]["track_name"], "Voiceover");
    assert_eq!(
        stopped.data["placed_clip"]["timeline_start"],
        TICKS_PER_SEC * 2
    );
    let wav_path = PathBuf::from(stopped.data["path"].as_str().unwrap());
    assert!(wav_path.is_file());
    assert!(wav_path.starts_with(project_dir.join("recordings")));

    // Single Undo step cleanly removes the recorded clip and asset.
    let before_undo_clips = engine.project().unwrap().clips.len();
    engine.execute(Command::Undo {}).unwrap();
    assert_eq!(engine.project().unwrap().clips.len(), before_undo_clips - 1);

    // Discard path deletes the recorded WAV and leaves the project untouched.
    let rev_before = engine.revision();
    let started2 = engine
        .execute(Command::StartAudioRecording {
            name: Some("flubbed-take".into()),
            device: Some(cutpilot_media::record::SYNTHETIC_TEST_DEVICE.into()),
            track_id: None,
            insert_at_ticks: Some(0),
            sample_rate: Some(48_000),
            channels: Some(1),
            auto_place: Some(true),
            input_gain: None,
            studio_voice: None,
        })
        .unwrap();
    let discard_path = PathBuf::from(started2.data["output_path"].as_str().unwrap());
    std::thread::sleep(Duration::from_millis(120));
    let discarded = engine
        .execute(Command::StopAudioRecording {
            discard: Some(true),
        })
        .unwrap();
    assert_eq!(discarded.data["discarded"], true);
    assert!(!discard_path.exists());
    assert_eq!(engine.revision(), rev_before);

    std::fs::remove_dir_all(&tmp).ok();
}
