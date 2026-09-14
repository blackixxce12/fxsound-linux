//! The FxSound application: the controller, the command-line surface and the desktop integration.
//!
//! Split into a library so every part of it can be tested without opening a window — the binary in
//! `main.rs` is only the shell that wires these modules to eframe.

#![forbid(unsafe_code)]

pub mod app;
pub mod cli;
pub mod commands;
pub mod ipc;
pub mod notify;
pub mod tray;

pub use app::{App, WindowVisibility};
pub use commands::{Outcome, WindowRequest};
