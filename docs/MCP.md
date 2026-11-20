# CutPilot MCP guide for coding agents

CutPilot is an MCP-first video editor. Every action below is available over
MCP; the native UI runs the exact same commands, so agent edits and human
edits never diverge.

- All times are integer **ticks**: `3,600,000 ticks = 1 second`
  (frame-exact for 29.97/23.976 too: one 29.97 frame = 120,120 ticks).
- All ids are UUIDs returned by tools; keep them for the whole session.
- Tool responses are JSON: `{"kind", "revision", "message", "data"}`.
- Engine failures come back as tool errors with `"code"` (e.g. `validation`,
  `revision_mismatch`) — read them, fix the arguments, retry.
- The full command registry (for `commands_execute`/`commands_batch`) is in
  `system_capabilities` → `data.commands`.

## Configure

The fastest way is directly inside `cutpilot-ui`: click **Connect MCP Agent**
in the top bar to copy a pre-filled config with your exact `cutpilot-mcp`
binary path (or write `.mcp.json` into your project folder in one click).

When `cutpilot-ui` is running, `cutpilot-mcp` automatically bridges over a
local Unix socket so every MCP tool call edits your **live open timeline** in
real time with shared Undo/Redo history. Pass `--standalone` (or set
`CUTPILOT_STANDALONE=1`) to run headless in-process.

```json
{
  "mcpServers": {
    "cutpilot": {
      "command": "/abs/path/CutPilot/target/release/cutpilot-mcp"
    }
  }
}
```

## Tool catalog

| Domain | Tools |
|---|---|
| System | `system_capabilities` |
| Project | `project_create`, `project_open`, `project_save`, `project_close`, `project_get`, `project_update_settings` |
| Media | `media_import`, `media_remove`, `media_list`, `media_probe`, `media_detect_silence`, `media_thumbnail`, `media_contact_sheet`, `media_waveform`, `media_transcribe` |
| Transcript | `transcript_get`, `transcript_to_captions` |
| Tracks | `track_add`, `track_update`, `track_move`, `track_remove` |
| Clips | `clip_insert`, `clip_move`, `clip_trim`, `clip_split`, `clip_remove`, `clip_update`, `clip_zoom_pan`, `clip_set_speed`, `clip_color_grade`, `clip_duplicate`, `clip_unlink` |
| Markers | `marker_add`, `marker_remove` |
| Timeline | `timeline_get`, `timeline_remove_ranges` |
| Transitions | `transition_add`, `transition_remove` |
| Captions | `caption_style_set`, `caption_add`, `caption_update`, `caption_remove`, `captions_import`, `captions_export` |
| HyperFrames | `hyperframes_create`, `hyperframes_update`, `hyperframes_render` |
| Preview/editor | `preview_frame`, `preview_seek`, `editor_select`, `editor_set_zoom`, `editor_state_get` |
| Render/jobs | `render_start`, `render_status`, `render_cancel`, `jobs_list` |
| History | `history_undo`, `history_redo`, `history_list` |
| Generic | `commands_execute`, `commands_batch` |

## Workflow 1 — cut dead space out of a phone recording

1. `system_capabilities` — confirm `ffmpeg: true` and check NVENC.
2. `project_create` with a fresh directory (default 3840x2160 @ 30).
3. `media_import` with `auto_place: true` — the recording lands on the base
   video track **with a linked audio clip** (same `link_group`), so cuts stay
   in sync.
4. `media_detect_silence` with `threshold_db: -35`, `min_duration_secs: 0.6`,
   `padding_secs: 0.15` — returns tick ranges.
5. `commands_batch` with one `remove_ranges` per detected range
   (`"kind":"remove_ranges"`), **in descending order**, plus any captions you
   want in the same atomic step. Omit `tracks` to cut every unlocked track
   (the linked A/V pair cuts together, later content ripples left).
6. `preview_frame` at a few ticks to visually confirm.
7. `render_start` → poll `render_status` until `status: "done"` → the data
   includes the ffprobe-verified duration and codec.

## Workflow 2 — built-in Whisper transcription & captions

CutPilot ships **embedded `whisper.cpp`** directly in the binary (no Python or external Whisper CLI required). Models (`tiny.en`, `base.en` [default], `small.en`, `base`, `small`, `medium.en`, `medium`, `large-v3-turbo`, `large-v3`) auto-download into `~/.cache/cutpilot/whisper` on first use.

