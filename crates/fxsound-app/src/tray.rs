//! The system tray, as a StatusNotifierItem.
//!
//! Port of `FxSystemTrayView` (`fxsound/Source/GUI/FxSystemTrayView.cpp`, 477 lines): the icon's
//! four states (`:90-111`), the tooltip (`:78-84`), the left-click toggle (`:446-455`) and the
//! whole context menu (`showContextMenu`, `:216-330`) — for two lanes (0.4.0 design §1.4): a
//! preset submenu per direction, a device group per direction with an `Off` row that detaches the
//! lane, and a tooltip with a line per lane.
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
//! # Deliberate departures
//!
//! 1. **Three icons, not four.** Windows picks between a red and a blue "processing" icon
//!    depending on its own theme (`FxSystemTrayView.cpp:90-111`). A panel owns its own background
//!    and recolours symbolic icons itself, so keying tray artwork off *our* theme is wrong here;
//!    the four states collapse onto `com.fxsound.FxSound-{off,on,processing}`
//!    (`docs/spec/07-startup-tray.md` §5.8): the five bars in grey, white and blue.
//! 2. **The status stays `Active`.** §5.8 suggests carrying "processing" as
//!    `Status::NeedsAttention`, but that is the state a panel flashes or highlights, and audio
//!    playing is not an alert — it would blink for as long as music runs. `Status::Passive` is
//!    worse still: it asks panels to *hide* the item, and with the window hidden the tray is the
//!    only way back into the app.
//! 3. **The devices are always a submenu, grouped by direction.** Windows inlines them behind a
//!    section header when there are five or fewer (`:339-348`); DBusMenu has no section header,
//!    and the special case only existed because a Win32 menu is cheap to build (§5.8). Each lane
//!    has a radio group under a disabled header row — "Output" and "Input", the nearest thing
//!    DBusMenu has to a section header — that starts with `Off`, as each of the window's two
//!    combos does. Every device is selectable: a mono output is a valid target (upstream review
//!    U7), so the original's greying of devices with fewer than two channels (`:356-359`) is gone.
//! 4. **Two preset submenus.** One per lane, each listing that lane's presets — the music presets
//!    and the voice presets — so the microphone's can be changed from the tray without the
//!    window, and without making the microphone the lane the window edits.
//! 5. **No Always On Top.** Windows ticks it off the window (`:275-278`, `:320`); winit's Wayland
//!    backend ignores window levels, so the item would tick and change nothing. The hamburger
//!    menu dropped it for the same reason.
//!
//! # The icon without an icon theme
//!
//! A package installs the three icons under `hicolor/scalable/status/`, and a panel looks them up
//! by [`TrayIcon::name`]. A binary run from a build directory, or a panel that reads only
//! `IconPixmap`, gets nothing that way, so the same SVGs are compiled in and rasterised once at
//! start-up ([`TrayPixmaps::rasterised`]); the item offers both, and the host uses what it can.
//!
//! # Threading
//!
//! `ksni` runs the item on its own D-Bus task and every menu callback fires there, so a callback
//! must never block — `showContextMenu`'s `settings_dialog.runModalLoop()` (`:261-265`) has no
//! equivalent and must not grow one. Callbacks here only *send* a [`TrayCommand`], through a
//! [`WakingSender`] that wakes the GUI thread for it (0.4.0 design §12); the tray holds a mirror
//! of the model and the application pushes changes back through [`TrayHandle::update`], which is
//! exactly where the C++ calls `setStatus` (`FxController.cpp:785`, `:1016`, `:2075`, `:2773`).

use crate::wake::WakingSender;
use fxsound_core::i18n::{tr, tr_args};
use fxsound_core::{DeviceDirection, ThemeMode};
use ksni::blocking::{Handle, TrayMethods as _};
use ksni::menu::{RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Category, Icon, MenuItem, Status, ToolTip, Tray};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// The three icons as the packages install them, compiled in for [`TrayPixmaps::rasterised`].
const SVG_OFF: &[u8] = include_bytes!("../../../assets/icons/status/com.fxsound.FxSound-off.svg");
const SVG_ON: &[u8] = include_bytes!("../../../assets/icons/status/com.fxsound.FxSound-on.svg");
const SVG_PROCESSING: &[u8] =
    include_bytes!("../../../assets/icons/status/com.fxsound.FxSound-processing.svg");

/// The pixel sizes each icon is rasterised at: the panel sizes in common use, 16 to 64 pixels,
/// so a host picks one that needs no scaling rather than blurring one that does.
pub const PIXMAP_SIZES: [u32; 6] = [16, 22, 24, 32, 48, 64];

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
    /// Every icon, in the order of [`TrayPixmaps`]' fields.
    pub const ALL: [Self; 3] = [Self::Off, Self::On, Self::Processing];

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

    /// The freedesktop icon name, installed under `hicolor/*/status/`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => ICON_OFF,
            Self::On => ICON_ON,
            Self::Processing => ICON_PROCESSING,
        }
    }

    /// The SVG a package installs under [`TrayIcon::name`], as compiled in.
    #[must_use]
    pub const fn svg(self) -> &'static [u8] {
        match self {
            Self::Off => SVG_OFF,
            Self::On => SVG_ON,
            Self::Processing => SVG_PROCESSING,
        }
    }
}

