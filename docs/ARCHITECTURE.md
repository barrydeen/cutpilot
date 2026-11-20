# CutPilot architecture

## Design rule #1: one command registry, two equal frontends

`cutpilot-core` defines a serde-tagged `Command` enum. **Every** semantic
action — down to viewer state like the playhead — is a registry command:

- The MCP layer is a thin typed shell over it.
- The UI calls the same engine methods in-process.
- `commands_execute` / `commands_batch` cover the whole registry generically,
  so no action can ever be UI-only. A parity test constructs one instance of
  every variant and asserts the serde tag, `Command::kind()`, and the `KINDS`
  table all agree.

```
cutpilot-core    model + pure command application (no I/O, fully tested)
      │
cutpilot-media  ffmpeg/ffprobe interop: probe, silence, frames, waveform,
      │          ASS/SRT, the pure export-plan builder, the process runner
      │          (progress + cancellation), HyperFrames sidecar
      ▼
cutpilot-engine single-writer engine: snapshots (undo), revisions,
      │          atomic batches, job registry, autosave, preview compositing
      ├── cutpilot-mcp  typed tools → spawn_blocking → engine (stdio)
      └── cutpilot-ui   native egui app → engine (in-process)
```

## Time

Integer ticks at 1/3,600,000 s. Divides exactly for 24/25/30/50/60 and the
NTSC 1001-based rates, so edits and exports are drift-free. Source seeking
converts to fractional seconds only at the ffmpeg boundary.

## Project format

A project is a directory: `project.json` (small human-readable JSON with
schema version + revision) plus `hyperframes/<slug>/` managed compositions.
Media is referenced by path, never copied. Saves are atomic
(tmp file + rename); the engine autosaves after every committed edit.

## Undo

Snapshot-based: the pre-edit document is cloned onto the undo stack. Project
documents are a few KB, so this is cheap and always correct (no inverse
command bugs). Undo/redo bump the revision monotonically.

## Export pipeline (v0.1)

A **pure function** builds one ffmpeg filtergraph from the document — no
wall clocks, no mutable render state:

- Base track clips: accurate input seek (`-ss`/`-t` before `-i`, CUDA
  hwaccel decode when NVENC is used), letterbox scale/pad to the canvas,
  per-clip fade filters for transitions, `fps` normalization, concat.
- Overlay video tracks: alpha-preserving scale (`format=yuva420p`),
  opacity via `colorchannelmixer`, alpha fades, `overlay` with
  `enable='between(t,start,end)'` + `eof_action=pass`.
- Audio: per-clip file audio or `anullsrc` beds, concat, music clips with
  gain/`afade`/`adelay`, `amix(..., duration=first, normalize=0)`.
- Captions: ASS sidecar written next to the output, burned with the `ass`
  filter, times remapped through the same gap-collapse map as overlays.
- Encode: `h264_nvenc` (detected via `ffmpeg -encoders`) or `libx264`;
  AAC audio; `+faststart`. One automatic software fallback retry covers a
  broken NVENC/driver combo.

A gap map (timeline → export time) collapses base-track gaps consistently for
overlays, music, and captions. Every finished export is ffprobe-verified
(duration ±250 ms, canvas size, audio presence) before its job reports done.

The argument builder is unit-tested directly (`export::tests`), so filter
regressions are caught without running ffmpeg.

## HyperFrames sidecar

Chromium must never sit on the preview or export frame path. Compositions
live inside the project, render to cached intermediates (ProRes 4444 MOV with
alpha for overlays, MP4 for opaque screens), and become ordinary assets that
the engine inserts automatically when a status poll observes the finished
job. Re-render via `hyperframes_render` after editing the source.

## Performance & live bridge architecture

- **Live MCP ↔ UI IPC bridge**: `cutpilot-ui` hosts a local Unix domain socket
  serving its in-memory `Engine`. `cutpilot-mcp` auto-connects when the UI is
  running so agent edits appear live on the human's timeline with shared undo
  history (or runs standalone when headless / `CUTPILOT_STANDALONE=1`).
- **Real-time playback & instant scrubbing**: persistent streaming `ffmpeg`
  video + audio decoders for smooth Spacebar playback; latest-wins coalesced
  scrub slot with frame-quantized 64-frame LRU cache, single reused GPU texture,
  and two-tier fast-seek/exact-seek scrubbing.
- **Background GPU edit proxies, filmstrips & detailed waveforms**: high-res/long-GOP
  footage automatically transcodes on NVENC (`p1 ull`, `-g 15 -bf 0`) into
  `cache/proxies/` alongside disk-cached high-resolution peak+RMS audio waveforms
  (`cache/waveforms/*.wf2`), progressive two-tier timeline video filmstrip atlases
  (`cache/filmstrips/*.fs1`) with on-demand zoomed tile decoding, and bin thumbnails.
- **Built-in Whisper speech-to-text**: `whisper.cpp` is compiled directly into
  `cutpilot-media` via `whisper-rs` (GGML models auto-download into
  `~/.cache/cutpilot/whisper` on first use). Stored transcripts persist in
  `project.json` and dynamically map to current timeline clip positions for both
  the UI and MCP agents (`media_transcribe`, `transcript_get`, `transcript_to_captions`).
- **Zero-copy NVIDIA GPU export**: cut-only timelines stay 100% resident on
  CUDA VRAM (`NVDEC → scale_cuda/pad_cuda → concat → h264_nvenc/hevc_nvenc/av1_nvenc`)
  with zero PCIe readbacks, cascading cleanly to hybrid NVDEC+NVENC or libx264
  if needed.

## Platform

Linux-first (this build targets Wayland/X11 with the NVIDIA driver);
Windows is expected to work since everything above is cross-platform, but is
not yet exercised in CI.
