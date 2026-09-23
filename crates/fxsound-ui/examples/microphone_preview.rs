//! A standing visual check of Settings ▸ Microphone and the calibration wizard, with no audio.
//!
//! Opens a frameless window the size of the Settings dialog and draws it over a made-up
//! [`SettingsState`], with the wizard on top when asked. A few lines at the bottom stand in for
//! the application, so the steppers, the checkbox and the wizard's buttons can be clicked
//! through. It never touches PipeWire.
//!
//! ```text
//! cargo run -p fxsound-ui --example microphone_preview
//! cargo run -p fxsound-ui --example microphone_preview -- --light --language=ru --wizard=result
//! ```
//!
//! Flags: `--light`, `--language=CODE`, `--wizard[=intro|silence|speech|loud|analysing|result|
//! failed]`, `--no-microphone`, `--cannot-measure` (the wizard as a host without an input lane
//! shows it: Start and Retry disabled), `--exit-after-paint`. Keys while it runs: `W` opens or
//! closes the wizard, `P` steps it to its next phase, `T` flips the palette, `Esc` quits.

use eframe::egui;
use fxsound_core::settings::CalibrationRecord;
use fxsound_core::{Settings, ThemeMode, i18n};
use fxsound_ui::dialogs::{
    CalibrationAction, CalibrationDialog, CalibrationPhase, CalibrationResultView, CalibrationView,
    NavIcons, SettingsAction, SettingsDialog, SettingsState, SettingsTab, settings,
};
use fxsound_ui::{AssetCache, Palette, theme};

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter().find_map(|a| {
            a.strip_prefix(name)
                .map(|rest| rest.trim_start_matches('='))
        })
    };

    if let Some(code) = value("--language") {
        i18n::set_language(code);
    }
    let theme = if flag("--light") {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    let can_measure = !flag("--cannot-measure");
    let wizard = value("--wizard").map(|phase| {
        let phase = CalibrationPhase::ALL
            .into_iter()
            .find(|p| format!("{p:?}").eq_ignore_ascii_case(phase))
            .unwrap_or_default();
        demo_view(phase, can_measure)
    });

    let mut state = demo_state();
    state.has_microphone = !flag("--no-microphone");
    let preview = Preview {
        state,
        wizard,
        can_measure,
        theme,
        assets: AssetCache::new(),
        icons: NavIcons::new(),
        exit_after_paint: flag("--exit-after-paint"),
        frames: 0,
    };

    let size = settings::WINDOW_SIZE;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([size.x, size.y])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_app_id("com.fxsound.FxSound.microphone-preview"),
        ..Default::default()
    };
    eframe::run_native(
        "FxSound microphone preview",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx, Palette::new(preview.theme));
            Ok(Box::new(preview))
        }),
    )
}

const DEVICE: &str = "fifine Microphone Analogue Stereo";

fn demo_state() -> SettingsState {
    let mut settings = Settings::default();
    settings.echo_cancel = true;
    settings.calibration = Some(CalibrationRecord {
        noise_floor_db: -48.2,
        speech_rms_db: -19.4,
        speech_peak_db: -6.0,
        clipped_ratio: 0.0,
        unix_time: 1_790_121_600,
        preset: format!("Calibrated — {DEVICE}"),
        device: "alsa_input.usb-fifine".to_owned(),
    });
    SettingsState {
        tab: SettingsTab::Microphone,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        echo_cancel_detail: "libspa-aec-webrtc is not installed".to_owned(),
        ..SettingsState::new(settings)
    }
}

fn demo_view(phase: CalibrationPhase, can_measure: bool) -> CalibrationView {
    CalibrationView {
        phase,
        seconds_left: phase.seconds() * 0.6,
        phase_fraction: 0.4,
        level_db: -27.0,
        result: (phase == CalibrationPhase::Result).then(|| CalibrationResultView {
            floor_db: -48.2,
            speech_rms_db: -19.4,
            speech_peak_db: -6.0,
            clipped_percent: 0.0,
            preset: "Headset".to_owned(),
            // The application's own keys (`fxsound-app/src/calibration.rs`), at the widest values
            // they take, so `--language` shows what a real result looks like.
            lines: vec![
                i18n::tr_args("High-pass %s", &["120 Hz"]),
                i18n::tr_args("Gate %s", &["−90 dB"]),
                i18n::tr_args("Compressor %s, ratio %s", &["−60 dB", "3:1"]),
                i18n::tr_args("Makeup gain %s", &["+18 dB"]),
                i18n::tr_args("Ceiling %s", &["−6 dB"]),
                i18n::tr_args("Noise suppression: %s", &[&i18n::tr("Medium")]),
            ],
        }),
        failure: i18n::tr("No speech was heard. Speak closer to the microphone."),
        can_measure,
        ..CalibrationView::intro(DEVICE)
    }
}

