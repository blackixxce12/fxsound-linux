//! The Settings window: a side nav and three panes.
//!
//! Port of `FxSettingsDialog` (`GUI/FxSettingsDialog.{h,cpp}`) and of the device-priority list it
//! owns (`GUI/FxOutputPreference.{h,cpp}`). 610 × 597 outside, 600 × 510 of content, three tab
//! buttons down the left and one pane to the right of a vertical rule
//! (`docs/spec/06-dialogs.md` §1).
//!
//! ## Pure view
//!
//! [`SettingsDialog`] borrows [`SettingsState`] **immutably** and returns a
//! [`DialogResponse<SettingsAction>`]. Nothing here writes a setting, reorders a device, deletes a
//! preset file or even changes the selected tab: the app layer applies every action and hands the
//! new state back next frame, which is the same contract [`crate::state`] sets for the main
//! window. It is also what makes the whole window testable without a sound server, a settings
//! file or a compositor.
//!
//! ## Three deliberate departures
//!
//! 1. **The five hotkey rows are read-only.** A Wayland client cannot register a global shortcut —
//!    `RegisterHotKey` has no equivalent, and the `GlobalShortcuts` portal hands the binding UI to
//!    the desktop, not to the app (`docs/spec/06-dialogs.md` §9.5). The rows still list the five
//!    commands and the chords the Windows build ships, because those are what a user wants to
//!    reproduce, but they are a **reference table**, not editors: there is no border to click
//!    into, no focus ring and no "press a chord now" state, and the note above them says where the
//!    binding really lives. The "Disable keyboard shortcuts" checkbox is gone with them — with
//!    nothing to register there is nothing for it to switch off, and a checkbox whose only effect
//!    is to grey out a table of text is exactly the dead control §9.5 warns against. The `hotkeys`
//!    key and the five chord strings stay in [`fxsound_core::Settings`] regardless, so the schema
//!    still migrates (Open question 5).
//! 2. **"Automatic updates" is gone.** `updater.exe /silent` has no Linux analogue and this fork
//!    never contacts the network; §9.6 says to drop the toggle and keep the key, and
//!    `fxsound_core::settings` already documents it as inert. The Maintenance section says so in
//!    one line instead of offering a switch that does nothing.
//! 3. **"Reset presets to factory defaults" asks first.** The original deletes every user preset
//!    file with no confirmation at all (`FxController.cpp:1334-1382`, Open question 4). The button
//!    here only emits [`SettingsAction::ResetPresets`]; putting a
//!    [`super::MessageBox`] in front of it — and routing the deletions through the XDG trash — is
//!    the app layer's job, and this module's doc is the record that it must.
//!
//! `launch_toggle_`, on the other hand, is *restored*: the original hides "Launch on system
//! startup" behind `OperatingSystemType == Windows7` (`FxSettingsDialog.cpp:402-406`), so nobody
//! has seen it since 2015. On Linux it writes `~/.config/autostart/fxsound.desktop` (§9.6) and is
//! always shown.

use super::{
    ChromeResponse, DialogChrome, DialogResponse, TextButton, draw_truncated, draw_wrapped, link,
    normal_font, small_font, title_font,
};
use crate::assets::{AssetCache, FxImage, rasterise};
use crate::theme::{FxColor, Palette};
use crate::widgets::FxComboBox;
use crate::widgets::icon_button::IconButton;
use egui::{
    Align2, Color32, Context, CornerRadius, CursorIcon, Id, Key, Rect, Sense, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Ui, UiBuilder, Vec2, pos2, vec2,
};
use fxsound_core::Settings;
use std::collections::HashMap;

// =============================================================================================
// Window and nav geometry (`docs/spec/06-dialogs.md` §1.3)
// =============================================================================================

/// `SettingsComponent::WIDTH` × `HEIGHT` (`FxSettingsDialog.h:190-191`).
use fxsound_core::i18n::{self, tr};

/// `SettingsComponent`'s content size.
pub const CONTENT_SIZE: Vec2 = vec2(600.0, 510.0);
/// Outer window size, from the shared formula.
pub const WINDOW_SIZE: Vec2 = vec2(610.0, 597.0);
/// `SettingsComponent::BUTTON_X` (`FxSettingsDialog.h:201`).
pub const BUTTON_X: f32 = 20.0;
/// `SettingsComponent::BUTTON_Y` — where the first tab button starts (`FxSettingsDialog.h:202`).
pub const BUTTON_Y: f32 = 50.0;
/// `BUTTON_WIDTH` × `BUTTON_HEIGHT` (`FxSettingsDialog.h:203-204`).
pub const BUTTON_SIZE: Vec2 = vec2(150.0, 40.0);
/// Vertical gap between tab buttons (`FxSettingsDialog.cpp:122-123`).
pub const BUTTON_GAP: f32 = 20.0;
/// `SettingsComponent::SEPARATOR_X` (`FxSettingsDialog.h:205`) — the pane starts one point right.
pub const SEPARATOR_X: f32 = 152.0;
/// `SettingsPane::X_MARGIN` (`FxSettingsDialog.h:85`).
pub const X_MARGIN: f32 = 20.0;
/// `SettingsPane::Y_MARGIN` (`FxSettingsDialog.h:86`).
pub const Y_MARGIN: f32 = 5.0;
/// `SettingsPane::TITLE_HEIGHT` (`FxSettingsDialog.h:87`).
pub const TITLE_HEIGHT: f32 = 24.0;

/// The tab button's icon square: side = button height, corner = height / 4
/// (`FxSettingsDialog.cpp:58-60`).
pub const NAV_ICON_CORNER: f32 = BUTTON_SIZE.y / 4.0;
/// The icon is drawn in that square `reduced(10, 10)` (`FxSettingsDialog.cpp:62`).
pub const NAV_ICON_INSET: f32 = 10.0;
/// The label starts `height + 5` points in (`FxSettingsDialog.cpp:73-75`).
pub const NAV_LABEL_GAP: f32 = 5.0;

/// The pane, to the right of the rule.
///
/// The original computes `(SEPARATOR_X + 1, 1, getWidth() - SEPARATOR_X + 1, getHeight() - 1)`
/// (`FxSettingsDialog.cpp:125`), and `153 + 449 = 602` overhangs the 600-point content by two
/// points — a sign error. `docs/spec/06-dialogs.md` §1.3 says to use 447 and to note the
/// difference, which is what this does; every widget inside is then derived from *this* rect, so
/// the panes are two points narrower than the Windows build's and nothing else moves.
#[must_use]
pub fn pane_rect(content: Rect) -> Rect {
    Rect::from_min_max(
        pos2(content.left() + SEPARATOR_X + 1.0, content.top() + 1.0),
        pos2(content.right(), content.bottom()),
    )
}

/// The vertical rule between the nav and the pane.
///
/// `FxSettingsDialog::paint` draws this at *window* x 152 while `SettingsComponent` lays the pane
/// out at *content* x 153, and the content is offset by `SHADOW_WIDTH`, so the original's line
/// lands five points to the left of the pane's edge and shows through the tab buttons' labels
/// (§1.2). Drawn once, at the pane's edge, here.
#[must_use]
pub fn divider_x(content: Rect) -> f32 {
    pane_rect(content).left() - 1.0
}

/// One of the three tab buttons: `(20, 50, 150, 40)`, `(20, 110, …)`, `(20, 170, …)`
/// (`FxSettingsDialog.cpp:121-123`).
#[must_use]
pub fn nav_button_rect(content: Rect, index: usize) -> Rect {
    Rect::from_min_size(
        pos2(
            content.left() + BUTTON_X,
            content.top() + BUTTON_Y + index as f32 * (BUTTON_SIZE.y + BUTTON_GAP),
        ),
        BUTTON_SIZE,
    )
}

/// A pane's title: `(20, 5, paneWidth - 20, 24)` (`FxSettingsDialog.cpp:168-172`).
#[must_use]
pub fn pane_title_rect(pane: Rect) -> Rect {
    Rect::from_min_size(
        pos2(pane.left() + X_MARGIN, pane.top() + Y_MARGIN),
        vec2(pane.width() - X_MARGIN, TITLE_HEIGHT),
    )
}

// =============================================================================================
// Tabs
// =============================================================================================

/// Which pane is showing. Audio is selected at construction (`FxSettingsDialog.cpp:93`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SettingsTab {
    #[default]
    Audio,
    General,
    Help,
}

impl SettingsTab {
    /// In nav order.
    pub const ALL: [Self; 3] = [Self::Audio, Self::General, Self::Help];

    /// The tab button's caption — also the component's name, which is what `TRANS` is given
    /// (`FxSettingsDialog.cpp:92-105`).
    #[must_use]
    pub const fn nav_label(self) -> &'static str {
        match self {
            Self::Audio => "Audio",
            Self::General => "General",
            Self::Help => "Help",
        }
    }

    /// The pane's own heading, which is **not** the same string as the tab's for General
    /// (`FxSettingsDialog.cpp:336`).
    #[must_use]
    pub const fn pane_title(self) -> &'static str {
        match self {
            Self::Audio => "Audio",
            Self::General => "General Preferences",
            Self::Help => "Help",
        }
    }

    /// The icon in the tab button's rounded square.
    #[must_use]
    pub const fn icon(self) -> NavIcon {
        match self {
            Self::Audio => NavIcon::Speaker,
            Self::General => NavIcon::Settings,
            Self::Help => NavIcon::Question,
        }
    }

    #[must_use]
    const fn index(self) -> usize {
        match self {
            Self::Audio => 0,
            Self::General => 1,
            Self::Help => 2,
        }
    }
}

// =============================================================================================
// The three nav icons
// =============================================================================================

/// The side-nav artwork.
///
/// These three are the only images in the app that `FxTheme`'s table does not hold: the dialog
/// loads them straight from `BinaryData` (`FxSettingsDialog.cpp:92-105`), and they have no
/// per-theme variant — they are drawn in the same neutral grey in both palettes.
/// [`crate::AssetCache`] is keyed by [`FxImage`] and has nowhere to put them, so [`NavIcons`] is
/// the same rasterise-once-and-keep-the-texture cache with its own key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavIcon {
    /// `speaker.svg`.
    Speaker,
    /// `settings.svg`.
    Settings,
    /// `question.svg`.
    Question,
}

