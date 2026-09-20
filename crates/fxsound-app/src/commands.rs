//! Turning a parsed command line into controller actions.
//!
//! The same path serves three callers: the cold start, a second invocation forwarded over the
//! control socket, and a compositor keybind (which is just the second case). That is deliberate —
//! `hyprctl`-driven shortcuts and `fxsound --bass 8` must mean exactly the same thing, and the
//! original has the same property through its `applyConfig` (`FxController.cpp:344-602`).
//!
//! Ordering inside a single invocation is the original's and is load-bearing; [`crate::cli::Cli`]
//! already emits the commands in that order, so this module only has to execute them in sequence.

use crate::app::App;
use crate::cli::{Command, OutputCommand, PowerCommand, PresetCommand, WindowCommand};
use fxsound_core::{DeviceDirection, Effect, ThemeMode, ViewMode, eq};
use fxsound_ui::UiAction;

/// What executing a command asks the window layer to do.
///
/// The controller itself has no window, so anything window-shaped comes back here for `main` to
/// carry out against the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowRequest {
    pub show: bool,
    pub hide: bool,
    pub toggle: bool,
    pub quit: bool,
}

impl WindowRequest {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.show && !self.hide && !self.toggle && !self.quit
    }

    fn merge(&mut self, other: Self) {
        self.show |= other.show;
        self.hide |= other.hide;
        self.toggle |= other.toggle;
        self.quit |= other.quit;
    }
}

/// The result of running one command line.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// Text for the invoking process's stdout — only `--status` produces any.
    pub stdout: String,
    /// Text for its stderr: why a command did nothing.
    pub stderr: String,
    /// Whether the invoking process should exit non-zero.
    ///
    /// A command that silently does nothing is the worst of the three possible outcomes, and it is
    /// what `--output` used to do before the device list had arrived. Anything scripted — a
    /// systemd unit, a compositor keybind at login, an autostart entry — cannot tell that apart
    /// from success.
    pub failed: bool,
    /// What the window should do afterwards.
    pub window: WindowRequest,
}

/// Execute a whole command line against the controller.
pub fn run(app: &mut App, commands: &[Command]) -> Outcome {
    let mut outcome = Outcome::default();
    for command in commands {
        let one = run_one(app, command);
        if !one.stderr.is_empty() {
            if !outcome.stderr.is_empty() {
                outcome.stderr.push('\n');
            }
            outcome.stderr.push_str(&one.stderr);
        }
        outcome.failed |= one.failed;
        if !one.stdout.is_empty() {
            if !outcome.stdout.is_empty() {
                outcome.stdout.push('\n');
            }
            outcome.stdout.push_str(&one.stdout);
        }
        outcome.window.merge(one.window);
    }
    outcome
}

