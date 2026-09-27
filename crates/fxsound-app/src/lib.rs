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
pub mod wake;

pub use app::{App, WindowVisibility};
pub use commands::{Outcome, WindowRequest};

/// What FxSound logs when `RUST_LOG` says nothing: its own `info` and up, and only warnings and
/// errors from the D-Bus library and the tracing bridge under it.
///
/// Left at `info` for everything, zbus wrote six lines of its authentication handshake, raw bytes
/// and all, for every connection it opened — the tray's, the service's, and one for every desktop
/// notification — into the terminal or the journal of anyone running FxSound, about a third of all
/// it logged in the 0.4.0 live check. `RUST_LOG` still sets whatever it likes.
pub const DEFAULT_LOG_FILTER: &str = "info,zbus=warn,tracing=warn";

#[cfg(test)]
mod tests {
    use super::DEFAULT_LOG_FILTER;
    use log::Level;

    #[test]
    fn the_default_log_filter_keeps_the_bus_librarys_handshakes_out_and_fxsounds_own_lines_in() {
        let logger = env_logger::Builder::new()
            .parse_filters(DEFAULT_LOG_FILTER)
            .build();
        let at = |target: &str, level: Level| {
            log::Log::enabled(
                &logger,
                &log::Metadata::builder().target(target).level(level).build(),
            )
        };
        assert!(!at("zbus::connection::handshake::common", Level::Info));
        assert!(!at("tracing::span", Level::Info));
        assert!(
            at("zbus::connection", Level::Warn),
            "a bus library's warning still shows"
        );
        assert!(at("fxsound", Level::Info));
        assert!(at("fxsound_audio::engine", Level::Info));
        assert!(at("fxsound_app::tray", Level::Warn));
        assert!(!at("fxsound_audio::engine", Level::Debug));
    }
}
