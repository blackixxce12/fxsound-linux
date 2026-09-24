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

pub mod equalizer_controls;
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

/// Which face of the Pro window's effect column is showing.
///
/// `FxAudioControls::effects_shown_` (`FxAudioControls.cpp:28`, `:41-46`): the five effect
/// sliders first, turned over by the flip button to the equalizer's own controls
/// ([`equalizer_controls`]). Like the original's flag it is the window's and nobody else's — it
/// is not a setting and is not saved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColumnFace {
    /// Face A, `FxEffects`.
    #[default]
    Effects,
    /// Face B, `FxEqualizerControl`.
    EqualizerControls,
}

impl ColumnFace {
    /// The other face.
    #[must_use]
    pub const fn flipped(self) -> Self {
        match self {
            Self::Effects => Self::EqualizerControls,
            Self::EqualizerControls => Self::Effects,
        }
    }
}

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
    /// Which face of the effect column the flip button last turned up.
    pub column_face: ColumnFace,
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

/// The preset picker, which `FxView` gives to both windows (`FxView.cpp:24-56`).
///
/// The original disables it with the master power (`FxProView.cpp:117-121`, `FxLiteView.cpp:57`).
/// Here it stays live, as the device pickers do (0.4.0 audit R7): the command line and D-Bus pick
/// presets with the power off, so the window refusing to was the two contradicting each other,
/// and a preset can now be chosen before switching on. What the preset sets stays grey until
/// then. It lists the edit direction's presets, and a long name is cut before its `*`, never
/// the `*` itself (audit #26). The box's response is handed back for a tooltip.
pub(crate) fn preset_combo(
    ui: &mut Ui,
    state: &UiState,
    palette: Palette,
    assets: &mut AssetCache,
    rect: Rect,
    response: &mut UiResponse,
) -> egui::Response {
    let presets: Vec<String> = state
        .presets
        .iter()
        .map(|preset| combo::preset_label(&preset.name, preset.modified))
        .collect();
    let (combo, picked) = FxComboBox::new(&presets, state.selected_preset)
        .keep_suffix(combo::MODIFIED_SUFFIX)
        .separator_before(first_user_preset(&state.presets))
        .show(ui, rect, palette, assets, "preset_list");
    if let Some(index) = picked {
        response.push(UiAction::SelectPreset(index));
    }
    combo
}

/// What a microphone picker says on hover while it shows a Bluetooth headset's microphone
/// **(port addition, `docs/0.4.0-upstream.md` U9)**. FxSound's capture of a microphone is passive
/// (U19), so a headset's microphone is woken — and the headset switched from music to its call
/// profile — only by something recording from FxSound (Input); picked and never recorded from,
/// it neither sounds nor costs anything, which is exactly what looks broken without a word.
pub const BLUETOOTH_MICROPHONE_TIP: &str = "A Bluetooth microphone wakes only while an application \
     records from FxSound (Input), and then switches its headset to call quality.";

/// Whether `device` is a Bluetooth headset's microphone: under WirePlumber 0.4 the headset's own
/// `bluez_input.<address>.0`, under 0.5 the loopback `bluez_input.<address>` in front of it. The
/// node name says so either way; a form factor of `headset` would take a USB headset in too.
#[must_use]
pub fn is_bluetooth_microphone(device: &AudioDevice) -> bool {
    device.direction == DeviceDirection::Input && device.name.starts_with("bluez_input.")
}

/// The hint a device picker's box gives on hover for the device of `direction` it shows
/// ([`BLUETOOTH_MICROPHONE_TIP`] for a Bluetooth microphone), or `None` — no tooltip at all — for
/// any other device, and while "Hide help tips" is ticked, as for every tip in the window.
#[must_use]
pub fn device_tip(state: &UiState, direction: DeviceDirection) -> Option<String> {
    if state.hide_tooltips {
        return None;
    }
    state
        .device_for(direction)
        .filter(|device| is_bluetooth_microphone(device))
        .map(|_| tr(BLUETOOTH_MICROPHONE_TIP))
}

