//! Core PPK2 controller: wraps the `ppk2` crate with a background drain thread,
//! a rolling sample buffer, running statistics, and CSV export.
//!
//! The PPK2 streams ~100k samples/s. The `ppk2` crate already parses that stream
//! on its own worker thread and delivers aggregated [`MeasurementMatch`] messages
//! over an mpsc channel (each message is the average of `100_000 / sps` raw
//! samples). This controller adds a *drain* thread that continuously empties that
//! channel into shared state so the channel never backs up, and exposes aggregate
//! queries suitable for an MCP tool surface. Current-only for v1 (logic pins ignored).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ppk2::{
    Ppk2,
    measurement::MeasurementMatch,
    types::{DevicePower, Level, LogicPortPins, MeasurementMode, SourceVoltage},
};
use serde::Serialize;

/// 1 mAh expressed in microcoulombs (µC): 1 mAh = 3.6 C = 3.6e6 µC.
const UC_PER_MAH: f64 = 3.6e6;

/// PPK2 source-voltage hardware limits (millivolts). The device can only supply
/// within this window; the `ppk2` crate silently clamps out-of-range requests,
/// so we validate against these ourselves and reject instead.
pub const VDD_MIN_MV: u16 = 800;
pub const VDD_HW_MAX_MV: u16 = 5000;

/// Default source-voltage safety ceiling (mV) when the operator sets none. 3300
/// mV (3.3 V) is the most common logic level, so it's the safe-by-default cap;
/// raise it via `--max-voltage-mv` / `PPK2_MAX_VOLTAGE_MV` for 5 V parts.
pub const DEFAULT_MAX_VOLTAGE_MV: u16 = 3300;

/// Reject a source voltage that is out of the PPK2's range, or above the
/// operator-configured safety ceiling, rather than silently clamping it (which
/// is what the underlying crate would otherwise do — a footgun for a DUT).
fn validate_voltage(mv: u16, ceiling_mv: u16) -> Result<()> {
    if mv < VDD_MIN_MV {
        bail!("voltage {mv} mV is below the PPK2 minimum of {VDD_MIN_MV} mV");
    }
    if mv > VDD_HW_MAX_MV {
        bail!("voltage {mv} mV exceeds the PPK2 hardware maximum of {VDD_HW_MAX_MV} mV");
    }
    if mv > ceiling_mv {
        bail!(
            "voltage {mv} mV exceeds the configured safety ceiling of {ceiling_mv} mV \
             (raise it with --max-voltage-mv or PPK2_MAX_VOLTAGE_MV)"
        );
    }
    Ok(())
}

/// PPK2 USB identifiers (Nordic Semiconductor).
pub const PPK2_VID: u16 = 0x1915;
pub const PPK2_PID: u16 = 0xc00a;
/// The PPK2 exposes two CDC-ACM interfaces with the same serial number; interface
/// 1 is the control/measurement port. Selecting by interface number (rather than
/// device path) is stable across USB re-enumeration.
const PPK2_CONTROL_INTERFACE: u8 = 1;

/// From candidate `(port_name, usb_interface)` pairs, pick the PPK2 control port:
/// prefer USB interface 1, else fall back to the lowest-named port. Pure so it
/// can be unit-tested without hardware.
fn select_control_port(mut candidates: Vec<(String, Option<u8>)>) -> Option<String> {
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates
        .iter()
        .find(|(_, iface)| *iface == Some(PPK2_CONTROL_INTERFACE))
        .or_else(|| candidates.first())
        .map(|(name, _)| name.clone())
}

