//! The system tray, as a StatusNotifierItem.
//!
//! Port of `FxSystemTrayView` (`fxsound/Source/GUI/FxSystemTrayView.cpp`, 477 lines): the icon's
//! four states (`:90-111`), the tooltip (`:78-84`), the left-click toggle (`:446-455`) and the
//! whole context menu (`showContextMenu`, `:216-330`).
//!
//! # Why `ksni` and not an X11 tray
//!
//! `Shell_NotifyIcon` has no direct equivalent. The legacy X11 system tray (XEmbed,
//! `_NET_SYSTEM_TRAY_S<n>`) works by the panel *reparenting* the client's X window into the tray
//! container, and Wayland has no cross-client window embedding at all; modern Wayland panels
//! (waybar, Plasma 6, GNOME Shell) implement only the D-Bus StatusNotifierItem protocol. SNI is
//! also the better fit: it carries a structured menu model (DBusMenu), so the port declares the
//! menu instead of drawing and positioning a popup by hand the way `showContextMenu` does
//! (`docs/spec/07-startup-tray.md` §5.8).
//!
//! # Three deliberate departures
//!
//! 1. **Three icons, not four.** Windows picks between a red and a blue "processing" icon
//!    depending on its own theme (`FxSystemTrayView.cpp:90-111`). A panel owns its own background
//!    and recolours symbolic icons itself, so keying tray artwork off *our* theme is wrong here;
//!    the four states collapse onto `com.fxsound.FxSound-{off,on,processing}`
//!    (`docs/spec/07-startup-tray.md` §5.8).
//! 2. **The status stays `Active`.** §5.8 suggests carrying "processing" as
//!    `Status::NeedsAttention`, but that is the state a panel flashes or highlights, and audio
//!    playing is not an alert — it would blink for as long as music runs. `Status::Passive` is
//!    worse still: it asks panels to *hide* the item, and with the window hidden the tray is the
//!    only way back into the app.
//! 3. **The devices are always a submenu, grouped by direction.** Windows inlines them behind a
//!    section header when there are five or fewer (`:339-348`); DBusMenu has no section header,
//!    and the special case only existed because a Win32 menu is cheap to build (§5.8). The Linux
//!    port also lists capture devices (`docs/spec/12-audio-io.md` §28), so the submenu carries two
//!    radio groups under two disabled header rows — "Output" and "Input" — the nearest thing
//!    DBusMenu has to a section header.
//!
//! # Threading
//!
//! `ksni` runs the item on its own D-Bus task and every menu callback fires there, so a callback
//! must never block — `showContextMenu`'s `settings_dialog.runModalLoop()` (`:261-265`) has no
//! equivalent and must not grow one. Callbacks here only *send* a [`TrayCommand`]; the tray holds
//! a mirror of the model and the application pushes changes back through [`TrayHandle::update`],
//! which is exactly where the C++ calls `setStatus` (`FxController.cpp:785`, `:1016`, `:2075`,
//! `:2773`).

use crossbeam_channel::Sender;
use fxsound_core::ThemeMode;
use fxsound_core::i18n::{tr, tr_args};
use ksni::blocking::{Handle, TrayMethods as _};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Category, Icon, MenuItem, Status, ToolTip, Tray};

/// The D-Bus / desktop identity of the application (`docs/spec/07-startup-tray.md` §1.5). It has
/// to match the `.desktop` file's basename and the Wayland `app_id` for shells to associate the
/// tray item with the window.
pub const APP_ID: &str = "com.fxsound.FxSound";

/// Tray icon for "audio processing is off" — the Windows `IDI_LOGO_GRAY`, `#6f6f6f`.
pub const ICON_OFF: &str = "com.fxsound.FxSound-off";
/// Tray icon for "on but idle" — the Windows `IDI_LOGO_WHITE`.
pub const ICON_ON: &str = "com.fxsound.FxSound-on";
/// Tray icon for "on and audio is flowing" — the Windows `IDI_LOGO_BLUE`/`IDI_LOGO_RED`, `#23b6eb`.
pub const ICON_PROCESSING: &str = "com.fxsound.FxSound-processing";

/// Device names longer than this are elided, from `getTruncatedText(name, 30)`
/// (`FxSystemTrayView.cpp:353`, implementation at `:422-432`).
pub const MENU_LABEL_MAX: usize = 30;

/// Which of the three icons the item is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrayIcon {
    /// Power off.
    Off,
    /// Power on, no audio flowing.
    On,
    /// Power on and audio flowing — the 5-sample / 500 ms hysteresis in
    /// `FxController::timerCallback` (`:2061-2088`) decides this, not the tray.
    Processing,
}

impl TrayIcon {
    /// The state machine of `FxSystemTrayView::setStatus` (`:90-111`), with the theme split
    /// collapsed as described in the module docs.
    #[must_use]
    pub const fn of(power: bool, processing: bool) -> Self {
        match (power, processing) {
            (false, _) => Self::Off,
            (true, false) => Self::On,
            (true, true) => Self::Processing,
        }
    }

    /// The freedesktop icon name, to be installed under `hicolor/*/status/`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => ICON_OFF,
            Self::On => ICON_ON,
            Self::Processing => ICON_PROCESSING,
        }
    }
}