/// One row of a device menu **(port addition)**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRow {
    /// `Off`: detach this lane.
    Off(DeviceDirection),
    /// A device, as an index into `UiState::devices`; picking it attaches its own lane to it.
    Device(usize),
}

/// What a device picker lists, and what each row stands for **(port addition)**.
///
/// The Windows build's list is its playback endpoints and nothing else (`FxView.cpp:88-96`). With
/// two lanes a device list also has to be able to say *none*: every lane's run of devices starts
/// with an `Off` row that detaches it, and a detached lane's box shows `Off` as its placeholder —
/// dimmed, as the original dims an empty box (`FxTheme.cpp:166-179`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceMenu {
    /// What each row does, in menu order.
    pub rows: Vec<DeviceRow>,
    /// Each row's text: `node.description` for a device — the friendly name, as the original
    /// shows — and the translated `Off` for the rest.
    pub labels: Vec<String>,
    /// A section title above the row at each index, for the list that holds both lanes.
    pub titles: Vec<(usize, DeviceDirection)>,
    /// The one rule the combo draws.
    pub separator: Option<usize>,
    /// The row the closed box shows; `None` draws the `Off` placeholder, unless `unlisted` says
    /// otherwise.
    pub selected: Option<usize>,
    /// With no row selected, the device the lane is on while the list does not carry it
    /// ([`UiState::unlisted`]): the closed box shows its name, not `Off`.
    pub unlisted: Option<String>,
}

impl DeviceMenu {
    /// One lane's list, for the Pro view: `Off`, a rule, then that lane's devices.
    #[must_use]
    pub fn lane(state: &UiState, direction: DeviceDirection) -> Self {
        let mut menu = Self::default();
        menu.push_off(direction);
        let first_device = menu.rows.len();
        menu.push_devices(state, direction);
        if menu.rows.len() > first_device {
            menu.separator = Some(first_device);
        }
        menu.selected = menu.row_of(state.selection(direction));
        menu.note_unlisted(state, direction);
        menu
    }

    /// Both lanes in one list, for the Lite view: each direction titled, as in 0.3.0, with an `Off`
    /// row first under each title, and the rule where the direction changes. The closed box shows
    /// the edit direction's device.
    ///
    /// A direction with no devices gets no section at all — an `Off` with nothing to turn off
    /// would be a row that does nothing.
    #[must_use]
    pub fn both(state: &UiState) -> Self {
        let mut menu = Self::default();
        for (_, direction) in device_sections(&state.devices).titles {
            if !menu.rows.is_empty() && menu.separator.is_none() {
                menu.separator = Some(menu.rows.len());
            }
            menu.titles.push((menu.rows.len(), direction));
            menu.push_off(direction);
            menu.push_devices(state, direction);
        }
        menu.selected = menu.row_of(state.selection(state.direction));
        menu.note_unlisted(state, state.direction);
        menu
    }

    fn note_unlisted(&mut self, state: &UiState, direction: DeviceDirection) {
        if self.selected.is_none() {
            self.unlisted = state.unlisted(direction).map(str::to_owned);
        }
    }

    fn push_off(&mut self, direction: DeviceDirection) {
        self.rows.push(DeviceRow::Off(direction));
        self.labels.push(tr("Off"));
    }

    fn push_devices(&mut self, state: &UiState, direction: DeviceDirection) {
        for (index, device) in state.devices.iter().enumerate() {
            if device.direction == direction {
                self.rows.push(DeviceRow::Device(index));
                self.labels.push(device.description.clone());
            }
        }
    }

    /// The row showing device `index`, if it is listed.
    fn row_of(&self, index: Option<usize>) -> Option<usize> {
        let index = index?;
        self.rows
            .iter()
            .position(|row| *row == DeviceRow::Device(index))
    }

