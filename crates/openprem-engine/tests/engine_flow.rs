//! End-to-end engine flow against real ffmpeg: create a project, import a
//! synthetic "phone recording" (8 s with a silent stretch in the middle),
//! detect and remove the dead space, add a caption + transition, export, and
//! verify the output with ffprobe. This is the heart of the MVP acceptance
//! workflow, exercised headlessly exactly the way an MCP agent drives it.

use openprem_core::{Command, TICKS_PER_SEC};
use openprem_engine::Engine;
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
    let tmp = std::env::temp_dir().join(format!("openprem-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let media = make_sample_media(&tmp);

    let mut engine = Engine::new();

    // capabilities
    let caps = engine.execute(Command::SystemCapabilities {}).unwrap();
    assert!(caps.data["ffmpeg"].as_bool().unwrap());

    // create project
    let project_dir = tmp.join("project.openprem");
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
                    text: "Welcome to OpenPrem".into(),
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
