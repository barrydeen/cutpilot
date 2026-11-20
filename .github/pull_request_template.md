## Summary
Briefly describe what this PR changes and why.

## Checklist
- [ ] Every new semantic action is a `Command` variant in `cutpilot-core` (no UI-only actions)
- [ ] `cargo build --workspace --all-targets` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `node scripts/check-docs.mjs` passes
- [ ] Updated `docs/MCP.md` if MCP tools or parameters changed
