//! PPK2 MCP server entry point. Serves the tool surface over stdio so it can be
//! launched by an MCP client (Claude Code / Claude Desktop) as a subprocess.

use anyhow::{Result, anyhow};
use mcp_nordic_ppk2::controller::{DEFAULT_MAX_VOLTAGE_MV, VDD_HW_MAX_MV, VDD_MIN_MV};
use mcp_nordic_ppk2::server::Ppk2Server;
use rmcp::{ServiceExt, transport::stdio};

/// Resolve the source-voltage safety ceiling (mV): `--max-voltage-mv <N>` (or
/// `--max-voltage-mv=<N>`) takes precedence over the `PPK2_MAX_VOLTAGE_MV`
/// environment variable; absent both, it defaults to [`DEFAULT_MAX_VOLTAGE_MV`].
/// The result is clamped into the hardware range so it can never widen it.
fn resolve_max_voltage_mv() -> Result<u16> {
    let mut cli: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--max-voltage-mv" {
            cli = args.next();
        } else if let Some(v) = a.strip_prefix("--max-voltage-mv=") {
            cli = Some(v.to_string());
        }
    }

    let raw = cli.or_else(|| std::env::var("PPK2_MAX_VOLTAGE_MV").ok());
    let mv = match raw {
        Some(s) => s
            .trim()
            .parse::<u16>()
            .map_err(|_| anyhow!("invalid max voltage {s:?}: expected millivolts (e.g. 3300)"))?,
        None => DEFAULT_MAX_VOLTAGE_MV,
    };
    Ok(mv.clamp(VDD_MIN_MV, VDD_HW_MAX_MV))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr; stdout is reserved for the JSON-RPC stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let max_voltage_mv = resolve_max_voltage_mv()?;
    tracing::info!(max_voltage_mv, "starting PPK2 MCP server on stdio");
    let service = Ppk2Server::new(max_voltage_mv).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
