//! Custom widgets.
//!
//! FxSound draws almost nothing with stock controls: every slider, button and combo box goes
//! through `FxTheme`'s `LookAndFeel` overrides. egui has no `LookAndFeel` hook, so each of those
//! becomes a widget here that paints itself with [`egui::Painter`] and does its own hit testing
//! through `Ui::interact`, which is also what lets the port keep the original's absolute pixel
//! layout instead of reflowing it.

pub mod combo;
pub mod equalizer;
pub mod icon_button;
pub mod power_button;
pub mod slider;
pub mod visualizer;

pub use combo::FxComboBox;
pub use equalizer::{EqInteraction, EqLayout, EqualizerWidget};
pub use icon_button::IconButton;
pub use power_button::PowerButton;
pub use slider::{Fidelity, FxSlider};
pub use visualizer::{VisualizerAnimation, VisualizerWidget};