/// Pre-rendered ARGB32 icons, for panels that do not look names up in the icon theme.
///
/// `ksni::Icon` is **ARGB32 in network byte order**, not RGBA
/// (`docs/api/linux-desktop-crates.md` §2.4), with straight alpha, and several sizes may be
/// supplied at once. Empty, the panel resolves [`TrayIcon::name`] alone.
#[derive(Debug, Clone, Default)]
pub struct TrayPixmaps {
    pub off: Vec<Icon>,
    pub on: Vec<Icon>,
    pub processing: Vec<Icon>,
}

impl TrayPixmaps {
    /// The three compiled-in icons at every one of [`PIXMAP_SIZES`] — once, at start-up, since the
    /// icon changes with every song that starts or stops and rasterising is not free.
    #[must_use]
    pub fn rasterised() -> Self {
        let at_every_size = |icon: TrayIcon| -> Vec<Icon> {
            PIXMAP_SIZES
                .iter()
                .filter_map(|&size| rasterise(icon.svg(), size))
                .collect()
        };
        Self {
            off: at_every_size(TrayIcon::Off),
            on: at_every_size(TrayIcon::On),
            processing: at_every_size(TrayIcon::Processing),
        }
    }

    #[must_use]
    pub fn for_icon(&self, icon: TrayIcon) -> &[Icon] {
        match icon {
            TrayIcon::Off => &self.off,
            TrayIcon::On => &self.on,
            TrayIcon::Processing => &self.processing,
        }
    }
}

/// `svg` drawn into a `size` × `size` square, centred and scaled to fit without distortion, as
/// straight-alpha ARGB32 in network byte order. `None` when the SVG cannot be read.
#[must_use]
pub fn rasterise(svg: &[u8], size: u32) -> Option<Icon> {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let (width, height) = (tree.size().width(), tree.size().height());
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let side = size as f32;
    let scale = side / width.max(height);
    let transform = tiny_skia::Transform::from_scale(scale, scale)
        .post_translate((side - width * scale) / 2.0, (side - height * scale) / 2.0);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    // tiny-skia keeps premultiplied RGBA and hands back straight RGBA; the item wants A, R, G, B.
    let rgba = pixmap.take_demultiplied();
    let data = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| [a, r, g, b])
        .collect();
    Some(Icon {
        width: i32::try_from(size).ok()?,
        height: i32::try_from(size).ok()?,
        data,
    })
}

/// One row of a preset submenu.
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
    /// The device's `node.name`, which a pick carries back ([`TrayCommand::SelectDevice`]).
    pub node_name: String,
    /// Playback or capture device: which lane's group the row is in.
    pub direction: DeviceDirection,
}

/// One lane as the tray draws it: its presets and its device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayLane {
    /// The lane's presets, factory ones first, as its picker lists them.
    pub presets: Vec<TrayPreset>,
    /// Index into `presets`.
    pub selected_preset: Option<usize>,
    /// The lane's device, as an index into [`TrayState::devices`]; `None` while the lane is off,
    /// or on a device the list does not carry right now.
    pub device: Option<usize>,
    /// The name of the device the lane is on while the list does not carry it — a Bluetooth
    /// headset between profiles, the moments before the first list. The tooltip names it, and
    /// neither `Off` nor any row is ticked: the lane is not off.
    pub unlisted: Option<String>,
}

/// The tray's mirror of the model.
///
/// Everything the menu and the tooltip need, and nothing else. The application owns the real
/// state; this copy is refreshed with [`TrayHandle::update`].
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The speakers' lane.
    pub output: TrayLane,
    /// The microphone's lane.
    pub input: TrayLane,
    /// Every device, outputs and inputs, in the order the lanes rank them.
    pub devices: Vec<TrayDevice>,
    /// The UI language the labels were built in, so a switch re-sends the menu.
    pub language: String,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            // `Settings.cpp:31` ships `power = 1`, so the tray starts the way a fresh install does.
            power: true,
            processing: false,
            power_enabled: true,
            theme: ThemeMode::default(),
            output: TrayLane::default(),
            input: TrayLane::default(),
            devices: Vec::new(),
            language: String::new(),
        }
    }
}

impl TrayState {
    /// One lane.
    #[must_use]
    pub const fn lane(&self, direction: DeviceDirection) -> &TrayLane {
        match direction {
            DeviceDirection::Output => &self.output,
            DeviceDirection::Input => &self.input,
        }
    }

    /// One lane's device, if the lane is on and its index still points at a device of its
    /// direction.
    #[must_use]
    pub fn device(&self, direction: DeviceDirection) -> Option<&TrayDevice> {
        self.lane(direction)
            .device
            .and_then(|at| self.devices.get(at))
            .filter(|device| device.direction == direction)
    }

    /// Which icon the item should be showing.
    #[must_use]
    pub const fn icon(&self) -> TrayIcon {
        TrayIcon::of(self.power, self.processing)
    }

