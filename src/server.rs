//! rmcp stdio server exposing the PPK2 controller as MCP tools.
//!
//! One shared [`Ppk2Controller`] lives behind a mutex. Device operations are
//! blocking (serial IO, and `ppk2_measure` sleeps for the capture window), so
//! every tool body runs on `spawn_blocking` to avoid stalling the async runtime.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use ppk2::types::MeasurementMode;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;

use crate::controller::{parse_trigger, Ppk2Controller, Stats};
use ppk2::types::Level;

type SharedCtl = Arc<Mutex<Option<Ppk2Controller>>>;

/// MCP server holding a single PPK2 controller instance.
pub struct Ppk2Server {
    ctl: SharedCtl,
    /// Operator safety ceiling (mV) applied to every connect/configure request.
    max_voltage_mv: u16,
    // Populated by `Self::tool_router()` and consumed by the `#[tool_handler]`
    // macro's generated dispatch; not read directly, hence the allow.
    #[allow(dead_code)]
    tool_router: ToolRouter<Ppk2Server>,
}

// ---- tool argument schemas ----

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConnectArgs {
    /// Serial port path. Defaults to /dev/ttyACM0.
    #[serde(default)]
    pub port: Option<String>,
    /// Measurement mode: "source" (PPK2 supplies the DUT) or "ampere" (external supply, inline meter). Defaults to source.
    #[serde(default)]
    pub mode: Option<String>,
    /// Source voltage in millivolts (800–5000). Rejected (not clamped) if out of range or above the server's configured safety ceiling. Defaults to 3300.
    #[serde(default)]
    pub voltage_mv: Option<u16>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfigureArgs {
    /// New source voltage in millivolts (800–5000). Rejected (not clamped) if out of range or above the server's configured safety ceiling. Omit to leave unchanged. Only valid while not measuring.
    #[serde(default)]
    pub voltage_mv: Option<u16>,
    /// Turn DUT power on/off. Omit to leave unchanged. Only valid while not measuring.
    #[serde(default)]
    pub dut_power: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MeasureArgs {
    /// Capture duration in seconds.
    pub duration_s: f64,
    /// Samples per second (1–100000). Higher = finer resolution, more memory. Defaults to 10000.
    #[serde(default)]
    pub sps: Option<usize>,
    /// Optional digital trigger: an 8-char pattern (D0 leftmost) of 1=high, 0=low, X=either. Only windows matching the pattern are recorded. E.g. "X1XXXXXX" records only while D1 is high.
    #[serde(default)]
    pub trigger: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartArgs {
    /// Samples per second (1–100000). Defaults to 10000.
    #[serde(default)]
    pub sps: Option<usize>,
    /// Rolling buffer retention window in seconds. Defaults to 60.
    #[serde(default)]
    pub retention_s: Option<f64>,
    /// Optional digital trigger: an 8-char pattern (D0 leftmost) of 1=high, 0=low, X=either. Only windows matching the pattern are recorded. E.g. "X1XXXXXX" records only while D1 is high.
    #[serde(default)]
    pub trigger: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExportArgs {
    /// Filesystem path to write the CSV (columns: t_seconds,current_ua).
    pub path: String,
}

// ---- helpers (non-tool impl) ----

impl Ppk2Server {
    /// Create a server that will never apply a source voltage above `max_voltage_mv`.
    pub fn new(max_voltage_mv: u16) -> Self {
        Self {
            ctl: Arc::new(Mutex::new(None)),
            max_voltage_mv,
            tool_router: Self::tool_router(),
        }
    }

    /// Run a blocking closure against the shared controller off the async runtime.
    async fn blocking<F, T>(&self, f: F) -> Result<T, McpError>
    where
        F: FnOnce(&mut Option<Ppk2Controller>) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let ctl = self.ctl.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = ctl.lock().unwrap();
            f(&mut guard)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("worker panicked: {e}"), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

impl Default for Ppk2Server {
    fn default() -> Self {
        Self::new(crate::controller::VDD_HW_MAX_MV)
    }
}

/// Borrow the connected controller or produce a helpful error.
fn require(g: &mut Option<Ppk2Controller>) -> Result<&mut Ppk2Controller> {
    g.as_mut()
        .ok_or_else(|| anyhow!("not connected — call ppk2_connect first"))
}

fn text(s: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn format_stats(label: &str, s: &Stats) -> String {
    let json = serde_json::to_string(s).unwrap_or_default();
    let duty = s
        .pin_duty
        .iter()
        .enumerate()
        .map(|(i, d)| format!("D{i}={:.0}%", d * 100.0))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{label}\n\
         samples:   {}\n\
         duration:  {:.3} s\n\
         mean:      {:.3} µA ({:.4} mA)\n\
         min/max:   {:.3} / {:.3} µA\n\
         stddev:    {:.3} µA\n\
         charge:    {:.3} µC ({:.6} mAh)\n\
         pin duty:  {duty}\n\
         json: {json}",
        s.samples, s.duration_s, s.mean_ua, s.avg_ma, s.min_ua, s.max_ua, s.stddev_ua,
        s.charge_uc, s.charge_mah
    )
}

/// Parse an optional trigger string into pin levels, or `None`.
fn trigger_arg(t: Option<String>) -> Result<Option<[Level; 8]>> {
    match t {
        Some(p) => Ok(Some(parse_trigger(&p)?)),
        None => Ok(None),
    }
}

// ---- tools ----

#[tool_router]
impl Ppk2Server {
    #[tool(description = "Connect to the PPK2 over serial, read calibration metadata, and set the source voltage. Leaves DUT power off.")]
    async fn ppk2_connect(
        &self,
        Parameters(args): Parameters<ConnectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let max_voltage_mv = self.max_voltage_mv;
        let s = self
            .blocking(move |g| {
                let port = args.port.unwrap_or_else(|| "/dev/ttyACM0".to_string());
                let mode = match args.mode.as_deref() {
                    Some(m) => m.parse::<MeasurementMode>().map_err(|e| anyhow!("{e}"))?,
                    None => MeasurementMode::Source,
                };
                let voltage = args.voltage_mv.unwrap_or(3300);
                let ctl = Ppk2Controller::connect(&port, mode, voltage, max_voltage_mv)?;
                let st = ctl.status();
                *g = Some(ctl);
                Ok(format!(
                    "connected: port={} mode={:?} voltage={}mV dut_power={}",
                    st.port, st.mode, st.voltage_mv, st.dut_power
                ))
            })
            .await?;
        Ok(text(s))
    }

    #[tool(description = "Set source voltage (mV) and/or toggle DUT power. Only valid while not measuring.")]
    async fn ppk2_configure(
        &self,
        Parameters(args): Parameters<ConfigureArgs>,
    ) -> Result<CallToolResult, McpError> {
        let s = self
            .blocking(move |g| {
                let c = require(g)?;
                if let Some(mv) = args.voltage_mv {
                    c.set_source_voltage(mv)?;
                }
                if let Some(on) = args.dut_power {
                    c.set_dut_power(on)?;
                }
                let st = c.status();
                Ok(format!(
                    "configured: voltage={}mV dut_power={}",
                    st.voltage_mv, st.dut_power
                ))
            })
            .await?;
        Ok(text(s))
    }

    #[tool(description = "Capture current for a fixed duration and return summary statistics (mean/min/max/stddev µA, charge). Blocks for the duration.")]
    async fn ppk2_measure(
        &self,
        Parameters(args): Parameters<MeasureArgs>,
    ) -> Result<CallToolResult, McpError> {
        let s = self
            .blocking(move |g| {
                if !(args.duration_s > 0.0 && args.duration_s <= 3600.0) {
                    return Err(anyhow!("duration_s must be in (0, 3600]"));
                }
                let sps = args.sps.unwrap_or(10_000);
                let trigger = trigger_arg(args.trigger)?;
                let c = require(g)?;
                let stats = c.measure(Duration::from_secs_f64(args.duration_s), sps, trigger)?;
                Ok(format_stats("measurement complete", &stats))
            })
            .await?;
        Ok(text(s))
    }

    #[tool(description = "Start a background measurement session. Use ppk2_stats to query and ppk2_stop to finish.")]
    async fn ppk2_start(
        &self,
        Parameters(args): Parameters<StartArgs>,
    ) -> Result<CallToolResult, McpError> {
        let s = self
            .blocking(move |g| {
                let sps = args.sps.unwrap_or(10_000);
                let retention = args.retention_s.unwrap_or(60.0);
                let trig_desc = match &args.trigger {
                    Some(p) => format!(", trigger={p}"),
                    None => String::new(),
                };
                let trigger = trigger_arg(args.trigger)?;
                let c = require(g)?;
                c.start(sps, retention, trigger)?;
                Ok(format!("measuring @ {sps} sps (retention {retention}s{trig_desc})"))
            })
            .await?;
        Ok(text(s))
    }

    #[tool(description = "Stop the current background measurement session and return its final statistics.")]
    async fn ppk2_stop(&self) -> Result<CallToolResult, McpError> {
        let s = self
            .blocking(move |g| {
                let c = require(g)?;
                let stats = c.stop()?;
                Ok(format_stats("session stopped", &stats))
            })
            .await?;
        Ok(text(s))
    }

    #[tool(description = "Return statistics for the live session (if measuring) or the most recent session.")]
    async fn ppk2_stats(&self) -> Result<CallToolResult, McpError> {
        let s = self
            .blocking(move |g| {
                let c = require(g)?;
                match c.stats() {
                    Some(stats) => Ok(format_stats("current stats", &stats)),
                    None => Ok("no statistics yet — start or run a measurement first".to_string()),
                }
            })
            .await?;
        Ok(text(s))
    }

    #[tool(description = "Export the retained sample buffer to a CSV file (columns: t_seconds,current_ua).")]
    async fn ppk2_export_csv(
        &self,
        Parameters(args): Parameters<ExportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let s = self
            .blocking(move |g| {
                let c = require(g)?;
                let n = c.export_csv(&args.path)?;
                Ok(format!("wrote {n} samples to {}", args.path))
            })
            .await?;
        Ok(text(s))
    }

    #[tool(description = "Report connection state, mode, voltage, DUT power, and buffer status.")]
    async fn ppk2_status(&self) -> Result<CallToolResult, McpError> {
        let s = self
            .blocking(move |g| {
                match g.as_ref() {
                    None => Ok("not connected".to_string()),
                    Some(c) => {
                        let st = c.status();
                        Ok(format!(
                            "port={} connected={} measuring={} broken={} mode={:?} voltage={}mV max_voltage={}mV dut_power={} sps={:?} buffered={:?} elapsed_s={:?} trigger={:?}",
                            st.port, st.connected, st.measuring, st.broken, st.mode,
                            st.voltage_mv, st.max_voltage_mv, st.dut_power, st.sps, st.buffered_samples, st.elapsed_s, st.trigger
                        ))
                    }
                }
            })
            .await?;
        Ok(text(s))
    }
}

#[tool_handler]
impl ServerHandler for Ppk2Server {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls a Nordic Power Profiler Kit II (PPK2) for current measurement. \
             Call ppk2_connect first (defaults: /dev/ttyACM0, source mode, 3300 mV), \
             then ppk2_configure to set voltage / DUT power, and ppk2_measure for a \
             fixed capture or ppk2_start/ppk2_stop for background sessions. Current-only.",
        )
    }
}
