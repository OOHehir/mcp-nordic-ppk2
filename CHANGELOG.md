# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-12

Initial release.

### Added
- MCP server (stdio) exposing a Nordic Power Profiler Kit II (PPK2).
- Device tools: `ppk2_connect`, `ppk2_configure`, `ppk2_measure`, `ppk2_start`,
  `ppk2_stop`, `ppk2_stats`, `ppk2_export_csv`, `ppk2_status`.
- Source mode with a configurable DUT supply voltage and a safety ceiling
  (`--max-voltage-mv` / `PPK2_MAX_VOLTAGE_MV`, default 3300 mV).
- Background drain thread with a bounded rolling buffer; aggregate statistics
  (mean/min/max/stddev µA, integrated charge in µC and mAh) instead of raw
  streams.
- 8 digital logic channels (D0–D7) with trigger patterns and per-channel duty
  cycle; raw sample CSV export.
- USB VID:PID auto-discovery of the control interface.

[0.1.0]: https://github.com/OOHehir/mcp-nordic-ppk2/releases/tag/v0.1.0