/// Locate the PPK2's control serial port by USB VID:PID, disambiguating the two
/// ACM interfaces in favour of the control one. Unlike a fixed `/dev/ttyACM0`,
/// this follows the device across a re-enumeration.
pub fn discover_ppk2_port() -> Result<String> {
    use serialport::SerialPortType::UsbPort;
    let candidates: Vec<(String, Option<u8>)> = serialport::available_ports()
        .context("enumerating serial ports")?
        .into_iter()
        .filter_map(|p| match p.port_type {
            UsbPort(usb) if usb.vid == PPK2_VID && usb.pid == PPK2_PID => {
                Some((p.port_name, usb.interface))
            }
            _ => None,
        })
        .collect();
    select_control_port(candidates).ok_or_else(|| {
        anyhow::anyhow!(
            "no PPK2 (USB {PPK2_VID:04x}:{PPK2_PID:04x}) found — is it plugged in, \
             and are permissions set (dialout group / nrf-udev rules)?"
        )
    })
}

/// Aggregate statistics over a measurement session.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Number of aggregated samples collected.
    pub samples: u64,
    /// Duration derived from sample count and sps (seconds).
    pub duration_s: f64,
    /// Mean current (µA).
    pub mean_ua: f64,
    /// Minimum sampled current (µA).
    pub min_ua: f64,
    /// Maximum sampled current (µA).
    pub max_ua: f64,
    /// Population standard deviation of current (µA).
    pub stddev_ua: f64,
    /// Time-integrated charge (µC).
    pub charge_uc: f64,
    /// Time-integrated charge (mAh).
    pub charge_mah: f64,
    /// Mean current expressed in mA.
    pub avg_ma: f64,
    /// Fraction of samples (0.0–1.0) each digital channel D0–D7 read high.
    pub pin_duty: [f64; 8],
}

/// Shared state mutated by the drain thread and read by query methods.
struct Shared {
    buf: VecDeque<(f32, u8)>, // rolling window of (current µA, digital bitmask), bounded by `cap`
    cap: usize,
    count: u64,
    sum: f64,
    sum_sq: f64,
    min: f32,
    max: f32,
    pin_high: [u64; 8], // per-channel count of samples read high (lifetime, not windowed)
    sps: usize,
}

impl Shared {
    fn new(sps: usize, cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(1 << 20)),
            cap,
            count: 0,
            sum: 0.0,
            sum_sq: 0.0,
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            pin_high: [0; 8],
            sps,
        }
    }

    fn push(&mut self, ua: f32, pins: u8) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back((ua, pins));
        self.count += 1;
        let v = ua as f64;
        self.sum += v;
        self.sum_sq += v * v;
        if ua < self.min {
            self.min = ua;
        }
        if ua > self.max {
            self.max = ua;
        }
        for (i, c) in self.pin_high.iter_mut().enumerate() {
            if pins & (1 << i) != 0 {
                *c += 1;
            }
        }
    }

    fn stats(&self) -> Stats {
        let n = self.count as f64;
        if self.count == 0 {
            return Stats {
                samples: 0,
                duration_s: 0.0,
                mean_ua: 0.0,
                min_ua: 0.0,
                max_ua: 0.0,
                stddev_ua: 0.0,
                charge_uc: 0.0,
                charge_mah: 0.0,
                avg_ma: 0.0,
                pin_duty: [0.0; 8],
            };
        }
        let mut pin_duty = [0.0f64; 8];
        for (i, d) in pin_duty.iter_mut().enumerate() {
            *d = self.pin_high[i] as f64 / n;
        }
        let mean = self.sum / n;
        let var = (self.sum_sq / n - mean * mean).max(0.0);
        // Each aggregated sample covers dt = 1/sps seconds; charge = Σ(I · dt).
        let charge_uc = if self.sps > 0 {
            self.sum / self.sps as f64
        } else {
            0.0
        };
        let duration_s = if self.sps > 0 {
            n / self.sps as f64
        } else {
            0.0
        };
        Stats {
            samples: self.count,
            duration_s,
            mean_ua: mean,
            min_ua: self.min as f64,
            max_ua: self.max as f64,
            stddev_ua: var.sqrt(),
            charge_uc,
            charge_mah: charge_uc / UC_PER_MAH,
            avg_ma: mean / 1000.0,
            pin_duty,
        }
    }
}

