//! Dedicated "Connect MCP Agent" modal and live bridge monitor.
//!
//! Makes wiring Claude Code, Cursor, Windsurf, Codex, or Claude Desktop into
//! CutPilot a 5-second copy/click experience — and shows live agent activity
//! flowing over the local IPC bridge directly into the open UI timeline.

use eframe::egui;
use cutpilot_engine::IpcStats;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ClientTab {
    #[default]
    ClaudeCode,
    Cursor,
    Generic,
}

#[derive(Default)]
pub struct McpPanelState {
    pub open: bool,
    tab: ClientTab,
    copied_notice: Option<(String, std::time::Instant)>,
    resolved_bin: Option<(PathBuf, bool)>,
}

impl McpPanelState {
    fn binary_path(&mut self) -> &(PathBuf, bool) {
        self.resolved_bin.get_or_insert_with(detect_mcp_binary)
    }

    fn set_notice(&mut self, msg: impl Into<String>) {
        self.copied_notice = Some((msg.into(), std::time::Instant::now()));
    }

    /// Top-bar status pill: click to open/close the Connect MCP Agent panel.
    pub fn top_bar_button(&mut self, ui: &mut egui::Ui, stats: &Arc<Mutex<IpcStats>>) {
        let (listening, calls) = stats
            .lock()
            .map(|s| (s.listening, s.total_calls))
            .unwrap_or((false, 0));

        let (dot_color, text) = if calls > 0 {
            (
                egui::Color32::from_rgb(76, 217, 123),
                format!("● MCP Live ({calls})"),
            )
        } else if listening {
            (
                egui::Color32::from_rgb(96, 175, 255),
                "● Connect MCP Agent".to_string(),
            )
        } else {
            (
                egui::Color32::from_rgb(240, 173, 78),
                "○ MCP Setup".to_string(),
            )
        };

        let btn = egui::Button::new(egui::RichText::new(text).color(dot_color).strong())
            .fill(if self.open {
                egui::Color32::from_rgb(44, 58, 82)
            } else {
                egui::Color32::from_rgb(32, 38, 48)
            })
            .stroke(egui::Stroke::new(1.0, dot_color.linear_multiply(0.55)));

        if ui
            .add(btn)
            .on_hover_text("Connect Claude Code, Cursor, Codex, or any MCP agent to edit this timeline live")
            .clicked()
        {
            self.open = !self.open;
            self.resolved_bin = None; // refresh check
        }
    }

