//! `cutpilot-mcp`: stdio MCP server exposing every CutPilot action to
//! coding agents.
//!
//! When the native `cutpilot-ui` editor is running, `cutpilot-mcp`
//! automatically connects to it over the local IPC bridge so agent edits
//! appear live on the human's timeline. Pass `--standalone` (or set
//! `CUTPILOT_STANDALONE=1`) to run strictly headless in-process.

mod params;
mod tools;

fn main() -> anyhow::Result<()> {
    let standalone = std::env::args().any(|a| a == "--standalone")
        || cutpilot_engine::ipc::is_standalone_env();
    // Diagnostics go to stderr; stdout is the MCP protocol.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(tools::serve(standalone));
    if let Err(e) = result {
        eprintln!("cutpilot-mcp: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}
