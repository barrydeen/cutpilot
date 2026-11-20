#!/usr/bin/env node
// Verifies that the agent-facing docs exist and actually document the
// MCP surface and workflows. Exits non-zero when anything is missing.

import { readFileSync, existsSync } from "node:fs";

const must = (cond, message) => {
  if (!cond) {
    console.error(`docs check FAILED: ${message}`);
    process.exit(1);
  }
};

const read = (p) => readFileSync(p, "utf8");

for (const file of [
  "README.md",
  "AGENTS.md",
  "docs/MCP.md",
  "docs/ARCHITECTURE.md",
  "LICENSE",
]) {
  must(existsSync(file), `${file} is missing`);
  const text = read(file);
  must(text.trim().length > 400, `${file} looks like a stub (${text.length} bytes)`);
}

const mcp = read("docs/MCP.md");
for (const tool of [
  "system_capabilities", "project_create", "project_open", "project_save",
  "project_get", "project_update_settings", "timeline_get",
  "media_import", "media_remove", "media_list", "media_probe",
  "media_detect_silence", "media_thumbnail", "media_contact_sheet",
  "media_waveform", "media_transcribe", "transcript_get",
  "transcript_to_captions", "track_add", "track_update", "track_move", "track_remove",
  "clip_insert", "clip_move", "clip_trim", "clip_split", "clip_remove",
  "clip_update", "clip_zoom_pan", "clip_set_speed", "clip_color_grade",
  "clip_duplicate", "clip_unlink", "marker_add", "marker_remove",
  "timeline_remove_ranges", "transition_add",
  "transition_remove", "caption_style_set", "caption_add", "caption_update",
  "caption_remove", "captions_import", "captions_export",
  "hyperframes_create", "hyperframes_update", "hyperframes_render",
  "preview_frame", "preview_seek", "editor_select", "editor_set_zoom",
  "editor_state_get", "render_start", "render_status", "render_cancel",
  "jobs_list", "history_undo", "history_redo", "history_list",
  "commands_execute", "commands_batch",
]) {
  must(mcp.includes(tool), `docs/MCP.md does not document tool \`${tool}\``);
}

for (const topic of [
  "3,600,000",              // tick semantics
  "descending",             // multi-range batching rule
  "expected_revision",      // concurrency guard
  "ProRes 4444",            // transparent overlay strategy
  "fallback_used",          // NVENC fallback honesty
  "hyperframes",            // sidecar integration
  "workflow",               // workflows present
]) {
  must(
    mcp.toLowerCase().includes(topic.toLowerCase()),
    `docs/MCP.md is missing required topic: ${topic}`
  );
}

const readme = read("README.md");
for (const topic of [
  "cutpilot-mcp", "cutpilot-ui", "mcpServers", "cargo build --workspace --release",
  "ticks", "NVENC", "undo", "HyperFrames",
]) {
  must(
    readme.toLowerCase().includes(topic.toLowerCase()),
    `README.md is missing required topic: ${topic}`
  );
}

const arch = read("docs/ARCHITECTURE.md");
for (const topic of [
  "command registry", "ticks", "snapshot", "gap map", "zero-copy", "playback",
]) {
  must(
    arch.toLowerCase().includes(topic.toLowerCase()),
    `docs/ARCHITECTURE.md is missing required topic: ${topic}`
  );
}

const agents = read("AGENTS.md");
for (const topic of ["UI-only", "KINDS", "edit.rs", "clippy", "mcp_e2e"]) {
  must(
    agents.includes(topic),
    `AGENTS.md is missing required topic: ${topic}`
  );
}

console.log("docs verification passed");
