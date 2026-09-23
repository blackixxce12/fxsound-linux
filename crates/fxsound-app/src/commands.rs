//! Turning a parsed command line into controller actions.
//!
//! The same path serves three callers: the cold start, a second invocation forwarded over the
//! control socket, and a compositor keybind (which is just the second case). That is deliberate —
//! `hyprctl`-driven shortcuts and `fxsound --bass 8` must mean exactly the same thing, and the
//! original has the same property through its `applyConfig` (`FxController.cpp:344-602`).
//!
//! Ordering inside a single invocation is the original's, but for the devices coming before the
//! preset (see [`crate::cli::Cli::commands`]), and is load-bearing; [`crate::cli::Cli`] already
//! emits the commands in that order, so this module only has to execute them in sequence.
//!
//! It also owns what `--status` reports: [`StatusDocument`], which the event stream and D-Bus
//! send as well.

use crate::app::{App, detach, select_on};
use crate::cli::{Command, DeviceCommand, PowerCommand, PresetCommand, WindowCommand};
use fxsound_core::{AudioDevice, DeviceDirection, Effect, ThemeMode, ViewMode, eq};
use fxsound_ui::{UiAction, state::UiState};
use serde::Serialize;

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
    /// Text for the invoking process's stdout — only `--status` and `--self-test` produce any.
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

impl Outcome {
    /// A refusal: `message` on stderr and a non-zero exit.
    fn refused(message: impl Into<String>) -> Self {
        Self {
            stderr: message.into(),
            failed: true,
            ..Self::default()
        }
    }
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

/// The self-test's report as a command's answer: on stdout, and failed when a check failed. The
/// environment is a parameter so a test can point the checks at a scratch prefix instead of this
/// host's installation, PipeWire socket and session bus.
fn self_test_outcome(env: &crate::selftest::Environment, json: bool) -> Outcome {
    let report = crate::selftest::run(env);
    Outcome {
        stdout: report.render(json),
        failed: !report.ok(),
        ..Outcome::default()
    }
}

fn run_one(app: &mut App, command: &Command) -> Outcome {
    let mut outcome = Outcome::default();

    match command {
        Command::Status { json } => {
            outcome.stdout = if *json {
                status_json(app)
            } else {
                status_report(app)
            };
        }

        // The stream is served by the control socket itself, which hands a frame with `watch`
        // set to a broadcaster instead of to this function (0.4.0 design §10). A `Watch` that
        // gets here came in a frame that expects one answer, and one answer is not a stream.
        Command::Watch { .. } => {
            return Outcome::refused(
                "--watch is a stream: subscribe with a watch request on the control socket, as \
                 `fxsound --watch` does",
            );
        }

        // `main` answers this before the lock; a running instance handed one runs the same
        // checks, against its own installation, and answers with the same report.
        Command::SelfTest { json } => {
            return self_test_outcome(&crate::selftest::Environment::detect(), *json);
        }

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

        Command::Preset(preset) => return run_preset(app, preset),

        Command::Output(device) => return run_device(app, DeviceDirection::Output, device),
        Command::Input(device) => return run_device(app, DeviceDirection::Input, device),
        Command::EditDirection(direction) => {
            app.handle(&[UiAction::SetEditDirection(*direction)]);
        }
        Command::NoiseSuppression(choice) => app.set_noise_suppression(*choice),

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

/// How a device name on the command line resolved against the device list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// A device of the lane that was asked for.
    Found(usize),
    /// A device of the *other* direction, and none of the one asked for.
    OtherDirection(usize),
    /// Nothing in the list is called that.
    NotFound,
}

/// Find the device a command line names for `lane`.
///
/// The stable `node.name` first, then what the user actually sees, because a person typing this
/// will copy the description out of the combo box. Within the lane asked for first, and only then
/// in the other direction: a USB microphone publishes a sink and a source under one description,
/// and `--output` naming that description means the sink, `--input` the source. The node name is
/// what keeps the two apart when a script needs the other one.
pub(crate) fn resolve_device(
    devices: &[AudioDevice],
    name: &str,
    lane: DeviceDirection,
) -> Resolved {
    let find = |direction: DeviceDirection| {
        devices
            .iter()
            .position(|d| d.direction == direction && d.name == name)
            .or_else(|| {
                devices
                    .iter()
                    .position(|d| d.direction == direction && d.description == name)
            })
    };
    if let Some(index) = find(lane) {
        Resolved::Found(index)
    } else if let Some(index) = find(lane.other()) {
        Resolved::OtherDirection(index)
    } else {
        Resolved::NotFound
    }
}

/// `--output`, `--input`, `--next-output`, `--next-input` and their `off`.
///
/// Whatever settles a lane's device also drops a name that lane was still waiting for the device
/// list with ([`App::cancel_pending_device`]): the later command is the one that counts, and the
/// list's arrival must not bring the earlier one back.
fn run_device(app: &mut App, lane: DeviceDirection, command: &DeviceCommand) -> Outcome {
    match command {
        DeviceCommand::Detach => {
            app.cancel_pending_device(lane);
            app.handle(&[detach(lane)]);
        }
        // Dropped even when there is nowhere to move yet: `--next-output` says the held name is
        // not the one wanted any more, and the lane stays on what the settings file says.
        DeviceCommand::Next => {
            app.cancel_pending_device(lane);
            if let Some(next) = next_device(app, lane) {
                app.handle(&[select_on(lane, next)]);
            }
        }
        DeviceCommand::Select(name) => match resolve_device(&app.state.devices, name, lane) {
            Resolved::Found(index) => {
                app.cancel_pending_device(lane);
                app.handle(&[select_on(lane, index)]);
            }
            // 0.3.0 had one lane and `--output` was its one way into the microphone; scripts
            // written then still work, and are told what to write now. It is an `--output`, and
            // it settles the microphone's device: both lanes' held names are spent.
            Resolved::OtherDirection(index) if lane == DeviceDirection::Output => {
                app.cancel_pending_device(DeviceDirection::Output);
                app.cancel_pending_device(DeviceDirection::Input);
                app.handle(&[UiAction::SelectInput(index)]);
                return Outcome {
                    stderr: format!(
                        "note: {name:?} is a microphone, so it was selected for the input lane; \
                         --input is the option for that since 0.4.0"
                    ),
                    ..Outcome::default()
                };
            }
            // No such courtesy the other way round: `--input` is new, and a speaker named there
            // is a mistake, not a habit from an older version.
            Resolved::OtherDirection(_) => {
                return Outcome::refused(format!(
                    "{name:?} is a playback device, not a microphone; --output selects it"
                ));
            }
            // The list exists and nothing in it is called that. Say so, and exit non-zero: a
            // script that asked for a device it did not get has to be able to find out.
            Resolved::NotFound if app.has_seen_devices() => {
                return Outcome::refused(match lane {
                    DeviceDirection::Output => format!("no audio device is called {name:?}"),
                    DeviceDirection::Input => format!("no microphone is called {name:?}"),
                });
            }
            // The list has not arrived yet. This is the common case for anything that runs at
            // login — the control socket answers as soon as the GUI thread is up, which is before
            // PipeWire has finished enumerating — and a name that matches nothing in an *empty*
            // list is not the same as a name that matches nothing. Hold it until the list exists
            // rather than failing a command that is about to become valid.
            //
            // The lane becomes the edit direction now, as selecting the device would make it, so
            // that a `--preset` later on the line is looked up in this lane's list; the preset it
            // picks is held with the name and kept when the device is selected.
            Resolved::NotFound => {
                app.handle(&[UiAction::SetEditDirection(lane)]);
                app.select_device_when_listed(name, lane);
            }
        },
    }
    Outcome::default()
}

/// The device `--next-output` or `--next-input` should move `lane` to: the next one of that
/// direction, wrapping, or the first when the lane is detached; `None` when there is nowhere else
/// to go.
///
/// Each keybind cycles its own lane and never crosses into the other: flipping the microphone
/// from a key meant for the speakers would be a surprise no user asked for
/// (`docs/spec/12-audio-io.md` §28.5), and in 0.4.0 the two lanes run side by side, so there is
/// no "current direction" left for one key to follow.
fn next_device(app: &App, lane: DeviceDirection) -> Option<usize> {
    let candidates: Vec<usize> = app
        .state
        .devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.direction == lane)
        .map(|(i, _)| i)
        .collect();
    let position = app
        .state
        .selection(lane)
        .and_then(|selected| candidates.iter().position(|&i| i == selected));
    match position {
        Some(position) => {
            let next = candidates[(position + 1) % candidates.len()];
            // A lone device is a no-op, as a lone device always was.
            (next != candidates[position]).then_some(next)
        }
        None => candidates.first().copied(),
    }
}

fn run_preset(app: &mut App, command: &PresetCommand) -> Outcome {
    match command {
        PresetCommand::Select(name) => {
            if let Some(index) = app.state.presets.iter().position(|p| p.name == *name) {
                app.handle(&[UiAction::SelectPreset(index)]);
            } else {
                // Selected in the edit direction's list and nowhere else (0.4.0 design §1.4), and
                // a name that is not in it is an error for either lane: 0.3.0 exited zero having
                // done nothing, which a script cannot tell from success. A name the other lane has
                // is almost certainly that lane's preset, so the message says how to reach it.
                let lane = app.state.direction;
                let other = lane.other();
                let hint = if app.lane_has_preset(other, name) {
                    format!(
                        "; it is {} preset, so add --edit={} to select it",
                        lane_noun(other),
                        other.key()
                    )
                } else {
                    String::new()
                };
                return Outcome::refused(format!(
                    "no {} preset is called {name:?}{hint}",
                    lane.key()
                ));
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
    Outcome::default()
}

/// "an output" / "an input", for a message about a lane's preset.
const fn lane_noun(lane: DeviceDirection) -> &'static str {
    match lane {
        DeviceDirection::Output => "an output",
        DeviceDirection::Input => "an input",
    }
}

/// Everything `--status` reports, in the shape `--status --json` prints it.
///
/// Every key 0.3.0 printed is still here and still means what it meant, read for the **edit
/// direction** where 0.3.0 had a single selection — `preset`, `device` and `direction` are what the
/// window shows. 0.4.0 adds a block per lane, the microphone's telemetry and the echo canceller.
/// Public so the event stream's `status` event and D-Bus `GetStatus` send this same document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusDocument {
    pub version: &'static str,
    pub power: bool,
    /// The edit direction's lane: `processing`, `idle`, or `unavailable` without an engine.
    pub audio: &'static str,
    /// The edit direction's preset, with `*` when it has unsaved changes, as the combo shows it.
    pub preset: String,
    /// The edit direction's device, by description.
    pub device: Option<String>,
    /// The edit direction. In 0.3.0 this was the selected device's direction — the same thing,
    /// now that there is a selection per lane and the window shows one of them.
    pub direction: &'static str,
    /// The same as `direction`, under the name 0.4.0 gives it.
    pub edit_direction: &'static str,
    pub view: &'static str,
    pub theme: &'static str,
    pub effects: EffectLevels,
    pub master_gain_db: i64,
    pub balance_db: i64,
    pub volume_leveling: f64,
    pub filter_q: f64,
    pub eq: EqStatus,
    pub format: FormatStatus,
    pub ring: RingStatus,
    pub output: LaneStatus,
    pub input: InputLaneStatus,
    pub input_meters: InputMeters,
    pub echo_cancel: EchoCancelStatus,
}

/// The five effect sliders, on their `0..=10` scale, rounded as 0.3.0 printed them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EffectLevels {
    pub fidelity: i64,
    pub ambience: i64,
    pub surround: i64,
    pub dynamic_boost: i64,
    pub bass: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EqStatus {
    pub enabled: bool,
    pub bands: usize,
    pub max_bands: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FormatStatus {
    pub sample_rate: u32,
    pub channels: u16,
}

/// How the ring between the two audio nodes is coping; cumulative since the stream was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RingStatus {
    pub dropped_frames: u64,
    pub underrun_frames: u64,
    pub resyncs: u64,
    pub format_mismatches: u64,
}

/// One lane: its device, its preset and whether it is processing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneStatus {
    /// Whether the lane has a device; `false` after `off`.
    pub enabled: bool,
    /// The device's description, `null` while the lane is detached or the list has not arrived.
    pub device: Option<String>,
    /// The device's `node.name`, which is what `--output`/`--input` and the settings file take.
    pub node_name: Option<String>,
    /// The lane's preset, by the name `--preset` takes (no `*`).
    pub preset: Option<String>,
    /// Whether that preset has unsaved changes.
    pub modified: bool,
    /// Whether the lane is processing audio right now.
    pub active: bool,
}

/// The input lane: a [`LaneStatus`] and the noise suppression in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InputLaneStatus {
    #[serde(flatten)]
    pub lane: LaneStatus,
    /// The Settings override: `preset`, `off`, `light`, `medium` or `strong`.
    pub noise_suppression: &'static str,
    /// The level the denoiser runs at once the override is applied.
    pub denoise_level: &'static str,
}

