# Gates: OpenPrem MVP build

OWNS: **

Scope: Build the OpenPrem Rust workspace end to end — core edit model, media/ffmpeg
layer (probe, silence, frames, NVENC export), engine (commands, undo, jobs),
MCP server exposing every action, native egui editor UI, tests, and agent docs —
so a coding agent can drive a full edit (import Pixel-style 4K footage, cut dead
space, add transitions/animations/captions, export MP4) and a human can browse
and adjust the same project in the native UI.

- [x] G1: Workspace and all targets compile cleanly
  CHECK: cargo build --workspace --all-targets
  EXPECT: Finished `dev` profile
  EVIDENCE: automatic-evidence=v1; definition-sha256=7fc5c1750b4cff9bdf3681c2fd891c43fc2dbd7bca7f47b2eaccd0068e63479f; exit=0; EXPECT=matched; output-sha256=03fa4ac44e7af416954dca6fd782933036abf98656a28757f250c7dfb3a6e39e; output-bytes=155; shell=/bin/sh; cwd=/home/barry/Projects/OpenPrem; path=74375e68222c/20 entries

- [x] G2: Unit tests pass for the pure edit model, time math, ffmpeg arg builder, ASS/SRT, and silence parser
  CHECK: cargo test --workspace --lib
  EXPECT: test result: ok
  EVIDENCE: automatic-evidence=v1; definition-sha256=9ff7accda3b3b435d86c0ee65ad464fc091573f2b89c17f43849af9c5128bbb5; exit=0; EXPECT=matched; output-sha256=dbc78d8f30b911597d172f59f2e57839e259d99c8997f85640a367c870693a90; output-bytes=2413; shell=/bin/sh; cwd=/home/barry/Projects/OpenPrem; path=74375e68222c/20 entries

- [x] G3: Integration tests pass, including an end-to-end MCP JSON-RPC session that creates a project, imports generated test media, removes detected silence, adds captions and a transition, starts an export job, and verifies the exported MP4 with ffprobe
  CHECK: cargo test --workspace --tests
  EXPECT: test result: ok
  EVIDENCE: automatic-evidence=v1; definition-sha256=a047f6d59b50fb089287def6544291dc2f212de7d2000eef32e2d3911a623e09; exit=0; EXPECT=matched; output-sha256=16444d078996284eeaccd38b96ab11ab590e9c93a60bc6eb02e0c9acdadfbec5; output-bytes=2869; shell=/bin/sh; cwd=/home/barry/Projects/OpenPrem; path=74375e68222c/20 entries

- [x] G4: Clippy passes with warnings denied across the workspace
  CHECK: cargo clippy --workspace --all-targets -- -D warnings
  EXPECT: Finished `dev` profile
  EVIDENCE: automatic-evidence=v1; definition-sha256=222e6fe2be2620512ba67e486783c3e9d9dca33e0801b723446352181391fa7b; exit=0; EXPECT=matched; output-sha256=299f16a2c523be18fef769975d2df00e0505521db047540feea3a74b94666887; output-bytes=72; shell=/bin/sh; cwd=/home/barry/Projects/OpenPrem; path=74375e68222c/20 entries

- [x] G5: Agent-facing docs exist and describe the MCP tool catalog, the dead-space workflow, HyperFrames integration, and the UI
  CHECK: node scripts/check-docs.mjs
  EXPECT: docs verification passed
  EVIDENCE: automatic-evidence=v1; definition-sha256=c42a7b79e63f3907ba8ece34f61df88183fb3a7892bc1ae6926030f89b5d8ead; exit=0; EXPECT=matched; output-sha256=a0b8a56b9e9cb377e1fea7fae83d4d29732b8b0c504e680f2cb1a3871fed8f8a; output-bytes=25; shell=/bin/sh; cwd=/home/barry/Projects/OpenPrem; path=74375e68222c/20 entries

- [ ] G6: Manual UI smoke test — launch `cargo run -p openprem-ui --release`, import a real video, scrub the preview, trim/split a clip, and start an export from the UI
  EVIDENCE: pending

- [ ] G7: Manual HyperFrames render — run the documented `hyperframes_create` workflow against a real MCP session so `npx hyperframes` renders a transparent ProRes overlay and it composites into the timeline and export
  EVIDENCE: pending