const NAV_SVGS: [&[u8]; 3] = [
    include_bytes!("../../../../assets/images/speaker.svg"),
    include_bytes!("../../../../assets/images/settings.svg"),
    include_bytes!("../../../../assets/images/question.svg"),
];

impl NavIcon {
    /// The raw SVG bytes.
    #[must_use]
    pub fn svg_bytes(self) -> &'static [u8] {
        NAV_SVGS[self as usize]
    }
}

/// Textures for the three nav icons, one per physical size.
///
/// Held by the application next to its [`crate::AssetCache`]; `load_texture` must never run per
/// frame.
#[derive(Default)]
pub struct NavIcons {
    textures: HashMap<(NavIcon, u32, u32), TextureHandle>,
}

impl std::fmt::Debug for NavIcons {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NavIcons")
            .field("textures", &self.textures.len())
            .finish()
    }
}

impl NavIcons {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A texture for `icon` at `size_points`, rasterised at the context's current scale.
    pub fn texture(
        &mut self,
        ctx: &Context,
        icon: NavIcon,
        size_points: Vec2,
    ) -> Option<&TextureHandle> {
        let scale = ctx.pixels_per_point().max(0.1);
        let width_px = (size_points.x * scale).round().max(1.0) as u32;
        let height_px = (size_points.y * scale).round().max(1.0) as u32;
        let key = (icon, width_px, height_px);

        if let std::collections::hash_map::Entry::Vacant(slot) = self.textures.entry(key) {
            let image = rasterise(icon.svg_bytes(), width_px, height_px)?;
            let handle = ctx.load_texture(
                format!("{icon:?}-{width_px}x{height_px}"),
                image,
                TextureOptions::LINEAR,
            );
            slot.insert(handle);
        }
        self.textures.get(&key)
    }

    /// Drop every cached texture.
    pub fn clear(&mut self) {
        self.textures.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.textures.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }
}

// =============================================================================================
// Hotkeys (`docs/spec/06-dialogs.md` §1.8, §9.5)
// =============================================================================================

/// The five commands a global shortcut can run, in the original's order
/// (`FxSettingsDialog.cpp:342-344`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyCommand {
    OnOff,
    OpenClose,
    NextPreset,
    PreviousPreset,
    ChangeOutput,
}

impl HotkeyCommand {
    pub const ALL: [Self; 5] = [
        Self::OnOff,
        Self::OpenClose,
        Self::NextPreset,
        Self::PreviousPreset,
        Self::ChangeOutput,
    ];

    /// The row's caption.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OnOff => "Turn FxSound On/Off",
            Self::OpenClose => "Open/Close FxSound",
            Self::NextPreset => "Use Next Preset",
            Self::PreviousPreset => "Use Previous Preset",
            Self::ChangeOutput => "Change Playback Device",
        }
    }

    /// The settings key (`FxController.h:54-58`), unchanged by the port so the file stays
    /// greppable against the C++.
    #[must_use]
    pub const fn settings_key(self) -> &'static str {
        match self {
            Self::OnOff => "cmd_on_off",
            Self::OpenClose => "cmd_open_close",
            Self::NextPreset => "cmd_next_preset",
            Self::PreviousPreset => "cmd_previous_preset",
            Self::ChangeOutput => "cmd_change_output",
        }
    }

    /// The chord the Windows build ships, decoded from its Win32 hotkey code
    /// (`Utils/Settings/Settings.cpp:34-38`) — see [`fxsound_core::settings::Hotkeys`].
    #[must_use]
    pub fn binding(self, settings: &Settings) -> &str {
        let hotkeys = &settings.hotkey_bindings;
        match self {
            Self::OnOff => &hotkeys.cmd_on_off,
            Self::OpenClose => &hotkeys.cmd_open_close,
            Self::NextPreset => &hotkeys.cmd_next_preset,
            Self::PreviousPreset => &hotkeys.cmd_previous_preset,
            Self::ChangeOutput => &hotkeys.cmd_change_output,
        }
    }

    /// What a compositor keybinding should actually run.
    ///
    /// These are the flags `packaging/hyprland.conf.example` binds; each one reaches the running
    /// instance over the control socket. This is the column that makes the table useful rather
    /// than decorative — it is the thing the user copies.
    #[must_use]
    pub const fn command_line(self) -> &'static str {
        match self {
            Self::OnOff => "fxsound --toggle-power",
            Self::OpenClose => "fxsound --toggle-window",
            Self::NextPreset => "fxsound --next-preset",
            Self::PreviousPreset => "fxsound --prev-preset",
            Self::ChangeOutput => "fxsound --next-output",
        }
    }
}

/// What the note above the hotkey table says.
pub const HOTKEY_NOTE: &str = "A Wayland client cannot bind global shortcuts: the compositor owns \
them. Bind these commands in your compositor's configuration — each one reaches the running \
instance over FxSound's control socket.";
/// The shipped example the note points at.
pub const HOTKEY_EXAMPLE_FILE: &str = "packaging/hyprland.conf.example";
/// Heading of the hotkey block, which replaces the original's "Disable keyboard shortcuts".
pub const HOTKEY_TITLE: &str = "Keyboard shortcuts";

// =============================================================================================
// Languages (`docs/spec/06-dialogs.md` §1.8)
// =============================================================================================

/// One position of the language switch.
///
/// The Windows build's `FxLanguage` cycles through its 30 codes; this port puts the desktop
/// session's language first — the position a fresh install is in — and then the same list,
/// minus Hungarian, which the Windows binary never actually shipped a table for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageChoice {
    /// Follow the desktop session (`Settings::language_follows_system`).
    System,
    /// One of [`i18n::LANGUAGES`], by code.
    Code(&'static str),
}

impl LanguageChoice {
    /// Every position, in order.
    #[must_use]
    pub fn all() -> Vec<Self> {
        std::iter::once(Self::System)
            .chain(i18n::LANGUAGES.iter().map(|language| Self::Code(language.code)))
            .collect()
    }

    /// The position the settings are in right now. An explicit code with no table falls back to
    /// the system entry, which is also what [`i18n::resolve`] does with it.
    #[must_use]
    pub fn current(settings: &Settings) -> Self {
        if settings.language_follows_system {
            return Self::System;
        }
        i18n::language(&settings.language).map_or(Self::System, |language| Self::Code(language.code))
    }

    /// What the switch shows: the native name, untranslated, as `getLanguageName` returns it
    /// (`FxController.cpp:2471-2594`) — and, for the system entry, which language that is.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::System => format!(
                "{} · {}",
                tr("System language"),
                i18n::native_name(i18n::system_language())
            ),
            Self::Code(code) => i18n::native_name(code).to_owned(),
        }
    }

    /// The position `steps` away, wrapping in both directions (`FxLanguage.cpp:80-111`).
    #[must_use]
    pub fn step(self, steps: isize) -> Self {
        let all = Self::all();
        let count = all.len() as isize;
        let current = all.iter().position(|choice| *choice == self).unwrap_or(0) as isize;
        all[(current + steps).rem_euclid(count) as usize]
    }

    /// What [`SettingsAction::SetLanguage`] carries for this position: `None` follows the system.
    #[must_use]
    pub fn setting(self) -> Option<String> {
        match self {
            Self::System => None,
            Self::Code(code) => Some(code.to_owned()),
        }
    }
}

/// One row of the output-device priority list (`FxOutputPreference.cpp:104-181`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePriority {
    /// `node.name` — stable across restarts, and what `device_configs` is keyed on. Also the id
    /// salt for the row's preset combo, because salting by row index would leave an open popup
    /// attached to the wrong device after a reorder (`docs/spec/06-dialogs.md` §9.2).
    pub id: String,
    /// `node.description`, what the row shows after its `"1. "` rank prefix.
    pub name: String,
    /// Index into [`SettingsState::presets`], or `None` for the `"Select preset"` placeholder.
    pub preset: Option<usize>,
    /// `false` for a remembered device that is not currently usable: the name greys out
    /// (`FxOutputPreference.cpp:141`).
    pub connected: bool,
    /// `false` for a device that is not enumerated at all any more: the ✕ appears, and only then
    /// (`FxOutputPreference.cpp:130`).
    pub present: bool,
}

impl DevicePriority {
    /// `sprintf("%d. ", row + 1) + device_name` (`FxOutputPreference.cpp:140`).
    #[must_use]
    pub fn label(&self, index: usize) -> String {
        format!("{}. {}", index + 1, self.name)
    }
}

/// Everything the Settings window draws.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    /// The persisted settings, read-only here: every change leaves as a [`SettingsAction`].
    pub settings: Settings,
    /// Which pane is showing.
    pub tab: SettingsTab,
    /// The device priority list, in priority order.
    pub devices: Vec<DevicePriority>,
    /// The selected row, which is what ▲/▼ and Shift+Up/Shift+Down act on.
    pub selected_device: Option<usize>,
    /// Every preset name, for the per-device preset picker.
    pub presets: Vec<String>,
    /// Whether the user has anything to lose: the reset button is enabled iff there is at least
    /// one user preset **or** some preset is modified (`FxSettingsDialog.cpp:210-220`).
    pub can_reset_presets: bool,
    /// The application version, shown as `"v" + version` and never translated
    /// (`FxSettingsDialog.cpp:542`). Pass `env!("CARGO_PKG_VERSION")`.
    pub version: String,
    /// Whether FxSound starts with the session.
    ///
    /// Not part of [`Settings`], and deliberately so: the original reads it live out of
    /// `HKCU\…\CurrentVersion\Run` rather than from its own settings file
    /// (`FxController.cpp:2789-2812`), and the Linux equivalent is the existence of
    /// `~/.config/autostart/fxsound.desktop` with `Hidden=false`
    /// (`docs/spec/06-dialogs.md` §9.6). The app layer reads the file and fills this in.
    pub launch_on_startup: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self::new(Settings::default())
    }
}

impl SettingsState {
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            tab: SettingsTab::default(),
            devices: Vec::new(),
            selected_device: None,
            presets: Vec::new(),
            can_reset_presets: false,
            version: String::new(),
            launch_on_startup: false,
        }
    }

    /// `"v1.1.25"` (`FxSettingsDialog.cpp:542`).
    #[must_use]
    pub fn version_text(&self) -> String {
        format!("v{}", self.version)
    }

    /// The row above `index`, if it can move up.
    #[must_use]
    pub fn can_move_up(&self, index: usize) -> bool {
        index > 0 && index < self.devices.len()
    }

    /// Whether `index` can move down.
    #[must_use]
    pub fn can_move_down(&self, index: usize) -> bool {
        index + 1 < self.devices.len()
    }
}