1. `media_transcribe { asset_id, model: "base.en" }` — runs Whisper as a background job (or pass `wait: true` to block and receive the transcript inline). Set `add_captions: true` to populate the caption track automatically.
2. Poll `render_status` / `jobs_list` until `status: "done"` (or call `transcript_get` directly once finished).
3. `transcript_get {}` (with `asset_id` omitted) returns the **timeline-mapped transcript**: every spoken segment with its current `timeline_start` / `timeline_end` ticks, automatically accounting for trims, splits, and dead-space cuts so you can edit by meaning!
4. `transcript_to_captions { replace: false }` converts the stored transcript into timed burned-in captions on the timeline (undoable).
5. You can also add/edit captions directly (`caption_add`, `captions_import`, `caption_style_set`, `captions_export`).

## Workflow 3 — HyperFrames transition screens, overlays & track z-order

**Video track layering (back-to-front):**
- Video tracks composite in ascending `index` order (`timeline_get` reports `index`, `layer_role`, and `video_layer_rank` on every track):
  - Lowest video track index (`video_layer_rank: 0`, typically `V1`) is the **back-most base/background layer**.
  - Higher video track indices (`V2`, `V3`, …) composite **in front / on top** of lower tracks (shown higher up in the UI timeline).
- Use `track_move { track_id, to_index }` to reorder tracks: move to a **higher `to_index`** to bring a track to the front, or a **lower `to_index`** to send it to the back.

`hyperframes_create` writes a managed composition into the project
(`hyperframes/<name>/index.html`) and renders it with the `hyperframes` CLI
(Node 22+; the CLI is fetched via `npx`). Author the HTML per the HyperFrames
schema: composition root with `data-composition-id`, `data-fps`,
`data-width`, `data-height`, `data-duration`, and seekable animations
(GSAP timeline registered on `window.__timelines`, CSS, etc.).

- **Transparent overlay** (default: `transparent: true, overlay: true`):
  renders ProRes 4444 with alpha, auto-inserts on the V2 overlay track at
  `insert_at_ticks` when the job completes. Leave `html`/`body` backgrounds
  unpainted wherever it must stay transparent.
- **Opaque transition screen** (`transparent: false, overlay: false`):
  renders MP4 and inserts on the base track — use it as a full-frame cutaway.

The render is a background job; poll `render_status`/`jobs_list`. With
`wait: true` the tool blocks (up to 10 min) and returns with the clip already
inserted. To change the animation later: `hyperframes_update` with new HTML,
then `hyperframes_render`.

Example overlay HTML (lower-third that slides in):

```html
<div id="stage" data-composition-id="lower3" data-start="0"
     data-duration="3" data-fps="30" data-width="3840" data-height="2160">
  <div id="bar"
        data-start="0" data-duration="3" data-track-index="1"
        style="position:absolute; left:120px; bottom:160px;
               background:#111; color:#fff; padding:24px 40px;
               font: 64px sans-serif; opacity:0;">
    Subscribe for more
  </div>
  <script src="https://cdn.jsdelivr.net/npm/gsap@3/dist/gsap.min.js"></script>
  <script>
    const tl = gsap.timeline({ paused: true });
    tl.to("#bar", { opacity: 1, x: 40, duration: 0.6 }, 0.2);
    tl.to("#bar", { opacity: 0, duration: 0.4 }, 2.4);
    window.__timelines = window.__timelines || {};
    window.__timelines.lower3 = tl;
  </script>
</div>
```

Then:

```json
hyperframes_create {
  "name": "subscribe-lower3",
  "html": "<div id=\"stage\" ...>...</div>",
  "duration_ticks": 10800000,
  "transparent": true,
  "overlay": true,
  "insert_at_ticks": 7200000
}
```

## Workflow 4 — transitions between clips

`transition_add` fades through a color at a cut: pass the tick of a base
clip's start (fade in) or end (fade out) and a duration. Find cut points via
`timeline_get` → `tracks[0].clips[].timeline_start/timeline_end`.

## Workflow 5 — music, voiceover & WAV audio

`media_import` any audio file (`.wav`, `.mp3`, `.flac`, `.aac`, `.ogg`, etc.):
- With `auto_place: true`, it lands in the Media Bin and automatically onto an
  unlocked empty audio track (`A1` if empty, or a freshly created `A2` at tick
  `0` if `A1` already holds linked video audio).
- With `auto_place: false`, it lands in the Media Bin only; place any range onto
  any audio track at any tick with `clip_insert`.
