# OpenPrem MCP guide for coding agents

OpenPrem is an MCP-first video editor. Every action below is available over
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

```json
{
  "mcpServers": {
    "openprem": {
      "command": "/abs/path/OpenPrem/target/release/openprem-mcp"
    }
  }
}
```

## Tool catalog

| Domain | Tools |
|---|---|
| System | `system_capabilities` |
| Project | `project_create`, `project_open`, `project_save`, `project_close`, `project_get`, `project_update_settings` |
| Media | `media_import`, `media_remove`, `media_list`, `media_probe`, `media_detect_silence`, `media_thumbnail`, `media_contact_sheet`, `media_waveform` |
| Tracks | `track_add`, `track_update`, `track_move`, `track_remove` |
| Clips | `clip_insert`, `clip_move`, `clip_trim`, `clip_split`, `clip_remove`, `clip_update` |
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

## Workflow 2 — captions

- Add cues directly: `caption_add {start, end, text}` (burned in at export).
- Or import existing subtitles: `captions_import {path: "talk.srt"}`.
- Restyle with `caption_style_set` (font, size, colors, margin, alignment).
- `captions_export` writes `.srt` back out at any time.

## Workflow 3 — HyperFrames transition screens and animations

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

## Workflow 5 — music

`media_import` an audio file, then `clip_insert` onto the audio track at the
desired tick. `clip_update` sets `volume` and `fade_in_ticks`/`fade_out_ticks`;
the whole track's gain is `track_update {volume}`.

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
- Bitrate defaults scale from the YouTube 4K recommendation (~45 Mbps at
  4K30; ~68 at 4K60). Override with `video_bitrate_mbps`.
- NVENC is used when available (`h264_nvenc`, `-rc vbr`); on failure the
  engine retries once with libx264 and reports `fallback_used`.
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
