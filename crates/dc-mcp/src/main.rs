//! `dumb-coder-mcp` — the MCP server binary. A thin stdio shell over
//! [`dc_mcp::serve`]: resolve config from the environment, build the tool set,
//! then pump JSON-RPC over stdin/stdout until the client disconnects.

use std::io::{self, BufReader};

fn main() -> io::Result<()> {
    let cfg = dc_mcp::Config::from_env();
    // A one-line banner on stderr (stdout is the JSON-RPC channel — must stay clean).
    eprintln!(
        "dumb-coder-mcp: backends [{}] ({}), binary {}, yolo={}",
        cfg.base_urls.join(", "),
        cfg.model,
        cfg.binary,
        cfg.yolo
    );
    let tools = dc_mcp::build_tools(cfg);

    let stdin = io::stdin();
    let stdout = io::stdout();
    dc_mcp::serve(&tools, BufReader::new(stdin.lock()), stdout.lock())
}
