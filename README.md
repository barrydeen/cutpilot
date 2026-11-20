# CutPilot

[![CI](https://github.com/barrydeen/cutpilot/actions/workflows/ci.yml/badge.svg)](https://github.com/barrydeen/cutpilot/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**An MCP-first, GPU-accelerated video editor built in Rust — co-piloted by AI agents and humans on the same live timeline.**

CutPilot is built around one architectural rule: *every semantic editing action lives in a versioned command registry shared equally by the MCP server and the native desktop UI.* An AI coding agent (Claude Code, Cursor, Codex, Windsurf, Antigravity, etc.) can run a **complete edit — import 4K footage and WAV audio, transcribe speech with embedded Whisper, cut dead space, add captions, transitions, and HyperFrames motion-graphics overlays, reorder tracks, and export at any resolution — entirely over MCP**, while you watch and fine-tune the exact same timeline live in the native editor with shared Undo/Redo history.

```
AI coding agent ──MCP/stdio──▶ cutpilot-mcp ──┐
                                              ├──(live IPC)──▶ engine ──▶ ffmpeg (NVDEC/CUDA/NVENC)
human ──────────native UI────▶ cutpilot-ui  ──┘                  ├──▶ whisper.cpp (embedded STT)
                                                                 └──▶ hyperframes CLI (Node)
```

## Workspace layout

| Binary | Purpose |
|---|---|
| `cutpilot-mcp` | Stdio MCP server exposing the full editing registry (~50 tools), auto-bridging to a live `cutpilot-ui` session or running standalone headless |
| `cutpilot-ui` | Native `egui`/`wgpu` desktop editor: multi-track timeline, live preview with real-time A/V + overlay compositing, inspector, Media Bin, MCP panel |

| Crate | Purpose |
|---|---|
| `cutpilot-core` | Tick-exact timeline document model + pure command application (zero I/O) |
| `cutpilot-media` | `ffprobe`, silence detection, frame/filmstrip/stream decoders, peak+RMS waveforms, embedded `whisper.cpp`, ASS/SRT captions, NVENC/CUDA export planner, HyperFrames sidecar |
| `cutpilot-engine` | Single-writer engine: command dispatcher, snapshot undo/redo, atomic batches, Unix-socket IPC bridge, job registry, autosave |

## Quickstart

### Prerequisites

- **Rust** (stable toolchain, 2021 edition)
- **FFmpeg & ffprobe** with `libx264` and `libass` (NVIDIA NVENC/NVDEC/CUDA are detected and used automatically when available, with automatic software fallback)
- **C/C++ build toolchain + CMake + libclang** (for compiling embedded `whisper.cpp`)
- **Node.js 22+** *(optional — only needed for rendering HyperFrames HTML animations)*

### Clone & Build

```bash
git clone https://github.com/barrydeen/cutpilot.git
cd cutpilot
cargo build --workspace --release
```

### Run the Native Editor

```bash
./target/release/cutpilot-ui
```

Use **File → New Project… (`Ctrl+N`)**, **File → Open Project… (`Ctrl+O`)**, **Import Media… (`Ctrl+I`)**, or drag & drop any video, audio (`.wav`, `.mp3`, `.flac`, …), or image file straight into the window.

### Connect an MCP Agent (Claude Code, Cursor, Codex, etc.)

Click **● Connect MCP Agent** in the top bar of `cutpilot-ui` for one-click copy/write configs, or add `.mcp.json`:

```json
{
  "mcpServers": {
    "cutpilot": {
      "command": "/absolute/path/to/cutpilot/target/release/cutpilot-mcp"
    }
  }
}
```

When `cutpilot-ui` is open, `cutpilot-mcp` automatically attaches over a local Unix socket so agent edits land live on your timeline. Pass `--standalone` (or `CUTPILOT_STANDALONE=1`) to run headless in-process.

Full agent guide and tool catalog: **[docs/MCP.md](docs/MCP.md)**.

## Highlights

- **Real-time multi-track A/V playback & instant scrubbing**: Press **Space** for live streaming playback with multi-track audio mixing (`A1`, `A2`, … with volume & fades) and live alpha-composited video overlays (`V2`, `V3`, …). High-resolution footage builds GPU short-GOP edit proxies, two-tier timeline filmstrips, and peak+RMS audio waveforms automatically in the background.
- **First-class WAV & multi-track audio**: Import `.wav` (16/24-bit PCM, 32-bit float, mono/stereo) and other audio formats directly via the default file picker or drag-and-drop; preview live mini-waveforms in the Media Bin, drag directly onto any audio track, or use `＋ Audio` / track menus and Inspector audio fade controls.
- **Built-in Whisper transcription**: `whisper.cpp` is compiled directly into CutPilot (no Python required). Click **🎙 Transcribe** or call `media_transcribe` / `transcript_get` to get timeline-mapped transcripts that stay accurate across trims and cuts, plus one-click captions.
- **Full timeline editing suite**: Select (`V`), drag-to-move with snapping, edge-drag trim, Razor (`C`), Split (`S`), In/Out (`I`/`O`) + Ripple Cut (`X`), Delete / Ripple Delete, track reorder (`▲`/`▼`), Mute/Hide/Lock, inline captions, and undo/redo history (`Ctrl+Z` / `Ctrl+Shift+Z`).
- **HyperFrames animated overlays & transitions**: Agents can author HTML/GSAP motion graphics via `hyperframes_create` (rendered to transparent ProRes 4444 MOV or opaque MP4), with a clean UI modal spinner lock while rendering.
- **Fast NVIDIA GPU export with resolution selection**: Zero-copy CUDA (`NVDEC → scale_cuda/pad_cuda → NVENC`) with H.264/HEVC/AV1 codecs, Fast/Balanced/Quality presets, selectable output resolution (`Match Project`, 4K, 1440p, 1080p, 720p, 480p, Vertical 9:16, Square 1:1, or Custom), `+faststart`, and automatic `ffprobe` verification.

## Tick-exact time model

All times are 64-bit integer **ticks** (`3,600,000 ticks = 1 second`), which divides cleanly for both integer frame rates (`24`, `25`, `30`, `50`, `60`) and NTSC rates (`23.976`, `29.97`, `59.94`) — eliminating floating-point drift across edits, transcripts, and exports.

## Testing & Verification

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
node scripts/check-docs.mjs
```

Integration tests synthesize real media with `ffmpeg` and drive the compiled `cutpilot-mcp` binary over stdio JSON-RPC end-to-end.

## Contributing

Contributions are welcome! See **[CONTRIBUTING.md](CONTRIBUTING.md)** and **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** to get started.

## License

MIT — see **[LICENSE](LICENSE)**.
