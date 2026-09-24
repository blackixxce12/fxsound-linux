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
//! send as well, and what `--list-apps` reports: [`AppListing`], which D-Bus `ListApps` sends.

use std::path::Path;

use crate::app::{App, AppRuleRefusal, detach, list_apps, select_on};
use crate::cli::{
    AppPresetChoice, Command, DeviceCommand, PowerCommand, PresetCommand, WindowCommand,
};
use fxsound_core::messages::AppStream;
use fxsound_core::{AppRules, AudioDevice, DeviceDirection, Effect, ThemeMode, ViewMode, eq};
use fxsound_ui::{UiAction, state::UiState};
use serde::Serialize;
use serde_json::Value;

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

        Command::ListApps { json } => {
            outcome.stdout = app_listing(app_statuses(app.app_rules(), app.app_streams()), *json);
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
        Command::AppPreset {
            direction,
            app: name,
            preset,
        } => return run_app_preset(app, *direction, name, preset),

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
/// to go. "Next" in the order of the lane's device priority list, which is the order the list
/// is kept in (U4, `crate::priority::sort_by_rank`), as upstream's `CMD_NEXT_OUTPUT` walks its
/// sorted output list (`FxController.cpp:1983-2011`).
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

/// A preset option: refused, with the reason on stderr, wherever the hamburger menu would grey
/// its item out ([`App::preset_command_allowed`]) — an unknown name, an overwrite of a factory
/// preset, a rename with unsaved changes, a new name that is taken, the user-preset cap — and
/// otherwise carried out as the menu item is.
///
/// A name that is not in the edit direction's list is an error for either lane (0.4.0 design
/// §1.4, §11): 0.3.0 exited zero having done nothing, which a script cannot tell from success. A
/// name the other lane has is almost certainly that lane's preset, so the message says how to
/// reach it.
fn run_preset(app: &mut App, command: &PresetCommand) -> Outcome {
    if let Err(refusal) = app.preset_command_allowed(command) {
        return Outcome::refused(refusal.to_string());
    }
    match command {
        PresetCommand::Select(name) => {
            if let Some(index) = app.state.presets.iter().position(|p| p.name == *name) {
                app.handle(&[UiAction::SelectPreset(index)]);
            }
        }
        PresetCommand::SaveAs(name) => app.handle(&[UiAction::SavePresetAs(name.clone())]),
        PresetCommand::Overwrite => app.handle(&[UiAction::SavePreset]),
        PresetCommand::Undo => app.handle(&[UiAction::UndoPresetChanges]),
        // The menu's Rename in one step: the saved file moves, everything that named the preset
        // follows it, and the stream hears the new name once.
        PresetCommand::Rename(name) => app.rename_preset(name),
        PresetCommand::Delete => app.handle(&[UiAction::DeletePreset]),
        PresetCommand::Next => app.cycle_preset(true),
        PresetCommand::Previous => app.cycle_preset(false),
    }
    Outcome::default()
}

/// `--app-preset APP=PRESET` and `--app-input-preset APP=PRESET`, and D-Bus's `SetAppPreset`:
/// the preset the applications `name` names run through in `lane` ([`App::set_named_app_preset`]).
///
/// A preset `lane`'s list does not have is refused, with a word on where it is when the other
/// lane's list has it, as `--preset` says it. A name no remembered application answers to is not
/// an error — FxSound keeps the preset for the Flatpak id, program or name the text looks like
/// ([`crate::app::unseen_key`]), the game that has not been started yet — but it is said, on
/// stderr, since a mistyped name is just as new to FxSound. So is a choice a cold start holds
/// until it has heard which applications play and record ([`App::hold_app_presets`]).
fn run_app_preset(
    app: &mut App,
    lane: DeviceDirection,
    name: &str,
    preset: &AppPresetChoice,
) -> Outcome {
    let runs = match lane {
        DeviceDirection::Output => "plays",
        DeviceDirection::Input => "records",
    };
    match app.set_named_app_preset(name, lane, preset.name()) {
        Ok(done) if done.held => Outcome {
            stderr: format!(
                "note: FxSound remembers no application called {name:?}; it chooses the preset \
                 once it has heard which applications play and record, for one of them that \
                 answers to the name or else for {} (--list-apps shows the applications FxSound \
                 knows)",
                crate::app::unseen_description(&crate::app::unseen_key(name))
            ),
            ..Outcome::default()
        },
        Ok(done) if done.apps.is_empty() => Outcome {
            stderr: format!(
                "note: FxSound has not seen an application called {name:?}, so it follows \
                 FxSound's preset already"
            ),
            ..Outcome::default()
        },
        Ok(done) if done.unseen => Outcome {
            stderr: format!(
                "note: FxSound has not seen an application called {name:?}; the preset is kept for \
                 {} and runs once it {runs} (--list-apps shows the applications FxSound knows)",
                done.apps
                    .first()
                    .map_or_else(|| format!("{name:?}"), crate::app::unseen_description)
            ),
            ..Outcome::default()
        },
        Ok(_) => Outcome::default(),
        Err(refusal) => {
            let mut message = refusal.to_string();
            if let AppRuleRefusal::UnknownPreset { direction, name } = &refusal
                && app.lane_has_preset(direction.other(), name)
            {
                message.push_str(match direction {
                    DeviceDirection::Output => {
                        "; it is an input preset, which --app-input-preset gives"
                    }
                    DeviceDirection::Input => "; it is an output preset, which --app-preset gives",
                });
            }
            Outcome::refused(message)
        }
    }
}

/// One remembered application, as the status document's `apps`, `--list-apps` and D-Bus
/// `ListApps` list it: who it is, what the user chose for it, and what it runs through now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppStatus {
    /// The name the window shows: the application's own name, else its program, else its Flatpak
    /// id. `--app-preset` takes it back.
    pub name: String,
    /// The program, as PipeWire reported it; empty when it did not.
    pub binary: String,
    /// The Flatpak id; empty for an application that is not one.
    pub flatpak: String,
    /// Whether it plays or records now.
    pub running: bool,
    /// Its playback preset of its own, as the store has it; `null` while it follows the output
    /// lane's.
    pub output_preset: Option<String>,
    /// Its recording preset of its own; `null` while it follows the input lane's.
    pub input_preset: Option<String>,
    /// What it runs through now, per lane.
    pub routed: RoutedStatus,
}

/// Per lane, the preset of the route FxSound has moved an application's streams onto — what
/// `app_routed` last said. `null` while they are on the lane's own chain: it follows the lane,
/// it is not playing or recording there, or its route could not be made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoutedStatus {
    pub output: Option<String>,
    pub input: Option<String>,
}

/// What `--list-apps --json` prints and D-Bus `ListApps` answers: the status document's
/// `schema` and its `apps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppListing {
    /// [`STATUS_SCHEMA`].
    pub schema: u32,
    pub apps: Vec<AppStatus>,
}

/// Every application of `rules`, in the order Settings ▸ Applications lists them
/// ([`list_apps`]), with what `streams` — the engine's last report — say they are doing.
#[must_use]
pub fn app_statuses(rules: &AppRules, streams: &[AppStream]) -> Vec<AppStatus> {
    list_apps(rules, streams)
        .into_iter()
        .map(|listed| {
            let rule = listed.rule;
            let own = |direction: DeviceDirection| {
                rule.has_preset(direction)
                    .then(|| rule.preset(direction).to_owned())
            };
            let routed = |direction: DeviceDirection| {
                listed.routed[match direction {
                    DeviceDirection::Output => 0,
                    DeviceDirection::Input => 1,
                }]
                .map(str::to_owned)
            };
            AppStatus {
                name: rule.key.display().to_owned(),
                binary: rule.key.binary.clone(),
                flatpak: rule.key.flatpak.clone(),
                running: listed.is_running(),
                output_preset: own(DeviceDirection::Output),
                input_preset: own(DeviceDirection::Input),
                routed: RoutedStatus {
                    output: routed(DeviceDirection::Output),
                    input: routed(DeviceDirection::Input),
                },
            }
        })
        .collect()
}

/// What `--list-apps` prints for `apps`: [`AppListing`] as one JSON object, or one line per
/// application of `key=value` pairs, spelled as `fxsound --watch` spells them — a value with a
/// space in it quoted, `null` left empty:
///
/// ```text
/// name="Battlefield 6" binary=bf6.exe flatpak="" running=true output_preset=Gaming input_preset= routed.output=Gaming routed.input=
/// ```
///
/// Nothing at all when no application is remembered.
#[must_use]
pub fn app_listing(apps: Vec<AppStatus>, json: bool) -> String {
    if json {
        let listing = AppListing {
            schema: STATUS_SCHEMA,
            apps,
        };
        return serde_json::to_string(&listing).unwrap_or_else(|err| {
            log::error!("could not serialise the application listing: {err}");
            "{}".to_owned()
        });
    }
    let lines: Vec<String> = apps
        .iter()
        .map(|app| {
            let mut line = String::new();
            for (key, value) in [
                ("name", Value::from(app.name.as_str())),
                ("binary", Value::from(app.binary.as_str())),
                ("flatpak", Value::from(app.flatpak.as_str())),
                ("running", Value::from(app.running)),
                ("output_preset", Value::from(app.output_preset.clone())),
                ("input_preset", Value::from(app.input_preset.clone())),
                ("routed.output", Value::from(app.routed.output.clone())),
                ("routed.input", Value::from(app.routed.input.clone())),
            ] {
                crate::events::push_plain(&mut line, key, &value);
            }
            line.trim_start().to_owned()
        })
        .collect();
    lines.join("\n")
}