fn run_one(app: &mut App, command: &Command) -> Outcome {
    let mut outcome = Outcome::default();

    match command {
        Command::Status => outcome.stdout = status_report(app),

        Command::Power(power) => {
            let want = match power {
                PowerCommand::On => true,
                PowerCommand::Off => false,
                PowerCommand::Toggle => !app.state.power,
            };
            if want != app.state.power {
                app.handle(&[UiAction::TogglePower]);
                // `TRANS("FxSound is %s.")` is the command-line path's toast (`FxController.cpp:1933`).
                app.notify_power();
            }
        }

        Command::Preset(preset) => run_preset(app, preset),

        Command::Output(OutputCommand::Select(name)) => {
            // Match on the stable node name first, then on what the user actually sees, because a
            // person typing this will copy the description out of the combo box. The list carries
            // both directions, so `--output` can name a microphone too; that is deliberate — it is
            // the one CLI path into the input mode (`docs/spec/12-audio-io.md` §28) — and the
            // node-name-first order is what keeps a USB microphone's twin sink and source apart
            // when their descriptions are identical.
            let index = app
                .state
                .devices
                .iter()
                .position(|d| d.name == *name)
                .or_else(|| {
                    app.state
                        .devices
                        .iter()
                        .position(|d| d.description == *name)
                });
            if let Some(index) = index {
                app.handle(&[UiAction::SelectDevice(index)]);
            } else if app.has_seen_devices() {
                // The list exists and nothing in it is called that. Say so, and exit non-zero:
                // a script that asked for a device it did not get has to be able to find out.
                return Outcome {
                    stderr: format!("no audio device is called {name:?}"),
                    failed: true,
                    ..Outcome::default()
                };
            } else {
                // The list has not arrived yet. This is the common case for anything that runs at
                // login — the control socket answers as soon as the GUI thread is up, which is
                // before PipeWire has finished enumerating — and a name that matches nothing in an
                // *empty* list is not the same as a name that matches nothing. Hold it until the
                // list exists rather than failing a command that is about to become valid.
                app.select_device_when_listed(name);
            }
        }
        Command::Output(OutputCommand::Next) => {
            if let Some(next) = next_device_in_direction(app) {
                app.handle(&[UiAction::SelectDevice(next)]);
            }
        }

        Command::NumBands(count) => {
            app.handle(&[UiAction::SetBandCount(*count as usize)]);
        }
        Command::VolumeLeveling(amount) => {
            app.handle(&[UiAction::SetVolumeLeveling(*amount)]);
        }
        Command::Balance(db) => app.handle(&[UiAction::SetBalance(*db)]),
        Command::FilterQ(q) => app.handle(&[UiAction::SetFilterQ(*q)]),
        Command::MasterGain(db) => app.handle(&[UiAction::SetMasterGain(*db)]),

        Command::View(view) => {
            if *view != app.state.view {
                app.handle(&[UiAction::ToggleView]);
            }
        }

        Command::Language(code) => app.set_language(code),

        Command::Window(window) => {
            outcome.window = match window {
                WindowCommand::Show => WindowRequest {
                    show: true,
                    ..WindowRequest::default()
                },
                WindowCommand::Hide => WindowRequest {
                    hide: true,
                    ..WindowRequest::default()
                },
                WindowCommand::Toggle => WindowRequest {
                    toggle: true,
                    ..WindowRequest::default()
                },
            };
        }

        // The original drops the whole list when it is longer than the live band count
        // (`FxController.cpp:536-553`) rather than applying a prefix, so that a command line
        // written for a 31-band layout cannot half-apply to a 10-band one.
        Command::BandFrequencies(pairs) => {
            if pairs
                .iter()
                .all(|(band, _)| *band < app.state.eq_bands.len())
            {
                let actions: Vec<_> = pairs
                    .iter()
                    .map(|(band, hz)| UiAction::SetBandFrequency(*band, *hz))
                    .collect();
                app.handle(&actions);
            }
        }
        Command::BandGains(pairs) => {
            if pairs
                .iter()
                .all(|(band, _)| *band < app.state.eq_bands.len())
            {
                let actions: Vec<_> = pairs
                    .iter()
                    .map(|(band, db)| UiAction::SetBandGain(*band, *db))
                    .collect();
                app.handle(&actions);
            }
        }
        Command::Effects(pairs) => {
            let actions: Vec<_> = pairs
                .iter()
                .map(|(effect, value)| UiAction::SetEffect(*effect, *value))
                .collect();
            app.handle(&actions);
        }

        Command::Quit => {
            outcome.window.quit = true;
        }
    }

    outcome
}

/// The device `--next-output` should move to: the next one *of the current direction*, wrapping,
/// or `None` when that direction has fewer than two devices.
///
/// The current direction is the selected device's, or `Output` when nothing is selected. A
/// compositor keybind cycles through speakers, or through microphones — never from one into the
/// other, because flipping the whole engine into input mode from a hotkey meant for "next output"
/// would be a surprise no user asked for (`docs/spec/12-audio-io.md` §28.5).
fn next_device_in_direction(app: &App) -> Option<usize> {
    let devices = &app.state.devices;
    let direction = app
        .state
        .device()
        .map_or(DeviceDirection::Output, |d| d.direction);
    let candidates: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.direction == direction)
        .map(|(i, _)| i)
        .collect();
    if candidates.len() < 2 {
        return None;
    }
    let position = app
        .state
        .selected_device
        .and_then(|selected| candidates.iter().position(|&i| i == selected));
    Some(match position {
        Some(position) => candidates[(position + 1) % candidates.len()],
        None => candidates[0],
    })
}

