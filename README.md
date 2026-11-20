# OpenPrem

**An MCP-first video editor for YouTube, written in Rust.**

OpenPrem is built around one principle: *every semantic editing action lives in a
versioned command registry that the MCP surface and the native UI share.* A
coding agent (Claude Code, Cursor, Codex, ...) can perform a **complete edit —
import 4K phone footage, cut out dead space, add captions, transitions,
HyperFrames animations, and export an MP4 — entirely over MCP** while a human
browses and adjusts the same project in the native editor.

```
coding agent ──MCP/stdio──▶ openprem-mcp ──┐
                                          ├──▶ engine ──▶ ffmpeg (NVDEC/NVENC)
human ───────native window──▶ openprem-ui ─┘       │
                                                   └──▶ hyperframes CLI (Node)
```

## What's in the box

| Binary | Purpose |
|---|---|
| `openprem-mcp` | Stdio MCP server exposing the full editing registry (~45 tools) |
| `openprem-ui` | Native egui/wgpu editor: timeline, tracks, preview, inspector, jobs |

| Crate | Purpose |
|---|---|
| `openprem-core` | Tick-exact timeline model + pure command language (no I/O) |
| `openprem-media` | ffprobe, silence detection, frame extraction, waveform, ASS/SRT, NVENC export planner, HyperFrames sidecar |
| `openprem-engine` | Single-writer engine: commands, snapshots/undo, jobs, autosave |

## Quickstart

### Requirements

- Rust 1.98+
- FFmpeg with `libx264`, `libass` (AAC muxing is built in) — NVENC/NVDEC used automatically when present
- Node.js 22+ **only if** you want HyperFrames-generated animations/transition screens

### Build

```bash
cargo build --workspace --release
```

### Wire it into Claude Code

`.mcp.json`:

```json
{
  "mcpServers": {
    "openprem": {
      "command": "/absolute/path/to/OpenPrem/target/release/openprem-mcp"
    }
  }
}
```

Then ask your agent to make an edit. Full workflow guide with the tool catalog:
**[docs/MCP.md](docs/MCP.md)**.

### Run the native editor

```bash
cargo run -p openprem-ui --release
```

Type a new project directory in the top bar and press **New**, drop video files
onto the window to import them, scrub the preview, drag clips, split/trim in
the inspector, and hit **Export MP4**. Every click goes through the same
engine commands the agent uses.

## Feature status (v0.1 MVP)

Done:

- Timeline with multiple video/audio tracks, linked A/V clips from phone footage
- Import (video/audio/stills), probe, thumbnails, contact sheets, waveforms
- Silence detection + ripple-delete dead space (agent or one click in the UI)
- Captions: add/edit/import SRT/VTT, styled burn-in via ASS at export
- Transitions: fade-through-color at cut points
- HyperFrames integration: `hyperframes_create` writes a managed composition
  and renders transparent ProRes 4444 (alpha) overlays or opaque MP4 screens
  via the `hyperframes` CLI, then auto-inserts the clip when the job completes
- Export: 4K MP4, NVENC with automatic x264 fallback, CUDA-accelerated decode,
  AAC audio, `+faststart`, bitrate scaled from the YouTube 4K recommendation,
  and every output ffprobe-verified before the job reports success
- Snapshot undo/redo, atomic `commands_batch`, revision checking for agents
- Preview frames for agents (`preview_frame` returns the composited image)

Roadmap (see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)):

- Real-time playback with audio clocking (preview is scrub-based today)
- Zero-copy NVDEC→GPU→NVENC compositing pipeline
- Edit proxies + waveform caching for smooth 4K scrubbing
- Native crossfades, keyframes, and color management (HDR→SDR today is passthrough)

## Time model

All times are integer **ticks**: 3,600,000 ticks = 1 second. This is frame-exact
for 23.976/29.97/59.94 NTSC rates *and* integer rates — no floating-point drift
ever reaches your edit decisions.

## Testing

```bash
cargo test --workspace        # unit + integration (uses ffmpeg to synthesize media)
cargo clippy --workspace --all-targets -- -D warnings
```

The MCP integration test drives the real binary over stdio JSON-RPC through the
full workflow, so the agent surface cannot silently regress.

## License

MIT — see [LICENSE](LICENSE).