/// Pre-rendered ARGB32 icons, for panels that do not look names up in the icon theme.
///
/// Optional: leave it empty and the panel resolves [`TrayIcon::name`] itself, which is what a
/// packaged install wants. `ksni::Icon` is **ARGB32 in network byte order**, not RGBA
/// (`docs/api/linux-desktop-crates.md` §2.4), and several sizes may be supplied at once.
#[derive(Debug, Clone, Default)]
pub struct TrayPixmaps {
    pub off: Vec<Icon>,
    pub on: Vec<Icon>,
    pub processing: Vec<Icon>,
}

impl TrayPixmaps {
    #[must_use]
    pub fn for_icon(&self, icon: TrayIcon) -> &[Icon] {
        match icon {
            TrayIcon::Off => &self.off,
            TrayIcon::On => &self.on,
            TrayIcon::Processing => &self.processing,
        }
    }
}

/// One row of the preset submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayPreset {
    pub name: String,
    /// `true` for a preset that ships with the app (`FxModel::PresetType::AppPreset`); the menu
    /// puts a separator where this changes (`FxSystemTrayView.cpp:238-242`).
    pub factory: bool,
    /// Unsaved changes; shown as a trailing ` *` (`:231`).
    pub modified: bool,
}

/// One row of the device submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayDevice {
    /// The friendly name, untruncated; [`MENU_LABEL_MAX`] is applied when the menu is built.
    pub name: String,
    /// `SoundDevice::deviceNumChannel`. Fewer than two and the row is shown but disabled
    /// (`FxSystemTrayView.cpp:356-359`).
    pub channels: u16,
    /// Playback or capture device; the submenu groups the rows by this.
    pub direction: fxsound_core::DeviceDirection,
}

/// The tray's mirror of the model.
///
/// Everything the menu and the tooltip need, and nothing else. The application owns the real
/// state; this copy is refreshed with [`TrayHandle::update`].
#[derive(Debug, Clone)]
pub struct TrayState {
    /// `FxModel::getPowerState()`.
    pub power: bool,
    /// `FxController::audio_process_on_`.
    pub processing: bool,
    /// Whether the Turn On/Off item is usable. Windows disables it in a remote session
    /// (`FxSystemTrayView.cpp:303`); `docs/spec/05-controller-model.md` §18.6 recommends dropping
    /// that concept on Linux, so this defaults to `true` and is kept only as the hook.
    pub power_enabled: bool,
    pub theme: ThemeMode,
    /// Ticked state of the Always On Top item. Windows reads it off the window rather than the
    /// controller (`:320`).
    pub always_on_top: bool,
    pub presets: Vec<TrayPreset>,
    pub selected_preset: Option<usize>,
    pub devices: Vec<TrayDevice>,
    pub selected_device: Option<usize>,
    /// The UI language the labels were built in, so a switch re-sends the menu.
    pub language: String,
    pub pixmaps: TrayPixmaps,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            // `Settings.cpp:31` ships `power = 1`, so the tray starts the way a fresh install does.
            power: true,
            processing: false,
            power_enabled: true,
            theme: ThemeMode::default(),
            always_on_top: false,
            presets: Vec::new(),
            selected_preset: None,
            devices: Vec::new(),
            selected_device: None,
            language: String::new(),
            pixmaps: TrayPixmaps::default(),
        }
    }
}

impl TrayState {
    /// The selected device, if the index still points at one.
    #[must_use]
    pub fn selected(&self) -> Option<&TrayDevice> {
        self.selected_device.and_then(|i| self.devices.get(i))
    }

    /// The selected device's friendly name, for the tooltip's second line.
    #[must_use]
    pub fn output_name(&self) -> &str {
        self.selected().map_or("", |device| device.name.as_str())
    }

    /// The direction the selected device sits in — the word the tooltip's second line starts
    /// with. With nothing selected it is `Output`, the only direction the Windows build had.
    #[must_use]
    pub fn selected_direction(&self) -> fxsound_core::DeviceDirection {
        self.selected()
            .map_or_else(Default::default, |device| device.direction)
    }

    /// Which icon the item should be showing.
    #[must_use]
    pub const fn icon(&self) -> TrayIcon {
        TrayIcon::of(self.power, self.processing)
    }

    /// The tooltip's second line: `"Output: <device>"`, or `"Input: <device>"` when FxSound sits
    /// behind a microphone (`docs/spec/12-audio-io.md` §28).
    #[must_use]
    pub fn device_line(&self) -> String {
        // `"Output: "` is the original's key, trailing space and all; `"Input: "` is this port's.
        let key = match self.selected_direction() {
            fxsound_core::DeviceDirection::Output => "Output: ",
            fxsound_core::DeviceDirection::Input => "Input: ",
        };
        format!("{}{}", tr(key), self.output_name())
    }

    /// The tooltip, byte for byte as `setStatus` composes it (`FxSystemTrayView.cpp:78-84`):
    /// `"FxSound is on."`, a blank line, then `"Output: "` and the device name — with `"Input: "`
    /// taking the place of `"Output: "` in the input direction, which Windows never had.
    ///
    /// Windows builds this with `swprintf_s` over a *translated* format string, which makes a bad
    /// translation a memory-safety bug (`:79`); Rust's formatting cannot be induced to do that.
    #[must_use]
    pub fn tooltip(&self) -> String {
        format!("{}\n\n{}", self.status_line(), self.device_line())
    }

    /// `TRANS("FxSound is %s.")` with `on`/`off` (`FxSystemTrayView.cpp:76-79`).
    #[must_use]
    pub fn status_line(&self) -> String {
        tr_args(
            "FxSound is %s.",
            &[&tr(if self.power { "on" } else { "off" })],
        )
    }
}

