//! A standing visual check of the real views, with no audio behind them.
//!
//! Opens the frameless window and draws [`fxsound_ui::views::show`] over a made-up [`UiState`]:
//! two speakers and two microphones, a preset list per lane, a live-looking spectrum and a moving
//! microphone readout. A few lines at the bottom of this file stand in for the application, so
//! the combos, the lanes, the edit direction and the notice can all be clicked through. It is not
//! the application, and it never touches PipeWire.
//!
//! ```text
//! cargo run -p fxsound-ui --example preview
//! cargo run -p fxsound-ui --example preview -- --light --input --notice
//! cargo run -p fxsound-ui --example preview -- --lite
//! cargo run -p fxsound-ui --example preview -- --input --lang=de
//! cargo run -p fxsound-ui --example preview -- --levels --eq-off
//! cargo run -p fxsound-ui --example preview -- --curve
//! cargo run -p fxsound-ui --example preview -- --apps --light
//! cargo run -p fxsound-ui --example preview -- --stored
//! cargo run -p fxsound-ui --example preview -- --lang=de --message
//! cargo run -p fxsound-ui --example preview -- --settings=general --light
//! cargo run -p fxsound-ui --example preview -- --settings=experimental --parity=sound --lang=ru
//! ```
//!
//! Flags: `--light`, `--input` (edit the microphone lane), `--lite`, `--notice[=TEXT]`,
//! `--detached` (both lanes off), `--levels` (start with the effect column turned over to the
//! equalizer's controls), `--eq-off` (the equalizer switched off), `--curve` (a curve of boosts and
//! cuts rather than a flat one, to see the response drawn), `--power-off`, `--stored` (the
//! effects as a Windows preset stores them, between the sliders' positions), `--lang=CODE`
//! (one of the translation tables' codes; English otherwise), `--apps[=empty]` (Settings ▸
//! Applications over a made-up list of applications, or none), `--settings=TAB` (Settings on
//! `audio`, `general`, `help`, `microphone`, `applications` or `experimental`),
//! `--parity=LEVEL` (the level of «Like FxSound for Windows», `off`, `interface` or `sound`:
//! what the Experimental pane's slider stands at, and what the views follow as each level is
//! built; `full` shows as `sound`, as the app runs a `settings.toml` that says it),
//! `--message[=TEXT]` (the Yes/No message box over the window: `TEXT` is translated, and its `%s`
//! is a long preset name; the
//! export's overwrite question otherwise), `--exit-after-paint`. Keys while it
//! runs: `I` switches the edit direction, `N` puts a notice up, `L` flips Pro/Lite, `T` flips the
//! palette, `Esc` quits.
//!
//! The made-up state has Battlefield 6 on its own preset on the speakers and Discord on the
//! microphone, so resting the pointer on the Pro window's preset list shows the edit direction's
//! routed applications.

