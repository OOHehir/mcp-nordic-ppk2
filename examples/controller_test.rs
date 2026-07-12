//! Live controller smoke test (former Phase 1 harness). Drives the device
//! directly, bypassing MCP. Run with the device attached:
//!   cargo run --example controller_test -- /dev/ttyACM0

use std::time::Duration;

use anyhow::Result;
use mcp_nordic_ppk2::controller::{Ppk2Controller, VDD_HW_MAX_MV};
use ppk2::types::MeasurementMode;

fn main() -> Result<()> {
    let port = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/ttyACM0".to_string());
    let voltage_mv = 3300u16;
    let sps = 10_000usize;

    println!("== controller smoke test ==\nport: {port}");

    let mut ctl =
        Ppk2Controller::connect(&port, MeasurementMode::Source, voltage_mv, VDD_HW_MAX_MV)?;
    println!("connected; {:?}", ctl.status());

    ctl.set_dut_power(true)?;
    ctl.start(sps, 5.0, None)?;
    println!("measuring @ {sps} sps ...");
    std::thread::sleep(Duration::from_secs(2));
    println!("mid-run: {:?}", ctl.status());

    let stats = ctl.stop()?;
    ctl.set_dut_power(false)?;
    println!(
        "stats: {} samples, mean {:.3} µA over {:.3} s, charge {:.3} µC",
        stats.samples, stats.mean_ua, stats.duration_s, stats.charge_uc
    );
    Ok(())
}
