# Contributing to CutPilot

Thank you for your interest in contributing to **CutPilot**!

## Architecture & Golden Rules

CutPilot is **MCP-first by construction**. Before submitting a PR, please review [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and keep these invariants intact:

1. **No UI-only semantic actions.** Every editing action must be a `Command` variant in `crates/cutpilot-core/src/commands.rs`, dispatched through `Engine::execute` by both `cutpilot-ui` and `cutpilot-mcp`.
2. **Keep command registry tables in sync.** `Command::kind()`, the `KINDS` constant, serde tags, and `commands::tests::sample_commands()` must all agree (enforced by unit tests).
3. **Pure edits stay in `cutpilot-core/src/edit.rs`.** Keep document mutations pure and deterministic so snapshot undo/redo and atomic `commands_batch` keep working. Put I/O (ffprobe, ffmpeg, jobs, persistence) in `cutpilot-media` / `cutpilot-engine`.
4. **Time is integer ticks** (`3,600,000 ticks = 1 s`). Never introduce `f64` timestamps into the persisted project model.
5. **Keep the export planner pure.** Changes to export filtergraphs belong in `crates/cutpilot-media/src/export.rs::build_export_plan` with unit tests on the generated ffmpeg arguments.

## Prerequisites

- Rust stable toolchain
- `ffmpeg` and `ffprobe` in `PATH` (with `libx264` and `libass`)
- C/C++ compiler, `cmake`, and `libclang` (required by `whisper-rs`)
- Node.js 22+ (for `scripts/check-docs.mjs` and optional HyperFrames rendering)

## Local Verification

Run the full verification suite before opening a pull request:

```bash
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
node scripts/check-docs.mjs
```

If you add or modify MCP tools or parameters, update [docs/MCP.md](docs/MCP.md) and extend `crates/cutpilot-mcp/tests/mcp_e2e.rs`.

## Pull Requests

1. Fork `https://github.com/barrydeen/cutpilot` and create a feature branch.
2. Keep commits focused and include tests for new behavior or bug fixes.
3. Open a pull request describing the change and how you verified it.

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