use eframe::egui;
use fxsound_core::{AppKey, AudioDevice, DeviceDirection, ThemeMode, ViewMode, WindowsParity};
use fxsound_ui::dialogs::settings::DevicePriority;
use fxsound_ui::dialogs::{
    AppLane, AppRow, NavIcons, SettingsAction, SettingsDialog, SettingsState, SettingsTab, settings,
};
use fxsound_ui::state::{PresetEntry, RoutedApp};
use fxsound_ui::{AssetCache, Palette, UiAction, UiState, ViewScratch, theme, views, window_size};

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let notice = args.iter().find_map(|a| {
        a.strip_prefix("--notice")
            .map(|rest| rest.strip_prefix('=').unwrap_or(DEFAULT_NOTICE).to_owned())
    });

    if let Some(code) = args.iter().find_map(|a| a.strip_prefix("--lang="))
        && !fxsound_core::i18n::set_language(code)
    {
        eprintln!("no translation table for {code:?}; showing English");
    }

    let mut state = demo_state();
    if flag("--light") {
        state.theme = ThemeMode::Light;
    }
    if flag("--lite") {
        state.view = ViewMode::Lite;
    }
    if flag("--detached") {
        state.selected_output = None;
        state.selected_input = None;
    }
    if flag("--eq-off") {
        state.eq_on = false;
    }
    if flag("--curve") {
        let gains = [4.0, 6.0, 2.0, -2.0, -5.0, -1.0, 2.0, 7.0, 3.0, -3.0];
        for (band, gain) in state.eq_bands.iter_mut().zip(gains) {
            band.boost_db = gain;
        }
    }
    if flag("--power-off") {
        state.power = false;
    }
    if flag("--stored") {
        // General's stored values: 50, 64, 20, 60 and 60 of 127, most of them between positions.
        use fxsound_core::{Effect, scale};
        for (effect, midi) in [
            (Effect::Fidelity, 50),
            (Effect::Ambience, 64),
            (Effect::Surround, 20),
            (Effect::DynamicBoost, 60),
            (Effect::Bass, 60),
        ] {
            state.effects[effect as usize] = scale::midi_to_slider_for(effect, midi);
        }
    }
    let mut preview = Preview::new(state, flag("--exit-after-paint"));
    if flag("--levels") {
        preview.scratch.column_face = views::ColumnFace::EqualizerControls;
    }
    if flag("--input") {
        preview.set_edit_direction(DeviceDirection::Input);
    }
    if let Some(text) = notice {
        preview.state.notify(text);
    }
    if let Some(which) = args.iter().find_map(|a| a.strip_prefix("--apps")) {
        preview.settings = Some(demo_settings(which == "=empty"));
    }
    if let Some(which) = args.iter().find_map(|a| a.strip_prefix("--settings=")) {
        let mut settings = demo_settings(false);
        settings.tab = match which {
            "general" => SettingsTab::General,
            "help" => SettingsTab::Help,
            "microphone" => SettingsTab::Microphone,
            "applications" => SettingsTab::Applications,
            "experimental" => SettingsTab::Experimental,
            _ => SettingsTab::Audio,
        };
        preview.settings = Some(settings);
    }
    if let Some(level) = args.iter().find_map(|a| a.strip_prefix("--parity=")) {
        match WindowsParity::parse(level) {
            Some(level) => {
                preview.state.windows_parity = level.offered_or_below();
                if let Some(settings) = &mut preview.settings {
                    settings.settings.windows_parity = level.offered_or_below();
                }
            }
            None => eprintln!("no level {level:?}; expected off, interface or sound"),
        }
    }
    preview.message = args.iter().find_map(|a| {
        a.strip_prefix("--message").map(|rest| {
            rest.strip_prefix('=')
                .unwrap_or(fxsound_ui::dialogs::presets::OVERWRITE_MESSAGE)
                .to_owned()
        })
    });

    let size = if preview.settings.is_some() {
        settings::WINDOW_SIZE
    } else {
        window_size(preview.state.view)
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([size.x, size.y])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_app_id("com.fxsound.FxSound.preview"),
        ..Default::default()
    };

    eframe::run_native(
        "FxSound preview",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx, preview.palette());
            Ok(Box::new(preview))
        }),
    )
}

const DEFAULT_NOTICE: &str = "Preset: Clean Voice";

fn device(id: u32, description: &str, direction: DeviceDirection) -> AudioDevice {
    AudioDevice {
        id,
        name: format!("preview.{id}"),
        description: description.to_owned(),
        is_default: id == 1,
        direction,
        form_factor: String::new(),
    }
}

fn presets(names: &[(&str, bool)]) -> Vec<PresetEntry> {
    names
        .iter()
        .map(|&(name, factory)| PresetEntry {
            name: name.to_owned(),
            factory,
            modified: false,
        })
        .collect()
}

