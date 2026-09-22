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
//! ```
//!
//! Flags: `--light`, `--input` (edit the microphone lane), `--lite`, `--notice[=TEXT]`,
//! `--detached` (both lanes off), `--lang=CODE` (one of the translation tables' codes; English
//! otherwise), `--exit-after-paint`. Keys while it runs: `I` switches the edit direction, `N` puts
//! a notice up, `L` flips Pro/Lite, `T` flips the palette, `Esc` quits.

use eframe::egui;
use fxsound_core::{AudioDevice, DeviceDirection, ThemeMode, ViewMode};
use fxsound_ui::state::PresetEntry;
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
    let mut preview = Preview::new(state, flag("--exit-after-paint"));
    if flag("--input") {
        preview.set_edit_direction(DeviceDirection::Input);
    }
    if let Some(text) = notice {
        preview.state.notify(text);
    }

    let size = window_size(preview.state.view);
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
            theme::apply(&cc.egui_ctx, Palette::new(preview.state.theme));
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
        ..UiState::default()
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
    /// The preset list and selection of the lane not being edited.
    parked: (Vec<PresetEntry>, Option<usize>),
    exit_after_paint: bool,
    frames: u32,
}

impl Preview {
    fn new(state: UiState, exit_after_paint: bool) -> Self {
        Self {
            state,
            scratch: ViewScratch::new(),
            assets: AssetCache::new(),
            parked: (input_presets(), Some(0)),
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

    fn toggle_view(&mut self, ctx: &egui::Context) {
        self.state.view = match self.state.view {
            ViewMode::Pro => ViewMode::Lite,
            ViewMode::Lite => ViewMode::Pro,
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(window_size(
            self.state.view,
        )));
    }

    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.state.theme = match self.state.theme {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        };
        theme::apply(ctx, Palette::new(self.state.theme));
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
        if view {
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

        let response = views::show(
            ui,
            &self.state,
            &mut self.scratch,
            Palette::new(self.state.theme),
            &mut self.assets,
        );
        for action in &response.actions {
            self.handle(&ctx, action);
        }

        self.frames += 1;
        // Exit on its own so a screenshot run cannot leave a window behind.
        if self.exit_after_paint && self.frames > 120 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}
