# Gates: OpenPrem MVP build

OWNS: **

Scope: Build the OpenPrem Rust workspace end to end — core edit model, media/ffmpeg
layer (probe, silence, frames, NVENC export), engine (commands, undo, jobs),
MCP server exposing every action, native egui editor UI, tests, and agent docs —
so a coding agent can drive a full edit (import Pixel-style 4K footage, cut dead
space, add transitions/animations/captions, export MP4) and a human can browse
and adjust the same project in the native UI.

- [ ] G1: Workspace and all targets compile cleanly
  CHECK: cargo build --workspace --all-targets
  EXPECT: Finished `dev` profile
  EVIDENCE: pending

- [ ] G2: Unit tests pass for the pure edit model, time math, ffmpeg arg builder, ASS/SRT, and silence parser
  CHECK: cargo test --workspace --lib
  EXPECT: test result: ok
  EVIDENCE: pending

- [ ] G3: Integration tests pass, including an end-to-end MCP JSON-RPC session that creates a project, imports generated test media, removes detected silence, adds captions and a transition, starts an export job, and verifies the exported MP4 with ffprobe
  CHECK: cargo test --workspace --tests
  EXPECT: test result: ok
  EVIDENCE: pending

- [ ] G4: Clippy passes with warnings denied across the workspace
  CHECK: cargo clippy --workspace --all-targets -- -D warnings
  EXPECT: Finished `dev` profile
  EVIDENCE: pending

- [ ] G5: Agent-facing docs exist and describe the MCP tool catalog, the dead-space workflow, HyperFrames integration, and the UI
  CHECK: node scripts/check-docs.mjs
  EXPECT: docs verification passed
  EVIDENCE: pending

- [ ] G6: Manual UI smoke test — launch `cargo run -p openprem-ui --release`, import a real video, scrub the preview, trim/split a clip, and start an export from the UI
  EVIDENCE: pending

- [ ] G7: Manual HyperFrames render — run the documented `hyperframes_create` workflow against a real MCP session so `npx hyperframes` renders a transparent ProRes overlay and it composites into the timeline and export
  EVIDENCE: pending