fn demo_state() -> UiState {
    UiState {
        presets: output_presets(),
        selected_preset: Some(1),
        devices: vec![
            device(
                1,
                "Ryzen HD Audio Controller Analogue Stereo",
                DeviceDirection::Output,
            ),
            device(
                2,
                "fifine Microphone Analogue Stereo",
                DeviceDirection::Output,
            ),
            device(
                3,
                "fifine Microphone Analogue Stereo",
                DeviceDirection::Input,
            ),
            device(4, "Webcam C920 Analogue Stereo", DeviceDirection::Input),
        ],
        selected_output: Some(0),
        selected_input: Some(2),
        effects: [3.0, 5.0, 7.0, 4.0, 8.0],
        master_gain_db: -4.0,
        volume_leveling: 1.5,
        filter_q: 2.0,
        balance_db: 6.0,
        audio_active: true,
        output_active: true,
        input_active: true,
        gate_on: true,
        compressor_on: true,
        deesser_on: true,
        deesser_running: true,
        denoise_on: true,
        denoise_running: true,
        deesser_requested_hz: 5_500.0,
        routed_apps: vec![
            RoutedApp {
                direction: DeviceDirection::Output,
                name: "Battlefield 6".to_owned(),
                preset: "Gaming".to_owned(),
            },
            RoutedApp {
                direction: DeviceDirection::Input,
                name: "Discord".to_owned(),
                preset: "Headset".to_owned(),
            },
        ],
        ..UiState::default()
    }
}

fn app(binary: &str, name: &str) -> AppKey {
    AppKey {
        binary: binary.to_owned(),
        name: name.to_owned(),
        flatpak: String::new(),
    }
}

fn app_row(
    binary: &str,
    name: &str,
    running: bool,
    lanes: &[(DeviceDirection, Option<&str>)],
) -> AppRow {
    AppRow {
        app: app(binary, name),
        name: name.to_owned(),
        running,
        lanes: lanes
            .iter()
            .map(|&(direction, preset)| AppLane {
                direction,
                preset: preset.map(str::to_owned),
            })
            .collect(),
    }
}

/// One row of a device priority list, remembering the preset at `preset` in its lane's list.
fn priority_row(
    id: &str,
    name: &str,
    preset: Option<usize>,
    connected: bool,
    present: bool,
) -> DevicePriority {
    DevicePriority {
        id: id.to_owned(),
        name: name.to_owned(),
        preset,
        connected,
        present,
    }
}

/// Settings as the app would fill it. Applications: three applications running, then four
/// remembered — one of them with a name too long for its room, one whose preset is gone. The
/// priority lists: three outputs and four microphones, each with the preset it remembers but one
/// on each list, which remembers none, and one of each gone.
fn demo_settings(empty: bool) -> SettingsState {
    use DeviceDirection::{Input, Output};
    let apps = if empty {
        Vec::new()
    } else {
        vec![
            app_row(
                "bf6.exe",
                "Battlefield 6",
                true,
                &[(Output, Some("Gaming"))],
            ),
            app_row(
                "Discord",
                "Discord",
                true,
                &[(Output, None), (Input, Some("Headset"))],
            ),
            app_row("firefox", "Firefox", true, &[(Output, None)]),
            app_row("brave", "Brave", false, &[(Output, Some("Music"))]),
            app_row(
                "chrome",
                "Google Chrome Canary Developer Build",
                false,
                &[(Output, Some("Loudness"))],
            ),
            app_row("obs", "OBS Studio", false, &[(Input, Some("Podcast"))]),
            app_row(
                "spotify",
                "Spotify",
                false,
                &[(Output, None), (Input, None)],
            ),
        ]
    };
    SettingsState {
        tab: SettingsTab::Applications,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        presets: output_presets().into_iter().map(|p| p.name).collect(),
        input_presets: input_presets().into_iter().map(|p| p.name).collect(),
        apps,
        devices: vec![
            priority_row(
                "alsa_output.usb-hecate",
                "HECATE G2000 Pro",
                Some(3),
                true,
                true,
            ),
            priority_row(
                "alsa_output.pci-speakers",
                "Laptop Speakers",
                Some(1),
                false,
                true,
            ),
            priority_row("alsa_output.usb-hp", "HP Speakers", None, false, false),
        ],
        microphones: vec![
            priority_row(
                "alsa_input.usb-fifine",
                "fifine Microphone",
                Some(3),
                true,
                true,
            ),
            priority_row(
                "alsa_input.usb-hecate",
                "HECATE G2000 Pro",
                Some(1),
                false,
                true,
            ),
            priority_row(
                "alsa_input.pci-mic",
                "Built-in Audio Analogue Stereo",
                Some(2),
                false,
                true,
            ),
            priority_row("bluez_input.AC_12", "Headset", None, false, false),
        ],
        has_microphone: true,
        ..SettingsState::default()
    }
}

