//! The two shipping windows.
//!
//! `FxMainWindow` is one frameless desktop component whose *content* is swapped between
//! `FxProView` and `FxLiteView` (`fxsound/Source/GUI/FxWindow.cpp:41-83`); the 56 point title bar
//! above the swap is identical in both. That shape survives the port: [`titlebar::show`] paints the
//! chrome, [`pro::show`] and [`lite::show`] paint a content area under it, and [`show`] picks the
//! one [`UiState::view`] asks for.
//!
//! Every rectangle here is **absolute, in window coordinates**, exactly as
//! `docs/spec/01-window-layout.md` §9 writes them. Nothing reflows and nothing is laid out by
//! egui's `Layout`: the original is a pixel-exact `setBounds` design and reproducing it means
//! computing the same rectangles every frame and painting into them. The one concession to
//! immediate mode is [`at`], which moves those window-local rectangles to wherever the root
//! [`Ui`] actually begins — `(0, 0)` in the real application, something else in a test.
//!
//! ## What a view is
//!
//! A view is a function, not an object. It reads [`UiState`], mutates only the transient widget
//! state in [`ViewScratch`], and returns the [`UiAction`]s the user produced. Nothing in here
//! writes a preset, retunes the engine or talks to a window manager — including the window drag,
//! which leaves as [`UiAction::DragWindow`] for the application layer to turn into
//! `egui::ViewportCommand::StartDrag`.

pub mod lite;
pub mod pro;
pub mod titlebar;

use crate::assets::AssetCache;
use crate::layout;
use crate::state::{PresetEntry, UiAction, UiResponse, UiState};
use crate::theme::Palette;
use crate::widgets::combo::SectionHeader;
use crate::widgets::{EqInteraction, FxComboBox, VisualizerAnimation, combo};
use egui::{Pos2, Rect, Ui, Vec2};
use fxsound_core::i18n::tr;
use fxsound_core::{AudioDevice, DeviceDirection, ViewMode};

/// The widget state that has to survive between frames.
///
/// Everything a view draws is derived from [`UiState`] each frame; this is the small remainder
/// that cannot be — which band is mid-drag, where the spectrum history has got to, whether the
/// window is being moved. The application owns one of these for the lifetime of the window and
/// hands it to whichever view is showing, so switching Pro↔Lite does not restart the visualizer's
/// animation or drop a gesture.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewScratch {
    /// Which equalizer band or frequency wheel is being dragged or hovered.
    pub eq: EqInteraction,
    /// The spectrum strip's ten-bar history and its release envelope.
    pub visualizer: VisualizerAnimation,
    /// `true` between `drag_started` and `drag_stopped` on the title bar.
    ///
    /// The compositor owns the move once [`UiAction::DragWindow`] has been sent, so this is only
    /// bookkeeping — the original keeps the same flag in `FxWindow::TitleBar::dragging_`
    /// (`FxWindow.cpp:339-358`).
    pub window_drag: bool,
    /// How far the wordmark has crossed from the plain logo to the highlighted one, `0.0..=1.0`
    /// (`FxWindow.cpp:193-208`).
    pub logo_fade: f32,
}

impl ViewScratch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a window move is in progress.
    #[must_use]
    pub const fn is_dragging_window(&self) -> bool {
        self.window_drag
    }

    /// Forget every gesture and every animation, e.g. after the window was hidden to the tray.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

/// The outer window size for a view (`docs/spec/01-window-layout.md` §5.3's comparison table).
#[must_use]
pub const fn window_size(view: ViewMode) -> Vec2 {
    match view {
        ViewMode::Pro => layout::pro::WINDOW_SIZE,
        ViewMode::Lite => layout::lite::WINDOW_SIZE,
    }
}

/// The whole window, starting at `origin`.
#[must_use]
pub fn window_rect(origin: Pos2, view: ViewMode) -> Rect {
    Rect::from_min_size(origin, window_size(view))
}

