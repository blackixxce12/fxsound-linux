//! What `fxsound --watch` prints: one line per thing that changed in the running instance
//! (0.4.0 design §10).
//!
//! Two spellings of the same event. JSON, one object per line, with a fixed envelope in front of
//! the event's own fields:
//!
//! ```text
//! {"v":1,"event":"preset_changed","ts":1790000000000,"direction":"output","name":"Rock","modified":false}
//! ```
//!
//! and the plain form, for `grep` and `read` in a shell loop:
//!
//! ```text
//! preset_changed direction=output name=Rock modified=false
//! ```
//!
//! In the plain form a value is written bare when it can be, quoted as a JSON string when it has
//! a space, a quote, a backslash or a control character in it (`name="Bass Booster"`), and left
//! empty for `null` (`node_name=` is a detached lane). The `status` event nests the whole
//! `--status --json` document under `status` in JSON, and spreads it over dotted keys in the plain
//! form (`output.device=… input.preset=…`).
//!
//! The controller is the one source of events. [`App`] queues one at every mutation that changes
//! something the stream reports — the power, a lane's preset or device, the device list, the edit
//! direction, a lane's audio state, a notice, echo cancellation, a calibration, an application
//! moved onto a route of its own or off it — and says nothing
//! for a mutation that leaves it as the stream last described it: [`Published`] is its record of
//! that. Each tick [`fan_out`] drains the queue once and hands the same events to every
//! consumer: the `--watch` streams, the D-Bus signals, and the tray, which is redrawn only when
//! something it draws changed.

use std::fmt::Write as _;

use fxsound_core::settings::CalibrationRecord;
use fxsound_core::{AppKey, AudioDevice, AudioStatus, DeviceDirection, WindowsParity};
use serde_json::Value;

use crate::App;
use crate::commands::{InputMeters, rounded};
use crate::tray::TrayState;

/// The envelope's `v`. Bump when an event changes shape incompatibly; adding an event or a field
/// is not that.
pub const EVENT_VERSION: u32 = 1;

/// One thing that happened in the running instance.
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    /// The whole `--status --json` document. Sent once, first, to every new subscriber.
    Status(Value),
    /// The power button.
    Power { on: bool },
    /// A lane's preset: another one picked, or the same one gaining or losing unsaved changes.
    /// `name` is `None` while the lane has no preset at all.
    PresetChanged {
        direction: DeviceDirection,
        name: Option<String>,
        modified: bool,
    },
    /// A lane's device. Both names are `None` while the lane is detached.
    DeviceChanged {
        direction: DeviceDirection,
        node_name: Option<String>,
        description: Option<String>,
    },
    /// The list of devices PipeWire offers changed; `count` is its new length.
    DevicesChanged { count: usize },
    /// A lane started or stopped processing, or renegotiated its format.
    AudioState {
        direction: DeviceDirection,
        state: LaneState,
        sample_rate: u32,
        channels: u16,
    },
    /// The lane the window edits.
    Direction { direction: DeviceDirection },
    /// The microphone's telemetry. Only to subscribers that asked for `--meters`, at most four
    /// times a second each.
    InputMeters(InputMeters),
    /// A notice the window shows in its bubble.
    Notice { message: String },
    /// Echo cancellation: whether Settings asks for it, whether the audio thread has it running,
    /// and the audio thread's reason when it does not (`None` when it has nothing to say).
    EchoCancel {
        on: bool,
        running: bool,
        detail: Option<String>,
    },
    /// The calibration wizard's result was applied: the voice preset it wrote, the microphone it
    /// measured and what it measured there.
    Calibrated(CalibrationRecord),
    /// An application's streams of one lane moved onto the route of a preset of its own, or back
    /// onto the lane (`preset` is `None`) — as the engine reports it, so a rule whose route could
    /// not be made says nothing (`docs/0.4.0-apps.md`). An application that stops playing while
    /// routed is back on nothing, and says so the same way.
    AppRouted {
        app: AppKey,
        direction: DeviceDirection,
        preset: Option<String>,
    },
    /// «Как в Windows» / "Like FxSound for Windows" moved to another level.
    WindowsParity { level: WindowsParity },
    /// The window was opened or hidden to the tray.
    Window { visible: bool },
    /// The instance is quitting; the stream ends right after this.
    Quit,
}

/// What a lane's audio is doing, as `audio_state` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneState {
    /// Buffers are flowing through the chain.
    Processing,
    /// Attached, but nothing is playing into it (or nothing is being captured).
    Idle,
    /// There is no audio engine at all.
    #[default]
    Unavailable,
}

impl LaneState {
    /// The spelling on the wire.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Processing => "processing",
            Self::Idle => "idle",
            Self::Unavailable => "unavailable",
        }
    }
}

impl AppEvent {
    /// The event's name: the `event` of the JSON envelope, the first word of a plain line.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::Power { .. } => "power",
            Self::PresetChanged { .. } => "preset_changed",
            Self::DeviceChanged { .. } => "device_changed",
            Self::DevicesChanged { .. } => "devices_changed",
            Self::AudioState { .. } => "audio_state",
            Self::Direction { .. } => "direction",
            Self::InputMeters(_) => "input_meters",
            Self::Notice { .. } => "notice",
            Self::EchoCancel { .. } => "echo_cancel",
            Self::Calibrated(_) => "calibrated",
            Self::AppRouted { .. } => "app_routed",
            Self::WindowsParity { .. } => "windows_parity",
            Self::Window { .. } => "window",
            Self::Quit => "quit",
        }
    }

    /// Whether the event changes something the tray draws: its icon's power, its preset list and
    /// its device list with their ticks. Not `audio_state`: the icon's processing is the shown
    /// lane's non-silent buffers, like the window's logo, and a lane that runs on silence is still
    /// `processing` there.
    ///
    /// What the tray draws that no event names — the theme, the language, a preset list that grew
    /// or shrank under an unchanged selection, sound starting or stopping — the controller flags
    /// on its own ([`App::take_tray_refresh`]).
    #[must_use]
    pub const fn touches_tray(&self) -> bool {
        matches!(
            self,
            Self::Power { .. }
                | Self::PresetChanged { .. }
                | Self::DeviceChanged { .. }
                | Self::DevicesChanged { .. }
                | Self::Direction { .. }
        )
    }

    /// One JSON object, without the trailing newline. `ts_ms` is the Unix time in milliseconds.
    ///
    /// Written by hand rather than through a `serde_json::Map`, which sorts its keys: the envelope
    /// comes first so that a reader can dispatch on `event` without parsing the rest.
    #[must_use]
    pub fn to_json(&self, ts_ms: u64) -> String {
        let mut out = format!(
            r#"{{"v":{EVENT_VERSION},"event":"{}","ts":{ts_ms}"#,
            self.name()
        );
        for (key, value) in self.fields() {
            // `Value`'s `Display` is compact JSON; the keys are fixed identifiers.
            let _ = write!(out, r#","{key}":{value}"#);
        }
        out.push('}');
        out
    }

    /// `event key=value …`, without the trailing newline.
    #[must_use]
    pub fn to_plain(&self) -> String {
        let mut out = self.name().to_owned();
        match self {
            // The document's own keys, not `status.power=…` on every one of them.
            Self::Status(Value::Object(document)) => {
                for (key, value) in document {
                    push_plain(&mut out, key, value);
                }
            }
            _ => {
                for (key, value) in self.fields() {
                    push_plain(&mut out, key, &value);
                }
            }
        }
        out
    }

    /// The event's own fields, in the order they are documented.
    fn fields(&self) -> Vec<(&'static str, Value)> {
        let direction = |direction: &DeviceDirection| Value::from(direction.key());
        match self {
            Self::Status(document) => vec![("status", document.clone())],
            Self::Power { on } => vec![("on", Value::from(*on))],
            Self::PresetChanged {
                direction: lane,
                name,
                modified,
            } => vec![
                ("direction", direction(lane)),
                ("name", Value::from(name.clone())),
                ("modified", Value::from(*modified)),
            ],
            Self::DeviceChanged {
                direction: lane,
                node_name,
                description,
            } => vec![
                ("direction", direction(lane)),
                ("node_name", Value::from(node_name.clone())),
                ("description", Value::from(description.clone())),
            ],
            Self::DevicesChanged { count } => vec![("count", Value::from(*count))],
            Self::AudioState {
                direction: lane,
                state,
                sample_rate,
                channels,
            } => vec![
                ("direction", direction(lane)),
                ("state", Value::from(state.key())),
                ("sample_rate", Value::from(*sample_rate)),
                ("channels", Value::from(*channels)),
            ],
            Self::Direction { direction: lane } => vec![("direction", direction(lane))],
            Self::InputMeters(meters) => {
                // Destructured in full, so a new meter has to be given a place here.
                let InputMeters {
                    voice_probability,
                    noise_floor_db,
                    denoise_reduction_db,
                    gate_reduction_db,
                    compressor_reduction_db,
                    deesser_reduction_db,
                    denoise_running,
                    deesser_running,
                } = *meters;
                vec![
                    ("voice_probability", Value::from(voice_probability)),
                    ("noise_floor_db", Value::from(noise_floor_db)),
                    ("denoise_reduction_db", Value::from(denoise_reduction_db)),
                    ("gate_reduction_db", Value::from(gate_reduction_db)),
                    (
                        "compressor_reduction_db",
                        Value::from(compressor_reduction_db),
                    ),
                    ("deesser_reduction_db", Value::from(deesser_reduction_db)),
                    ("denoise_running", Value::from(denoise_running)),
                    ("deesser_running", Value::from(deesser_running)),
                ]
            }
            Self::Notice { message } => vec![("message", Value::from(message.as_str()))],
            Self::EchoCancel {
                on,
                running,
                detail,
            } => vec![
                ("on", Value::from(*on)),
                ("running", Value::from(*running)),
                ("detail", Value::from(detail.clone())),
            ],
            Self::Calibrated(record) => {
                // Destructured in full, so a new measurement has to be given a place here. The
                // time is the envelope's `ts`.
                let CalibrationRecord {
                    noise_floor_db,
                    speech_rms_db,
                    speech_peak_db,
                    clipped_ratio,
                    unix_time: _,
                    preset,
                    device,
                } = record;
                vec![
                    ("device", Value::from(device.as_str())),
                    ("preset", Value::from(preset.as_str())),
                    ("noise_floor_db", Value::from(rounded(*noise_floor_db, 1))),
                    ("speech_rms_db", Value::from(rounded(*speech_rms_db, 1))),
                    ("speech_peak_db", Value::from(rounded(*speech_peak_db, 1))),
                    ("clipped_ratio", Value::from(rounded(*clipped_ratio, 4))),
                ]
            }
            Self::AppRouted {
                app,
                direction: lane,
                preset,
            } => vec![
                // The name the window shows, then what `--app-preset` can match it by.
                ("app", Value::from(app.display())),
                ("binary", Value::from(app.binary.as_str())),
                ("flatpak", Value::from(app.flatpak.as_str())),
                ("direction", direction(lane)),
                ("preset", Value::from(preset.clone())),
            ],
            Self::WindowsParity { level } => vec![("level", Value::from(level.key()))],
            Self::Window { visible } => vec![("visible", Value::from(*visible))],
            Self::Quit => Vec::new(),
        }
    }
}