fn output_presets() -> Vec<PresetEntry> {
    presets(&[
        ("General", true),
        ("Music", true),
        ("Movies", true),
        ("Gaming", true),
        ("My Mix", false),
    ])
}

fn input_presets() -> Vec<PresetEntry> {
    presets(&[
        ("Clean Voice", true),
        ("Headset", true),
        ("Laptop Mic", true),
        ("Podcast", true),
    ])
}

struct Preview {
    state: UiState,
    scratch: ViewScratch,
    assets: AssetCache,
    /// Settings, drawn instead of the main window when `--apps` asked for it.
    settings: Option<SettingsState>,
    icons: NavIcons,
    /// The preset list and selection of the lane not being edited.
    parked: (Vec<PresetEntry>, Option<usize>),
    /// The message box's template, while `--message` keeps it up.
    message: Option<String>,
    exit_after_paint: bool,
    frames: u32,
}

impl Preview {
    /// The palette the application would hand the views: the Windows theme from Interface on.
    fn palette(&self) -> Palette {
        Palette::new(self.state.theme)
            .windows(self.state.windows_look(fxsound_core::WindowsLook::Palette))
    }

    fn new(state: UiState, exit_after_paint: bool) -> Self {
        Self {
            state,
            scratch: ViewScratch::new(),
            assets: AssetCache::new(),
            settings: None,
            icons: NavIcons::new(),
            parked: (input_presets(), Some(0)),
            message: None,
            exit_after_paint,
            frames: 0,
        }
    }

    fn set_edit_direction(&mut self, direction: DeviceDirection) {
        if direction == self.state.direction {
            return;
        }
        self.state.direction = direction;
        let presets =
            std::mem::replace(&mut self.state.presets, std::mem::take(&mut self.parked.0));
        let selected = std::mem::replace(&mut self.state.selected_preset, self.parked.1);
        self.parked = (presets, selected);
    }

    /// Made-up telemetry that moves, so the strip and the visualizer can be judged in motion.
    fn animate(&mut self, t: f64) {
        let wave = |speed: f64, phase: f64| ((t * speed + phase).sin() * 0.5 + 0.5) as f32;
        for (band, bar) in self.state.spectrum.iter_mut().enumerate() {
            *bar = 0.25 + 0.6 * wave(1.3 + band as f64 * 0.21, band as f64);
        }
        self.state.voice_probability = wave(0.9, 0.0);
        self.state.denoise_reduction_db = 6.0 + 14.0 * wave(0.7, 1.0);
        self.state.noise_floor_db = -58.0 + 10.0 * wave(0.2, 2.0);
        self.state.gate_reduction_db = 12.0 * (1.0 - self.state.voice_probability);
        self.state.compressor_reduction_db = 5.0 * wave(1.7, 0.5);
        self.state.deesser_reduction_db = 3.0 * wave(3.1, 0.2);
        // Now and then the adaptive de-esser lowers its corner, and the strip says so.
        self.state.deesser_hz = if wave(0.15, 0.0) > 0.7 {
            4_000.0
        } else {
            5_500.0
        };
    }

