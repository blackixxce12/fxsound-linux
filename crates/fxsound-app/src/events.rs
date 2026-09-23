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
//! [`Snapshot`] is where the events come from for now: it remembers what the stream last said
//! about the controller and names what differs on each tick. The controller keeping its own queue
//! of events as they happen replaces it.

use std::fmt::Write as _;
use std::time::Instant;

use fxsound_core::DeviceDirection;
use serde_json::Value;

use crate::App;
use crate::commands::InputMeters;

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
    /// The window was opened or hidden to the tray.
    Window { visible: bool },
    /// The instance is quitting; the stream ends right after this.
    Quit,
}

/// What a lane's audio is doing, as `audio_state` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    /// Buffers are flowing through the chain.
    Processing,
    /// Attached, but nothing is playing into it (or nothing is being captured).
    Idle,
    /// There is no audio engine at all.
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
            Self::Window { .. } => "window",
            Self::Quit => "quit",
        }
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
            Self::Window { visible } => vec![("visible", Value::from(*visible))],
            Self::Quit => Vec::new(),
        }
    }
}

/// Append ` key=value` — or one such pair per leaf, with dotted keys, for an object.
fn push_plain(out: &mut String, key: &str, value: &Value) {
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
// Where the events come from, until the controller keeps its own queue
// =============================================================================================

/// What the stream last said about the controller, so that a tick says only what changed.
///
/// Diffed on every tick, so it is built to cost nothing when nothing moved: the comparisons run on
/// borrowed strings and a name is cloned only when it changed. The controller that queues events
/// as it makes the changes (`App::drain_events`, 0.4.0 design §10) replaces this.
#[derive(Debug, Clone)]
pub struct Snapshot {
    power: bool,
    direction: DeviceDirection,
    /// Per lane, indexed by [`lane`]: the preset and whether it has unsaved changes.
    presets: [Option<(String, bool)>; 2],
    /// Per lane: `node.name` and description of the device, both `None` while detached.
    devices: [(Option<String>, Option<String>); 2],
    /// The `node.name` of every device on offer, in list order.
    device_names: Vec<String>,
    /// Per lane: state, rate and channel count.
    audio: [(LaneState, u32, u16); 2],
    /// When the notice on screen was raised, so the same text raised again is a new notice.
    notice: Option<Instant>,
}

/// A lane's slot in the per-lane arrays.
const fn lane(direction: DeviceDirection) -> usize {
    match direction {
        DeviceDirection::Output => 0,
        DeviceDirection::Input => 1,
    }
}

impl Snapshot {
    /// What the controller looks like now; diffing against it right away yields nothing.
    #[must_use]
    pub fn of(app: &App) -> Self {
        let mut snapshot = Self {
            power: false,
            direction: DeviceDirection::Output,
            presets: [None, None],
            devices: [(None, None), (None, None)],
            device_names: Vec::new(),
            audio: [(LaneState::Unavailable, 0, 0); 2],
            notice: None,
        };
        snapshot.diff(app, |_| {});
        snapshot
    }

    /// Hand every difference between the controller and the snapshot to `emit`, and take the
    /// controller's values as the new snapshot.
    pub fn diff(&mut self, app: &App, mut emit: impl FnMut(AppEvent)) {
        let state = &app.state;

        if self.power != state.power {
            self.power = state.power;
            emit(AppEvent::Power { on: state.power });
        }
        if self.direction != state.direction {
            self.direction = state.direction;
            emit(AppEvent::Direction {
                direction: state.direction,
            });
        }

        let listed = &state.devices;
        if self.device_names.len() != listed.len()
            || self
                .device_names
                .iter()
                .zip(listed)
                .any(|(seen, device)| *seen != device.name)
        {
            self.device_names = listed.iter().map(|d| d.name.clone()).collect();
            emit(AppEvent::DevicesChanged {
                count: listed.len(),
            });
        }

        for direction in DeviceDirection::ALL {
            let slot = lane(direction);

            let device = state.device_for(direction);
            let (node_name, description) = &self.devices[slot];
            if node_name.as_deref() != device.map(|d| d.name.as_str())
                || description.as_deref() != device.map(|d| d.description.as_str())
            {
                let node_name = device.map(|d| d.name.clone());
                let description = device.map(|d| d.description.clone());
                self.devices[slot] = (node_name.clone(), description.clone());
                emit(AppEvent::DeviceChanged {
                    direction,
                    node_name,
                    description,
                });
            }

            let preset = app.lane_preset(direction);
            if self.presets[slot]
                .as_ref()
                .map(|(name, modified)| (name.as_str(), *modified))
                != preset
            {
                self.presets[slot] = preset.map(|(name, modified)| (name.to_owned(), modified));
                emit(AppEvent::PresetChanged {
                    direction,
                    name: preset.map(|(name, _)| name.to_owned()),
                    modified: preset.is_some_and(|(_, modified)| modified),
                });
            }

            // Each lane's own format: a 16 kHz headset microphone beside 48 kHz speakers reports
            // two different rates.
            let status = app.audio_status_for(direction);
            let active = match direction {
                DeviceDirection::Output => state.output_active,
                DeviceDirection::Input => state.input_active,
            };
            let lane_state = if !app.has_audio() {
                LaneState::Unavailable
            } else if active {
                LaneState::Processing
            } else {
                LaneState::Idle
            };
            let audio = (lane_state, status.sample_rate, status.channels);
            if self.audio[slot] != audio {
                self.audio[slot] = audio;
                emit(AppEvent::AudioState {
                    direction,
                    state: lane_state,
                    sample_rate: status.sample_rate,
                    channels: status.channels,
                });
            }
        }

        // A notice's clock starts when it is raised (`UiState::notify`), or on the first poll
        // that sees one written straight into the field; either way a new instant is a new
        // notice, the same text raised twice included.
        let clock = state.notice_clock.as_ref();
        let raised = clock.map(|(_, since)| *since);
        if self.notice != raised {
            self.notice = raised;
            if let Some((message, _)) = clock {
                emit(AppEvent::Notice {
                    message: message.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::AudioDevice;
    use fxsound_ui::state::PresetEntry;
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

    fn changes(snapshot: &mut Snapshot, app: &App) -> Vec<AppEvent> {
        let mut events = Vec::new();
        snapshot.diff(app, |event| events.push(event));
        events
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
    fn a_snapshot_taken_now_has_nothing_to_report() {
        let app = App::headless_for_tests();
        let mut snapshot = Snapshot::of(&app);
        assert!(changes(&mut snapshot, &app).is_empty());
    }

    #[test]
    fn switching_the_power_off_is_reported_once() {
        let mut app = App::headless_for_tests();
        let mut snapshot = Snapshot::of(&app);
        app.state.power = false;
        assert_eq!(
            changes(&mut snapshot, &app),
            [AppEvent::Power { on: false }]
        );
        assert!(
            changes(&mut snapshot, &app).is_empty(),
            "the same state is not news twice"
        );
    }

    #[test]
    fn a_new_edit_direction_is_reported() {
        let mut app = App::headless_for_tests();
        let mut snapshot = Snapshot::of(&app);
        app.state.direction = DeviceDirection::Input;
        let events = changes(&mut snapshot, &app);
        assert!(
            events.contains(&AppEvent::Direction {
                direction: DeviceDirection::Input
            }),
            "{events:?}"
        );
    }

    #[test]
    fn picking_a_device_reports_the_list_and_the_lane_and_detaching_reports_null() {
        let mut app = App::headless_for_tests();
        let mut snapshot = Snapshot::of(&app);
        app.state.devices = vec![
            device("alsa_output.speakers", "Speakers", DeviceDirection::Output),
            device("alsa_input.mic", "Microphone", DeviceDirection::Input),
        ];
        app.state.selected_input = Some(1);
        let events = changes(&mut snapshot, &app);
        assert!(events.contains(&AppEvent::DevicesChanged { count: 2 }));
        assert!(events.contains(&AppEvent::DeviceChanged {
            direction: DeviceDirection::Input,
            node_name: Some("alsa_input.mic".to_owned()),
            description: Some("Microphone".to_owned()),
        }));
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AppEvent::DeviceChanged {
                    direction: DeviceDirection::Output,
                    ..
                }
            )),
            "the output lane did not move: {events:?}"
        );

        app.state.selected_input = None;
        assert_eq!(
            changes(&mut snapshot, &app),
            [AppEvent::DeviceChanged {
                direction: DeviceDirection::Input,
                node_name: None,
                description: None,
            }]
        );
    }

    #[test]
    fn a_device_replaced_by_another_of_the_same_count_is_still_a_new_list() {
        let mut app = App::headless_for_tests();
        app.state.devices = vec![device("a", "A", DeviceDirection::Output)];
        let mut snapshot = Snapshot::of(&app);
        app.state.devices = vec![device("b", "B", DeviceDirection::Output)];
        assert_eq!(
            changes(&mut snapshot, &app),
            [AppEvent::DevicesChanged { count: 1 }]
        );
    }

    #[test]
    fn a_preset_gaining_unsaved_changes_is_reported_for_its_lane() {
        let mut app = App::headless_for_tests();
        app.state.presets = vec![PresetEntry {
            name: "Rock".to_owned(),
            factory: true,
            modified: false,
        }];
        app.state.selected_preset = Some(0);
        let mut snapshot = Snapshot::of(&app);

        app.state.presets[0].modified = true;
        assert_eq!(
            changes(&mut snapshot, &app),
            [AppEvent::PresetChanged {
                direction: DeviceDirection::Output,
                name: Some("Rock".to_owned()),
                modified: true,
            }]
        );
    }

    #[test]
    fn the_same_notice_raised_twice_is_two_events() {
        let mut app = App::headless_for_tests();
        let mut snapshot = Snapshot::of(&app);
        app.state.notify("Preset saved");
        assert_eq!(
            changes(&mut snapshot, &app),
            [AppEvent::Notice {
                message: "Preset saved".to_owned()
            }]
        );
        // `Instant` has to move for the clock to be a new one.
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.state.notify("Preset saved");
        assert_eq!(changes(&mut snapshot, &app).len(), 1);

        app.state.dismiss_notification();
        assert!(
            changes(&mut snapshot, &app).is_empty(),
            "a notice going away is not a notice"
        );
    }

    #[test]
    fn a_lane_without_an_engine_is_unavailable_from_the_start() {
        let app = App::headless_for_tests();
        let snapshot = Snapshot::of(&app);
        assert_eq!(snapshot.audio[0].0, LaneState::Unavailable);
        assert_eq!(snapshot.audio[1].0, LaneState::Unavailable);
    }

    #[test]
    fn each_lane_reports_its_own_format() {
        let mut app = App::headless_for_tests();
        let mut snapshot = Snapshot::of(&app);
        // A 16 kHz headset microphone beside speakers that have not said anything new.
        app.receive(fxsound_core::AudioToUi::Status {
            direction: DeviceDirection::Input,
            status: fxsound_core::AudioStatus {
                sample_rate: 16_000,
                channels: 1,
                ..fxsound_core::AudioStatus::default()
            },
        });
        assert_eq!(
            changes(&mut snapshot, &app),
            [AppEvent::AudioState {
                direction: DeviceDirection::Input,
                state: LaneState::Unavailable,
                sample_rate: 16_000,
                channels: 1,
            }],
            "the speakers' lane kept its own format, so it has nothing to report"
        );
    }
}