struct Preview {
    state: SettingsState,
    wizard: Option<CalibrationView>,
    /// Stands in for the application having an input lane and a calibration state machine.
    can_measure: bool,
    theme: ThemeMode,
    assets: AssetCache,
    icons: NavIcons,
    exit_after_paint: bool,
    frames: u32,
}

impl Preview {
    /// The stand-in for the application's controller.
    fn handle(&mut self, ctx: &egui::Context, action: &SettingsAction) {
        let settings = &mut self.state.settings;
        match action {
            SettingsAction::SelectTab(tab) => self.state.tab = *tab,
            SettingsAction::SetNoiseSuppression(v) => settings.noise_suppression = *v,
            SettingsAction::SetDenoiseChannels(v) => settings.denoise_channels = *v,
            SettingsAction::SetDeEsserMode(v) => settings.deesser_mode = *v,
            SettingsAction::SetDereverb(v) => settings.dereverb = *v,
            SettingsAction::SetEchoCancel(on) => settings.echo_cancel = *on,
            SettingsAction::OpenCalibration => self.wizard = Some(self.intro()),
            SettingsAction::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
    }

    fn intro(&self) -> CalibrationView {
        demo_view(CalibrationPhase::Intro, self.can_measure)
    }

    fn next_phase(&mut self) {
        if let Some(view) = &self.wizard {
            let all = CalibrationPhase::ALL;
            let at = all.iter().position(|p| *p == view.phase).unwrap_or(0);
            self.wizard = Some(demo_view(all[(at + 1) % all.len()], self.can_measure));
        }
    }
}

impl eframe::App for Preview {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let (wizard_key, phase_key, theme_key, quit) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::W),
                i.key_pressed(egui::Key::P),
                i.key_pressed(egui::Key::T),
                i.key_pressed(egui::Key::Escape) && self.wizard.is_none(),
            )
        });
        if wizard_key {
            self.wizard = match self.wizard {
                Some(_) => None,
                None => Some(self.intro()),
            };
        }
        if phase_key {
            self.next_phase();
        }
        if theme_key {
            self.theme = match self.theme {
                ThemeMode::Dark => ThemeMode::Light,
                ThemeMode::Light => ThemeMode::Dark,
            };
            theme::apply(&ctx, Palette::new(self.theme));
            self.assets.clear();
        }
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let palette = Palette::new(self.theme);
        let window = ui.max_rect();
        let outer = egui::Rect::from_min_size(window.min, settings::WINDOW_SIZE);
        let response = SettingsDialog::new(&self.state).show(
            ui,
            outer,
            palette,
            &mut self.assets,
            &mut self.icons,
        );
        if self.wizard.is_none() {
            for action in &response.actions {
                self.handle(&ctx, action);
            }
        }

        if let Some(view) = self.wizard.clone() {
            // The application's backdrop, so the wizard reads as modal over Settings.
            ui.painter().rect_filled(
                outer.shrink(5.0),
                egui::CornerRadius::same(21),
                egui::Color32::from_black_alpha(96),
            );
            let wizard = egui::Rect::from_center_size(outer.center(), view.window_size());
            let response = CalibrationDialog::new(&view).show(
                ui,
                wizard,
                palette,
                &mut self.assets,
                "preview",
            );
            for action in &response.actions {
                match action {
                    CalibrationAction::Cancel | CalibrationAction::Close => self.wizard = None,
                    CalibrationAction::Start | CalibrationAction::Retry => {
                        self.wizard = Some(demo_view(CalibrationPhase::Silence, self.can_measure));
                    }
                    CalibrationAction::Apply => self.wizard = None,
                }
            }
        }

        self.frames += 1;
        if self.exit_after_paint && self.frames > 120 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}