    /// The stand-in for the application's controller.
    fn handle(&mut self, ctx: &egui::Context, action: &UiAction) {
        match action {
            UiAction::SelectPreset(index) => self.state.selected_preset = Some(*index),
            UiAction::SelectOutput(index) | UiAction::SelectInput(index) => {
                if let Some(direction) = self.state.devices.get(*index).map(|d| d.direction) {
                    self.state.set_selection(direction, Some(*index));
                    self.set_edit_direction(direction);
                }
            }
            UiAction::DetachOutput => self.state.selected_output = None,
            UiAction::DetachInput => self.state.selected_input = None,
            UiAction::SetEditDirection(direction) => self.set_edit_direction(*direction),
            UiAction::DismissNotice => self.state.dismiss_notification(),
            UiAction::SetEffect(effect, value) => self.state.effects[*effect as usize] = *value,
            UiAction::SetBandGain(band, gain) => {
                if let Some(slot) = self.state.eq_bands.get_mut(*band) {
                    slot.boost_db = *gain;
                }
            }
            UiAction::SetBandFrequency(band, hz) => {
                if let Some(slot) = self.state.eq_bands.get_mut(*band) {
                    slot.center_hz = *hz;
                }
            }
            // The solo is heard, not stored: the window draws the walk itself.
            UiAction::SoloBand(_) => {}
            UiAction::SetBandCount(count) => self.set_band_count(*count),
            UiAction::SetMasterGain(db) => self.state.master_gain_db = *db,
            UiAction::SetVolumeLeveling(amount) => self.state.volume_leveling = *amount,
            UiAction::SetFilterQ(q) => self.state.filter_q = *q,
            UiAction::SetBalance(db) => self.state.balance_db = *db,
            UiAction::RestoreDefaults => {
                self.set_band_count(fxsound_core::eq::DEFAULT_BANDS);
                self.state.master_gain_db = 0.0;
                self.state.volume_leveling = 0.0;
                self.state.filter_q = 1.0;
                self.state.balance_db = 0.0;
            }
            UiAction::TogglePower => self.state.power = !self.state.power,
            UiAction::ToggleView => self.toggle_view(ctx),
            UiAction::ToggleTheme => self.toggle_theme(ctx),
            UiAction::Close | UiAction::Minimise => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            UiAction::DragWindow => ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag),
            _ => {}
        }
    }

    /// A new ladder for `count` bands, each taking the gain of the old band nearest it — enough of
    /// the controller's remap to see a curve survive the change.
    fn set_band_count(&mut self, count: usize) {
        let old = std::mem::take(&mut self.state.eq_bands);
        self.state.eq_bands = (0..count)
            .map(|band| {
                let nearest = if count > 1 && !old.is_empty() {
                    (band * (old.len() - 1) + (count - 1) / 2) / (count - 1)
                } else {
                    0
                };
                fxsound_core::EqBand::new(
                    fxsound_ui::widgets::equalizer::default_band_frequency(band, count),
                    old.get(nearest).map_or(0.0, |b| b.boost_db),
                )
            })
            .collect();
    }

    fn toggle_view(&mut self, ctx: &egui::Context) {
        self.state.view = match self.state.view {
            ViewMode::Pro => ViewMode::Lite,
            ViewMode::Lite => ViewMode::Pro,
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(window_size(
            self.state.view,
        )));
    }

    /// The stand-in for the application's side of Settings.
    fn handle_settings(&mut self, ctx: &egui::Context, action: &SettingsAction) {
        let Some(settings) = &mut self.settings else {
            return;
        };
        match action {
            SettingsAction::SelectTab(tab) => settings.tab = *tab,
            SettingsAction::SetAppPreset {
                app,
                direction,
                preset,
            } => {
                let lane = settings
                    .apps
                    .iter_mut()
                    .filter(|row| row.app == *app)
                    .flat_map(|row| row.lanes.iter_mut())
                    .find(|lane| lane.direction == *direction);
                if let Some(lane) = lane {
                    lane.preset.clone_from(preset);
                }
            }
            SettingsAction::ForgetApp(app) => {
                settings.apps.retain(|row| row.app != *app || row.running);
                for row in settings.apps.iter_mut().filter(|row| row.app == *app) {
                    for lane in &mut row.lanes {
                        lane.preset = None;
                    }
                }
            }
            SettingsAction::SetDevicePreset {
                direction,
                device,
                preset,
            } => {
                let (rows, presets) = match direction {
                    DeviceDirection::Output => (&mut settings.devices, &settings.presets),
                    DeviceDirection::Input => (&mut settings.microphones, &settings.input_presets),
                };
                if let Some(row) = rows.get_mut(*device) {
                    row.preset = presets.iter().position(|name| name == preset);
                }
            }
            SettingsAction::MoveDeviceUp(row) if *row > 0 => settings.devices.swap(*row, row - 1),
            SettingsAction::MoveDeviceDown(row) if row + 1 < settings.devices.len() => {
                settings.devices.swap(*row, row + 1);
            }
            SettingsAction::MoveMicrophoneUp(row) if *row > 0 => {
                settings.microphones.swap(*row, row - 1);
            }
            SettingsAction::MoveMicrophoneDown(row) if row + 1 < settings.microphones.len() => {
                settings.microphones.swap(*row, row + 1);
            }
            SettingsAction::SelectDeviceRow(row) => settings.selected_device = Some(*row),
            SettingsAction::SetWindowsParity(level) => {
                settings.settings.windows_parity = *level;
                self.state.windows_parity = *level;
                // The level decides the popups' and tooltips' edges too (`Palette::windows`).
                theme::apply(ctx, self.palette());
            }
            SettingsAction::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
    }

    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.state.theme = match self.state.theme {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        };
        theme::apply(ctx, self.palette());
        self.assets.clear();
    }
}

