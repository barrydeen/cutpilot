//! `openprem-mcp`: stdio MCP server exposing every OpenPrem action to
//! coding agents. Configure in `.mcp.json`:
//!
//! ```json
//! { "mcpServers": { "openprem": {
//!     "command": "/path/to/target/release/openprem-mcp" } } }
//! ```

mod params;
mod tools;

fn main() -> anyhow::Result<()> {
    // Diagnostics go to stderr; stdout is the MCP protocol.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(tools::serve());
    if let Err(e) = result {
        eprintln!("openprem-mcp: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}