/// Move a window-local rectangle to where this window actually starts.
///
/// `layout`'s rectangles are written in the original's window coordinates, which have their origin
/// at the window's top-left corner. In the shipping application that is `(0, 0)` and this is the
/// identity; in a test — or inside any [`Ui`] that does not start at the origin — it is not.
#[must_use]
pub fn at(origin: Pos2, rect: Rect) -> Rect {
    rect.translate(origin.to_vec2())
}

/// Where this [`Ui`]'s window begins.
#[must_use]
pub fn window_origin(ui: &Ui) -> Pos2 {
    ui.max_rect().min
}

/// Draw whichever window [`UiState::view`] selects.
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    scratch: &mut ViewScratch,
    palette: Palette,
    assets: &mut AssetCache,
) -> UiResponse {
    match state.view {
        ViewMode::Pro => pro::show(ui, state, scratch, palette, assets),
        ViewMode::Lite => lite::show(ui, state, scratch, palette, assets),
    }
}

/// Where `FxView::modelChanged` puts the preset list's one separator: at the factory→user boundary
/// (`FxView.cpp:205-225`, `docs/spec/03-controls.md` §8.4 rule 3).
///
/// `None` when every preset is a factory one, and also when the very first entry is already a user
/// preset — a rule above the top of the list would be a stray line.
#[must_use]
pub fn first_user_preset(presets: &[PresetEntry]) -> Option<usize> {
    presets
        .iter()
        .position(|preset| !preset.factory)
        .filter(|&index| index > 0)
}

/// How the device list is cut into its `Output` and `Input` runs **(port addition)**.
///
/// The Windows build lists playback endpoints only (`FxView.cpp:88-96`); the port lists capture
/// devices in the same box, and the audio engine publishes them grouped — every
/// [`DeviceDirection::Output`] device first, then every [`DeviceDirection::Input`] one — so the
/// two runs can be titled and ruled apart without reordering anything. The indices here are
/// indices into `UiState::devices`, which is also what [`UiAction::SelectDevice`] carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceSections {
    /// A title above the first device of each direction, in list order: the row index and the
    /// direction, translated at draw time.
    pub titles: Vec<(usize, DeviceDirection)>,
    /// The rule where the direction changes, `None` when the list holds only one direction.
    pub separator: Option<usize>,
}

/// Where `devices` changes direction and what to call each run — see [`DeviceSections`].
///
/// A run is titled with [`DeviceDirection::label`]; a direction with no devices gets no title, so
/// a machine with no microphone shows `Output` and nothing else, and an empty list shows neither.
/// The rule follows the preset list's convention ([`first_user_preset`]): it sits at the first
/// change of direction and never above the very first row.
#[must_use]
pub fn device_sections(devices: &[AudioDevice]) -> DeviceSections {
    let mut sections = DeviceSections::default();
    let mut current: Option<DeviceDirection> = None;
    for (index, device) in devices.iter().enumerate() {
        if current == Some(device.direction) {
            continue;
        }
        current = Some(device.direction);
        sections.titles.push((index, device.direction));
        if index > 0 && sections.separator.is_none() {
            sections.separator = Some(index);
        }
    }
    sections
}

