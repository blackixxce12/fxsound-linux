//! The egui front end: palette, geometry, custom widgets and the two views.
//!
//! This crate is deliberately free of any audio or PipeWire dependency. It renders whatever state
//! it is handed and reports what the user did, so it can be exercised without a sound server.

#![forbid(unsafe_code)]

pub mod assets;
pub mod dialogs;
pub mod layout;
pub mod state;
pub mod theme;
pub mod views;
pub mod widgets;

pub use assets::{AssetCache, FxImage};
pub use state::{UiAction, UiResponse, UiState};
pub use theme::{FxColor, Palette};
pub use views::{ViewScratch, window_size};
pub use widgets::{FxSlider, VisualizerAnimation, VisualizerWidget};