/// Parse an 8-character trigger pattern into per-pin levels. Position 0 (leftmost)
/// is D0. Accepts `1`/`H`/`h` = high, `0`/`L`/`l` = low, `X`/`x`/`.`/`*` = either.
pub fn parse_trigger(pattern: &str) -> Result<[Level; 8]> {
    let chars: Vec<char> = pattern.trim().chars().collect();
    if chars.len() != 8 {
        bail!(
            "trigger must be exactly 8 characters (D0..D7), got {}",
            chars.len()
        );
    }
    let mut levels = [Level::Either; 8];
    for (i, c) in chars.iter().enumerate() {
        levels[i] = match c {
            '1' | 'H' | 'h' => Level::High,
            '0' | 'L' | 'l' => Level::Low,
            'X' | 'x' | '.' | '*' => Level::Either,
            other => bail!("invalid trigger char '{other}' at position {i} (use 1/0/X)"),
        };
    }
    Ok(levels)
}

/// Pack a combined measurement's logic port state into a bitmask (bit i = D`i` high).
fn pins_to_byte(p: &LogicPortPins) -> u8 {
    let mut b = 0u8;
    for i in 0..8 {
        if p.pin_is_high(i) {
            b |= 1 << i;
        }
    }
    b
}

/// Render a trigger pattern back to an 8-char string (D0 leftmost): 1=high, 0=low, X=either.
fn trigger_to_string(levels: &[Level; 8]) -> String {
    levels
        .iter()
        .map(|l| match l {
            Level::High => '1',
            Level::Low => '0',
            Level::Either => 'X',
        })
        .collect()
}

/// An in-flight measurement session.
struct Session {
    shared: Arc<Mutex<Shared>>,
    stop_flag: Arc<AtomicBool>,
    drain: Option<JoinHandle<()>>,
    /// Crate-provided closure that stops the device worker and returns the `Ppk2`.
    stop_device: Option<Box<dyn FnOnce() -> std::result::Result<Ppk2, ppk2::Error> + Send>>,
    sps: usize,
    started: Instant,
    /// Active digital trigger pattern, if any (only matching windows are recorded).
    trigger: Option<[Level; 8]>,
}

enum State {
    Idle(Ppk2),
    Measuring(Session),
    /// Transient/irrecoverable: the device handle was lost; caller must reconnect.
    Broken,
}

/// High-level PPK2 controller.
pub struct Ppk2Controller {
    state: State,
    port: String,
    mode: MeasurementMode,
    voltage_mv: u16,
    dut_power: bool,
    /// Operator-configured maximum source voltage (mV); requests above this are rejected.
    max_voltage_mv: u16,
    /// Retained samples from the most recent session (for CSV export after stop).
    last_samples: Option<(usize, Vec<(f32, u8)>)>,
    last_stats: Option<Stats>,
}

/// Snapshot of controller state for a status query.
#[derive(Debug, Clone)]
pub struct Status {
    pub port: String,
    pub connected: bool,
    pub measuring: bool,
    pub broken: bool,
    pub mode: MeasurementMode,
    pub voltage_mv: u16,
    pub max_voltage_mv: u16,
    /// Last-commanded DUT power state. Only reliable while `!broken`; when
    /// `broken`, the true state is unknown — a failed power write may have left
    /// the DUT in either state, so callers must treat this as last-known-only.
    pub dut_power: bool,
    pub sps: Option<usize>,
    pub buffered_samples: Option<usize>,
    pub elapsed_s: Option<f64>,
    /// Active digital trigger pattern (D0 leftmost), if any.
    pub trigger: Option<String>,
}