/// `--list-apps` with no FxSound running: the store at `path` listed as [`app_listing`] lists
/// it, with nothing running and nothing routed.
///
/// Read only: a file that does not load is reported and left where it is, rather than moved
/// aside as the instance's own load does ([`AppRules::load_from`]) — a question does not tidy the
/// user's files. A missing file is an empty store.
#[must_use]
pub fn store_listing(path: &Path, json: bool) -> Outcome {
    let rules = match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<AppRules>(&text) {
            Ok(mut rules) => {
                rules.sanitise();
                rules
            }
            Err(err) => {
                return Outcome::refused(format!("{} does not load: {err}", path.display()));
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => AppRules::default(),
        Err(err) => return Outcome::refused(format!("{}: {err}", path.display())),
    };
    Outcome {
        stdout: app_listing(app_statuses(&rules, &[]), json),
        ..Outcome::default()
    }
}

/// What `fxsound` says when it cannot find a running FxSound to answer.
pub const NOT_RUNNING: &str = "FxSound is not running";

/// The answer to a command line that has no FxSound to ask, or `None` when it starts one.
///
/// `main` asks this once it holds the lock, before the engine starts, with the line's
/// [`Cli::commands`](crate::cli::Cli::commands): the same list a running instance would run, so a
/// line means the same thing whether FxSound runs or not. `--watch --list-apps` is the listing
/// either way, and so is `--quit --list-apps`.
///
/// * `--list-apps` is answered from the store at `store` ([`store_listing`]), and nothing
///   starts.
/// * `--status` and `--watch` have nobody to ask, and starting FxSound to answer would be the
///   opposite of what was asked: [`NOT_RUNNING`], and a failure.
/// * `--quit` has nothing to stop: [`NOT_RUNNING`] too, but what was asked for is true already,
///   so not a failure. The rest of that line is not carried out.
///
/// `--self-test` is not here: `main` answers it before it tries the lock at all.
#[must_use]
pub fn answer_without_an_instance(commands: &[Command], store: &Path) -> Option<Outcome> {
    match commands {
        [Command::ListApps { json }] => Some(store_listing(store, *json)),
        [Command::Status { .. } | Command::Watch { .. }] => Some(Outcome::refused(NOT_RUNNING)),
        _ if commands.contains(&Command::Quit) => Some(Outcome {
            stderr: NOT_RUNNING.to_owned(),
            ..Outcome::default()
        }),
        _ => None,
    }
}

/// The shape of [`StatusDocument`], as its `schema` key says it. 0.3.0's document had no number
/// and is schema 1; 2 is 0.4.0's, every key of 1 kept and upstream's `printStatus` keys added.
pub const STATUS_SCHEMA: u32 = 2;

/// Everything `--status` reports, in the shape `--status --json` prints it.
///
/// Every key 0.3.0 printed is still here and still means what it meant, read for the **edit
/// direction** where 0.3.0 had a single selection — `preset`, `device` and `direction` are what the
/// window shows. 0.4.0 adds a block per lane, the microphone's telemetry and the echo canceller,
/// and is a superset of the document upstream's `--status` writes (`FxController::printStatus`,
/// `FxController.cpp:609-683`; review item U14): `presets`, `selected_preset`,
/// `output_devices`, `selected_output`, `equalizer` and upstream's names in `effects` mean what
/// they mean there and have the JSON kinds they have there, so a client written against the
/// Windows build (`fxmcp`'s `status.go`) reads this one. What upstream's names cannot carry goes
/// under names of its own: each device's `node_name` and whether it is present, in
/// `output_device_list` and `input_device_list`. Public so the event stream's `status` event and
/// D-Bus `GetStatus` send this same document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusDocument {
    /// [`STATUS_SCHEMA`].
    pub schema: u32,
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
    /// The edit direction's presets, as upstream lists its one set: factory and bonus presets
    /// under `built_in`, the user's own under `user_defined`.
    pub presets: PresetLists,
    /// The edit direction's preset by the name `--preset` takes; `null` while the lane has none.
    pub selected_preset: Option<String>,
    /// The speakers' presets, whichever lane the window is editing.
    pub output_presets: PresetLists,
    /// The microphone's voice presets, whichever lane the window is editing.
    pub input_presets: PresetLists,
    /// The playback devices PipeWire has now, by description, in `output_device_list`'s order:
    /// upstream's list, an array of names (`FxController.cpp:638-644`), which `status.go` reads
    /// as `[]string`.
    pub output_devices: Vec<String>,
    /// The same for the microphones.
    pub input_devices: Vec<String>,
    /// Every playback device the priority list knows, in its order, present or not, and any it
    /// does not know yet after them.
    pub output_device_list: Vec<DeviceStatus>,
    /// The same for the microphones.
    pub input_device_list: Vec<DeviceStatus>,
    /// The output lane's device by description, as upstream names it; `null` while detached.
    pub selected_output: Option<String>,
    /// The input lane's microphone by description; `null` while detached.
    pub selected_input: Option<String>,
    /// The edit direction's equalizer and gain stage, band by band.
    pub equalizer: Equalizer,
    /// Every application FxSound remembers, the ones playing or recording now first, with the
    /// presets of their own and what they run through now (`docs/0.4.0-apps.md`).
    pub apps: Vec<AppStatus>,
}

/// A value on a control's own scale, as the document prints it: exactly, and a whole number
/// without a fraction.
///
/// 0.3.0 printed the effect levels rounded to whole numbers, and a reader that decodes them into
/// an integer — Go's `encoding/json` into an `int`, for one — rejects `7.0`. Upstream prints them
/// as they are, and `--set_effect=bass:7.5` read back as 8 here (review item U14). So a whole
/// number goes out as `7`, which both kinds of reader take, and anything else as the shortest
/// decimal of the `f32` the controller holds rather than the noise of its widening
/// (`0.93_f32 as f64` is `0.9300000071525574`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level(pub f32);

impl Level {
    /// The whole number this is, if it is one.
    fn whole(self) -> Option<i64> {
        // Far inside `i64` and exact in `f32`; no control's scale comes near it.
        const LIMIT: f32 = 16_777_216.0;
        (self.0.is_finite() && self.0.fract() == 0.0 && self.0.abs() <= LIMIT)
            .then_some(self.0 as i64)
    }
}

impl Serialize for Level {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.whole() {
            Some(whole) => serializer.serialize_i64(whole),
            None => serializer.serialize_f64(exact(self.0)),
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.whole() {
            Some(whole) => write!(f, "{whole}"),
            None => write!(f, "{}", exact(self.0)),
        }
    }
}

/// `value` as the `f64` whose shortest decimal is the `f32`'s own, and `0` for what JSON cannot
/// carry.
pub(crate) fn exact(value: f32) -> f64 {
    if value.is_finite() {
        value.to_string().parse().unwrap_or(0.0)
    } else {
        0.0
    }
}

/// The five effect sliders, on their `0..=10` scale: 0.3.0's names, then upstream's names for the
/// two it calls otherwise (`FxController.cpp:666-670`), with the same values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EffectLevels {
    pub fidelity: Level,
    pub ambience: Level,
    pub surround: Level,
    pub dynamic_boost: Level,
    pub bass: Level,
    /// `fidelity`, by upstream's name.
    pub clarity: Level,
    /// `dynamic_boost`, by upstream's name.
    pub dynamicboost: Level,
}

/// One lane's presets, split as upstream splits them (`FxController.cpp:616-634`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PresetLists {
    pub built_in: Vec<PresetStatus>,
    pub user_defined: Vec<PresetStatus>,
}

/// One preset: its name and whether it carries unsaved changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresetStatus {
    pub name: String,
    pub modified: bool,
}

/// One device a lane can be attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceStatus {
    /// What `--output`/`--input` and the settings file take.
    pub node_name: String,
    /// What the window's combo shows; the one last seen for a device that is not present.
    pub description: String,
    /// Whether PipeWire has the device now.
    pub present: bool,
}

/// Upstream's `equalizer` block (`FxController.cpp:647-663`), for the edit direction.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Equalizer {
    pub num_bands: usize,
    /// dB.
    pub master_gain: Level,
    pub volume_leveling: f64,
    pub filter_q: f64,
    /// dB, negative to the left.
    pub balance: Level,
    pub bands: Vec<BandStatus>,
}

/// One equalizer band, with the range its frequency can be tuned over — what upstream's
/// frequency slider allows (`GraphicEqGet.cpp:105-168`), which its `printStatus` leaves out.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BandStatus {
    pub index: usize,
    /// Hz.
    pub frequency: f64,
    /// dB.
    pub gain: f64,
    /// Hz.
    pub min_frequency: f64,
    /// Hz.
    pub max_frequency: f64,
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
    let effect = |effect: Effect| Level(state.effect(effect));
    let presets = |lane: DeviceDirection| preset_lists(&app.lane_preset_list(lane));
    let output_device_list = device_list(app, DeviceDirection::Output);
    let input_device_list = device_list(app, DeviceDirection::Input);

    StatusDocument {
        schema: STATUS_SCHEMA,
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
            clarity: effect(Effect::Fidelity),
            dynamicboost: effect(Effect::DynamicBoost),
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
        presets: preset_lists(&state.presets),
        selected_preset: state.preset().map(|p| p.name.clone()),
        output_presets: presets(DeviceDirection::Output),
        input_presets: presets(DeviceDirection::Input),
        output_devices: device_names(&output_device_list),
        input_devices: device_names(&input_device_list),
        output_device_list,
        input_device_list,
        selected_output: state
            .device_for(DeviceDirection::Output)
            .map(|d| d.description.clone()),
        selected_input: state
            .device_for(DeviceDirection::Input)
            .map(|d| d.description.clone()),
        equalizer: equalizer(state),
        apps: app_statuses(app.app_rules(), app.app_streams()),
    }
}