impl eframe::App for Preview {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Fully transparent: the rounded window is painted by the view, so the corners must not
        // be filled by the backend.
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let (time, keys) = ctx.input(|input| {
            let pressed = |key| input.key_pressed(key);
            (
                input.time,
                [
                    pressed(egui::Key::I),
                    pressed(egui::Key::N),
                    pressed(egui::Key::L),
                    pressed(egui::Key::T),
                    pressed(egui::Key::Escape),
                ],
            )
        });
        let [edit, notice, view, palette, quit] = keys;
        if edit {
            let other = match self.state.direction {
                DeviceDirection::Output => DeviceDirection::Input,
                DeviceDirection::Input => DeviceDirection::Output,
            };
            self.set_edit_direction(other);
        }
        if notice {
            self.state.notify(DEFAULT_NOTICE);
        }
        if view && self.settings.is_none() {
            self.toggle_view(&ctx);
        }
        if palette {
            self.toggle_theme(&ctx);
        }
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        self.animate(time);
        self.state.expire_notification(std::time::Instant::now());

        if let Some(settings) = &self.settings {
            let outer = egui::Rect::from_min_size(ui.max_rect().min, settings::WINDOW_SIZE);
            let actions = SettingsDialog::new(settings)
                .show(ui, outer, self.palette(), &mut self.assets, &mut self.icons)
                .actions;
            for action in &actions {
                self.handle_settings(&ctx, action);
            }
            self.frames += 1;
            if self.exit_after_paint && self.frames > 120 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
            return;
        }

        let palette = self.palette();
        let response = views::show(
            ui,
            &self.state,
            &mut self.scratch,
            palette,
            &mut self.assets,
        );
        for action in &response.actions {
            self.handle(&ctx, action);
        }
        if let Some(template) = &self.message {
            let text = fxsound_ui::dialogs::message::message_with_name(
                &ctx,
                &fxsound_core::i18n::tr(template),
                "Rock Ballad Extended Night",
            );
            if fxsound_ui::dialogs::MessageBox::new(&text)
                .show_modal(&ctx, self.palette(), &mut self.assets, "preview.message")
                .is_some()
            {
                self.message = None;
            }
        }

        self.frames += 1;
        // Exit on its own so a screenshot run cannot leave a window behind.
        if self.exit_after_paint && self.frames > 120 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}
