//! End-to-end MCP session test. Speaks newline-delimited JSON-RPC 2.0 over
//! the binary's stdio (exactly what an MCP host like Claude Code does) and
//! drives the full MVP workflow: create project -> import media -> detect
//! and remove dead space (atomic batch) -> caption -> transition -> preview
//! frame (image content) -> render job polling -> verified MP4 on disk.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

struct McpClient {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn spawn() -> Self {
        let exe = env!("CARGO_BIN_EXE_cutpilot-mcp");
        let mut child = StdCommand::new(exe)
            .env("CUTPILOT_STANDALONE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cutpilot-mcp");
        let stdout = child.stdout.take().expect("stdout piped");
        Self {
            child,
            reader: BufReader::new(stdout),
            next_id: 1,
        }
    }

    fn send(&mut self, value: &Value) {
        let mut stdin = self.child.stdin.take().expect("stdin piped");
        let line = value.to_string();
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .expect("write to server stdin");
        // put stdin back so future sends work
        self.child.stdin = Some(stdin);
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&msg);
        self.wait_response(id)
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        self.send(&msg);
    }

    fn wait_response(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for response {id}");
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .expect("read server line");
            assert!(n > 0, "server closed stdout while waiting for id {id}");
            let msg: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("bad json from server: {e}: {line}"));
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    panic!("json-rpc error for id {id}: {err}");
                }
                return msg["result"].clone();
            }
            // skip notifications and other ids
        }
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let result = self.request(
            "tools/call",
            json!({"name": name, "arguments": args}),
        );
        assert!(
            !result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false),
            "tool {name} failed: {result}"
        );
        result
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extract the text block of a tool result and parse it as JSON.
fn tool_json(result: &Value) -> Value {
    let content = result["content"].as_array().expect("content array");
    for block in content {
        if block["type"] == "text" {
            let text = block["text"].as_str().expect("text");
            return serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("tool text is not JSON: {e}\n{text}"));
        }
    }
    panic!("tool result has no text block: {result}");
}

fn has_image(result: &Value) -> bool {
    result["content"]
        .as_array()
        .map(|blocks| blocks.iter().any(|b| b["type"] == "image"))
        .unwrap_or(false)
}

fn make_sample_media(dir: &Path) -> PathBuf {
    let out = dir.join("pixel-sample.mp4");
    let status = StdCommand::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-filter_complex",
            "[1:a]volume=enable='between(t,2,4)':volume=0,volume=0.9[a]",
            "-map", "0:v", "-map", "[a]",
            "-t", "8", "-pix_fmt", "yuv420p",
            "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac",
        ])
        .arg(&out)
        .status()
        .expect("synthesizing sample media");
    assert!(status.success());
    out
}

fn make_sample_wav(dir: &Path) -> PathBuf {
    let out = dir.join("music.wav");
    let status = StdCommand::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", "sine=frequency=660:sample_rate=44100",
            "-ac", "2", "-t", "3", "-c:a", "pcm_s16le",
        ])
        .arg(&out)
        .status()
        .expect("synthesizing sample wav");
    assert!(status.success());
    out
}