/// The microphone's telemetry, as the readout strip shows it. Reductions are positive dB.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct InputMeters {
    /// The denoiser's opinion that the last frame was voice, `0..=1`.
    pub voice_probability: f64,
    /// The running noise floor, dBFS; `null` until there has been a measurement.
    pub noise_floor_db: Option<f64>,
    pub denoise_reduction_db: f64,
    pub gate_reduction_db: f64,
    pub compressor_reduction_db: f64,
    pub deesser_reduction_db: f64,
    /// Whether the denoiser is actually processing (on, not `off`, and at 48 kHz).
    pub denoise_running: bool,
    /// Whether the de-esser is actually processing (the source has a sibilance band).
    pub deesser_running: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EchoCancelStatus {
    /// Asked for in Settings.
    pub on: bool,
    /// The module is loaded and its source is present.
    pub running: bool,
}

/// The document `--status` reports, read from the controller.
#[must_use]
pub fn status_document(app: &App) -> StatusDocument {
    let state = &app.state;
    let audio = app.audio_status();
    let edit = state.direction;
    let lane = |direction: DeviceDirection| {
        let device = state.device_for(direction);
        let preset = app.lane_preset(direction);
        LaneStatus {
            enabled: state.lane_enabled(direction),
            device: device.map(|d| d.description.clone()),
            node_name: device.map(|d| d.name.clone()),
            preset: preset.map(|(name, _)| name.to_owned()),
            modified: preset.is_some_and(|(_, modified)| modified),
            active: match direction {
                DeviceDirection::Output => state.output_active,
                DeviceDirection::Input => state.input_active,
            },
        }
    };
    let effect = |effect: Effect| state.effect(effect).round() as i64;

    StatusDocument {
        version: env!("CARGO_PKG_VERSION"),
        power: state.power,
        audio: if !app.has_audio() {
            "unavailable"
        } else if state.audio_active {
            "processing"
        } else {
            "idle"
        },
        preset: state.preset_label(),
        device: state.device().map(|d| d.description.clone()),
        direction: edit.key(),
        edit_direction: edit.key(),
        view: match state.view {
            ViewMode::Pro => "pro",
            ViewMode::Lite => "lite",
        },
        theme: match state.theme {
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
        },
        effects: EffectLevels {
            fidelity: effect(Effect::Fidelity),
            ambience: effect(Effect::Ambience),
            surround: effect(Effect::Surround),
            dynamic_boost: effect(Effect::DynamicBoost),
            bass: effect(Effect::Bass),
        },
        master_gain_db: state.master_gain_db.round() as i64,
        balance_db: state.balance_db.round() as i64,
        volume_leveling: rounded(state.volume_leveling, 1),
        filter_q: rounded(state.filter_q, 1),
        eq: EqStatus {
            enabled: state.eq_on,
            bands: state.eq_bands.len(),
            max_bands: eq::MAX_BANDS,
        },
        format: FormatStatus {
            sample_rate: audio.sample_rate,
            channels: audio.channels,
        },
        ring: RingStatus {
            dropped_frames: audio.dropped_frames,
            underrun_frames: audio.underrun_frames,
            resyncs: audio.resyncs,
            format_mismatches: audio.format_mismatches,
        },
        output: lane(DeviceDirection::Output),
        input: InputLaneStatus {
            lane: lane(DeviceDirection::Input),
            noise_suppression: app.settings().noise_suppression.key(),
            denoise_level: state.denoise_level.key(),
        },
        input_meters: input_meters(state),
        echo_cancel: EchoCancelStatus {
            on: state.echo_cancel_on,
            running: state.echo_cancel_running,
        },
    }
}