fn run_preset(app: &mut App, command: &PresetCommand) {
    match command {
        PresetCommand::Select(name) => {
            if let Some(index) = app.state.presets.iter().position(|p| p.name == *name) {
                app.handle(&[UiAction::SelectPreset(index)]);
            }
        }
        PresetCommand::SaveAs(name) => app.handle(&[UiAction::SavePresetAs(name.clone())]),
        PresetCommand::Overwrite => app.handle(&[UiAction::SavePreset]),
        PresetCommand::Undo => app.handle(&[UiAction::UndoPresetChanges]),
        PresetCommand::Rename(name) => {
            // Rename is save-under-the-new-name followed by deleting the old one, which is what
            // the original does through its preset list rather than a filesystem rename.
            let old = app.state.preset().map(|p| p.name.clone());
            app.handle(&[UiAction::SavePresetAs(name.clone())]);
            if let Some(old) = old
                && old != *name
                && let Some(index) = app.state.presets.iter().position(|p| p.name == old)
            {
                let previous = app.state.selected_preset;
                app.state.selected_preset = Some(index);
                app.handle(&[UiAction::DeletePreset]);
                if let Some(index) = app.state.presets.iter().position(|p| p.name == *name) {
                    app.handle(&[UiAction::SelectPreset(index)]);
                } else {
                    app.state.selected_preset = previous;
                }
            }
        }
        PresetCommand::Delete => app.handle(&[UiAction::DeletePreset]),
        PresetCommand::Next => app.cycle_preset(true),
        PresetCommand::Previous => app.cycle_preset(false),
    }
}