    /// One of the tooltip's device lines: `"Output: <device>"` or `"Input: <device>"`, with `Off`
    /// for a lane that has none (`docs/spec/12-audio-io.md` §28).
    #[must_use]
    pub fn device_line(&self, direction: DeviceDirection) -> String {
        // `"Output: "` is the original's key, trailing space and all; `"Input: "` is this port's.
        let key = match direction {
            DeviceDirection::Output => "Output: ",
            DeviceDirection::Input => "Input: ",
        };
        let device = match self.device(direction) {
            Some(device) => device.name.clone(),
            None => self
                .lane(direction)
                .unlisted
                .clone()
                .unwrap_or_else(|| tr("Off")),
        };
        format!("{}{device}", tr(key))
    }

    /// The two device lines, output first.
    #[must_use]
    pub fn device_lines(&self) -> String {
        format!(
            "{}\n{}",
            self.device_line(DeviceDirection::Output),
            self.device_line(DeviceDirection::Input)
        )
    }

    /// The tooltip as `setStatus` composes it (`FxSystemTrayView.cpp:78-84`): `"FxSound is on."`,
    /// a blank line, then `"Output: "` and the device — and under it, for the lane Windows never
    /// had, `"Input: "` and the microphone.
    ///
    /// Windows builds this with `swprintf_s` over a *translated* format string, which makes a bad
    /// translation a memory-safety bug (`:79`); Rust's formatting cannot be induced to do that.
    #[must_use]
    pub fn tooltip(&self) -> String {
        format!("{}\n\n{}", self.status_line(), self.device_lines())
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
/// from the menu's Open (`:253-255`), and the two lanes' own: a preset by lane, and `Off`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    /// Left click, or Enter on the item: show the window if it is hidden, hide it if it is showing.
    ToggleWindow,
    /// Menu ▸ Open — always shows, never hides.
    Open,
    /// Menu ▸ Turn On / Turn Off, carrying the requested state (`setPowerState(!power)`, `:257-259`).
    SetPower(bool),
    /// A preset of one lane's submenu, by name (`setPreset(id - 101)`, `:244-247`): the name,
    /// not the row, so a list that changed since the menu was drawn cannot pick another preset.
    SelectPreset {
        direction: DeviceDirection,
        name: String,
    },
    /// A device of one lane's group (`setOutput(id - 201)`, `:365-368`), by its `node.name` and
    /// direction rather than by its row: the list can change between drawing the menu and the
    /// click being handled — a hotplug, a newcomer ranked first — and a row would then pick
    /// another device, perhaps of the other lane. A device no longer listed does nothing.
    SelectDevice {
        direction: DeviceDirection,
        node_name: String,
    },
    /// A lane's `Off` row: detach it.
    Detach(DeviceDirection),
    /// Menu ▸ Settings. The Windows handler runs a modal loop here (`:261-265`); this one must
    /// return at once and let the GUI thread open the pane.
    OpenSettings,
    /// Menu ▸ Theme ▸ Dark | Light (`:267-273`).
    SetTheme(ThemeMode),
    /// Menu ▸ Exit. The only way to quit from the UI on Windows (`:285-287`).
    Exit,
}

/// The StatusNotifierItem itself.
///
/// Public because [`TrayHandle`] names it, and because the menu is worth testing without a D-Bus
/// session: build one and call [`Tray::menu`].
pub struct FxTray {
    state: TrayState,
    /// Rasterised once and kept here, not in the mirror the application replaces on each redraw.
    pixmaps: TrayPixmaps,
    tx: WakingSender<TrayCommand>,
    /// Whether a `StatusNotifierWatcher` is actually there to draw the icon.
    ///
    /// Shared with the GUI thread, which needs it to answer one question honestly: when the
    /// window hides, is there anything left on screen? On a GNOME session without the
    /// AppIndicator extension there is not — no window, no icon, and until now no message saying
    /// so, which leaves a running process the user cannot see or reach.
    watcher: Arc<AtomicBool>,
}

impl FxTray {
    /// An item drawing `state`, sending what is clicked on `tx`, with no pixmaps: a panel then
    /// resolves the icon by name alone. [`FxTray::with_pixmaps`] adds them.
    #[must_use]
    pub fn new(state: TrayState, tx: impl Into<WakingSender<TrayCommand>>) -> Self {
        Self {
            state,
            pixmaps: TrayPixmaps::default(),
            tx: tx.into(),
            watcher: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Offer `pixmaps` beside the icon's name.
    #[must_use]
    pub fn with_pixmaps(self, pixmaps: TrayPixmaps) -> Self {
        Self { pixmaps, ..self }
    }

    /// The flag the GUI thread reads to find out whether the icon is really there.
    #[must_use]
    pub fn watcher_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.watcher)
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

    /// The two lanes' preset submenus, `Output Presets ▸` and `Input Presets ▸` — the original's
    /// `Preset Select ▸`, once per lane. The original drops it while the power is off
    /// (`FxSystemTrayView.cpp:313-316`); here it stays, as the window's preset list and the
    /// command line do, so a preset can be picked before switching on (0.4.0 audit R7). A lane
    /// with no presets has no submenu.
    fn preset_menus(&self) -> Vec<MenuItem<Self>> {
        DeviceDirection::ALL
            .into_iter()
            .filter_map(|direction| self.preset_menu(direction))
            .collect()
    }

    fn preset_menu(&self, direction: DeviceDirection) -> Option<MenuItem<Self>> {
        let lane = self.state.lane(direction);
        if lane.presets.is_empty() {
            return None;
        }
        let mut submenu = Vec::new();
        if let Some(group) = self.preset_group(direction, true) {
            submenu.push(group);
        }
        // The separator Windows inserts where `preset.type` changes (`:238-242`). It has to sit
        // *between* two radio groups because DBusMenu radio state is per group.
        if submenu.len() == 1 && lane.presets.iter().any(|preset| !preset.factory) {
            submenu.push(MenuItem::Separator);
        }
        if let Some(group) = self.preset_group(direction, false) {
            submenu.push(group);
        }
        Some(
            SubMenu {
                label: preset_menu_label(direction),
                submenu,
                ..Default::default()
            }
            .into(),
        )
    }

    fn preset_group(&self, direction: DeviceDirection, factory: bool) -> Option<MenuItem<Self>> {
        let lane = self.state.lane(direction);
        let indices: Vec<usize> = lane
            .presets
            .iter()
            .enumerate()
            .filter(|(_, preset)| preset.factory == factory)
            .map(|(i, _)| i)
            .collect();
        if indices.is_empty() {
            return None;
        }
        let options = indices
            .iter()
            .map(|&i| RadioItem {
                label: preset_label(&lane.presets[i]),
                ..Default::default()
            })
            .collect();
        // `usize::MAX` when the selection lives in the *other* group: ksni ticks the option whose
        // index equals `selected` (`ksni-0.3.6/src/menu.rs:947-948`), so an unreachable index
        // leaves the whole group unticked, which is what a split radio group needs.
        let selected = lane
            .selected_preset
            .and_then(|selected| indices.iter().position(|&i| i == selected))
            .unwrap_or(usize::MAX);
        let names: Vec<String> = indices
            .iter()
            .map(|&i| lane.presets[i].name.clone())
            .collect();
        Some(
            RadioGroup {
                selected,
                select: Box::new(move |tray: &mut Self, index| {
                    if let Some(name) = names.get(index) {
                        tray.send(TrayCommand::SelectPreset {
                            direction,
                            name: name.clone(),
                        });
                    }
                }),
                options,
            }
            .into(),
        )
    }

    /// `Playback Device Select ▸` (`FxSystemTrayView.cpp:339-381`), always a submenu here.
    ///
    /// A disabled `"Output"` header and the output lane's radio group, a separator, a disabled
    /// `"Input"` header and the input lane's group — each half present only when its direction has
    /// devices. Each group starts with `Off`, ticked while the lane is detached. Radio state is
    /// per group in DBusMenu, so the two lanes' ticks are independent, as the lanes are.
    fn device_menu(&self) -> Option<MenuItem<Self>> {
        if self.state.devices.is_empty() {
            return None;
        }
        let mut submenu: Vec<MenuItem<Self>> = Vec::new();
        for direction in DeviceDirection::ALL {
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

    /// One lane's radio group — `Off`, then its direction's devices — or `None` when the direction
    /// has no devices.
    fn device_group(&self, direction: DeviceDirection) -> Option<MenuItem<Self>> {
        let indices: Vec<usize> = self
            .state
            .devices
            .iter()
            .enumerate()
            .filter(|(_, device)| device.direction == direction)
            .map(|(i, _)| i)
            .collect();
        if indices.is_empty() {
            return None;
        }
        let options = std::iter::once(RadioItem {
            label: tr("Off"),
            ..Default::default()
        })
        .chain(indices.iter().map(|&i| RadioItem {
            label: truncate_label(&self.state.devices[i].name),
            ..Default::default()
        }))
        .collect();
        // Row 0 is `Off`; a device index that no longer points into this direction's rows ticks
        // nothing rather than the wrong row.
        let lane = self.state.lane(direction);
        let selected = match lane.device {
            None if lane.unlisted.is_some() => usize::MAX,
            None => 0,
            Some(device) => indices
                .iter()
                .position(|&i| i == device)
                .map_or(usize::MAX, |row| row + 1),
        };
        Some(
            RadioGroup {
                selected,
                select: Box::new(move |tray: &mut Self, row| match row {
                    0 => tray.send(TrayCommand::Detach(direction)),
                    row => {
                        if let Some(device) = indices
                            .get(row - 1)
                            .and_then(|&device| tray.state.devices.get(device))
                        {
                            let node_name = device.node_name.clone();
                            tray.send(TrayCommand::SelectDevice {
                                direction,
                                node_name,
                            });
                        }
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
        self.pixmaps.for_icon(self.state.icon()).to_vec()
    }

    /// Many panels render only the tooltip's `title`, so the line that changes with the power goes
    /// there and the two lanes' devices follow in the description, a line each
    /// (`docs/spec/07-startup-tray.md` §5.8).
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
            title: self.state.status_line(),
            description: self.state.device_lines(),
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

        menu.extend(self.preset_menus());
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
        // No Always On Top here: departure 5 in the module docs.
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
        self.watcher.store(false, Ordering::Relaxed);
        true
    }

    /// A watcher appeared — a panel starting, or the session finally providing one.
    fn watcher_online(&self) {
        log::info!("a StatusNotifierWatcher is present; the tray icon is visible");
        self.watcher.store(true, Ordering::Relaxed);
    }
}

/// The GUI thread's end of the tray.
///
/// [`Handle::update`] here is the *blocking* one (`ksni-0.3.6/src/blocking.rs:205`), so the egui
/// thread can call it directly without an async runtime of its own.
pub struct TrayHandle {
    handle: Handle<FxTray>,
    watcher: Arc<AtomicBool>,
}

impl TrayHandle {
    /// Whether the icon is actually on screen.
    ///
    /// `spawn` succeeds on a session with no `StatusNotifierWatcher` — deliberately, so the icon
    /// appears the moment a panel starts — which means "the tray is running" and "the tray is
    /// visible" are two different questions. This answers the second.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.watcher.load(Ordering::Relaxed)
    }

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

/// The tray is redrawn from the controller's events (`crate::events::fan_out`): the whole mirror
/// at once, and only when something it draws changed — every update is a D-Bus round trip and a
/// set of property signals.
impl crate::events::TraySink for TrayHandle {
    fn redraw(&self, state: TrayState) {
        self.update(|mirror| *mirror = state);
    }
}

/// Register the tray item and start serving it, with the icons rasterised for hosts that do not
/// look them up by name.
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
pub fn spawn(
    state: TrayState,
    tx: impl Into<WakingSender<TrayCommand>>,
) -> Result<TrayHandle, ksni::Error> {
    let tray = FxTray::new(state, tx).with_pixmaps(TrayPixmaps::rasterised());
    let watcher = tray.watcher_flag();
    let handle = tray.assume_sni_available(true).spawn()?;
    Ok(TrayHandle { handle, watcher })
}

/// A lane's preset submenu, by its direction.
fn preset_menu_label(direction: DeviceDirection) -> String {
    match direction {
        DeviceDirection::Output => tr("Output Presets"),
        DeviceDirection::Input => tr("Input Presets"),
    }
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

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    fn preset(name: &str, factory: bool, modified: bool) -> TrayPreset {
        TrayPreset {
            name: name.to_owned(),
            factory,
            modified,
        }
    }

    fn device(name: &str, direction: DeviceDirection) -> TrayDevice {
        TrayDevice {
            name: name.to_owned(),
            node_name: node_name(name),
            direction,
        }
    }

    /// The `node.name` [`device`] gives a device called `name`.
    fn node_name(name: &str) -> String {
        format!("node.{}", name.to_lowercase().replace(' ', "_"))
    }

    /// What picking the device called `name` sends.
    fn pick(name: &str, direction: DeviceDirection) -> TrayCommand {
        TrayCommand::SelectDevice {
            direction,
            node_name: node_name(name),
        }
    }

    /// Three outputs — one of them a mono headset, selectable like any other — and the two
    /// capture devices of the development machine, the speakers on the first output and the
    /// microphone lane off.
    fn populated() -> TrayState {
        TrayState {
            output: TrayLane {
                presets: vec![
                    preset("General", true, false),
                    preset("Bass Booster", true, false),
                    preset("My Mix", false, true),
                ],
                selected_preset: Some(2),
                device: Some(0),
                unlisted: None,
            },
            input: TrayLane {
                presets: vec![
                    preset("Podcast", true, false),
                    preset("Streaming", true, false),
                ],
                selected_preset: Some(0),
                device: None,
                unlisted: None,
            },
            devices: vec![
                device("Built-in Audio Analogue Stereo", OUT),
                device("HDMI / DisplayPort 3 Output That Goes On Forever", OUT),
                device("Mono Headset", OUT),
                device("fifine Microphone Analogue Stereo", IN),
                device("Ryzen HD Audio Controller Analogue Stereo", IN),
            ],
            ..TrayState::default()
        }
    }

    /// `populated()` with the microphone lane on the fifine.
    fn both_lanes() -> TrayState {
        let mut state = populated();
        state.input.device = Some(3);
        state
    }

    fn with_state(state: TrayState) -> (FxTray, Receiver<TrayCommand>) {
        let (tx, rx) = unbounded();
        (FxTray::new(state, tx), rx)
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

    fn options(group: &RadioGroup<FxTray>) -> Vec<String> {
        group.options.iter().map(|o| o.label.clone()).collect()
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
    fn the_menu_carries_the_windows_items_in_the_windows_order_with_a_preset_submenu_per_lane() {
        let (tray, _rx) = with_state(populated());
        assert_eq!(
            labels(&tray.menu()),
            vec![
                "Open",
                "Turn Off",
                "Output Presets",
                "Input Presets",
                "---",
                "Playback Device Select",
                "---",
                "Settings",
                "Theme",
                "Exit",
            ]
        );
    }

    #[test]
    fn the_menu_has_no_always_on_top_item_that_would_change_nothing() {
        // Departure 5: winit's Wayland backend ignores window levels, so the item is gone rather
        // than ticked for nothing, whether the power is on or off.
        for power in [true, false] {
            let (tray, _rx) = with_state(TrayState {
                power,
                ..populated()
            });
            let labels = labels(&tray.menu());
            assert!(
                !labels
                    .iter()
                    .any(|label| label == "Always On Top" || *label == tr("Always On Top")),
                "{labels:?}"
            );
        }
    }

    #[test]
    fn the_preset_submenus_stay_while_the_power_is_off() {
        // `FxSystemTrayView.cpp:313-316` drops them with the power off; the port keeps them, as
        // the window's list and the command line do (0.4.0 audit R7).
        let (on, _rx) = with_state(populated());
        let (tray, _rx) = with_state(TrayState {
            power: false,
            ..populated()
        });
        let off = labels(&tray.menu());
        assert!(off.contains(&tr("Output Presets")), "{off:?}");
        assert!(off.contains(&tr("Input Presets")), "{off:?}");
        assert_eq!(off[1], "Turn On", "the item says what it will do");
        assert!(off.contains(&tr("Playback Device Select")));
        // The same items as with the power on, but for the power item's words.
        let on = labels(&on.menu());
        assert_eq!(off.len(), on.len(), "{off:?} / {on:?}");
    }

    #[test]
    fn a_lane_with_no_presets_has_no_submenu() {
        let mut state = populated();
        state.input = TrayLane::default();
        let (tray, _rx) = with_state(state);
        let labels = labels(&tray.menu());
        assert!(labels.contains(&tr("Output Presets")), "{labels:?}");
        assert!(!labels.contains(&tr("Input Presets")), "{labels:?}");
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
        let presets = submenu_of(&menu, "Output Presets");

        assert_eq!(
            labels(presets),
            vec!["<radio>", "---", "<radio>"],
            "a separator sits where the preset type changes"
        );

        let groups = radio_groups(presets);
        let built_in = &groups[0];
        let user = &groups[1];
        assert_eq!(options(built_in), vec!["General", "Bass Booster"]);
        assert_eq!(
            options(user),
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
    fn each_lane_s_submenu_lists_its_own_presets_with_its_own_tick() {
        let (tray, _rx) = with_state(populated());
        let menu = tray.menu();
        let voices = submenu_of(&menu, "Input Presets");
        assert_eq!(
            labels(voices),
            vec!["<radio>"],
            "factory voices only: no separator"
        );
        let group = radio_groups(voices)[0];
        assert_eq!(options(group), vec!["Podcast", "Streaming"]);
        assert_eq!(group.selected, 0);
    }

    #[test]
    fn picking_a_preset_names_it_and_its_lane() {
        let (mut tray, rx) = with_state(populated());
        let menu = tray.menu();
        let music = radio_groups(submenu_of(&menu, "Output Presets"));
        (music[1].select)(&mut tray, 0);
        assert_eq!(
            rx.try_recv(),
            Ok(TrayCommand::SelectPreset {
                direction: OUT,
                name: "My Mix".to_owned(),
            }),
            "the name, without the unsaved-changes star"
        );
        let voices = radio_groups(submenu_of(&menu, "Input Presets"));
        (voices[0].select)(&mut tray, 1);
        assert_eq!(
            rx.try_recv(),
            Ok(TrayCommand::SelectPreset {
                direction: IN,
                name: "Streaming".to_owned(),
            })
        );
        (voices[0].select)(&mut tray, 9);
        assert!(rx.try_recv().is_err(), "a row past the group sends nothing");
    }

    #[test]
    fn a_preset_picked_in_the_tray_with_the_power_off_is_sent_as_with_it_on() {
        // 0.4.0 audit R7.
        let (mut tray, rx) = with_state(TrayState {
            power: false,
            ..populated()
        });
        let menu = tray.menu();
        let music = radio_groups(submenu_of(&menu, "Output Presets"));
        (music[1].select)(&mut tray, 0);
        assert_eq!(
            rx.try_recv(),
            Ok(TrayCommand::SelectPreset {
                direction: OUT,
                name: "My Mix".to_owned(),
            })
        );
    }

    #[test]
    fn each_lane_has_a_device_group_that_starts_with_off() {
        let (tray, _rx) = with_state(both_lanes());
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
            assert!(!header.enabled, "a header is not clickable");
        }

        let groups = radio_groups(devices);
        let (outputs, inputs) = (groups[0], groups[1]);
        assert_eq!(
            options(outputs),
            vec![
                "Off",
                "Built-in Audio Analogue Stereo",
                "HDMI / DisplayPort 3 Output...",
                "Mono Headset",
            ]
        );
        assert_eq!(
            options(inputs),
            vec![
                "Off",
                "fifine Microphone Analogue ...",
                "Ryzen HD Audio Controller A...",
            ],
            "truncated to 30 characters like every other row"
        );
        assert_eq!(outputs.selected, 1, "the speakers");
        assert_eq!(inputs.selected, 1, "and, beside them, the microphone");
        assert!(
            outputs.options.iter().all(|row| row.enabled),
            "a mono output is a valid target (U7), and Off always is"
        );
    }

    #[test]
    fn a_lane_that_is_off_ticks_its_off_row() {
        let (tray, _rx) = with_state(populated());
        let menu = tray.menu();
        let groups = radio_groups(submenu_of(&menu, "Playback Device Select"));
        assert_eq!(groups[0].selected, 1);
        assert_eq!(groups[1].selected, 0, "the microphone lane is off");

        let mut state = populated();
        state.output.device = None;
        let (tray, _rx) = with_state(state);
        let menu = tray.menu();
        let groups = radio_groups(submenu_of(&menu, "Playback Device Select"));
        assert_eq!(groups[0].selected, 0, "and so, now, is the speakers' lane");
    }

    #[test]
    fn a_lane_on_a_device_the_list_does_not_carry_is_named_and_not_called_off() {
        let mut state = populated();
        state.output.device = None;
        state.output.unlisted = Some("WH-1000XM4".to_owned());
        assert_eq!(
            state.device_line(OUT),
            format!("{}WH-1000XM4", tr("Output: "))
        );
        let (tray, _rx) = with_state(state);
        let menu = tray.menu();
        let groups = radio_groups(submenu_of(&menu, "Playback Device Select"));
        assert!(
            groups[0].selected >= groups[0].options.len(),
            "neither Off nor another device is ticked"
        );
    }

    #[test]
    fn a_device_index_that_points_into_the_other_direction_ticks_nothing() {
        let mut state = populated();
        state.output.device = Some(3);
        let (tray, _rx) = with_state(state);
        let menu = tray.menu();
        let groups = radio_groups(submenu_of(&menu, "Playback Device Select"));
        assert!(groups[0].selected >= groups[0].options.len());
    }

    #[test]
    fn picking_a_device_reports_its_node_name_and_lane_and_off_detaches_the_lane() {
        let (mut tray, rx) = with_state(both_lanes());
        let menu = tray.menu();
        let groups = radio_groups(submenu_of(&menu, "Playback Device Select"));
        (groups[1].select)(&mut tray, 2);
        assert_eq!(
            rx.try_recv(),
            Ok(pick("Ryzen HD Audio Controller Analogue Stereo", IN)),
            "the second input, by name: not the fifth row of a list that may have moved"
        );
        (groups[0].select)(&mut tray, 3);
        assert_eq!(rx.try_recv(), Ok(pick("Mono Headset", OUT)));
        (groups[1].select)(&mut tray, 0);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::Detach(IN)));
        (groups[0].select)(&mut tray, 0);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::Detach(OUT)));
        (groups[1].select)(&mut tray, 7);
        assert!(rx.try_recv().is_err(), "a row past the group sends nothing");
    }

    #[test]
    fn outputs_only_makes_a_submenu_with_just_the_output_half() {
        // The usual case on a machine with no microphone.
        let (tray, _rx) = with_state(TrayState {
            devices: vec![
                device("Built-in Audio Analogue Stereo", OUT),
                device("Mono Headset", OUT),
            ],
            output: TrayLane {
                device: Some(0),
                ..populated().output
            },
            input: TrayLane {
                device: None,
                ..populated().input
            },
            ..populated()
        });
        let menu = tray.menu();
        let devices = submenu_of(&menu, "Playback Device Select");
        assert_eq!(
            labels(devices),
            vec!["Output", "<radio>"],
            "one header, one group, no separator and no Input header"
        );
        assert_eq!(radio_groups(devices)[0].selected, 1);
    }

    #[test]
    fn inputs_only_makes_a_submenu_with_just_the_input_half() {
        let (tray, _rx) = with_state(TrayState {
            devices: vec![device("Microphone", IN)],
            output: TrayLane {
                device: None,
                ..populated().output
            },
            input: TrayLane {
                device: Some(0),
                ..populated().input
            },
            ..populated()
        });
        let menu = tray.menu();
        let devices = submenu_of(&menu, "Playback Device Select");
        assert_eq!(labels(devices), vec!["Input", "<radio>"]);
        assert_eq!(radio_groups(devices)[0].selected, 1);
    }

    #[test]
    fn the_device_section_disappears_when_there_are_no_devices() {
        let mut state = populated();
        state.devices.clear();
        state.output.device = None;
        let (tray, _rx) = with_state(state);
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
        assert_eq!(options(group), vec!["Dark", "Light"]);
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

        activate(&menu, "Exit", &mut tray);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::Exit));

        let theme = submenu_of(&menu, "Theme");
        (radio_groups(theme)[0].select)(&mut tray, 1);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::SetTheme(ThemeMode::Light)));

        let devices = submenu_of(&menu, "Playback Device Select");
        (radio_groups(devices)[0].select)(&mut tray, 2);
        assert_eq!(
            rx.try_recv(),
            Ok(pick(
                "HDMI / DisplayPort 3 Output That Goes On Forever",
                OUT
            ))
        );
        assert_eq!(tray.state(), &populated(), "the mirror is as it was drawn");
    }

    #[test]
    fn a_click_wakes_the_gui_thread_to_carry_it_out() {
        let waker = crate::wake::Waker::new();
        let (tx, rx) = unbounded();
        let mut tray = FxTray::new(populated(), WakingSender::new(tx, waker.clone()));
        tray.activate(0, 0);
        assert_eq!(rx.try_recv(), Ok(TrayCommand::ToggleWindow));
        assert_eq!(
            waker.pending().len(),
            1,
            "the pump is told there is something to do"
        );
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
    fn the_tooltip_reads_like_the_windows_one_with_a_line_for_the_microphone() {
        // `FxSystemTrayView.cpp:78-84` — status line, blank line, "Output: " and the device —
        // and the lane Windows never had under it.
        let state = both_lanes();
        assert_eq!(
            state.tooltip(),
            "FxSound is on.\n\nOutput: Built-in Audio Analogue Stereo\n\
             Input: fifine Microphone Analogue Stereo"
        );
        let off = TrayState {
            power: false,
            ..both_lanes()
        };
        assert_eq!(
            off.tooltip(),
            "FxSound is off.\n\nOutput: Built-in Audio Analogue Stereo\n\
             Input: fifine Microphone Analogue Stereo"
        );

        let (tray, _rx) = with_state(state);
        let tip = tray.tool_tip();
        assert_eq!(tip.title, "FxSound is on.");
        assert_eq!(
            tip.description,
            "Output: Built-in Audio Analogue Stereo\nInput: fifine Microphone Analogue Stereo"
        );
    }

    #[test]
    fn a_lane_with_no_device_says_off_in_the_tooltip() {
        let state = populated();
        assert_eq!(
            state.device_lines(),
            "Output: Built-in Audio Analogue Stereo\nInput: Off"
        );
        let mut neither = populated();
        neither.output.device = None;
        assert_eq!(neither.device_lines(), "Output: Off\nInput: Off");
    }

    #[test]
    fn every_status_icon_is_the_five_bars_centred_in_a_square_with_a_transparent_background() {
        for icon in TrayIcon::ALL {
            let svg = std::str::from_utf8(icon.svg()).expect("utf-8");
            assert_eq!(svg.matches("<rect").count(), 5, "{icon:?}: the five bars");
            assert!(
                svg.contains(r#"viewBox="0 -40.285 299.83 299.83""#),
                "{icon:?}: a square around the glyph, so no host squeezes it"
            );
        }
        let colours = TrayIcon::ALL.map(|icon| {
            let svg = std::str::from_utf8(icon.svg()).expect("utf-8");
            let at = svg.find("fill=\"").expect("a fill") + 6;
            svg[at..at + 7].to_owned()
        });
        assert_eq!(
            colours,
            ["#6f6f6f", "#ffffff", "#23b6eb"],
            "grey, white, blue"
        );
    }

    #[test]
    fn the_pixmaps_are_rasterised_at_every_size_as_argb_with_the_corners_left_clear() {
        let pixmaps = TrayPixmaps::rasterised();
        for icon in TrayIcon::ALL {
            let sizes: Vec<i32> = pixmaps.for_icon(icon).iter().map(|p| p.width).collect();
            assert_eq!(
                sizes,
                PIXMAP_SIZES.map(|s| s as i32),
                "{icon:?}: one pixmap per size"
            );
            for pixmap in pixmaps.for_icon(icon) {
                assert_eq!(pixmap.width, pixmap.height);
                assert_eq!(
                    pixmap.data.len(),
                    (pixmap.width * pixmap.height * 4) as usize
                );
                assert_eq!(
                    pixmap.data[0], 0,
                    "{icon:?}: the top-left corner is transparent"
                );
            }
        }
        // The middle of the tallest bar, at 32 pixels: opaque, in the icon's colour, as A R G B.
        let blue = &pixmaps.processing[3];
        let middle = ((16 * blue.width + 16) * 4) as usize;
        assert_eq!(&blue.data[middle..middle + 4], &[0xff, 0x23, 0xb6, 0xeb]);
        let grey = &pixmaps.off[3];
        assert_eq!(&grey.data[middle..middle + 4], &[0xff, 0x6f, 0x6f, 0x6f]);
    }

    /// What a pixmap list shows: `ksni::Icon` has no `PartialEq`.
    fn pixels(icons: &[Icon]) -> Vec<(i32, i32, &[u8])> {
        icons
            .iter()
            .map(|icon| (icon.width, icon.height, icon.data.as_slice()))
            .collect()
    }

    #[test]
    fn the_pixmaps_follow_the_icon_state_and_an_item_without_them_offers_the_name_alone() {
        let (tray, _rx) = with_state(populated());
        assert!(
            tray.icon_pixmap().is_empty(),
            "with no pixmaps supplied the panel resolves the icon name itself"
        );

        let pixmaps = TrayPixmaps::rasterised();
        let (tray, _rx) = with_state(TrayState {
            processing: true,
            ..populated()
        });
        let tray = tray.with_pixmaps(pixmaps.clone());
        assert_eq!(pixels(&tray.icon_pixmap()), pixels(&pixmaps.processing));
        let (tray, _rx) = with_state(TrayState {
            power: false,
            ..populated()
        });
        let tray = tray.with_pixmaps(pixmaps.clone());
        assert_eq!(pixels(&tray.icon_pixmap()), pixels(&pixmaps.off));
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