/// Something the user did in the Settings window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    /// A tab button was pressed. Plain radio behaviour (`FxSettingsDialog.cpp:131-163`).
    SelectTab(SettingsTab),

    // ---- audio pane ---------------------------------------------------------------------------
    /// A device row was clicked; it becomes the one ▲/▼ and Shift+Up/Down act on.
    SelectDeviceRow(usize),
    /// Swap this row with the one above and persist (`FxOutputPreference.cpp:238-248`).
    MoveDeviceUp(usize),
    /// Swap with the one below (`:250-260`).
    MoveDeviceDown(usize),
    /// Forget a device that is no longer present (`:262-272`).
    RemoveDevice(usize),
    /// Bind a preset to a device. If the row is the *current* output the app also applies the
    /// preset live (`FxOutputPreference.cpp:63-75`).
    SetDevicePreset { device: usize, preset: usize },
    /// `prioritize_new_output` — a newly seen device goes to the top of the list rather than the
    /// bottom (`DeviceConfig.cpp:57`, `:78-85`).
    SetPrioritizeNewOutput(bool),
    /// The user asked to restore the factory presets.
    ///
    /// **Destructive**: this deletes every user preset file. The original does it with no
    /// confirmation whatsoever (`FxController.cpp:1353-1366`); the app layer must put a Yes/No in
    /// front of it and send the files to the XDG trash (`docs/spec/06-dialogs.md` Open question 4,
    /// §9.6).
    ResetPresets,

    // ---- general pane -------------------------------------------------------------------------
    /// A new UI language: a code from [`i18n::LANGUAGES`], or `None` to follow the desktop
    /// session (see [`LanguageChoice`]).
    SetLanguage(Option<String>),
    /// Write or remove `~/.config/autostart/fxsound.desktop` (§9.6).
    SetLaunchOnStartup(bool),
    /// `hide_help_tooltips` — suppresses every control tooltip in the main window.
    SetHideHelpTips(bool),
    /// `hide_notifications` — suppresses the desktop notifications.
    SetHideNotifications(bool),
    /// Show the shipped `packaging/hyprland.conf.example`. The app knows where its own data files
    /// were installed; this crate must not guess.
    ShowHotkeyExample,

    // ---- help pane ----------------------------------------------------------------------------
    /// Open a URL in the user's browser.
    OpenUrl(String),
    /// Show the bundled changelog — what the original's "Changelog" link opened on the web
    /// (`FxSettingsDialog.cpp:474-475`); this fork carries it in the package instead.
    ShowChangelog,

    /// Close the window — the ✕ or Escape (`FxSettingsDialog.cpp:78-88`). The caller then runs
    /// the equivalent of `FxController::refreshOutputList()` (`FxMainWindow.cpp:454`).
    Close,
}

// =============================================================================================
// The window
// =============================================================================================

/// The Settings window.
pub struct SettingsDialog<'a> {
    state: &'a SettingsState,
}

impl<'a> SettingsDialog<'a> {
    #[must_use]
    pub fn new(state: &'a SettingsState) -> Self {
        Self { state }
    }

    /// Draw the whole window into `outer`, which should be [`WINDOW_SIZE`].
    pub fn show(
        self,
        ui: &mut Ui,
        outer: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        icons: &mut NavIcons,
    ) -> DialogResponse<SettingsAction> {
        let id = Id::new("fx_settings_dialog");
        let mut response = DialogResponse::default();

        let ChromeResponse {
            content,
            close_clicked,
            ..
        } = DialogChrome::titled(&tr("Settings")).show(ui, outer, palette, assets, id.with("chrome"));
        response.push_if(close_clicked, SettingsAction::Close);
        response.push_if(
            ui.input(|i| i.key_pressed(Key::Escape)),
            SettingsAction::Close,
        );

        // §1.2's rule, drawn once and at the pane's edge.
        ui.painter().vline(
            divider_x(content),
            content.y_range(),
            Stroke::new(1.0, palette.color(FxColor::Outline)),
        );

        for tab in SettingsTab::ALL {
            let rect = nav_button_rect(content, tab.index());
            if nav_button(ui, rect, tab, self.state.tab == tab, palette, icons) {
                response.push(SettingsAction::SelectTab(tab));
            }
        }

        let pane = pane_rect(content);
        draw_truncated(
            ui.painter(),
            &tr(self.state.tab.pane_title()),
            title_font(),
            palette.color(FxColor::HighlightedText),
            pane_title_rect(pane),
            Align2::LEFT_CENTER,
        );

        match self.state.tab {
            SettingsTab::Audio => {
                audio_pane(ui, pane, self.state, palette, assets, id, &mut response);
            }
            SettingsTab::General => {
                general_pane(ui, pane, self.state, palette, assets, id, &mut response);
            }
            SettingsTab::Help => help_pane(ui, pane, self.state, palette, id, &mut response),
        }
        response
    }
}