/// Append ` key=value` — or one such pair per leaf, with dotted keys, for an object. `--list-apps`
/// writes its lines with it too.
pub(crate) fn push_plain(out: &mut String, key: &str, value: &Value) {
    match value {
        Value::Object(map) => {
            for (inner, value) in map {
                push_plain(out, &format!("{key}.{inner}"), value);
            }
        }
        Value::Null => {
            let _ = write!(out, " {key}=");
        }
        Value::String(text) => {
            let _ = write!(out, " {key}=");
            push_plain_text(out, text);
        }
        // Booleans and numbers have no spaces; an array is written as compact JSON, quoted if a
        // string inside it has a space.
        other => {
            let _ = write!(out, " {key}=");
            push_plain_text(out, &other.to_string());
        }
    }
}

/// `text` bare when a shell `read` would take it back in one piece, quoted as a JSON string
/// otherwise — which is also how an empty string is told apart from `null`.
fn push_plain_text(out: &mut String, text: &str) {
    let bare = !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\');
    if bare {
        out.push_str(text);
    } else {
        out.push_str(&Value::from(text).to_string());
    }
}

// =============================================================================================
// The controller's record of what it has said
// =============================================================================================

/// What the event stream has last said about each thing it reports, so that the controller says
/// a change once, at the mutation that makes it, and says nothing for a mutation that changes
/// nothing on the stream's terms.
///
/// Each method takes the thing as it is now and answers with the event that says so, or `None`
/// when the stream already says exactly that; either way the record then holds the new value. The
/// comparisons run on borrowed values, and a name is only copied when it changed. Notices,
/// calibrations, the window and the quit are not in it: each of those is news every time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Published {
    power: bool,
    direction: DeviceDirection,
    /// Per lane, indexed by [`lane`]: the preset and whether it has unsaved changes.
    presets: [Option<(String, bool)>; 2],
    /// Per lane: `node.name` and description of the device, `None` while detached.
    devices: [Option<(String, String)>; 2],
    /// `node.name` and description of every device on offer, sorted: which devices, not their
    /// order.
    listed: Vec<(String, String)>,
    /// Per lane: state, rate and channel count.
    audio: [(LaneState, u32, u16); 2],
    /// Asked for, running, and the audio thread's reason.
    echo_cancel: (bool, bool, String),
    /// The level of «Как в Windows».
    windows_parity: WindowsParity,
}

/// A lane's slot in the per-lane arrays.
const fn lane(direction: DeviceDirection) -> usize {
    match direction {
        DeviceDirection::Output => 0,
        DeviceDirection::Input => 1,
    }
}

impl Published {
    /// What the controller looks like now: the record a queue starts from, so that nothing it
    /// already is gets said.
    #[must_use]
    pub fn of(app: &App) -> Self {
        let mut record = Self::default();
        let _ = record.catch_up(app);
        record
    }

    /// Every change in `app` that the record has not been told about, as the events that would
    /// say it — empty whenever every mutation has said what it changed. The tests' oracle: a
    /// mutation point that forgot to say something shows up here.
    #[doc(hidden)]
    #[must_use]
    pub fn unsaid(&self, app: &App) -> Vec<AppEvent> {
        self.clone().catch_up(app)
    }

    /// Take everything the stream reports from `app`, and answer with what differed.
    fn catch_up(&mut self, app: &App) -> Vec<AppEvent> {
        let state = &app.state;
        let mut said = Vec::new();
        said.extend(self.power(state.power));
        said.extend(self.direction(state.direction));
        said.extend(self.devices(&state.devices));
        for direction in DeviceDirection::ALL {
            said.extend(self.device(direction, state.device_for(direction)));
            said.extend(self.preset(direction, app.lane_preset(direction)));
            said.extend(self.audio(
                direction,
                app.lane_state(direction),
                app.audio_status_for(direction),
            ));
        }
        said.extend(self.echo_cancel(
            state.echo_cancel_on,
            state.echo_cancel_running,
            app.echo_cancel_detail(),
        ));
        said.extend(self.windows_parity(app.windows_parity()));
        said
    }

    /// The level of «Как в Windows».
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn windows_parity(&mut self, level: WindowsParity) -> Option<AppEvent> {
        (self.windows_parity != level).then(|| {
            self.windows_parity = level;
            AppEvent::WindowsParity { level }
        })
    }

    /// The power button.
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn power(&mut self, on: bool) -> Option<AppEvent> {
        (self.power != on).then(|| {
            self.power = on;
            AppEvent::Power { on }
        })
    }

    /// The edit direction.
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn direction(&mut self, direction: DeviceDirection) -> Option<AppEvent> {
        (self.direction != direction).then(|| {
            self.direction = direction;
            AppEvent::Direction { direction }
        })
    }

    /// A lane's preset — its name and whether it has unsaved changes — or none.
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn preset(
        &mut self,
        direction: DeviceDirection,
        preset: Option<(&str, bool)>,
    ) -> Option<AppEvent> {
        let slot = &mut self.presets[lane(direction)];
        if slot
            .as_ref()
            .map(|(name, modified)| (name.as_str(), *modified))
            == preset
        {
            return None;
        }
        *slot = preset.map(|(name, modified)| (name.to_owned(), modified));
        Some(AppEvent::PresetChanged {
            direction,
            name: preset.map(|(name, _)| name.to_owned()),
            modified: preset.is_some_and(|(_, modified)| modified),
        })
    }

    /// A lane's device, or none while it is detached. A new description for the same node is a
    /// change too: it is what the bars and the tray show.
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn device(
        &mut self,
        direction: DeviceDirection,
        device: Option<&AudioDevice>,
    ) -> Option<AppEvent> {
        let slot = &mut self.devices[lane(direction)];
        let now = device.map(|d| (d.name.as_str(), d.description.as_str()));
        if slot
            .as_ref()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            == now
        {
            return None;
        }
        *slot = device.map(|d| (d.name.clone(), d.description.clone()));
        Some(AppEvent::DeviceChanged {
            direction,
            node_name: device.map(|d| d.name.clone()),
            description: device.map(|d| d.description.clone()),
        })
    }