/// A preset list split into factory and user presets, each keeping the picker's order.
fn preset_lists(entries: &[fxsound_ui::state::PresetEntry]) -> PresetLists {
    let (built_in, user_defined): (Vec<_>, Vec<_>) =
        entries.iter().partition(|entry| entry.factory);
    let status = |entries: Vec<&fxsound_ui::state::PresetEntry>| {
        entries
            .into_iter()
            .map(|entry| PresetStatus {
                name: entry.name.clone(),
                modified: entry.modified,
            })
            .collect()
    };
    PresetLists {
        built_in: status(built_in),
        user_defined: status(user_defined),
    }
}

/// `direction`'s devices: the priority list's, most preferred first, present or not, then any
/// device present that the list has not learnt yet — the order the window's combo offers them in.
fn device_list(app: &App, direction: DeviceDirection) -> Vec<DeviceStatus> {
    let live = |name: &str| {
        app.state
            .devices
            .iter()
            .find(|d| d.direction == direction && d.name == name)
    };
    let mut list: Vec<DeviceStatus> = crate::priority::ranked(app.settings(), direction)
        .map(|config| {
            let device = live(&config.device_id);
            DeviceStatus {
                node_name: config.device_id.clone(),
                description: device
                    .map_or_else(|| config.device_name.clone(), |d| d.description.clone()),
                present: device.is_some(),
            }
        })
        .collect();
    for device in app
        .state
        .devices
        .iter()
        .filter(|d| d.direction == direction)
    {
        if !list.iter().any(|known| known.node_name == device.name) {
            list.push(DeviceStatus {
                node_name: device.name.clone(),
                description: device.description.clone(),
                present: true,
            });
        }
    }
    list
}

/// Upstream's `output_devices`: of `list`, the devices present, by the name the combo shows,
/// in its order.
fn device_names(list: &[DeviceStatus]) -> Vec<String> {
    list.iter()
        .filter(|device| device.present)
        .map(|device| device.description.clone())
        .collect()
}