/// One tab button (`FxSettingsDialog.cpp:46-76`). Returns whether it was clicked.
fn nav_button(
    ui: &mut Ui,
    rect: Rect,
    tab: SettingsTab,
    selected: bool,
    palette: Palette,
    icons: &mut NavIcons,
) -> bool {
    let response = ui.interact(
        rect,
        Id::new("fx_settings_nav").with(tab.nav_label()),
        Sense::click(),
    );

    let square = Rect::from_min_size(rect.min, Vec2::splat(rect.height()));
    let fill = if selected {
        palette.color(FxColor::MenuHighlightBackground)
    } else {
        palette.color(FxColor::MenuBackground)
    };
    ui.painter()
        .rect_filled(square, CornerRadius::same(NAV_ICON_CORNER as u8), fill);

    let art = square.shrink(NAV_ICON_INSET);
    if let Some(texture) = icons.texture(ui.ctx(), tab.icon(), art.size()) {
        ui.painter().image(
            texture.id(),
            art,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    let colour = if selected {
        palette.color(FxColor::HighlightedText)
    } else {
        palette.color(FxColor::DefaultText)
    };
    // `(height + 5, 0, width - height + 5, height)` — note the label is allowed five points more
    // than is left, so it may run one glyph past the button's right edge.
    let label = Rect::from_min_size(
        pos2(rect.left() + rect.height() + NAV_LABEL_GAP, rect.top()),
        vec2(rect.width() - rect.height() + NAV_LABEL_GAP, rect.height()),
    );
    draw_truncated(
        ui.painter(),
        &tr(tab.nav_label()),
        normal_font(),
        colour,
        label,
        Align2::LEFT_CENTER,
    );

    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response.clicked()
}

// =============================================================================================
// Audio pane (`docs/spec/06-dialogs.md` §1.6)
// =============================================================================================

/// Audio-pane geometry (`FxSettingsDialog.h:101-109`, `.cpp:241-262`).
pub mod audio {
    /// `GROUP_MARGIN` — how far the backdrop spreads past the widgets it groups.
    pub const GROUP_MARGIN: f32 = 10.0;
    /// `ENDPOINT_Y` — y of the "Output Device Preference" heading.
    pub const ENDPOINT_Y: f32 = 50.0;
    /// `LABEL_WIDTH` × `LABEL_HEIGHT` of that heading.
    /// The original's 220 (`FxSettingsDialog.cpp:243`) fits its English caption and nothing
    /// longer; nothing sits to the caption's right, so the port lets it run to the group's edge.
    pub const LABEL_WIDTH: f32 = 400.0;
    pub const LABEL_HEIGHT: f32 = 14.0;
    /// `OUTPUT_PREFERENCE_HEIGHT`.
    pub const LIST_HEIGHT: f32 = 260.0;
    /// `TOGGLE_BUTTON_HEIGHT`.
    pub const TOGGLE_HEIGHT: f32 = 30.0;
    /// `RESET_PRESETS_BUTTON_WIDTH` — the reset button never gets narrower than this.
    pub const RESET_MIN_WIDTH: f32 = 220.0;
    /// `MAX_BUTTON_WIDTH` — nor wider than this.
    pub const RESET_MAX_WIDTH: f32 = 315.0;
    /// `BUTTON_HEIGHT`, per line of label.
    pub const RESET_LINE_HEIGHT: f32 = 24.0;
    /// The reset button grows to at most three lines (`FxSettingsDialog.cpp:296`).
    pub const RESET_MAX_LINES: usize = 3;
    /// The group backdrop's corner (`FxSettingsDialog.cpp:270`).
    pub const GROUP_CORNER: f32 = 8.0;
    /// …and its alpha over `DefaultFill` (`FxSettingsDialog.cpp:268`).
    pub const GROUP_ALPHA: f32 = 0.2;
}

/// `"Output Device Preference"` (`FxSettingsDialog.cpp:281`).
pub const OUTPUT_PREFERENCE_TITLE: &str = "Output Device Preference";
/// `"Prioritize new output devices"` (`FxSettingsDialog.cpp:283`).
pub const PRIORITIZE_NEW_OUTPUT: &str = "Prioritize new output devices";
/// `"Reset presets to factory defaults"` (`FxSettingsDialog.cpp:285`).
pub const RESET_PRESETS: &str = "Reset presets to factory defaults";
/// The list's tooltip (`FxOutputPreference.cpp:316`).
pub const PRIORITY_TOOLTIP: &str = "Use Shift+Up or Shift+Down to change the device priority";

/// `(20, 50, 220, 14)`.
#[must_use]
pub fn output_title_rect(pane: Rect) -> Rect {
    Rect::from_min_size(
        pos2(pane.left() + X_MARGIN, pane.top() + audio::ENDPOINT_Y),
        vec2(audio::LABEL_WIDTH, audio::LABEL_HEIGHT),
    )
}

/// `(20, 74, paneWidth - 50, 260)` — the width is `paneW - (X_MARGIN + 5) * 2`
/// (`FxSettingsDialog.cpp:246-248`).
#[must_use]
pub fn output_list_rect(pane: Rect) -> Rect {
    Rect::from_min_size(
        pos2(pane.left() + X_MARGIN, output_title_rect(pane).bottom() + 10.0),
        vec2(pane.width() - (X_MARGIN + 5.0) * 2.0, audio::LIST_HEIGHT),
    )
}

/// `(20, 344, paneWidth - 50, 30)` (`FxSettingsDialog.cpp:250-251`).
#[must_use]
pub fn prioritize_toggle_rect(pane: Rect) -> Rect {
    let list = output_list_rect(pane);
    Rect::from_min_size(
        pos2(list.left(), list.bottom() + 10.0),
        vec2(list.width(), audio::TOGGLE_HEIGHT),
    )
}

/// The rounded backdrop behind the heading, the list and the checkbox: each edge ten points out
/// (`FxSettingsDialog.cpp:254-258`).
#[must_use]
pub fn group_rect(pane: Rect) -> Rect {
    let title = output_title_rect(pane);
    Rect::from_min_max(
        pos2(
            title.left() - audio::GROUP_MARGIN,
            title.top() - audio::GROUP_MARGIN,
        ),
        pos2(
            output_list_rect(pane).right() + audio::GROUP_MARGIN,
            prioritize_toggle_rect(pane).bottom() + audio::GROUP_MARGIN,
        ),
    )
}

/// `resizeResetButton` (`FxSettingsDialog.cpp:289-315`).
///
/// `text_width` is the label measured in the button's own font; `getBestWidthForHeight` adds the
/// button's height to it **[JUCE semantics]**. The height is 24 per line of label, up to three;
/// the width is clamped into `220..=315`.
#[must_use]
pub fn reset_button_size(label: &str, text_width: f32) -> Vec2 {
    let explicit = label.lines().count().max(1);
    // The original only counted explicit line breaks; a translation wider than the button's
    // maximum wraps inside it, so those lines are counted too, or the second line would hang
    // out of the bottom of the button (port addition).
    let usable = audio::RESET_MAX_WIDTH - audio::RESET_LINE_HEIGHT;
    let wrapped = (text_width / usable).ceil().max(1.0) as usize;
    let lines = explicit.max(wrapped).clamp(1, audio::RESET_MAX_LINES) as f32;
    let height = audio::RESET_LINE_HEIGHT * lines;
    // `min(MAX_BUTTON_WIDTH)` and then `if (width < RESET_PRESETS_BUTTON_WIDTH) width = …`, which
    // is a clamp written out (`FxSettingsDialog.cpp:308-313`).
    let width = (text_width + height).clamp(audio::RESET_MIN_WIDTH, audio::RESET_MAX_WIDTH);
    vec2(width, height)
}

/// `(20, 404, …)` — thirty points below the checkbox (`FxSettingsDialog.cpp:259-260`).
#[must_use]
pub fn reset_button_rect(pane: Rect, size: Vec2) -> Rect {
    Rect::from_min_size(
        pos2(
            pane.left() + X_MARGIN,
            prioritize_toggle_rect(pane).bottom() + 30.0,
        ),
        size,
    )
}

#[allow(clippy::too_many_arguments)]
fn audio_pane(
    ui: &mut Ui,
    pane: Rect,
    state: &SettingsState,
    palette: Palette,
    assets: &mut AssetCache,
    id: Id,
    response: &mut DialogResponse<SettingsAction>,
) {
    ui.painter().rect_filled(
        group_rect(pane),
        CornerRadius::same(audio::GROUP_CORNER as u8),
        palette.color_alpha(FxColor::DefaultFill, audio::GROUP_ALPHA),
    );

    draw_truncated(
        ui.painter(),
        &tr(OUTPUT_PREFERENCE_TITLE),
        normal_font(),
        palette.color(FxColor::HighlightedText),
        output_title_rect(pane),
        Align2::LEFT_CENTER,
    );

    output_preference(
        ui,
        output_list_rect(pane),
        state,
        palette,
        assets,
        id.with("outputs"),
        response,
    );

    if toggle(
        ui,
        prioritize_toggle_rect(pane),
        &tr(PRIORITIZE_NEW_OUTPUT),
        state.settings.prioritize_new_output,
        true,
        palette,
        id.with("prioritize"),
    ) {
        response.push(SettingsAction::SetPrioritizeNewOutput(
            !state.settings.prioritize_new_output,
        ));
    }

    let reset_label = tr(RESET_PRESETS);
    let text_width = ui
        .painter()
        .layout_no_wrap(reset_label.clone(), normal_font(), Color32::PLACEHOLDER)
        .size()
        .x;
    let size = reset_button_size(&reset_label, text_width);
    if TextButton::new(&reset_label)
        .enabled(state.can_reset_presets)
        .show(ui, reset_button_rect(pane, size), palette, id.with("reset"))
        .clicked()
    {
        response.push(SettingsAction::ResetPresets);
    }
}

// ---------------------------------------------------------------------------------------------
// The device priority list (`docs/spec/06-dialogs.md` §1.7)
// ---------------------------------------------------------------------------------------------

/// Row geometry (`FxOutputPreference.h:35-37`, `:103`).
pub mod device_row {
    /// `ROW_HEIGHT`.
    pub const HEIGHT: f32 = 40.0;
    /// `BUTTON_WIDTH` — the ▲, ▼ and ✕ buttons are square.
    pub const BUTTON_WIDTH: f32 = 18.0;
    /// `MARGIN`.
    pub const MARGIN: f32 = 5.0;
    /// `PRESET_LIST_WIDTH`.
    pub const PRESET_WIDTH: f32 = 150.0;
    /// The container's rounded corner (`FxOutputPreference.cpp:360`).
    pub const CORNER: f32 = 8.0;
    /// The list is inset `reduced(5, 10)` inside it (`FxOutputPreference.cpp:364`).
    pub const LIST_INSET_X: f32 = 5.0;
    pub const LIST_INSET_Y: f32 = 10.0;
    /// An unselected row's separator is half a point thick; a selected one's is a whole point
    /// (`FxOutputPreference.cpp:183-195`).
    pub const SEPARATOR_THICKNESS: f32 = 0.5;
    pub const SELECTED_SEPARATOR_THICKNESS: f32 = 1.0;
    /// The outline alpha of an unselected row's preset combo (`FxOutputPreference.cpp:156`).
    pub const UNSELECTED_OUTLINE_ALPHA: f32 = 0.5;
}

/// The ▲ button: at the margin, but pulled in half a button's width on the last row, where there
/// is no ▼ beside it (`FxOutputPreference.cpp:108-118`).
#[must_use]
pub fn up_button_rect(row: Rect, index: usize, count: usize) -> Rect {
    let x = if index + 1 < count {
        device_row::MARGIN
    } else {
        device_row::MARGIN + device_row::BUTTON_WIDTH / 2.0
    };
    button_square(row, x)
}

/// The ▼ button: immediately right of ▲, or in ▲'s place on the first row, which has no ▲
/// (`FxOutputPreference.cpp:119-127`).
#[must_use]
pub fn down_button_rect(row: Rect, index: usize, count: usize) -> Rect {
    let x = if index == 0 {
        device_row::MARGIN + device_row::BUTTON_WIDTH / 2.0
    } else {
        up_button_rect(row, index, count).right() - row.left()
    };
    button_square(row, x)
}

/// The ✕ button.
///
/// The original computes `bounds.getWidth() - BUTTON_WIDTH - MARGIN` where `bounds` is the row
/// `reduced(2)` (`FxOutputPreference.cpp:129`) — a width used as an x, which lands the button four
/// points left of where the five-point margin implies. Corrected here: the ✕ is a margin in from
/// the inset row's right edge, like everything else.
#[must_use]
pub fn remove_button_rect(row: Rect) -> Rect {
    let bounds = row.shrink(2.0);
    Rect::from_min_size(
        pos2(
            bounds.right() - device_row::MARGIN - device_row::BUTTON_WIDTH,
            row.top() + (device_row::HEIGHT - device_row::BUTTON_WIDTH) / 2.0,
        ),
        Vec2::splat(device_row::BUTTON_WIDTH),
    )
}

/// The per-device preset combo: 150 wide, full inset height, left of the ✕
/// (`FxOutputPreference.cpp:133`).
#[must_use]
pub fn preset_combo_rect(row: Rect) -> Rect {
    let bounds = row.shrink(2.0);
    Rect::from_min_size(
        pos2(
            remove_button_rect(row).left() - device_row::PRESET_WIDTH - device_row::MARGIN,
            bounds.top(),
        ),
        vec2(device_row::PRESET_WIDTH, bounds.height()),
    )
}

/// The device name: from past both arrow buttons to a margin short of the combo
/// (`FxOutputPreference.cpp:135-137`).
#[must_use]
pub fn device_name_rect(row: Rect) -> Rect {
    let bounds = row.shrink(2.0);
    let left = row.left() + device_row::MARGIN * 2.0 + device_row::BUTTON_WIDTH * 2.0;
    Rect::from_min_max(
        pos2(left, bounds.top()),
        pos2(
            preset_combo_rect(row).left() - device_row::MARGIN,
            bounds.bottom(),
        ),
    )
}

fn button_square(row: Rect, x: f32) -> Rect {
    Rect::from_min_size(
        pos2(
            row.left() + x,
            row.top() + (device_row::HEIGHT - device_row::BUTTON_WIDTH) / 2.0,
        ),
        Vec2::splat(device_row::BUTTON_WIDTH),
    )
}

#[allow(clippy::too_many_arguments)]
fn output_preference(
    ui: &mut Ui,
    rect: Rect,
    state: &SettingsState,
    palette: Palette,
    assets: &mut AssetCache,
    id: Id,
    response: &mut DialogResponse<SettingsAction>,
) {
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(device_row::CORNER as u8),
        palette.color(FxColor::WidgetBackground),
    );
    let list = rect.shrink2(vec2(device_row::LIST_INSET_X, device_row::LIST_INSET_Y));
    let count = state.devices.len();

    let hovered = ui.interact(rect, id.with("list"), Sense::hover());
    if !state.settings.hide_help_tooltips {
        // `ListBox::setTooltip` (`FxOutputPreference.cpp:316`), suppressed by the General pane's
        // "Hide help tips" checkbox like every other tooltip in the app.
        let _ = hovered.on_hover_text(
            egui::RichText::new(PRIORITY_TOOLTIP)
                .font(small_font())
                .color(palette.color(FxColor::DefaultText)),
        );
    }

    ui.scope_builder(UiBuilder::new().max_rect(list).id_salt(id), |ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        egui::ScrollArea::vertical()
            .id_salt(id)
            .max_height(list.height())
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, device) in state.devices.iter().enumerate() {
                    let (row, click) = ui.allocate_exact_size(
                        vec2(ui.available_width(), device_row::HEIGHT),
                        Sense::click(),
                    );
                    let selected = state.selected_device == Some(index);
                    if click.clicked() {
                        response.push(SettingsAction::SelectDeviceRow(index));
                    }
                    device_row(
                        ui, row, index, count, device, selected, state, palette, assets, response,
                    );
                }
            });
    });

    // `keyPressed` moves the selected row while the list has keyboard focus
    // (`FxOutputPreference.cpp:339-351`). There is no invisible focus target here, so the gate is
    // simply "a row is selected", which is the same condition from the user's side.
    if let Some(index) = state.selected_device {
        let (up, down) = ui.input(|i| {
            (
                i.modifiers.shift && i.key_pressed(Key::ArrowUp),
                i.modifiers.shift && i.key_pressed(Key::ArrowDown),
            )
        });
        if up && state.can_move_up(index) {
            response.push(SettingsAction::MoveDeviceUp(index));
        }
        if down && state.can_move_down(index) {
            response.push(SettingsAction::MoveDeviceDown(index));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn device_row(
    ui: &mut Ui,
    row: Rect,
    index: usize,
    count: usize,
    device: &DevicePriority,
    selected: bool,
    state: &SettingsState,
    palette: Palette,
    assets: &mut AssetCache,
    response: &mut DialogResponse<SettingsAction>,
) {
    let id = Id::new("fx_device_row").with(&device.id);

    // The arrows swap to the accent-coloured "selected" artwork for the selected row, and hover
    // to it otherwise (`FxOutputPreference.cpp:148-155`).
    if index > 0 {
        let button = if selected {
            IconButton::new(FxImage::ArrowUpSelected)
        } else {
            IconButton::new(FxImage::ArrowUp).hover(FxImage::ArrowUpSelected)
        };
        if button
            .min_hit_size(device_row::BUTTON_WIDTH)
            .show(
                ui,
                up_button_rect(row, index, count),
                palette,
                assets,
                id.with("up"),
            )
            .clicked()
        {
            response.push(SettingsAction::MoveDeviceUp(index));
        }
    }
    if index + 1 < count {
        let button = if selected {
            IconButton::new(FxImage::ArrowDownSelected)
        } else {
            IconButton::new(FxImage::ArrowDown).hover(FxImage::ArrowDownSelected)
        };
        if button
            .min_hit_size(device_row::BUTTON_WIDTH)
            .show(
                ui,
                down_button_rect(row, index, count),
                palette,
                assets,
                id.with("down"),
            )
            .clicked()
        {
            response.push(SettingsAction::MoveDeviceDown(index));
        }
    }
    // The ✕ only exists for an entry whose device is not there any anymore
    // (`FxOutputPreference.cpp:130`).
    if !device.present
        && IconButton::new(FxImage::RemoveButton)
            .min_hit_size(device_row::BUTTON_WIDTH)
            .show(ui, remove_button_rect(row), palette, assets, id.with("remove"))
            .clicked()
    {
        response.push(SettingsAction::RemoveDevice(index));
    }

    let name = device_name_rect(row);
    let colour = if device.connected {
        palette.color(FxColor::DefaultText)
    } else {
        palette.color(FxColor::HintText)
    };
    draw_truncated(
        ui.painter(),
        &device.label(index),
        normal_font(),
        colour,
        name,
        Align2::LEFT_CENTER,
    );

    // `drawLine(x, bottom - 0.5, right, bottom - 1.0)` gives the selected row's rule a half-point
    // slope (`FxOutputPreference.cpp:188`). Drawn flat.
    let (thickness, rule) = if selected {
        (
            device_row::SELECTED_SEPARATOR_THICKNESS,
            palette.color(FxColor::SelectedRowOutline),
        )
    } else {
        (
            device_row::SEPARATOR_THICKNESS,
            palette.color(FxColor::RowOutline),
        )
    };
    ui.painter().hline(
        name.left()..=name.right(),
        name.bottom() - 0.5,
        Stroke::new(thickness, rule),
    );

    let (_, picked) = FxComboBox::new(&state.presets, device.preset)
        .placeholder(&tr("Select preset"))
        .show(
            ui,
            preset_combo_rect(row),
            palette,
            assets,
            id.with("preset"),
        );
    if let Some(preset) = picked {
        response.push(SettingsAction::SetDevicePreset {
            device: index,
            preset,
        });
    }

    // The combo's own outline is the row's selection marker: full strength when selected, half
    // when not (`FxOutputPreference.cpp:150`, `:156`).
    let outline = if selected {
        palette.color(FxColor::SelectedRowOutline)
    } else {
        palette.color_alpha(FxColor::RowOutline, device_row::UNSELECTED_OUTLINE_ALPHA)
    };
    ui.painter().rect_stroke(
        preset_combo_rect(row),
        CornerRadius::same(crate::widgets::combo::corner_radius(preset_combo_rect(row).height())
            as u8),
        Stroke::new(1.0, outline),
        StrokeKind::Inside,
    );
}

// =============================================================================================
// General pane (`docs/spec/06-dialogs.md` §1.8)
// =============================================================================================

/// General-pane geometry (`FxSettingsDialog.h:137-143`, `.cpp:420-448`).
pub mod general {
    use super::X_MARGIN;

    /// `LANGUAGE_SWITCH_Y`.
    pub const LANGUAGE_Y: f32 = 50.0;
    /// `FxLanguage`'s own size (`FxLanguage.cpp:40-48`).
    /// The original's switch is 180 wide (`FxLanguage.h:31`); this port's first entry names the
    /// language it resolves to ("System language · русский"), which needs the room.
    pub const LANGUAGE_WIDTH: f32 = 300.0;
    pub const LANGUAGE_HEIGHT: f32 = 30.0;
    /// `TOGGLE_BUTTON_HEIGHT`.
    pub const TOGGLE_HEIGHT: f32 = 30.0;
    /// Gap from the language switch to the first checkbox (`FxSettingsDialog.cpp:426`).
    pub const GAP_AFTER_LANGUAGE: f32 = 20.0;
    /// Gap from "Launch on system startup" to the next checkbox — twenty, not ten
    /// (`FxSettingsDialog.cpp:430`).
    pub const GAP_AFTER_LAUNCH: f32 = 20.0;
    /// Gap between the remaining checkboxes (`FxSettingsDialog.cpp:436`, `:439`).
    pub const GAP_BETWEEN_TOGGLES: f32 = 10.0;
    /// `HOTKEY_LABEL_X = X_MARGIN + 25`.
    pub const HOTKEY_X: f32 = X_MARGIN + 25.0;
    /// `HOTKEY_LABEL_HEIGHT`.
    pub const HOTKEY_ROW_HEIGHT: f32 = 20.0;
    /// `HOTKEY_LABEL_HEIGHT + 10` (`FxSettingsDialog.cpp:446`).
    pub const HOTKEY_PITCH: f32 = HOTKEY_ROW_HEIGHT + 10.0;
    /// `FxHotkeyLabel::HOTKEY_LABEL_WIDTH` — the command's name column
    /// (`FxHotkeyLabel.h:56`, `.cpp:34-43`).
    /// The original's 170 (`FxHotkeyLabel.cpp:56`) plus the chord column it no longer needs: on
    /// Wayland the binding lives in the compositor, so the row shows the command to bind instead.
    pub const HOTKEY_NAME_WIDTH: f32 = 200.0;
    /// `FxHotkeyEditor::HOTKEY_EDITOR_WIDTH` — the chord column, which is a label here, not an
    /// editor.
    pub const HOTKEY_CHORD_WIDTH: f32 = 120.0;
    /// A one-point gutter, as in `.withX(171)`.
    /// The original's columns touch (`FxHotkeyLabel.cpp:56-60`); a translated command name
    /// runs right up to the command line without a gutter, so the port keeps ten points clear.
    pub const HOTKEY_GUTTER: f32 = 10.0;
    /// Height of the explanatory note above the table — four lines of the small font, which is
    /// what the English text wraps to at the pane's width.
    pub const NOTE_HEIGHT: f32 = 70.0;
    /// Height of the link to the shipped example.
    pub const LINK_HEIGHT: f32 = 24.0;
}

/// `"Launch on system startup"` (`FxSettingsDialog.cpp:337`).
pub const LAUNCH_ON_STARTUP: &str = "Launch on system startup";
/// `"Hide help tips for audio controls"` (`FxSettingsDialog.cpp:338`).
pub const HIDE_HELP_TIPS: &str = "Hide help tips for audio controls";
/// `"Hide notifications"` (`FxSettingsDialog.cpp:339`).
pub const HIDE_NOTIFICATIONS: &str = "Hide notifications";

/// `(20, 50, 180, 30)`.
#[must_use]
pub fn language_rect(pane: Rect) -> Rect {
    Rect::from_min_size(
        pos2(pane.left() + X_MARGIN, pane.top() + general::LANGUAGE_Y),
        vec2(general::LANGUAGE_WIDTH, general::LANGUAGE_HEIGHT),
    )
}

/// The three checkboxes: `(20, 100, …)`, `(20, 150, …)`, `(20, 190, …)`.
///
/// The gaps are not uniform — twenty below the language switch, twenty below
/// "Launch on system startup", ten between the rest (`FxSettingsDialog.cpp:426-439`). These are
/// the y values the original only ever reaches with `launch_toggle_` visible, which on Windows it
/// never is; here it always is.
#[must_use]
pub fn toggle_rect(pane: Rect, index: usize) -> Rect {
    let mut top = language_rect(pane).bottom() + general::GAP_AFTER_LANGUAGE;
    for previous in 0..index {
        top += general::TOGGLE_HEIGHT
            + if previous == 0 {
                general::GAP_AFTER_LAUNCH
            } else {
                general::GAP_BETWEEN_TOGGLES
            };
    }
    Rect::from_min_size(
        pos2(pane.left() + X_MARGIN, top),
        vec2(pane.width() - X_MARGIN, general::TOGGLE_HEIGHT),
    )
}

/// The hotkey block's heading, in the exact slot the "Disable keyboard shortcuts" checkbox had:
/// ten points below the last checkbox (`FxSettingsDialog.cpp:438-440`).
#[must_use]
pub fn hotkey_title_rect(pane: Rect) -> Rect {
    let last = toggle_rect(pane, 2);
    Rect::from_min_size(
        pos2(
            pane.left() + X_MARGIN,
            last.bottom() + general::GAP_BETWEEN_TOGGLES,
        ),
        vec2(pane.width() - X_MARGIN, general::HOTKEY_ROW_HEIGHT),
    )
}

/// The note explaining that the compositor owns the bindings.
#[must_use]
pub fn hotkey_note_rect(pane: Rect) -> Rect {
    let title = hotkey_title_rect(pane);
    Rect::from_min_size(
        pos2(title.left(), title.bottom() + 5.0),
        vec2(title.width(), general::NOTE_HEIGHT),
    )
}

/// One hotkey row: `(45, …, paneWidth - 45, 20)`, five points below the note and then thirty
/// apart (`FxSettingsDialog.cpp:442-447`).
#[must_use]
pub fn hotkey_row_rect(pane: Rect, index: usize) -> Rect {
    Rect::from_min_size(
        pos2(
            pane.left() + general::HOTKEY_X,
            hotkey_note_rect(pane).bottom() + 5.0 + index as f32 * general::HOTKEY_PITCH,
        ),
        vec2(
            pane.width() - general::HOTKEY_X,
            general::HOTKEY_ROW_HEIGHT,
        ),
    )
}

/// The link to `packaging/hyprland.conf.example`, under the table.
#[must_use]
pub fn hotkey_link_rect(pane: Rect) -> Rect {
    let last = hotkey_row_rect(pane, HotkeyCommand::ALL.len() - 1);
    Rect::from_min_size(
        pos2(last.left(), last.bottom() + 10.0),
        vec2(last.width(), general::LINK_HEIGHT),
    )
}

#[allow(clippy::too_many_arguments)]
fn general_pane(
    ui: &mut Ui,
    pane: Rect,
    state: &SettingsState,
    palette: Palette,
    assets: &mut AssetCache,
    id: Id,
    response: &mut DialogResponse<SettingsAction>,
) {
    let choice = LanguageChoice::current(&state.settings);
    if let Some(next) = language_switch(
        ui,
        language_rect(pane),
        choice,
        palette,
        assets,
        id.with("language"),
    ) {
        response.push(SettingsAction::SetLanguage(next.setting()));
    }

    let toggles = [
        (
            tr(LAUNCH_ON_STARTUP),
            state.launch_on_startup,
            SettingsAction::SetLaunchOnStartup(!state.launch_on_startup),
        ),
        (
            tr(HIDE_HELP_TIPS),
            state.settings.hide_help_tooltips,
            SettingsAction::SetHideHelpTips(!state.settings.hide_help_tooltips),
        ),
        (
            tr(HIDE_NOTIFICATIONS),
            state.settings.hide_notifications,
            SettingsAction::SetHideNotifications(!state.settings.hide_notifications),
        ),
    ];
    for (index, (label, checked, action)) in toggles.into_iter().enumerate() {
        if toggle(
            ui,
            toggle_rect(pane, index),
            &label,
            checked,
            true,
            palette,
            id.with(("toggle", index)),
        ) {
            response.push(action);
        }
    }

    draw_truncated(
        ui.painter(),
        &tr(HOTKEY_TITLE),
        normal_font(),
        palette.color(FxColor::HighlightedText),
        hotkey_title_rect(pane),
        Align2::LEFT_CENTER,
    );
    draw_wrapped(
        ui.painter(),
        &tr(HOTKEY_NOTE),
        small_font(),
        palette.color(FxColor::DefaultText),
        hotkey_note_rect(pane),
    );

    for (index, command) in HotkeyCommand::ALL.into_iter().enumerate() {
        hotkey_row(
            ui,
            hotkey_row_rect(pane, index),
            command,
            &state.settings,
            palette,
        );
    }

    if link(
        ui,
        hotkey_link_rect(pane),
        HOTKEY_EXAMPLE_FILE,
        palette,
        Align2::LEFT_TOP,
        id.with("hotkey-example"),
    )
    .clicked()
    {
        response.push(SettingsAction::ShowHotkeyExample);
    }
}

/// One row of the hotkey reference table.
///
/// `FxHotkeyLabel` puts a 170-point name beside a 120-point `FxHotkeyEditor` — a focusable text
/// field with a rounded two-point border that thickens on focus and accepts a chord
/// (`FxHotkeyLabel.cpp:56-222`). None of that is drawn here: the field cannot accept a binding on
/// Wayland, and a bordered box that takes focus and then refuses to do anything is worse than no
/// box at all. What is left is three columns of text — the command, the chord the Windows build
/// ships, and the command line to bind.
fn hotkey_row(ui: &mut Ui, rect: Rect, command: HotkeyCommand, settings: &Settings, palette: Palette) {
    let name = Rect::from_min_size(
        rect.min,
        vec2(general::HOTKEY_NAME_WIDTH, rect.height()),
    );
    draw_truncated(
        ui.painter(),
        &tr(command.label()),
        small_font(),
        palette.color(FxColor::DefaultText),
        name,
        Align2::LEFT_CENTER,
    );

    // The Windows chord (`command.binding(settings)`) is not shown: it is not what runs the
    // command here, and the room is better spent on the translated name and the command line.
    let _ = settings;
    let line = Rect::from_min_max(
        pos2(name.right() + general::HOTKEY_GUTTER, rect.top()),
        rect.max,
    );
    draw_truncated(
        ui.painter(),
        command.command_line(),
        small_font(),
        palette.color(FxColor::DefaultText),
        line,
        Align2::LEFT_CENTER,
    );
}

/// `FxLanguage` (`FxLanguage.h:29-38`, `.cpp:40-111`): a rounded box with a ‹ and a › either side
/// of the current language's own name. Returns the code the user moved to.
fn language_switch(
    ui: &mut Ui,
    rect: Rect,
    choice: LanguageChoice,
    palette: Palette,
    assets: &mut AssetCache,
    id: Id,
) -> Option<LanguageChoice> {
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(5),
        palette.color(FxColor::ControlBackground),
    );

    let arrow = |index: f32| {
        Rect::from_min_size(
            pos2(
                rect.left() + if index < 0.0 { 10.0 } else { rect.width() - 14.0 - 10.0 },
                rect.top() + 4.0,
            ),
            vec2(14.0, 22.0),
        )
    };
    let prev = IconButton::new(FxImage::ArrowPrev)
        .disabled_image(FxImage::ArrowPrevBW)
        .show(ui, arrow(-1.0), palette, assets, id.with("prev"))
        .clicked();
    let next = IconButton::new(FxImage::ArrowNext)
        .disabled_image(FxImage::ArrowNextBW)
        .show(ui, arrow(1.0), palette, assets, id.with("next"))
        .clicked();

    draw_truncated(
        ui.painter(),
        &choice.label(),
        normal_font(),
        palette.color(FxColor::DefaultText),
        Rect::from_min_size(
            pos2(rect.left() + 24.0, rect.top() + 4.0),
            vec2(rect.width() - 48.0, 22.0),
        ),
        Align2::CENTER_CENTER,
    );

    // Both directions wrap, so neither arrow is ever disabled (`FxLanguage.cpp:80-111`).
    match (prev, next) {
        (true, _) => Some(choice.step(-1)),
        (_, true) => Some(choice.step(1)),
        _ => None,
    }
}

// =============================================================================================
// Help pane (`docs/spec/06-dialogs.md` §1.9)
// =============================================================================================

/// Help-pane geometry (`FxSettingsDialog.h:165-170`, `.cpp:512-524`).
pub mod help {
    /// `TEXT_Y` — y of the first section title.
    pub const TEXT_Y: f32 = 50.0;
    /// `TITLE_HEIGHT` of a section heading.
    pub const SECTION_HEIGHT: f32 = 24.0;
    /// `TEXT_HEIGHT` of a line of body text.
    pub const TEXT_HEIGHT: f32 = 20.0;
    /// `HYPERLINK_HEIGHT`.
    pub const LINK_HEIGHT: f32 = 24.0;
    /// Links and the body text under a heading are indented five points past it
    /// (`FxSettingsDialog.cpp:519`).
    pub const INDENT: f32 = 5.0;
    /// Gap from a heading to its text, and from that text to its link.
    pub const GAP: f32 = 10.0;
    /// Gap from a section's last row to the next heading (`FxSettingsDialog.cpp:523`).
    pub const SECTION_GAP: f32 = 20.0;
}

/// `"Version"` (`FxSettingsDialog.cpp:472`).
pub const VERSION_TITLE: &str = "Version";
/// `"Changelog"` (`FxSettingsDialog.cpp:474`).
pub const CHANGELOG_LINK: &str = "Changelog";

/// The three rows of the Help pane, top to bottom — what is left of the original's eight
/// (`docs/spec/06-dialogs.md` §1.9). "Support" and its "Help center" link pointed at the
/// upstream website, "Maintenance" only held the updater's toggle, and neither belongs to a
/// fork that never contacts the network; the changelog is bundled and shown in-app instead.
#[must_use]
fn help_rows(pane: Rect) -> [Rect; 3] {
    let left = pane.left() + X_MARGIN;
    let width = pane.width() - X_MARGIN;
    let indent = left + help::INDENT;
    let indent_width = width - help::INDENT;

    let version_title = Rect::from_min_size(
        pos2(left, pane.top() + help::TEXT_Y),
        vec2(width, help::SECTION_HEIGHT),
    );
    let version_text = Rect::from_min_size(
        pos2(left, version_title.bottom() + help::GAP),
        vec2(width, help::TEXT_HEIGHT),
    );
    let changelog = Rect::from_min_size(
        pos2(indent, version_text.bottom() + help::GAP),
        vec2(indent_width, help::LINK_HEIGHT),
    );
    [version_title, version_text, changelog]
}

fn help_pane(
    ui: &mut Ui,
    pane: Rect,
    state: &SettingsState,
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<SettingsAction>,
) {
    let [version_title, version_text, changelog] = help_rows(pane);

    draw_truncated(
        ui.painter(),
        &tr(VERSION_TITLE),
        normal_font(),
        palette.color(FxColor::HighlightedText),
        version_title,
        Align2::LEFT_CENTER,
    );
    // Never translated (`FxSettingsDialog.cpp:542`).
    draw_truncated(
        ui.painter(),
        &state.version_text(),
        small_font(),
        palette.color(FxColor::DefaultText),
        version_text,
        Align2::LEFT_CENTER,
    );
    if link(
        ui,
        changelog,
        &tr(CHANGELOG_LINK),
        palette,
        Align2::LEFT_TOP,
        id.with("changelog"),
    )
    .clicked()
    {
        response.push(SettingsAction::ShowChangelog);
    }
}

// =============================================================================================
// The checkbox
// =============================================================================================

/// The tick box's side **[JUCE semantics]**.
///
/// `FxTheme` overrides neither `drawToggleButton` nor `drawTickBox`, and the JUCE modules are not
/// vendored in this tree, so the tick's size, its inset and the label's offset have **no citable
/// value** (`docs/spec/06-dialogs.md` Open question 1). These three numbers are the port's own.
pub const TICK_BOX_SIDE: f32 = 18.0;
/// Gap between the tick box and its label **[JUCE semantics]**.
pub const TICK_BOX_GAP: f32 = 10.0;
/// The tick box's corner **[JUCE semantics]**.
pub const TICK_BOX_CORNER: f32 = 4.0;

/// A `juce::ToggleButton`. Tick colour and text colour are both `TextButton::textColourOnId`, i.e.
/// `HighlightedText` (`FxSettingsDialog.cpp:352-353`). Returns whether it was clicked.
fn toggle(
    ui: &mut Ui,
    rect: Rect,
    label: &str,
    checked: bool,
    enabled: bool,
    palette: Palette,
    id: Id,
) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, id, sense);

    let colour = if enabled {
        palette.color(FxColor::HighlightedText)
    } else {
        palette.color(FxColor::HintText)
    };
    let box_ = Align2::LEFT_CENTER.align_size_within_rect(Vec2::splat(TICK_BOX_SIDE), rect);
    ui.painter().rect_stroke(
        box_,
        CornerRadius::same(TICK_BOX_CORNER as u8),
        Stroke::new(1.5, colour),
        StrokeKind::Inside,
    );
    if checked {
        let tick = box_.shrink(4.0);
        let stroke = Stroke::new(2.0, colour);
        let elbow = pos2(
            tick.left() + tick.width() * 0.36,
            tick.top() + tick.height() * 0.82,
        );
        ui.painter()
            .line_segment([pos2(tick.left(), tick.center().y), elbow], stroke);
        ui.painter()
            .line_segment([elbow, pos2(tick.right(), tick.top())], stroke);
    }

    draw_truncated(
        ui.painter(),
        label,
        normal_font(),
        colour,
        Rect::from_min_max(
            pos2(box_.right() + TICK_BOX_GAP, rect.top()),
            pos2(rect.right(), rect.bottom()),
        ),
        Align2::LEFT_CENTER,
    );

    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response.clicked()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{frame, test_context};
    use super::*;
    use fxsound_core::ThemeMode;

    fn content() -> Rect {
        Rect::from_min_size(pos2(5.0, 62.0), CONTENT_SIZE)
    }

    /// The pane the *original* lays its widgets out in: two points wider than the port's, so the
    /// spec's tables can be checked against the same functions.
    fn original_pane() -> Rect {
        Rect::from_min_size(pos2(158.0, 63.0), vec2(449.0, 509.0))
    }

    fn local(pane: Rect, r: Rect) -> Rect {
        r.translate(-pane.min.to_vec2())
    }

    fn populated() -> SettingsState {
        let devices = vec![
            DevicePriority {
                id: "alsa_output.pci-0000_00_1f.3".into(),
                name: "Speakers".into(),
                preset: Some(1),
                connected: true,
                present: true,
            },
            DevicePriority {
                id: "bluez_output.AC_12".into(),
                name: "Headphones".into(),
                preset: None,
                connected: false,
                present: false,
            },
        ];
        SettingsState {
            version: "0.1.0".into(),
            presets: vec!["General".into(), "Rock".into(), "Jazz".into()],
            devices,
            selected_device: Some(0),
            can_reset_presets: true,
            ..SettingsState::new(Settings::default())
        }
    }

    #[test]
    fn the_window_is_six_hundred_and_ten_by_five_hundred_and_ninety_seven() {
        assert!((super::super::outer_size(CONTENT_SIZE) - WINDOW_SIZE).length() < 1e-4);
    }

    #[test]
    fn the_three_tab_buttons_land_on_the_originals_rows() {
        // FxSettingsDialog.cpp:121-123, in content-local coordinates.
        let content = content();
        for (index, top) in [(0, 50.0), (1, 110.0), (2, 170.0)] {
            let button = local(content, nav_button_rect(content, index));
            assert!((button.min - pos2(20.0, top)).length() < 1e-4, "{button:?}");
            assert!((button.size() - vec2(150.0, 40.0)).length() < 1e-4);
        }
    }

    #[test]
    fn the_pane_is_two_points_narrower_than_the_originals_overhanging_one() {
        let content = content();
        let pane = pane_rect(content);
        assert!((pane.left() - (content.left() + 153.0)).abs() < 1e-4);
        assert!((pane.top() - (content.top() + 1.0)).abs() < 1e-4);
        // The original's `getWidth() - SEPARATOR_X + 1` is 449 and overhangs by two points; 447
        // stops exactly at the content's edge.
        assert!((pane.width() - 447.0).abs() < 1e-4, "{pane:?}");
        assert!((pane.right() - content.right()).abs() < 1e-4);
        assert!((pane.height() - 509.0).abs() < 1e-4);
        // The rule sits at the pane's edge, not five points to its left as in the original.
        assert!((divider_x(content) - (pane.left() - 1.0)).abs() < 1e-4);
    }

    #[test]
    fn the_audio_panes_widgets_match_the_specs_table_when_given_the_originals_pane() {
        // docs/spec/06-dialogs.md §1.6, which is derived from the 449-wide pane.
        let pane = original_pane();
        let expect = |r: Rect, min: egui::Pos2, size: Vec2| {
            let r = local(pane, r);
            assert!((r.min - min).length() < 1e-4, "{r:?} should start at {min:?}");
            assert!((r.size() - size).length() < 1e-4, "{r:?} should be {size:?}");
        };
        expect(pane_title_rect(pane), pos2(20.0, 5.0), vec2(429.0, 24.0));
        expect(output_title_rect(pane), pos2(20.0, 50.0), vec2(400.0, 14.0));
        expect(output_list_rect(pane), pos2(20.0, 74.0), vec2(399.0, 260.0));
        expect(prioritize_toggle_rect(pane), pos2(20.0, 344.0), vec2(399.0, 30.0));
        expect(group_rect(pane), pos2(10.0, 40.0), vec2(419.0, 344.0));
        // The reset button's row, whatever its measured width comes out as.
        let row = local(pane, reset_button_rect(pane, vec2(220.0, 24.0)));
        assert!((row.min - pos2(20.0, 404.0)).length() < 1e-4, "{row:?}");
    }

    #[test]
    fn the_reset_button_is_clamped_between_two_twenty_and_three_fifteen() {
        // FxSettingsDialog.cpp:308-313.
        let one_line = reset_button_size(RESET_PRESETS, 10.0);
        assert!((one_line.y - 24.0).abs() < 1e-4);
        assert!((one_line.x - 220.0).abs() < 1e-4, "a tiny label should still be 220 wide");

        let wide = reset_button_size(RESET_PRESETS, 1000.0);
        assert!((wide.x - 315.0).abs() < 1e-4, "{wide:?}");
        // …and a label that has to wrap gets a line per wrap, so the text stays inside: 400
        // points of text in a 291-point line is two lines.
        let translated = reset_button_size(RESET_PRESETS, 400.0);
        assert!((translated.y - 48.0).abs() < 1e-4, "{translated:?}");
        assert!((wide.y - 72.0).abs() < 1e-4, "1000 points wraps to the three-line cap: {wide:?}");

        // Height is 24 per line, up to three; a fourth line does not make it taller.
        let two = reset_button_size("Reset presets to\nfactory defaults", 10.0);
        assert!((two.y - 48.0).abs() < 1e-4);
        let many = reset_button_size("a\nb\nc\nd\ne", 10.0);
        assert!((many.y - 72.0).abs() < 1e-4, "{many:?}");
    }

    #[test]
    fn a_device_row_lays_out_like_the_specs_diagram() {
        // docs/spec/06-dialogs.md §1.7 for a 389 x 40 row: buttons at y 11, the combo 150 wide.
        let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(389.0, 40.0));
        let middle_up = up_button_rect(row, 1, 3);
        assert!((middle_up.min - pos2(5.0, 11.0)).length() < 1e-4, "{middle_up:?}");
        assert!((middle_up.size() - vec2(18.0, 18.0)).length() < 1e-4);

        let middle_down = down_button_rect(row, 1, 3);
        assert!((middle_down.left() - 23.0).abs() < 1e-4, "{middle_down:?}");

        // The first row has no ▲, so its ▼ moves into the gap; the last has no ▼, so its ▲ does.
        assert!((down_button_rect(row, 0, 3).left() - 14.0).abs() < 1e-4);
        assert!((up_button_rect(row, 2, 3).left() - 14.0).abs() < 1e-4);

        let combo = preset_combo_rect(row);
        assert!((combo.width() - 150.0).abs() < 1e-4);
        assert!((combo.top() - 2.0).abs() < 1e-4);
        assert!((combo.height() - 36.0).abs() < 1e-4);

        let name = device_name_rect(row);
        assert!((name.left() - 46.0).abs() < 1e-4, "{name:?}");
        assert!((name.right() - (combo.left() - 5.0)).abs() < 1e-4);
    }

    #[test]
    fn the_remove_button_is_a_margin_in_from_the_row_not_four_points_further_left() {
        // FxOutputPreference.cpp:129 uses `bounds.getWidth()` where `bounds.getRight()` is meant,
        // which would put a 389 point row's ✕ at x 366 instead of 364. This port corrects it.
        let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(389.0, 40.0));
        let remove = remove_button_rect(row);
        assert!((remove.right() - (389.0 - 2.0 - 5.0)).abs() < 1e-4, "{remove:?}");
        assert!((remove.size() - vec2(18.0, 18.0)).length() < 1e-4);
        assert!((remove.top() - 11.0).abs() < 1e-4);
    }

    #[test]
    fn nothing_in_a_device_row_overlaps_anything_else() {
        let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(389.0, 40.0));
        let name_left = device_name_rect(row).left();
        for (index, count) in [(0, 3), (1, 3), (2, 3), (0, 1)] {
            // A button only has a rect worth checking on the rows that draw it: the first row has
            // no ▲ and the last no ▼ (`FxOutputPreference.cpp:100-101`).
            let up = (index > 0).then(|| up_button_rect(row, index, count));
            let down = (index + 1 < count).then(|| down_button_rect(row, index, count));
            if let (Some(up), Some(down)) = (up, down) {
                assert!(up.right() <= down.left() + 1e-4, "{up:?} overlaps {down:?}");
            }
            for button in [up, down].into_iter().flatten() {
                assert!(
                    button.right() <= name_left + 1e-4,
                    "{button:?} runs under the device name"
                );
            }
        }
        assert!(device_name_rect(row).right() <= preset_combo_rect(row).left());
        assert!(preset_combo_rect(row).right() <= remove_button_rect(row).left());
    }

    #[test]
    fn a_device_row_is_numbered_from_one() {
        let device = DevicePriority {
            id: "id".into(),
            name: "Speakers (Realtek)".into(),
            preset: None,
            connected: true,
            present: true,
        };
        assert_eq!(device.label(0), "1. Speakers (Realtek)");
        assert_eq!(device.label(9), "10. Speakers (Realtek)");
    }

    #[test]
    fn the_general_pane_keeps_the_originals_hotkey_column_geometry() {
        let pane = original_pane();
        let language = local(pane, language_rect(pane));
        assert!((language.min - pos2(20.0, 50.0)).length() < 1e-4, "{language:?}");
        assert!((language.size() - vec2(300.0, 30.0)).length() < 1e-4);

        // Exactly where the original puts them once `launch_toggle_` is visible: 20 below the
        // switch, 20 below the launch checkbox, then 10 apart (`FxSettingsDialog.cpp:426-439`).
        for (index, top) in [(0, 100.0), (1, 150.0), (2, 190.0)] {
            let row = local(pane, toggle_rect(pane, index));
            assert!((row.min - pos2(20.0, top)).length() < 1e-4, "{row:?}");
            assert!((row.size() - vec2(429.0, 30.0)).length() < 1e-4);
        }
        // …and the hotkey block starts in the slot "Disable keyboard shortcuts" used to occupy.
        let title = local(pane, hotkey_title_rect(pane));
        assert!((title.top() - 230.0).abs() < 1e-4, "{title:?}");
        assert!(hotkey_note_rect(pane).bottom() <= hotkey_row_rect(pane, 0).top());

        // Five rows, thirty apart, indented to x 45 and 20 tall — all as in the original.
        let rows: Vec<Rect> = (0..5).map(|i| hotkey_row_rect(pane, i)).collect();
        for row in &rows {
            assert!((row.left() - (pane.left() + 45.0)).abs() < 1e-4);
            assert!((row.height() - 20.0).abs() < 1e-4);
        }
        for pair in rows.windows(2) {
            assert!((pair[1].top() - pair[0].top() - 30.0).abs() < 1e-4);
        }
        // And the whole block still fits the pane.
        assert!(hotkey_link_rect(pane).bottom() <= pane.bottom());
    }

    #[test]
    fn the_hotkey_note_fits_the_box_it_is_wrapped_into() {
        // Measured with the real faces, because a note that overflows would be painted straight
        // over the first row of the table.
        let ctx = test_context();
        let pane = pane_rect(content());
        let box_ = hotkey_note_rect(pane);
        frame(&ctx, |ui| {
            let height = ui
                .painter()
                .layout(
                    HOTKEY_NOTE.to_owned(),
                    small_font(),
                    Color32::PLACEHOLDER,
                    box_.width(),
                )
                .size()
                .y;
            assert!(
                height <= box_.height(),
                "the note wrapped to {height} points in a {} point box",
                box_.height()
            );
        });
    }

    #[test]
    fn the_five_hotkey_commands_are_the_originals_five_in_order() {
        let expected = [
            ("Turn FxSound On/Off", "cmd_on_off", "Ctrl+Shift+Q"),
            ("Open/Close FxSound", "cmd_open_close", "Ctrl+Shift+E"),
            ("Use Next Preset", "cmd_next_preset", "Ctrl+Shift+A"),
            ("Use Previous Preset", "cmd_previous_preset", "Ctrl+Shift+Z"),
            ("Change Playback Device", "cmd_change_output", "Ctrl+Shift+W"),
        ];
        let settings = Settings::default();
        for (command, (label, key, chord)) in HotkeyCommand::ALL.into_iter().zip(expected) {
            assert_eq!(command.label(), label);
            assert_eq!(command.settings_key(), key);
            assert_eq!(command.binding(&settings), chord);
            // Every row names a command a compositor can actually run.
            assert!(
                command.command_line().starts_with("fxsound --"),
                "{:?} offers {}",
                command,
                command.command_line()
            );
        }
    }

    #[test]
    fn the_language_switch_starts_with_the_system_entry_and_then_the_originals_list() {
        let all = LanguageChoice::all();
        assert_eq!(all[0], LanguageChoice::System);
        assert_eq!(all.len(), 1 + i18n::LANGUAGES.len());
        assert_eq!(all[1], LanguageChoice::Code("en"));
        assert_eq!(all[all.len() - 1], LanguageChoice::Code("zh-TW"));
        // No Hungarian: the Windows binary declares it but never shipped its table.
        assert!(!all.contains(&LanguageChoice::Code("hu")));
    }

    #[test]
    fn the_language_switch_wraps_in_both_directions() {
        assert_eq!(LanguageChoice::System.step(-1), LanguageChoice::Code("zh-TW"));
        assert_eq!(LanguageChoice::Code("zh-TW").step(1), LanguageChoice::System);
        assert_eq!(LanguageChoice::System.step(1), LanguageChoice::Code("en"));
        assert_eq!(LanguageChoice::Code("en").step(1), LanguageChoice::Code("ar"));
    }

    #[test]
    fn the_current_choice_follows_the_settings_and_falls_back_to_the_system() {
        let mut settings = Settings::default();
        assert_eq!(LanguageChoice::current(&settings), LanguageChoice::System);
        settings.choose_language(Some("ru"));
        assert_eq!(LanguageChoice::current(&settings), LanguageChoice::Code("ru"));
        assert_eq!(LanguageChoice::Code("ru").setting().as_deref(), Some("ru"));
        assert_eq!(LanguageChoice::System.setting(), None);
        // An explicit code without a table (an old settings file, say) is the system entry.
        settings.choose_language(Some("hu"));
        assert_eq!(LanguageChoice::current(&settings), LanguageChoice::System);
    }

    #[test]
    fn the_switch_shows_native_names_and_names_the_system_language() {
        assert_eq!(LanguageChoice::Code("pt").label(), "Português");
        assert_eq!(LanguageChoice::Code("pt-br").label(), "português brasileiro");
        let system = LanguageChoice::System.label();
        assert!(system.contains(i18n::native_name(i18n::system_language())), "{system}");
    }

    #[test]
    fn the_help_pane_matches_the_specs_table() {
        // docs/spec/06-dialogs.md §1.9.
        let pane = original_pane();
        let [version_title, version_text, changelog] = help_rows(pane);
        let expect = |r: Rect, min: egui::Pos2, size: Vec2| {
            let r = local(pane, r);
            assert!((r.min - min).length() < 1e-4, "{r:?} should start at {min:?}");
            assert!((r.size() - size).length() < 1e-4, "{r:?} should be {size:?}");
        };
        expect(version_title, pos2(20.0, 50.0), vec2(429.0, 24.0));
        expect(version_text, pos2(20.0, 84.0), vec2(429.0, 20.0));
        expect(changelog, pos2(25.0, 114.0), vec2(424.0, 24.0));
    }

    #[test]
    fn the_version_string_is_not_translated_and_carries_its_v() {
        let state = SettingsState {
            version: "1.1.25".into(),
            ..SettingsState::default()
        };
        assert_eq!(state.version_text(), "v1.1.25");
    }

    #[test]
    fn the_defaults_match_the_persistence_table() {
        // docs/spec/06-dialogs.md §8.
        let settings = Settings::default();
        assert!(!settings.prioritize_new_output);
        assert!(!settings.hide_help_tooltips);
        assert!(!settings.hide_notifications);
        assert_eq!(settings.language, "en");
        assert!(settings.language_follows_system);
        assert_eq!(settings.max_user_presets, 120);
        assert_eq!(settings.device_configs_version, 2);
    }

    #[test]
    fn a_row_can_only_move_where_there_is_room() {
        let state = populated();
        assert!(!state.can_move_up(0));
        assert!(state.can_move_down(0));
        assert!(state.can_move_up(1));
        assert!(!state.can_move_down(1));
        // An index past the end cannot move at all.
        assert!(!state.can_move_up(9));
        assert!(!state.can_move_down(9));
    }

    #[test]
    fn every_pane_draws_in_both_palettes_and_asks_for_nothing_on_its_own() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let mut icons = NavIcons::new();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), WINDOW_SIZE);
        let mut state = populated();
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            for tab in SettingsTab::ALL {
                state.tab = tab;
                frame(&ctx, |ui| {
                    let response = SettingsDialog::new(&state).show(
                        ui,
                        outer,
                        Palette::new(mode),
                        &mut assets,
                        &mut icons,
                    );
                    assert!(
                        response.is_empty(),
                        "{tab:?} in {mode:?} emitted {:?}",
                        response.actions
                    );
                });
            }
        }
        // The three nav icons were rasterised exactly once each per size.
        assert!(!icons.is_empty());
        let cached = icons.len();
        icons.clear();
        assert!(icons.is_empty());
        assert_eq!(cached, 3);
    }

    #[test]
    fn an_empty_device_list_and_an_empty_preset_list_still_draw() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let mut icons = NavIcons::new();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), WINDOW_SIZE);
        let state = SettingsState::default();
        frame(&ctx, |ui| {
            let response = SettingsDialog::new(&state).show(
                ui,
                outer,
                Palette::new(ThemeMode::Dark),
                &mut assets,
                &mut icons,
            );
            assert!(response.is_empty());
        });
    }

    #[test]
    fn the_three_nav_icons_rasterise() {
        for icon in [NavIcon::Speaker, NavIcon::Settings, NavIcon::Question] {
            let raster = rasterise(icon.svg_bytes(), 20, 20)
                .unwrap_or_else(|| panic!("{icon:?} failed to render"));
            assert_eq!(raster.size, [20, 20]);
            assert!(
                raster.pixels.iter().any(|p| p.a() > 0),
                "{icon:?} rendered blank"
            );
        }
    }
}
