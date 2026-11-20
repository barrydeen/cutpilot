//! `cutpilot-ui`: the native human editor. No webview anywhere — winit +
//! egui/wgpu, one window, GPU-composited UI. The UI talks to the exact same
//! engine command surface the MCP server exposes, and hosts a live IPC bridge
//! so MCP agents can edit the open timeline in real time.

mod app;
mod filepicker;
mod mcp_panel;
mod media_cache;
mod playback;
mod timeline;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1520.0, 920.0])
            .with_min_inner_size([960.0, 620.0])
            .with_title("CutPilot — MCP-First Video Editor"),
        ..Default::default()
    };
    eframe::run_native(
        "CutPilot",
        options,
        Box::new(|cc| Ok(Box::new(app::CutPilotApp::new(cc)))),
    )
}