    /// What picking `row` asks for.
    ///
    /// `Off` detaches its lane and nothing else. A device attaches its own lane — whichever
    /// direction the device is, never the one the list happens to be for — and makes that lane the
    /// edit direction, said first so that the preset list the window shows is already the right
    /// one when the device arrives.
    #[must_use]
    pub fn actions_for(&self, row: usize, state: &UiState) -> Vec<UiAction> {
        match self.rows.get(row) {
            Some(DeviceRow::Off(direction)) => vec![UiAction::detach(*direction)],
            Some(DeviceRow::Device(index)) => {
                let Some(direction) = state.devices.get(*index).map(|d| d.direction) else {
                    return Vec::new();
                };
                let mut actions = Vec::with_capacity(2);
                if direction != state.direction {
                    actions.push(UiAction::SetEditDirection(direction));
                }
                actions.push(UiAction::select(direction, *index));
                actions
            }
            None => Vec::new(),
        }
    }

    /// Draw it and report the pick.
    #[allow(clippy::too_many_arguments)]
    fn show(
        &self,
        ui: &mut Ui,
        state: &UiState,
        palette: Palette,
        assets: &mut AssetCache,
        rect: Rect,
        accent: bool,
        id_salt: &str,
        response: &mut UiResponse,
    ) -> egui::Response {
        let titles: Vec<String> = self
            .titles
            .iter()
            .map(|(_, direction)| tr(direction.label()))
            .collect();
        let headers: Vec<SectionHeader<'_>> = self
            .titles
            .iter()
            .zip(&titles)
            .map(|((row, _), title)| SectionHeader::new(*row, title))
            .collect();
        let placeholder = tr("Off");
        let (box_response, picked) = FxComboBox::new(&self.labels, self.selected)
            .placeholder(&placeholder)
            .current(self.unlisted.as_deref())
            .separator_before(self.separator)
            .headers(&headers)
            .accent(accent)
            .show(ui, rect, palette, assets, id_salt);
        if let Some(row) = picked {
            for action in self.actions_for(row, state) {
                if !response.contains(&action) {
                    response.push(action);
                }
            }
        }
        box_response
    }
}

/// The Pro view's two device pickers, one per lane **(port addition, 0.4.0 design §1.4)**.
///
/// Each lists its own direction only. Clicking one — to look, to pick, or to turn its lane off —
/// makes its lane the edit direction, and the box of the edit direction carries the accent
/// outline, so the window always says which chain the preset list and the equalizer are for.
pub(crate) fn lane_combos(
    ui: &mut Ui,
    state: &UiState,
    palette: Palette,
    assets: &mut AssetCache,
    origin: Pos2,
    response: &mut UiResponse,
) {
    for (direction, id_salt) in [
        (DeviceDirection::Output, "output_list"),
        (DeviceDirection::Input, "input_list"),
    ] {
        let menu = DeviceMenu::lane(state, direction);
        let rect = at(origin, layout::pro::device_combo(direction));
        let edited = state.direction == direction;
        let box_response = menu.show(ui, state, palette, assets, rect, edited, id_salt, response);
        let switch = UiAction::SetEditDirection(direction);
        if box_response.clicked() && !edited && !response.contains(&switch) {
            response.push(switch);
        }
        if let Some(tip) = device_tip(state, direction) {
            let _ = box_response.on_hover_text(tip);
        }
    }
}

/// The Lite view's single device picker, listing both lanes.
pub(crate) fn device_combo(
    ui: &mut Ui,
    state: &UiState,
    palette: Palette,
    assets: &mut AssetCache,
    rect: Rect,
    response: &mut UiResponse,
) {
    let box_response = DeviceMenu::both(state).show(
        ui,
        state,
        palette,
        assets,
        rect,
        false,
        "output_list",
        response,
    );
    // The closed box shows the edit direction's device.
    if let Some(tip) = device_tip(state, state.direction) {
        let _ = box_response.on_hover_text(tip);
    }
}