/// Upstream's `equalizer` block, from the edit direction's controls. The bands' ranges are the
/// equalizer's own (`fxsound_dsp::eq::band_frequency_range` over the band count's table).
fn equalizer(state: &UiState) -> Equalizer {
    let count = state.eq_bands.len();
    let (min_hz, max_hz) = fxsound_dsp::eq::band_table(count)
        .map_or((20.0, 20_000.0), |(_, min_hz, max_hz)| (min_hz, max_hz));
    Equalizer {
        num_bands: count,
        master_gain: Level(state.master_gain_db),
        volume_leveling: exact(state.volume_leveling),
        filter_q: exact(state.filter_q),
        balance: Level(state.balance_db),
        bands: state
            .eq_bands
            .iter()
            .enumerate()
            .map(|(index, band)| {
                let (low, high) =
                    fxsound_dsp::eq::band_frequency_range(index, count, min_hz, max_hz);
                BandStatus {
                    index,
                    frequency: exact(band.center_hz),
                    gain: exact(band.boost_db),
                    min_frequency: exact(low),
                    max_frequency: exact(high),
                }
            })
            .collect(),
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
pub(crate) fn rounded(value: f32, places: i32) -> f64 {
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

    /// What `fxmcp`'s `status.go` decodes (upstream `fxmcp/internal/fxsound/status.go:18-72`), by
    /// JSON kind: every key `FxController::printStatus` writes.
    fn assert_upstream_shape(json: &Value) {
        assert!(json["version"].is_string(), "version: {json}");
        assert!(json["power"].is_boolean(), "power: {json}");
        for list in ["built_in", "user_defined"] {
            for preset in json["presets"][list]
                .as_array()
                .expect("an array of presets")
            {
                assert!(preset["name"].is_string(), "{preset}");
                assert!(preset["modified"].is_boolean(), "{preset}");
            }
        }
        assert!(
            json["selected_preset"].is_string() || json["selected_preset"].is_null(),
            "{json}"
        );
        // `OutputDevices []string`: one object in it and the whole document fails to decode.
        for device in json["output_devices"]
            .as_array()
            .expect("an array of devices")
        {
            assert!(device.is_string(), "output_devices: {json}");
        }
        assert!(
            json["selected_output"].is_string() || json["selected_output"].is_null(),
            "{json}"
        );
        let equalizer = &json["equalizer"];
        assert!(equalizer["num_bands"].is_u64(), "{equalizer}");
        for key in ["master_gain", "volume_leveling", "filter_q", "balance"] {
            assert!(equalizer[key].is_number(), "equalizer.{key}: {equalizer}");
        }
        for band in equalizer["bands"].as_array().expect("an array of bands") {
            assert!(band["index"].is_u64(), "{band}");
            assert!(band["frequency"].is_number(), "{band}");
            assert!(band["gain"].is_number(), "{band}");
        }
        for key in ["clarity", "ambience", "surround", "dynamicboost", "bass"] {
            assert!(json["effects"][key].is_number(), "effects.{key}: {json}");
        }
    }

    #[test]
    fn the_json_is_a_superset_of_what_upstreams_status_writes_and_names_its_schema() {
        let mut a = app_with_presets("upstream-shape");
        a.receive(fxsound_core::messages::AudioToUi::Devices(mixed_devices()));
        let json = status(&mut a);
        assert_eq!(json["schema"].as_u64(), Some(u64::from(STATUS_SCHEMA)));
        assert_eq!(STATUS_SCHEMA, 2);
        assert_upstream_shape(&json);
        assert_eq!(
            json["output_devices"].as_array().map(Vec::len),
            Some(2),
            "both outputs, so every element was looked at: {json}"
        );
        assert_eq!(json["equalizer"]["num_bands"].as_u64(), Some(10));
        assert_eq!(
            json["equalizer"]["bands"].as_array().map(Vec::len),
            Some(10)
        );
        assert_eq!(json["selected_preset"], json["output"]["preset"]);

        // And with nothing listed and nothing selected, it still decodes.
        let mut bare = app();
        assert_upstream_shape(&status(&mut bare));
    }

    /// `fxmcp`'s `Status` (upstream `fxmcp/internal/fxsound/status.go:18-72`), field for field,
    /// in the serde types that take what the Go types take: a key of another JSON kind fails the
    /// decode, as it fails `json.Unmarshal` of the whole document. Go reads a `null` string as
    /// the empty one, hence the `Option`s.
    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code, reason = "decoding is the test; nothing reads the fields")]
    struct UpstreamStatus {
        version: String,
        power: bool,
        presets: UpstreamPresets,
        selected_preset: Option<String>,
        output_devices: Vec<String>,
        selected_output: Option<String>,
        equalizer: UpstreamEqualizer,
        effects: UpstreamEffects,
    }

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code, reason = "decoding is the test; nothing reads the fields")]
    struct UpstreamPresets {
        built_in: Vec<UpstreamPreset>,
        user_defined: Vec<UpstreamPreset>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code, reason = "decoding is the test; nothing reads the fields")]
    struct UpstreamPreset {
        name: String,
        modified: bool,
    }

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code, reason = "decoding is the test; nothing reads the fields")]
    struct UpstreamEqualizer {
        num_bands: i64,
        master_gain: f64,
        volume_leveling: f64,
        filter_q: f64,
        balance: f64,
        bands: Vec<UpstreamBand>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code, reason = "decoding is the test; nothing reads the fields")]
    struct UpstreamBand {
        index: i64,
        frequency: f64,
        gain: f64,
    }

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code, reason = "decoding is the test; nothing reads the fields")]
    struct UpstreamEffects {
        clarity: f64,
        ambience: f64,
        surround: f64,
        dynamicboost: f64,
        bass: f64,
    }

    #[test]
    fn the_json_decodes_into_the_status_type_fxmcp_reads_upstreams_into() {
        let mut a = app_with_presets("upstream-decode");
        a.receive(fxsound_core::messages::AudioToUi::Devices(mixed_devices()));
        run(&mut a, &[Command::Effects(vec![(Effect::Bass, 7.5)])]);
        let text = run(&mut a, &[Command::Status { json: true }]).stdout;
        let decoded: UpstreamStatus = serde_json::from_str(&text).expect("status.go's Status");
        assert_eq!(
            decoded.output_devices,
            [
                "Ryzen HD Audio Controller Analogue Stereo",
                "fifine Microphone Analogue Stereo"
            ]
        );

        let mut bare = app();
        let text = run(&mut bare, &[Command::Status { json: true }]).stdout;
        let decoded: UpstreamStatus = serde_json::from_str(&text).expect("status.go's Status");
        assert!(decoded.output_devices.is_empty());
        let _ = std::fs::remove_dir_all(user_dir("upstream-decode").parent().expect("the root"));
    }

    #[test]
    fn an_effect_set_to_a_fraction_reads_back_as_set_under_both_names() {
        // `--set_effect=bass:7.5` read back as 8 in 0.3.0 (review item U14).
        let mut a = app();
        run(
            &mut a,
            &[Command::Effects(vec![
                (Effect::Bass, 7.5),
                (Effect::Fidelity, 3.3),
                (Effect::DynamicBoost, 6.0),
            ])],
        );
        let json = status(&mut a);
        let effects = &json["effects"];
        assert_eq!(effects["bass"].as_f64(), Some(7.5));
        assert_eq!(
            effects["fidelity"].as_f64(),
            Some(3.3),
            "not 3.299999952316284"
        );
        assert_eq!(effects["clarity"], effects["fidelity"]);
        assert_eq!(
            effects["dynamic_boost"].as_i64(),
            Some(6),
            "a whole value stays whole"
        );
        assert_eq!(effects["dynamicboost"], effects["dynamic_boost"]);
        let lines = status_lines(&mut a);
        assert_eq!(line(&lines, "bass"), "7.5");
        assert_eq!(line(&lines, "dynamic_boost"), "6");
    }

    #[test]
    fn a_level_is_whole_when_it_is_whole_and_exact_when_it_is_not() {
        let json = |value: f32| serde_json::to_string(&Level(value)).expect("serialises");
        assert_eq!(json(7.0), "7");
        assert_eq!(json(-6.0), "-6");
        assert_eq!(json(-0.0), "0");
        assert_eq!(json(7.5), "7.5");
        assert_eq!(json(0.93), "0.93");
        assert_eq!(json(f32::NAN), "0.0");
        assert_eq!(Level(-0.5).to_string(), "-0.5");
        assert_eq!(Level(10.0).to_string(), "10");
    }

    #[test]
    fn the_presets_of_each_lane_are_split_into_built_in_and_user_defined() {
        let tag = "preset-lists";
        let mut a = app_with_presets(tag);
        let first = a.state.presets[0].name.clone();
        run(&mut a, &[preset(PresetCommand::Select(first))]);
        run(&mut a, &[Command::BandGains(vec![(0, 6.0)])]);
        let outcome = run(&mut a, &[preset(PresetCommand::SaveAs("Mine".into()))]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        run(&mut a, &[Command::BandGains(vec![(0, -2.0)])]);

        let names = |lists: &Value, list: &str| -> Vec<(String, bool)> {
            lists[list]
                .as_array()
                .expect("an array")
                .iter()
                .map(|p| {
                    (
                        p["name"].as_str().expect("a name").to_owned(),
                        p["modified"].as_bool().expect("a flag"),
                    )
                })
                .collect()
        };
        let json = status(&mut a);
        let output = &json["output_presets"];
        assert_eq!(
            names(output, "built_in"),
            [("Alpha".to_owned(), false), ("Beta".to_owned(), false)]
        );
        assert_eq!(names(output, "user_defined"), [("Mine".to_owned(), true)]);
        assert_eq!(json["presets"], *output, "the edit direction's");
        assert_eq!(json["selected_preset"], "Mine", "without the `*`");
        assert_eq!(
            names(&json["input_presets"], "built_in"),
            [
                ("Clean Voice".to_owned(), false),
                ("Flat Voice".to_owned(), false)
            ]
        );

        // The other way round, and the speakers' list keeps its unsaved changes off screen.
        run(&mut a, &[Command::EditDirection(DeviceDirection::Input)]);
        let json = status(&mut a);
        assert_eq!(json["presets"], json["input_presets"]);
        assert_eq!(
            names(&json["output_presets"], "user_defined"),
            [("Mine".to_owned(), true)]
        );
        assert_eq!(json["selected_preset"], json["input"]["preset"]);
        let _ = std::fs::remove_dir_all(user_dir(tag).parent().expect("the root"));
    }

    #[test]
    fn the_device_lists_follow_the_priority_list_and_say_which_devices_are_present() {
        use fxsound_core::messages::AudioToUi;
        let mut a = app();
        a.receive(AudioToUi::Devices(mixed_devices()));
        // The fifine unplugged: the list still knows both halves of it.
        let without_fifine: Vec<AudioDevice> = mixed_devices()
            .into_iter()
            .filter(|d| !d.name.contains("fifine"))
            .collect();
        a.receive(AudioToUi::Devices(without_fifine));
        run(
            &mut a,
            &[Command::Output(OutputCommand::Select(
                "alsa_output.pci".into(),
            ))],
        );

        let json = status(&mut a);
        let devices = |key: &str| -> Vec<(String, String, bool)> {
            json[key]
                .as_array()
                .expect("an array")
                .iter()
                .map(|d| {
                    (
                        d["node_name"].as_str().expect("a node name").to_owned(),
                        d["description"].as_str().expect("a description").to_owned(),
                        d["present"].as_bool().expect("a flag"),
                    )
                })
                .collect()
        };
        let outputs = devices("output_device_list");
        assert_eq!(
            outputs
                .iter()
                .map(|(name, _, present)| (name.as_str(), *present))
                .collect::<Vec<_>>(),
            [("alsa_output.pci", true), ("alsa_output.usb-fifine", false)]
        );
        assert_eq!(
            outputs[1].1, "fifine Microphone Analogue Stereo",
            "the description last seen"
        );
        let inputs = devices("input_device_list");
        assert_eq!(inputs.len(), 2, "{inputs:?}");
        assert!(
            inputs
                .iter()
                .all(|(name, ..)| name.starts_with("alsa_input."))
        );
        // Upstream's lists: the names of the devices there are, the unplugged one left out.
        assert_eq!(
            json["output_devices"],
            serde_json::json!(["Ryzen HD Audio Controller Analogue Stereo"])
        );
        assert_eq!(
            json["input_devices"],
            serde_json::json!(["Ryzen HD Audio Controller Analogue Stereo"])
        );
        assert_eq!(
            json["selected_output"],
            "Ryzen HD Audio Controller Analogue Stereo"
        );
        assert_eq!(json["selected_output"], json["output"]["device"]);
        assert_eq!(json["selected_input"], json["input"]["device"]);
    }

    #[test]
    fn a_device_the_priority_list_has_not_learnt_yet_is_listed_after_the_ones_it_has() {
        use fxsound_core::messages::AudioToUi;
        let mut a = app();
        let pci = |d: &AudioDevice| d.name.ends_with(".pci");
        a.receive(AudioToUi::Devices(
            mixed_devices().into_iter().filter(pci).collect(),
        ));
        // A list the controller has not learnt from yet, the newcomer first.
        let (mut devices, known): (Vec<AudioDevice>, Vec<AudioDevice>) =
            mixed_devices().into_iter().partition(|d| !pci(d));
        devices.extend(known);
        a.state.devices = devices;

        let json = status(&mut a);
        for (key, expected) in [
            (
                "output_device_list",
                ["alsa_output.pci", "alsa_output.usb-fifine"],
            ),
            (
                "input_device_list",
                ["alsa_input.pci", "alsa_input.usb-fifine"],
            ),
        ] {
            let listed: Vec<(&str, bool)> = json[key]
                .as_array()
                .expect("an array")
                .iter()
                .map(|d| {
                    (
                        d["node_name"].as_str().expect("a node name"),
                        d["present"].as_bool().expect("a flag"),
                    )
                })
                .collect();
            assert_eq!(listed, [(expected[0], true), (expected[1], true)], "{key}");
        }
        assert_eq!(
            json["output_devices"],
            serde_json::json!([
                "Ryzen HD Audio Controller Analogue Stereo",
                "fifine Microphone Analogue Stereo"
            ]),
            "upstream's list, in the same order"
        );
    }

    #[test]
    fn each_band_reports_the_range_the_windows_frequency_slider_allows() {
        let mut a = app();
        run(
            &mut a,
            &[
                Command::MasterGain(-3.0),
                Command::Balance(2.0),
                Command::VolumeLeveling(1.5),
                Command::BandGains(vec![(2, 4.5)]),
            ],
        );
        for count in [10_usize, 5, 15, 20, 31] {
            run(&mut a, &[Command::NumBands(count as u32)]);
            let json = status(&mut a);
            let equalizer = &json["equalizer"];
            assert_eq!(equalizer["num_bands"].as_u64(), Some(count as u64));
            let bands = equalizer["bands"].as_array().expect("an array");
            assert_eq!(bands.len(), count);
            for (index, band) in bands.iter().enumerate() {
                let (low, high) =
                    fxsound_ui::widgets::equalizer::band_frequency_range(index, count);
                assert_eq!(band["index"].as_u64(), Some(index as u64));
                assert_eq!(band["min_frequency"].as_f64(), Some(exact(low)), "{band}");
                assert_eq!(band["max_frequency"].as_f64(), Some(exact(high)), "{band}");
                let frequency = band["frequency"].as_f64().expect("a frequency");
                assert!(
                    (exact(low)..=exact(high)).contains(&frequency),
                    "{count} bands: {band}"
                );
                assert_eq!(
                    band["frequency"].as_f64(),
                    Some(exact(a.state.eq_bands[index].center_hz))
                );
            }
        }
        let json = status(&mut a);
        let equalizer = &json["equalizer"];
        assert_eq!(
            equalizer["bands"][30]["max_frequency"].as_f64(),
            Some(20_000.0)
        );
        assert_eq!(equalizer["master_gain"].as_i64(), Some(-3));
        assert_eq!(equalizer["balance"].as_i64(), Some(2));
        assert_eq!(equalizer["volume_leveling"].as_f64(), Some(1.5));
        assert_eq!(equalizer["filter_q"], json["filter_q"]);
        assert_eq!(
            json["master_gain_db"].as_i64(),
            Some(-3),
            "0.3.0's key as it was"
        );
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
    fn next_output_and_next_input_walk_their_priority_lists_in_order() {
        use fxsound_core::messages::AudioToUi;
        use fxsound_ui::dialogs::settings::SettingsAction;
        let mut a = app();
        // The first list ranks each direction as the engine names it; Settings then puts the
        // fifine output and the Ryzen microphone first.
        a.receive(AudioToUi::Devices(mixed_devices()));
        let mut pane = a.settings_state();
        a.handle_settings(&SettingsAction::MoveDeviceUp(1), &mut pane);
        a.handle_settings(&SettingsAction::MoveMicrophoneUp(1), &mut pane);
        let names: Vec<&str> = a.state.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "alsa_output.usb-fifine",
                "alsa_output.pci",
                "alsa_input.pci",
                "alsa_input.usb-fifine"
            ]
        );
        let output = |a: &App| {
            a.state
                .device_for(DeviceDirection::Output)
                .map(|d| d.name.clone())
        };
        let input = |a: &App| {
            a.state
                .device_for(DeviceDirection::Input)
                .map(|d| d.name.clone())
        };

        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(
            output(&a).as_deref(),
            Some("alsa_output.usb-fifine"),
            "the top of the list"
        );
        run(&mut a, &[Command::Output(OutputCommand::Next)]);
        assert_eq!(output(&a).as_deref(), Some("alsa_output.pci"));
        run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert_eq!(input(&a).as_deref(), Some("alsa_input.pci"));
        run(&mut a, &[Command::Input(InputCommand::Next)]);
        assert_eq!(input(&a).as_deref(), Some("alsa_input.usb-fifine"));
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
    fn a_voice_preset_picked_while_the_microphone_waits_is_the_one_it_remembers() {
        // What a held `--output` does with the `--preset` beside it, a held `--input` does too:
        // once the list arrives the microphone keeps the voice preset and remembers it, under
        // its own lane, for the next time it is picked.
        use fxsound_core::DeviceDirection::{Input, Output};
        let mut a = app_with_presets("held-microphone-remembers");
        let outcome = run_line(&mut a, &["--input=alsa_input.pci", "--preset=Flat Voice"]);
        assert!(!outcome.failed, "{:?}", outcome.stderr);
        assert_eq!(
            a.settings().preset_for_device("alsa_input.pci", Input),
            None,
            "nothing to remember it against yet"
        );

        devices_arrive(&mut a);
        assert_eq!(a.lane_preset(Input), Some(("Flat Voice", false)));
        assert_eq!(
            a.settings().preset_for_device("alsa_input.pci", Input),
            Some("Flat Voice")
        );
        assert_eq!(
            a.settings().preset_for_device("alsa_input.pci", Output),
            None
        );
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

    /// The directory `app_with_presets(tag)` keeps its user presets in: the speakers' `.fac`
    /// files directly, the microphone's voice presets under `Input/`.
    fn user_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("fxsound-commands-{}-{tag}", std::process::id()))
            .join("user")
    }

    /// Where `lane` files a user preset called `name` under `user`, and where the other lane
    /// would have, had its kind of file been written for it.
    fn files(user: &std::path::Path, lane: DeviceDirection, name: &str) -> [std::path::PathBuf; 2] {
        let music = user.join(format!("{name}.fac"));
        let voice = user.join("Input").join(format!("{name}.toml"));
        match lane {
            DeviceDirection::Output => [music, voice],
            DeviceDirection::Input => [voice, music],
        }
    }

    fn preset(command: PresetCommand) -> Command {
        Command::Preset(command)
    }

    #[test]
    fn the_preset_commands_save_undo_rename_and_delete_in_either_lane() {
        // The hamburger's items from the command line: the same on the speakers' `.fac` set and
        // on the microphone's voice set, each lane's files in its own store and of its own kind.
        for lane in DeviceDirection::ALL {
            let tag = format!("preset-commands-{}", lane.key());
            let mut a = app_with_presets(&tag);
            let user = user_dir(&tag);
            run(&mut a, &[Command::EditDirection(lane)]);
            let first = a.state.presets[0].name.clone();
            run(&mut a, &[preset(PresetCommand::Select(first))]);
            run(&mut a, &[Command::BandGains(vec![(0, 6.0)])]);
            assert!(a.state.preset().is_some_and(|p| p.modified), "{lane:?}");

            let outcome = run(&mut a, &[preset(PresetCommand::SaveAs("Mine".into()))]);
            assert!(!outcome.failed, "{lane:?}: {}", outcome.stderr);
            let [own, other] = files(&user, lane, "Mine");
            assert!(own.is_file(), "{lane:?}: {}", own.display());
            assert!(!other.exists(), "{lane:?}: {}", other.display());
            assert_eq!(a.lane_preset(lane), Some(("Mine", false)), "{lane:?}");
            assert!(a.lane_has_preset(lane, "Mine"));
            assert!(!a.lane_has_preset(lane.other(), "Mine"));

            run(&mut a, &[Command::BandGains(vec![(0, -3.0)])]);
            assert_eq!(a.lane_preset(lane), Some(("Mine", true)), "{lane:?}");
            run(&mut a, &[preset(PresetCommand::Overwrite)]);
            assert_eq!(a.lane_preset(lane), Some(("Mine", false)), "{lane:?}");

            run(&mut a, &[Command::BandGains(vec![(0, 2.0)])]);
            run(&mut a, &[preset(PresetCommand::Undo)]);
            assert_eq!(a.lane_preset(lane), Some(("Mine", false)), "{lane:?}");
            assert_eq!(a.state.eq_bands[0].boost_db, -3.0, "{lane:?}: as saved");

            run(&mut a, &[preset(PresetCommand::Rename("Yours".into()))]);
            let [renamed, _] = files(&user, lane, "Yours");
            assert!(renamed.is_file(), "{lane:?}");
            assert!(!own.exists(), "{lane:?}");
            assert_eq!(a.lane_preset(lane), Some(("Yours", false)), "{lane:?}");

            run(&mut a, &[preset(PresetCommand::Delete)]);
            assert!(!renamed.exists(), "{lane:?}");
            assert!(!a.lane_has_preset(lane, "Yours"), "{lane:?}");
            let _ = std::fs::remove_dir_all(user.parent().expect("the root"));
        }
    }

    #[test]
    fn a_user_voice_preset_is_selected_by_name_and_reported_with_its_changes() {
        let tag = "user-voice";
        let mut a = app_with_presets(tag);
        run(
            &mut a,
            &[
                Command::EditDirection(DeviceDirection::Input),
                preset(PresetCommand::Select("Flat Voice".into())),
                Command::BandGains(vec![(0, 6.0)]),
                preset(PresetCommand::SaveAs("Mine".into())),
                preset(PresetCommand::Select("Clean Voice".into())),
            ],
        );
        let outcome = run(&mut a, &[preset(PresetCommand::Select("Mine".into()))]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert_eq!(a.settings().input_preset, "Mine");

        run(&mut a, &[Command::BandGains(vec![(1, 3.0)])]);
        let json = status(&mut a);
        assert_eq!(json["input"]["preset"], "Mine");
        assert_eq!(json["input"]["modified"], true, "{json}");
        assert_eq!(json["preset"], "Mine*");

        // From the speakers, the voice preset is the other lane's, and the refusal says so.
        run(&mut a, &[Command::EditDirection(DeviceDirection::Output)]);
        let outcome = run(&mut a, &[preset(PresetCommand::Select("Mine".into()))]);
        assert!(outcome.failed);
        assert!(
            outcome.stderr.contains("--edit=input"),
            "{}",
            outcome.stderr
        );
        let _ = std::fs::remove_dir_all(user_dir(tag).parent().expect("the root"));
    }

    #[test]
    fn num_bands_on_the_command_line_carries_the_curve_over() {
        // U1: `--num_bands` goes through the same remap as the window, rather than wiping the
        // curve flat — by frequency (audit report #13).
        let mut a = app();
        run(&mut a, &[Command::BandGains(vec![(0, 6.0), (9, -3.0)])]);
        let ten: Vec<f32> = a.state.eq_bands.iter().map(|b| b.boost_db).collect();
        run(&mut a, &[Command::NumBands(31)]);
        let gains: Vec<f32> = a.state.eq_bands.iter().map(|b| b.boost_db).collect();
        assert_eq!(gains, fxsound_dsp::eq::remap_band_gains(&ten, 31));
        assert!(
            (gains[5] - 6.0).abs() < 0.2,
            "the 62.5 Hz boost stays at 63 Hz: {gains:?}"
        );
        assert_eq!(gains[29], -3.0, "the 16 kHz cut stays at 16 kHz");
        assert_eq!(a.settings().num_bands, 31);
    }

    #[test]
    fn a_preset_command_the_menu_would_grey_out_is_refused_and_changes_nothing() {
        // U15: the same rule as the hamburger menu, with the reason on stderr and a non-zero exit
        // instead of a user copy shadowing a factory preset or a save nobody asked for.
        let tag = "refusals";
        let mut a = app_with_presets(tag);
        let user = user_dir(tag);
        run(&mut a, &[preset(PresetCommand::Select("Alpha".into()))]);
        for (command, words) in [
            (PresetCommand::Overwrite, "factory"),
            (PresetCommand::Rename("Mine".into()), "factory"),
            (PresetCommand::Delete, "factory"),
            (PresetCommand::Undo, "no unsaved changes"),
        ] {
            let outcome = run(&mut a, &[preset(command.clone())]);
            assert!(outcome.failed, "{command:?}");
            assert!(
                outcome.stderr.contains(words),
                "{command:?}: {}",
                outcome.stderr
            );
        }

        run(&mut a, &[Command::BandGains(vec![(0, 6.0)])]);
        let outcome = run(&mut a, &[preset(PresetCommand::Overwrite)]);
        assert!(outcome.failed);
        assert!(
            outcome.stderr.contains("--save_preset"),
            "{}",
            outcome.stderr
        );
        let outcome = run(&mut a, &[preset(PresetCommand::SaveAs("beta".into()))]);
        assert!(outcome.failed);
        assert!(
            outcome.stderr.contains("already exists"),
            "{}",
            outcome.stderr
        );

        let saved: Vec<_> = std::fs::read_dir(&user)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "fac"))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            saved,
            Vec::<std::path::PathBuf>::new(),
            "no user preset was written"
        );
        assert_eq!(
            a.lane_preset(DeviceDirection::Output),
            Some(("Alpha", true)),
            "the edits are still there to save under a new name"
        );
        let outcome = run(&mut a, &[preset(PresetCommand::SaveAs("Mine".into()))]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        let _ = std::fs::remove_dir_all(user.parent().expect("the root"));
    }

    #[test]
    fn save_preset_on_a_preset_with_no_unsaved_changes_saves_a_copy_of_it() {
        // 0.4.0 audit #17: `--save_preset=Copy` on a clean preset was refused, so copying a
        // factory preset meant moving a slider and moving it back first.
        let tag = "save-a-copy";
        let mut a = app_with_presets(tag);
        let user = user_dir(tag);
        run(&mut a, &[preset(PresetCommand::Select("Alpha".into()))]);
        let outcome = run(&mut a, &[preset(PresetCommand::SaveAs("Copy".into()))]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert!(user.join("Copy.fac").is_file(), "the copy was written");
        assert_eq!(
            a.lane_preset(DeviceDirection::Output),
            Some(("Copy", false))
        );
        assert!(
            a.state
                .presets
                .iter()
                .any(|p| p.name == "Alpha" && p.factory),
            "the preset it copies is still there"
        );
        let _ = std::fs::remove_dir_all(user.parent().expect("the root"));
    }

    #[test]
    fn a_line_stops_at_nothing_and_still_fails_for_the_command_that_was_refused() {
        // Each command is answered on its own: a refused one fails the line without undoing or
        // skipping the others, as an unknown device already does.
        let tag = "refusal-in-a-line";
        let mut a = app_with_presets(tag);
        let outcome = run(
            &mut a,
            &[
                preset(PresetCommand::Select("Alpha".into())),
                preset(PresetCommand::Delete),
                preset(PresetCommand::Select("Beta".into())),
            ],
        );
        assert!(outcome.failed);
        assert_eq!(outcome.stderr.lines().count(), 1, "{}", outcome.stderr);
        assert_eq!(
            a.lane_preset(DeviceDirection::Output),
            Some(("Beta", false))
        );
        let _ = std::fs::remove_dir_all(user_dir(tag).parent().expect("the root"));
    }

    #[test]
    fn preset_commands_run_with_the_power_off_where_the_original_ignored_them() {
        // A deliberate difference (README): the menu greys its preset items out while the power
        // is off, and the original's command line ignores them, but a script here is answered.
        let tag = "power-off";
        let mut a = app_with_presets(tag);
        let outcome = run(
            &mut a,
            &[
                Command::Power(PowerCommand::Off),
                preset(PresetCommand::Select("Beta".into())),
                Command::BandGains(vec![(0, 6.0)]),
                preset(PresetCommand::SaveAs("Mine".into())),
            ],
        );
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert!(!a.state.power);
        assert_eq!(
            a.lane_preset(DeviceDirection::Output),
            Some(("Mine", false))
        );
        assert!(!a.preset_menu().delete, "while the menu offers nothing");
        let _ = std::fs::remove_dir_all(user_dir(tag).parent().expect("the root"));
    }

    // ---- per-application presets --------------------------------------------------------------

    use crate::audio_link::FakeEngine;
    use fxsound_core::messages::AudioToUi;
    use fxsound_core::{AppKey, UiToAudio};

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    fn key(binary: &str, name: &str, flatpak: &str) -> AppKey {
        AppKey {
            binary: binary.to_owned(),
            name: name.to_owned(),
            flatpak: flatpak.to_owned(),
        }
    }

    fn battlefield() -> AppKey {
        key("bf6.exe", "Battlefield 6", "")
    }

    fn brave() -> AppKey {
        key("brave", "Brave", "")
    }

    fn discord() -> AppKey {
        key("Discord", "Discord", "com.discordapp.Discord")
    }

    fn stream(id: u32, direction: DeviceDirection, app: &AppKey) -> AppStream {
        AppStream {
            id,
            direction,
            app: app.clone(),
            route: None,
        }
    }

    fn on_route(id: u32, direction: DeviceDirection, app: &AppKey, preset: &str) -> AppStream {
        AppStream {
            route: Some(preset.to_owned()),
            ..stream(id, direction, app)
        }
    }

    /// An app started against a stand-in engine, both lanes on: the speakers' presets `Gaming`,
    /// `Music` and `Volume Boost`, the microphone's `Clean` and `Headset`. What start-up said and
    /// sent is taken already, and the store is kept in memory.
    fn with_apps() -> (App, FakeEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("scratch directory");
        let factory = dir.path().join("factory");
        std::fs::create_dir_all(&factory).expect("factory directory");
        for name in ["Gaming", "Music", "Volume Boost"] {
            let preset = fxsound_core::Preset {
                name: name.to_owned(),
                ..fxsound_core::Preset::default()
            };
            fxsound_preset::save(&preset, &factory.join(format!("{name}.fac"))).expect("write");
        }
        let mut music =
            fxsound_preset::PresetStore::with_dirs(vec![factory], dir.path().join("user"));
        music.rescan();
        let voice = |name: &str| fxsound_preset::input::InputPreset {
            name: name.to_owned(),
            ..fxsound_preset::input::InputPreset::default()
        };
        let voices = crate::app::voice_store_for_tests(
            &[voice("Clean"), voice("Headset")],
            &dir.path().join("voice-factory"),
            dir.path().join("user").join("Input"),
        );
        let mut settings = fxsound_core::Settings::default();
        settings.output_preset = "Music".to_owned();
        settings.input_preset = "Clean".to_owned();
        settings.set_lane_enabled(IN, true);
        let engine = FakeEngine::new();
        let mut app = App::start_for_tests(settings, music, voices, &engine);
        let _ = engine.take_sent();
        let _ = app.drain_events();
        (app, engine, dir)
    }

    /// The engine reports these streams, and the app takes the report.
    fn play(app: &mut App, engine: &FakeEngine, streams: Vec<AppStream>) {
        engine.feed(AudioToUi::AppStreams(streams));
        app.poll_audio();
    }

    /// The routes the controller last sent the engine since the last look, as (lane, application,
    /// preset); `None` when it sent none.
    fn routes_sent(engine: &FakeEngine) -> Option<Vec<(DeviceDirection, String, String)>> {
        engine
            .take_sent()
            .into_iter()
            .rev()
            .find_map(|message| match message {
                UiToAudio::SetAppRoutes(routes) => Some(
                    routes
                        .iter()
                        .map(|route| {
                            (
                                route.direction,
                                route.app.display().to_owned(),
                                route.preset.clone(),
                            )
                        })
                        .collect(),
                ),
                _ => None,
            })
    }

    /// What the rule for `app` says for `lane`, as the store has it.
    fn chosen(a: &App, app: &AppKey, lane: DeviceDirection) -> String {
        a.app_rules()
            .rule(app)
            .map(|rule| rule.preset(lane).to_owned())
            .unwrap_or_default()
    }

    fn triple(lane: DeviceDirection, app: &str, preset: &str) -> (DeviceDirection, String, String) {
        (lane, app.to_owned(), preset.to_owned())
    }

    #[test]
    fn app_preset_reaches_a_remembered_application_by_program_name_or_flatpak_id_in_any_case() {
        let (mut a, engine, _dir) = with_apps();
        play(
            &mut a,
            &engine,
            vec![
                stream(1, OUT, &battlefield()),
                stream(2, OUT, &brave()),
                stream(3, IN, &discord()),
            ],
        );
        let _ = engine.take_sent();
        let direction = a.state.direction;

        let outcome = run_line(
            &mut a,
            &[
                "--app-preset=BF6.EXE=Gaming",
                "--app-preset=brave=Volume Boost",
                "--app-input-preset=COM.DISCORDAPP.DISCORD=Headset",
            ],
        );
        assert!(
            !outcome.failed && outcome.stderr.is_empty(),
            "{}",
            outcome.stderr
        );
        assert!(outcome.window.is_empty(), "no window: {:?}", outcome.window);
        assert_eq!(a.state.direction, direction, "the edit direction stays");
        assert_eq!(a.app_rules().apps.len(), 3, "no rule was added");
        assert_eq!(chosen(&a, &battlefield(), OUT), "Gaming");
        assert_eq!(chosen(&a, &brave(), OUT), "Volume Boost");
        assert_eq!(chosen(&a, &discord(), IN), "Headset");
        assert_eq!(
            routes_sent(&engine),
            Some(vec![
                triple(OUT, "Battlefield 6", "Gaming"),
                triple(OUT, "Brave", "Volume Boost"),
                // Discord's rule follows the speakers, and it could outrank the two above for a
                // stream that carried its id and their program or name.
                triple(OUT, "Discord", ""),
                triple(IN, "Discord", "Headset"),
            ]),
            "sent at once, the way the Settings pane's choice is"
        );

        // By the name the window shows, in another case.
        let outcome = run_line(&mut a, &["--app-preset=battlefield 6=Music"]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert_eq!(chosen(&a, &battlefield(), OUT), "Music");
        assert_eq!(a.app_rules().apps.len(), 3);
    }

    #[test]
    fn the_flatpak_id_decides_before_the_program_and_the_program_before_the_name() {
        let (mut a, engine, _dir) = with_apps();
        let vesktop = key("vesktop", "Discord", "");
        let canary = key("Discord", "Discord Canary", "com.discordapp.DiscordCanary");
        let spotify = key("", "Spotify", "");
        play(
            &mut a,
            &engine,
            vec![
                stream(1, IN, &vesktop),
                stream(2, IN, &canary),
                stream(3, OUT, &spotify),
            ],
        );

        // No Flatpak is called `discord`, and a program is: the one called `Discord Canary`.
        run_line(&mut a, &["--app-input-preset=discord=Headset"]);
        assert_eq!(chosen(&a, &canary, IN), "Headset");
        assert_eq!(
            chosen(&a, &vesktop, IN),
            "",
            "its name is Discord, its program is not"
        );

        run_line(
            &mut a,
            &["--app-input-preset=com.discordapp.discordcanary=Clean"],
        );
        assert_eq!(chosen(&a, &canary, IN), "Clean");
        run_line(&mut a, &["--app-input-preset=/usr/bin/VESKTOP=Headset"]);
        assert_eq!(
            chosen(&a, &vesktop, IN),
            "Headset",
            "by the program's last component"
        );
        run_line(&mut a, &["--app-preset=SPOTIFY=Music"]);
        assert_eq!(
            chosen(&a, &spotify, OUT),
            "Music",
            "by its name, when that is all it has"
        );
        assert_eq!(a.app_rules().apps.len(), 3);
    }

    #[test]
    fn a_name_two_remembered_applications_answer_to_gives_both_the_preset() {
        let (mut a, engine, _dir) = with_apps();
        let native = key("firefox", "Firefox", "");
        let sandboxed = key("firefox", "Firefox", "org.mozilla.firefox");
        play(&mut a, &engine, vec![stream(1, OUT, &native)]);
        a.set_app_preset(&sandboxed, IN, Some("Headset"))
            .expect("a rule of the sandboxed one's own");
        assert_eq!(a.app_rules().apps.len(), 2);

        run_line(&mut a, &["--app-preset=Firefox=Gaming"]);
        assert_eq!(chosen(&a, &native, OUT), "Gaming");
        assert_eq!(chosen(&a, &sandboxed, OUT), "Gaming");

        run_line(&mut a, &["--app-preset=org.mozilla.firefox=Music"]);
        assert_eq!(
            chosen(&a, &sandboxed, OUT),
            "Music",
            "the Flatpak id tells them apart"
        );
        assert_eq!(chosen(&a, &native, OUT), "Gaming");
    }

    #[test]
    fn a_running_application_a_rule_covers_under_other_names_is_still_reached_by_its_own() {
        let (mut a, engine, _dir) = with_apps();
        let native = key("firefox", "Firefox", "");
        let sandboxed = key("firefox", "Firefox", "org.mozilla.firefox");
        play(&mut a, &engine, vec![stream(1, OUT, &native)]);
        a.set_app_preset(&native, IN, Some("Clean"))
            .expect("chosen");
        // The Flatpak one plays now, and the native one's rule covers it: its id is in no rule.
        play(&mut a, &engine, vec![stream(2, OUT, &sandboxed)]);
        assert_eq!(a.app_rules().apps.len(), 1);
        let _ = engine.take_sent();

        let outcome = run_line(&mut a, &["--app-preset=org.mozilla.firefox=Gaming"]);
        assert!(
            outcome.stderr.is_empty(),
            "it is running: {}",
            outcome.stderr
        );
        assert_eq!(a.app_rules().apps.len(), 2, "a rule of its own");
        assert_eq!(chosen(&a, &sandboxed, OUT), "Gaming");
        assert_eq!(chosen(&a, &sandboxed, IN), "Clean", "carried over");
        assert_eq!(chosen(&a, &native, OUT), "", "the native one is untouched");
        assert!(routes_sent(&engine).is_some(), "sent at once");
        // The engine runs what the store says for each: the native rule follows the speakers
        // beside the Flatpak's on Gaming, which matches the native stream by its program too.
        let lane = |direction: DeviceDirection| -> Vec<&fxsound_core::messages::AppRoute> {
            a.app_routes()
                .iter()
                .filter(|route| route.direction == direction)
                .collect()
        };
        let engine_runs = |direction: DeviceDirection, app: &AppKey| {
            let lane = lane(direction);
            app.best_match(lane.iter().map(|route| &route.app))
                .map(|at| lane[at].preset.clone())
                .unwrap_or_default()
        };
        assert_eq!(engine_runs(OUT, &sandboxed), "Gaming");
        assert_eq!(engine_runs(OUT, &native), "");
        assert_eq!(engine_runs(IN, &sandboxed), "Clean");
        assert_eq!(engine_runs(IN, &native), "Clean");
    }

    #[test]
    fn an_application_not_seen_yet_gets_a_rule_for_the_program_of_that_name_and_a_note() {
        let (mut a, engine, _dir) = with_apps();
        let outcome = run_line(&mut a, &["--app-preset=bf6.exe=Gaming"]);
        assert!(!outcome.failed, "not an error: {}", outcome.stderr);
        assert!(
            outcome
                .stderr
                .starts_with("note: FxSound has not seen an application called \"bf6.exe\"")
                && outcome
                    .stderr
                    .contains("kept for the program \"bf6.exe\" and runs once it plays"),
            "{}",
            outcome.stderr
        );
        assert_eq!(a.app_rules().apps.len(), 1);
        assert_eq!(a.app_rules().apps[0].key, key("bf6.exe", "", ""));
        assert_eq!(a.app_rules().apps[0].output_preset, "Gaming");
        assert_eq!(
            routes_sent(&engine),
            Some(vec![triple(OUT, "bf6.exe", "Gaming")]),
            "the engine has the rule before the game plays"
        );

        // The game starts: the rule was waiting for it.
        play(&mut a, &engine, vec![stream(1, OUT, &battlefield())]);
        assert_eq!(routes_sent(&engine), None, "nothing new to say");
        assert_eq!(
            a.app_rules().apps.len(),
            1,
            "the game is the program of the rule"
        );

        let outcome = run_line(&mut a, &["--app-input-preset=OBS=Headset"]);
        assert!(
            outcome.stderr.contains("once it records"),
            "{}",
            outcome.stderr
        );
    }

    #[test]
    fn a_flatpak_id_or_a_name_not_seen_yet_is_kept_as_what_it_is_and_the_note_says_so() {
        let (mut a, engine, _dir) = with_apps();
        // The manual's own example, on a first run.
        let outcome = run_line(
            &mut a,
            &["--app-input-preset=com.discordapp.Discord=Headset"],
        );
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains(
                "kept for the Flatpak \"com.discordapp.Discord\" and runs once it records"
            ),
            "{}",
            outcome.stderr
        );
        let outcome = run_line(&mut a, &["--app-preset=Battlefield 6=Gaming"]);
        assert!(
            outcome
                .stderr
                .contains("kept for the application called \"Battlefield 6\""),
            "{}",
            outcome.stderr
        );
        assert_eq!(
            chosen(&a, &key("", "", "com.discordapp.Discord"), IN),
            "Headset"
        );
        assert_eq!(chosen(&a, &key("", "Battlefield 6", ""), OUT), "Gaming");

        // Both start: each is its rule's application, and no second rule is added for either.
        play(
            &mut a,
            &engine,
            vec![stream(1, IN, &discord()), stream(2, OUT, &battlefield())],
        );
        assert_eq!(a.app_rules().apps.len(), 2, "{:?}", a.app_rules());
        assert_eq!(chosen(&a, &discord(), IN), "Headset");
        assert_eq!(chosen(&a, &battlefield(), OUT), "Gaming");
    }

    #[test]
    fn a_cold_start_holds_a_name_it_does_not_know_and_says_so() {
        let (mut a, engine, _dir) = with_apps();
        a.hold_app_presets();
        let outcome = run_line(&mut a, &["--app-preset=Firefox=Music"]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert!(
            outcome
                .stderr
                .starts_with("note: FxSound remembers no application called \"Firefox\"")
                && outcome
                    .stderr
                    .contains("or else for the program \"Firefox\""),
            "{}",
            outcome.stderr
        );
        assert!(a.app_rules().apps.is_empty());

        // Firefox plays under a program of another name: the name reaches it.
        let firefox = key("firefox-bin", "Firefox", "");
        play(&mut a, &engine, vec![stream(1, OUT, &firefox)]);
        assert_eq!(chosen(&a, &firefox, OUT), "Music");
        assert_eq!(a.app_rules().apps.len(), 1);
    }

    #[test]
    fn a_program_name_with_nothing_after_its_last_separator_names_no_application() {
        let (mut a, _engine, _dir) = with_apps();
        for app in ["/", "C:\\Games\\"] {
            let outcome = run_line(&mut a, &["--app-preset", &format!("{app}=Gaming")]);
            assert!(outcome.failed, "{app}");
            assert_eq!(outcome.stderr, "no application was named", "{app}");
        }
        assert!(a.app_rules().apps.is_empty());
    }

    #[test]
    fn following_the_lane_for_an_application_not_seen_adds_nothing() {
        let (mut a, _engine, _dir) = with_apps();
        let outcome = run_line(&mut a, &["--app-preset=ghost=default"]);
        assert!(!outcome.failed);
        assert!(
            outcome.stderr.contains("follows FxSound's preset already"),
            "{}",
            outcome.stderr
        );
        assert!(a.app_rules().apps.is_empty());
    }

    #[test]
    fn default_takes_an_applications_preset_away_and_its_route_with_it() {
        let (mut a, engine, _dir) = with_apps();
        play(&mut a, &engine, vec![stream(1, OUT, &battlefield())]);
        run_line(&mut a, &["--app-preset=bf6.exe=Gaming"]);
        assert!(routes_sent(&engine).is_some_and(|routes| routes.len() == 1));

        for follow in ["bf6.exe=default", "bf6.exe=follow", "bf6.exe="] {
            run_line(&mut a, &["--app-preset=bf6.exe=Gaming"]);
            let _ = engine.take_sent();
            let outcome = run_line(&mut a, &["--app-preset", follow]);
            assert!(
                !outcome.failed && outcome.stderr.is_empty(),
                "{follow}: {}",
                outcome.stderr
            );
            assert_eq!(chosen(&a, &battlefield(), OUT), "", "{follow}");
            assert_eq!(routes_sent(&engine), Some(Vec::new()), "{follow}");
        }
    }

    #[test]
    fn an_unknown_preset_is_refused_and_changes_nothing_not_even_a_new_rule() {
        let (mut a, engine, _dir) = with_apps();
        play(&mut a, &engine, vec![stream(1, OUT, &battlefield())]);
        let _ = engine.take_sent();

        let outcome = run_line(&mut a, &["--app-preset=ghost=Nope"]);
        assert!(outcome.failed);
        assert_eq!(outcome.stderr, "no output preset is called \"Nope\"");
        assert_eq!(a.app_rules().apps.len(), 1, "no rule for the ghost");

        // A preset only the other lane has: said where it is, as `--preset` says it.
        let outcome = run_line(&mut a, &["--app-preset=bf6.exe=Headset"]);
        assert!(outcome.failed);
        assert_eq!(
            outcome.stderr,
            "no output preset is called \"Headset\"; it is an input preset, which \
             --app-input-preset gives"
        );
        let outcome = run_line(&mut a, &["--app-input-preset=bf6.exe=Gaming"]);
        assert_eq!(
            outcome.stderr,
            "no input preset is called \"Gaming\"; it is an output preset, which --app-preset \
             gives"
        );
        // Preset names are exact, as `--preset` takes them.
        assert!(run_line(&mut a, &["--app-preset=bf6.exe=gaming"]).failed);
        assert_eq!(chosen(&a, &battlefield(), OUT), "");
        assert_eq!(routes_sent(&engine), None);

        // A refusal does not stop the rest of the line, and still fails it.
        let outcome = run_line(
            &mut a,
            &["--app-preset=bf6.exe=Nope", "--app-preset=bf6.exe=Gaming"],
        );
        assert!(outcome.failed);
        assert_eq!(chosen(&a, &battlefield(), OUT), "Gaming");
    }

    #[test]
    fn list_apps_puts_the_running_applications_first_with_what_they_run_through_now() {
        let (mut a, engine, _dir) = with_apps();
        // Brave played and quit; the game and the voice chat play now.
        play(&mut a, &engine, vec![stream(1, OUT, &brave())]);
        run_line(&mut a, &["--app-preset=brave=Volume Boost"]);
        play(
            &mut a,
            &engine,
            vec![
                on_route(2, OUT, &battlefield(), "Gaming"),
                stream(3, IN, &discord()),
            ],
        );
        run_line(&mut a, &["--app-preset=bf6.exe=Gaming"]);

        let outcome = run_line(&mut a, &["--list-apps", "--json"]);
        assert!(!outcome.failed && outcome.window.is_empty());
        let listing: Value = serde_json::from_str(&outcome.stdout).expect("one JSON object");
        assert_eq!(listing["schema"], STATUS_SCHEMA);
        assert_eq!(
            listing["apps"],
            serde_json::json!([
                {
                    "name": "Battlefield 6", "binary": "bf6.exe", "flatpak": "",
                    "running": true, "output_preset": "Gaming", "input_preset": null,
                    "routed": {"output": "Gaming", "input": null}
                },
                {
                    "name": "Discord", "binary": "Discord", "flatpak": "com.discordapp.Discord",
                    "running": true, "output_preset": null, "input_preset": null,
                    "routed": {"output": null, "input": null}
                },
                {
                    "name": "Brave", "binary": "brave", "flatpak": "",
                    "running": false, "output_preset": "Volume Boost", "input_preset": null,
                    "routed": {"output": null, "input": null}
                },
            ])
        );

        let plain = run_line(&mut a, &["--list-apps"]).stdout;
        assert_eq!(
            plain.lines().collect::<Vec<_>>(),
            [
                "name=\"Battlefield 6\" binary=bf6.exe flatpak=\"\" running=true \
                 output_preset=Gaming input_preset= routed.output=Gaming routed.input=",
                "name=Discord binary=Discord flatpak=com.discordapp.Discord running=true \
                 output_preset= input_preset= routed.output= routed.input=",
                "name=Brave binary=brave flatpak=\"\" running=false \
                 output_preset=\"Volume Boost\" input_preset= routed.output= routed.input=",
            ]
        );
    }

    #[test]
    fn list_apps_with_nothing_remembered_prints_nothing_or_an_empty_list() {
        let (mut a, _engine, _dir) = with_apps();
        assert_eq!(run_line(&mut a, &["--list-apps"]).stdout, "");
        assert_eq!(
            run_line(&mut a, &["--list-apps", "--json"]).stdout,
            format!(r#"{{"schema":{STATUS_SCHEMA},"apps":[]}}"#)
        );
    }

    #[test]
    fn the_status_document_carries_every_application_under_apps() {
        let (mut a, engine, _dir) = with_apps();
        play(
            &mut a,
            &engine,
            vec![on_route(1, IN, &discord(), "Headset")],
        );
        run_line(&mut a, &["--app-input-preset=discord=Headset"]);
        let document = status(&mut a);
        let apps = document["apps"].as_array().expect("an array");
        assert_eq!(apps.len(), 1);
        let keys: Vec<&str> = apps[0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = [
            "name",
            "binary",
            "flatpak",
            "running",
            "output_preset",
            "input_preset",
            "routed",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(apps[0]["input_preset"], "Headset");
        assert_eq!(apps[0]["routed"]["input"], "Headset");
        assert_eq!(
            document["apps"],
            serde_json::from_str::<Value>(&run_line(&mut a, &["--list-apps", "--json"]).stdout)
                .expect("JSON")["apps"],
            "the same list --list-apps prints"
        );
        // A headless instance remembers nothing, and says so with an empty list.
        assert_eq!(status(&mut app())["apps"], serde_json::json!([]));
    }

    #[test]
    fn list_apps_without_an_instance_reads_the_store_and_leaves_a_bad_one_where_it_is() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let path = dir.path().join("apps.toml");

        let missing = store_listing(&path, true);
        assert!(!missing.failed);
        assert_eq!(
            missing.stdout,
            format!(r#"{{"schema":{STATUS_SCHEMA},"apps":[]}}"#)
        );
        assert!(!path.exists(), "a question creates nothing");

        let mut rules = AppRules::default();
        rules.upsert(&battlefield(), OUT, "Gaming", 20);
        rules.seen(&brave(), 30);
        rules.save_to(&path).expect("write the store");
        let listed = store_listing(&path, false);
        assert!(!listed.failed, "{}", listed.stderr);
        assert_eq!(
            listed.stdout.lines().collect::<Vec<_>>(),
            [
                "name=Brave binary=brave flatpak=\"\" running=false output_preset= \
                 input_preset= routed.output= routed.input=",
                "name=\"Battlefield 6\" binary=bf6.exe flatpak=\"\" running=false \
                 output_preset=Gaming input_preset= routed.output= routed.input=",
            ],
            "nothing runs, so the most recently seen first"
        );

        std::fs::write(&path, "[[app]\nbinary = ").expect("write a broken store");
        let broken = store_listing(&path, true);
        assert!(broken.failed);
        assert!(broken.stdout.is_empty());
        assert!(broken.stderr.contains("does not load"), "{}", broken.stderr);
        assert!(path.exists(), "left where it is");
        assert!(!AppRules::bad_path(&path).exists(), "not moved aside");
    }

    /// What `main` does with `args` once it holds the lock and so knows no FxSound runs.
    fn without_an_instance(args: &[&str], store: &Path) -> Option<Outcome> {
        let cli =
            crate::cli::Cli::try_parse_from(std::iter::once("fxsound").chain(args.iter().copied()))
                .expect("parses");
        answer_without_an_instance(&cli.commands(), store)
    }

    #[test]
    fn a_line_with_watch_and_list_apps_is_the_store_listing_when_no_instance_runs() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let path = dir.path().join("apps.toml");
        let mut rules = AppRules::default();
        rules.upsert(&battlefield(), OUT, "Gaming", 20);
        rules.save_to(&path).expect("write the store");

        for args in [
            &["--watch", "--list-apps"][..],
            &["--list-apps", "--watch", "--meters"][..],
            &["--quit", "--list-apps"][..],
        ] {
            let answer = without_an_instance(args, &path).expect("answered, nothing started");
            assert_eq!(
                answer.stdout,
                store_listing(&path, false).stdout,
                "{args:?}"
            );
            assert!(answer.stdout.contains("output_preset=Gaming"), "{args:?}");
            assert!(answer.stderr.is_empty(), "{args:?}: {}", answer.stderr);
            assert!(!answer.failed, "{args:?}");
        }
        let json = without_an_instance(&["--watch", "--list-apps", "--json"], &path)
            .expect("answered, nothing started");
        assert_eq!(json.stdout, store_listing(&path, true).stdout);
    }

    #[test]
    fn a_question_only_a_running_instance_can_answer_fails_without_one() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let path = dir.path().join("apps.toml");
        for args in [
            &["--status"][..],
            &["--status", "--json"][..],
            &["--watch"][..],
            &["--watch", "--meters", "--json"][..],
            // `--status` wins over `--list-apps` for a running instance as well.
            &["--list-apps", "--status"][..],
            &["--quit", "--status"][..],
            &["--quit", "--watch"][..],
        ] {
            let answer = without_an_instance(args, &path).expect("answered, nothing started");
            assert_eq!(answer.stderr, NOT_RUNNING, "{args:?}");
            assert!(answer.stdout.is_empty(), "{args:?}");
            assert!(answer.failed, "{args:?}");
        }
        assert!(!path.exists(), "a question creates nothing");
    }

    #[test]
    fn quit_without_an_instance_says_so_and_is_not_a_failure() {
        let path = Path::new("/nonexistent/apps.toml");
        for args in [&["--quit"][..], &["--power=on", "--quit"][..]] {
            let answer = without_an_instance(args, path).expect("answered, nothing started");
            assert_eq!(answer.stderr, NOT_RUNNING, "{args:?}");
            assert!(answer.stdout.is_empty(), "{args:?}");
            assert!(!answer.failed, "{args:?}");
            assert!(answer.window.is_empty(), "{args:?}");
        }
    }

    #[test]
    fn a_line_that_does_something_starts_fxsound_when_none_runs() {
        let path = Path::new("/nonexistent/apps.toml");
        for args in [
            &[][..],
            &["--hide"][..],
            &["--power=on", "--preset=Gaming"][..],
            &["--app-preset=bf6.exe=Gaming"][..],
            &["--activated"][..],
        ] {
            assert!(without_an_instance(args, path).is_none(), "{args:?}");
        }
    }
}