/// What the user asked for by clicking the tray.
///
/// One variant per binding in `FxSystemTrayView.cpp:244-287` and `:365-368`, plus
/// [`TrayCommand::ToggleWindow`] for the left click (`:446-455`), which is a *different* action
/// from the menu's Open (`:253-255`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    /// Left click, or Enter on the item: show the window if it is hidden, hide it if it is showing.
    ToggleWindow,
    /// Menu ▸ Open — always shows, never hides.
    Open,
    /// Menu ▸ Turn On / Turn Off, carrying the requested state (`setPowerState(!power)`, `:257-259`).
    SetPower(bool),
    /// Index into [`TrayState::presets`] (`setPreset(id - 101)`, `:244-247`).
    SelectPreset(usize),
    /// Index into [`TrayState::devices`] (`setOutput(id - 201)`, `:365-368`).
    SelectDevice(usize),
    /// Menu ▸ Settings. The Windows handler runs a modal loop here (`:261-265`); this one must
    /// return at once and let the GUI thread open the pane.
    OpenSettings,
    /// Menu ▸ Theme ▸ Dark | Light (`:267-273`).
    SetTheme(ThemeMode),
    /// Menu ▸ Always On Top, carrying the requested state (`:275-278`).
    SetAlwaysOnTop(bool),
    /// Menu ▸ Exit. The only way to quit from the UI on Windows (`:285-287`).
    Exit,
}

/// The StatusNotifierItem itself.
///
/// Public because [`TrayHandle`] names it, and because the menu is worth testing without a D-Bus
/// session: build one and call [`Tray::menu`].
pub struct FxTray {
    state: TrayState,
    tx: Sender<TrayCommand>,
}

impl FxTray {
    #[must_use]
    pub fn new(state: TrayState, tx: Sender<TrayCommand>) -> Self {
        Self { state, tx }
    }

    /// The mirror the menu is built from.
    #[must_use]
    pub fn state(&self) -> &TrayState {
        &self.state
    }

    /// Callbacks only ever do this. A full channel means the application is not draining it, which
    /// is a bug in the application and not something the D-Bus task can wait out.
    fn send(&self, command: TrayCommand) {
        if self.tx.try_send(command).is_err() {
            log::warn!("dropped a tray command: the application is not reading them");
        }
    }

    /// `Preset Select ▸`, present only while power is on (`FxSystemTrayView.cpp:313-316`).
    fn preset_menu(&self) -> Option<MenuItem<Self>> {
        if !self.power_on_with_presets() {
            return None;
        }
        let mut submenu = Vec::new();
        if let Some(group) = self.preset_group(true) {
            submenu.push(group);
        }
        // The separator Windows inserts where `preset.type` changes (`:238-242`). It has to sit
        // *between* two radio groups because DBusMenu radio state is per group.
        if submenu.len() == 1 && self.presets_of(false).next().is_some() {
            submenu.push(MenuItem::Separator);
        }
        if let Some(group) = self.preset_group(false) {
            submenu.push(group);
        }
        Some(
            SubMenu {
                label: tr("Preset Select"),
                submenu,
                ..Default::default()
            }
            .into(),
        )
    }

    fn power_on_with_presets(&self) -> bool {
        self.state.power && !self.state.presets.is_empty()
    }

    fn presets_of(&self, factory: bool) -> impl Iterator<Item = (usize, &TrayPreset)> {
        self.state
            .presets
            .iter()
            .enumerate()
            .filter(move |(_, preset)| preset.factory == factory)
    }

    fn preset_group(&self, factory: bool) -> Option<MenuItem<Self>> {
        let indices: Vec<usize> = self.presets_of(factory).map(|(i, _)| i).collect();
        if indices.is_empty() {
            return None;
        }
        let options = indices
            .iter()
            .map(|&i| RadioItem {
                label: preset_label(&self.state.presets[i]),
                ..Default::default()
            })
            .collect();
        // `usize::MAX` when the selection lives in the *other* group: ksni ticks the option whose
        // index equals `selected` (`ksni-0.3.6/src/menu.rs:947-948`), so an unreachable index
        // leaves the whole group unticked, which is what a split radio group needs.
        let selected = self
            .state
            .selected_preset
            .and_then(|selected| indices.iter().position(|&i| i == selected))
            .unwrap_or(usize::MAX);
        let global = indices;
        Some(
            RadioGroup {
                selected,
                select: Box::new(move |tray: &mut Self, index| {
                    if let Some(&preset) = global.get(index) {
                        tray.send(TrayCommand::SelectPreset(preset));
                    }
                }),
                options,
            }
            .into(),
        )
    }