    /// The devices on offer: a device that came, went, or was renamed changes the list, whatever
    /// the count does. Their order does not: the list is kept in the order of the device priority
    /// list (U4), and moving a row in Settings offers no device that was not on offer before.
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn devices(&mut self, listed: &[AudioDevice]) -> Option<AppEvent> {
        let mut now: Vec<(String, String)> = listed
            .iter()
            .map(|d| (d.name.clone(), d.description.clone()))
            .collect();
        now.sort_unstable();
        if now == self.listed {
            return None;
        }
        self.listed = now;
        Some(AppEvent::DevicesChanged {
            count: listed.len(),
        })
    }

    /// A lane's state and format. Each lane has its own: a 16 kHz headset microphone beside
    /// 48 kHz speakers reports two different rates. The status's counters are not news.
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn audio(
        &mut self,
        direction: DeviceDirection,
        state: LaneState,
        status: &AudioStatus,
    ) -> Option<AppEvent> {
        let now = (state, status.sample_rate, status.channels);
        let slot = &mut self.audio[lane(direction)];
        (*slot != now).then(|| {
            *slot = now;
            AppEvent::AudioState {
                direction,
                state,
                sample_rate: status.sample_rate,
                channels: status.channels,
            }
        })
    }

    /// Echo cancellation: asked for, running, and why not.
    #[must_use = "the event is what tells the stream"]
    pub(crate) fn echo_cancel(
        &mut self,
        on: bool,
        running: bool,
        detail: &str,
    ) -> Option<AppEvent> {
        let (was_on, was_running, was_detail) = &self.echo_cancel;
        if (*was_on, *was_running, was_detail.as_str()) == (on, running, detail) {
            return None;
        }
        self.echo_cancel = (on, running, detail.to_owned());
        Some(AppEvent::EchoCancel {
            on,
            running,
            detail: (!detail.is_empty()).then(|| detail.to_owned()),
        })
    }
}

// =============================================================================================
// Where the events go
// =============================================================================================

/// A consumer of the controller's events: the `--watch` streams ([`crate::ipc::Server`]) and the
/// D-Bus service ([`crate::dbus::DbusHandle`]). Publishing must not block for long: it runs on the
/// GUI thread.
pub trait EventSink {
    /// Take one event.
    fn publish(&self, event: &AppEvent);

    /// Whether this consumer has a subscriber that asked for `input_meters`. The meters are
    /// gathered only when one did; holding each subscriber to four a second is the consumer's own
    /// business, since only it knows when each one last had some.
    fn wants_meters(&self) -> bool {
        false
    }
}

/// The tray, which draws a mirror of the model ([`App::tray_state`]).
pub trait TraySink {
    /// Replace what the tray draws.
    fn redraw(&self, state: TrayState);
}

