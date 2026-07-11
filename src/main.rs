//! PPK2 MCP server entry point. Serves the tool surface over stdio so it can be
//! launched by an MCP client (Claude Code / Claude Desktop) as a subprocess.

use anyhow::Result;
use mcp_nordic_ppk2::server::Ppk2Server;
use rmcp::{transport::stdio, ServiceExt};

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr; stdout is reserved for the JSON-RPC stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("starting PPK2 MCP server on stdio");
    let service = Ppk2Server::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
