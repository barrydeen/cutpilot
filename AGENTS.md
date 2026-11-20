# AGENTS.md — working on CutPilot

CutPilot is MCP-first by construction. If you change editing behavior, the
change must land in the registry and stay agent-reachable.

## Golden rules

1. **No UI-only actions.** Every semantic action is a `Command` variant in
   `cutpilot-core/src/commands.rs`. The UI and MCP tools both route through
   `Engine::execute`. If you add a button, make sure a command exists.
2. **Keep the registry tables in sync.** `Command::kind()` (the exhaustive
   match), the `KINDS` const, and the serde tags must all agree —
   `commands::tests::serde_tags_match_kinds_exactly` constructs every
   variant and enforces this. Update all three together.
3. **Pure edits belong in `cutpilot-core/src/edit.rs`.** Anything that only
   mutates the document should be pure so undo, dry-run, and atomic batches
   keep working. I/O (probe, render, jobs) stays in the engine.
4. **Time is ticks** (`3,600,000 = 1 s`). Never introduce f64 timestamps into
   the document model.
5. **The export plan builder is pure.** Extend
   `cutpilot-media/src/export.rs::build_export_plan` and unit-test the
   filtergraph string; the runner only adds progress/cancel/fallback.
6. **Verify before claiming done:**
   - `cargo test --workspace` (integration tests synthesize media with ffmpeg)
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - The MCP e2e test spawns the real `cutpilot-mcp` binary over stdio
     JSON-RPC; if you touched tools or params, read it and extend it.

## Adding a command (checklist)

1. Add the variant to `Command` (struct variant, snake_case serde).
2. Add its name to `KINDS` and to the exhaustive `kind()` match.
3. Add one instance to `sample_commands()` in the commands test.
4. Implement it: `edit.rs::apply_edit` (pure) or `engine.rs::execute_inner`
   (I/O). Mutations must snapshot (`mutate`) and autosave.
5. Add an MCP param struct in `cutpilot-mcp/src/params.rs` and either a typed
   tool or rely on `commands_execute` coverage; document it in `docs/MCP.md`.
6. Add tests: unit (core), integration (engine), and extend `mcp_e2e.rs`
   if it changes the wire surface.

## Project layout

```
crates/cutpilot-core     model, ticks, commands, pure edit application
crates/cutpilot-media     ffprobe/silence/frames/waveform/ASS/SRT/export/hyperframes
crates/cutpilot-engine   dispatcher, snapshots, jobs, persistence, preview
crates/cutpilot-mcp      rmcp server (tools.rs is the surface)
crates/cutpilot-ui       native egui app (app.rs, timeline.rs)
docs/MCP.md              agent-facing usage guide — keep the catalog current
```

## Gates

`GATES.md` at the repo root tracks acceptance. Run the runnable ones before
reporting work:

```bash
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
node scripts/check-docs.mjs
```