/// What `--status` prints.
///
/// Plain `key: value` lines so it can be grepped from a shell script or a compositor rule, which
/// is the only reason anyone runs it.
fn status_report(app: &App) -> String {
    use std::fmt::Write as _;
    let state = &app.state;
    let mut out = String::new();

    let _ = writeln!(out, "power: {}", if state.power { "on" } else { "off" });
    let _ = writeln!(
        out,
        "audio: {}",
        if !app.has_audio() {
            "unavailable"
        } else if state.audio_active {
            "processing"
        } else {
            "idle"
        }
    );
    let _ = writeln!(out, "preset: {}", state.preset_label());
    // `output:` keeps its name for the scripts that already grep it; `direction:` says whether
    // that device is the speakers FxSound renders to or the microphone it listens to.
    let _ = writeln!(
        out,
        "output: {}",
        state.device().map_or("(none)", |d| d.description.as_str())
    );
    let _ = writeln!(
        out,
        "direction: {}",
        match state
            .device()
            .map_or(DeviceDirection::Output, |d| d.direction)
        {
            DeviceDirection::Output => "output",
            DeviceDirection::Input => "input",
        }
    );
    let _ = writeln!(
        out,
        "view: {}",
        match state.view {
            ViewMode::Pro => "pro",
            ViewMode::Lite => "lite",
        }
    );
    let _ = writeln!(
        out,
        "theme: {}",
        match state.theme {
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
        }
    );

    for effect in Effect::ALL {
        let _ = writeln!(out, "{}: {:.0}", effect.key(), state.effect(effect));
    }

    let _ = writeln!(out, "master_gain: {:.0} dB", state.master_gain_db);
    let _ = writeln!(out, "balance: {:.0} dB", state.balance_db);
    let _ = writeln!(out, "volume_leveling: {:.1}", state.volume_leveling);
    let _ = writeln!(out, "filter_q: {:.1}", state.filter_q);
    let _ = writeln!(
        out,
        "eq: {} ({} bands, max {})",
        if state.eq_on { "on" } else { "off" },
        state.eq_bands.len(),
        eq::MAX_BANDS
    );

    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::AudioDevice;

    fn app() -> App {
        App::headless_for_tests()
    }

    #[test]
    fn power_on_and_off_are_idempotent() {
        let mut a = app();
        run(&mut a, &[Command::Power(PowerCommand::On)]);
        assert!(a.state.power);
        run(&mut a, &[Command::Power(PowerCommand::On)]);
        assert!(
            a.state.power,
            "a second --power=on must not toggle it back off"
        );

        run(&mut a, &[Command::Power(PowerCommand::Off)]);
        assert!(!a.state.power);
        run(&mut a, &[Command::Power(PowerCommand::Toggle)]);
        assert!(a.state.power);
    }

    #[test]
    fn effects_arrive_on_the_gui_scale_and_reach_the_dsp_snapshot() {
        let mut a = app();
        run(
            &mut a,
            &[Command::Effects(vec![
                (Effect::Bass, 10.0),
                (Effect::Fidelity, 5.0),
            ])],
        );
        assert_eq!(a.state.effect(Effect::Bass), 10.0);
        assert!((a.params().effect(Effect::Bass) - 1.0).abs() < 1e-6);
        assert!((a.params().effect(Effect::Fidelity) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_band_list_longer_than_the_layout_is_dropped_whole() {
        let mut a = app();
        assert_eq!(a.state.eq_bands.len(), 10);
        run(&mut a, &[Command::BandGains(vec![(0, 6.0), (30, 6.0)])]);
        assert!(
            a.state.eq_bands.iter().all(|b| b.boost_db == 0.0),
            "band 30 does not exist, so the whole list must be refused"
        );

        // The same list applies once the layout is big enough, and --num_bands comes first.
        run(
            &mut a,
            &[
                Command::NumBands(31),
                Command::BandGains(vec![(0, 6.0), (30, 6.0)]),
            ],
        );
        assert_eq!(a.state.eq_bands.len(), 31);
        assert_eq!(a.state.eq_bands[0].boost_db, 6.0);
        assert_eq!(a.state.eq_bands[30].boost_db, 6.0);
    }

    #[test]
    fn window_commands_come_back_as_a_request_rather_than_acting() {
        let mut a = app();
        let outcome = run(&mut a, &[Command::Window(WindowCommand::Toggle)]);
        assert!(outcome.window.toggle);
        assert!(!outcome.window.quit);

        let outcome = run(&mut a, &[Command::Quit]);
        assert!(outcome.window.quit);
    }

    #[test]
    fn status_reports_the_live_state_as_greppable_lines() {
        let mut a = app();
        run(&mut a, &[Command::Effects(vec![(Effect::Bass, 7.0)])]);
        let outcome = run(&mut a, &[Command::Status]);

        assert!(outcome.stdout.contains("power: on"));
        assert!(outcome.stdout.contains("bass: 7"));
        assert!(outcome.stdout.contains("audio: unavailable"));
        // Every line is `key: value`.
        assert!(outcome.stdout.lines().all(|l| l.contains(": ")));
    }

    #[test]
    fn selecting_an_output_matches_the_node_name_or_the_description() {
        let mut a = app();
        a.state.devices = vec![
            AudioDevice {
                id: 1,
                name: "alsa_output.pci-0000_00_1f.3".into(),
                description: "Built-in Speakers".into(),
                is_default: true,
                direction: fxsound_core::DeviceDirection::Output,
                form_factor: "speaker".into(),
            },
            AudioDevice {
                id: 2,
                name: "bluez_output.AA_BB".into(),
                description: "Headphones".into(),
                is_default: false,
                direction: fxsound_core::DeviceDirection::Output,
                form_factor: "speaker".into(),
            },
        ];

        run(
            &mut a,
            &[Command::Output(OutputCommand::Select("Headphones".into()))],
        );
        assert_eq!(a.state.selected_device, Some(1));

        run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_output.pci-0000_00_1f.3".into(),
            ))],
        );
        assert_eq!(a.state.selected_device, Some(0));

        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_device, Some(1));
    }

    fn device(
        id: u32,
        name: &str,
        description: &str,
        direction: fxsound_core::DeviceDirection,
    ) -> AudioDevice {
        AudioDevice {
            id,
            name: name.into(),
            description: description.into(),
            is_default: false,
            direction,
            form_factor: "speaker".into(),
        }
    }

    /// The development machine's graph: two sinks and two sources, with the USB microphone's sink
    /// and source sharing one description — published outputs first, then inputs, as the engine
    /// groups them.
    fn mixed_devices() -> Vec<AudioDevice> {
        use fxsound_core::DeviceDirection::{Input, Output};
        vec![
            device(
                57,
                "alsa_output.pci",
                "Ryzen HD Audio Controller Analogue Stereo",
                Output,
            ),
            device(
                55,
                "alsa_output.usb-fifine",
                "fifine Microphone Analogue Stereo",
                Output,
            ),
            device(
                56,
                "alsa_input.usb-fifine",
                "fifine Microphone Analogue Stereo",
                Input,
            ),
            device(
                58,
                "alsa_input.pci",
                "Ryzen HD Audio Controller Analogue Stereo",
                Input,
            ),
        ]
    }

    #[test]
    fn selecting_by_name_reaches_a_microphone_and_by_description_prefers_the_output() {
        use fxsound_core::DeviceDirection::{Input, Output};
        let mut a = app();
        a.state.devices = mixed_devices();

        // The node name is unambiguous and can name a source: this is the CLI's way into the
        // input mode.
        run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_input.usb-fifine".into(),
            ))],
        );
        assert_eq!(a.state.selected_device, Some(2));
        assert_eq!(a.state.device().map(|d| d.direction), Some(Input));

        // The shared description matches the first entry in list order — an output, because the
        // engine publishes outputs first — so a description alone never flips the mode by accident.
        run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "fifine Microphone Analogue Stereo".into(),
            ))],
        );
        assert_eq!(a.state.selected_device, Some(1));
        assert_eq!(a.state.device().map(|d| d.direction), Some(Output));
    }

    #[test]
    fn next_output_cycles_within_the_current_direction_only() {
        let mut a = app();
        a.state.devices = mixed_devices();

        // Nothing selected: the first *output*, never an input.
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_device, Some(0));
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_device, Some(1));
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(
            a.state.selected_device,
            Some(0),
            "wraps among the outputs, skipping the inputs"
        );

        // In input mode the same keybind cycles among the microphones.
        run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_input.usb-fifine".into(),
            ))],
        );
        assert_eq!(a.state.selected_device, Some(2));
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_device, Some(3));
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(
            a.state.selected_device,
            Some(2),
            "wraps among the inputs, never back to a sink"
        );

        // A lone device in the current direction is a no-op, as a lone device always was.
        a.state.devices.truncate(3);
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_device, Some(2));
    }

    #[test]
    fn status_reports_the_direction_of_the_selected_device() {
        let mut a = app();
        a.state.devices = mixed_devices();
        let outcome = run(&mut a, &[Command::Status]);
        assert!(outcome.stdout.contains("output: (none)"));
        assert!(
            outcome.stdout.contains("direction: output"),
            "{}",
            outcome.stdout
        );

        run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_input.pci".into(),
            ))],
        );
        let outcome = run(&mut a, &[Command::Status]);
        assert!(
            outcome
                .stdout
                .contains("output: Ryzen HD Audio Controller Analogue Stereo")
        );
        assert!(
            outcome.stdout.contains("direction: input"),
            "{}",
            outcome.stdout
        );
    }

    #[test]
    fn an_unknown_output_name_changes_nothing() {
        let mut a = app();
        run(
            &mut a,
            &[Command::Output(OutputCommand::Select("nope".into()))],
        );
        assert!(a.state.selected_device.is_none());
    }

    #[test]
    fn the_view_command_only_switches_when_it_differs() {
        let mut a = app();
        assert_eq!(a.state.view, ViewMode::Pro);
        run(&mut a, &[Command::View(ViewMode::Pro)]);
        assert_eq!(
            a.state.view,
            ViewMode::Pro,
            "asking for the current view is a no-op"
        );
        run(&mut a, &[Command::View(ViewMode::Lite)]);
        assert_eq!(a.state.view, ViewMode::Lite);
    }

    #[test]
    fn the_level_commands_are_clamped_like_the_sliders() {
        let mut a = app();
        run(
            &mut a,
            &[
                Command::MasterGain(99.0),
                Command::Balance(-99.0),
                Command::FilterQ(9.0),
                Command::VolumeLeveling(9.0),
            ],
        );
        assert_eq!(a.state.master_gain_db, 20.0);
        assert_eq!(a.state.balance_db, -20.0);
        assert_eq!(a.state.filter_q, 3.0);
        assert_eq!(a.state.volume_leveling, 4.0);
    }

    #[test]
    fn several_commands_on_one_line_run_in_order() {
        let mut a = app();
        let outcome = run(
            &mut a,
            &[
                Command::Effects(vec![(Effect::Ambience, 3.0)]),
                Command::MasterGain(-6.0),
                Command::Status,
            ],
        );
        assert_eq!(a.state.effect(Effect::Ambience), 3.0);
        assert_eq!(a.state.master_gain_db, -6.0);
        // Status ran last, so it reports what the earlier commands did.
        assert!(outcome.stdout.contains("ambience: 3"));
        assert!(outcome.stdout.contains("master_gain: -6 dB"));
    }
}