/// The preset and device pickers, which `FxView` gives to both windows (`FxView.cpp:24-56`).
///
/// The two differ in more than their contents. The preset list is disabled with the master power
/// (`FxProView.cpp:117-121`, `FxLiteView.cpp:57`) while the device list deliberately is **not** —
/// the one thing a user must still be able to do with the power off is pick a different device.
/// Both windows share this, which is why it lives here rather than in either of them.
pub(crate) fn combos(
    ui: &mut Ui,
    state: &UiState,
    palette: Palette,
    assets: &mut AssetCache,
    preset_rect: Rect,
    output_rect: Rect,
    response: &mut UiResponse,
) {
    let presets: Vec<String> = state
        .presets
        .iter()
        .map(|preset| combo::preset_label(&preset.name, preset.modified))
        .collect();
    let (_, picked) = FxComboBox::new(&presets, state.selected_preset)
        .enabled(state.controls_enabled())
        .separator_before(first_user_preset(&state.presets))
        .show(ui, preset_rect, palette, assets, "preset_list");
    if let Some(index) = picked {
        response.push(UiAction::SelectPreset(index));
    }

    // `node.description` is what the Windows build's endpoint list shows too — the friendly name,
    // not the stable id the settings file keys on (`FxView.cpp:88-96`). The closed box shows the
    // selected device's description alone; the `Output` / `Input` titles exist only in the menu.
    let devices: Vec<String> = state
        .devices
        .iter()
        .map(|device| device.description.clone())
        .collect();
    let sections = device_sections(&state.devices);
    let titles: Vec<String> = sections
        .titles
        .iter()
        .map(|(_, direction)| tr(direction.label()))
        .collect();
    let headers: Vec<SectionHeader<'_>> = sections
        .titles
        .iter()
        .zip(&titles)
        .map(|((index, _), title)| SectionHeader::new(*index, title))
        .collect();
    let (_, picked) = FxComboBox::new(&devices, state.selected_device)
        .separator_before(sections.separator)
        .headers(&headers)
        .show(ui, output_rect, palette, assets, "output_list");
    if let Some(index) = picked {
        response.push(UiAction::SelectDevice(index));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PresetEntry;
    use egui::pos2;

    fn preset(name: &str, factory: bool) -> PresetEntry {
        PresetEntry {
            name: name.to_owned(),
            factory,
            modified: false,
        }
    }

    fn device(description: &str, direction: DeviceDirection) -> AudioDevice {
        AudioDevice {
            id: 0,
            name: format!("node.{description}"),
            description: description.to_owned(),
            is_default: false,
            direction,
            form_factor: "speaker".into(),
        }
    }

    #[test]
    fn each_view_mode_names_the_window_size_the_spec_tabulates() {
        // docs/spec/01-window-layout.md §8: 1040 x 588 and 550 x 189.
        let pro = window_size(ViewMode::Pro);
        assert!(
            (pro.x - 1040.0).abs() < 1e-4 && (pro.y - 588.0).abs() < 1e-4,
            "{pro:?}"
        );
        let lite = window_size(ViewMode::Lite);
        assert!(
            (lite.x - 550.0).abs() < 1e-4 && (lite.y - 189.0).abs() < 1e-4,
            "{lite:?}"
        );
    }

    #[test]
    fn a_window_rect_starts_where_its_ui_starts() {
        let rect = window_rect(pos2(12.0, 34.0), ViewMode::Lite);
        assert!((rect.left() - 12.0).abs() < 1e-4);
        assert!((rect.top() - 34.0).abs() < 1e-4);
        assert!((rect.right() - (12.0 + 550.0)).abs() < 1e-4);
        assert!((rect.bottom() - (34.0 + 189.0)).abs() < 1e-4);
    }

    #[test]
    fn translating_by_the_origin_leaves_a_window_at_zero_untouched() {
        let panel = layout::pro::panel();
        let moved = at(pos2(0.0, 0.0), panel);
        assert!((moved.min - panel.min).length() < 1e-6);
        assert!((moved.max - panel.max).length() < 1e-6);

        let shifted = at(pos2(5.0, -3.0), panel);
        assert!((shifted.left() - (panel.left() + 5.0)).abs() < 1e-4);
        assert!((shifted.top() - (panel.top() - 3.0)).abs() < 1e-4);
        assert!((shifted.size() - panel.size()).length() < 1e-6);
    }

    #[test]
    fn the_separator_marks_the_boundary_between_factory_and_user_presets() {
        let presets = [
            preset("Flat", true),
            preset("Rock", true),
            preset("My Mix", false),
            preset("Late Night", false),
        ];
        assert_eq!(first_user_preset(&presets), Some(2));
    }

    #[test]
    fn a_list_with_no_user_presets_gets_no_separator() {
        let presets = [preset("Flat", true), preset("Rock", true)];
        assert_eq!(first_user_preset(&presets), None);
        assert_eq!(first_user_preset(&[]), None);
    }

    #[test]
    fn a_list_that_opens_with_a_user_preset_gets_no_separator_above_its_first_row() {
        let presets = [preset("My Mix", false), preset("Flat", true)];
        assert_eq!(first_user_preset(&presets), None);
    }

    #[test]
    fn an_empty_device_list_has_no_titles_and_no_rule() {
        assert_eq!(device_sections(&[]), DeviceSections::default());
    }

    #[test]
    fn a_list_of_outputs_alone_is_titled_output_once_and_never_ruled() {
        let devices = [
            device("Built-in Audio", DeviceDirection::Output),
            device("Scarlett 2i2", DeviceDirection::Output),
        ];
        let sections = device_sections(&devices);
        assert_eq!(sections.titles, vec![(0, DeviceDirection::Output)]);
        assert_eq!(sections.separator, None);
    }

    #[test]
    fn a_list_of_inputs_alone_is_titled_input_once_and_never_ruled() {
        let devices = [device("Fifine Microphone", DeviceDirection::Input)];
        let sections = device_sections(&devices);
        assert_eq!(sections.titles, vec![(0, DeviceDirection::Input)]);
        assert_eq!(sections.separator, None);
    }

    #[test]
    fn outputs_then_inputs_get_two_titles_and_one_rule_at_the_change_of_direction() {
        // The engine's order: outputs sorted by description, then inputs sorted by description.
        let devices = [
            device("Built-in Audio", DeviceDirection::Output),
            device("Fifine Speakers", DeviceDirection::Output),
            device("Scarlett 2i2", DeviceDirection::Output),
            device("Fifine Microphone", DeviceDirection::Input),
            device("Webcam", DeviceDirection::Input),
        ];
        let sections = device_sections(&devices);
        assert_eq!(
            sections.titles,
            vec![(0, DeviceDirection::Output), (3, DeviceDirection::Input)]
        );
        assert_eq!(sections.separator, Some(3));
        // The titles are the shared contract's words, not a spelling of this module's own.
        assert_eq!(sections.titles[0].1, DeviceDirection::Output);
        assert_eq!(sections.titles[1].1, DeviceDirection::Input);
    }

    #[test]
    fn the_titles_and_the_rule_leave_the_device_indices_alone() {
        // `UiAction::SelectDevice(i)` indexes `state.devices`; the sections only say where to draw
        // decoration, so every header index must be the index of a real device of that direction.
        let devices = [
            device("Built-in Audio", DeviceDirection::Output),
            device("Fifine Microphone", DeviceDirection::Input),
            device("Webcam", DeviceDirection::Input),
        ];
        let sections = device_sections(&devices);
        for header in &sections.titles {
            assert_eq!(devices[header.0].direction, header.1);
        }
        assert_eq!(sections.separator, Some(1));
        assert_eq!(devices[1].direction, DeviceDirection::Input);
        assert_eq!(devices[0].direction, DeviceDirection::Output);
    }

    #[test]
    fn fresh_scratch_holds_no_gesture_and_no_animation() {
        let scratch = ViewScratch::new();
        assert!(!scratch.is_dragging_window());
        assert!(scratch.eq.dragged_band().is_none());
        assert!(scratch.eq.hovered_band().is_none());
        assert!(scratch.visualizer.is_settled());
        assert!(scratch.logo_fade.abs() < 1e-6);
        assert_eq!(scratch, ViewScratch::default());
    }

    #[test]
    fn resetting_scratch_forgets_a_drag_and_a_running_crossfade() {
        let mut scratch = ViewScratch::new();
        scratch.window_drag = true;
        scratch.logo_fade = 0.4;
        assert!(scratch.is_dragging_window());
        scratch.reset();
        assert!(!scratch.is_dragging_window());
        assert!(scratch.logo_fade.abs() < 1e-6);
    }
}
