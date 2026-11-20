//! `openprem-ui`: the native human editor. No webview anywhere — winit +
//! egui/wgpu, one window, GPU-composited UI. The UI talks to the exact same
//! engine command surface the MCP server exposes.

mod app;
mod filepicker;
mod timeline;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_title("OpenPrem"),
        ..Default::default()
    };
    eframe::run_native(
        "OpenPrem",
        options,
        Box::new(|cc| Ok(Box::new(app::OpenPremApp::new(cc)))),
    )
}