/// Each tick's hand-off: drain the controller's queue once, give every event to every sink in
/// the order it happened, redraw the tray when something it draws changed, and gather the
/// microphone's meters for whoever asked for them.
pub fn fan_out(app: &mut App, sinks: &[&dyn EventSink], tray: Option<&dyn TraySink>) {
    let events = app.drain_events();
    for event in &events {
        for sink in sinks {
            sink.publish(event);
        }
    }

    // Taken whether or not there is a tray, so that a flag raised while there is none is not
    // left standing.
    let redraw = app.take_tray_refresh() | events.iter().any(AppEvent::touches_tray);
    if redraw && let Some(tray) = tray {
        tray.redraw(app.tray_state());
    }

    // Not a mutation, so not in the queue: the meters move on every buffer, and are only worth
    // reading for a subscriber that asked.
    if sinks.iter().any(|sink| sink.wants_meters()) {
        let meters = AppEvent::InputMeters(crate::commands::input_meters(&app.state));
        for sink in sinks.iter().filter(|sink| sink.wants_meters()) {
            sink.publish(&meters);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).expect("every event is one JSON object")
    }

    fn device(name: &str, description: &str, direction: DeviceDirection) -> AudioDevice {
        AudioDevice {
            id: 0,
            name: name.to_owned(),
            description: description.to_owned(),
            is_default: false,
            direction,
            form_factor: String::new(),
        }
    }

    #[test]
    fn the_envelope_comes_first_and_in_the_documented_order() {
        let line = AppEvent::Power { on: true }.to_json(1_790_000_000_123);
        assert_eq!(
            line,
            r#"{"v":1,"event":"power","ts":1790000000123,"on":true}"#
        );
    }

    #[test]
    fn a_preset_change_names_its_lane_and_whether_it_has_unsaved_changes() {
        let event = AppEvent::PresetChanged {
            direction: DeviceDirection::Input,
            name: Some("Podcast Voice".to_owned()),
            modified: true,
        };
        let json = parse(&event.to_json(7));
        assert_eq!(json["event"], "preset_changed");
        assert_eq!(json["direction"], "input");
        assert_eq!(json["name"], "Podcast Voice");
        assert_eq!(json["modified"], true);
        assert_eq!(
            event.to_plain(),
            r#"preset_changed direction=input name="Podcast Voice" modified=true"#
        );
    }

    #[test]
    fn a_detached_lane_is_null_in_json_and_empty_in_the_plain_form() {
        let event = AppEvent::DeviceChanged {
            direction: DeviceDirection::Output,
            node_name: None,
            description: None,
        };
        let json = parse(&event.to_json(0));
        assert!(json["node_name"].is_null());
        assert!(json["description"].is_null());
        assert_eq!(
            event.to_plain(),
            "device_changed direction=output node_name= description="
        );
    }

    #[test]
    fn an_empty_string_is_quoted_so_it_differs_from_null() {
        let event = AppEvent::Notice {
            message: String::new(),
        };
        assert_eq!(event.to_plain(), r#"notice message="""#);
    }

    #[test]
    fn quotes_and_backslashes_in_a_plain_value_are_escaped_as_json() {
        let event = AppEvent::Notice {
            message: r#"Preset "A\B" saved"#.to_owned(),
        };
        assert_eq!(
            event.to_plain(),
            r#"notice message="Preset \"A\\B\" saved""#
        );
        let json = parse(&event.to_json(0));
        assert_eq!(json["message"], r#"Preset "A\B" saved"#);
    }

    #[test]
    fn a_newline_in_a_value_never_splits_the_event_over_two_lines() {
        let event = AppEvent::Notice {
            message: "two\nlines".to_owned(),
        };
        assert!(!event.to_plain().contains('\n'));
        assert!(!event.to_json(0).contains('\n'));
    }

    #[test]
    fn the_status_event_nests_the_document_in_json_and_spreads_it_in_plain() {
        let document = json!({
            "power": true,
            "preset": "Bass Booster",
            "output": { "device": "Built-in Audio", "node_name": null },
        });
        let event = AppEvent::Status(document.clone());
        let json = parse(&event.to_json(42));
        assert_eq!(json["event"], "status");
        assert_eq!(json["ts"], 42);
        assert_eq!(json["status"], document);

        let plain = event.to_plain();
        assert!(plain.starts_with("status "), "{plain}");
        assert!(plain.contains(" power=true"), "{plain}");
        assert!(plain.contains(r#" preset="Bass Booster""#), "{plain}");
        assert!(
            plain.contains(r#" output.device="Built-in Audio""#),
            "{plain}"
        );
        assert!(plain.contains(" output.node_name="), "{plain}");
    }

    #[test]
    fn the_real_status_document_makes_one_plain_line_and_one_json_object() {
        let mut app = App::headless_for_tests();
        app.state.devices = vec![device(
            "alsa_output.pci",
            "Built-in Audio Analog Stereo",
            DeviceDirection::Output,
        )];
        app.state.selected_output = Some(0);
        // What the default status source parses: `fxsound --status --json`'s own text.
        let document: Value =
            serde_json::from_str(&crate::commands::status_json(&app)).expect("status json");
        let event = AppEvent::Status(document);

        let plain = event.to_plain();
        assert!(!plain.contains('\n'));
        assert!(
            plain.contains(r#" output.device="Built-in Audio Analog Stereo""#),
            "{plain}"
        );
        assert!(
            plain.contains(" output.node_name=alsa_output.pci"),
            "{plain}"
        );
        assert!(plain.contains(" input.node_name= "), "{plain}");

        let json = parse(&event.to_json(1));
        assert_eq!(json["status"]["output"]["node_name"], "alsa_output.pci");
        assert_eq!(json["status"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn an_application_routed_names_itself_as_the_window_does_and_by_what_a_rule_can_match() {
        let routed = AppEvent::AppRouted {
            app: AppKey {
                binary: "bf6.exe".to_owned(),
                name: "Battlefield 6".to_owned(),
                flatpak: String::new(),
            },
            direction: DeviceDirection::Output,
            preset: Some("Gaming".to_owned()),
        };
        assert_eq!(
            routed.to_json(7),
            r#"{"v":1,"event":"app_routed","ts":7,"app":"Battlefield 6","binary":"bf6.exe","flatpak":"","direction":"output","preset":"Gaming"}"#
        );
        assert_eq!(
            routed.to_plain(),
            r#"app_routed app="Battlefield 6" binary=bf6.exe flatpak="" direction=output preset=Gaming"#
        );

        // Back on its lane: the preset is null, as a detached lane's device is.
        let back = AppEvent::AppRouted {
            app: AppKey {
                binary: String::new(),
                name: String::new(),
                flatpak: "com.discordapp.Discord".to_owned(),
            },
            direction: DeviceDirection::Input,
            preset: None,
        };
        let json = parse(&back.to_json(0));
        assert_eq!(json["app"], "com.discordapp.Discord");
        assert_eq!(json["direction"], "input");
        assert_eq!(json["preset"], Value::Null);
        assert!(back.to_plain().ends_with(" direction=input preset="));
    }

    #[test]
    fn every_event_has_a_snake_case_name_and_quit_has_no_fields() {
        assert_eq!(AppEvent::Quit.to_plain(), "quit");
        assert_eq!(
            AppEvent::Quit.to_json(5),
            r#"{"v":1,"event":"quit","ts":5}"#
        );
        assert_eq!(
            AppEvent::Window { visible: false }.to_plain(),
            "window visible=false"
        );
        assert_eq!(
            AppEvent::DevicesChanged { count: 4 }.to_plain(),
            "devices_changed count=4"
        );
        assert_eq!(
            AppEvent::Direction {
                direction: DeviceDirection::Input
            }
            .to_plain(),
            "direction direction=input"
        );
    }

    #[test]
    fn an_audio_state_carries_the_lane_the_state_and_the_format() {
        let event = AppEvent::AudioState {
            direction: DeviceDirection::Output,
            state: LaneState::Processing,
            sample_rate: 48_000,
            channels: 2,
        };
        assert_eq!(
            event.to_plain(),
            "audio_state direction=output state=processing sample_rate=48000 channels=2"
        );
        let json = parse(&event.to_json(0));
        assert_eq!(json["state"], "processing");
        assert_eq!(json["sample_rate"], 48_000);
    }

    #[test]
    fn the_meters_event_reports_every_meter_and_a_missing_floor_as_null() {
        let event = AppEvent::InputMeters(InputMeters {
            voice_probability: 0.93,
            noise_floor_db: None,
            denoise_reduction_db: 18.2,
            gate_reduction_db: 0.0,
            compressor_reduction_db: 3.5,
            deesser_reduction_db: 1.0,
            denoise_running: true,
            deesser_running: false,
        });
        let json = parse(&event.to_json(0));
        assert_eq!(json["event"], "input_meters");
        assert_eq!(json["voice_probability"].as_f64(), Some(0.93));
        assert!(json["noise_floor_db"].is_null());
        assert_eq!(json["denoise_running"], true);
        assert!(
            event
                .to_plain()
                .starts_with("input_meters voice_probability=0.93 noise_floor_db= "),
            "{}",
            event.to_plain()
        );
    }

    #[test]
    fn echo_cancellation_says_whether_it_was_asked_for_whether_it_runs_and_why_not() {
        let event = AppEvent::EchoCancel {
            on: true,
            running: false,
            detail: Some("the WebRTC module is not installed".to_owned()),
        };
        assert_eq!(
            event.to_plain(),
            r#"echo_cancel on=true running=false detail="the WebRTC module is not installed""#
        );
        let running = AppEvent::EchoCancel {
            on: true,
            running: true,
            detail: None,
        };
        let json = parse(&running.to_json(0));
        assert_eq!(json["event"], "echo_cancel");
        assert_eq!(json["running"], true);
        assert!(json["detail"].is_null(), "nothing to explain while it runs");
    }

    #[test]
    fn a_calibration_names_the_microphone_and_the_preset_and_rounds_what_it_measured() {
        let event = AppEvent::Calibrated(CalibrationRecord {
            noise_floor_db: -52.34,
            speech_rms_db: -21.06,
            speech_peak_db: -6.5,
            clipped_ratio: 0.000_12,
            unix_time: 1_790_000_000,
            preset: "Calibrated — Blue Yeti".to_owned(),
            device: "alsa_input.usb-Blue_Yeti".to_owned(),
        });
        let json = parse(&event.to_json(0));
        assert_eq!(json["event"], "calibrated");
        assert_eq!(json["device"], "alsa_input.usb-Blue_Yeti");
        assert_eq!(json["preset"], "Calibrated — Blue Yeti");
        assert_eq!(json["noise_floor_db"].as_f64(), Some(-52.3));
        assert_eq!(json["speech_rms_db"].as_f64(), Some(-21.1));
        assert_eq!(json["clipped_ratio"].as_f64(), Some(0.0001));
        assert!(
            json.get("unix_time").is_none(),
            "the envelope's ts says when"
        );
        assert!(
            event.to_plain().starts_with(
                r#"calibrated device=alsa_input.usb-Blue_Yeti preset="Calibrated — Blue Yeti" "#
            ),
            "{}",
            event.to_plain()
        );
    }

    #[test]
    fn only_what_the_tray_draws_touches_it() {
        let lane = DeviceDirection::Output;
        for event in [
            AppEvent::Power { on: true },
            AppEvent::PresetChanged {
                direction: lane,
                name: None,
                modified: false,
            },
            AppEvent::DeviceChanged {
                direction: lane,
                node_name: None,
                description: None,
            },
            AppEvent::DevicesChanged { count: 0 },
            AppEvent::Direction { direction: lane },
        ] {
            assert!(event.touches_tray(), "{event:?}");
        }
        for event in [
            AppEvent::Status(Value::Null),
            // The icon's processing is the meters' `active`, flagged by the controller itself.
            AppEvent::AudioState {
                direction: lane,
                state: LaneState::Processing,
                sample_rate: 48_000,
                channels: 2,
            },
            AppEvent::Notice {
                message: String::new(),
            },
            AppEvent::EchoCancel {
                on: false,
                running: false,
                detail: None,
            },
            AppEvent::Calibrated(CalibrationRecord::default()),
            AppEvent::AppRouted {
                app: AppKey::default(),
                direction: lane,
                preset: None,
            },
            AppEvent::WindowsParity {
                level: WindowsParity::Full,
            },
            AppEvent::Window { visible: true },
            AppEvent::Quit,
        ] {
            assert!(!event.touches_tray(), "{event:?}");
        }
    }

    #[test]
    fn a_new_windows_parity_level_is_the_windows_parity_event_with_its_level() {
        let event = AppEvent::WindowsParity {
            level: WindowsParity::Sound,
        };
        assert_eq!(event.to_plain(), "windows_parity level=sound");
        assert_eq!(
            event.to_json(7),
            r#"{"v":1,"event":"windows_parity","ts":7,"level":"sound"}"#
        );
        let mut record = Published::default();
        assert_eq!(record.windows_parity(WindowsParity::Off), None);
        assert_eq!(
            record.windows_parity(WindowsParity::Full),
            Some(AppEvent::WindowsParity {
                level: WindowsParity::Full
            })
        );
        assert_eq!(record.windows_parity(WindowsParity::Full), None);
    }

    // ---- the controller's queue ------------------------------------------------------------

    use crate::app::voice_store_for_tests;
    use crate::audio_link::FakeEngine;
    use crate::tray::TrayState;
    use fxsound_core::messages::{AudioToUi, Meters};
    use fxsound_core::{Preset, Settings};
    use fxsound_preset::PresetStore;
    use fxsound_preset::input::InputPreset;
    use fxsound_ui::UiAction;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;
    const SPEAKERS: &str = "alsa_output.speakers";
    const HEADPHONES: &str = "alsa_output.headphones";
    const MIC: &str = "alsa_input.mic";

    /// A controller started the way the real one starts, against a fake engine: `Alpha` and
    /// `Beta` for the speakers, `Loud` and `Quiet` for the microphone, both lanes on.
    fn started() -> (App, FakeEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("scratch directory");
        let factory = dir.path().join("factory");
        std::fs::create_dir_all(&factory).expect("factory directory");
        for name in ["Alpha", "Beta"] {
            let preset = Preset {
                name: name.to_owned(),
                ..Preset::default()
            };
            fxsound_preset::save(&preset, &factory.join(format!("{name}.fac"))).expect("write");
        }
        let mut music = PresetStore::with_dirs(vec![factory], dir.path().join("user"));
        music.rescan();
        let voices = voice_store_for_tests(
            &[
                InputPreset {
                    name: "Loud".to_owned(),
                    makeup_db: 9.0,
                    ..InputPreset::default()
                },
                InputPreset {
                    name: "Quiet".to_owned(),
                    ..InputPreset::default()
                },
            ],
            &dir.path().join("voice-factory"),
            dir.path().join("user").join("Input"),
        );
        let mut settings = Settings::default();
        settings.output_preset = "Alpha".to_owned();
        settings.input_preset = "Quiet".to_owned();
        settings.set_lane_enabled(IN, true);
        let engine = FakeEngine::new();
        let app = App::start_for_tests(settings, music, voices, &engine);
        (app, engine, dir)
    }

    fn listed() -> Vec<AudioDevice> {
        vec![
            device(SPEAKERS, "Speakers", OUT),
            device(HEADPHONES, "Headphones", OUT),
            device(MIC, "Microphone", IN),
        ]
    }

    fn attached(direction: DeviceDirection, node_name: &str) -> AudioToUi {
        AudioToUi::Attached {
            direction,
            node_name: Some(node_name.to_owned()),
        }
    }

    fn status(direction: DeviceDirection, processing: bool, rate: u32) -> AudioToUi {
        AudioToUi::Status {
            direction,
            status: fxsound_core::AudioStatus {
                processing,
                sample_rate: rate,
                channels: 2,
                ..fxsound_core::AudioStatus::default()
            },
        }
    }

    /// What the controller said since the last look — after checking it left nothing unsaid.
    fn said(app: &mut App) -> Vec<AppEvent> {
        assert_eq!(
            app.unsaid_changes(),
            [],
            "a mutation changed something and did not say so"
        );
        app.drain_events()
    }

    /// Feed the engine's words to the controller the way a frame does.
    fn hear(app: &mut App, engine: &FakeEngine, messages: impl IntoIterator<Item = AudioToUi>) {
        for message in messages {
            engine.feed(message);
        }
        app.poll_audio();
    }

    fn preset_changed(direction: DeviceDirection, name: &str, modified: bool) -> AppEvent {
        AppEvent::PresetChanged {
            direction,
            name: Some(name.to_owned()),
            modified,
        }
    }

    fn device_changed(direction: DeviceDirection, node: Option<(&str, &str)>) -> AppEvent {
        AppEvent::DeviceChanged {
            direction,
            node_name: node.map(|(name, _)| name.to_owned()),
            description: node.map(|(_, text)| text.to_owned()),
        }
    }

    #[test]
    fn a_controller_that_has_just_started_has_nothing_to_say() {
        let (mut app, _engine, _dir) = started();
        assert_eq!(said(&mut app), []);
        assert!(
            !app.take_tray_refresh(),
            "the tray is built from the same state"
        );

        let mut headless = App::headless_for_tests();
        assert_eq!(said(&mut headless), []);
    }

    #[test]
    fn nothing_changing_says_nothing() {
        let (mut app, engine, _dir) = started();
        hear(&mut app, &engine, [AudioToUi::Devices(listed())]);
        let _ = app.drain_events();

        app.handle(&[]);
        app.poll_audio();
        hear(&mut app, &engine, [AudioToUi::Devices(listed())]);
        assert_eq!(said(&mut app), [], "the same list again is not news");
        app.handle(&[UiAction::SetEditDirection(OUT)]);
        assert_eq!(said(&mut app), [], "the lane the window already edits");
    }

    #[test]
    fn the_power_is_said_once_each_way() {
        let (mut app, _engine, _dir) = started();
        app.handle(&[UiAction::TogglePower]);
        assert_eq!(said(&mut app), [AppEvent::Power { on: false }]);
        app.handle_tray(crate::tray::TrayCommand::SetPower(false));
        assert_eq!(said(&mut app), [], "off already");
        app.handle(&[UiAction::TogglePower]);
        assert_eq!(said(&mut app), [AppEvent::Power { on: true }]);
    }

    #[test]
    fn the_first_unsaved_change_to_a_preset_is_said_and_the_next_ones_are_not() {
        let (mut app, _engine, _dir) = started();
        app.handle(&[UiAction::SetEffect(fxsound_core::Effect::Bass, 4.0)]);
        assert_eq!(said(&mut app), [preset_changed(OUT, "Alpha", true)]);
        app.handle(&[UiAction::SetEffect(fxsound_core::Effect::Bass, 5.0)]);
        assert_eq!(said(&mut app), [], "still Alpha, still modified");

        app.handle(&[UiAction::UndoPresetChanges]);
        assert_eq!(said(&mut app), [preset_changed(OUT, "Alpha", false)]);
    }

    #[test]
    fn a_preset_picked_is_said_on_its_own_lane() {
        let (mut app, _engine, _dir) = started();
        app.cycle_preset(true);
        assert_eq!(said(&mut app), [preset_changed(OUT, "Beta", false)]);

        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert_eq!(said(&mut app), [AppEvent::Direction { direction: IN }]);
        app.cycle_preset(false);
        assert_eq!(said(&mut app), [preset_changed(IN, "Loud", false)]);
    }

    /// `argv` run against the controller as the command line, the socket and D-Bus run it.
    fn command_line(app: &mut App, argv: &[&str]) -> crate::commands::Outcome {
        use crate::cli::Cli;
        use clap::Parser as _;
        let cli = Cli::try_parse_from(std::iter::once("fxsound").chain(argv.iter().copied()))
            .expect("a valid command line");
        crate::commands::run(app, &cli.commands())
    }

    fn presets_said(events: &[AppEvent]) -> Vec<&AppEvent> {
        events
            .iter()
            .filter(|event| matches!(event, AppEvent::PresetChanged { .. }))
            .collect()
    }

    #[test]
    fn a_rename_from_the_command_line_says_the_new_name_once_on_either_lane() {
        for lane in DeviceDirection::ALL {
            let (mut app, _engine, _dir) = started();
            // `Other` sits where `Mine` was once it is gone, and so is what a rename made of a
            // save, a delete and a select would pass through on its way to `Ours`.
            app.handle(&[
                UiAction::SetEditDirection(lane),
                UiAction::SavePresetAs("Other".to_owned()),
                UiAction::SavePresetAs("Mine".to_owned()),
            ]);
            let _ = app.drain_events();

            let outcome = command_line(&mut app, &["--rename_preset", "Ours"]);
            assert!(!outcome.failed, "{lane:?}: {}", outcome.stderr);
            let events = said(&mut app);
            assert_eq!(
                presets_said(&events),
                [&preset_changed(lane, "Ours", false)],
                "{lane:?}: {events:?}"
            );
            assert!(app.lane_has_preset(lane, "Ours"), "{lane:?}");
            assert!(!app.lane_has_preset(lane, "Mine"), "{lane:?}");
            assert_eq!(app.lane_preset(lane), Some(("Ours", false)), "{lane:?}");
        }
    }

    #[test]
    fn a_rename_the_command_line_cannot_do_says_no_preset_changed() {
        let (mut app, _engine, _dir) = started();
        // A factory preset stays where it is, and the command says why, as the menu would by
        // offering no Rename at all.
        let outcome = command_line(&mut app, &["--rename_preset", "Ours"]);
        assert!(outcome.failed);
        assert!(outcome.stderr.contains("factory"), "{}", outcome.stderr);
        assert_eq!(said(&mut app), []);
        assert!(!app.lane_has_preset(OUT, "Ours"));

        // Unsaved edits would stay behind under a name that is gone, so the command refuses.
        app.handle(&[UiAction::SavePresetAs("Mine".to_owned())]);
        app.handle(&[UiAction::SetEffect(fxsound_core::Effect::Bass, 4.0)]);
        let _ = app.drain_events();
        let outcome = command_line(&mut app, &["--rename_preset", "Ours"]);
        assert!(outcome.failed);
        assert!(outcome.stderr.contains("unsaved"), "{}", outcome.stderr);
        assert_eq!(said(&mut app), []);
        assert_eq!(app.lane_preset(OUT), Some(("Mine", true)));
        assert!(!app.lane_has_preset(OUT, "Ours"));
    }

    #[test]
    fn a_device_list_says_the_list_and_each_lane_the_engine_attached() {
        let (mut app, engine, _dir) = started();
        hear(
            &mut app,
            &engine,
            [
                AudioToUi::Devices(listed()),
                attached(OUT, SPEAKERS),
                attached(IN, MIC),
            ],
        );
        assert_eq!(
            said(&mut app),
            [
                AppEvent::DevicesChanged { count: 3 },
                device_changed(OUT, Some((SPEAKERS, "Speakers"))),
                device_changed(IN, Some((MIC, "Microphone"))),
            ]
        );

        // A device renamed under the same node is a new list, and a new name for the lane.
        let mut renamed = listed();
        renamed[0].description = "Laptop Speakers".to_owned();
        hear(&mut app, &engine, [AudioToUi::Devices(renamed.clone())]);
        assert_eq!(
            said(&mut app),
            [
                AppEvent::DevicesChanged { count: 3 },
                device_changed(OUT, Some((SPEAKERS, "Laptop Speakers"))),
            ]
        );

        // Another node in an unused one's place, in one list: the count is the same and neither
        // lane moved, but the list is a new one.
        let mut swapped = renamed;
        swapped[1] = device("bluez_output.buds", "Earbuds", OUT);
        hear(&mut app, &engine, [AudioToUi::Devices(swapped)]);
        assert_eq!(said(&mut app), [AppEvent::DevicesChanged { count: 3 }]);
    }

    #[test]
    fn picking_a_microphone_says_its_lane_moved_and_the_window_followed_it() {
        let (mut app, engine, _dir) = started();
        hear(
            &mut app,
            &engine,
            [AudioToUi::Devices(listed()), attached(OUT, SPEAKERS)],
        );
        let _ = app.drain_events();

        app.handle(&[UiAction::SelectInput(2)]);
        assert_eq!(
            said(&mut app),
            [
                device_changed(IN, Some((MIC, "Microphone"))),
                AppEvent::Direction { direction: IN },
            ]
        );
        hear(&mut app, &engine, [attached(IN, MIC)]);
        assert_eq!(said(&mut app), [], "the engine only confirmed the pick");
    }

    #[test]
    fn switching_a_lane_off_says_it_is_detached_and_idle() {
        let (mut app, engine, _dir) = started();
        hear(
            &mut app,
            &engine,
            [
                AudioToUi::Devices(listed()),
                attached(OUT, SPEAKERS),
                status(OUT, true, 48_000),
            ],
        );
        let events = said(&mut app);
        assert!(
            events.contains(&AppEvent::AudioState {
                direction: OUT,
                state: LaneState::Processing,
                sample_rate: 48_000,
                channels: 2,
            }),
            "{events:?}"
        );

        app.handle(&[UiAction::DetachOutput]);
        assert_eq!(
            said(&mut app),
            [
                device_changed(OUT, None),
                AppEvent::AudioState {
                    direction: OUT,
                    state: LaneState::Idle,
                    sample_rate: 48_000,
                    channels: 2,
                },
            ]
        );
    }

    #[test]
    fn a_status_that_only_moves_the_counters_says_nothing() {
        let (mut app, engine, _dir) = started();
        hear(&mut app, &engine, [status(OUT, true, 48_000)]);
        assert_eq!(said(&mut app).len(), 1);
        let mut later = fxsound_core::AudioStatus {
            processing: true,
            sample_rate: 48_000,
            channels: 2,
            ..fxsound_core::AudioStatus::default()
        };
        later.processed_secs = 60;
        later.underrun_frames = 12;
        hear(
            &mut app,
            &engine,
            [AudioToUi::Status {
                direction: OUT,
                status: later,
            }],
        );
        assert_eq!(said(&mut app), []);
    }

    #[test]
    fn a_lane_without_an_engine_is_unavailable_from_the_start_and_says_so_with_its_format() {
        let mut app = App::headless_for_tests();
        assert_eq!(app.lane_state(OUT), LaneState::Unavailable);
        assert_eq!(app.lane_state(IN), LaneState::Unavailable);

        // A status that reaches a controller with no engine moves the format, never the state.
        app.receive(AudioToUi::Status {
            direction: IN,
            status: fxsound_core::AudioStatus {
                processing: true,
                sample_rate: 16_000,
                channels: 1,
                ..fxsound_core::AudioStatus::default()
            },
        });
        assert_eq!(
            said(&mut app),
            [AppEvent::AudioState {
                direction: IN,
                state: LaneState::Unavailable,
                sample_rate: 16_000,
                channels: 1,
            }],
            "the speakers' lane kept its own format, so it has nothing to report"
        );
        assert_eq!(app.lane_state(IN), LaneState::Unavailable);
    }

    #[test]
    fn each_lane_reports_its_own_format() {
        let (mut app, engine, _dir) = started();
        // A 16 kHz headset microphone beside speakers that have not said anything new.
        hear(&mut app, &engine, [status(IN, false, 16_000)]);
        assert_eq!(
            said(&mut app),
            [AppEvent::AudioState {
                direction: IN,
                state: LaneState::Idle,
                sample_rate: 16_000,
                channels: 2,
            }]
        );
    }

    #[test]
    fn a_lost_connection_says_both_lanes_stopped_and_that_the_audio_is_disconnected() {
        let (mut app, engine, _dir) = started();
        hear(
            &mut app,
            &engine,
            [status(OUT, true, 48_000), status(IN, true, 48_000)],
        );
        let _ = app.drain_events();
        hear(
            &mut app,
            &engine,
            [AudioToUi::Disconnected {
                reason: "PipeWire went away".to_owned(),
            }],
        );
        let events = said(&mut app);
        let stopped: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::AudioState {
                    direction, state, ..
                } => Some((*direction, *state)),
                _ => None,
            })
            .collect();
        assert_eq!(stopped, [(OUT, LaneState::Idle), (IN, LaneState::Idle)]);
        // The notice the window shows, in the user's language; the engine's English reason is
        // a log line, as an engine error's text is.
        assert_eq!(
            events.last(),
            Some(&AppEvent::Notice {
                message: "Audio disconnected".to_owned()
            }),
            "{events:?}"
        );
    }

    #[test]
    fn every_notice_is_said_the_same_text_twice_included() {
        let (mut app, _engine, _dir) = started();
        app.handle(&[UiAction::SelectPreset(0), UiAction::DeletePreset]);
        app.handle(&[UiAction::DeletePreset]);
        let refused = AppEvent::Notice {
            message: "Factory presets cannot be deleted".to_owned(),
        };
        assert_eq!(said(&mut app), [refused.clone(), refused]);

        app.handle(&[UiAction::DismissNotice]);
        assert_eq!(said(&mut app), [], "a notice going away is not a notice");
    }

    #[test]
    fn echo_cancellation_is_said_when_asked_for_and_again_when_the_audio_thread_answers() {
        let (mut app, engine, _dir) = started();
        let mut pane = app.settings_state();
        app.handle_settings(
            &fxsound_ui::dialogs::settings::SettingsAction::SetEchoCancel(true),
            &mut pane,
        );
        assert_eq!(
            said(&mut app),
            [AppEvent::EchoCancel {
                on: true,
                running: false,
                detail: None,
            }]
        );
        hear(
            &mut app,
            &engine,
            [AudioToUi::EchoCancel {
                running: false,
                detail: "no WebRTC module".to_owned(),
            }],
        );
        assert_eq!(
            said(&mut app),
            [AppEvent::EchoCancel {
                on: true,
                running: false,
                detail: Some("no WebRTC module".to_owned()),
            }]
        );
        hear(
            &mut app,
            &engine,
            [AudioToUi::EchoCancel {
                running: false,
                detail: "no WebRTC module".to_owned(),
            }],
        );
        assert_eq!(said(&mut app), [], "the same answer again");
    }

    #[test]
    fn an_applied_calibration_is_kept_and_said_and_a_broken_one_is_neither() {
        let (mut app, _engine, _dir) = started();
        let record = CalibrationRecord {
            noise_floor_db: -55.0,
            speech_rms_db: -20.0,
            speech_peak_db: -8.0,
            clipped_ratio: 0.0,
            unix_time: 1_790_000_000,
            preset: "Calibrated — Microphone".to_owned(),
            device: MIC.to_owned(),
        };
        app.record_calibration(record.clone());
        assert_eq!(said(&mut app), [AppEvent::Calibrated(record.clone())]);
        assert_eq!(app.settings().calibration.as_ref(), Some(&record));

        app.record_calibration(CalibrationRecord {
            noise_floor_db: f32::NAN,
            ..record.clone()
        });
        assert_eq!(said(&mut app), []);
        assert_eq!(app.settings().calibration.as_ref(), Some(&record));
    }

    /// A key an event reports on: which thing, on which lane.
    fn key(event: &AppEvent) -> Option<(&'static str, Option<DeviceDirection>)> {
        match event {
            AppEvent::PresetChanged { direction, .. }
            | AppEvent::DeviceChanged { direction, .. }
            | AppEvent::AudioState { direction, .. } => Some((event.name(), Some(*direction))),
            AppEvent::Power { .. }
            | AppEvent::Direction { .. }
            | AppEvent::DevicesChanged { .. }
            | AppEvent::EchoCancel { .. } => Some((event.name(), None)),
            _ => None,
        }
    }

    /// One mutation of a session, against the controller and its fake engine.
    type Step = Box<dyn Fn(&mut App, &FakeEngine)>;

    /// Everything a long session does, one mutation at a time.
    fn session_steps() -> Vec<(&'static str, Step)> {
        use crate::cli::Cli;
        use clap::Parser as _;
        use fxsound_core::Effect;
        let run = |line: &'static str| -> Step {
            Box::new(move |app, _| {
                let argv = std::iter::once("fxsound").chain(line.split(' '));
                let cli = Cli::try_parse_from(argv).expect("a valid command line");
                let _ = crate::commands::run(app, &cli.commands());
            })
        };
        let act = |actions: Vec<UiAction>| -> Step { Box::new(move |app, _| app.handle(&actions)) };
        let hear_all = |messages: Vec<AudioToUi>| -> Step {
            Box::new(move |app, engine| hear(app, engine, messages.clone()))
        };
        let settings = |action: fxsound_ui::dialogs::settings::SettingsAction| -> Step {
            Box::new(move |app, _| {
                let mut pane = app.settings_state();
                app.handle_settings(&action, &mut pane);
            })
        };
        vec![
            (
                "the list",
                hear_all(vec![AudioToUi::Devices(listed()), attached(OUT, SPEAKERS)]),
            ),
            ("processing", hear_all(vec![status(OUT, true, 48_000)])),
            ("power", act(vec![UiAction::TogglePower])),
            ("power back", act(vec![UiAction::TogglePower])),
            (
                "an effect",
                act(vec![UiAction::SetEffect(Effect::Bass, 3.0)]),
            ),
            ("a band", act(vec![UiAction::SetBandGain(0, 4.0)])),
            (
                "save as",
                act(vec![UiAction::SavePresetAs("Mine".to_owned())]),
            ),
            ("edit it", act(vec![UiAction::SetEqEnabled(false)])),
            ("overwrite", act(vec![UiAction::SavePreset])),
            (
                "rename",
                Box::new(|app: &mut App, _: &FakeEngine| app.rename_preset("Ours")),
            ),
            ("rename by command", run("--rename_preset Yours")),
            ("bands", act(vec![UiAction::SetBandCount(5)])),
            ("defaults", act(vec![UiAction::RestoreDefaults])),
            ("undo", act(vec![UiAction::UndoPresetChanges])),
            ("delete", act(vec![UiAction::DeletePreset])),
            ("next preset", run("--next-preset")),
            ("headphones", act(vec![UiAction::SelectOutput(1)])),
            (
                "attached",
                hear_all(vec![attached(OUT, HEADPHONES), status(OUT, false, 44_100)]),
            ),
            ("the microphone", run("--input alsa_input.mic")),
            (
                "its answer",
                hear_all(vec![attached(IN, MIC), status(IN, true, 16_000)]),
            ),
            ("a voice", act(vec![UiAction::SelectPreset(0)])),
            ("a voice edit", act(vec![UiAction::SetMasterGain(6.0)])),
            ("back to music", run("--edit output")),
            (
                "reset presets",
                Box::new(|app: &mut App, _: &FakeEngine| {
                    let mut pane = app.settings_state();
                    app.handle_settings(
                        &fxsound_ui::dialogs::settings::SettingsAction::ResetPresets,
                        &mut pane,
                    );
                }),
            ),
            ("microphone off", run("--input off")),
            (
                "its answer",
                hear_all(vec![AudioToUi::Attached {
                    direction: IN,
                    node_name: None,
                }]),
            ),
            (
                "unplugged",
                hear_all(vec![AudioToUi::Devices(listed()[..1].to_vec())]),
            ),
            (
                "an error",
                hear_all(vec![AudioToUi::Error {
                    direction: Some(OUT),
                    message: "could not attach".to_owned(),
                }]),
            ),
            (
                "gone",
                hear_all(vec![AudioToUi::Disconnected {
                    reason: "restart".to_owned(),
                }]),
            ),
            ("power by name", run("--power off")),
            ("power on again", run("--power on")),
            (
                "new devices first",
                settings(
                    fxsound_ui::dialogs::settings::SettingsAction::SetPrioritizeNewOutput(true),
                ),
            ),
            (
                "a dock, ranked first and taken",
                hear_all(vec![AudioToUi::Devices({
                    let mut devices = listed();
                    devices.push(device("alsa_output.dock", "Dock", OUT));
                    devices
                })]),
            ),
            (
                "its answer",
                hear_all(vec![attached(OUT, "alsa_output.dock")]),
            ),
            (
                "ranked down",
                settings(fxsound_ui::dialogs::settings::SettingsAction::MoveDeviceDown(0)),
            ),
            (
                "the system decides",
                settings(
                    fxsound_ui::dialogs::settings::SettingsAction::SetFollowSystemDefault(true),
                ),
            ),
            (
                "asleep",
                Box::new(|app: &mut App, _: &FakeEngine| app.system_sleeping(true)),
            ),
            (
                "awake",
                Box::new(|app: &mut App, _: &FakeEngine| app.system_sleeping(false)),
            ),
            (
                "off, on the system's default",
                act(vec![UiAction::TogglePower]),
            ),
        ]
    }

    #[test]
    fn a_long_session_leaves_nothing_unsaid_and_never_says_the_same_thing_twice() {
        let (mut app, engine, _dir) = started();
        let mut last: HashMap<(&'static str, Option<DeviceDirection>), AppEvent> = HashMap::new();
        let mut total = 0;
        for (step, act) in session_steps() {
            act(&mut app, &engine);
            let unsaid = app.unsaid_changes();
            assert!(
                unsaid.is_empty(),
                "{step}: changed without saying so: {unsaid:?}"
            );
            // One step is one tick's worth: the stream hears where each thing ended up, not the
            // places it passed through on the way.
            let mut keys = Vec::new();
            for event in app.drain_events() {
                if let Some(key) = key(&event) {
                    assert!(
                        !keys.contains(&key),
                        "{step}: said {key:?} twice in one step, the second time {event:?}"
                    );
                    keys.push(key);
                    assert_ne!(
                        last.get(&key),
                        Some(&event),
                        "{step}: said again what the stream already says"
                    );
                    last.insert(key, event);
                }
                total += 1;
            }
        }
        assert!(total > 20, "the session said only {total} things");
    }

    /// Every sink records what it was handed; the tray what it was told to draw.
    #[derive(Default)]
    struct Recorder {
        events: RefCell<Vec<AppEvent>>,
        meters: bool,
        redraws: RefCell<Vec<TrayState>>,
    }

    impl EventSink for Recorder {
        fn publish(&self, event: &AppEvent) {
            self.events.borrow_mut().push(event.clone());
        }

        fn wants_meters(&self) -> bool {
            self.meters
        }
    }

    impl TraySink for Recorder {
        fn redraw(&self, state: TrayState) {
            self.redraws.borrow_mut().push(state);
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<AppEvent> {
            self.events.take()
        }

        fn redrawn(&self) -> usize {
            self.redraws.take().len()
        }
    }

    #[test]
    fn a_tick_hands_every_sink_the_same_events_in_order_once() {
        let (mut app, engine, _dir) = started();
        let (watch, bus) = (Recorder::default(), Recorder::default());
        hear(&mut app, &engine, [AudioToUi::Devices(listed())]);
        app.handle(&[UiAction::TogglePower, UiAction::SetEditDirection(IN)]);
        fan_out(&mut app, &[&watch, &bus], None);

        let handed = watch.take();
        assert_eq!(handed, bus.take());
        assert_eq!(
            handed
                .iter()
                .map(AppEvent::name)
                .filter(|name| *name != "device_changed")
                .collect::<Vec<_>>(),
            ["devices_changed", "power", "direction"]
        );
        fan_out(&mut app, &[&watch, &bus], None);
        assert_eq!(watch.take(), [], "drained once");
    }

    #[test]
    fn an_application_the_engine_moves_reaches_every_consumer_as_app_routed_once() {
        let (mut app, engine, _dir) = started();
        let (watch, bus) = (Recorder::default(), Recorder::default());
        let game = AppKey {
            binary: "bf6.exe".to_owned(),
            name: "Battlefield 6".to_owned(),
            flatpak: String::new(),
        };
        let playing = |route: Option<&str>| {
            AudioToUi::AppStreams(vec![fxsound_core::messages::AppStream {
                id: 7,
                direction: OUT,
                app: game.clone(),
                route: route.map(str::to_owned),
            }])
        };
        hear(&mut app, &engine, [playing(None), playing(Some("Beta"))]);
        fan_out(&mut app, &[&watch, &bus], None);
        let handed = watch.take();
        assert_eq!(handed, bus.take());
        let routed: Vec<String> = handed
            .iter()
            .filter(|event| event.name() == "app_routed")
            .map(|event| event.to_json(1))
            .collect();
        assert_eq!(
            routed,
            [
                r#"{"v":1,"event":"app_routed","ts":1,"app":"Battlefield 6","binary":"bf6.exe","flatpak":"","direction":"output","preset":"Beta"}"#
            ]
        );

        // The same report again says nothing; the way back says so once.
        hear(&mut app, &engine, [playing(Some("Beta")), playing(None)]);
        fan_out(&mut app, &[&watch, &bus], None);
        let back: Vec<String> = watch
            .take()
            .iter()
            .filter(|event| event.name() == "app_routed")
            .map(AppEvent::to_plain)
            .collect();
        assert_eq!(
            back,
            [
                r#"app_routed app="Battlefield 6" binary=bf6.exe flatpak="" direction=output preset="#
            ]
        );
    }

    #[test]
    fn meters_are_gathered_only_for_a_sink_that_asked() {
        let (mut app, _engine, _dir) = started();
        let plain = Recorder::default();
        let metered = Recorder {
            meters: true,
            ..Recorder::default()
        };
        fan_out(&mut app, &[&plain, &metered], None);
        assert_eq!(plain.take(), []);
        assert!(matches!(metered.take()[..], [AppEvent::InputMeters(_)]));

        fan_out(&mut app, &[&plain], None);
        assert_eq!(plain.take(), [], "nobody asked, so nothing was gathered");
    }

    /// The tray as it would draw, without the language: another test may switch the global one.
    fn drawn(app: &App) -> String {
        let mut state = app.tray_state();
        state.language.clear();
        format!("{state:?}")
    }

    #[test]
    fn the_tray_is_redrawn_for_every_change_it_draws_and_for_nothing_else() {
        let (mut app, engine, _dir) = started();
        let tray = Recorder::default();
        for (step, act) in session_steps() {
            let before = drawn(&app);
            act(&mut app, &engine);
            fan_out(&mut app, &[], Some(&tray));
            let redrawn = tray.redrawn();
            if drawn(&app) != before {
                assert_eq!(redrawn, 1, "{step}: the tray kept showing the old state");
            }
            fan_out(&mut app, &[], Some(&tray));
            assert_eq!(tray.redrawn(), 0, "{step}: a quiet tick redrew the tray");
        }

        for (step, act) in [
            (
                "the theme",
                Box::new(|app: &mut App| app.handle(&[UiAction::ToggleTheme]))
                    as Box<dyn Fn(&mut App)>,
            ),
            (
                "the effect sliders",
                Box::new(|app: &mut App| {
                    app.handle(&[UiAction::SetEffect(fxsound_core::Effect::Bass, 7.0)]);
                }),
            ),
        ] {
            let before = drawn(&app);
            act(&mut app);
            fan_out(&mut app, &[], Some(&tray));
            assert_eq!(tray.redrawn(), usize::from(drawn(&app) != before), "{step}");
        }
    }

    #[test]
    fn with_no_tray_the_flag_is_still_taken() {
        let (mut app, _engine, _dir) = started();
        app.handle(&[UiAction::ToggleTheme]);
        fan_out(&mut app, &[], None);
        assert!(!app.take_tray_refresh());
    }

    /// Meters with sound in them, or silence.
    fn meters(active: bool) -> Meters {
        Meters {
            active,
            sample_rate: 48_000,
            ..Meters::default()
        }
    }

    #[test]
    fn the_tray_is_redrawn_once_when_sound_starts_or_stops_and_not_while_it_goes_on() {
        let (mut app, engine, _dir) = started();
        let tray = Recorder::default();
        // The speakers' lane is running, on silence to begin with.
        hear(
            &mut app,
            &engine,
            [
                AudioToUi::Devices(listed()),
                attached(OUT, SPEAKERS),
                status(OUT, true, 48_000),
            ],
        );
        fan_out(&mut app, &[], Some(&tray));
        let _ = tray.redraws.take();
        for (step, lane, sound, redrawn_with) in [
            ("sound starts", OUT, true, Some(true)),
            ("sound goes on", OUT, true, None),
            ("sound goes on, a second tick", OUT, true, None),
            ("the microphone starts, off screen", IN, true, None),
            ("sound stops", OUT, false, Some(false)),
            ("silence goes on", OUT, false, None),
        ] {
            engine.set_meters(lane, meters(sound));
            app.poll_audio();
            fan_out(&mut app, &[], Some(&tray));
            let processing: Vec<bool> = tray.redraws.take().iter().map(|s| s.processing).collect();
            assert_eq!(processing, Vec::from_iter(redrawn_with), "{step}");
        }
    }

    #[test]
    fn a_lane_running_on_silence_leaves_the_tray_idle_with_the_logo_and_the_status() {
        let (mut app, engine, _dir) = started();
        let tray = Recorder::default();
        // A paused stream keeps the sink cycling: the lane's status says processing, while every
        // buffer it hands over is silent.
        engine.set_meters(OUT, meters(false));
        hear(
            &mut app,
            &engine,
            [
                AudioToUi::Devices(listed()),
                attached(OUT, SPEAKERS),
                status(OUT, true, 48_000),
            ],
        );
        fan_out(&mut app, &[], Some(&tray));
        let _ = tray.redraws.take();
        assert_eq!(app.lane_state(OUT), LaneState::Processing, "audio_state");
        let shown = |app: &App| {
            (
                app.tray_state().processing,
                app.state.audio_active,
                crate::commands::status_document(app).audio,
            )
        };
        assert_eq!(shown(&app), (false, false, "idle"));

        // The stream plays again: the three move together, and the tray is told once.
        engine.set_meters(OUT, meters(true));
        app.poll_audio();
        fan_out(&mut app, &[], Some(&tray));
        assert_eq!(shown(&app), (true, true, "processing"));
        assert_eq!(tray.redrawn(), 1);
    }

    #[test]
    fn a_lane_that_went_to_sleep_with_sound_in_its_last_meters_is_not_playing() {
        let (mut app, engine, _dir) = started();
        let tray = Recorder::default();
        engine.set_meters(OUT, meters(true));
        hear(
            &mut app,
            &engine,
            [
                AudioToUi::Devices(listed()),
                attached(OUT, SPEAKERS),
                status(OUT, true, 48_000),
            ],
        );
        fan_out(&mut app, &[], Some(&tray));
        let _ = tray.redraws.take();
        assert!(app.state.audio_active, "a song is playing");

        // The player is closed in the middle of it: the lane's node sleeps, its meters are never
        // published again, and the last of them still say `active`. The lane's status is the word.
        hear(&mut app, &engine, [status(OUT, false, 48_000)]);
        fan_out(&mut app, &[], Some(&tray));
        assert!(
            !app.state.audio_active,
            "the logo and the visualizer go quiet"
        );
        assert!(!app.tray_state().processing, "and so does the tray's icon");
        assert_eq!(crate::commands::status_document(&app).audio, "idle");
        assert_eq!(tray.redrawn(), 1, "told once");

        // The frozen meters, read again and again, change nothing more.
        app.poll_audio();
        fan_out(&mut app, &[], Some(&tray));
        assert!(!app.state.audio_active);
        assert!(
            !app.meters_moved(),
            "and the window has nothing to paint for them"
        );
        assert_eq!(tray.redrawn(), 0);
    }
}
