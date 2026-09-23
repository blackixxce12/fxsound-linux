//! The D-Bus service: `org.fxsound.FxSound` on the session bus (0.4.0 design §9).
//!
//! The public API for whatever would rather hold a connection than spawn a process per question —
//! `busctl`, Waybar and Noctalia modules, KWin and GNOME Shell scripts. The command line stays on
//! the control socket ([`crate::ipc`]): that works without a session bus, and it forwards raw
//! argv, so parsing and its error text have a single source.
//!
//! ```text
//! bus names  org.fxsound.FxSound (the API), com.fxsound.FxSound (the desktop-entry id)
//! object     /org/fxsound/FxSound
//! interface  org.fxsound.FxSound
//!
//! methods    TogglePower() → b, SetPower(b), NextPreset(), PrevPreset(), SetPreset(s),
//!            GetPreset() → s, SetOutput(s), SetInput(s), NextOutput(), NextInput(),
//!            SetNoiseSuppression(s), SetEditDirection(s), GetStatus() → s, Show(), Hide(),
//!            ToggleWindow(), Quit(), Apply(as argv) → (b ok, s stdout, s stderr),
//!            ListPresets() → s, ListDevices() → s,
//!            SetAppPreset(s app, s direction, s preset), ListApps() → s
//! properties Version (s, const), Power (b), Preset (s), Output (s), Input (s), Direction (s)
//! signals    PowerChanged(b), PresetChanged(s direction, s name),
//!            DeviceChanged(s direction, s node_name, s description), AudioStateChanged(s json),
//!            Notice(s message), AppRouted(s app, s direction, s preset)
//! ```
//!
//! # One way to carry out a command
//!
//! Every method builds the command list the matching option builds ([`Call::commands`]) and hands
//! it to the GUI thread through the control socket's own channel ([`Control::call`]). There the
//! `commands::run` that answers `fxsound --next-preset` answers it too, under the same four-second
//! cap. A refusal comes back as `org.fxsound.FxSound.Error.Refused` ([`REFUSED`]) carrying the
//! text `fxsound` would print on stderr — an unknown name, a factory preset asked to be
//! overwritten, the user-preset cap; an argument that cannot be parsed, as `InvalidArgs`; no
//! answer in time, as `Failed`. What a method does not copy is the original's habit of raising
//! the window after a command line: a bus call comes from a keybind or a status bar, and only
//! `Show` and `ToggleWindow` bring the window up.
//!
//! `Apply` is the whole command line in one call — what `fxsound ARGV` run beside this instance
//! prints and exits with, as `(ok, stdout, stderr)`: the argv is read by the parser `fxsound`
//! reads its own with, so an option it does not know is `ok = false` with the text `fxsound`
//! prints for it, not a bus error. `ListPresets` and `ListDevices` answer with the parts of the
//! `--status --json` document that list the presets and the devices (review items U14 and the
//! D-Bus additions, the prerequisites of an MCP server). `SetAppPreset` and `ListApps` are
//! `--app-preset` / `--app-input-preset` and `--list-apps --json` (per-application presets,
//! `docs/0.4.0-apps.md`), and `AppRouted` is the `app_routed` event. The bus starts FxSound for
//! a call when it is not running — any call, a property read included, unless the caller sets
//! `NO_AUTO_START` (`busctl --auto-start=no`), which the manual tells pollers to:
//! `org.fxsound.FxSound.service` in `/usr/share/dbus-1/services/` hands the start to
//! `fxsound.service`, whose `--activated` exits quietly when an instance still starting up turns
//! out to hold the lock already.
//!
//! At most [`ipc::MAX_CONNECTIONS`] calls are in flight at once, the control socket's own limit;
//! past it a call is refused with [`ipc::TOO_MANY_CALLERS`]. A call whose caller was answered
//! `Failed` for want of an answer in time is not carried out late ([`ipc::Forwarded::is_abandoned`]).
//!
//! `Preset`, `GetPreset` and `Direction` are about the edit direction, the lane the window shows,
//! as `--status`'s `preset:` is. `Output` and `Input` are the lanes' devices by description, as
//! `--status`'s `output:` and `input:` are; an empty string is a detached lane. `DeviceChanged`
//! carries the `node.name` as well, which is what `SetOutput` and the settings file take best.
//!
//! # Names
//!
//! `org.fxsound.FxSound` is the API. `com.fxsound.FxSound` is the desktop-entry id — the
//! `.desktop` file's name, the Wayland app id, the tray item's id — owned as well so that a shell
//! can associate the window with the service. The single-instance lock stays the authority on who
//! is primary, and only the primary starts this service. When the API name is owned already — by
//! a foreign process, or by a primary whose runtime directory differs from this one's on the same
//! bus — the instance logs that and carries on with the control socket alone, as it does when
//! there is no session bus.
//!
//! # Threads and runtimes
//!
//! The service has a thread of its own, `fxsound-dbus`, with a current-thread tokio runtime on it:
//! the connection's socket, its object server and every method handler run there, so a slow bus
//! never holds up the window. Handlers are `async` and await the GUI thread's answer; they never
//! block, and never call `zbus::block_on`, which would start a runtime inside this one. This
//! runtime has nothing to do with the one ksni's tray runs on (`crate::tray`), nor with the one
//! zbus keeps for notify-rust's toasts.
//!
//! The GUI thread holds a [`DbusHandle`]. [`DbusHandle::publish`] folds each [`AppEvent`] of the
//! `--watch` stream into the [`Properties`] the getters read, and hands the matching [`Signal`] and
//! the properties it moved to the service thread, which emits them. The stream and the signals
//! come from one source, and cannot disagree. Dropping the handle releases the names, lets a call
//! that is being answered — `Quit`'s own — get its reply out, and closes the connection.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fxsound_core::DeviceDirection;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use zbus::fdo;
use zbus::object_server::{InterfaceRef, SignalEmitter};

use crate::App;
use crate::cli::{
    self, AppPresetChoice, Cli, Command, DeviceCommand, PowerCommand, PresetCommand, WindowCommand,
};
use crate::events::{AppEvent, EventSink};
use crate::ipc::{self, Control, Response};

/// The API's well-known name.
pub const BUS_NAME: &str = "org.fxsound.FxSound";
/// The desktop-entry id, owned too so that a shell can tie the window to the service.
pub const DESKTOP_BUS_NAME: &str = "com.fxsound.FxSound";
/// Where the interface is served.
pub const OBJECT_PATH: &str = "/org/fxsound/FxSound";
/// The interface.
pub const INTERFACE: &str = "org.fxsound.FxSound";

/// The `Version` property: the same version `--status` reports.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the way out waits for calls in flight to be answered and for the names to be given
/// back before the connection is closed regardless. The GUI thread waits this long at most.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// How long connecting, the handshake and asking for the names may take. zbus has no timeout of
/// its own for any of them, and a bus that accepts and then says nothing would otherwise leave
/// the service starting for ever, with nothing in the log to say why.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// =============================================================================================
// What a method call does
// =============================================================================================

/// One method call, with its arguments as they came off the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    TogglePower,
    SetPower(bool),
    NextPreset,
    PrevPreset,
    SetPreset(String),
    GetPreset,
    SetOutput(String),
    SetInput(String),
    NextOutput,
    NextInput,
    SetNoiseSuppression(String),
    SetEditDirection(String),
    GetStatus,
    Show,
    Hide,
    ToggleWindow,
    Quit,
    Apply(Vec<String>),
    ListPresets,
    ListDevices,
    /// The application, the direction and the preset, as they came.
    SetAppPreset {
        app: String,
        direction: String,
        preset: String,
    },
    ListApps,
}

impl Call {
    /// The member the call arrives as.
    #[must_use]
    pub const fn member(&self) -> &'static str {
        match self {
            Self::TogglePower => "TogglePower",
            Self::SetPower(_) => "SetPower",
            Self::NextPreset => "NextPreset",
            Self::PrevPreset => "PrevPreset",
            Self::SetPreset(_) => "SetPreset",
            Self::GetPreset => "GetPreset",
            Self::SetOutput(_) => "SetOutput",
            Self::SetInput(_) => "SetInput",
            Self::NextOutput => "NextOutput",
            Self::NextInput => "NextInput",
            Self::SetNoiseSuppression(_) => "SetNoiseSuppression",
            Self::SetEditDirection(_) => "SetEditDirection",
            Self::GetStatus => "GetStatus",
            Self::Show => "Show",
            Self::Hide => "Hide",
            Self::ToggleWindow => "ToggleWindow",
            Self::Quit => "Quit",
            Self::Apply(_) => "Apply",
            Self::ListPresets => "ListPresets",
            Self::ListDevices => "ListDevices",
            Self::SetAppPreset { .. } => "SetAppPreset",
            Self::ListApps => "ListApps",
        }
    }

    /// The command list the call hands to the GUI thread: the one the matching option builds
    /// (`--toggle-power`, `--preset NAME`, `--output DEVICE|off`, `--edit`, `--quit`, …), without
    /// the window raise a typed command line adds. The two questions ask for the `--status --json`
    /// document and read their answer out of it; `TogglePower` asks for it after the toggle, to
    /// answer with the state the toggle left.
    ///
    /// # Errors
    ///
    /// An argument the option would not take either, with the words the option uses — and an
    /// empty name, which on the command line is no command at all and on the bus would be a call
    /// that silently does nothing.
    pub fn commands(&self) -> Result<Vec<Command>, String> {
        let status = || Command::Status { json: true };
        Ok(match self {
            Self::TogglePower => vec![Command::Power(PowerCommand::Toggle), status()],
            Self::SetPower(on) => vec![Command::Power(if *on {
                PowerCommand::On
            } else {
                PowerCommand::Off
            })],
            Self::NextPreset => vec![Command::Preset(PresetCommand::Next)],
            Self::PrevPreset => vec![Command::Preset(PresetCommand::Previous)],
            Self::SetPreset(name) => {
                if name.is_empty() {
                    return Err("a preset name cannot be empty".to_owned());
                }
                vec![Command::Preset(PresetCommand::Select(name.clone()))]
            }
            Self::GetPreset | Self::GetStatus | Self::ListPresets | Self::ListDevices => {
                vec![status()]
            }
            Self::SetOutput(device) => vec![Command::Output(device_command(device)?)],
            Self::SetInput(device) => vec![Command::Input(device_command(device)?)],
            Self::NextOutput => vec![Command::Output(DeviceCommand::Next)],
            Self::NextInput => vec![Command::Input(DeviceCommand::Next)],
            Self::SetNoiseSuppression(level) => vec![Command::NoiseSuppression(
                cli::parse_noise_suppression(level)?,
            )],
            Self::SetEditDirection(direction) => {
                vec![Command::EditDirection(cli::parse_direction(direction)?)]
            }
            Self::Show => vec![Command::Window(WindowCommand::Show)],
            Self::Hide => vec![Command::Window(WindowCommand::Hide)],
            Self::ToggleWindow => vec![Command::Window(WindowCommand::Toggle)],
            Self::Quit => vec![Command::Quit],
            Self::Apply(argv) => apply_commands(argv).map_err(|response| {
                if response.ok {
                    response.stdout
                } else {
                    response.stderr
                }
            })?,
            Self::SetAppPreset {
                app,
                direction,
                preset,
            } => {
                if app.trim().is_empty() {
                    return Err("an application name cannot be empty".to_owned());
                }
                vec![Command::AppPreset {
                    direction: cli::parse_direction(direction)?,
                    app: app.trim().to_owned(),
                    preset: AppPresetChoice::parse(preset),
                }]
            }
            Self::ListApps => vec![Command::ListApps { json: true }],
        })
    }
}

/// `Apply`'s command list: `argv` read by the parser `fxsound` reads its own command line with,
/// the program name optional, without the window raise a typed line gets for nothing
/// ([`Cli::commands_without_implicit_raise`]).
///
/// # Errors
///
/// What `fxsound ARGV` would answer without asking the running instance anything: clap's text
/// for a line it cannot parse, failed as `fxsound` exits 2 for it; `--help` and `--version`,
/// which succeed with the text on stdout.
pub fn apply_commands(argv: &[String]) -> Result<Vec<Command>, Response> {
    let options = match argv.split_first() {
        Some((first, rest)) if is_program_name(first) => rest,
        _ => argv,
    };
    let args = std::iter::once("fxsound").chain(options.iter().map(String::as_str));
    match <Cli as clap::Parser>::try_parse_from(args) {
        Ok(cli) => Ok(cli.commands_without_implicit_raise()),
        Err(err) if err.use_stderr() => Err(Response::failed(err.render().to_string())),
        Err(err) => Err(Response::output(err.render().to_string())),
    }
}

/// Whether `arg` is a program name rather than an option: `fxsound`, or a path to it. The line
/// takes no positional argument, so nothing else is lost by reading it as one.
fn is_program_name(arg: &str) -> bool {
    !arg.starts_with('-')
        && std::path::Path::new(arg)
            .file_name()
            .is_some_and(|name| name == "fxsound")
}

/// `Apply`'s answer: the command line's, whatever it was — only no answer in time is a bus
/// error, `Failed`, as for every other method: the command may still be running.
fn applied(response: Response) -> Result<(bool, String, String), MethodError> {
    if response.is_unanswered() {
        return Err(fdo::Error::Failed(response.stderr).into());
    }
    Ok((response.ok, response.stdout, response.stderr))
}

/// `ListPresets`'s answer: of the status document, the `schema` it is read by, the edit
/// direction, its preset and its list, and each lane's list — under the document's own keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetListing {
    pub schema: Value,
    pub edit_direction: Value,
    pub selected_preset: Value,
    pub presets: Value,
    pub output_presets: Value,
    pub input_presets: Value,
}