impl Ppk2Controller {
    /// Open the device, read metadata, and set the source voltage. Leaves the
    /// controller idle with DUT power off. `max_voltage_mv` is the operator safety
    /// ceiling: `voltage_mv` (and any later change) above it is rejected.
    ///
    /// Safety: the requested voltage is validated *before* the device is touched,
    /// and DUT power is explicitly forced off on the hardware immediately after
    /// opening — so a device left powered by a previous session cannot keep
    /// driving the DUT. If that disable can't be confirmed, connect fails closed.
    pub fn connect(
        port: &str,
        mode: MeasurementMode,
        voltage_mv: u16,
        max_voltage_mv: u16,
    ) -> Result<Self> {
        validate_voltage(voltage_mv, max_voltage_mv)?;
        let mut ppk2 = Ppk2::new(port, mode).with_context(|| {
            format!("opening PPK2 on {port} (in use, missing permissions, or unplugged?)")
        })?;
        ppk2.set_device_power(DevicePower::Disabled)
            .context("forcing DUT power off on connect")?;
        ppk2.set_source_voltage(SourceVoltage::from_millivolts(voltage_mv))
            .context("setting source voltage")?;
        Ok(Self {
            state: State::Idle(ppk2),
            port: port.to_string(),
            mode,
            voltage_mv,
            dut_power: false,
            max_voltage_mv,
            last_samples: None,
            last_stats: None,
        })
    }

    /// Take the idle device handle for a write, leaving the controller marked
    /// `Broken` for the duration. The caller restores `Idle` on success; on a
    /// device IO error it stays `Broken` (the handle is dropped), so a failed
    /// write is detected instead of leaving a live-looking but dead connection.
    fn take_idle_for_write(&mut self, busy_msg: &str) -> Result<Ppk2> {
        match std::mem::replace(&mut self.state, State::Broken) {
            State::Idle(ppk2) => Ok(ppk2),
            State::Measuring(s) => {
                self.state = State::Measuring(s);
                bail!("{busy_msg}");
            }
            State::Broken => bail!("controller is broken; reconnect (ppk2_connect)"),
        }
    }

    /// Set the source voltage (mV). Only valid while idle. Rejected (not clamped)
    /// if out of the PPK2 range or above the configured safety ceiling. A device
    /// IO failure marks the controller broken.
    pub fn set_source_voltage(&mut self, mv: u16) -> Result<()> {
        validate_voltage(mv, self.max_voltage_mv)?;
        let mut ppk2 = self.take_idle_for_write("stop measuring before changing voltage")?;
        match ppk2.set_source_voltage(SourceVoltage::from_millivolts(mv)) {
            Ok(()) => {
                self.voltage_mv = mv;
                self.state = State::Idle(ppk2);
                Ok(())
            }
            // state stays Broken; ppk2 (dead handle) is dropped here.
            Err(e) => Err(anyhow::Error::new(e).context("setting source voltage (link lost)")),
        }
    }

    /// Enable/disable DUT power. Only valid while idle. A device IO failure marks
    /// the controller broken — critically, if disabling power fails, the real DUT
    /// power state becomes unknown (see [`Status::dut_power`]).
    pub fn set_dut_power(&mut self, on: bool) -> Result<()> {
        let mut ppk2 = self.take_idle_for_write("stop measuring before toggling DUT power")?;
        let p = if on {
            DevicePower::Enabled
        } else {
            DevicePower::Disabled
        };
        match ppk2.set_device_power(p) {
            Ok(()) => {
                self.dut_power = on;
                self.state = State::Idle(ppk2);
                Ok(())
            }
            // state stays Broken; `dut_power` keeps its last-commanded value, but
            // status reports it as unknown while broken.
            Err(e) => Err(anyhow::Error::new(e).context("setting DUT power (link lost)")),
        }
    }

