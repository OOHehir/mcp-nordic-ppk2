//! PPK2 MCP: a Model Context Protocol server exposing a Nordic Power Profiler
//! Kit II over stdio. `controller` is the device-facing core; `server` is the
//! rmcp tool surface built on top of it.

pub mod controller;
pub mod server;