/// `ListDevices`'s answer: of the status document, the `schema` it is read by, each lane's
/// device and each lane's list of devices with their node names — under the document's own keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceListing {
    pub schema: Value,
    pub selected_output: Value,
    pub selected_input: Value,
    pub output_device_list: Value,
    pub input_device_list: Value,
}

/// A listing out of the status document, as one JSON object: `PresetListing` or
/// `DeviceListing`.
///
/// # Errors
///
/// A document that is not the status document, or one without one of the keys.
pub fn listing_of_status<T: Serialize + DeserializeOwned>(
    document: &str,
) -> Result<String, String> {
    let listing: T = serde_json::from_str(document)
        .map_err(|err| format!("the status document does not have the listing: {err}"))?;
    serde_json::to_string(&listing).map_err(|err| format!("the listing: {err}"))
}

/// `--output`'s and `--input`'s reading of a device argument: `off` detaches, anything else is a
/// name.
fn device_command(device: &str) -> Result<DeviceCommand, String> {
    cli::device_command(Some(device), false)
        .ok_or_else(|| "a device name cannot be empty; `off` detaches the lane".to_owned())
}

/// The error name of FxSound refusing a command: what `fxsound` exits 1 for, with its stderr as
/// the message (upstream review item U15).
pub const REFUSED: &str = "org.fxsound.FxSound.Error.Refused";

/// What a method answers with when it does not do what it was asked.
#[derive(Debug)]
pub enum MethodError {
    /// [`REFUSED`]: FxSound heard the command and says no — the answer `fxsound` prints on stderr
    /// and exits 1 for, from the same `commands::run`.
    Refused(String),
    /// A standard error: `InvalidArgs` for an argument the option would not take either, `Failed`
    /// when the answer could not be had or read.
    Standard(fdo::Error),
}

impl From<fdo::Error> for MethodError {
    fn from(err: fdo::Error) -> Self {
        Self::Standard(err)
    }
}

impl zbus::DBusError for MethodError {
    fn create_reply(
        &self,
        call: &zbus::message::Header<'_>,
    ) -> zbus::Result<zbus::message::Message> {
        match self {
            Self::Refused(text) => zbus::message::Message::error(call, self.name())?.build(text),
            Self::Standard(err) => err.create_reply(call),
        }
    }

    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            Self::Refused(_) => zbus::names::ErrorName::from_static_str_unchecked(REFUSED),
            Self::Standard(err) => err.name(),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::Refused(text) => Some(text),
            Self::Standard(err) => err.description(),
        }
    }
}

/// A method's result, out of the GUI thread's answer: its stdout, or [`REFUSED`] with its
/// stderr — `Failed` only when there was no answer in time, which refuses nothing.
fn answer(response: Response) -> Result<String, MethodError> {
    if response.ok {
        Ok(response.stdout)
    } else if response.is_unanswered() {
        Err(fdo::Error::Failed(response.stderr).into())
    } else if response.stderr.is_empty() {
        Err(MethodError::Refused(
            "FxSound refused the command".to_owned(),
        ))
    } else {
        Err(MethodError::Refused(response.stderr))
    }
}

/// `TogglePower`'s answer: `power` in the status document the call asked for after the toggle.
///
/// # Errors
///
/// A document that is not the status document.
pub fn power_of_status(document: &str) -> Result<bool, String> {
    let document: Value = serde_json::from_str(document)
        .map_err(|err| format!("the status document is not JSON: {err}"))?;
    document["power"]
        .as_bool()
        .ok_or_else(|| "the status document has no `power`".to_owned())
}

/// `GetPreset`'s answer: the edit direction's preset, by the name `SetPreset` takes (no `*` for
/// unsaved changes), or the empty string while that lane has none.
///
/// # Errors
///
/// A document that is not the status document.
pub fn preset_of_status(document: &str) -> Result<String, String> {
    let document: Value = serde_json::from_str(document)
        .map_err(|err| format!("the status document is not JSON: {err}"))?;
    let lane = document["edit_direction"]
        .as_str()
        .and_then(DeviceDirection::from_key)
        .ok_or_else(|| "the status document has no `edit_direction`".to_owned())?;
    Ok(document[lane.key()]["preset"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

// =============================================================================================
// What the bus sees of the state: properties and signals
// =============================================================================================

/// What the properties read: the controller as the last published event left it.
///
/// Kept by the GUI thread ([`DbusHandle::publish`]) and read by the getters on the service
/// thread, so a `Get` never waits for a tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Properties {
    pub power: bool,
    /// The edit direction.
    pub direction: DeviceDirection,
    /// Per lane, output first: the preset's name, `None` while the lane has none.
    pub presets: [Option<String>; 2],
    /// Per lane: the device's `node.name` and description, `None` while the lane is detached.
    pub devices: [Option<(String, String)>; 2],
}

/// One of the properties that can change. `Version` cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Property {
    Power,
    Preset,
    Output,
    Input,
    Direction,
}

impl Property {
    /// The property's name on the bus.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Power => "Power",
            Self::Preset => "Preset",
            Self::Output => "Output",
            Self::Input => "Input",
            Self::Direction => "Direction",
        }
    }

    /// The property that holds `direction`'s device.
    const fn device(direction: DeviceDirection) -> Self {
        match direction {
            DeviceDirection::Output => Self::Output,
            DeviceDirection::Input => Self::Input,
        }
    }
}

/// A signal of the interface, with its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    PowerChanged(bool),
    /// A lane's preset, by name; empty while the lane has none.
    PresetChanged {
        direction: DeviceDirection,
        name: String,
    },
    /// A lane's device; both empty when the lane was detached.
    DeviceChanged {
        direction: DeviceDirection,
        node_name: String,
        description: String,
    },
    /// The `audio_state` line `fxsound --watch --json` prints for the same change.
    AudioStateChanged(String),
    Notice(String),
    /// An application's streams of one lane moved onto the route of a preset of its own, or back
    /// onto the lane (`preset` empty): the `app_routed` event. `app` is the name the window shows.
    AppRouted {
        app: String,
        direction: DeviceDirection,
        preset: String,
    },
}

impl Signal {
    /// The member the signal is emitted as.
    #[must_use]
    pub const fn member(&self) -> &'static str {
        match self {
            Self::PowerChanged(_) => "PowerChanged",
            Self::PresetChanged { .. } => "PresetChanged",
            Self::DeviceChanged { .. } => "DeviceChanged",
            Self::AudioStateChanged(_) => "AudioStateChanged",
            Self::Notice(_) => "Notice",
            Self::AppRouted { .. } => "AppRouted",
        }
    }
}

/// What one event changes on the bus: the signal that announces it, and the properties whose
/// values it moved (announced by `PropertiesChanged`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Update {
    pub signal: Option<Signal>,
    pub changed: Vec<Property>,
}

impl Update {
    /// Nothing to emit.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.signal.is_none() && self.changed.is_empty()
    }
}

/// A lane's slot in the per-lane arrays.
const fn lane(direction: DeviceDirection) -> usize {
    match direction {
        DeviceDirection::Output => 0,
        DeviceDirection::Input => 1,
    }
}

impl Properties {
    /// What the controller looks like now — what the properties say before the first event.
    #[must_use]
    pub fn of(app: &App) -> Self {
        let state = &app.state;
        let mut properties = Self {
            power: state.power,
            direction: state.direction,
            ..Self::default()
        };
        for direction in DeviceDirection::ALL {
            properties.presets[lane(direction)] = app
                .lane_preset(direction)
                .map(|(name, _modified)| name.to_owned());
            properties.devices[lane(direction)] = state
                .device_for(direction)
                .map(|device| (device.name.clone(), device.description.clone()));
        }
        properties
    }

    /// `Preset`: the edit direction's preset, or the empty string.
    #[must_use]
    pub fn preset(&self) -> &str {
        self.presets[lane(self.direction)]
            .as_deref()
            .unwrap_or_default()
    }

    /// `Output` and `Input`: the lane's device by description, or the empty string while it is
    /// detached.
    #[must_use]
    pub fn device(&self, direction: DeviceDirection) -> &str {
        self.devices[lane(direction)]
            .as_ref()
            .map_or("", |(_node_name, description)| description.as_str())
    }

    /// Take `event` in, and say what it changes on the bus. `ts_ms` is the Unix time in
    /// milliseconds, for the `AudioStateChanged` line.
    ///
    /// A signal goes out only for a real change: a preset that gains unsaved changes keeps its
    /// name, so it is not a new preset on the bus, and an event that repeats what the properties
    /// already say is nothing.
    pub fn fold(&mut self, event: &AppEvent, ts_ms: u64) -> Update {
        match event {
            AppEvent::Power { on } => {
                if self.power == *on {
                    return Update::default();
                }
                self.power = *on;
                Update {
                    signal: Some(Signal::PowerChanged(*on)),
                    changed: vec![Property::Power],
                }
            }
            AppEvent::PresetChanged {
                direction, name, ..
            } => {
                let slot = &mut self.presets[lane(*direction)];
                if slot == name {
                    return Update::default();
                }
                slot.clone_from(name);
                Update {
                    signal: Some(Signal::PresetChanged {
                        direction: *direction,
                        name: name.clone().unwrap_or_default(),
                    }),
                    changed: if *direction == self.direction {
                        vec![Property::Preset]
                    } else {
                        Vec::new()
                    },
                }
            }
            AppEvent::DeviceChanged {
                direction,
                node_name,
                description,
            } => {
                let device = node_name
                    .clone()
                    .map(|node_name| (node_name, description.clone().unwrap_or_default()));
                let slot = &mut self.devices[lane(*direction)];
                if *slot == device {
                    return Update::default();
                }
                *slot = device;
                Update {
                    signal: Some(Signal::DeviceChanged {
                        direction: *direction,
                        node_name: node_name.clone().unwrap_or_default(),
                        description: description.clone().unwrap_or_default(),
                    }),
                    changed: vec![Property::device(*direction)],
                }
            }
            AppEvent::Direction { direction } => {
                if self.direction == *direction {
                    return Update::default();
                }
                let shown = self.preset().to_owned();
                self.direction = *direction;
                let mut changed = vec![Property::Direction];
                if self.preset() != shown {
                    changed.push(Property::Preset);
                }
                Update {
                    signal: None,
                    changed,
                }
            }
            AppEvent::AudioState { .. } => Update {
                signal: Some(Signal::AudioStateChanged(event.to_json(ts_ms))),
                changed: Vec::new(),
            },
            AppEvent::Notice { message } => Update {
                signal: Some(Signal::Notice(message.clone())),
                changed: Vec::new(),
            },
            // The controller says it once per move, so every one is news.
            AppEvent::AppRouted {
                app,
                direction,
                preset,
            } => Update {
                signal: Some(Signal::AppRouted {
                    app: app.display().to_owned(),
                    direction: *direction,
                    preset: preset.clone().unwrap_or_default(),
                }),
                changed: Vec::new(),
            },
            // The stream's own business: its first document, the device list, the meters, the
            // window and the stream's end have no signal of their own on the bus.
            AppEvent::Status(_)
            | AppEvent::DevicesChanged { .. }
            | AppEvent::InputMeters(_)
            | AppEvent::EchoCancel { .. }
            | AppEvent::Calibrated(_)
            | AppEvent::Window { .. }
            | AppEvent::Quit => Update::default(),
        }
    }
}

// =============================================================================================
// The interface
// =============================================================================================

/// The object at [`OBJECT_PATH`].
struct Service {
    control: Control,
    properties: Arc<Mutex<Properties>>,
    in_flight: Arc<InFlight>,
}

impl Service {
    /// Carry `call` out on the GUI thread and return what it printed.
    async fn run(&self, call: Call) -> Result<String, MethodError> {
        let commands = call.commands().map_err(fdo::Error::InvalidArgs)?;
        answer(self.call(commands).await)
    }

    /// Hand `commands` to the GUI thread and wait for the answer, as one of at most
    /// [`ipc::MAX_CONNECTIONS`] calls in flight — the socket's own limit. zbus answers every call
    /// on a task of its own, so without it a caller that does not wait for its replies, or a GUI
    /// thread that stalls for a few seconds, piles up tasks and queued commands without end; past
    /// it a call is refused with [`ipc::TOO_MANY_CALLERS`], as a keybind past the socket's limit is.
    async fn call(&self, commands: Vec<Command>) -> Response {
        let Some(_busy) = self.in_flight.try_enter() else {
            return Response::failed(ipc::TOO_MANY_CALLERS);
        };
        self.control.call(commands).await
    }

    /// [`Service::run`], for a method that returns nothing.
    async fn run_quietly(&self, call: Call) -> Result<(), MethodError> {
        self.run(call).await.map(drop)
    }

    fn read<T>(&self, read: impl FnOnce(&Properties) -> T) -> T {
        read(&lock(&self.properties))
    }
}

#[zbus::interface(name = "org.fxsound.FxSound")]
impl Service {
    /// Turn processing off if it is on and on if it is off, as `--toggle-power` does; returns
    /// whether it is on now.
    async fn toggle_power(&self) -> Result<bool, MethodError> {
        let status = self.run(Call::TogglePower).await?;
        Ok(power_of_status(&status).map_err(fdo::Error::Failed)?)
    }

    /// Turn processing on or off, as `--power` does.
    async fn set_power(&self, on: bool) -> Result<(), MethodError> {
        self.run_quietly(Call::SetPower(on)).await
    }