    /// `Playback Device Select ▸` (`FxSystemTrayView.cpp:339-381`), always a submenu here.
    ///
    /// The rows are grouped by direction: a disabled `"Output"` header, the output radio group, a
    /// separator, a disabled `"Input"` header, the input radio group — each half present only when
    /// it has devices. Radio state is per group in DBusMenu, so the split works the same way the
    /// preset submenu's built-in/user split does: the group that does not hold the selection is
    /// given an unreachable index and stays unticked.
    fn device_menu(&self) -> Option<MenuItem<Self>> {
        if self.state.devices.is_empty() {
            return None;
        }
        let mut submenu: Vec<MenuItem<Self>> = Vec::new();
        for direction in [
            fxsound_core::DeviceDirection::Output,
            fxsound_core::DeviceDirection::Input,
        ] {
            let Some(group) = self.device_group(direction) else {
                continue;
            };
            if !submenu.is_empty() {
                submenu.push(MenuItem::Separator);
            }
            submenu.push(
                StandardItem {
                    label: tr(direction.label()),
                    // A header, not a command: DBusMenu has no section header, and a disabled
                    // row is how every SNI host draws one.
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            submenu.push(group);
        }
        Some(
            SubMenu {
                label: tr("Playback Device Select"),
                submenu,
                ..Default::default()
            }
            .into(),
        )
    }

    fn devices_of(
        &self,
        direction: fxsound_core::DeviceDirection,
    ) -> impl Iterator<Item = (usize, &TrayDevice)> {
        self.state
            .devices
            .iter()
            .enumerate()
            .filter(move |(_, device)| device.direction == direction)
    }

    /// The radio group for one direction, or `None` when it has no devices.
    fn device_group(&self, direction: fxsound_core::DeviceDirection) -> Option<MenuItem<Self>> {
        let indices: Vec<usize> = self.devices_of(direction).map(|(i, _)| i).collect();
        if indices.is_empty() {
            return None;
        }
        let options = indices
            .iter()
            .map(|&i| RadioItem {
                label: truncate_label(&self.state.devices[i].name),
                // Fewer than two channels: shown, but not selectable (`:356-359`).
                enabled: self.state.devices[i].channels >= 2,
                ..Default::default()
            })
            .collect();
        // `usize::MAX` when the selection lives in the *other* group — see `preset_group`.
        let selected = self
            .state
            .selected_device
            .and_then(|selected| indices.iter().position(|&i| i == selected))
            .unwrap_or(usize::MAX);
        let global = indices;
        Some(
            RadioGroup {
                selected,
                select: Box::new(move |tray: &mut Self, index| {
                    if let Some(&device) = global.get(index) {
                        tray.send(TrayCommand::SelectDevice(device));
                    }
                }),
                options,
            }
            .into(),
        )
    }

    /// `Theme ▸ Dark | Light` (`FxSystemTrayView.cpp:289-290`, `:319`).
    fn theme_menu(&self) -> MenuItem<Self> {
        // `FxThemeMode { Dark = 0, Light }` (`FxTheme.h:28`); keep that order so the radio index
        // and the enum agree.
        let modes = [ThemeMode::Dark, ThemeMode::Light];
        let selected = modes
            .iter()
            .position(|&mode| mode == self.state.theme)
            .unwrap_or(0);
        SubMenu {
            label: tr("Theme"),
            submenu: vec![
                RadioGroup {
                    selected,
                    select: Box::new(move |tray: &mut Self, index| {
                        if let Some(&mode) = modes.get(index) {
                            tray.send(TrayCommand::SetTheme(mode));
                        }
                    }),
                    options: vec![
                        RadioItem {
                            label: tr("Dark"),
                            ..Default::default()
                        },
                        RadioItem {
                            label: tr("Light"),
                            ..Default::default()
                        },
                    ],
                }
                .into(),
            ],
            ..Default::default()
        }
        .into()
    }
}

impl Tray for FxTray {
    /// A left click activates rather than popping the menu, matching `NIN_SELECT`
    /// (`FxSystemTrayView.cpp:446-455`).
    const MENU_ON_ACTIVATE: bool = false;

    fn id(&self) -> String {
        APP_ID.to_owned()
    }

    fn title(&self) -> String {
        "FxSound".to_owned()
    }

    /// The item represents the application, not a piece of hardware, which is what
    /// `ApplicationStatus` means in the SNI spec (`ksni-0.3.6/src/tray.rs:24-41`).
    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    /// Always visible — see departure 2 in the module docs.
    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_name(&self) -> String {
        self.state.icon().name().to_owned()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.state.pixmaps.for_icon(self.state.icon()).to_vec()
    }

    /// Many panels render only the tooltip's `title`, so the line that changes goes there and the
    /// device name follows in the description (`docs/spec/07-startup-tray.md` §5.8).
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
            title: self.state.status_line(),
            description: self.state.device_line(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::ToggleWindow);
    }

    /// Windows binds neither a middle click nor a double click (`FxSystemTrayView.cpp:434-478`),
    /// and inventing one here would be a behaviour no user of the original expects.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {}

    fn scroll(&mut self, _delta: i32, _orientation: ksni::Orientation) {}

    /// The tree of `showContextMenu` (`FxSystemTrayView.cpp:216-330`), in its order.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: tr("Open"),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Open)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                // `power ? "Turn Off" : "Turn On"` (`:300-303`).
                label: if self.state.power {
                    tr("Turn Off")
                } else {
                    tr("Turn On")
                },
                enabled: self.state.power_enabled,
                activate: Box::new(|tray: &mut Self| {
                    tray.send(TrayCommand::SetPower(!tray.state.power));
                }),
                ..Default::default()
            }
            .into(),
        ];

        if let Some(presets) = self.preset_menu() {
            menu.push(presets);
        }
        if let Some(devices) = self.device_menu() {
            // The separator that wraps the device section on Windows (`:344-347`, `:380`).
            menu.push(MenuItem::Separator);
            menu.push(devices);
            menu.push(MenuItem::Separator);
        }

        menu.push(
            StandardItem {
                label: tr("Settings"),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::OpenSettings)),
                ..Default::default()
            }
            .into(),
        );
        menu.push(self.theme_menu());
        menu.push(
            CheckmarkItem {
                label: tr("Always On Top"),
                checked: self.state.always_on_top,
                // ksni does not flip `checked` for us (`ksni-0.3.6/src/menu.rs:299-306`), and we
                // deliberately do not flip it here either: the application answers with
                // `TrayHandle::update` so the tick can never disagree with the window.
                activate: Box::new(|tray: &mut Self| {
                    tray.send(TrayCommand::SetAlwaysOnTop(!tray.state.always_on_top));
                }),
                ..Default::default()
            }
            .into(),
        );
        menu.push(
            StandardItem {
                label: tr("Exit"),
                icon_name: "application-exit".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Exit)),
                ..Default::default()
            }
            .into(),
        );
        menu
    }

    /// The `StatusNotifierWatcher` went away — a panel restarting, or a session that has not
    /// started one yet. Returning `true` keeps the service alive and waiting, which is the
    /// analogue of re-adding the icon on the `"TaskbarCreated"` broadcast
    /// (`FxSystemTrayView.cpp:42`, `:438-441`). Returning `false` would shut it down for good.
    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::warn!("no StatusNotifierWatcher: {reason:?}; waiting for one to appear");
        true
    }
}