/// The microphone's telemetry as `--status` and the `input_meters` event report it.
#[must_use]
pub fn input_meters(state: &UiState) -> InputMeters {
    InputMeters {
        voice_probability: rounded(state.voice_probability, 2),
        // Read as the strip reads it: a floor is a measurement only while the microphone is
        // delivering and once the running minimum has come down from full scale — `0.0` is
        // `Meters::default()`, and a room at 0 dBFS is not a reading anyone will get.
        noise_floor_db: (state.input_active
            && state.noise_floor_db.is_finite()
            && state.noise_floor_db < 0.0)
            .then(|| rounded(state.noise_floor_db, 1)),
        denoise_reduction_db: rounded(state.denoise_reduction_db, 1),
        gate_reduction_db: rounded(state.gate_reduction_db, 1),
        compressor_reduction_db: rounded(state.compressor_reduction_db, 1),
        deesser_reduction_db: rounded(state.deesser_reduction_db, 1),
        denoise_running: state.denoise_running,
        deesser_running: state.deesser_running,
    }
}

/// `value` to `places` decimals, as an `f64` that prints as short as it reads: an `f32` widened
/// first would print `0.9300000071525574`.
fn rounded(value: f32, places: i32) -> f64 {
    let scale = 10_f64.powi(places);
    let value = (f64::from(value) * scale).round() / scale;
    if value.is_finite() { value } else { 0.0 }
}

/// What `--status` prints.
///
/// Plain `key: value` lines so it can be grepped from a shell script or a compositor rule, which
/// is the only reason anyone runs it. Every 0.3.0 line is still here; 0.4.0 adds `version`, a
/// line per lane for the device and the preset, and the microphone's telemetry as `input_*`.
fn status_report(app: &App) -> String {
    use std::fmt::Write as _;
    let doc = status_document(app);
    let mut out = String::new();
    let on_off = |on: bool| if on { "on" } else { "off" };
    let running = |on: bool| if on { "running" } else { "stopped" };
    let or_none = |value: &Option<String>| value.clone().unwrap_or_else(|| "(none)".to_owned());
    let preset = |lane: &LaneStatus| match &lane.preset {
        Some(name) if lane.modified => format!("{name}*"),
        Some(name) => name.clone(),
        None => "(none)".to_owned(),
    };

    let _ = writeln!(out, "version: {}", doc.version);
    let _ = writeln!(out, "power: {}", on_off(doc.power));
    let _ = writeln!(out, "audio: {}", doc.audio);
    let _ = writeln!(out, "preset: {}", doc.preset);
    // `output:` keeps its name for the scripts that already grep it, and now means the output
    // lane's device whichever lane the window shows; the microphone has `input:` beside it.
    let _ = writeln!(out, "output: {}", or_none(&doc.output.device));
    let _ = writeln!(out, "input: {}", or_none(&doc.input.lane.device));
    let _ = writeln!(out, "output_preset: {}", preset(&doc.output));
    let _ = writeln!(out, "input_preset: {}", preset(&doc.input.lane));
    let _ = writeln!(out, "direction: {}", doc.direction);
    let _ = writeln!(out, "view: {}", doc.view);
    let _ = writeln!(out, "theme: {}", doc.theme);

    let effects = doc.effects;
    for (key, value) in [
        (Effect::Fidelity.key(), effects.fidelity),
        (Effect::Ambience.key(), effects.ambience),
        (Effect::Surround.key(), effects.surround),
        (Effect::DynamicBoost.key(), effects.dynamic_boost),
        (Effect::Bass.key(), effects.bass),
    ] {
        let _ = writeln!(out, "{key}: {value}");
    }

    let _ = writeln!(out, "master_gain: {} dB", doc.master_gain_db);
    let _ = writeln!(out, "balance: {} dB", doc.balance_db);
    let _ = writeln!(out, "volume_leveling: {:.1}", doc.volume_leveling);
    let _ = writeln!(out, "filter_q: {:.1}", doc.filter_q);
    let _ = writeln!(
        out,
        "eq: {} ({} bands, max {})",
        on_off(doc.eq.enabled),
        doc.eq.bands,
        doc.eq.max_bands
    );

    // How the ring between the two audio nodes is coping. Collected since the port began and
    // published nowhere until 0.3.0, so nothing could assert on it and nobody whose audio
    // crackled could say how often. All four are cumulative since the stream was built.
    let _ = writeln!(out, "sample_rate: {} Hz", doc.format.sample_rate);
    let _ = writeln!(out, "channels: {}", doc.format.channels);
    let _ = writeln!(
        out,
        "ring: {} dropped, {} underrun, {} resyncs, {} format mismatches",
        doc.ring.dropped_frames,
        doc.ring.underrun_frames,
        doc.ring.resyncs,
        doc.ring.format_mismatches
    );

    // The microphone: what it is set to, and what the strip under the window reads.
    let _ = writeln!(out, "noise_suppression: {}", doc.input.noise_suppression);
    let meters = doc.input_meters;
    let _ = writeln!(
        out,
        "input_denoise: {}, {}",
        doc.input.denoise_level,
        running(meters.denoise_running)
    );
    let _ = writeln!(
        out,
        "input_voice_probability: {:.2}",
        meters.voice_probability
    );
    let _ = writeln!(
        out,
        "input_noise_floor: {}",
        meters
            .noise_floor_db
            .map_or_else(|| "(none)".to_owned(), |db| format!("{db:.1} dB"))
    );
    let _ = writeln!(
        out,
        "input_denoise_reduction: {:.1} dB",
        meters.denoise_reduction_db
    );
    let _ = writeln!(
        out,
        "input_gate_reduction: {:.1} dB",
        meters.gate_reduction_db
    );
    let _ = writeln!(
        out,
        "input_compressor_reduction: {:.1} dB",
        meters.compressor_reduction_db
    );
    let _ = writeln!(
        out,
        "input_deesser_reduction: {:.1} dB, {}",
        meters.deesser_reduction_db,
        running(meters.deesser_running)
    );
    let _ = writeln!(
        out,
        "echo_cancel: {}, {}",
        on_off(doc.echo_cancel.on),
        running(doc.echo_cancel.running)
    );

    out.trim_end().to_owned()
}