    /// Draw the modal window when open.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        stats: &Arc<Mutex<IpcStats>>,
        project_dir: Option<&Path>,
    ) {
        if !self.open {
            return;
        }
        let mut is_open = self.open;
        let (bin_path, bin_exists) = self.binary_path().clone();
        let bin_str = bin_path.display().to_string();

        let snapshot = stats.lock().ok().map(|s| s.clone());

        egui::Window::new("Connect MCP Agent")
            .open(&mut is_open)
            .resizable(true)
            .default_width(660.0)
            .default_height(560.0)
            .show(ctx, |ui| {
                // Live bridge status banner.
                if let Some(st) = &snapshot {
                    let (badge_col, badge_txt) = if st.listening && st.total_calls > 0 {
                        (
                            egui::Color32::from_rgb(76, 217, 123),
                            format!(
                                "LIVE SYNC ACTIVE — {} agent call(s) received in this session",
                                st.total_calls
                            ),
                        )
                    } else if st.listening {
                        (
                            egui::Color32::from_rgb(96, 175, 255),
                            "LIVE SYNC READY — edits from your MCP agent will appear live on this timeline"
                                .to_string(),
                        )
                    } else {
                        (
                            egui::Color32::from_rgb(240, 110, 110),
                            st.last_error
                                .clone()
                                .unwrap_or_else(|| "Live bridge unavailable".into()),
                        )
                    };
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(26, 32, 42))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .stroke(egui::Stroke::new(1.0, badge_col.linear_multiply(0.5)))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(badge_txt).color(badge_col).strong());
                            ui.label(
                                egui::RichText::new(format!(
                                    "Live IPC socket: {}",
                                    st.socket_path.display()
                                ))
                                .small()
                                .monospace()
                                .color(ui.visuals().weak_text_color()),
                            );
                        });
                }

                ui.add_space(8.0);

                // Binary check.
                if !bin_exists {
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(62, 42, 24))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(
                                    "⚠ `cutpilot-mcp` binary not found yet — build it once:",
                                )
                                .color(egui::Color32::from_rgb(255, 200, 100))
                                .strong(),
                            );
                            let build_cmd = "cargo build --workspace --release";
                            ui.horizontal(|ui| {
                                ui.code(build_cmd);
                                if ui.button("Copy build command").clicked() {
                                    ctx.copy_text(build_cmd.into());
                                    self.set_notice("Copied build command!");
                                }
                            });
                        });
                    ui.add_space(6.0);
                }

                // Client tabs.
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, ClientTab::ClaudeCode, "Claude Code");
                    ui.selectable_value(&mut self.tab, ClientTab::Cursor, "Cursor / Windsurf");
                    ui.selectable_value(
                        &mut self.tab,
                        ClientTab::Generic,
                        "Claude Desktop / Codex / Generic",
                    );
                });
                ui.separator();

                let mcp_json = serde_json::to_string_pretty(&serde_json::json!({
                    "mcpServers": {
                        "cutpilot": {
                            "command": bin_str,
                        }
                    }
                }))
                .unwrap_or_default();

                match self.tab {
                    ClientTab::ClaudeCode => {
                        ui.label(egui::RichText::new("Option 1 — One-line CLI setup (recommended):").strong());
                        let cli_cmd = format!("claude mcp add cutpilot -- \"{bin_str}\"");
                        ui.horizontal(|ui| {
                            let mut txt = cli_cmd.clone();
                            ui.add(
                                egui::TextEdit::singleline(&mut txt)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(ui.available_width() - 90.0),
                            );
                            if ui.button("Copy").clicked() {
                                ctx.copy_text(cli_cmd);
                                self.set_notice("Copied Claude Code CLI command!");
                            }
                        });
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new("Option 2 — Project `.mcp.json`:").strong());
                        let mut snippet = mcp_json.clone();
                        ui.add(
                            egui::TextEdit::multiline(&mut snippet)
                                .font(egui::TextStyle::Monospace)
                                .desired_rows(7)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Copy .mcp.json").clicked() {
                                ctx.copy_text(mcp_json.clone());
                                self.set_notice("Copied .mcp.json!");
                            }
                            if let Some(dir) = project_dir {
                                if ui.button("Write .mcp.json into Project Folder").clicked() {
                                    let dest = dir.join(".mcp.json");
                                    match std::fs::write(&dest, &mcp_json) {
                                        Ok(_) => self.set_notice(format!(
                                            "Wrote {}",
                                            dest.display()
                                        )),
                                        Err(e) => self.set_notice(format!("Write failed: {e}")),
                                    }
                                }
                            }
                        });
                    }
                    ClientTab::Cursor => {
                        ui.label(
                            "Paste into `.cursor/mcp.json` (project) or `~/.cursor/mcp.json` (global):",
                        );
                        let mut snippet = mcp_json.clone();
                        ui.add(
                            egui::TextEdit::multiline(&mut snippet)
                                .font(egui::TextStyle::Monospace)
                                .desired_rows(8)
                                .desired_width(f32::INFINITY),
                        );
                        if ui.button("Copy Cursor MCP config").clicked() {
                            ctx.copy_text(mcp_json.clone());
                            self.set_notice("Copied Cursor MCP JSON!");
                        }
                    }
                    ClientTab::Generic => {
                        ui.label(
                            "Use this stdio MCP server config in Claude Desktop, Codex, Antigravity, or any MCP client:",
                        );
                        let mut snippet = mcp_json.clone();
                        ui.add(
                            egui::TextEdit::multiline(&mut snippet)
                                .font(egui::TextStyle::Monospace)
                                .desired_rows(8)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Copy JSON").clicked() {
                                ctx.copy_text(mcp_json.clone());
                                self.set_notice("Copied MCP config JSON!");
                            }
                            if ui.button("Copy binary path").clicked() {
                                ctx.copy_text(bin_str.clone());
                                self.set_notice("Copied binary path!");
                            }
                        });
                    }
                }

                // Copied feedback toast.
                if let Some((msg, at)) = &self.copied_notice {
                    if at.elapsed().as_secs() < 5 {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(format!("✓ {msg}"))
                                .color(egui::Color32::from_rgb(76, 217, 123))
                                .small(),
                        );
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.label(egui::RichText::new("Try asking your agent:").strong());
                let sample_prompt = "Inspect the open CutPilot timeline with timeline_get, detect and ripple-cut dead space longer than 0.6s, add clean captions, and export with NVIDIA NVENC.";
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("“{sample_prompt}”"))
                            .small()
                            .italics()
                            .color(ui.visuals().weak_text_color()),
                    );
                    if ui.small_button("Copy").clicked() {
                        ctx.copy_text(sample_prompt.into());
                        self.set_notice("Copied prompt!");
                    }
                });

                // Live activity log.
                ui.add_space(8.0);
                ui.separator();
                ui.label(egui::RichText::new("Live MCP Activity Log").strong());
                egui::ScrollArea::vertical()
                    .max_height(130.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let empty = snapshot
                            .as_ref()
                            .map(|s| s.recent.is_empty())
                            .unwrap_or(true);
                        if empty {
                            ui.label(
                                egui::RichText::new(
                                    "Waiting for an MCP agent to call tools… (every tool call appears here live)",
                                )
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                        } else if let Some(st) = &snapshot {
                            for entry in &st.recent {
                                let icon = if entry.ok { "✓" } else { "✗" };
                                let col = if entry.ok {
                                    egui::Color32::from_rgb(130, 220, 155)
                                } else {
                                    egui::Color32::from_rgb(245, 120, 120)
                                };
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(icon).color(col).monospace().small());
                                    ui.label(
                                        egui::RichText::new(&entry.kind)
                                            .strong()
                                            .monospace()
                                            .small(),
                                    );
                                    ui.label(
                                        egui::RichText::new(&entry.message)
                                            .small()
                                            .color(ui.visuals().weak_text_color()),
                                    );
                                });
                            }
                        }
                    });
            });

        self.open = is_open;
    }
}

/// Locate the `cutpilot-mcp` binary cleanly for copy-paste MCP config.
fn detect_mcp_binary() -> (PathBuf, bool) {
    let bin_name = if cfg!(windows) {
        "cutpilot-mcp.exe"
    } else {
        "cutpilot-mcp"
    };

    // 1. Sibling binary right next to the running `cutpilot-ui` executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(bin_name);
            if sibling.is_file() {
                return (sibling, true);
            }
            // Also check target/release when running via cargo run (target/debug).
            if let Some(target_dir) = dir.parent() {
                let rel = target_dir.join("release").join(bin_name);
                if rel.is_file() {
                    return (rel, true);
                }
            }
        }
    }

    // 2. Relative to current working directory.
    if let Ok(cwd) = std::env::current_dir() {
        for sub in ["target/release", "target/debug"] {
            let p = cwd.join(sub).join(bin_name);
            if p.is_file() {
                return (p, true);
            }
        }
        return (cwd.join("target/release").join(bin_name), false);
    }

    (PathBuf::from(bin_name), false)
}