/// The GUI thread's end of the tray.
///
/// [`Handle::update`] here is the *blocking* one (`ksni-0.3.6/src/blocking.rs:205`), so the egui
/// thread can call it directly without an async runtime of its own.
pub struct TrayHandle {
    handle: Handle<FxTray>,
}

impl TrayHandle {
    /// Push a change into the tray and let ksni emit the D-Bus property signals.
    ///
    /// Returns `false` when the service is gone — mutating a `TrayState` anywhere else does
    /// nothing at all, because this call is what makes a change visible.
    pub fn update(&self, f: impl FnOnce(&mut TrayState)) -> bool {
        self.handle
            .update(|tray: &mut FxTray| f(&mut tray.state))
            .is_some()
    }

    /// `FxSystemTrayView::setStatus(power, processing)` (`FxSystemTrayView.cpp:66-121`): the icon
    /// and the tooltip in one call, which is how every caller in the C++ uses it.
    pub fn set_status(&self, power: bool, processing: bool) -> bool {
        self.update(|state| {
            state.power = power;
            state.processing = processing;
        })
    }

    /// `true` once the service has shut down.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Remove the item, the analogue of `Shell_NotifyIcon(NIM_DELETE)` (`:56-59`). Returns once
    /// the service is really gone, so it is safe to call on the way out of `main`.
    pub fn shutdown(&self) {
        self.handle.shutdown().wait();
    }
}

/// Register the tray item and start serving it.
///
/// `assume_sni_available(true)` turns "no watcher yet" into a [`Tray::watcher_offline`] callback
/// instead of a hard error (`ksni-0.3.6/src/lib.rs:480-493`), because a session-start app routinely
/// beats its panel to the bus. The cost is that a desktop with *no* SNI host at all — GNOME
/// without the AppIndicator extension — never reports an error, so the caller should also warn the
/// user when nothing has registered after a few seconds; with the window hidden and no tray the
/// app is otherwise invisible (`docs/spec/07-startup-tray.md` §5.8).
///
/// # Errors
///
/// If the session bus is unreachable or the item cannot be registered.
pub fn spawn(state: TrayState, tx: Sender<TrayCommand>) -> Result<TrayHandle, ksni::Error> {
    let handle = FxTray::new(state, tx).assume_sni_available(true).spawn()?;
    Ok(TrayHandle { handle })
}

/// `"<name> *"` for a preset with unsaved changes (`FxSystemTrayView.cpp:231`).
fn preset_label(preset: &TrayPreset) -> String {
    if preset.modified {
        format!("{} *", preset.name)
    } else {
        preset.name.clone()
    }
}