/// A headless window for the view tests: real fonts, real artwork, frames driven by hand.
#[cfg(test)]
pub(crate) mod testing {
    use super::{ViewScratch, show, window_size};
    use crate::assets::AssetCache;
    use crate::state::{UiAction, UiState};
    use crate::theme::{self, Palette};
    use egui::epaint::ClippedShape;
    use egui::{Color32, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Shape};
    use fxsound_core::ThemeMode;

    pub struct Harness {
        pub ctx: egui::Context,
        pub scratch: ViewScratch,
        pub assets: AssetCache,
        pub palette: Palette,
        /// A screen taller than the window, for a test that needs a whole menu on it: the Lite
        /// window is 189 points tall, and a menu longer than that scrolls inside it.
        pub screen: Option<egui::Vec2>,
    }

    impl Harness {
        pub fn new(mode: ThemeMode) -> Self {
            let ctx = egui::Context::default();
            ctx.set_fonts(theme::font_definitions());
            // Popups fade in over `animation_time`; at zero a painted menu is at full opacity.
            ctx.all_styles_mut(|style| style.animation_time = 0.0);
            Self {
                ctx,
                scratch: ViewScratch::new(),
                assets: AssetCache::new(),
                palette: Palette::new(mode),
                screen: None,
            }
        }

        /// One frame of whichever view `state` asks for.
        pub fn frame(
            &mut self,
            state: &UiState,
            events: Vec<Event>,
        ) -> (Vec<UiAction>, Vec<ClippedShape>) {
            let screen = self.screen.unwrap_or_else(|| window_size(state.view));
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
                events,
                ..Default::default()
            };
            let mut actions = Vec::new();
            let Self {
                ctx,
                scratch,
                assets,
                palette,
                screen: _,
            } = self;
            let mut output = ctx.run_ui(input, |ui| {
                actions = show(ui, state, scratch, *palette, assets).actions;
            });
            let shapes = std::mem::take(&mut output.shapes);
            output.drop_without_applying_deltas();
            (actions, shapes)
        }

        /// Two quiet frames — artwork uploaded, popups laid out — and the second one's shapes.
        pub fn settle(&mut self, state: &UiState) -> Vec<ClippedShape> {
            self.frame(state, Vec::new());
            self.frame(state, Vec::new()).1
        }

        /// Rest the pointer on `pos` past egui's half-second tooltip delay, and every line of text
        /// the last frame painted.
        pub fn rest(&mut self, state: &UiState, pos: Pos2) -> Vec<String> {
            self.frame(state, vec![Event::PointerMoved(pos)]);
            // Sixty quiet frames of a sixtieth each.
            let mut shapes = Vec::new();
            for _ in 0..60 {
                shapes = self.frame(state, Vec::new()).1;
            }
            texts(&shapes).into_iter().map(|(text, ..)| text).collect()
        }