- `clip_update` sets `volume` and `fade_in_ticks`/`fade_out_ticks`; the whole
  track's gain/mute is `track_update {volume, muted}`. All unmuted audio tracks
  mix together in both live UI playback and final MP4 export.

## Workflow 6 — zoom & pan (Ken Burns), transforms, speed, color & markers

- **Static zoom, pan, rotate, flip & crop** (works on BOTH base `V1` and overlay tracks):
  `clip_update { clip_id, scale: 1.25, x_offset: 80, y_offset: -40, rotation_deg: 0, flip_h: false, flip_v: false, crop_left: 0.05, crop_right: 0.05 }`.
- **Animated zoom & pan (Ken Burns / punch-in / smooth pan)**:
  `clip_zoom_pan { clip_id, preset: "zoom_in" }` (presets: `zoom_in`, `zoom_out`, `punch_in`, `pan_right`, `pan_left`, `none`), or pass custom `start_scale` / `end_scale` (`1.0..=10.0`), `start_pan_x` / `start_pan_y` / `end_pan_x` / `end_pan_y` (`-1.0..=1.0`, `0` = center), and `easing` (`linear`, `ease_in_out`, `ease_in`, `ease_out`).
- **Speed control (slow-mo & timelapse)**:
  `clip_set_speed { clip_id, speed: 2.0 }` (`0.25..=8.0`). Scales linked A/V partners together, updates timeline duration, and applies smooth `setpts` + chained `atempo` on export.
- **Color grading & filters**:
  `clip_color_grade { clip_id, brightness: 0.05, contrast: 1.15, saturation: 1.1, gamma: 1.0, blur: 0, vignette: 0.3 }` (pass `saturation: 0` for black & white, or `reset: true` to clear).
- **Duplicate & Unlink A/V**:
  `clip_duplicate { clip_id }` clones a clip (and its linked partner, preserving all effects) after itself or at `timeline_start`. `clip_unlink { clip_id }` detaches a clip from its A/V link group for J-cuts, L-cuts, or swapping camera audio for a clean WAV track.
- **Timeline markers / chapters**:
  `marker_add { at, label, color }` and `marker_remove { marker_id }` annotate beats/chapters on the ruler (returned in `timeline_get` and shifted automatically on ripple edits).

## Concurrency & atomicity

- Every mutating response carries a `revision`. Pass it back as
  `expected_revision` in `commands_execute`/`commands_batch` to fail instead
  of clobbering concurrent edits (e.g. a human in the UI).
- `commands_batch` applies **all or nothing**. Only pure edits (tracks,
  clips, timeline, transitions, captions, settings) and viewer state can be
  batched.
- When cutting multiple ranges, batch them in **descending** order so earlier
  removals don't shift later ranges.

## Export notes

- Default output: `<project>/export-<unixtime>.mp4`.
- Optional `width` / `height` (even pixels, 16..=7680): override export
  resolution independently of the project canvas (pass both for an explicit
  size, or pass one to preserve the project aspect ratio).
- Bitrate defaults scale from the YouTube 4K recommendation (~45 Mbps at
  4K30; ~68 at 4K60) for the chosen export resolution. Override with
  `video_bitrate_mbps`.
- Optional `codec`: `"h264"` (default), `"hevc"`, or `"av1"`.
- Optional `preset`: `"fast"`, `"balanced"` (default), or `"quality"`.
- On NVIDIA GPUs, cut-only timelines use a **zero-copy GPU-resident CUDA
  pipeline** (`-hwaccel cuda -hwaccel_output_format cuda` → `scale_cuda` /
  `pad_cuda` → `h264_nvenc` / `hevc_nvenc`); timelines with transitions,
  overlays, or burned-in captions use NVDEC + NVENC; on failure the engine
  cascades cleanly and reports `fallback_used` if software x264 was needed.
- Base-track gaps collapse on export (concat is sequential). Use
  `remove_ranges` or insert a transition screen to fill intentional gaps.
- The engine ffprobes the finished file (duration within 250 ms, expected
  canvas, audio present) **before** the job reports success.

## Inspecting your work

- `preview_frame` returns the composited frame (base + overlays) as an image
  plus the active captions as text.
- `media_contact_sheet` for a quick look at an asset.
- `project_get` returns the entire document if you need raw state.
- `history_list` shows undo labels; `history_undo`/`history_redo` step back
  and forth (each bumps the revision).