fn ffprobe_duration_secs(path: &Path) -> f64 {
    let out = StdCommand::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    let v: Value = serde_json::from_slice(&out.stdout).expect("ffprobe json");
    v["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("duration")
}

#[test]
fn mcp_session_full_youtube_workflow() {
    let tmp = std::env::temp_dir().join(format!("cutpilot-mcp-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let media = make_sample_media(&tmp);
    let project_dir = tmp.join("project.cutpilot");
    let export_path = tmp.join("final.mp4");

    let mut client = McpClient::spawn();

    // --- handshake ---
    let init = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "cutpilot-e2e", "version": "0"}
        }),
    );
    assert_eq!(init["serverInfo"]["name"], "cutpilot");
    client.notify("notifications/initialized");

    // --- tool catalog ---
    let tools = client.request("tools/list", json!({}));
    let names: Vec<String> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for required in [
        "system_capabilities", "project_create", "project_open", "project_save",
        "project_get", "timeline_get", "media_import", "media_list", "media_probe",
        "media_detect_silence", "media_thumbnail", "media_contact_sheet",
        "media_waveform", "media_transcribe", "transcript_get",
        "transcript_to_captions", "track_add", "track_update", "clip_insert", "clip_move",
        "clip_trim", "clip_split", "clip_remove", "clip_update",
        "clip_zoom_pan", "clip_set_speed", "clip_color_grade",
        "clip_duplicate", "clip_unlink", "marker_add", "marker_remove",
        "timeline_remove_ranges", "transition_add", "transition_remove",
        "caption_style_set", "caption_add", "caption_update", "captions_import",
        "captions_export", "hyperframes_create", "hyperframes_render",
        "preview_seek", "editor_select", "editor_state_get", "preview_frame",
        "render_start", "render_status", "render_cancel", "jobs_list",
        "history_undo", "history_redo", "commands_execute", "commands_batch",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "missing MCP tool `{required}`; have {names:?}"
        );
    }

    // --- capabilities ---
    let caps = tool_json(&client.call_tool("system_capabilities", json!({})));
    assert!(caps["data"]["ffmpeg"].as_bool().unwrap());
    assert!(caps["data"]["whisper"].as_bool().unwrap());

    // --- project ---
    let created = tool_json(&client.call_tool(
        "project_create",
        json!({
            "name": "MCP E2E",
            "path": project_dir.display().to_string(),
            "width": 640, "height": 360, "fps_num": 30, "fps_den": 1,
        }),
    ));
    let revision = created["revision"].as_u64().unwrap();
    assert!(revision >= 1);

    // --- import with auto-place ---
    let imported = tool_json(&client.call_tool(
        "media_import",
        json!({
            "path": media.display().to_string(),
            "auto_place": true,
        }),
    ));
    let asset_id = imported["data"]["asset_id"].as_str().unwrap().to_string();
    assert_eq!(imported["data"]["placed_clips"].as_array().unwrap().len(), 2);

    // --- silence ---
    let silence = tool_json(&client.call_tool(
        "media_detect_silence",
        json!({
            "asset_id": asset_id,
            "threshold_db": -35.0,
            "min_duration_secs": 0.5,
            "padding_secs": 0.0,
        }),
    ));
    let ranges = silence["data"]["ranges"].as_array().unwrap().clone();
    assert_eq!(ranges.len(), 1, "one silent stretch: {silence}");
    let start = ranges[0]["start"].as_i64().unwrap();
    let end = ranges[0]["end"].as_i64().unwrap();
    assert!(start > 1_000_000 && end > start, "range {start}..{end}");

    // --- atomic batch: cut dead space + add a caption ---
    let revision = imported["revision"].as_u64().unwrap();
    let batch = tool_json(&client.call_tool(
        "commands_batch",
        json!({
            "commands": [
                {"kind": "remove_ranges", "start": start, "end": end, "ripple": true},
                {"kind": "add_caption", "start": 0, "end": 3_600_000, "text": "Cut by an agent"}
            ],
            "expected_revision": revision,
            "label": "cut dead space + caption"
        }),
    ));
    assert_eq!(batch["data"]["applied"], 2);

    // --- transition at the cut ---
    client.call_tool(
        "transition_add",
        json!({"at": start, "duration": 1_200_000, "color": "#000000"}),
    );

    // --- generic execute path ---
    let seek = tool_json(&client.call_tool(
        "commands_execute",
        json!({"command": {"kind": "set_playhead", "ticks": 0}}),
    ));
    assert_eq!(seek["kind"], "set_playhead");

    // --- wide zoom-out + transcript query ---
    let zoomed = tool_json(&client.call_tool(
        "editor_set_zoom",
        json!({"pixels_per_second": 0.05}),
    ));
    assert!((zoomed["data"]["pixels_per_second"].as_f64().unwrap() - 0.05).abs() < 1e-4);
    let tr = tool_json(&client.call_tool("transcript_get", json!({})));
    assert_eq!(tr["data"]["scope"], "timeline");
    assert_eq!(
        tr["data"]["untranscribed_audio_assets"].as_array().unwrap().len(),
        1
    );

    // --- import WAV audio and place onto audio track ---
    let wav = make_sample_wav(&tmp);
    let wav_imported = tool_json(&client.call_tool(
        "media_import",
        json!({
            "path": wav.display().to_string(),
            "auto_place": true,
        }),
    ));
    assert_eq!(wav_imported["data"]["kind"], "audio");
    assert_eq!(wav_imported["data"]["probe"]["audio_codec"], "pcm_s16le");
    let wav_placed = wav_imported["data"]["placed_clips"].as_array().unwrap();
    assert_eq!(wav_placed.len(), 1);
    assert_eq!(wav_placed[0]["timeline_start"], 0);

    // --- timeline sanity ---
    let timeline = tool_json(&client.call_tool("timeline_get", json!({})));
    let duration_ticks = timeline["data"]["duration_ticks"].as_i64().unwrap();
    assert!(
        (duration_ticks - 21_600_000).abs() < 1_800_000,
        "expected ~6s timeline, got {duration_ticks} ticks"
    );

    // --- zoom & pan, color grade, transform, duplicate/unlink, speed, markers ---
    let first_clip_id = timeline["data"]["tracks"][0]["clips"][0]["clip_id"]
        .as_str()
        .unwrap()
        .to_string();
    client.call_tool(
        "clip_zoom_pan",
        json!({"clip_id": first_clip_id, "preset": "zoom_in", "easing": "ease_in_out"}),
    );
    client.call_tool(
        "clip_color_grade",
        json!({"clip_id": first_clip_id, "contrast": 1.1, "saturation": 1.15, "vignette": 0.25}),
    );
    client.call_tool(
        "clip_update",
        json!({"clip_id": first_clip_id, "flip_h": true, "crop_left": 0.02, "crop_right": 0.02}),
    );
    let m_added = tool_json(&client.call_tool(
        "marker_add",
        json!({"at": 1_800_000, "label": "Punch-in", "color": "#F59E0B"}),
    ));
    let marker_id = m_added["data"]["marker_id"].as_str().unwrap().to_string();
    let dup = tool_json(&client.call_tool(
        "clip_duplicate",
        json!({"clip_id": first_clip_id}),
    ));
    let dup_id = dup["data"]["clip_id"].as_str().unwrap().to_string();
    client.call_tool("clip_set_speed", json!({"clip_id": dup_id, "speed": 2.0}));
    client.call_tool("clip_unlink", json!({"clip_id": dup_id}));
    // Undo unlink + speed + duplicate so exported length remains the 6s dead-space cut.
    client.call_tool("history_undo", json!({}));
    client.call_tool("history_undo", json!({}));
    client.call_tool("history_undo", json!({}));
    client.call_tool("marker_remove", json!({"marker_id": marker_id}));

    // --- preview frame with image content ---
    let preview = client.call_tool(
        "preview_frame",
        json!({"ticks": 1_800_000, "max_width": 320}),
    );
    assert!(has_image(&preview), "preview_frame must return an image");
    let preview_json = tool_json(&preview);
    assert_eq!(preview_json["data"]["width"], 320);

    // --- export job lifecycle (with custom output resolution) ---
    let render = tool_json(&client.call_tool(
        "render_start",
        json!({
            "output_path": export_path.display().to_string(),
            "use_nvenc": false,
            "video_bitrate_mbps": 2.0,
            "width": 480,
            "height": 270,
        }),
    ));
    assert_eq!(render["data"]["width"], 480);
    assert_eq!(render["data"]["height"], 270);
    let job_id = render["data"]["job_id"].as_str().unwrap().to_string();

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut done_data = None;
    while Instant::now() < deadline {
        let status = tool_json(&client.call_tool(
            "render_status",
            json!({"job_id": job_id}),
        ));
        let state = status["data"]["state"]["status"].as_str().unwrap();
        match state {
            "done" => {
                done_data = Some(status["data"]["state"]["data"].clone());
                break;
            }
            "failed" => panic!(
                "export failed: {}",
                status["data"]["state"]["error"]
            ),
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    let done_data = done_data.expect("export did not finish in time");
    assert_eq!(done_data["video_codec"], "libx264");
    assert_eq!(done_data["width"], 480);
    assert_eq!(done_data["height"], 270);

    // --- verify the artifact with ffprobe ---
    assert!(export_path.is_file());
    let size = export_path.metadata().unwrap().len();
    assert!(size > 10_000, "export suspiciously small ({size} bytes)");
    let duration = ffprobe_duration_secs(&export_path);
    assert!(
        (duration - 6.0).abs() < 0.35,
        "exported duration {duration}s, expected ~6s"
    );

    // --- save + reopen ---
    client.call_tool("project_save", json!({}));
    let reopened = tool_json(&client.call_tool(
        "project_open",
        json!({"path": project_dir.display().to_string()}),
    ));
    assert!(reopened["data"]["assets"].as_u64().unwrap() >= 1);

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn mcp_live_ipc_bridge_syncs_with_host_engine() {
    let tmp = std::env::temp_dir().join(format!("cutpilot-mcp-ipc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let sock = tmp.join("live.sock");
    let project_dir = tmp.join("live.cutpilot");

    // Simulate `cutpilot-ui` hosting its live Engine over IPC.
    let host_engine = std::sync::Arc::new(std::sync::Mutex::new(cutpilot_engine::Engine::new()));
    let _server = cutpilot_engine::IpcServer::start_at(sock.clone(), host_engine.clone(), None);

    // Spawn `cutpilot-mcp` WITHOUT standalone mode, pointed at our test socket.
    let exe = env!("CARGO_BIN_EXE_cutpilot-mcp");
    let mut child = StdCommand::new(exe)
        .env("CUTPILOT_IPC_SOCKET", &sock)
        .env_remove("CUTPILOT_STANDALONE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cutpilot-mcp");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut client = McpClient {
        child,
        reader: BufReader::new(stdout),
        next_id: 1,
    };

    client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "cutpilot-ipc-e2e", "version": "0"}
        }),
    );
    client.notify("notifications/initialized");

    client.call_tool(
        "project_create",
        json!({
            "name": "Live Synced Project",
            "path": project_dir.display().to_string(),
            "width": 1920, "height": 1080, "fps_num": 30, "fps_den": 1,
        }),
    );
    client.call_tool(
        "caption_add",
        json!({"start": 0, "end": 3_600_000, "text": "Edited live by MCP agent"}),
    );

    // Verify the host engine (representing the open UI) received the edits live!
    {
        let guard = host_engine.lock().unwrap();
        let proj = guard.project().expect("host engine should have project open");
        assert_eq!(proj.name, "Live Synced Project");
        assert_eq!(proj.captions.len(), 1);
        assert_eq!(proj.captions[0].text, "Edited live by MCP agent");
    }

    drop(client);
    drop(_server);
    std::fs::remove_dir_all(&tmp).ok();
}
