//! The FxSound application: the controller, the command-line surface and the desktop integration.
//!
//! Split into a library so every part of it can be tested without opening a window — the binary in
//! `main.rs` is only the shell that wires these modules to eframe.

#![forbid(unsafe_code)]

pub mod app;
pub mod audio_link;
pub mod calibration;
pub mod cli;
pub mod commands;
pub mod dbus;
pub mod events;
pub mod ipc;
pub mod notify;
mod priority;
#[cfg(test)]
mod private_bus;
pub mod selftest;
pub mod sleep;
pub mod tray;

pub use app::{App, WindowVisibility};
pub use commands::{Outcome, WindowRequest};