/// `getTruncatedText(text, 30)` (`FxSystemTrayView.cpp:422-432`): a name longer than the limit
/// loses `(len - 30) + 3` characters and gains `"..."`, so the result is exactly 30 characters.
fn truncate_label(text: &str) -> String {
    if text.chars().count() <= MENU_LABEL_MAX {
        return text.to_owned();
    }
    let mut label: String = text.chars().take(MENU_LABEL_MAX - 3).collect();
    label.push_str("...");
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{Receiver, unbounded};

    fn preset(name: &str, factory: bool, modified: bool) -> TrayPreset {
        TrayPreset {
            name: name.to_owned(),
            factory,
            modified,
        }
    }

    fn device(name: &str, channels: u16) -> TrayDevice {
        TrayDevice {
            name: name.to_owned(),
            channels,
            direction: fxsound_core::DeviceDirection::Output,
        }
    }

    fn input(name: &str, channels: u16) -> TrayDevice {
        TrayDevice {
            name: name.to_owned(),
            channels,
            direction: fxsound_core::DeviceDirection::Input,
        }
    }

    /// `populated()`, plus the two capture devices of the development machine — whose USB
    /// microphone exposes a sink *and* a source under the same description.
    fn with_inputs() -> TrayState {
        let mut state = populated();
        state
            .devices
            .push(input("fifine Microphone Analogue Stereo", 2));
        state
            .devices
            .push(input("Ryzen HD Audio Controller Analogue Stereo", 1));
        state
    }

    fn with_state(state: TrayState) -> (FxTray, Receiver<TrayCommand>) {
        let (tx, rx) = unbounded();
        (FxTray::new(state, tx), rx)
    }

    fn populated() -> TrayState {
        TrayState {
            presets: vec![
                preset("General", true, false),
                preset("Bass Booster", true, false),
                preset("My Mix", false, true),
            ],
            selected_preset: Some(2),
            devices: vec![
                device("Built-in Audio Analogue Stereo", 2),
                device("HDMI / DisplayPort 3 Output That Goes On Forever", 2),
                device("Mono Headset", 1),
            ],
            selected_device: Some(0),
            ..TrayState::default()
        }
    }

    fn label(item: &MenuItem<FxTray>) -> String {
        match item {
            MenuItem::Standard(item) => item.label.clone(),
            MenuItem::Checkmark(item) => item.label.clone(),
            MenuItem::SubMenu(item) => item.label.clone(),
            MenuItem::Separator => "---".to_owned(),
            MenuItem::RadioGroup(_) => "<radio>".to_owned(),
        }
    }

    fn labels(menu: &[MenuItem<FxTray>]) -> Vec<String> {
        menu.iter().map(label).collect()
    }

    fn submenu_of<'a>(menu: &'a [MenuItem<FxTray>], name: &str) -> &'a [MenuItem<FxTray>] {
        menu.iter()
            .find_map(|item| match item {
                MenuItem::SubMenu(sub) if sub.label == name => Some(sub.submenu.as_slice()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no `{name}` submenu in {:?}", labels(menu)))
    }

    fn radio_groups(menu: &[MenuItem<FxTray>]) -> Vec<&RadioGroup<FxTray>> {
        menu.iter()
            .filter_map(|item| match item {
                MenuItem::RadioGroup(group) => Some(group),
                _ => None,
            })
            .collect()
    }

    fn activate(menu: &[MenuItem<FxTray>], name: &str, tray: &mut FxTray) {
        let item = menu
            .iter()
            .find(|item| label(item) == name)
            .unwrap_or_else(|| panic!("no `{name}` item in {:?}", labels(menu)));
        match item {
            MenuItem::Standard(item) => (item.activate)(tray),
            MenuItem::Checkmark(item) => (item.activate)(tray),
            other => panic!("`{}` is not clickable", label(other)),
        }
    }

    #[test]
    fn the_menu_carries_the_windows_items_in_the_windows_order() {
        let (tray, _rx) = with_state(populated());
        assert_eq!(
            labels(&tray.menu()),
            vec![
                "Open",
                "Turn Off",
                "Preset Select",
                "---",
                "Playback Device Select",
                "---",
                "Settings",
                "Theme",
                "Always On Top",
                "Exit",
            ]
        );
    }

    #[test]
    fn the_preset_submenu_is_only_there_while_power_is_on() {
        // `FxSystemTrayView.cpp:313-316`: with power off the menu is Open / Turn On / devices /
        // Settings / Theme / Always On Top / Donate / Exit.
        let (tray, _rx) = with_state(TrayState {
            power: false,
            ..populated()
        });
        let menu = tray.menu();
        let labels = labels(&menu);
        assert!(!labels.contains(&tr("Preset Select")), "{labels:?}");
        assert_eq!(labels[1], "Turn On", "the item says what it will do");
        assert!(labels.contains(&tr("Playback Device Select")));
    }

    #[test]
    fn the_power_item_can_be_disabled_the_way_a_remote_session_disables_it() {
        let (tray, _rx) = with_state(TrayState {
            power_enabled: false,
            ..populated()
        });
        let menu = tray.menu();
        let MenuItem::Standard(power) = &menu[1] else {
            panic!("the second item is Turn On/Off");
        };
        assert!(!power.enabled);
    }

    #[test]
    fn built_in_and_user_presets_are_separated_and_only_the_selection_is_ticked() {
        let (tray, _rx) = with_state(populated());
        let menu = tray.menu();
        let presets = submenu_of(&menu, "Preset Select");

        assert_eq!(
            labels(presets),
            vec!["<radio>", "---", "<radio>"],
            "a separator sits where the preset type changes"
        );

        let groups = radio_groups(presets);
        let built_in = &groups[0];
        let user = &groups[1];
        assert_eq!(
            built_in
                .options
                .iter()
                .map(|o| o.label.clone())
                .collect::<Vec<_>>(),
            vec!["General", "Bass Booster"]
        );
        assert_eq!(
            user.options
                .iter()
                .map(|o| o.label.clone())
                .collect::<Vec<_>>(),
            vec!["My Mix *"],
            "an unsaved preset gets a trailing star"
        );
        assert_eq!(user.selected, 0, "the selection is in the user group");
        assert!(
            built_in.selected >= built_in.options.len(),
            "no built-in preset may be ticked at the same time"
        );
    }

    #[test]
    fn picking_a_preset_reports_its_index_in_the_whole_list_not_in_its_group() {
        let (mut tray, rx) = with_state(populated());
        let menu = tray.menu();
        let presets = submenu_of(&menu, "Preset Select");
        let groups = radio_groups(presets);
        let select_user = &groups[1].select;

        select_user(&mut tray, 0);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::SelectPreset(2)));
    }

    #[test]
    fn device_labels_are_truncated_and_mono_devices_are_shown_but_disabled() {
        let (tray, _rx) = with_state(populated());
        let menu = tray.menu();
        let devices = submenu_of(&menu, "Playback Device Select");
        assert_eq!(
            labels(devices),
            vec!["Output", "<radio>"],
            "outputs only: one header, one group, no separator and no Input header"
        );
        let MenuItem::Standard(header) = &devices[0] else {
            panic!("the header is a StandardItem");
        };
        assert!(!header.enabled, "the header is not clickable");
        let group = radio_groups(devices)[0];

        assert_eq!(group.options[0].label, "Built-in Audio Analogue Stereo");
        assert_eq!(
            group.options[1].label, "HDMI / DisplayPort 3 Output...",
            "exactly 30 characters, per getTruncatedText"
        );
        assert_eq!(group.options[1].label.chars().count(), MENU_LABEL_MAX);
        assert!(group.options[2].visible, "a mono device is still listed");
        assert!(!group.options[2].enabled, "but it cannot be selected");
        assert_eq!(group.selected, 0);
    }

    #[test]
    fn the_device_submenu_groups_outputs_then_inputs_under_headers() {
        let (tray, _rx) = with_state(with_inputs());
        let menu = tray.menu();
        let devices = submenu_of(&menu, "Playback Device Select");
        assert_eq!(
            labels(devices),
            vec!["Output", "<radio>", "---", "Input", "<radio>"],
            "header, group, separator, header, group"
        );
        for header in [&devices[0], &devices[3]] {
            let MenuItem::Standard(header) = header else {
                panic!("headers are StandardItems");
            };
            assert!(!header.enabled);
        }

        let groups = radio_groups(devices);
        let outputs = groups[0];
        let inputs = groups[1];
        assert_eq!(
            outputs
                .options
                .iter()
                .map(|o| o.label.clone())
                .collect::<Vec<_>>(),
            vec![
                "Built-in Audio Analogue Stereo",
                "HDMI / DisplayPort 3 Output...",
                "Mono Headset",
            ]
        );
        assert_eq!(
            inputs
                .options
                .iter()
                .map(|o| o.label.clone())
                .collect::<Vec<_>>(),
            vec![
                "fifine Microphone Analogue ...",
                "Ryzen HD Audio Controller A...",
            ],
            "truncated to 30 characters like every other row"
        );
        assert_eq!(outputs.selected, 0, "the selection is in the output group");
        assert!(
            inputs.selected >= inputs.options.len(),
            "no input may be ticked at the same time"
        );
        assert!(
            !inputs.options[1].enabled,
            "a mono microphone is greyed out in the tray the way a mono sink is (`:356-359`)"
        );

        // With a microphone selected the tick moves to the input group and leaves the outputs.
        let (tray, _rx) = with_state(TrayState {
            selected_device: Some(3),
            ..with_inputs()
        });
        let menu = tray.menu();
        let groups = radio_groups(submenu_of(&menu, "Playback Device Select"));
        assert!(groups[0].selected >= groups[0].options.len());
        assert_eq!(groups[1].selected, 0);
    }

    #[test]
    fn inputs_only_makes_a_submenu_with_just_the_input_half() {
        let (tray, _rx) = with_state(TrayState {
            devices: vec![input("Microphone", 2)],
            selected_device: Some(0),
            ..populated()
        });
        let menu = tray.menu();
        let devices = submenu_of(&menu, "Playback Device Select");
        assert_eq!(labels(devices), vec!["Input", "<radio>"]);
        assert_eq!(radio_groups(devices)[0].selected, 0);
    }

    #[test]
    fn picking_an_input_reports_its_index_in_the_whole_list_not_in_its_group() {
        let (mut tray, rx) = with_state(with_inputs());
        let menu = tray.menu();
        let groups = radio_groups(submenu_of(&menu, "Playback Device Select"));
        (groups[1].select)(&mut tray, 1);
        assert_eq!(
            rx.try_recv(),
            Ok(TrayCommand::SelectDevice(4)),
            "the second input is the fifth device overall"
        );
        (groups[0].select)(&mut tray, 2);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::SelectDevice(2)));
        (groups[1].select)(&mut tray, 7);
        assert!(
            rx.try_recv().is_err(),
            "an index past the group sends nothing"
        );
    }

    #[test]
    fn the_device_section_disappears_when_there_are_no_devices() {
        let (tray, _rx) = with_state(TrayState {
            devices: Vec::new(),
            selected_device: None,
            ..populated()
        });
        let labels = labels(&tray.menu());
        assert!(!labels.contains(&tr("Playback Device Select")));
        assert!(
            !labels.contains(&"---".to_owned()),
            "and so do its separators"
        );
    }

    #[test]
    fn the_theme_submenu_ticks_the_current_mode_in_the_enum_order() {
        // `FxThemeMode { Dark = 0, Light }` (`FxTheme.h:28`).
        let (tray, _rx) = with_state(populated());
        let menu = tray.menu();
        let group = radio_groups(submenu_of(&menu, "Theme"))[0];
        assert_eq!(
            group
                .options
                .iter()
                .map(|o| o.label.clone())
                .collect::<Vec<_>>(),
            vec!["Dark", "Light"]
        );
        assert_eq!(group.selected, 0);

        let (light, _rx) = with_state(TrayState {
            theme: ThemeMode::Light,
            ..populated()
        });
        let menu = light.menu();
        assert_eq!(radio_groups(submenu_of(&menu, "Theme"))[0].selected, 1);
    }

    #[test]
    fn every_clickable_item_reports_a_command_and_changes_nothing_itself() {
        let (mut tray, rx) = with_state(populated());
        let menu = tray.menu();

        activate(&menu, "Open", &mut tray);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::Open));

        activate(&menu, "Turn Off", &mut tray);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::SetPower(false)));
        assert!(
            tray.state().power,
            "the tray mirrors the application; it does not decide"
        );

        activate(&menu, "Settings", &mut tray);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::OpenSettings));

        activate(&menu, "Always On Top", &mut tray);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::SetAlwaysOnTop(true)));
        assert!(!tray.state().always_on_top);

        activate(&menu, "Exit", &mut tray);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::Exit));

        let theme = submenu_of(&menu, "Theme");
        (radio_groups(theme)[0].select)(&mut tray, 1);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::SetTheme(ThemeMode::Light)));

        let devices = submenu_of(&menu, "Playback Device Select");
        (radio_groups(devices)[0].select)(&mut tray, 1);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::SelectDevice(1)));
    }

    #[test]
    fn a_left_click_toggles_the_window_rather_than_opening_it() {
        // `NIN_SELECT` at `FxSystemTrayView.cpp:446-455` hides a visible window.
        let (mut tray, rx) = with_state(populated());
        tray.activate(0, 0);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::ToggleWindow));

        tray.secondary_activate(0, 0);
        assert!(
            rx.try_recv().is_err(),
            "Windows binds no middle click (`:434-478`)"
        );
    }

    #[test]
    fn the_icon_follows_the_windows_state_machine() {
        assert_eq!(TrayIcon::of(false, false), TrayIcon::Off);
        assert_eq!(TrayIcon::of(false, true), TrayIcon::Off);
        assert_eq!(TrayIcon::of(true, false), TrayIcon::On);
        assert_eq!(TrayIcon::of(true, true), TrayIcon::Processing);
        assert_eq!(TrayIcon::Off.name(), "com.fxsound.FxSound-off");
        assert_eq!(TrayIcon::On.name(), "com.fxsound.FxSound-on");
        assert_eq!(
            TrayIcon::Processing.name(),
            "com.fxsound.FxSound-processing"
        );

        let (tray, _rx) = with_state(TrayState {
            processing: true,
            ..populated()
        });
        assert_eq!(tray.icon_name(), ICON_PROCESSING);
        assert_eq!(tray.id(), APP_ID);
        assert_eq!(tray.title(), "FxSound");
    }

    #[test]
    fn the_tooltip_reads_exactly_like_the_windows_one() {
        // `FxSystemTrayView.cpp:78-84` — status line, blank line, "Output: " and the device.
        let state = populated();
        assert_eq!(
            state.tooltip(),
            "FxSound is on.\n\nOutput: Built-in Audio Analogue Stereo"
        );
        let off = TrayState {
            power: false,
            ..populated()
        };
        assert_eq!(
            off.tooltip(),
            "FxSound is off.\n\nOutput: Built-in Audio Analogue Stereo"
        );

        let (tray, _rx) = with_state(state);
        let tip = tray.tool_tip();
        assert_eq!(tip.title, "FxSound is on.");
        assert_eq!(tip.description, "Output: Built-in Audio Analogue Stereo");
    }

    #[test]
    fn a_tooltip_with_no_device_selected_still_reads_sensibly() {
        let state = TrayState {
            selected_device: None,
            ..populated()
        };
        assert_eq!(state.output_name(), "");
        assert_eq!(
            state.selected_direction(),
            fxsound_core::DeviceDirection::Output,
            "with nothing selected the tooltip keeps the Windows wording"
        );
        assert_eq!(state.tooltip(), "FxSound is on.\n\nOutput: ");
    }

    #[test]
    fn the_tooltip_says_input_when_fxsound_sits_behind_a_microphone() {
        let state = TrayState {
            selected_device: Some(3),
            ..with_inputs()
        };
        assert_eq!(
            state.selected_direction(),
            fxsound_core::DeviceDirection::Input
        );
        assert_eq!(
            state.tooltip(),
            "FxSound is on.\n\nInput: fifine Microphone Analogue Stereo"
        );
        let (tray, _rx) = with_state(state);
        let tip = tray.tool_tip();
        assert_eq!(tip.title, "FxSound is on.");
        assert_eq!(tip.description, "Input: fifine Microphone Analogue Stereo");
    }

    #[test]
    fn pixmaps_are_optional_and_follow_the_icon_state() {
        let (tray, _rx) = with_state(populated());
        assert!(
            tray.icon_pixmap().is_empty(),
            "with no pixmaps supplied the panel resolves the icon name itself"
        );

        let pixel = Icon {
            width: 1,
            height: 1,
            data: vec![0xff, 0x23, 0xb6, 0xeb],
        };
        let (with_pixmaps, _rx) = with_state(TrayState {
            processing: true,
            pixmaps: TrayPixmaps {
                processing: vec![pixel],
                ..TrayPixmaps::default()
            },
            ..populated()
        });
        assert_eq!(with_pixmaps.icon_pixmap().len(), 1);
        assert_eq!(with_pixmaps.icon_pixmap()[0].width, 1);
    }

    #[test]
    fn a_short_device_name_is_left_alone_and_a_long_one_is_elided() {
        assert_eq!(truncate_label("Speakers"), "Speakers");
        let exactly_thirty = "a".repeat(MENU_LABEL_MAX);
        assert_eq!(truncate_label(&exactly_thirty), exactly_thirty);
        let too_long = "a".repeat(MENU_LABEL_MAX + 1);
        let truncated = truncate_label(&too_long);
        assert_eq!(truncated.chars().count(), MENU_LABEL_MAX);
        assert!(truncated.ends_with("..."));
    }
}
