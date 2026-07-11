# mcp-nordic-ppk2

An [MCP](https://modelcontextprotocol.io) server that exposes a **Nordic Power
Profiler Kit II (PPK2)** to an MCP client (Claude Code / Claude Desktop) over
stdio. It lets a model connect to the device, power a DUT in source mode,
capture current, read the 8 digital channels, and use them as triggers —
returning summary statistics or a CSV.

Built in Rust on top of the [`ppk2`](https://crates.io/crates/ppk2) crate
(device protocol) and [`rmcp`](https://crates.io/crates/rmcp) (official MCP SDK).

## How it works

The PPK2 streams ~100,000 samples/s. Handing that to a model is neither useful
nor possible, so the server:

- runs a **background drain thread** that continuously empties the sample stream
  into a bounded rolling buffer (the serial link never backs up), and
- exposes **aggregate** tools — mean/min/max/stddev current, integrated charge,
  per-channel duty cycle — plus CSV export for raw data. Raw samples go to a
  file, never into the chat.

## Tools

| Tool | Arguments | Description |
|------|-----------|-------------|
| `ppk2_connect` | `port?`, `mode?` (`source`\|`ampere`), `voltage_mv?` | Open the device, read calibration, set voltage. Defaults: `/dev/ttyACM0`, source, 3300 mV. |
| `ppk2_configure` | `voltage_mv?`, `dut_power?` | Set voltage and/or toggle DUT power. Idle only. |
| `ppk2_measure` | `duration_s`, `sps?`, `trigger?` | Capture for a fixed time, return stats. Blocks for the duration. |
| `ppk2_start` | `sps?`, `retention_s?`, `trigger?` | Begin a background session. |
| `ppk2_stop` | — | Stop the session, return final stats. |
| `ppk2_stats` | — | Stats for the live or most recent session. |
| `ppk2_export_csv` | `path` | Write the retained buffer to CSV (`t_seconds,current_ua,d0..d7`). |
| `ppk2_status` | — | Connection/mode/voltage/DUT/buffer/trigger state. |

`sps` defaults to 10000 (0.1 ms resolution). Charge is reported in µC and mAh.

### Digital channels & triggers

The PPK2 has 8 digital logic inputs (D0–D7). Every sample records their state:

- **CSV export** includes one column per channel (`d0`…`d7`).
- **Stats** include each channel's duty cycle (fraction of samples read high).
- **Triggers**: pass `trigger` as an 8-character pattern (D0 leftmost) where
  `1` = high, `0` = low, `X` = either. Only windows matching the pattern are
  recorded — everything else is dropped. For example, `trigger: "X1XXXXXX"`
  captures current only while **D1 is high**, so you can profile a device's
  consumption during a specific activity signalled on a GPIO.

## Safety

In source mode the PPK2 supplies the DUT, so a wrong voltage can destroy it.
The server has three guardrails:

- **Voltage is rejected, not clamped.** Requests outside the PPK2's 800–5000 mV
  range return an error instead of being silently clamped (which is what the
  underlying crate does on its own). A fat-fingered `33000` fails loudly rather
  than quietly applying 5.0 V.
- **Operator ceiling.** Set `--max-voltage-mv <mV>` (or the
  `PPK2_MAX_VOLTAGE_MV` env var) to your DUT's rating and the server will refuse
  any higher voltage — so the model *cannot* exceed it. Defaults to the 5000 mV
  hardware maximum if unset. This is the real per-DUT rail: **set it to match
  your part** (e.g. `3300` for a 3.3 V board).

  ```json
  { "mcpServers": { "ppk2": { "command": "mcp-nordic-ppk2",
      "args": ["--max-voltage-mv", "3300"] } } }
  ```
- **Power off at startup, fail closed.** On connect the server explicitly forces
  DUT power **off** on the hardware (not just in software) before doing anything
  else, and aborts the connect if it can't confirm that — so a device left
  powered by a previous crashed session can't keep driving the DUT. Power is only
  applied by an explicit `ppk2_configure` with `dut_power: true`.
  Consequence by design: reconnecting cuts power to an already-powered DUT.

The ceiling protects against wrong *values*; it cannot know your DUT's true
rating, so setting `--max-voltage-mv` per bench is what actually protects the part.

## Install

Requires a Rust toolchain and, on Linux, libudev for the `serialport` crate:

```sh
sudo apt install libudev-dev pkg-config   # Debian/Ubuntu
cargo install --path .                     # builds and installs `mcp-nordic-ppk2` to ~/.cargo/bin
```

### Device access (Linux)

To reach the PPK2's serial port without root, install Nordic's official udev
rules — [`NordicSemiconductor/nrf-udev`](https://github.com/NordicSemiconductor/nrf-udev)
— which grant access to all Nordic devices (USB vendor `1915`, including the
PPK2 at `1915:c00a`):

```sh
# Debian/Ubuntu: grab the latest .deb from the releases page, then
sudo dpkg -i nrf-udev_*.deb
# other distros: copy 71-nrf.rules into /etc/udev/rules.d/ and reload
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Replug the device afterward. As a simpler alternative, add your user to the
`dialout` group (`sudo usermod -aG dialout $USER`, then log out and back in).

<details>
<summary>Building without <code>libudev-dev</code> (e.g. no root)</summary>

The `serialport` crate needs libudev at build time. If you can't install
`libudev-dev` but the runtime `libudev.so.1` is present, you can point
`pkg-config` at a userspace shim: create a `libudev.pc` and an unversioned
`libudev.so` symlink to the runtime library, and set `PKG_CONFIG_PATH` to the
directory containing the `.pc`. (This repo's own dev sandbox does exactly that;
those files are git-ignored.)
</details>

## Use with Claude Code

`.mcp.json` in this directory registers the server (assumes `mcp-nordic-ppk2` is on
your `PATH` via `cargo install`):

```json
{ "mcpServers": { "ppk2": { "command": "mcp-nordic-ppk2" } } }
```

Claude Code auto-discovers it. Then just ask, e.g. *"connect to the PPK2 and
measure current for 5 seconds at 3.3 V."*

## Use with Claude Desktop

Add to `claude_desktop_config.json` (use an absolute path if `mcp-nordic-ppk2` isn't on
Claude Desktop's `PATH`):

```json
{ "mcpServers": { "ppk2": { "command": "mcp-nordic-ppk2" } } }
```

## Device notes

- The PPK2 enumerates **two** serial ports; use the first (`/dev/ttyACM0`,
  USB interface 01).
- Voltage/DUT-power changes are only allowed while **not** measuring.
- Source mode = the PPK2 supplies the DUT (0.8–5 V). Ampere mode = external
  supply, PPK2 as an inline meter.

## Platform support

Developed and tested on **Linux**. The underlying `serialport` crate also
supports macOS and Windows, and the port is just a `port` argument, so those
platforms should work but are untested — pass the OS-appropriate port (e.g.
`/dev/cu.usbmodem*` on macOS, `COM3` on Windows). The libudev/udev notes above
apply to Linux only.

## Direct smoke test (no MCP)

```sh
cargo run --example controller_test -- /dev/ttyACM0
```

## Layout

- `src/controller.rs` — device-facing core: state machine, drain thread, stats, CSV, triggers.
- `src/server.rs` — rmcp tool surface.
- `src/main.rs` — stdio entry point.
- `examples/controller_test.rs` — live smoke test bypassing MCP.

## License

MIT — see [LICENSE](LICENSE).