    /// Begin a background measurement session at the given samples-per-second and
    /// retention window (seconds of samples kept in the rolling buffer). When
    /// `trigger` is set, only aggregated windows whose digital pins match the
    /// pattern are recorded (High/Low require that level; Either is a wildcard).
    pub fn start(
        &mut self,
        sps: usize,
        retention_secs: f64,
        trigger: Option<[Level; 8]>,
    ) -> Result<()> {
        if sps == 0 || sps > 100_000 {
            bail!("sps must be in 1..=100000");
        }
        let ppk2 = match std::mem::replace(&mut self.state, State::Broken) {
            State::Idle(p) => p,
            State::Measuring(s) => {
                self.state = State::Measuring(s);
                bail!("already measuring");
            }
            State::Broken => bail!("controller is broken; reconnect"),
        };

        // Consumes `ppk2`; on error the device handle is gone -> Broken.
        let pins = match trigger {
            Some(levels) => LogicPortPins::with_levels(levels),
            None => LogicPortPins::default(), // all Either -> everything matches
        };
        let (rx, stop) = ppk2
            .start_measurement_matching(pins, sps)
            .context("start_measurement_matching")?;

        let cap = ((sps as f64) * retention_secs).ceil().max(1.0) as usize;
        let shared = Arc::new(Mutex::new(Shared::new(sps, cap)));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let drain = {
            let shared = shared.clone();
            let stop_flag = stop_flag.clone();
            thread::spawn(move || {
                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(MeasurementMatch::Match(m)) => shared
                            .lock()
                            .unwrap()
                            .push(m.micro_amps, pins_to_byte(&m.pins)),
                        // NoMatch = window filtered out by the trigger; nothing recorded.
                        Ok(MeasurementMatch::NoMatch) => {}
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
        };

        self.state = State::Measuring(Session {
            shared,
            stop_flag,
            drain: Some(drain),
            stop_device: Some(Box::new(stop)),
            sps,
            started: Instant::now(),
            trigger,
        });
        Ok(())
    }

    /// Stop the current session, returning its final statistics. Order matters:
    /// stop the device worker first (so it exits cleanly without a dangling send),
    /// then tear down the drain thread.
    pub fn stop(&mut self) -> Result<Stats> {
        let mut session = match std::mem::replace(&mut self.state, State::Broken) {
            State::Measuring(s) => s,
            other => {
                self.state = other;
                bail!("not measuring");
            }
        };

        let stop_device = session.stop_device.take().expect("stop_device present");
        let ppk2 = match stop_device() {
            Ok(p) => p,
            Err(e) => {
                session.stop_flag.store(true, Ordering::Relaxed);
                if let Some(h) = session.drain.take() {
                    let _ = h.join();
                }
                // device handle lost
                return Err(anyhow::Error::new(e).context("stopping device worker"));
            }
        };

        session.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = session.drain.take() {
            let _ = h.join();
        }

        let guard = session.shared.lock().unwrap();
        let stats = guard.stats();
        self.last_samples = Some((guard.sps, guard.buf.iter().copied().collect()));
        drop(guard);

        self.last_stats = Some(stats.clone());
        self.state = State::Idle(ppk2);
        Ok(stats)
    }

    /// Convenience: start, capture for `duration`, stop, return stats. Retains the
    /// full capture window so no samples are dropped. `trigger` behaves as in
    /// [`Ppk2Controller::start`].
    pub fn measure(
        &mut self,
        duration: Duration,
        sps: usize,
        trigger: Option<[Level; 8]>,
    ) -> Result<Stats> {
        self.start(sps, duration.as_secs_f64() + 1.0, trigger)?;
        thread::sleep(duration);
        self.stop()
    }

    /// Current statistics: live session if measuring, else the last session's stats.
    pub fn stats(&self) -> Option<Stats> {
        match &self.state {
            State::Measuring(s) => Some(s.shared.lock().unwrap().stats()),
            _ => self.last_stats.clone(),
        }
    }

    /// Write retained samples to CSV (`t_seconds,current_ua,d0..d7`). Uses the live
    /// buffer if measuring, otherwise the most recent session's retained window.
    pub fn export_csv(&self, path: &str) -> Result<usize> {
        let (sps, samples): (usize, Vec<(f32, u8)>) = match &self.state {
            State::Measuring(s) => {
                let g = s.shared.lock().unwrap();
                (g.sps, g.buf.iter().copied().collect())
            }
            _ => match &self.last_samples {
                Some((sps, v)) => (*sps, v.clone()),
                None => bail!("no samples to export"),
            },
        };
        let f = File::create(path).with_context(|| format!("creating {path}"))?;
        let mut w = BufWriter::new(f);
        writeln!(w, "t_seconds,current_ua,d0,d1,d2,d3,d4,d5,d6,d7")?;
        let dt = if sps > 0 { 1.0 / sps as f64 } else { 0.0 };
        for (i, (ua, pins)) in samples.iter().enumerate() {
            write!(w, "{:.6},{:.4}", i as f64 * dt, ua)?;
            for b in 0..8 {
                write!(w, ",{}", (pins >> b) & 1)?;
            }
            writeln!(w)?;
        }
        w.flush()?;
        Ok(samples.len())
    }

    /// Snapshot of controller status.
    pub fn status(&self) -> Status {
        let (measuring, broken) = match &self.state {
            State::Measuring(_) => (true, false),
            State::Idle(_) => (false, false),
            State::Broken => (false, true),
        };
        let (sps, buffered, elapsed, trigger) = match &self.state {
            State::Measuring(s) => {
                let g = s.shared.lock().unwrap();
                (
                    Some(s.sps),
                    Some(g.buf.len()),
                    Some(s.started.elapsed().as_secs_f64()),
                    s.trigger.as_ref().map(trigger_to_string),
                )
            }
            _ => (None, None, None, None),
        };
        Status {
            port: self.port.clone(),
            connected: !broken,
            measuring,
            broken,
            mode: self.mode,
            voltage_mv: self.voltage_mv,
            max_voltage_mv: self.max_voltage_mv,
            dut_power: self.dut_power,
            sps,
            buffered_samples: buffered,
            elapsed_s: elapsed,
            trigger,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_within_range_and_ceiling_ok() {
        assert!(validate_voltage(3300, 5000).is_ok());
        assert!(validate_voltage(VDD_MIN_MV, VDD_HW_MAX_MV).is_ok());
        assert!(validate_voltage(VDD_HW_MAX_MV, VDD_HW_MAX_MV).is_ok());
        assert!(validate_voltage(3300, 3300).is_ok()); // exactly at ceiling
    }

    #[test]
    fn voltage_below_minimum_rejected() {
        assert!(validate_voltage(VDD_MIN_MV - 1, VDD_HW_MAX_MV).is_err());
        assert!(validate_voltage(0, VDD_HW_MAX_MV).is_err());
    }

    #[test]
    fn voltage_above_hardware_max_rejected() {
        assert!(validate_voltage(VDD_HW_MAX_MV + 1, VDD_HW_MAX_MV).is_err());
        // The classic extra-zero fat-finger (3300 -> 33000) must fail loudly,
        // not clamp to 5000 V and cook a 3.3 V part.
        assert!(validate_voltage(33000, VDD_HW_MAX_MV).is_err());
    }

    #[test]
    fn voltage_above_ceiling_rejected() {
        assert!(validate_voltage(3301, 3300).is_err());
        assert!(validate_voltage(VDD_HW_MAX_MV, 3300).is_err());
    }

    fn p(name: &str, iface: Option<u8>) -> (String, Option<u8>) {
        (name.to_string(), iface)
    }

    #[test]
    fn control_port_prefers_interface_1() {
        // Real PPK2 layout: control = iface 1 (ttyACM0), secondary = iface 3.
        let got = select_control_port(vec![p("/dev/ttyACM2", Some(3)), p("/dev/ttyACM0", Some(1))]);
        assert_eq!(got.as_deref(), Some("/dev/ttyACM0"));
    }

    #[test]
    fn control_port_interface_1_wins_over_lower_path() {
        // Interface number, not device path, decides — survives re-enumeration.
        let got = select_control_port(vec![p("/dev/ttyACM0", Some(3)), p("/dev/ttyACM2", Some(1))]);
        assert_eq!(got.as_deref(), Some("/dev/ttyACM2"));
    }

    #[test]
    fn control_port_falls_back_to_lowest_path_without_interface_info() {
        let got = select_control_port(vec![p("/dev/ttyACM2", None), p("/dev/ttyACM0", None)]);
        assert_eq!(got.as_deref(), Some("/dev/ttyACM0"));
    }

    #[test]
    fn control_port_none_when_empty() {
        assert_eq!(select_control_port(vec![]), None);
    }
}