    /// Select the edit direction's next preset, wrapping, as `--next-preset` does.
    async fn next_preset(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::NextPreset).await
    }

    /// Select the edit direction's previous preset, wrapping, as `--prev-preset` does.
    async fn prev_preset(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::PrevPreset).await
    }

    /// Select a preset by its exact name, as `--preset` does. Fails for a name that is not one.
    async fn set_preset(&self, name: &str) -> Result<(), MethodError> {
        self.run_quietly(Call::SetPreset(name.to_owned())).await
    }

    /// The edit direction's preset, without the marker for unsaved changes; empty for none.
    async fn get_preset(&self) -> Result<String, MethodError> {
        let status = self.run(Call::GetPreset).await?;
        Ok(preset_of_status(&status).map_err(fdo::Error::Failed)?)
    }

    /// Attach the output lane to a playback device by `node.name` or description, or detach it
    /// with `off`, as `--output` does.
    async fn set_output(&self, device: &str) -> Result<(), MethodError> {
        self.run_quietly(Call::SetOutput(device.to_owned())).await
    }

    /// Attach the input lane to a microphone by `node.name` or description, or detach it with
    /// `off`, as `--input` does.
    async fn set_input(&self, device: &str) -> Result<(), MethodError> {
        self.run_quietly(Call::SetInput(device.to_owned())).await
    }

    /// Move the output lane to the next playback device, wrapping, as `--next-output` does.
    async fn next_output(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::NextOutput).await
    }

    /// Move the input lane to the next microphone, wrapping, as `--next-input` does.
    async fn next_input(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::NextInput).await
    }

    /// The microphone's noise suppression: `off`, `light`, `medium`, `strong`, or `preset` to
    /// follow the voice preset, as `--noise-suppression` takes it.
    async fn set_noise_suppression(&self, level: &str) -> Result<(), MethodError> {
        self.run_quietly(Call::SetNoiseSuppression(level.to_owned()))
            .await
    }

    /// The lane the window edits, `output` or `input`, as `--edit` takes it.
    async fn set_edit_direction(&self, direction: &str) -> Result<(), MethodError> {
        self.run_quietly(Call::SetEditDirection(direction.to_owned()))
            .await
    }

    /// The document `fxsound --status --json` prints.
    async fn get_status(&self) -> Result<String, MethodError> {
        self.run(Call::GetStatus).await
    }

    /// Show and raise the window, as `--show` does.
    async fn show(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::Show).await
    }

    /// Hide the window to the tray, as `--hide` does.
    async fn hide(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::Hide).await
    }

    /// Show the window if it is hidden and hide it if it is showing, as `--toggle-window` does.
    async fn toggle_window(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::ToggleWindow).await
    }

    /// Quit, as `--quit` does. The reply comes before the connection closes.
    async fn quit(&self) -> Result<(), MethodError> {
        self.run_quietly(Call::Quit).await
    }

    /// Run a command line — `["--preset", "Rock", "--set_effect=bass:7.5"]`, the program name
    /// optional — and answer with what `fxsound` given it would: whether it succeeded, and what it
    /// printed on stdout and on stderr. A line it cannot parse or FxSound refuses is `ok = false`, not a
    /// bus error. The window is left alone unless the line says `--show`, `--toggle-window` or
    /// `--hide`.
    #[zbus(out_args("ok", "stdout", "stderr"))]
    async fn apply(&self, argv: Vec<String>) -> Result<(bool, String, String), MethodError> {
        let response = match apply_commands(&argv) {
            Ok(commands) => self.call(commands).await,
            Err(response) => response,
        };
        applied(response)
    }

    /// The presets of both lanes, as JSON: `schema`, `edit_direction`, `selected_preset` and the
    /// `presets`, `output_presets` and `input_presets` lists of `GetStatus`.
    async fn list_presets(&self) -> Result<String, MethodError> {
        let status = self.run(Call::ListPresets).await?;
        Ok(listing_of_status::<PresetListing>(&status).map_err(fdo::Error::Failed)?)
    }

    /// The devices of both lanes, as JSON: `schema`, `selected_output`, `selected_input` and the
    /// `output_device_list` and `input_device_list` of `GetStatus` — each device's `node_name`,
    /// `description` and `present`.
    async fn list_devices(&self) -> Result<String, MethodError> {
        let status = self.run(Call::ListDevices).await?;
        Ok(listing_of_status::<DeviceListing>(&status).map_err(fdo::Error::Failed)?)
    }

    /// Give an application a preset of its own for `direction` (`output` or `input`), or have it
    /// follow the lane's again with `default`, `follow` or an empty `preset` — as
    /// `--app-preset` and `--app-input-preset` do: the application by its Flatpak id, program or
    /// name, in any case, and one FxSound has not seen kept as the program of that name.
    async fn set_app_preset(
        &self,
        app: &str,
        direction: &str,
        preset: &str,
    ) -> Result<(), MethodError> {
        self.run_quietly(Call::SetAppPreset {
            app: app.to_owned(),
            direction: direction.to_owned(),
            preset: preset.to_owned(),
        })
        .await
    }

    /// The applications FxSound remembers, as JSON: `schema` and `apps`, the list `GetStatus`
    /// carries and `fxsound --list-apps --json` prints.
    async fn list_apps(&self) -> Result<String, MethodError> {
        self.run(Call::ListApps).await
    }

    /// The version `--status` reports.
    #[zbus(property(emits_changed_signal = "const"))]
    fn version(&self) -> String {
        VERSION.to_owned()
    }

    /// Whether processing is on.
    #[zbus(property)]
    fn power(&self) -> bool {
        self.read(|properties| properties.power)
    }

    /// The edit direction's preset; empty for none.
    #[zbus(property)]
    fn preset(&self) -> String {
        self.read(|properties| properties.preset().to_owned())
    }

    /// The output lane's device, by description; empty while the lane is detached.
    #[zbus(property)]
    fn output(&self) -> String {
        self.read(|properties| properties.device(DeviceDirection::Output).to_owned())
    }

    /// The input lane's microphone, by description; empty while the lane is detached.
    #[zbus(property)]
    fn input(&self) -> String {
        self.read(|properties| properties.device(DeviceDirection::Input).to_owned())
    }

    /// The edit direction: `output` or `input`.
    #[zbus(property)]
    fn direction(&self) -> String {
        self.read(|properties| properties.direction.key().to_owned())
    }

    /// Processing was turned on or off.
    #[zbus(signal, name = "PowerChanged")]
    async fn power_signal(emitter: &SignalEmitter<'_>, on: bool) -> zbus::Result<()>;

    /// A lane's preset changed: its direction and the preset's name, empty for none.
    #[zbus(signal, name = "PresetChanged")]
    async fn preset_signal(
        emitter: &SignalEmitter<'_>,
        direction: &str,
        name: &str,
    ) -> zbus::Result<()>;

    /// A lane's device changed: its direction, `node.name` and description, both empty when the
    /// lane was detached.
    #[zbus(signal, name = "DeviceChanged")]
    async fn device_signal(
        emitter: &SignalEmitter<'_>,
        direction: &str,
        node_name: &str,
        description: &str,
    ) -> zbus::Result<()>;

    /// A lane started or stopped processing: the `audio_state` event as `--watch --json`
    /// prints it.
    #[zbus(signal, name = "AudioStateChanged")]
    async fn audio_state_signal(emitter: &SignalEmitter<'_>, json: &str) -> zbus::Result<()>;

    /// The notice the window shows in its bubble.
    #[zbus(signal, name = "Notice")]
    async fn notice_signal(emitter: &SignalEmitter<'_>, message: &str) -> zbus::Result<()>;

    /// An application's streams moved onto the route of a preset of its own, or back onto the
    /// lane: the name the window shows, the lane, and the preset, empty once they are back —
    /// the `app_routed` event.
    #[zbus(signal, name = "AppRouted")]
    async fn app_routed_signal(
        emitter: &SignalEmitter<'_>,
        app: &str,
        direction: &str,
        preset: &str,
    ) -> zbus::Result<()>;
}

/// Calls being answered, so the way out can let them finish.
#[derive(Default)]
struct InFlight {
    count: AtomicUsize,
    idle: Notify,
}

/// One call in flight; counted until dropped.
struct Busy(Arc<InFlight>);

impl InFlight {
    /// One more call in flight, or `None` when [`ipc::MAX_CONNECTIONS`] are already.
    fn try_enter(self: &Arc<Self>) -> Option<Busy> {
        self.count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < ipc::MAX_CONNECTIONS).then_some(count + 1)
            })
            .ok()?;
        Some(Busy(Arc::clone(self)))
    }

    /// Until no call is in flight.
    async fn idle(&self) {
        loop {
            // Registered before the check, so a call finishing in between still wakes it.
            let finished = self.idle.notified();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            finished.await;
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Emit what `update` says on the service thread.
async fn emit(iface: &InterfaceRef<Service>, update: Update) -> zbus::Result<()> {
    let emitter = iface.signal_emitter();
    if let Some(signal) = update.signal {
        match signal {
            Signal::PowerChanged(on) => Service::power_signal(emitter, on).await?,
            Signal::PresetChanged { direction, name } => {
                Service::preset_signal(emitter, direction.key(), &name).await?;
            }
            Signal::DeviceChanged {
                direction,
                node_name,
                description,
            } => {
                Service::device_signal(emitter, direction.key(), &node_name, &description).await?;
            }
            Signal::AudioStateChanged(json) => {
                Service::audio_state_signal(emitter, &json).await?;
            }
            Signal::Notice(message) => Service::notice_signal(emitter, &message).await?,
            Signal::AppRouted {
                app,
                direction,
                preset,
            } => {
                Service::app_routed_signal(emitter, &app, direction.key(), &preset).await?;
            }
        }
    }
    if !update.changed.is_empty() {
        let service = iface.get().await;
        for property in update.changed {
            match property {
                Property::Power => service.power_changed(emitter).await?,
                Property::Preset => service.preset_changed(emitter).await?,
                Property::Output => service.output_changed(emitter).await?,
                Property::Input => service.input_changed(emitter).await?,
                Property::Direction => service.direction_changed(emitter).await?,
            }
        }
    }
    Ok(())
}

// =============================================================================================
// Hosting
// =============================================================================================

/// Which bus to serve on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bus {
    /// The session bus: `DBUS_SESSION_BUS_ADDRESS`, or `$XDG_RUNTIME_DIR/bus`.
    Session,
    /// A bus at this address — a private `dbus-daemon`, for the tests.
    Address(String),
}

/// Where the service is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Connecting and asking for the names.
    Starting,
    /// The API name is ours and the interface is served.
    Serving,
    /// Not served: no bus, the name taken, or the service stopped. The control socket carries on.
    Unavailable,
}

/// [`ServiceState`], shared between the service thread and the handle.
struct StateCell {
    state: Mutex<ServiceState>,
    settled: Condvar,
}

impl StateCell {
    fn get(&self) -> ServiceState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, state: ServiceState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
        self.settled.notify_all();
    }
}

/// The GUI thread's end of the service. Dropping it stops the service.
pub struct DbusHandle {
    /// To the service thread; dropped to stop it.
    updates: Option<mpsc::UnboundedSender<Update>>,
    properties: Arc<Mutex<Properties>>,
    state: Arc<StateCell>,
    thread: Option<JoinHandle<()>>,
}

impl DbusHandle {
    /// Serve the interface on the session bus, from a thread of its own, with `properties` as
    /// what the controller looks like now. Never fails: without a session bus, or with the name
    /// taken, the service logs why and [`DbusHandle::state`] says
    /// [`ServiceState::Unavailable`].
    #[must_use]
    pub fn start(control: Control, properties: Properties) -> Self {
        Self::start_on(Bus::Session, control, properties)
    }

    /// [`DbusHandle::start`] on `bus`.
    #[must_use]
    pub fn start_on(bus: Bus, control: Control, properties: Properties) -> Self {
        let (updates, receiver) = mpsc::unbounded_channel();
        let properties = Arc::new(Mutex::new(properties));
        let state = Arc::new(StateCell {
            state: Mutex::new(ServiceState::Starting),
            settled: Condvar::new(),
        });
        let service = Service {
            control,
            properties: Arc::clone(&properties),
            in_flight: Arc::default(),
        };
        let thread = {
            let state = Arc::clone(&state);
            thread::Builder::new()
                .name("fxsound-dbus".to_owned())
                .spawn(move || serve(&bus, service, receiver, &state))
        };
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(err) => {
                log::warn!("no D-Bus service: its thread did not start ({err})");
                state.set(ServiceState::Unavailable);
                None
            }
        };
        Self {
            updates: Some(updates),
            properties,
            state,
            thread,
        }
    }

    /// Take `event` into the properties, and emit the signal and the property changes it makes.
    ///
    /// Cheap: a lock, a comparison, and a message to the service thread when something changed
    /// while the service is up. It never waits on the bus.
    pub fn publish(&self, event: &AppEvent) {
        let update = lock(&self.properties).fold(event, ipc::unix_millis());
        if update.is_empty() || self.state.get() != ServiceState::Serving {
            return;
        }
        if let Some(updates) = &self.updates {
            // Gone only once the service has stopped, and then there is nobody to tell.
            let _ = updates.send(update);
        }
    }

    /// Where the service is.
    #[must_use]
    pub fn state(&self) -> ServiceState {
        self.state.get()
    }

    /// Wait until the service is serving or has given up, for at most `timeout`; returns where
    /// it is then.
    #[must_use]
    pub fn wait_until_settled(&self, timeout: Duration) -> ServiceState {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *state == ServiceState::Starting {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .state
                .settled
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *state
    }

    /// What the properties say now.
    #[must_use]
    pub fn properties(&self) -> Properties {
        lock(&self.properties).clone()
    }

    /// Stop the service: give the names back, let calls in flight be answered — for at most
    /// `SHUTDOWN_GRACE` — and close the connection. The same as dropping the handle, spelled
    /// out for the way out.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.updates = None;
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::warn!("the D-Bus service thread panicked");
        }
    }
}

impl Drop for DbusHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The D-Bus signals and properties are one of the consumers of the controller's events.
impl EventSink for DbusHandle {
    fn publish(&self, event: &AppEvent) {
        Self::publish(self, event);
    }
}