        /// Hover, press and release at `pos`, with everything reported on the way.
        ///
        /// egui hit-tests against the previous pass's rectangles, so a click takes three frames.
        pub fn click(&mut self, state: &UiState, pos: Pos2) -> Vec<UiAction> {
            let press = |pressed| Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::default(),
            };
            let mut actions = Vec::new();
            for events in [
                vec![Event::PointerMoved(pos)],
                vec![Event::PointerMoved(pos), press(true)],
                vec![press(false)],
            ] {
                actions.extend(self.frame(state, events).0);
            }
            actions
        }
    }

    /// Every line of text painted, with where it landed and its colour.
    pub fn texts(shapes: &[ClippedShape]) -> Vec<(String, Rect, Color32)> {
        shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                Shape::Text(text) => Some((
                    text.galley.text().to_owned(),
                    clipped.shape.visual_bounding_rect(),
                    text.fallback_color,
                )),
                _ => None,
            })
            .collect()
    }

    /// Where the one line reading `wanted` below `y` was painted.
    pub fn text_below(shapes: &[ClippedShape], wanted: &str, y: f32) -> Vec<Rect> {
        texts(shapes)
            .into_iter()
            .filter(|(text, rect, _)| text == wanted && rect.top() >= y)
            .map(|(_, rect, _)| rect)
            .collect()
    }

    /// Everything painted, clipped to where it was allowed to be.
    pub fn painted_bounds(shapes: &[ClippedShape]) -> Rect {
        let mut painted = Rect::NOTHING;
        for clipped in shapes {
            let bounds = clipped
                .shape
                .visual_bounding_rect()
                .intersect(clipped.clip_rect);
            if bounds.is_positive() {
                painted = painted.union(bounds);
            }
        }
        painted
    }

    /// The colour of the outline stroked around exactly `rect`, if one was.
    pub fn outline_of(shapes: &[ClippedShape], rect: Rect) -> Option<Color32> {
        shapes.iter().find_map(|clipped| match &clipped.shape {
            Shape::Rect(shape)
                if shape.stroke.width > 0.0
                    && (shape.rect.min - rect.min).length() < 1e-3
                    && (shape.rect.max - rect.max).length() < 1e-3 =>
            {
                Some(shape.stroke.color)
            }
            _ => None,
        })
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

    fn lanes() -> UiState {
        UiState {
            devices: vec![
                device("Speakers", DeviceDirection::Output),
                device("Headphones", DeviceDirection::Output),
                device("Microphone", DeviceDirection::Input),
                device("Webcam", DeviceDirection::Input),
            ],
            selected_output: Some(1),
            selected_input: None,
            ..UiState::default()
        }
    }

    #[test]
    fn a_lane_list_offers_off_first_then_only_its_own_devices() {
        let state = lanes();
        let output = DeviceMenu::lane(&state, DeviceDirection::Output);
        assert_eq!(
            output.rows,
            vec![
                DeviceRow::Off(DeviceDirection::Output),
                DeviceRow::Device(0),
                DeviceRow::Device(1),
            ]
        );
        assert_eq!(output.labels, ["Off", "Speakers", "Headphones"]);
        assert_eq!(
            output.separator,
            Some(1),
            "a rule between Off and the devices"
        );
        assert!(
            output.titles.is_empty(),
            "a list of one direction needs no titles"
        );

        let input = DeviceMenu::lane(&state, DeviceDirection::Input);
        assert_eq!(
            input.rows,
            vec![
                DeviceRow::Off(DeviceDirection::Input),
                DeviceRow::Device(2),
                DeviceRow::Device(3),
            ]
        );
        assert_eq!(input.labels, ["Off", "Microphone", "Webcam"]);
    }

    #[test]
    fn a_lane_list_shows_its_device_or_nothing_for_the_off_placeholder() {
        let state = lanes();
        assert_eq!(
            DeviceMenu::lane(&state, DeviceDirection::Output).selected,
            Some(2),
            "Headphones is row 2, after Off and Speakers"
        );
        assert_eq!(
            DeviceMenu::lane(&state, DeviceDirection::Input).selected,
            None,
            "a detached lane shows the placeholder"
        );
    }

    #[test]
    fn a_lane_on_a_device_the_list_does_not_carry_shows_that_device_and_not_off() {
        // A Bluetooth headset between its profiles: the lane is on, the list lacks the device.
        let state = UiState {
            selected_output: None,
            unlisted_output: Some("WH-1000XM4".to_owned()),
            ..lanes()
        };
        let output = DeviceMenu::lane(&state, DeviceDirection::Output);
        assert_eq!(output.selected, None, "no row is that device");
        assert_eq!(output.unlisted.as_deref(), Some("WH-1000XM4"));
        assert_eq!(
            DeviceMenu::both(&state).unlisted.as_deref(),
            Some("WH-1000XM4")
        );
        let input = DeviceMenu::lane(&state, DeviceDirection::Input);
        assert_eq!(input.unlisted, None, "the microphone lane is off");

        // Drawn as a device is, not as the dimmed Off.
        let mut harness = testing::Harness::new(fxsound_core::ThemeMode::Dark);
        let shapes = harness.settle(&state);
        let painted = testing::texts(&shapes);
        let (_, _, colour) = painted
            .iter()
            .find(|(text, _, _)| text == "WH-1000XM4")
            .expect("the device's name in the box");
        let (_, _, speakers_colour) = testing::texts(&harness.settle(&lanes()))
            .into_iter()
            .find(|(text, _, _)| text == "Headphones")
            .expect("a listed device in the box");
        assert_eq!(*colour, speakers_colour);
    }

    #[test]
    fn a_direction_with_no_devices_is_just_off_and_no_rule() {
        let state = UiState {
            devices: vec![device("Speakers", DeviceDirection::Output)],
            ..UiState::default()
        };
        let input = DeviceMenu::lane(&state, DeviceDirection::Input);
        assert_eq!(input.rows, vec![DeviceRow::Off(DeviceDirection::Input)]);
        assert_eq!(input.separator, None);
    }

    #[test]
    fn picking_off_detaches_that_lane_and_nothing_else() {
        let state = lanes();
        let input = DeviceMenu::lane(&state, DeviceDirection::Input);
        assert_eq!(input.actions_for(0, &state), vec![UiAction::DetachInput]);
        let output = DeviceMenu::lane(&state, DeviceDirection::Output);
        assert_eq!(output.actions_for(0, &state), vec![UiAction::DetachOutput]);
    }

    #[test]
    fn picking_a_device_of_the_other_lane_switches_the_edit_direction_first() {
        let state = lanes();
        let input = DeviceMenu::lane(&state, DeviceDirection::Input);
        assert_eq!(
            input.actions_for(2, &state),
            vec![
                UiAction::SetEditDirection(DeviceDirection::Input),
                UiAction::SelectInput(3)
            ]
        );
        // The edited lane's own list just selects.
        let output = DeviceMenu::lane(&state, DeviceDirection::Output);
        assert_eq!(
            output.actions_for(1, &state),
            vec![UiAction::SelectOutput(0)]
        );
        // A row past the end asks for nothing.
        assert!(output.actions_for(9, &state).is_empty());
    }

    #[test]
    fn the_combined_list_titles_both_lanes_with_an_off_row_under_each_title() {
        let state = lanes();
        let menu = DeviceMenu::both(&state);
        assert_eq!(
            menu.labels,
            [
                "Off",
                "Speakers",
                "Headphones",
                "Off",
                "Microphone",
                "Webcam"
            ]
        );
        assert_eq!(
            menu.titles,
            vec![(0, DeviceDirection::Output), (3, DeviceDirection::Input)]
        );
        assert_eq!(
            menu.separator,
            Some(3),
            "the rule sits above the Input title"
        );
        assert_eq!(menu.rows[0], DeviceRow::Off(DeviceDirection::Output));
        assert_eq!(menu.rows[3], DeviceRow::Off(DeviceDirection::Input));
        assert_eq!(menu.actions_for(3, &state), vec![UiAction::DetachInput]);
        assert_eq!(menu.actions_for(0, &state), vec![UiAction::DetachOutput]);
    }

    #[test]
    fn the_combined_list_shows_the_edit_directions_device() {
        let mut state = lanes();
        state.selected_input = Some(2);
        assert_eq!(DeviceMenu::both(&state).selected, Some(2), "Headphones");
        state.direction = DeviceDirection::Input;
        assert_eq!(DeviceMenu::both(&state).selected, Some(4), "Microphone");
        state.selected_input = None;
        assert_eq!(DeviceMenu::both(&state).selected, None, "Off, dimmed");
    }

    #[test]
    fn the_combined_list_leaves_out_a_direction_with_no_devices() {
        let state = UiState {
            devices: vec![device("Speakers", DeviceDirection::Output)],
            ..UiState::default()
        };
        let menu = DeviceMenu::both(&state);
        assert_eq!(menu.labels, ["Off", "Speakers"]);
        assert_eq!(menu.titles, vec![(0, DeviceDirection::Output)]);
        assert_eq!(menu.separator, None);
        assert_eq!(DeviceMenu::both(&UiState::default()), DeviceMenu::default());
    }

    #[test]
    fn picking_a_microphone_in_the_combined_list_edits_the_input() {
        let state = lanes();
        let menu = DeviceMenu::both(&state);
        assert_eq!(
            menu.actions_for(4, &state),
            vec![
                UiAction::SetEditDirection(DeviceDirection::Input),
                UiAction::SelectInput(2)
            ]
        );
    }

    /// A Bluetooth headset's microphone, as WirePlumber 0.5 names its loopback.
    fn bluetooth_microphone() -> AudioDevice {
        AudioDevice {
            name: "bluez_input.AC_80_0A_12_34_56".to_owned(),
            description: "WH-1000XM4".to_owned(),
            form_factor: "headset".into(),
            ..device("WH-1000XM4", DeviceDirection::Input)
        }
    }

    /// The window on the microphone's lane, attached to `microphone`, beside some speakers.
    fn on_the_microphone(microphone: AudioDevice) -> UiState {
        let mut state = UiState {
            view: ViewMode::Pro,
            direction: DeviceDirection::Input,
            devices: vec![device("Speakers", DeviceDirection::Output), microphone],
            ..UiState::default()
        };
        state.set_selection(DeviceDirection::Output, Some(0));
        state.set_selection(DeviceDirection::Input, Some(1));
        state
    }

    #[test]
    fn only_a_bluetooth_microphone_s_picker_explains_when_it_wakes() {
        let state = on_the_microphone(bluetooth_microphone());
        assert_eq!(
            device_tip(&state, DeviceDirection::Input).as_deref(),
            Some(BLUETOOTH_MICROPHONE_TIP)
        );
        assert_eq!(device_tip(&state, DeviceDirection::Output), None);

        // A USB headset says `headset` too, and is awake whenever its lane is.
        let usb = AudioDevice {
            name: "alsa_input.usb-Jabra_Evolve2".to_owned(),
            form_factor: "headset".into(),
            ..device("Jabra Evolve2", DeviceDirection::Input)
        };
        assert_eq!(
            device_tip(&on_the_microphone(usb), DeviceDirection::Input),
            None
        );

        // WirePlumber 0.4's own node for the headset's microphone is one as well.
        let old = AudioDevice {
            name: "bluez_input.AC_80_0A_12_34_56.0".to_owned(),
            ..bluetooth_microphone()
        };
        assert!(is_bluetooth_microphone(&old));

        let quiet = UiState {
            hide_tooltips: true,
            ..on_the_microphone(bluetooth_microphone())
        };
        assert_eq!(
            device_tip(&quiet, DeviceDirection::Input),
            None,
            "\"Hide help tips\" hides it"
        );
    }

    #[test]
    fn hovering_a_bluetooth_microphone_s_picker_shows_the_tip() {
        use crate::layout;
        use egui::Event;
        use fxsound_core::ThemeMode;

        let mut harness = testing::Harness::new(ThemeMode::Dark);
        harness.ctx.all_styles_mut(|style| {
            style.interaction.tooltip_delay = 0.0;
            style.interaction.show_tooltips_only_when_still = false;
        });
        let over = layout::pro::device_combo(DeviceDirection::Input).center();
        // A frame to hover, one to lay the tip out, one that paints it.
        let shown = |harness: &mut testing::Harness, state: &UiState| {
            harness.frame(state, vec![Event::PointerMoved(over)]);
            harness.frame(state, Vec::new());
            let (_, shapes) = harness.frame(state, Vec::new());
            testing::texts(&shapes)
                .into_iter()
                .any(|(text, _, _)| text == BLUETOOTH_MICROPHONE_TIP)
        };
        assert!(shown(
            &mut harness,
            &on_the_microphone(bluetooth_microphone())
        ));
        let usb = device("USB microphone", DeviceDirection::Input);
        assert!(!shown(&mut harness, &on_the_microphone(usb)));
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