/// The same state as one JSON object, for anything that would otherwise parse the lines above.
///
/// The lines stay the default because scripts already grep them. Serialised with `serde_json`
/// since 0.4.0 — 0.3.0 wrote it by hand, which was fine for fifteen flat keys and is not for
/// nested lane objects carrying a device description and a preset name, neither of which is under
/// this program's control.
#[must_use]
pub fn status_json(app: &App) -> String {
    serde_json::to_string(&status_document(app)).unwrap_or_else(|err| {
        // Only a map with non-string keys can fail to serialise, and this document has none.
        log::error!("could not serialise the status document: {err}");
        "{}".to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{InputCommand, OutputCommand};
    use clap::Parser as _;
    use fxsound_core::{AudioDevice, DenoiseLevel, NoiseSuppressionOverride};
    use serde_json::Value;

    fn app() -> App {
        App::headless_for_tests()
    }

    /// A headless app with two `.fac` presets for the speakers and two voice presets for the
    /// microphone. `tag` keeps each test's scratch directory its own: the tests run on threads of
    /// one process.
    fn app_with_presets(tag: &str) -> App {
        let root =
            std::env::temp_dir().join(format!("fxsound-commands-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let factory = root.join("factory");
        std::fs::create_dir_all(&factory).expect("create the preset directory");
        for name in ["Alpha", "Beta"] {
            let preset = fxsound_core::Preset {
                name: name.to_owned(),
                ..fxsound_core::Preset::default()
            };
            fxsound_preset::save(&preset, &factory.join(format!("{name}.fac"))).expect("write");
        }
        let voice = |name: &str| fxsound_preset::input::InputPreset {
            name: name.to_owned(),
            ..fxsound_preset::input::InputPreset::default()
        };
        let mut a = app();
        a.use_presets_for_tests(
            vec![factory],
            root.join("user"),
            vec![voice("Clean Voice"), voice("Flat Voice")],
        );
        a
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
    fn status_reports_how_the_ring_is_coping() {
        // Four counters that were collected correctly since the port began and then thrown away:
        // they crossed no process boundary and appeared in no message, so nothing could assert on
        // them and nobody whose audio crackled could say how often.
        let mut a = app();
        let outcome = run(&mut a, &[Command::Status { json: false }]);
        assert!(
            outcome
                .stdout
                .contains("ring: 0 dropped, 0 underrun, 0 resyncs, 0 format mismatches"),
            "{}",
            outcome.stdout
        );
        assert!(outcome.stdout.contains("sample_rate: 48000 Hz"));
        assert!(outcome.stdout.contains("channels: 2"));
    }

    /// `--status --json`, parsed.
    fn status(a: &mut App) -> Value {
        let outcome = run(a, &[Command::Status { json: true }]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        serde_json::from_str(&outcome.stdout)
            .unwrap_or_else(|err| panic!("--status --json is not JSON ({err}): {}", outcome.stdout))
    }

    /// `--status`, as `key` → `value` for every line.
    fn status_lines(a: &mut App) -> Vec<(String, String)> {
        let outcome = run(a, &[Command::Status { json: false }]);
        outcome
            .stdout
            .lines()
            .map(|line| {
                let (key, value) = line
                    .split_once(": ")
                    .unwrap_or_else(|| panic!("not a `key: value` line: {line:?}"));
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }

    fn line<'a>(lines: &'a [(String, String)], key: &str) -> &'a str {
        lines
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no `{key}:` line in {lines:?}"))
    }

    #[test]
    fn status_can_come_back_as_json_and_the_lines_are_still_the_default() {
        // The help text promised JSON since 0.2.0 and never produced any. It is real now, and it
        // is opt-in: the line format is what every existing script greps.
        let mut a = app();
        let lines = run(&mut a, &[Command::Status { json: false }]);
        assert!(lines.stdout.starts_with("version: "), "{}", lines.stdout);
        assert!(!lines.stdout.trim_start().starts_with('{'));

        let json = status(&mut a);
        assert!(json.is_object(), "{json}");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_json_keeps_every_key_0_3_0_printed_with_the_same_type() {
        // Scripts written against 0.3.0 index into these; a key that moved or changed type breaks
        // them silently. `master_gain_db`, `balance_db` and the effects were whole numbers then
        // (`{:.0}`), and a strictly typed reader (Go's `encoding/json` into an int) rejects `-6.0`.
        let mut a = app();
        run(
            &mut a,
            &[
                Command::MasterGain(-6.0),
                Command::Balance(3.0),
                Command::Effects(vec![(Effect::Bass, 7.0)]),
            ],
        );
        let json = status(&mut a);
        for key in [
            "version",
            "power",
            "audio",
            "preset",
            "device",
            "direction",
            "view",
            "theme",
            "effects",
            "master_gain_db",
            "balance_db",
            "volume_leveling",
            "filter_q",
            "eq",
            "format",
            "ring",
        ] {
            assert!(json.get(key).is_some(), "{key} missing from {json}");
        }
        assert_eq!(json["power"], true);
        assert_eq!(json["audio"], "unavailable");
        assert_eq!(json["direction"], "output");
        assert_eq!(json["view"], "pro");
        assert_eq!(json["theme"], "dark");
        assert!(json["device"].is_null());
        assert_eq!(json["master_gain_db"].as_i64(), Some(-6));
        assert_eq!(json["balance_db"].as_i64(), Some(3));
        assert!(json["volume_leveling"].is_f64());
        assert!(json["filter_q"].is_f64());
        for effect in Effect::ALL {
            assert!(
                json["effects"][effect.key()].is_i64(),
                "effects.{} should be a whole number: {json}",
                effect.key()
            );
        }
        assert_eq!(json["effects"]["bass"].as_i64(), Some(7));
        assert_eq!(json["eq"]["enabled"], true);
        assert_eq!(json["eq"]["bands"].as_u64(), Some(10));
        assert_eq!(json["eq"]["max_bands"].as_u64(), Some(eq::MAX_BANDS as u64));
        assert_eq!(json["format"]["sample_rate"].as_u64(), Some(48_000));
        assert_eq!(json["format"]["channels"].as_u64(), Some(2));
        for key in [
            "dropped_frames",
            "underrun_frames",
            "resyncs",
            "format_mismatches",
        ] {
            assert_eq!(json["ring"][key].as_u64(), Some(0), "ring.{key}: {json}");
        }
    }

    #[test]
    fn the_json_has_a_block_per_lane_the_edit_direction_and_the_microphone_meters() {
        let mut a = app();
        a.state.devices = mixed_devices();
        a.mark_devices_seen_for_tests();
        run(
            &mut a,
            &[
                Command::Output(OutputCommand::Select("alsa_output.pci".into())),
                Command::Input(InputCommand::Select("alsa_input.usb-fifine".into())),
            ],
        );
        a.state.voice_probability = 0.934;
        a.state.noise_floor_db = -42.04;
        a.state.denoise_reduction_db = 18.25;
        a.state.gate_reduction_db = 3.0;
        a.state.denoise_running = true;
        a.state.echo_cancel_running = true;
        a.state.input_active = true;

        let json = status(&mut a);
        assert_eq!(
            json["edit_direction"], "input",
            "the last device picked is the one edited"
        );
        assert_eq!(json["direction"], json["edit_direction"]);

        let output = &json["output"];
        assert_eq!(output["enabled"], true);
        assert_eq!(
            output["device"],
            "Ryzen HD Audio Controller Analogue Stereo"
        );
        assert_eq!(output["node_name"], "alsa_output.pci");
        assert_eq!(output["active"], false);
        assert!(output.get("preset").is_some(), "{json}");

        let input = &json["input"];
        assert_eq!(input["enabled"], true);
        assert_eq!(input["device"], "fifine Microphone Analogue Stereo");
        assert_eq!(input["node_name"], "alsa_input.usb-fifine");
        assert_eq!(input["active"], true);
        assert_eq!(input["noise_suppression"], "preset");
        assert!(input.get("preset").is_some(), "{json}");
        assert_eq!(
            json["device"], input["device"],
            "`device` is the edit direction's"
        );

        let meters = &json["input_meters"];
        assert_eq!(meters["voice_probability"].as_f64(), Some(0.93));
        assert_eq!(meters["noise_floor_db"].as_f64(), Some(-42.0));
        assert_eq!(meters["denoise_reduction_db"].as_f64(), Some(18.3));
        assert_eq!(meters["gate_reduction_db"].as_f64(), Some(3.0));
        assert_eq!(meters["compressor_reduction_db"].as_f64(), Some(0.0));
        assert_eq!(meters["deesser_reduction_db"].as_f64(), Some(0.0));
        assert_eq!(meters["denoise_running"], true);
        assert_eq!(meters["deesser_running"], false);

        assert_eq!(json["echo_cancel"]["on"], false);
        assert_eq!(json["echo_cancel"]["running"], true);
    }

    #[test]
    fn a_noise_floor_that_has_not_been_measured_is_null_rather_than_full_scale() {
        // `0.0` is what the meters hold before the first measurement; reporting it would tell a
        // status bar that the room is at 0 dBFS. With no microphone delivering there is nothing
        // to measure either, whatever the field last held.
        let mut a = app();
        a.state.input_active = true;
        a.state.noise_floor_db = 0.0;
        assert!(status(&mut a)["input_meters"]["noise_floor_db"].is_null());
        let lines = status_lines(&mut a);
        assert_eq!(line(&lines, "input_noise_floor"), "(none)");
        a.state.noise_floor_db = f32::NAN;
        assert!(status(&mut a)["input_meters"]["noise_floor_db"].is_null());

        a.state.noise_floor_db = -61.26;
        a.state.input_active = false;
        assert!(status(&mut a)["input_meters"]["noise_floor_db"].is_null());
        a.state.input_active = true;
        assert_eq!(
            status(&mut a)["input_meters"]["noise_floor_db"].as_f64(),
            Some(-61.3)
        );
        assert_eq!(line(&status_lines(&mut a), "input_noise_floor"), "-61.3 dB");
    }

    #[test]
    fn a_detached_lane_reports_null_and_disabled() {
        let mut a = app();
        a.state.devices = mixed_devices();
        a.mark_devices_seen_for_tests();
        run(
            &mut a,
            &[Command::Input(InputCommand::Select(
                "alsa_input.pci".into(),
            ))],
        );
        assert_eq!(status(&mut a)["input"]["enabled"], true);

        run(&mut a, &[Command::Input(InputCommand::Detach)]);
        let json = status(&mut a);
        assert_eq!(json["input"]["enabled"], false);
        assert!(json["input"]["device"].is_null());
        assert!(json["input"]["node_name"].is_null());
        assert_eq!(line(&status_lines(&mut a), "input"), "(none)");
    }

    #[test]
    fn the_status_lines_name_both_lanes_their_presets_and_the_microphone() {
        let mut a = app_with_presets("status-lines");
        a.state.devices = mixed_devices();
        a.mark_devices_seen_for_tests();
        run(
            &mut a,
            &[
                Command::Input(InputCommand::Select("alsa_input.pci".into())),
                Command::Preset(PresetCommand::Select("Flat Voice".into())),
                Command::Output(OutputCommand::Select("alsa_output.usb-fifine".into())),
                Command::Preset(PresetCommand::Select("Beta".into())),
                Command::NoiseSuppression(NoiseSuppressionOverride::Strong),
            ],
        );
        let lines = status_lines(&mut a);
        assert_eq!(line(&lines, "output"), "fifine Microphone Analogue Stereo");
        assert_eq!(
            line(&lines, "input"),
            "Ryzen HD Audio Controller Analogue Stereo"
        );
        assert_eq!(line(&lines, "output_preset"), "Beta");
        assert_eq!(line(&lines, "input_preset"), "Flat Voice");
        assert_eq!(
            line(&lines, "preset"),
            "Beta",
            "`preset` is the edit direction's"
        );
        assert_eq!(line(&lines, "direction"), "output");
        assert_eq!(line(&lines, "noise_suppression"), "strong");
        assert_eq!(line(&lines, "input_denoise"), "strong, stopped");
        assert_eq!(line(&lines, "echo_cancel"), "off, stopped");
        for key in [
            "version",
            "power",
            "audio",
            "view",
            "theme",
            "master_gain",
            "balance",
            "volume_leveling",
            "filter_q",
            "eq",
            "sample_rate",
            "channels",
            "ring",
            "input_voice_probability",
            "input_noise_floor",
            "input_denoise_reduction",
            "input_gate_reduction",
            "input_compressor_reduction",
            "input_deesser_reduction",
        ] {
            line(&lines, key);
        }
        // Every key once: a grep for `^input:` must not also catch a telemetry line.
        let mut keys: Vec<&str> = lines.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "a key is printed twice: {lines:?}");

        // And the JSON says the same.
        let json = status(&mut a);
        assert_eq!(json["output"]["preset"], "Beta");
        assert_eq!(json["input"]["preset"], "Flat Voice");
        assert_eq!(json["input"]["denoise_level"], "strong");
    }

    #[test]
    fn a_device_name_with_a_quote_in_it_cannot_break_the_json() {
        // A device description is whatever the hardware calls itself and a preset name is whatever
        // the user typed; neither is this program's to trust.
        let mut a = app();
        let awkward = "Bob\"s \\ Mic\ttab\u{1}";
        a.state.devices = vec![AudioDevice {
            id: 1,
            name: "node".to_owned(),
            description: awkward.to_owned(),
            is_default: true,
            direction: DeviceDirection::Output,
            form_factor: "microphone".into(),
        }];
        a.state.selected_output = Some(0);

        let json = status(&mut a);
        assert_eq!(json["device"], awkward);
        assert_eq!(json["output"]["device"], awkward);
    }

    #[test]
    fn status_reports_the_live_state_as_greppable_lines() {
        let mut a = app();
        run(&mut a, &[Command::Effects(vec![(Effect::Bass, 7.0)])]);
        let outcome = run(&mut a, &[Command::Status { json: false }]);

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
        assert_eq!(a.state.selected_device(), Some(1));

        run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_output.pci-0000_00_1f.3".into(),
            ))],
        );
        assert_eq!(a.state.selected_device(), Some(0));

        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_device(), Some(1));
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
    fn output_naming_a_microphone_still_selects_it_for_the_input_lane_with_a_note() {
        use fxsound_core::DeviceDirection::{Input, Output};
        let mut a = app();
        a.state.devices = mixed_devices();

        // 0.3.0 compatibility: the node name is unambiguous and names a source, which was the
        // CLI's way into the input mode. It still works, and says what to write instead.
        let outcome = run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_input.usb-fifine".into(),
            ))],
        );
        assert!(!outcome.failed, "a 0.3.0 script must keep working");
        assert!(outcome.stderr.contains("--input"), "{:?}", outcome.stderr);
        assert!(outcome.stderr.contains("alsa_input.usb-fifine"));
        assert_eq!(a.state.selected_input, Some(2));
        assert_eq!(
            a.state.selected_output, None,
            "the speakers were not touched"
        );
        assert_eq!(a.state.direction, Input);

        // The shared description matches the output for `--output` — so a description alone never
        // lands on the microphone by accident — and leaves the input lane where it was.
        let outcome = run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "fifine Microphone Analogue Stereo".into(),
            ))],
        );
        assert!(outcome.stderr.is_empty(), "{:?}", outcome.stderr);
        assert_eq!(a.state.selected_output, Some(1));
        assert_eq!(a.state.selected_input, Some(2));
        assert_eq!(a.state.direction, Output);
    }

    #[test]
    fn input_takes_the_microphone_when_a_description_is_shared_with_a_sink() {
        let mut a = app();
        a.state.devices = mixed_devices();
        let outcome = run(
            &mut a,
            &[Command::Input(InputCommand::Select(
                "fifine Microphone Analogue Stereo".into(),
            ))],
        );
        assert!(outcome.stderr.is_empty() && !outcome.failed, "{outcome:?}");
        assert_eq!(a.state.selected_input, Some(2));
        assert_eq!(a.state.selected_output, None);
        assert_eq!(a.state.direction, DeviceDirection::Input);
    }

    #[test]
    fn input_naming_a_speaker_is_refused_rather_than_guessed_at() {
        let mut a = app();
        a.state.devices = mixed_devices();
        let outcome = run(
            &mut a,
            &[Command::Input(InputCommand::Select(
                "alsa_output.pci".into(),
            ))],
        );
        assert!(outcome.failed);
        assert!(outcome.stderr.contains("--output"), "{:?}", outcome.stderr);
        assert_eq!(a.state.selected_input, None);
        assert_eq!(a.state.selected_output, None);
    }

    #[test]
    fn an_unknown_microphone_fails_once_the_list_exists_and_waits_before() {
        let mut a = app();
        let outcome = run(
            &mut a,
            &[Command::Input(InputCommand::Select("Jabra".into()))],
        );
        assert!(!outcome.failed, "no list yet: held, not refused");

        a.state.devices = mixed_devices();
        a.mark_devices_seen_for_tests();
        let outcome = run(
            &mut a,
            &[Command::Input(InputCommand::Select("Jabra".into()))],
        );
        assert!(outcome.failed);
        assert!(outcome.stderr.contains("no microphone is called \"Jabra\""));
    }

    #[test]
    fn off_detaches_each_lane_and_leaves_the_other_alone() {
        let mut a = app();
        a.state.devices = mixed_devices();
        run(
            &mut a,
            &[
                Command::Output(OutputCommand::Select("alsa_output.pci".into())),
                Command::Input(InputCommand::Select("alsa_input.pci".into())),
            ],
        );
        assert_eq!(
            (a.state.selected_output, a.state.selected_input),
            (Some(0), Some(3))
        );

        run(&mut a, &[Command::Output(OutputCommand::Detach)]);
        assert_eq!(
            (a.state.selected_output, a.state.selected_input),
            (None, Some(3))
        );
        assert!(!a.settings().lane_enabled(DeviceDirection::Output));
        assert!(a.settings().lane_enabled(DeviceDirection::Input));

        run(&mut a, &[Command::Input(InputCommand::Detach)]);
        assert_eq!(
            (a.state.selected_output, a.state.selected_input),
            (None, None)
        );
        assert!(!a.settings().lane_enabled(DeviceDirection::Input));
    }

    #[test]
    fn next_output_cycles_the_outputs_and_next_input_the_inputs() {
        let mut a = app();
        a.state.devices = mixed_devices();

        // Nothing selected: the first *output*, never an input.
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_output, Some(0));
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_output, Some(1));
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(
            a.state.selected_output,
            Some(0),
            "wraps among the outputs, skipping the inputs"
        );
        assert_eq!(
            a.state.selected_input, None,
            "--next-output never touches the microphone"
        );

        // The input lane has a keybind of its own, whatever the window is editing.
        run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert_eq!(a.state.selected_input, Some(2));
        run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert_eq!(a.state.selected_input, Some(3));
        run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert_eq!(
            a.state.selected_input,
            Some(2),
            "wraps among the inputs, never back to a sink"
        );
        assert_eq!(
            a.state.selected_output,
            Some(0),
            "--next-input never touches the speakers"
        );

        // And --next-output while the microphone is being edited still cycles the speakers.
        assert_eq!(a.state.direction, DeviceDirection::Input);
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.state.selected_output, Some(1));
        assert_eq!(a.state.selected_input, Some(2));
    }

    #[test]
    fn a_lone_device_is_a_no_op_for_next_but_a_detached_lane_comes_back_on_it() {
        let mut a = app();
        a.state.devices = mixed_devices();
        a.state.devices.truncate(3); // two outputs, one input
        run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert_eq!(
            a.state.selected_input,
            Some(2),
            "off → the one microphone there is"
        );
        run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert_eq!(a.state.selected_input, Some(2), "nowhere else to go");

        // No device of that direction at all: nothing to do, and nothing to fail.
        a.state.devices.truncate(2);
        a.state.selected_input = None;
        let outcome = run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert!(!outcome.failed);
        assert_eq!(a.state.selected_input, None);
    }

    #[test]
    fn status_reports_each_lane_under_its_own_name() {
        let mut a = app();
        a.state.devices = mixed_devices();
        let lines = status_lines(&mut a);
        assert_eq!(line(&lines, "output"), "(none)");
        assert_eq!(line(&lines, "input"), "(none)");
        assert_eq!(line(&lines, "direction"), "output");

        run(
            &mut a,
            &[Command::Input(InputCommand::Select(
                "alsa_input.pci".into(),
            ))],
        );
        let lines = status_lines(&mut a);
        assert_eq!(
            line(&lines, "output"),
            "(none)",
            "the speakers are still detached"
        );
        assert_eq!(
            line(&lines, "input"),
            "Ryzen HD Audio Controller Analogue Stereo"
        );
        assert_eq!(line(&lines, "direction"), "input");
    }

    #[test]
    fn edit_switches_the_lane_the_window_and_the_preset_commands_address() {
        let mut a = app_with_presets("edit");
        assert_eq!(a.state.direction, DeviceDirection::Output);
        run(&mut a, &[Command::EditDirection(DeviceDirection::Input)]);
        assert_eq!(a.state.direction, DeviceDirection::Input);
        assert_eq!(a.settings().device_direction, DeviceDirection::Input);
        let names: Vec<&str> = a.state.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Clean Voice", "Flat Voice"]);

        run(&mut a, &[Command::EditDirection(DeviceDirection::Output)]);
        assert_eq!(a.state.direction, DeviceDirection::Output);
    }

    #[test]
    fn an_unknown_preset_name_fails_in_either_lane() {
        let mut a = app_with_presets("unknown-preset");
        for lane in DeviceDirection::ALL {
            run(&mut a, &[Command::EditDirection(lane)]);
            let before = a.state.selected_preset;
            let outcome = run(
                &mut a,
                &[Command::Preset(PresetCommand::Select("Nope".into()))],
            );
            assert!(outcome.failed, "{lane:?}: exited zero having done nothing");
            assert!(
                outcome
                    .stderr
                    .contains(&format!("no {} preset is called \"Nope\"", lane.key())),
                "{:?}",
                outcome.stderr
            );
            assert_eq!(
                a.state.selected_preset, before,
                "{lane:?}: the selection moved"
            );
        }
    }

    #[test]
    fn a_preset_of_the_other_lane_fails_and_says_how_to_reach_it() {
        let mut a = app_with_presets("other-lane");
        let outcome = run(
            &mut a,
            &[Command::Preset(PresetCommand::Select("Flat Voice".into()))],
        );
        assert!(outcome.failed);
        assert!(
            outcome.stderr.contains("--edit=input"),
            "{:?}",
            outcome.stderr
        );
        assert_eq!(
            a.state.direction,
            DeviceDirection::Output,
            "not switched behind the user's back"
        );

        // With the lane named, the same name works.
        let outcome = run(
            &mut a,
            &[
                Command::EditDirection(DeviceDirection::Input),
                Command::Preset(PresetCommand::Select("Flat Voice".into())),
            ],
        );
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(
            a.state.preset().map(|p| p.name.as_str()),
            Some("Flat Voice")
        );
        assert_eq!(a.settings().input_preset, "Flat Voice");
    }

    #[test]
    fn a_known_preset_is_selected_in_the_edit_direction() {
        let mut a = app_with_presets("known-preset");
        let outcome = run(
            &mut a,
            &[Command::Preset(PresetCommand::Select("Beta".into()))],
        );
        assert!(!outcome.failed && outcome.stderr.is_empty(), "{outcome:?}");
        assert_eq!(a.state.preset().map(|p| p.name.as_str()), Some("Beta"));
        assert_eq!(a.settings().output_preset, "Beta");
    }

    #[test]
    fn a_line_with_a_device_and_a_preset_puts_the_preset_on_that_device_lane() {
        // `fxsound --input=… --preset='Gaming Headset'`, the man page's own example: the device
        // runs first, so the preset is looked up in the microphone's list.
        let mut a = app_with_presets("device-then-preset");
        a.state.devices = mixed_devices();
        let cli = crate::cli::Cli::try_parse_from([
            "fxsound",
            "--preset=Flat Voice",
            "--input=alsa_input.pci",
        ])
        .expect("parses");
        let outcome = run(&mut a, &cli.commands());
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(a.state.direction, DeviceDirection::Input);
        assert_eq!(
            a.state.preset().map(|p| p.name.as_str()),
            Some("Flat Voice")
        );
    }

    /// Parse a command line and run it, as a cold start or a forwarded one would.
    fn run_line(a: &mut App, args: &[&str]) -> Outcome {
        let cli =
            crate::cli::Cli::try_parse_from(std::iter::once("fxsound").chain(args.iter().copied()))
                .expect("parses");
        run(a, &cli.commands())
    }

    /// The device list arriving, as far as a held `--output` or `--input` can tell.
    fn devices_arrive(a: &mut App) {
        a.state.devices = mixed_devices();
        a.mark_devices_seen_for_tests();
        a.apply_pending_device();
    }

    #[test]
    fn a_device_and_a_preset_at_login_find_the_preset_in_that_lane_before_the_list_arrives() {
        // The man page's example again, at login: the control socket answers before PipeWire has
        // listed anything, so the device is held. The preset still has to be looked up in the
        // microphone's list rather than refused as "no output preset", and still be the one the
        // microphone has once the list arrives.
        let mut a = app_with_presets("held-device-then-preset");
        assert!(!a.has_seen_devices());
        assert_eq!(a.state.direction, DeviceDirection::Output);

        let outcome = run_line(&mut a, &["--preset=Flat Voice", "--input=alsa_input.pci"]);
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert!(outcome.stderr.is_empty(), "{:?}", outcome.stderr);
        assert_eq!(a.state.direction, DeviceDirection::Input);
        assert_eq!(
            a.pending_device(DeviceDirection::Input),
            Some("alsa_input.pci")
        );
        assert_eq!(
            a.state.preset().map(|p| p.name.as_str()),
            Some("Flat Voice")
        );

        devices_arrive(&mut a);
        assert_eq!(a.state.selected_input, Some(3));
        assert_eq!(a.pending_device(DeviceDirection::Input), None);
        assert_eq!(a.state.direction, DeviceDirection::Input);
        assert_eq!(
            a.lane_preset(DeviceDirection::Input),
            Some(("Flat Voice", false))
        );
        assert_eq!(a.settings().input_preset, "Flat Voice");
    }

    #[test]
    fn a_preset_picked_while_the_device_waits_beats_the_one_the_device_remembers() {
        use fxsound_core::DeviceDirection::Output;
        let mut a = app_with_presets("held-preset-beats-remembered");

        // A session in which the speakers were used with Alpha, which they now remember.
        a.state.devices = mixed_devices();
        let outcome = run_line(&mut a, &["--output=alsa_output.pci", "--preset=Alpha"]);
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(
            a.settings().preset_for_device("alsa_output.pci", Output),
            Some("Alpha")
        );

        // The next login, before the list: the same speakers, with Beta this time.
        a.state.devices.clear();
        a.state.set_selection(Output, None);
        let outcome = run_line(&mut a, &["--output=alsa_output.pci", "--preset=Beta"]);
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(a.pending_device(Output), Some("alsa_output.pci"));
        assert_eq!(a.state.preset().map(|p| p.name.as_str()), Some("Beta"));

        // Selecting the speakers would bring Alpha back. Beta was asked for after them.
        devices_arrive(&mut a);
        assert_eq!(a.state.selected_output, Some(0));
        assert_eq!(a.state.preset().map(|p| p.name.as_str()), Some("Beta"));
        assert_eq!(a.settings().output_preset, "Beta");
        assert_eq!(
            a.settings().preset_for_device("alsa_output.pci", Output),
            Some("Beta"),
            "picked with the speakers on the line, so the speakers remember it"
        );
    }

    #[test]
    fn a_waiting_device_with_no_preset_picked_still_brings_back_its_own() {
        use fxsound_core::DeviceDirection::Output;
        let mut a = app_with_presets("held-device-remembered-preset");
        a.state.devices = mixed_devices();
        run_line(&mut a, &["--output=alsa_output.pci", "--preset=Beta"]);
        run_line(
            &mut a,
            &["--output=alsa_output.usb-fifine", "--preset=Alpha"],
        );
        a.state.devices.clear();
        a.state.set_selection(Output, None);

        let outcome = run_line(&mut a, &["--output=alsa_output.pci"]);
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        devices_arrive(&mut a);
        assert_eq!(a.state.selected_output, Some(0));
        assert_eq!(a.state.preset().map(|p| p.name.as_str()), Some("Beta"));
    }

    #[test]
    fn edit_later_on_the_line_outlives_the_arrival_of_a_waiting_device() {
        // `--input=… --edit=output --preset=Beta` at login does what it does with the list there:
        // the microphone is selected, and the window, the preset and the settings stay on the
        // speakers, which `--edit` asked for after the microphone.
        let mut a = app_with_presets("held-device-then-edit");
        let outcome = run_line(
            &mut a,
            &["--input=alsa_input.pci", "--edit=output", "--preset=Beta"],
        );
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(a.state.direction, DeviceDirection::Output);

        devices_arrive(&mut a);
        assert_eq!(a.state.selected_input, Some(3));
        assert_eq!(a.state.direction, DeviceDirection::Output);
        assert_eq!(a.state.preset().map(|p| p.name.as_str()), Some("Beta"));
        assert_eq!(a.settings().device_direction, DeviceDirection::Output);
    }

    #[test]
    fn two_waiting_devices_leave_the_edit_direction_on_the_later_one() {
        let mut a = app_with_presets("two-held-devices");
        run_line(
            &mut a,
            &["--output=alsa_output.pci", "--input=alsa_input.pci"],
        );
        devices_arrive(&mut a);
        assert_eq!(
            (a.state.selected_output, a.state.selected_input),
            (Some(0), Some(3))
        );
        assert_eq!(a.state.direction, DeviceDirection::Input);
    }

    #[test]
    fn off_before_the_list_arrives_cancels_the_device_the_lane_was_waiting_for() {
        use fxsound_core::DeviceDirection::{Input, Output};
        let mut a = app();
        a.select_device_when_listed("alsa_output.pci", Output);
        a.select_device_when_listed("alsa_input.pci", Input);

        let outcome = run(&mut a, &[Command::Output(OutputCommand::Detach)]);
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(a.pending_device(Output), None);
        assert_eq!(
            a.pending_device(Input),
            Some("alsa_input.pci"),
            "the other lane's wait is its own"
        );

        devices_arrive(&mut a);
        assert_eq!(
            a.state.selected_output, None,
            "`off` came after the name and was undone by the list"
        );
        assert!(!a.settings().lane_enabled(Output));
        assert_eq!(a.state.selected_input, Some(3));
    }

    #[test]
    fn a_listed_device_or_next_supersedes_a_device_still_waiting_for_the_list() {
        use fxsound_core::DeviceDirection::Output;

        // A name that resolves now settles the lane, and the held one is spent.
        let mut a = app();
        a.select_device_when_listed("alsa_output.usb-fifine", Output);
        a.state.devices = mixed_devices();
        let outcome = run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_output.pci".into(),
            ))],
        );
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(a.pending_device(Output), None);
        devices_arrive(&mut a);
        assert_eq!(a.state.selected_output, Some(0));

        // `--next-output` says the held name is not the one wanted either, even with nowhere to
        // move to yet.
        let mut a = app();
        a.select_device_when_listed("alsa_output.usb-fifine", Output);
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(a.pending_device(Output), None);
        devices_arrive(&mut a);
        assert_eq!(a.state.selected_output, None);
    }

    #[test]
    fn a_second_name_for_a_waiting_lane_replaces_the_first_and_the_preset_held_with_it() {
        use fxsound_core::DeviceDirection::Output;
        let mut a = app_with_presets("held-device-replaced");
        a.state.devices = mixed_devices();
        run_line(&mut a, &["--output=alsa_output.pci", "--preset=Alpha"]);
        a.state.devices.clear();
        a.state.set_selection(Output, None);

        // Beta was picked for the USB speakers; the line after it names other ones, which bring
        // back their own preset exactly as they would with the list there.
        run_line(
            &mut a,
            &["--output=alsa_output.usb-fifine", "--preset=Beta"],
        );
        run_line(&mut a, &["--output=alsa_output.pci"]);
        assert_eq!(a.pending_device(Output), Some("alsa_output.pci"));
        devices_arrive(&mut a);
        assert_eq!(a.state.selected_output, Some(0));
        assert_eq!(a.state.preset().map(|p| p.name.as_str()), Some("Alpha"));
    }

    #[test]
    fn noise_suppression_is_saved_and_reaches_the_voice_chain_from_either_lane() {
        let mut a = app_with_presets("noise-suppression");
        assert_eq!(a.state.direction, DeviceDirection::Output);
        run(
            &mut a,
            &[Command::NoiseSuppression(NoiseSuppressionOverride::Strong)],
        );
        assert_eq!(
            a.settings().noise_suppression,
            NoiseSuppressionOverride::Strong
        );
        assert!(a.input_params().rnnoise, "a pinned level runs the denoiser");
        assert_eq!(a.input_params().denoise_level, DenoiseLevel::Strong);
        assert_eq!(
            a.state.denoise_level,
            DenoiseLevel::Strong,
            "the strip says so too"
        );

        run(
            &mut a,
            &[Command::NoiseSuppression(NoiseSuppressionOverride::Off)],
        );
        assert!(!a.input_params().rnnoise);

        run(
            &mut a,
            &[Command::NoiseSuppression(NoiseSuppressionOverride::Preset)],
        );
        assert_eq!(
            a.settings().noise_suppression,
            NoiseSuppressionOverride::Preset
        );
    }

    #[test]
    fn an_open_settings_pane_follows_a_noise_suppression_set_from_the_command_line() {
        let mut a = app();
        let mut pane = a.settings_state();
        run(
            &mut a,
            &[Command::NoiseSuppression(NoiseSuppressionOverride::Light)],
        );
        a.refresh_settings_state(&mut pane);
        assert_eq!(
            pane.settings.noise_suppression,
            NoiseSuppressionOverride::Light
        );
    }

    #[test]
    fn watch_is_refused_by_the_command_path_rather_than_answered_once() {
        let mut a = app();
        let outcome = run(
            &mut a,
            &[Command::Watch {
                json: true,
                meters: false,
            }],
        );
        assert!(outcome.failed);
        assert!(
            outcome.stderr.contains("watch request"),
            "{}",
            outcome.stderr
        );
        assert!(outcome.stdout.is_empty());
        assert!(outcome.window.is_empty());
    }

    // The self-test is run against a scratch prefix, never through `run`: the command arm checks
    // this host's own installation, and with it the live PipeWire socket and session bus.

    #[test]
    fn a_self_test_handed_to_a_running_instance_answers_with_the_report() {
        let (_tmp, env) = crate::selftest::tests::installed();
        let outcome = self_test_outcome(&env, true);
        let report: Value = serde_json::from_str(&outcome.stdout).expect("the report is JSON");
        assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(report["ok"].as_bool(), Some(true), "{}", outcome.stdout);
        assert!(!outcome.failed, "a passing installation exits zero");
        assert!(outcome.stderr.is_empty());
        assert!(
            outcome.window.is_empty(),
            "a self-test never raises the window"
        );
    }

    #[test]
    fn a_self_test_that_finds_a_broken_installation_fails_the_command() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let env = crate::selftest::Environment::for_prefix(tmp.path());
        let outcome = self_test_outcome(&env, true);
        let report: Value = serde_json::from_str(&outcome.stdout).expect("the report is JSON");
        assert_eq!(
            report["ok"].as_bool(),
            Some(false),
            "an empty prefix has no presets"
        );
        assert!(
            outcome.failed,
            "a failing check makes the invoking process exit non-zero"
        );
        assert!(outcome.window.is_empty());
    }

    #[test]
    fn a_self_test_without_json_answers_one_line_per_check() {
        let (_tmp, env) = crate::selftest::tests::installed();
        let outcome = self_test_outcome(&env, false);
        assert!(serde_json::from_str::<Value>(&outcome.stdout).is_err());
        let first = outcome.stdout.lines().next().expect("a line");
        assert!(first.starts_with("version: ok"), "{first}");
        assert!(
            outcome
                .stdout
                .lines()
                .any(|line| line.starts_with("pipewire: skip")),
            "the scratch prefix has no PipeWire, so nothing is probed: {}",
            outcome.stdout
        );
    }

    #[test]
    fn the_scratch_prefix_the_command_tests_use_probes_neither_pipewire_nor_the_bus() {
        let (_tmp, env) = crate::selftest::tests::installed();
        assert_eq!(env.xdg_runtime_dir, None);
        assert_eq!(env.dbus_address, None);
        let outcome = self_test_outcome(&env, true);
        let report: Value = serde_json::from_str(&outcome.stdout).expect("the report is JSON");
        let status_of = |name: &str| {
            report["checks"]
                .as_array()
                .expect("checks")
                .iter()
                .find(|check| check["name"] == name)
                .map(|check| check["status"].clone())
        };
        assert_eq!(status_of("pipewire"), Some(Value::from("skip")));
        assert_eq!(status_of("dbus"), Some(Value::from("skip")));
    }

    #[test]
    fn an_unknown_output_name_changes_nothing() {
        let mut a = app();
        run(
            &mut a,
            &[Command::Output(OutputCommand::Select("nope".into()))],
        );
        assert!(a.state.selected_device().is_none());
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
                Command::Status { json: false },
            ],
        );
        assert_eq!(a.state.effect(Effect::Ambience), 3.0);
        assert_eq!(a.state.master_gain_db, -6.0);
        // Status ran last, so it reports what the earlier commands did.
        assert!(outcome.stdout.contains("ambience: 3"));
        assert!(outcome.stdout.contains("master_gain: -6 dB"));
    }
}