impl std::fmt::Debug for DbusHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbusHandle")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The service thread: connect, serve until the handle is dropped, give the names back.
fn serve(
    bus: &Bus,
    service: Service,
    mut updates: mpsc::UnboundedReceiver<Update>,
    state: &StateCell,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            log::warn!("no D-Bus service: its runtime did not start ({err})");
            state.set(ServiceState::Unavailable);
            return;
        }
    };
    let in_flight = Arc::clone(&service.in_flight);

    // Everything zbus is dropped inside `block_on`: a connection spawns its clean-up on the
    // runtime it was built on.
    runtime.block_on(async {
        let connecting = async {
            tokio::time::timeout(CONNECT_TIMEOUT, connect(bus, service))
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "the bus did not answer within {} s",
                        CONNECT_TIMEOUT.as_secs()
                    ))
                })
        };
        let (connection, iface) = match until_stopped(connecting, &mut updates).await {
            Some(Ok(connected)) => connected,
            Some(Err(why)) => {
                log::warn!("no D-Bus service: {why}; the control socket carries on alone");
                state.set(ServiceState::Unavailable);
                return;
            }
            // Stopped before the bus answered.
            None => {
                state.set(ServiceState::Unavailable);
                return;
            }
        };
        state.set(ServiceState::Serving);
        log::info!("serving {INTERFACE} at {OBJECT_PATH} on the bus as {BUS_NAME}");

        while let Some(update) = updates.recv().await {
            if let Err(err) = emit(&iface, update).await {
                log::debug!("a D-Bus signal was not emitted: {err}");
            }
        }

        // The handle is gone: the instance is quitting.
        state.set(ServiceState::Unavailable);
        drop(iface);
        let wind_down = async {
            // A call already answered — `Quit` itself — gets its reply out; the replies go
            // out on this connection ahead of the name releases, which are round trips.
            in_flight.idle().await;
            for name in [BUS_NAME, DESKTOP_BUS_NAME] {
                let _ = connection.release_name(name).await;
            }
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, wind_down)
            .await
            .is_err()
        {
            log::debug!("closing the D-Bus connection with calls still in flight");
        }
        drop(connection);
    });
    state.set(ServiceState::Unavailable);
}

/// Connect to `bus`, serve the interface and own the names.
async fn connect(
    bus: &Bus,
    service: Service,
) -> Result<(zbus::Connection, InterfaceRef<Service>), String> {
    let builder = match bus {
        Bus::Session => zbus::connection::Builder::session()
            .map_err(|err| format!("there is no session bus ({err})"))?,
        Bus::Address(address) => zbus::connection::Builder::address(address.as_str())
            .map_err(|err| format!("{address} is not a bus address ({err})"))?,
    };
    // The object is served before a name is asked for, so no call to the name can miss it.
    let connection = builder
        .serve_at(OBJECT_PATH, service)
        .map_err(|err| format!("{OBJECT_PATH} could not be served ({err})"))?
        .build()
        .await
        .map_err(|err| format!("the bus could not be reached ({err})"))?;
    // Not queued behind an owner: a service that would only answer once somebody else quits is
    // not one to report as serving.
    let flags = fdo::RequestNameFlags::DoNotQueue.into();
    match connection.request_name_with_flags(BUS_NAME, flags).await {
        Ok(_) => {}
        Err(zbus::Error::NameTaken) => {
            return Err(format!("{BUS_NAME} is owned by another process on the bus"));
        }
        Err(err) => return Err(format!("{BUS_NAME} could not be owned ({err})")),
    }
    // The desktop-entry name is a courtesy to the shell; the API works without it.
    if let Err(err) = connection
        .request_name_with_flags(DESKTOP_BUS_NAME, flags)
        .await
    {
        log::warn!("D-Bus: {DESKTOP_BUS_NAME} could not be owned ({err}); serving {BUS_NAME}");
    }
    let iface = connection
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await
        .map_err(|err| format!("{OBJECT_PATH} is not served after all ({err})"))?;
    Ok((connection, iface))
}

/// `work`, unless the handle goes away first — `None` then. Nothing is sent to the service
/// before it is serving, so all the channel can say meanwhile is that it closed.
async fn until_stopped<F: Future>(
    work: F,
    updates: &mut mpsc::UnboundedReceiver<Update>,
) -> Option<F::Output> {
    let mut work = pin!(work);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        while let Poll::Ready(update) = updates.poll_recv(cx) {
            if update.is_none() {
                return Poll::Ready(None);
            }
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::InputMeters;
    use crate::events::LaneState;
    use crate::ipc::{Forwarded, Instance};
    use crate::private_bus::PrivateBus;
    use clap::Parser as _;
    use fxsound_core::NoiseSuppressionOverride;
    use std::sync::atomic::AtomicBool;

    // ---- the method → command mapping --------------------------------------------------------

    /// What `fxsound ARGS` hands the running instance.
    fn cli(args: &[&str]) -> Vec<Command> {
        Cli::try_parse_from(std::iter::once("fxsound").chain(args.iter().copied()))
            .expect("the option parses")
            .commands()
    }

    /// The same, without the window raise a typed command line adds and a bus call leaves out.
    fn cli_without_raise(args: &[&str]) -> Vec<Command> {
        cli(args)
            .into_iter()
            .filter(|command| *command != Command::Window(WindowCommand::Show))
            .collect()
    }

    fn commands(call: Call) -> Vec<Command> {
        call.commands().expect("the call maps to commands")
    }

    #[test]
    fn toggle_power_is_toggle_power_followed_by_the_status_it_answers_from() {
        let mut expected = cli(&["--toggle-power"]);
        expected.push(Command::Status { json: true });
        assert_eq!(commands(Call::TogglePower), expected);
    }

    #[test]
    fn set_power_is_power_on_or_off() {
        assert_eq!(
            commands(Call::SetPower(true)),
            cli_without_raise(&["--power=1"])
        );
        assert_eq!(
            commands(Call::SetPower(false)),
            cli_without_raise(&["--power=0"])
        );
    }

    #[test]
    fn next_preset_and_prev_preset_are_the_keybind_options() {
        assert_eq!(commands(Call::NextPreset), cli(&["--next-preset"]));
        assert_eq!(commands(Call::PrevPreset), cli(&["--prev-preset"]));
    }

    #[test]
    fn set_preset_selects_by_the_exact_name_as_preset_does() {
        assert_eq!(
            commands(Call::SetPreset("Bass Booster".to_owned())),
            cli_without_raise(&["--preset", "Bass Booster"])
        );
    }

    #[test]
    fn set_preset_with_an_empty_name_is_an_invalid_argument_rather_than_nothing() {
        let why = Call::SetPreset(String::new()).commands().unwrap_err();
        assert!(why.contains("empty"), "{why}");
    }

    #[test]
    fn get_preset_and_get_status_ask_for_the_json_status_document() {
        let status = cli(&["--status", "--json"]);
        assert_eq!(commands(Call::GetPreset), status);
        assert_eq!(commands(Call::GetStatus), status);
    }

    #[test]
    fn set_output_selects_a_device_or_detaches_the_lane_like_output() {
        assert_eq!(
            commands(Call::SetOutput("alsa_output.usb".to_owned())),
            cli_without_raise(&["--output", "alsa_output.usb"])
        );
        assert_eq!(
            commands(Call::SetOutput("Off".to_owned())),
            cli_without_raise(&["--output", "Off"])
        );
        assert_eq!(
            commands(Call::SetOutput("off".to_owned())),
            vec![Command::Output(DeviceCommand::Detach)]
        );
    }

    #[test]
    fn set_input_selects_a_microphone_or_detaches_the_lane_like_input() {
        assert_eq!(
            commands(Call::SetInput("Headset Microphone".to_owned())),
            cli_without_raise(&["--input", "Headset Microphone"])
        );
        assert_eq!(
            commands(Call::SetInput("off".to_owned())),
            vec![Command::Input(DeviceCommand::Detach)]
        );
    }

    #[test]
    fn a_device_call_with_an_empty_name_is_an_invalid_argument() {
        for call in [
            Call::SetOutput(String::new()),
            Call::SetInput(String::new()),
        ] {
            let why = call.commands().unwrap_err();
            assert!(why.contains("off"), "{why}");
        }
    }

    #[test]
    fn next_output_and_next_input_each_cycle_their_own_lane() {
        assert_eq!(commands(Call::NextOutput), cli(&["--next-output"]));
        assert_eq!(commands(Call::NextInput), cli(&["--next-input"]));
        assert_eq!(
            commands(Call::NextInput),
            vec![Command::Input(DeviceCommand::Next)]
        );
    }

    #[test]
    fn set_noise_suppression_takes_what_the_option_takes_mild_included() {
        for level in [
            "off", "light", "medium", "strong", "preset", "Mild", " STRONG ",
        ] {
            assert_eq!(
                commands(Call::SetNoiseSuppression(level.to_owned())),
                cli(&["--noise-suppression", level]),
                "{level}"
            );
        }
        assert_eq!(
            commands(Call::SetNoiseSuppression("mild".to_owned())),
            vec![Command::NoiseSuppression(
                NoiseSuppressionOverride::from_key("light").unwrap()
            )]
        );
    }

    #[test]
    fn set_noise_suppression_refuses_a_level_it_does_not_know_in_the_options_words() {
        let why = Call::SetNoiseSuppression("loud".to_owned())
            .commands()
            .unwrap_err();
        assert!(
            why.contains("expected preset, off, light, medium or strong"),
            "{why}"
        );
        assert!(Cli::try_parse_from(["fxsound", "--noise-suppression", "loud"]).is_err());
    }

    #[test]
    fn set_edit_direction_parses_like_edit() {
        assert_eq!(
            commands(Call::SetEditDirection("input".to_owned())),
            cli_without_raise(&["--edit", "input"])
        );
        assert_eq!(
            commands(Call::SetEditDirection("Output".to_owned())),
            vec![Command::EditDirection(DeviceDirection::Output)]
        );
        let why = Call::SetEditDirection("sideways".to_owned())
            .commands()
            .unwrap_err();
        assert!(why.contains("expected output or input"), "{why}");
    }

    fn set_app_preset(app: &str, direction: &str, preset: &str) -> Call {
        Call::SetAppPreset {
            app: app.to_owned(),
            direction: direction.to_owned(),
            preset: preset.to_owned(),
        }
    }

    #[test]
    fn set_app_preset_is_app_preset_or_app_input_preset_by_its_direction() {
        assert_eq!(
            commands(set_app_preset("bf6.exe", "output", "Gaming")),
            cli(&["--app-preset=bf6.exe=Gaming"])
        );
        assert_eq!(
            commands(set_app_preset(" Battlefield 6 ", "INPUT", "Bass=Max")),
            cli(&["--app-input-preset=Battlefield 6=Bass=Max"]),
            "the words the options take, the application trimmed and the preset whole"
        );
        for follow in ["", "default", "Follow", "  "] {
            assert_eq!(
                commands(set_app_preset("brave", "output", follow)),
                cli(&["--app-preset=brave=default"]),
                "{follow:?} follows the lane, as an empty preset reads on the bus"
            );
        }
    }

    #[test]
    fn set_app_preset_refuses_an_empty_application_and_a_direction_it_does_not_know() {
        let why = set_app_preset("  ", "output", "Gaming")
            .commands()
            .unwrap_err();
        assert!(why.contains("application name cannot be empty"), "{why}");
        let why = set_app_preset("bf6.exe", "sideways", "Gaming")
            .commands()
            .unwrap_err();
        assert!(why.contains("expected output or input"), "{why}");
    }

    #[test]
    fn list_apps_is_list_apps_as_json() {
        assert_eq!(commands(Call::ListApps), cli(&["--list-apps", "--json"]));
    }

    #[test]
    fn show_hide_and_toggle_window_are_the_window_options() {
        assert_eq!(commands(Call::Show), cli(&["--show"]));
        assert_eq!(commands(Call::Hide), cli(&["--hide"]));
        assert_eq!(commands(Call::ToggleWindow), cli(&["--toggle-window"]));
    }

    #[test]
    fn quit_is_quit() {
        assert_eq!(commands(Call::Quit), cli(&["--quit"]));
    }

    /// One of each, with arguments that parse.
    fn every_call() -> Vec<Call> {
        vec![
            Call::TogglePower,
            Call::SetPower(true),
            Call::NextPreset,
            Call::PrevPreset,
            Call::SetPreset("Rock".to_owned()),
            Call::GetPreset,
            Call::SetOutput("Speakers".to_owned()),
            Call::SetInput("Headset".to_owned()),
            Call::NextOutput,
            Call::NextInput,
            Call::SetNoiseSuppression("medium".to_owned()),
            Call::SetEditDirection("input".to_owned()),
            Call::GetStatus,
            Call::Show,
            Call::Hide,
            Call::ToggleWindow,
            Call::Quit,
            Call::Apply(vec!["--preset".to_owned(), "Rock".to_owned()]),
            Call::ListPresets,
            Call::ListDevices,
            set_app_preset("bf6.exe", "output", "Gaming"),
            Call::ListApps,
        ]
    }

    #[test]
    fn only_show_and_toggle_window_bring_the_window_up() {
        for call in every_call() {
            let raises = commands(call.clone()).iter().any(|command| {
                matches!(
                    command,
                    Command::Window(WindowCommand::Show | WindowCommand::Toggle)
                )
            });
            assert_eq!(
                raises,
                matches!(call, Call::Show | Call::ToggleWindow),
                "{call:?}"
            );
        }
    }

    #[test]
    fn every_call_has_its_own_member_name() {
        let mut members: Vec<_> = every_call().iter().map(Call::member).collect();
        members.sort_unstable();
        members.dedup();
        assert_eq!(members.len(), 22);
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|&arg| arg.to_owned()).collect()
    }

    #[test]
    fn apply_runs_the_line_fxsound_would_run_without_raising_the_window() {
        let line = [
            "--preset",
            "Rock",
            "--set_effect=bass:7.5",
            "--num_bands=31",
        ];
        assert_eq!(commands(Call::Apply(argv(&line))), cli_without_raise(&line));
        // `--preset` alone raises a typed line's window; a bus call's is left where it is.
        assert!(cli(&line).contains(&Command::Window(WindowCommand::Show)));
        assert_eq!(commands(Call::Apply(Vec::new())), Vec::<Command>::new());
    }

    #[test]
    fn apply_keeps_the_window_options_the_line_spells_out() {
        for (option, window) in [
            ("--show", WindowCommand::Show),
            ("--toggle-window", WindowCommand::Toggle),
            ("--hide", WindowCommand::Hide),
        ] {
            assert_eq!(
                commands(Call::Apply(argv(&["--preset", "Rock", option]))),
                cli(&["--preset", "Rock", option]),
                "{option}"
            );
            assert!(
                commands(Call::Apply(argv(&[option]))).contains(&Command::Window(window)),
                "{option}"
            );
        }
    }

    #[test]
    fn apply_takes_argv_with_or_without_the_program_name() {
        let bare = commands(Call::Apply(argv(&["--next-preset"])));
        assert_eq!(
            commands(Call::Apply(argv(&["fxsound", "--next-preset"]))),
            bare
        );
        assert_eq!(
            commands(Call::Apply(argv(&["/usr/bin/fxsound", "--next-preset"]))),
            bare
        );
        assert_eq!(bare, cli(&["--next-preset"]));
    }

    #[test]
    fn apply_answers_a_line_that_does_not_parse_as_fxsound_does_rather_than_with_a_bus_error() {
        let refused = apply_commands(&argv(&["--bass-boost", "11"])).unwrap_err();
        assert!(!refused.ok);
        assert!(refused.stdout.is_empty());
        assert!(
            refused.stderr.contains("--bass-boost"),
            "{}",
            refused.stderr
        );
        assert_eq!(
            applied(refused.clone()).unwrap(),
            (false, String::new(), refused.stderr)
        );

        let out_of_range = apply_commands(&argv(&["--balance=99"])).unwrap_err();
        assert!(!out_of_range.ok);
        assert!(!out_of_range.stderr.is_empty());
    }

    #[test]
    fn apply_answers_help_and_version_on_stdout() {
        let help = apply_commands(&argv(&["--help"])).unwrap_err();
        assert!(help.ok);
        assert!(help.stdout.contains("--status"), "{}", help.stdout);
        let version = apply_commands(&argv(&["--version"])).unwrap_err();
        assert!(version.ok);
        assert!(
            version.stdout.contains(env!("CARGO_PKG_VERSION")),
            "{}",
            version.stdout
        );
    }

    #[test]
    fn apply_answers_with_the_command_lines_outcome_and_fails_only_without_an_answer() {
        assert_eq!(
            applied(Response::output("power: on")).unwrap(),
            (true, "power: on".to_owned(), String::new())
        );
        let refused = Response::failed("no output preset is called \"Nope\"");
        assert_eq!(
            applied(refused).unwrap(),
            (
                false,
                String::new(),
                "no output preset is called \"Nope\"".to_owned()
            )
        );
        match applied(Response::unanswered()) {
            Err(MethodError::Standard(fdo::Error::Failed(text))) => {
                assert!(text.contains("did not answer"), "{text}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn apply_runs_through_the_same_commands_as_the_command_line() {
        let (mut app, _dir) = app_with_presets("apply", &["Alpha", "Beta"]);
        let run = |app: &mut App, args: &[&str]| {
            let commands = apply_commands(&argv(args)).expect("the line parses");
            crate::commands::run(app, &commands)
        };
        let outcome = run(&mut app, &["--preset", "Beta", "--set_effect=bass:7.5"]);
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert!(outcome.window.is_empty(), "{:?}", outcome.window);
        assert_eq!(Properties::of(&app).preset(), "Beta");

        let outcome = run(&mut app, &["--status", "--json"]);
        let status: Value = serde_json::from_str(&outcome.stdout).expect("the status document");
        assert_eq!(status["effects"]["bass"].as_f64(), Some(7.5));

        let outcome = run(&mut app, &["--preset", "Nope"]);
        assert!(outcome.failed);
        assert!(outcome.stderr.contains("Nope"), "{}", outcome.stderr);

        let outcome = run(&mut app, &["--watch"]);
        assert!(outcome.failed, "a stream is not an answer");
    }

    #[test]
    fn a_listing_is_the_schema_and_its_keys_of_the_status_document() {
        let document = r#"{"schema":2,"version":"0.4.0","presets":{"built_in":[]},
            "edit_direction":"output","selected_preset":"Rock","output_presets":{},
            "input_presets":{},"selected_output":null,"selected_input":"Headset",
            "output_devices":[],"input_devices":["Headset"],"output_device_list":[],
            "input_device_list":[{"node_name":"alsa_input.usb"}]}"#;
        let keys = |json: &str| -> Vec<String> {
            let value: Value = serde_json::from_str(json).expect("JSON");
            value
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect()
        };
        let presets = listing_of_status::<PresetListing>(document).unwrap();
        let mut listed = keys(&presets);
        listed.sort_unstable();
        assert_eq!(
            listed,
            [
                "edit_direction",
                "input_presets",
                "output_presets",
                "presets",
                "schema",
                "selected_preset"
            ]
        );
        assert!(presets.starts_with(r#"{"schema":2,"#), "{presets}");
        assert!(presets.contains(r#""selected_preset":"Rock""#), "{presets}");

        let devices = listing_of_status::<DeviceListing>(document).unwrap();
        let mut listed = keys(&devices);
        listed.sort_unstable();
        assert_eq!(
            listed,
            [
                "input_device_list",
                "output_device_list",
                "schema",
                "selected_input",
                "selected_output"
            ]
        );
        assert!(devices.contains(r#""selected_output":null"#), "{devices}");
        assert!(devices.contains(r#""input_device_list":[{"node_name":"alsa_input.usb"}]"#));

        assert!(listing_of_status::<PresetListing>("[]").is_err());
        assert!(listing_of_status::<DeviceListing>(r#"{"schema":2}"#).is_err());
        assert!(listing_of_status::<PresetListing>("not json").is_err());
    }

    #[test]
    fn the_real_status_document_lists_both_lanes_presets_and_devices() {
        let (mut app, _dir) = app_with_presets("listings", &["Alpha", "Beta"]);
        app.state.devices = vec![fxsound_core::AudioDevice {
            id: 7,
            name: "alsa_input.usb".to_owned(),
            description: "Headset".to_owned(),
            is_default: true,
            direction: DeviceDirection::Input,
            form_factor: "headset".to_owned(),
        }];
        let status = crate::commands::run(&mut app, &commands(Call::ListPresets)).stdout;
        let presets: Value =
            serde_json::from_str(&listing_of_status::<PresetListing>(&status).unwrap()).unwrap();
        assert_eq!(presets["schema"], crate::commands::STATUS_SCHEMA);
        let names = |list: &Value| -> Vec<String> {
            list["built_in"]
                .as_array()
                .unwrap()
                .iter()
                .map(|preset| preset["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(names(&presets["output_presets"]), ["Alpha", "Beta"]);
        assert_eq!(names(&presets["input_presets"]), ["Clean Voice"]);
        assert_eq!(presets["presets"], presets["output_presets"]);

        let status = crate::commands::run(&mut app, &commands(Call::ListDevices)).stdout;
        let devices: Value =
            serde_json::from_str(&listing_of_status::<DeviceListing>(&status).unwrap()).unwrap();
        assert_eq!(
            devices["input_device_list"][0]["node_name"],
            "alsa_input.usb"
        );
        assert_eq!(devices["input_device_list"][0]["description"], "Headset");
        assert_eq!(devices["input_device_list"][0]["present"], true);
        assert_eq!(devices["output_device_list"], serde_json::json!([]));
        assert!(
            devices.get("input_devices").is_none(),
            "upstream's names-only list stays in GetStatus: {devices}"
        );
    }

    // ---- answers --------------------------------------------------------------------------------

    fn refused_text(result: Result<String, MethodError>) -> String {
        match result {
            Err(MethodError::Refused(text)) => text,
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_comes_back_as_refused_with_the_text_fxsound_prints() {
        let text = refused_text(answer(Response::failed(
            "no output preset is called \"Nope\"",
        )));
        assert_eq!(text, "no output preset is called \"Nope\"");
    }

    #[test]
    fn a_refusal_without_text_still_says_something() {
        let mut response = Response::failed("");
        response.stderr.clear();
        assert!(!refused_text(answer(response)).is_empty());
    }

    #[test]
    fn no_answer_in_time_is_a_failure_rather_than_a_refusal() {
        match answer(Response::unanswered()) {
            Err(MethodError::Standard(fdo::Error::Failed(text))) => {
                assert!(text.contains("did not answer"), "{text}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_goes_on_the_bus_as_org_fxsound_fxsound_error_refused_and_the_rest_as_standard() {
        use zbus::DBusError as _;
        let refused = MethodError::Refused("\"Rock\" is a factory preset".to_owned());
        assert_eq!(refused.name().as_str(), "org.fxsound.FxSound.Error.Refused");
        assert_eq!(refused.description(), Some("\"Rock\" is a factory preset"));
        let invalid = MethodError::from(fdo::Error::InvalidArgs("loud".to_owned()));
        assert_eq!(
            invalid.name().as_str(),
            "org.freedesktop.DBus.Error.InvalidArgs"
        );
        assert_eq!(invalid.description(), Some("loud"));
    }

    #[test]
    fn an_accepted_command_answers_with_its_stdout() {
        assert_eq!(answer(Response::output("{}")).unwrap(), "{}");
        assert_eq!(answer(Response::ok()).unwrap(), "");
    }

    #[test]
    fn the_power_toggle_answer_is_read_from_the_status_document() {
        assert_eq!(power_of_status(r#"{"power":true}"#), Ok(true));
        assert_eq!(power_of_status(r#"{"power":false}"#), Ok(false));
        assert!(power_of_status(r#"{"preset":"Rock"}"#).is_err());
    }

    #[test]
    fn the_preset_answer_is_the_edit_directions_preset_by_the_name_set_preset_takes() {
        let document = r#"{"edit_direction":"input","preset":"Clean Voice*",
            "output":{"preset":"Rock"},"input":{"preset":"Clean Voice","modified":true}}"#;
        assert_eq!(preset_of_status(document).unwrap(), "Clean Voice");
        let document = document.replace(
            r#""edit_direction":"input""#,
            r#""edit_direction":"output""#,
        );
        assert_eq!(preset_of_status(&document).unwrap(), "Rock");
    }

    #[test]
    fn a_lane_with_no_preset_answers_an_empty_name() {
        let document = r#"{"edit_direction":"output","output":{"preset":null}}"#;
        assert_eq!(preset_of_status(document).unwrap(), "");
    }

    #[test]
    fn a_status_answer_that_is_not_the_document_is_a_failure_not_a_panic() {
        assert!(power_of_status("not json").is_err());
        assert!(preset_of_status("[]").is_err());
        assert!(preset_of_status(r#"{"edit_direction":"sideways"}"#).is_err());
    }

    #[test]
    fn the_real_status_document_answers_both_questions() {
        let mut app = App::headless_for_tests();
        let outcome = crate::commands::run(&mut app, &commands(Call::TogglePower));
        assert!(!outcome.failed, "{}", outcome.stderr);
        assert_eq!(power_of_status(&outcome.stdout), Ok(app.state.power));

        let outcome = crate::commands::run(&mut app, &commands(Call::GetPreset));
        assert_eq!(preset_of_status(&outcome.stdout).unwrap(), "");
    }

    #[test]
    fn a_users_own_preset_answers_on_the_bus_in_either_lane() {
        // SetPreset, GetPreset and the Preset property run the command line's commands, so a
        // voice preset the user saved is as reachable as a `.fac` they saved, and each lane keeps
        // its own.
        let (mut app, _dir) = app_with_presets("own-preset", &["Alpha", "Beta"]);
        let call = |app: &mut App, call: Call| {
            let outcome = crate::commands::run(app, &commands(call));
            assert!(!outcome.failed, "{}", outcome.stderr);
            outcome.stdout
        };
        for (lane, saved) in [("output", "My Music"), ("input", "My Voice")] {
            call(&mut app, Call::SetEditDirection(lane.to_owned()));
            let shipped = app.state.presets[0].name.clone();
            call(&mut app, Call::SetPreset(shipped.clone()));
            let outcome = crate::commands::run(
                &mut app,
                &[
                    Command::BandGains(vec![(0, 6.0)]),
                    Command::Preset(PresetCommand::SaveAs(saved.to_owned())),
                ],
            );
            assert!(!outcome.failed, "{}", outcome.stderr);
            assert_eq!(
                Properties::of(&app).preset(),
                saved,
                "{lane}: saved and selected"
            );
            call(&mut app, Call::SetPreset(shipped.clone()));
            assert_eq!(Properties::of(&app).preset(), shipped, "{lane}");
            call(&mut app, Call::SetPreset(saved.to_owned()));
            let status = call(&mut app, Call::GetPreset);
            assert_eq!(preset_of_status(&status).unwrap(), saved, "{lane}");
            assert_eq!(Properties::of(&app).preset(), saved, "{lane}");
        }
        assert_eq!(
            Properties::of(&app).presets,
            [Some("My Music".to_owned()), Some("My Voice".to_owned())]
        );
    }

    #[test]
    fn toggle_power_answers_with_the_state_the_toggle_left() {
        let mut app = App::headless_for_tests();
        let before = app.state.power;
        let outcome = crate::commands::run(&mut app, &commands(Call::TogglePower));
        assert_eq!(power_of_status(&outcome.stdout), Ok(!before));
        let outcome = crate::commands::run(&mut app, &commands(Call::TogglePower));
        assert_eq!(power_of_status(&outcome.stdout), Ok(before));
    }

    // ---- properties and signals -------------------------------------------------------------

    fn properties() -> Properties {
        Properties {
            power: true,
            direction: DeviceDirection::Output,
            presets: [Some("Rock".to_owned()), Some("Clean Voice".to_owned())],
            devices: [
                Some(("alsa_output.pci".to_owned(), "Speakers".to_owned())),
                None,
            ],
        }
    }

    #[test]
    fn the_getters_read_the_edit_directions_preset_and_each_lanes_description() {
        let mut properties = properties();
        assert_eq!(properties.preset(), "Rock");
        assert_eq!(properties.device(DeviceDirection::Output), "Speakers");
        assert_eq!(properties.device(DeviceDirection::Input), "");
        properties.direction = DeviceDirection::Input;
        assert_eq!(properties.preset(), "Clean Voice");
    }

    #[test]
    fn the_properties_start_from_the_controller() {
        let mut app = App::headless_for_tests();
        app.state.devices = vec![fxsound_core::AudioDevice {
            id: 0,
            name: "alsa_output.pci".to_owned(),
            description: "Speakers".to_owned(),
            is_default: true,
            direction: DeviceDirection::Output,
            form_factor: String::new(),
        }];
        app.state.selected_output = Some(0);
        let properties = Properties::of(&app);
        assert_eq!(properties.power, app.state.power);
        assert_eq!(properties.direction, app.state.direction);
        assert_eq!(properties.device(DeviceDirection::Output), "Speakers");
        assert_eq!(properties.device(DeviceDirection::Input), "");
    }

    #[test]
    fn a_power_event_changes_power_and_is_signalled() {
        let mut properties = properties();
        let update = properties.fold(&AppEvent::Power { on: false }, 0);
        assert_eq!(update.signal, Some(Signal::PowerChanged(false)));
        assert_eq!(update.changed, vec![Property::Power]);
        assert!(!properties.power);
    }

    #[test]
    fn an_event_that_repeats_the_properties_is_nothing_on_the_bus() {
        let mut properties = properties();
        assert!(properties.fold(&AppEvent::Power { on: true }, 0).is_empty());
        assert!(
            properties
                .fold(
                    &AppEvent::Direction {
                        direction: DeviceDirection::Output
                    },
                    0
                )
                .is_empty()
        );
    }

    #[test]
    fn a_preset_of_the_edit_direction_changes_preset_and_is_signalled_with_its_lane() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::PresetChanged {
                direction: DeviceDirection::Output,
                name: Some("Jazz".to_owned()),
                modified: false,
            },
            0,
        );
        assert_eq!(
            update.signal,
            Some(Signal::PresetChanged {
                direction: DeviceDirection::Output,
                name: "Jazz".to_owned()
            })
        );
        assert_eq!(update.changed, vec![Property::Preset]);
        assert_eq!(properties.preset(), "Jazz");
    }

    #[test]
    fn a_preset_of_the_other_lane_is_signalled_but_leaves_the_preset_property_alone() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::PresetChanged {
                direction: DeviceDirection::Input,
                name: Some("Podcast".to_owned()),
                modified: false,
            },
            0,
        );
        assert_eq!(
            update.signal.as_ref().map(Signal::member),
            Some("PresetChanged")
        );
        assert!(update.changed.is_empty());
        assert_eq!(properties.preset(), "Rock");
    }

    #[test]
    fn a_preset_gaining_unsaved_changes_is_not_a_new_preset_on_the_bus() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::PresetChanged {
                direction: DeviceDirection::Output,
                name: Some("Rock".to_owned()),
                modified: true,
            },
            0,
        );
        assert!(update.is_empty(), "{update:?}");
    }

    #[test]
    fn a_lane_losing_its_preset_is_signalled_with_an_empty_name() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::PresetChanged {
                direction: DeviceDirection::Output,
                name: None,
                modified: false,
            },
            0,
        );
        assert_eq!(
            update.signal,
            Some(Signal::PresetChanged {
                direction: DeviceDirection::Output,
                name: String::new()
            })
        );
        assert_eq!(properties.preset(), "");
    }

    #[test]
    fn a_new_microphone_changes_input_and_is_signalled_with_both_names() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::DeviceChanged {
                direction: DeviceDirection::Input,
                node_name: Some("alsa_input.usb".to_owned()),
                description: Some("Headset".to_owned()),
            },
            0,
        );
        assert_eq!(
            update.signal,
            Some(Signal::DeviceChanged {
                direction: DeviceDirection::Input,
                node_name: "alsa_input.usb".to_owned(),
                description: "Headset".to_owned(),
            })
        );
        assert_eq!(update.changed, vec![Property::Input]);
        assert_eq!(properties.device(DeviceDirection::Input), "Headset");
    }

    #[test]
    fn a_detached_lane_is_signalled_with_empty_names_and_an_empty_property() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::DeviceChanged {
                direction: DeviceDirection::Output,
                node_name: None,
                description: None,
            },
            0,
        );
        assert_eq!(
            update.signal,
            Some(Signal::DeviceChanged {
                direction: DeviceDirection::Output,
                node_name: String::new(),
                description: String::new(),
            })
        );
        assert_eq!(update.changed, vec![Property::Output]);
        assert_eq!(properties.device(DeviceDirection::Output), "");
    }

    #[test]
    fn two_devices_with_one_description_are_still_two_devices() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::DeviceChanged {
                direction: DeviceDirection::Output,
                node_name: Some("alsa_output.usb-2".to_owned()),
                description: Some("Speakers".to_owned()),
            },
            0,
        );
        assert_eq!(
            update.signal.as_ref().map(Signal::member),
            Some("DeviceChanged")
        );
    }

    #[test]
    fn switching_the_edit_direction_changes_direction_and_the_preset_it_shows() {
        let mut properties = properties();
        let update = properties.fold(
            &AppEvent::Direction {
                direction: DeviceDirection::Input,
            },
            0,
        );
        assert_eq!(update.signal, None);
        assert_eq!(update.changed, vec![Property::Direction, Property::Preset]);
        assert_eq!(properties.preset(), "Clean Voice");
    }

    #[test]
    fn switching_between_lanes_with_the_same_preset_name_leaves_preset_alone() {
        let mut properties = properties();
        properties.presets = [Some("Flat".to_owned()), Some("Flat".to_owned())];
        let update = properties.fold(
            &AppEvent::Direction {
                direction: DeviceDirection::Input,
            },
            0,
        );
        assert_eq!(update.changed, vec![Property::Direction]);
    }

    #[test]
    fn audio_state_is_signalled_as_the_line_watch_json_prints() {
        let event = AppEvent::AudioState {
            direction: DeviceDirection::Output,
            state: LaneState::Processing,
            sample_rate: 48_000,
            channels: 2,
        };
        let update = properties().fold(&event, 1_790_000_000_000);
        assert_eq!(
            update.signal,
            Some(Signal::AudioStateChanged(event.to_json(1_790_000_000_000)))
        );
        assert!(update.changed.is_empty());
        let Some(Signal::AudioStateChanged(json)) = update.signal else {
            unreachable!()
        };
        let json: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["event"], "audio_state");
        assert_eq!(json["state"], "processing");
    }

    #[test]
    fn a_notice_is_signalled_with_its_text_every_time() {
        let mut properties = properties();
        let notice = AppEvent::Notice {
            message: "Preset saved".to_owned(),
        };
        for _ in 0..2 {
            assert_eq!(
                properties.fold(&notice, 0).signal,
                Some(Signal::Notice("Preset saved".to_owned()))
            );
        }
    }

    #[test]
    fn an_application_routed_is_signalled_by_the_name_the_window_shows_and_back_as_empty() {
        let mut properties = properties();
        let before = properties.clone();
        let game = fxsound_core::AppKey {
            binary: "bf6.exe".to_owned(),
            name: "Battlefield 6".to_owned(),
            flatpak: String::new(),
        };
        let moved = AppEvent::AppRouted {
            app: game.clone(),
            direction: DeviceDirection::Output,
            preset: Some("Gaming".to_owned()),
        };
        assert_eq!(
            properties.fold(&moved, 0),
            Update {
                signal: Some(Signal::AppRouted {
                    app: "Battlefield 6".to_owned(),
                    direction: DeviceDirection::Output,
                    preset: "Gaming".to_owned(),
                }),
                changed: Vec::new(),
            }
        );
        let back = AppEvent::AppRouted {
            app: fxsound_core::AppKey {
                name: String::new(),
                ..game
            },
            direction: DeviceDirection::Input,
            preset: None,
        };
        assert_eq!(
            properties.fold(&back, 0).signal,
            Some(Signal::AppRouted {
                app: "bf6.exe".to_owned(),
                direction: DeviceDirection::Input,
                preset: String::new(),
            }),
            "no name: the program, as the window shows it"
        );
        assert_eq!(properties, before, "no property holds routes");
    }

    #[test]
    fn events_the_bus_has_no_signal_for_change_nothing() {
        let mut properties = properties();
        let before = properties.clone();
        for event in [
            AppEvent::Status(Value::Null),
            AppEvent::DevicesChanged { count: 3 },
            AppEvent::InputMeters(InputMeters {
                voice_probability: 0.5,
                noise_floor_db: None,
                denoise_reduction_db: 0.0,
                gate_reduction_db: 0.0,
                compressor_reduction_db: 0.0,
                deesser_reduction_db: 0.0,
                denoise_running: false,
                deesser_running: false,
            }),
            AppEvent::EchoCancel {
                on: true,
                running: false,
                detail: Some("no WebRTC module".to_owned()),
            },
            AppEvent::Calibrated(fxsound_core::settings::CalibrationRecord::default()),
            AppEvent::Window { visible: false },
            AppEvent::Quit,
        ] {
            assert!(properties.fold(&event, 0).is_empty(), "{event:?}");
        }
        assert_eq!(properties, before);
    }

    // ---- the interface as the bus sees it -----------------------------------------------------

    fn introspection() -> String {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let service = Service {
            control: Control::from_sender(tx),
            properties: Arc::default(),
            in_flight: Arc::default(),
        };
        let mut xml = String::new();
        zbus::object_server::Interface::introspect_to_writer(&service, &mut xml, 0);
        xml
    }

    /// The `<kind name="name">` element, whole: up to its `/>` if it has no children, and up to
    /// its closing tag if it has.
    fn element<'x>(xml: &'x str, kind: &str, name: &str) -> &'x str {
        let open = format!(r#"<{kind} name="{name}""#);
        let start = xml
            .find(&open)
            .unwrap_or_else(|| panic!("no {kind} {name} in:\n{xml}"));
        let rest = &xml[start..];
        let tag_end = rest.find('>').expect("the opening tag ends");
        if rest[..tag_end].ends_with('/') {
            return &rest[..=tag_end];
        }
        let close = format!("</{kind}>");
        let end = rest.find(&close).expect("the element is closed");
        &rest[..end + close.len()]
    }

    #[test]
    fn the_interface_is_named_as_documented() {
        assert!(introspection().starts_with(r#"<interface name="org.fxsound.FxSound">"#));
        assert_eq!(
            <Service as zbus::object_server::Interface>::name().as_str(),
            INTERFACE
        );
    }

    #[test]
    fn every_method_is_served_with_its_documented_signature() {
        let xml = introspection();
        let signatures: [(&str, &[&str]); 22] = [
            ("TogglePower", &[r#"type="b" direction="out""#]),
            ("SetPower", &[r#"type="b" direction="in""#]),
            ("NextPreset", &[]),
            ("PrevPreset", &[]),
            ("SetPreset", &[r#"type="s" direction="in""#]),
            ("GetPreset", &[r#"type="s" direction="out""#]),
            ("SetOutput", &[r#"type="s" direction="in""#]),
            ("SetInput", &[r#"type="s" direction="in""#]),
            ("NextOutput", &[]),
            ("NextInput", &[]),
            ("SetNoiseSuppression", &[r#"type="s" direction="in""#]),
            ("SetEditDirection", &[r#"type="s" direction="in""#]),
            ("GetStatus", &[r#"type="s" direction="out""#]),
            ("Show", &[]),
            ("Hide", &[]),
            ("ToggleWindow", &[]),
            ("Quit", &[]),
            (
                "Apply",
                &[
                    r#"name="argv" type="as" direction="in""#,
                    r#"name="ok" type="b" direction="out""#,
                    r#"name="stdout" type="s" direction="out""#,
                    r#"name="stderr" type="s" direction="out""#,
                ],
            ),
            ("ListPresets", &[r#"type="s" direction="out""#]),
            ("ListDevices", &[r#"type="s" direction="out""#]),
            (
                "SetAppPreset",
                &[
                    r#"name="app" type="s" direction="in""#,
                    r#"name="direction" type="s" direction="in""#,
                    r#"name="preset" type="s" direction="in""#,
                ],
            ),
            ("ListApps", &[r#"type="s" direction="out""#]),
        ];
        for (member, args) in signatures {
            let method = element(&xml, "method", member);
            assert_eq!(
                method.matches("<arg ").count(),
                args.len(),
                "{member}: {method}"
            );
            for arg in args {
                assert!(method.contains(arg), "{member} has no {arg}: {method}");
            }
        }
        assert_eq!(xml.matches("<method ").count(), 22, "{xml}");
        for call in every_call() {
            element(&xml, "method", call.member());
        }
    }

    #[test]
    fn every_property_is_served_read_only_with_its_type() {
        let xml = introspection();
        for (name, kind) in [
            ("Version", "s"),
            ("Power", "b"),
            ("Preset", "s"),
            ("Output", "s"),
            ("Input", "s"),
            ("Direction", "s"),
        ] {
            let property = element(&xml, "property", name);
            assert!(
                property.contains(&format!(r#"type="{kind}""#)),
                "{property}"
            );
            assert!(property.contains(r#"access="read""#), "{property}");
        }
        let version = element(&xml, "property", "Version");
        assert!(
            version.contains(
                r#"name="org.freedesktop.DBus.Property.EmitsChangedSignal" value="const""#
            ),
            "Version is announced as constant: {version}"
        );
    }

    #[test]
    fn every_signal_is_served_with_its_documented_arguments() {
        let xml = introspection();
        for (member, types) in [
            ("PowerChanged", "b"),
            ("PresetChanged", "ss"),
            ("DeviceChanged", "sss"),
            ("AudioStateChanged", "s"),
            ("Notice", "s"),
            ("AppRouted", "sss"),
        ] {
            let signal = element(&xml, "signal", member);
            let served: String = signal
                .match_indices(r#"type=""#)
                .map(|(at, _)| signal[at + 6..].chars().next().unwrap())
                .collect();
            assert_eq!(served, types, "{member}: {signal}");
        }
    }

    #[test]
    fn every_signal_the_properties_emit_is_one_the_interface_serves() {
        let xml = introspection();
        for signal in [
            Signal::PowerChanged(true),
            Signal::PresetChanged {
                direction: DeviceDirection::Output,
                name: String::new(),
            },
            Signal::DeviceChanged {
                direction: DeviceDirection::Output,
                node_name: String::new(),
                description: String::new(),
            },
            Signal::AudioStateChanged(String::new()),
            Signal::Notice(String::new()),
            Signal::AppRouted {
                app: String::new(),
                direction: DeviceDirection::Input,
                preset: String::new(),
            },
        ] {
            element(&xml, "signal", signal.member());
        }
        for property in [
            Property::Power,
            Property::Preset,
            Property::Output,
            Property::Input,
            Property::Direction,
        ] {
            element(&xml, "property", property.name());
        }
    }

    #[test]
    fn the_manual_page_documents_every_member_the_interface_serves() {
        let page = include_str!("../../../packaging/fxsound.1");
        let section = &page[page.find(".SH D\\-BUS").expect("a D-BUS section")..];
        let section = &section[..section[4..]
            .find(".SH ")
            .map_or(section.len(), |end| end + 4)];
        let xml = introspection();
        for kind in ["method", "property", "signal"] {
            for (at, _) in xml.match_indices(&format!(r#"<{kind} name=""#)) {
                let name = &xml[at + kind.len() + 8..];
                let name = &name[..name.find('"').unwrap()];
                assert!(
                    section.contains(name),
                    "fxsound.1 does not document {kind} {name}"
                );
            }
        }
        for name in [BUS_NAME, DESKTOP_BUS_NAME, OBJECT_PATH] {
            assert!(section.contains(name), "fxsound.1 does not name {name}");
        }
    }

    // ---- the control channel ------------------------------------------------------------------

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime")
    }

    #[test]
    fn a_call_is_answered_with_what_the_gui_thread_said() {
        let (tx, rx) = crossbeam_channel::unbounded::<Forwarded>();
        let gui = thread::spawn(move || {
            let forwarded = rx.recv().expect("a command list");
            assert_eq!(forwarded.commands(), [Command::Status { json: true }]);
            forwarded.respond_with("{}".to_owned(), String::new(), false);
        });
        let response =
            runtime().block_on(Control::from_sender(tx).call(vec![Command::Status { json: true }]));
        gui.join().unwrap();
        assert!(response.ok);
        assert_eq!(response.stdout, "{}");
    }

    #[test]
    fn a_command_list_dropped_unanswered_is_acknowledged() {
        let (tx, rx) = crossbeam_channel::unbounded::<Forwarded>();
        let gui = thread::spawn(move || drop(rx.recv().expect("a command list")));
        let response = runtime().block_on(Control::from_sender(tx).call(vec![Command::Quit]));
        gui.join().unwrap();
        assert!(response.ok);
    }

    #[test]
    fn a_call_with_nobody_taking_commands_is_refused_at_once() {
        let (tx, rx) = crossbeam_channel::unbounded::<Forwarded>();
        drop(rx);
        let started = Instant::now();
        let response = runtime().block_on(Control::from_sender(tx).call(vec![Command::Quit]));
        assert!(!response.ok);
        assert!(
            response.stderr.contains("shutting down"),
            "{}",
            response.stderr
        );
        assert!(started.elapsed() < ipc::HANDLER_TIMEOUT);
    }

    #[test]
    fn the_way_out_refuses_what_was_forwarded_and_not_yet_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener
            .serve_with_status(Arc::new(|| None))
            .expect("serve");
        let control = server.control();
        let caller = thread::spawn(move || runtime().block_on(control.call(vec![Command::Quit])));
        let deadline = Instant::now() + Duration::from_secs(5);
        let response = loop {
            server.refuse_pending();
            if caller.is_finished() {
                break caller.join().unwrap();
            }
            assert!(Instant::now() < deadline, "the call was never refused");
            thread::sleep(Duration::from_millis(5));
        };
        assert!(!response.ok);
        assert!(
            response.stderr.contains("shutting down"),
            "{}",
            response.stderr
        );
    }

    #[test]
    fn past_the_sockets_own_limit_a_call_is_refused_rather_than_queued() {
        let (tx, rx) = crossbeam_channel::unbounded::<Forwarded>();
        let service = Service {
            control: Control::from_sender(tx),
            properties: Arc::default(),
            in_flight: Arc::default(),
        };
        let held: Vec<_> = (0..ipc::MAX_CONNECTIONS)
            .map(|_| service.in_flight.try_enter().expect("a place"))
            .collect();
        let started = Instant::now();
        let response = runtime().block_on(service.call(vec![Command::Status { json: true }]));
        assert!(started.elapsed() < Duration::from_secs(1), "not held");
        assert!(!response.ok);
        assert_eq!(response.stderr, ipc::TOO_MANY_CALLERS);
        assert!(rx.try_recv().is_err(), "nothing was queued to run later");
        assert!(
            matches!(answer(response), Err(MethodError::Refused(text)) if text == ipc::TOO_MANY_CALLERS),
            "a refusal on the bus"
        );

        // A place frees up, and the next call is carried out.
        drop(held);
        let gui = thread::spawn(move || rx.recv().expect("a command").respond("{}"));
        let response = runtime().block_on(service.call(vec![Command::Status { json: true }]));
        gui.join().expect("the GUI thread");
        assert!(response.ok);
        assert_eq!(service.in_flight.count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_call_arriving_once_the_way_out_began_is_refused_at_once() {
        let (tx, rx) = crossbeam_channel::unbounded::<Forwarded>();
        let closing = ipc::Closing::default();
        closing.set();
        let started = Instant::now();
        let response =
            runtime().block_on(Control::from_sender_closing(tx, closing).call(vec![Command::Quit]));
        assert!(!response.ok);
        assert!(
            response.stderr.contains("shutting down"),
            "{}",
            response.stderr
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(rx.try_recv().is_err());
    }

    // ---- hosting ------------------------------------------------------------------------------

    /// A handle whose control channel nobody drains.
    fn idle_control() -> Control {
        let (tx, rx) = crossbeam_channel::unbounded();
        // Kept open for the life of the test process, so a call waits rather than being refused.
        std::mem::forget(rx);
        Control::from_sender(tx)
    }

    #[test]
    fn with_no_bus_to_reach_the_service_stands_down_and_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        let address = format!("unix:path={}", dir.path().join("no-bus-here").display());
        let dbus = DbusHandle::start_on(Bus::Address(address), idle_control(), properties());
        assert_eq!(
            dbus.wait_until_settled(Duration::from_secs(5)),
            ServiceState::Unavailable
        );
        // Publishing still keeps the properties current, and costs nothing on the bus.
        dbus.publish(&AppEvent::Power { on: false });
        assert!(!dbus.properties().power);
        dbus.shutdown();
    }

    #[test]
    fn the_controllers_own_events_keep_the_properties_current_through_the_fan_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let address = format!("unix:path={}", dir.path().join("no-bus-here").display());
        let (mut app, _presets) = app_with_presets("fan-out", &["Alpha", "Beta"]);
        let dbus =
            DbusHandle::start_on(Bus::Address(address), idle_control(), Properties::of(&app));
        let _ = dbus.wait_until_settled(Duration::from_secs(5));

        app.handle(&[
            fxsound_ui::UiAction::TogglePower,
            fxsound_ui::UiAction::SelectPreset(1),
        ]);
        crate::events::fan_out(&mut app, &[&dbus], None);
        let properties = dbus.properties();
        assert!(!properties.power);
        assert_eq!(properties.preset(), "Beta");
        assert_eq!(properties, Properties::of(&app), "nothing left behind");
        dbus.shutdown();
    }

    #[test]
    fn an_address_that_is_not_one_stands_the_service_down_too() {
        let dbus = DbusHandle::start_on(
            Bus::Address("not an address".to_owned()),
            idle_control(),
            Properties::default(),
        );
        assert_eq!(
            dbus.wait_until_settled(Duration::from_secs(5)),
            ServiceState::Unavailable
        );
    }

    #[test]
    fn a_bus_that_never_answers_does_not_hold_up_the_way_out() {
        // A socket that accepts and then says nothing: the connection's handshake waits for ever.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("silent-bus");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let dbus = DbusHandle::start_on(
            Bus::Address(format!("unix:path={}", path.display())),
            idle_control(),
            Properties::default(),
        );
        let (_silent, _) = listener.accept().expect("the service connects");
        assert_eq!(dbus.state(), ServiceState::Starting);
        let started = Instant::now();
        dbus.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    // ---- a private bus: `crate::private_bus` -------------------------------------------------

    /// A preset directory of the test's own with `names` in it.
    fn app_with_presets(tag: &str, names: &[&str]) -> (App, tempfile::TempDir) {
        let root = tempfile::Builder::new()
            .prefix(&format!("fxsound-dbus-{tag}-"))
            .tempdir()
            .expect("temp dir");
        let factory = root.path().join("factory");
        std::fs::create_dir_all(&factory).expect("create the preset directory");
        for name in names {
            let preset = fxsound_core::Preset {
                name: (*name).to_owned(),
                ..fxsound_core::Preset::default()
            };
            fxsound_preset::save(&preset, &factory.join(format!("{name}.fac"))).expect("write");
        }
        let voice = |name: &str| fxsound_preset::input::InputPreset {
            name: name.to_owned(),
            ..fxsound_preset::input::InputPreset::default()
        };
        let mut app = App::headless_for_tests();
        app.use_presets_for_tests(
            vec![factory],
            root.path().join("user"),
            vec![voice("Clean Voice")],
        );
        (app, root)
    }

    /// An instance without a window: a control socket, the D-Bus service on `bus`, and a pump
    /// that runs what arrives the way `Runtime::tick` does, until a `Quit` or `stop`.
    struct Host {
        state: ServiceState,
        stop: Arc<AtomicBool>,
        pump: Option<JoinHandle<()>>,
    }

    impl Host {
        fn start(bus: &PrivateBus, tag: &str) -> Self {
            let address = bus.address.clone();
            let tag = tag.to_owned();
            let stop = Arc::new(AtomicBool::new(false));
            let (settled, state) = crossbeam_channel::bounded(1);
            let pump = {
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let dir = tempfile::tempdir().expect("temp dir");
                    let Instance::Primary(listener) =
                        Instance::acquire_in(dir.path()).expect("acquire")
                    else {
                        panic!("expected to be primary");
                    };
                    let server = listener
                        .serve_with_status(Arc::new(|| None))
                        .expect("serve");
                    let (mut app, _presets) = app_with_presets(&tag, &["Alpha", "Beta"]);
                    let dbus = DbusHandle::start_on(
                        Bus::Address(address),
                        server.control(),
                        Properties::of(&app),
                    );
                    let _ = settled.send(dbus.wait_until_settled(Duration::from_secs(10)));
                    loop {
                        let mut quit = false;
                        for forwarded in server.drain() {
                            let outcome = crate::commands::run(&mut app, forwarded.commands());
                            quit |= outcome.window.quit;
                            forwarded.respond_with(outcome.stdout, outcome.stderr, outcome.failed);
                        }
                        // The runtime's own hand-off, the tray and the watchers left out.
                        crate::events::fan_out(&mut app, &[&dbus], None);
                        if quit || stop.load(Ordering::SeqCst) {
                            server.refuse_pending();
                            dbus.shutdown();
                            return;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                })
            };
            let state = state
                .recv_timeout(Duration::from_secs(15))
                .expect("the service settles");
            Self {
                state,
                stop,
                pump: Some(pump),
            }
        }

        /// Wait for the pump to end on its own — after a `Quit`.
        fn join(mut self) {
            if let Some(pump) = self.pump.take() {
                pump.join().expect("the pump ends cleanly");
            }
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
        }
    }

    fn proxy(client: &zbus::blocking::Connection) -> zbus::blocking::Proxy<'static> {
        zbus::blocking::Proxy::new(client, BUS_NAME, OBJECT_PATH, INTERFACE).expect("a proxy")
    }

    /// Every `member` signal the service emits, on a channel.
    fn signals(
        proxy: &zbus::blocking::Proxy<'static>,
        member: &'static str,
    ) -> crossbeam_channel::Receiver<zbus::Message> {
        let iterator = proxy.receive_signal(member).expect("subscribe");
        let (tx, rx) = crossbeam_channel::unbounded();
        thread::spawn(move || {
            for message in iterator {
                if tx.send(message).is_err() {
                    return;
                }
            }
        });
        rx
    }

    fn method_error(result: zbus::Result<()>) -> (String, String) {
        match result {
            Err(zbus::Error::MethodError(name, detail, _)) => {
                (name.to_string(), detail.unwrap_or_default())
            }
            other => panic!("expected a method error, got {other:?}"),
        }
    }

    /// Poll `property` until it reads `expected`, for at most five seconds.
    fn eventually<T>(proxy: &zbus::blocking::Proxy<'static>, property: &str, expected: &T)
    where
        T: TryFrom<zvariant_owned::OwnedValue> + PartialEq + std::fmt::Debug,
        T::Error: Into<zbus::Error>,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let value: T = proxy.get_property(property).expect("Get");
            if value == *expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{property} stayed {value:?}, expected {expected:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    use zbus::zvariant as zvariant_owned;

    #[test]
    fn over_a_private_bus_every_method_runs_on_the_gui_thread_and_the_signals_arrive() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let host = Host::start(&bus, "methods");
        assert_eq!(host.state, ServiceState::Serving);
        let client = bus.client();
        assert!(bus.has_owner(&client, BUS_NAME));
        assert!(bus.has_owner(&client, DESKTOP_BUS_NAME));
        let proxy = proxy(&client);

        let version: String = proxy.get_property("Version").expect("Version");
        assert_eq!(version, VERSION);

        // Power, answered from the status the toggle left, and announced both ways.
        let power_signals = signals(&proxy, "PowerChanged");
        let initially: bool = proxy.get_property("Power").expect("Power");
        let now: bool = proxy.call("TogglePower", &()).expect("TogglePower");
        assert_eq!(now, !initially);
        let signal = power_signals
            .recv_timeout(Duration::from_secs(5))
            .expect("PowerChanged");
        assert_eq!(signal.body().deserialize::<(bool,)>().unwrap(), (now,));
        eventually(&proxy, "Power", &now);
        let () = proxy.call("SetPower", &(initially,)).expect("SetPower");
        eventually(&proxy, "Power", &initially);

        // A preset by name, read back three ways.
        let preset_signals = signals(&proxy, "PresetChanged");
        let () = proxy.call("SetPreset", &("Beta",)).expect("SetPreset");
        let preset: String = proxy.call("GetPreset", &()).expect("GetPreset");
        assert_eq!(preset, "Beta");
        let signal = preset_signals
            .recv_timeout(Duration::from_secs(5))
            .expect("PresetChanged");
        assert_eq!(
            signal.body().deserialize::<(String, String)>().unwrap(),
            ("output".to_owned(), "Beta".to_owned())
        );
        eventually(&proxy, "Preset", &"Beta".to_owned());
        let () = proxy.call("NextPreset", &()).expect("NextPreset");
        let () = proxy.call("PrevPreset", &()).expect("PrevPreset");
        let preset: String = proxy.call("GetPreset", &()).expect("GetPreset");
        assert_eq!(preset, "Beta");

        // The refusal fxsound would print, and an argument the option would not take.
        let (name, detail) = method_error(proxy.call("SetPreset", &("Nope",)));
        assert_eq!(name, REFUSED);
        assert!(detail.contains("Nope"), "{detail}");
        let (name, detail) = method_error(proxy.call("SetNoiseSuppression", &("loud",)));
        assert_eq!(name, "org.freedesktop.DBus.Error.InvalidArgs");
        assert!(detail.contains("expected preset, off"), "{detail}");

        // The microphone lane.
        let () = proxy
            .call("SetNoiseSuppression", &("strong",))
            .expect("SetNoiseSuppression");
        let () = proxy
            .call("SetEditDirection", &("input",))
            .expect("SetEditDirection");
        eventually(&proxy, "Direction", &"input".to_owned());
        let status: String = proxy.call("GetStatus", &()).expect("GetStatus");
        let status: Value = serde_json::from_str(&status).expect("the --status --json document");
        assert_eq!(status["edit_direction"], "input");
        assert_eq!(status["input"]["noise_suppression"], "strong");
        assert_eq!(status["version"], VERSION);

        // The window is the pump's business; the calls only have to be taken.
        for member in ["Show", "Hide", "ToggleWindow"] {
            let () = proxy
                .call(member, &())
                .unwrap_or_else(|err| panic!("{member}: {err}"));
        }

        // Quit is answered before the connection closes, and the names are given back.
        let () = proxy.call("Quit", &()).expect("Quit is answered");
        host.join();
        assert!(!bus.has_owner(&client, BUS_NAME));
        assert!(!bus.has_owner(&client, DESKTOP_BUS_NAME));
    }

    #[test]
    fn over_a_private_bus_apply_answers_as_the_command_line_and_the_listings_are_json() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let host = Host::start(&bus, "apply");
        assert_eq!(host.state, ServiceState::Serving);
        let client = bus.client();
        let proxy = proxy(&client);
        let apply = |args: &[&str]| -> (bool, String, String) {
            proxy.call("Apply", &(argv(args),)).expect("Apply answers")
        };

        let (ok, stdout, stderr) = apply(&["--preset", "Beta", "--set_effect=bass:7.5"]);
        assert!(ok, "{stderr}");
        assert!(stdout.is_empty() && stderr.is_empty(), "{stdout}{stderr}");
        let preset: String = proxy.call("GetPreset", &()).expect("GetPreset");
        assert_eq!(preset, "Beta");

        let (ok, stdout, _) = apply(&["fxsound", "--status", "--json"]);
        assert!(ok);
        let status: Value = serde_json::from_str(&stdout).expect("the status document");
        assert_eq!(status["effects"]["bass"].as_f64(), Some(7.5));
        assert_eq!(status["selected_preset"], "Beta");

        let (ok, stdout, stderr) = apply(&["--preset", "Nope"]);
        assert!(!ok);
        assert!(stdout.is_empty());
        assert!(stderr.contains("Nope"), "{stderr}");
        let (ok, _, stderr) = apply(&["--no-such-option"]);
        assert!(!ok);
        assert!(stderr.contains("--no-such-option"), "{stderr}");

        let presets: String = proxy.call("ListPresets", &()).expect("ListPresets");
        let presets: Value = serde_json::from_str(&presets).expect("JSON");
        assert_eq!(presets["selected_preset"], "Beta");
        assert_eq!(presets["output_presets"]["built_in"][1]["name"], "Beta");
        assert_eq!(
            presets["input_presets"]["built_in"][0]["name"],
            "Clean Voice"
        );
        let devices: String = proxy.call("ListDevices", &()).expect("ListDevices");
        let devices: Value = serde_json::from_str(&devices).expect("JSON");
        assert!(devices["output_device_list"].is_array(), "{devices}");
        assert!(devices["input_device_list"].is_array(), "{devices}");
        assert_eq!(devices["schema"], crate::commands::STATUS_SCHEMA);

        let (ok, _, stderr) = apply(&["--quit"]);
        assert!(ok, "{stderr}");
        host.join();
    }

    #[test]
    fn over_a_private_bus_an_application_is_given_a_preset_and_listed() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let host = Host::start(&bus, "apps");
        assert_eq!(host.state, ServiceState::Serving);
        let client = bus.client();
        let proxy = proxy(&client);
        let list = || -> Value {
            let listing: String = proxy.call("ListApps", &()).expect("ListApps");
            serde_json::from_str(&listing).expect("JSON")
        };
        assert_eq!(
            list(),
            serde_json::json!({"schema": crate::commands::STATUS_SCHEMA, "apps": []})
        );

        let () = proxy
            .call("SetAppPreset", &("bf6.exe", "output", "Beta"))
            .expect("SetAppPreset");
        let () = proxy
            .call("SetAppPreset", &("BF6.EXE", "input", "Clean Voice"))
            .expect("the same application, by its program in another case");
        let listing = list();
        assert_eq!(
            listing["apps"].as_array().map(Vec::len),
            Some(1),
            "{listing}"
        );
        let game = &listing["apps"][0];
        assert_eq!(game["binary"], "bf6.exe");
        assert_eq!(game["output_preset"], "Beta");
        assert_eq!(game["input_preset"], "Clean Voice");
        assert_eq!(game["running"], false);
        let status: String = proxy.call("GetStatus", &()).expect("GetStatus");
        let status: Value = serde_json::from_str(&status).expect("the status document");
        assert_eq!(status["apps"], listing["apps"]);

        // The refusal the command line prints; arguments the options would not take either.
        let (name, detail) =
            method_error(proxy.call("SetAppPreset", &("bf6.exe", "output", "Nope")));
        assert_eq!(name, REFUSED);
        assert_eq!(detail, "no output preset is called \"Nope\"");
        let (name, _) = method_error(proxy.call("SetAppPreset", &("bf6.exe", "sideways", "Beta")));
        assert_eq!(name, "org.freedesktop.DBus.Error.InvalidArgs");
        let (name, _) = method_error(proxy.call("SetAppPreset", &("", "output", "Beta")));
        assert_eq!(name, "org.freedesktop.DBus.Error.InvalidArgs");

        // An empty preset follows the lane again; the other lane keeps its own.
        let () = proxy
            .call("SetAppPreset", &("bf6.exe", "output", ""))
            .expect("SetAppPreset");
        let game = &list()["apps"][0];
        assert_eq!(game["output_preset"], Value::Null);
        assert_eq!(game["input_preset"], "Clean Voice");

        // The command line through Apply, with its note and its lines.
        let (ok, stdout, stderr): (bool, String, String) = proxy
            .call("Apply", &(argv(&["--app-preset=Brave=Alpha"]),))
            .expect("Apply");
        assert!(ok);
        assert!(stdout.is_empty());
        assert!(stderr.starts_with("note: FxSound has not seen"), "{stderr}");
        let (ok, stdout, _): (bool, String, String) = proxy
            .call("Apply", &(argv(&["--list-apps"]),))
            .expect("Apply");
        assert!(ok);
        assert_eq!(stdout.lines().count(), 2, "{stdout}");
        assert!(stdout.contains("binary=Brave"), "{stdout}");
        drop(host);
    }

    #[test]
    fn over_a_private_bus_an_application_moved_onto_a_route_and_back_is_signalled() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let client = bus.client();
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener
            .serve_with_status(Arc::new(|| None))
            .expect("serve");
        let dbus = DbusHandle::start_on(
            Bus::Address(bus.address.clone()),
            server.control(),
            properties(),
        );
        assert_eq!(
            dbus.wait_until_settled(Duration::from_secs(10)),
            ServiceState::Serving
        );
        let proxy = proxy(&client);
        let routed = signals(&proxy, "AppRouted");
        let game = fxsound_core::AppKey {
            binary: "bf6.exe".to_owned(),
            name: "Battlefield 6".to_owned(),
            flatpak: String::new(),
        };
        dbus.publish(&AppEvent::AppRouted {
            app: game.clone(),
            direction: DeviceDirection::Output,
            preset: Some("Gaming".to_owned()),
        });
        dbus.publish(&AppEvent::AppRouted {
            app: game,
            direction: DeviceDirection::Output,
            preset: None,
        });
        for preset in ["Gaming", ""] {
            let signal = routed
                .recv_timeout(Duration::from_secs(5))
                .expect("AppRouted");
            assert_eq!(
                signal
                    .body()
                    .deserialize::<(String, String, String)>()
                    .unwrap(),
                (
                    "Battlefield 6".to_owned(),
                    "output".to_owned(),
                    preset.to_owned()
                )
            );
        }
        dbus.shutdown();
        drop(server);
    }

    #[test]
    fn over_a_private_bus_a_second_instance_finds_the_name_taken_and_stands_down() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let first = Host::start(&bus, "first");
        assert_eq!(first.state, ServiceState::Serving);
        let second = Host::start(&bus, "second");
        assert_eq!(second.state, ServiceState::Unavailable);
        // The first one is still the one answering.
        let client = bus.client();
        let status: String = proxy(&client).call("GetStatus", &()).expect("GetStatus");
        assert!(status.contains("\"version\""), "{status}");
        drop(second);
        drop(first);
        assert!(!bus.has_owner(&client, BUS_NAME));
    }

    #[test]
    fn over_a_private_bus_a_notice_and_an_audio_state_are_signalled() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let client = bus.client();
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener
            .serve_with_status(Arc::new(|| None))
            .expect("serve");
        let dbus = DbusHandle::start_on(
            Bus::Address(bus.address.clone()),
            server.control(),
            properties(),
        );
        assert_eq!(
            dbus.wait_until_settled(Duration::from_secs(10)),
            ServiceState::Serving
        );
        let proxy = proxy(&client);
        let notices = signals(&proxy, "Notice");
        let audio = signals(&proxy, "AudioStateChanged");
        let devices = signals(&proxy, "DeviceChanged");

        dbus.publish(&AppEvent::Notice {
            message: "Preset saved".to_owned(),
        });
        dbus.publish(&AppEvent::AudioState {
            direction: DeviceDirection::Input,
            state: LaneState::Idle,
            sample_rate: 48_000,
            channels: 1,
        });
        dbus.publish(&AppEvent::DeviceChanged {
            direction: DeviceDirection::Input,
            node_name: Some("alsa_input.usb".to_owned()),
            description: Some("Headset".to_owned()),
        });

        let notice = notices
            .recv_timeout(Duration::from_secs(5))
            .expect("Notice");
        assert_eq!(
            notice.body().deserialize::<(String,)>().unwrap().0,
            "Preset saved"
        );
        let state = audio
            .recv_timeout(Duration::from_secs(5))
            .expect("AudioStateChanged");
        let json: Value =
            serde_json::from_str(&state.body().deserialize::<(String,)>().unwrap().0).unwrap();
        assert_eq!(json["event"], "audio_state");
        assert_eq!(json["direction"], "input");
        assert_eq!(json["state"], "idle");
        let device = devices
            .recv_timeout(Duration::from_secs(5))
            .expect("DeviceChanged");
        assert_eq!(
            device
                .body()
                .deserialize::<(String, String, String)>()
                .unwrap(),
            (
                "input".to_owned(),
                "alsa_input.usb".to_owned(),
                "Headset".to_owned()
            )
        );
        eventually(&proxy, "Input", &"Headset".to_owned());
        dbus.shutdown();
        drop(server);
    }
}
