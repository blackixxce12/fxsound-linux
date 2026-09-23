//! The FxSound audio backend for Linux: a PipeWire virtual sink in front of the speakers and a
//! PipeWire virtual source behind the microphone, each with its own DSP chain in its process
//! callback, running side by side.
//!
//! # What this replaces
//!
//! On Windows, FxSound ships a kernel virtual audio driver, forces it to be the system default
//! render endpoint, captures from it with `AUDCLNT_STREAMFLAGS_LOOPBACK`, runs the DSP over the
//! captured buffer and renders the result to the real device — the `audiopassthru/` module,
//! reverse-engineered in full in `docs/spec/12-audio-io.md`. On Linux none of that machinery is
//! needed: a `pw_stream` with `media.class = "Audio/Sink"` **is** a sink, so the audio applications
//! write arrives directly in our own process callback. There is no driver to install, no reboot,
//! no root, no loopback capture and no monitor hop.
//!
//! # Topology
//!
//! One pair of nodes per **lane** — one lane per [`fxsound_core::DeviceDirection`] — the two nodes
//! of a pair sharing a `node.link-group` of their own ([`link_group`]): the design
//! `docs/spec/12-audio-io.md` §18 chose (Option A), §19.1 draws, and §29 extends to two lanes.
//! Either lane can be enabled or detached on its own, and both pairs can exist at once
//! (`docs/0.4.0-design.md` §1). The two lanes are mirror images of each other, and the two process
//! callbacks and the ring between them are the same code in both — each lane has its own ring,
//! its own chain and its own paths to the GUI:
//!
//! ```text
//!  OUTPUT LANE (the Windows behaviour; enabled at start):
//!
//!  apps ──► fxsound_sink    (Audio/Sink, Direction::Input)    process(): DSP in place ──┐
//!                                                                                       │ ring
//!           fxsound_output  (Stream/Output/Audio, Output)     process(): pop ◄──────────┘
//!                │
//!                ▼  target.object = <chosen sink's node.name>
//!           alsa_output.…   (the user's real speakers)
//!
//!  INPUT LANE (Linux only, `docs/spec/12-audio-io.md` §28; enabled when a microphone is picked):
//!
//!           alsa_input.…    (the user's real microphone)
//!                │
//!                ▼  target.object = <chosen source's node.name>
//!           fxsound_capture (Stream/Input/Audio, Direction::Input)  process(): DSP in place ──┐
//!                                                                                            │ ring
//!  apps ◄── fxsound_source  (Audio/Source, Direction::Output)       process(): pop ◄─────────┘
//! ```
//!
//! Why two streams rather than one filter: `pipewire` 0.10.1 has no `pw_filter` binding at all
//! (`docs/api/pipewire-0.10-rust.md` §12), and even if it had, one node cannot be both an
//! `Audio/Sink` and a stream that targets a chosen device. Why not a `module-null-sink` created
//! through `pactl`: a null sink outlives a `SIGKILL`ed app, and if it was the default the user is
//! left with a silent machine — the exact failure class the project `CLAUDE.md` warns about for
//! `audiopassthru/`. Nodes owned by our own client connection disappear the instant the socket
//! closes.
//!
//! The `node.link-group` is not decoration: it is how WirePlumber learns our nodes are one
//! logical device and refuses to link `fxsound_output` back into `fxsound_sink` — or, in the input
//! lane, `fxsound_capture` into `fxsound_source` — which is otherwise exactly what happens the
//! moment our node becomes the default.
//!
//! It is not only WirePlumber's business, either. Since 0.3.68 the PipeWire server schedules the
//! members of a link-group together: a node made runnable by a link of its own makes every other
//! member runnable too (`run_nodes` in `src/pipewire/context.c`). That is what lets the output
//! lane's playback stream sleep while nothing plays into the sink (see `engine`, "Idle"), and it is
//! why each lane has a group **of its own** — `fxsound` for the speakers, `fxsound-input` for the
//! microphone (`docs/spec/12-audio-io.md` §29.2). WirePlumber would be content either way: each
//! stream still shares a group with its own virtual node, which is all its refusal needs, and no
//! link either lane needs ever joins two of our own nodes. The server is not. With one group, a
//! capture stream fed by a microphone — which runs for as long as the input lane is enabled —
//! made the speakers' pair runnable with it, and the speakers never went idle again while the
//! microphone lane was on.
//!
//! With both lanes enabled the system sees *two* FxSound devices: "FxSound (Output)" under its
//! sinks and "FxSound (Input)" under its sources, each with the word in the system language
//! ([`locale`]). Detaching a lane takes only its own device away.
//!
//! # Echo cancellation
//!
//! Off by default, and on request the input lane records through PipeWire's own canceller rather
//! than from the microphone directly (`docs/0.4.0-design.md` §7). The canceller is
//! `libpipewire-module-echo-cancel`, loaded into this crate's PipeWire context (`aec`): it hears
//! the microphone and the monitor of the speakers the output lane plays to, and its
//! `fxsound_aec_source` takes the microphone's place in front of `fxsound_capture`. The three nodes
//! it makes are FxSound's as much as the lanes' four are — named by us, in a link-group of their
//! own ([`AEC_LINK_GROUP`]), never offered as devices — and they come and go with the microphone
//! lane: loaded once it has a pair, reloaded when its microphone or the speakers change, unloaded
//! when echo cancellation is switched off, when the lane is detached, and before the connection
//! or the thread ends.
//!
//! # Threads
//!
//! Nothing in `pipewire` or `libspa` is `Send`, so the whole PipeWire side lives on one thread
//! that this crate spawns and owns. The GUI keeps an [`EngineHandle`], which is `Send` and talks
//! over paths chosen for what each one carries. Every path that reaches a chain is **per lane** —
//! the output lane's music chain and the input lane's voice chain each have their own — so an
//! event meant for the microphone can never be consumed by the music chain, and the two chains'
//! meters can never overwrite each other:
//!
//! | Direction | Carries | Mechanism | Per lane | Why |
//! | --- | --- | --- | --- | --- |
//! | GUI → DSP | [`DspParams`] / [`InputDspParams`] snapshots | [`triple_buffer`] | one per chain: [`EngineHandle::set_params`], [`EngineHandle::set_input_params`] | Wait-free on both ends, no lock, no allocation. A coalesced intermediate snapshot is harmless: the next one supersedes it. |
//! | GUI → DSP | [`DspEvent`] one-shots | bounded `crossbeam_channel` | one per lane: [`EngineHandle::send_event`] | Must not be coalesced. Pre-allocated array channel; `try_recv` never allocates and never parks. |
//! | DSP → GUI | [`Meters`] | [`triple_buffer`] | one per lane: [`EngineHandle::meters`] | Same reasoning, other way round. |
//! | GUI ↔ control | [`UiToAudio`] / [`AudioToUi`] | `pipewire::channel` / `crossbeam_channel` | shared; messages name their lane | May allocate and block; never touched from the process callback. |
//!
//! The process callback itself allocates nothing, locks nothing and cannot panic: every buffer it
//! needs is sized once in [`AudioEngine::start`] for the worst case in §24 of the spec (2048
//! frames × 8 channels), every `Option` is handled with `let … else { return }`, and the two
//! callbacks exchange samples through an SPSC ring built out of atomics.
//!
//! # The session default
//!
//! Once a lane's pair of nodes is up, FxSound **does** make itself the session default for that
//! lane's direction — `default.configured.audio.sink = fxsound_sink` for the output lane,
//! `default.configured.audio.source = fxsound_source` for the input lane — because that is the
//! whole point: the user picks *their* speakers or *their* microphone in FxSound and every
//! application follows without being re-routed by hand, exactly as the Windows driver does. Each
//! lane holds its own claim. It does so politely, per `docs/spec/12-audio-io.md` §21: the default
//! that was there before is remembered first and handed back — on exit (both lanes'), when a lane
//! is detached (that lane's), and whenever a lane's nodes go away for good — **before** the nodes
//! are destroyed, so there is never a window in which a default names a node that no longer
//! exists. Only the `configured` keys are ever written; `default.audio.*` is WirePlumber's. A
//! caller can opt out, lane by lane, with [`UiToAudio::SetAsDefault`] with `want: false`.
//!
//! # What it deliberately does not do
//!
//! * **It never writes volume or mute on the target device.** On Windows the DFX endpoint and the
//!   real endpoint are two separate system volume controls and FxSound mirrors them
//!   (`sndDevicesVolCallbacks.cpp`). On Linux, once `fxsound_sink` is the default it *is* the
//!   control the volume keys, `wpctl` and every desktop slider target; mirroring would fight
//!   WirePlumber's own restore and would leave the real device permanently changed after we exit.
//!   The whole `savedPlaybackVolume` / `b_never_raise_volume` contract therefore disappears — see
//!   `docs/spec/12-audio-io.md` §19.7.
//! * **It never resamples.** The zero-order-hold upsampler at `sndDevicesDoPlayback.cpp:79-89` is
//!   not ported; PipeWire's sinc resampler handles any rate mismatch — and its channel mixer turns
//!   a mono microphone into the stereo pair the DSP runs on, and that pair into a mono headset.
//! * **It never wins the default implicitly.** `priority.session` is deliberately low, so a
//!   WirePlumber that has never been told a default picks real hardware, not us; the default is
//!   taken only through the explicit metadata write above, which is also the only thing that can
//!   be handed back.
//! * **It never feeds one lane from the other.** The two lanes share nothing, not even a
//!   link-group: each has its own group, its own ring, its own chain, its own counters and its own
//!   claim on a default, so the music chain cannot reach the microphone's signal or the other way
//!   round, the microphone running cannot keep the speakers awake, and one lane failing leaves the
//!   other playing. Echo cancellation is the one bridge, and only while it is on: the canceller
//!   hears what the speakers play in order to take its echo out of the microphone — nothing of it
//!   reaches a recording — and it hears that on the microphone's clock, so the speakers and the
//!   output pair run for as long as it does (`docs/0.4.0-design.md` §7).
//!
//! [`DspParams`]: fxsound_core::messages::DspParams
//! [`InputDspParams`]: fxsound_core::messages::InputDspParams
//! [`DspEvent`]: fxsound_core::messages::DspEvent
//! [`Meters`]: fxsound_core::messages::Meters
//! [`UiToAudio`]: fxsound_core::messages::UiToAudio
//! [`AudioToUi`]: fxsound_core::messages::AudioToUi
//! [`UiToAudio::SetAsDefault`]: fxsound_core::messages::UiToAudio::SetAsDefault

// `deny`, not `forbid`, for exactly one reason: the echo canceller. PipeWire's canceller is a
// module (`libpipewire-module-echo-cancel`, `docs/0.4.0-design.md` §7), and `pipewire` 0.10.1 has
// no binding for loading one — no `pw_context_load_module`, no `pw_impl_module` at all. So `aec`
// calls the C functions through `pipewire-sys`, and those three functions, and nothing else in
// the crate, carry an `#[allow(unsafe_code)]` — each `unsafe` block with the reason it is sound
// beside it. Everything else here, the process callbacks above all, stays as safe as `forbid`
// made it, and a new `unsafe` anywhere else still fails the build.
#![deny(unsafe_code)]

mod aec;
pub mod devices;
pub mod engine;
mod lane_dsp;
pub mod locale;
mod per_direction;

use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use fxsound_core::DeviceDirection;
use fxsound_core::messages::{AudioToUi, DspEvent, DspParams, InputDspParams, Meters, UiToAudio};
use triple_buffer::{Input, Output, TripleBuffer};

use crate::per_direction::PerDirection;

pub use devices::{
    BLUEZ_HEADSET_RATE, ChannelMap, DeviceInfo, FormFactor, MAX_CHANNELS, MAX_SAMPLE_RATE,
    MIN_CHANNELS, Selection, SelectionMemory,
};
pub use locale::{node_description, sink_description, source_description};

/// `node.name` of the virtual sink applications render into (output direction, NODE 1).
///
/// This string is the FxSound analogue of `SND_DEVICES_DFX_DEVICE_STRING`
/// (`audiopassthru/include/sndDevices.h:51`) and, like it, is matched by name elsewhere — it is
/// what gets written into `default.configured.audio.sink`, so it must stay stable across releases.
pub const SINK_NODE_NAME: &str = "fxsound_sink";

/// `node.name` of the stream that renders into the user's real device (output direction, NODE 2).
pub const OUTPUT_NODE_NAME: &str = "fxsound_output";

/// `node.name` of the stream that captures from the user's real microphone (input direction,
/// NODE 1).
pub const CAPTURE_NODE_NAME: &str = "fxsound_capture";

/// `node.name` of the virtual source applications record from (input direction, NODE 2).
///
/// Written into `default.configured.audio.source`; stable for the same reason as
/// [`SINK_NODE_NAME`].
pub const SOURCE_NODE_NAME: &str = "fxsound_source";

/// `node.name` of the echo canceller's capture stream: the one that hears the microphone
/// (`docs/0.4.0-design.md` §7).
pub const AEC_CAPTURE_NODE_NAME: &str = "fxsound_aec_capture";

/// `node.name` of the echo canceller's monitor stream: the one that hears what the speakers play,
/// the echo it takes out of the microphone.
pub const AEC_MONITOR_NODE_NAME: &str = "fxsound_aec_monitor";

/// `node.name` of the echo canceller's source: the microphone with the echo taken out, and what the
/// input lane's capture stream records from while echo cancellation runs.
pub const AEC_SOURCE_NODE_NAME: &str = "fxsound_aec_source";

/// The four nodes of the two lanes' pairs, in lane order: the output lane's, then the input lane's.
pub const LANE_NODE_NAMES: [&str; 4] = [
    SINK_NODE_NAME,
    OUTPUT_NODE_NAME,
    CAPTURE_NODE_NAME,
    SOURCE_NODE_NAME,
];

/// The three nodes the echo canceller's module makes while echo cancellation runs.
pub const AEC_NODE_NAMES: [&str; 3] = [
    AEC_CAPTURE_NODE_NAME,
    AEC_MONITOR_NODE_NAME,
    AEC_SOURCE_NODE_NAME,
];

/// Every node this crate ever creates, or has PipeWire create for it: [`LANE_NODE_NAMES`], then
/// [`AEC_NODE_NAMES`]. None of them is a device FxSound could attach to, so
/// [`DeviceInfo::from_props`] drops them by name whatever their `media.class` says. The canceller's
/// source is an `Audio/Source` like any microphone, and offered as one it would let the input lane
/// capture from its own canceller.
pub const OUR_NODE_NAMES: [&str; 7] = [
    SINK_NODE_NAME,
    OUTPUT_NODE_NAME,
    CAPTURE_NODE_NAME,
    SOURCE_NODE_NAME,
    AEC_CAPTURE_NODE_NAME,
    AEC_MONITOR_NODE_NAME,
    AEC_SOURCE_NODE_NAME,
];

/// The `node.link-group` of the output lane's two nodes. **Mandatory**; see the module docs.
///
/// The same string 0.3.0 gave its one pair, so the speakers' pair looks to anything outside the
/// process exactly as it always has.
pub const LINK_GROUP: &str = "fxsound";

/// The `node.link-group` of the input lane's two nodes: a group of their own, not the output
/// lane's, because the server runs a group's members together (module docs).
pub const INPUT_LINK_GROUP: &str = "fxsound-input";

/// The `node.link-group` of the echo canceller's three streams: a group of their own, neither
/// lane's (`docs/0.4.0-design.md` §7, and `aec` for the reasoning).
///
/// Not the input lane's, because the input lane's capture stream links to the canceller's source,
/// and WirePlumber refuses a link between two nodes of one group. Not the output lane's, because
/// the server runs a group together, and the canceller's capture stream — which runs whenever the
/// microphone does — would keep the speakers' pair running with it.
pub const AEC_LINK_GROUP: &str = "fxsound-aec";

/// The `node.link-group` of a lane's pair.
#[must_use]
pub const fn link_group(direction: DeviceDirection) -> &'static str {
    match direction {
        DeviceDirection::Output => LINK_GROUP,
        DeviceDirection::Input => INPUT_LINK_GROUP,
    }
}

/// The product name every description starts with, and the `media.name` of the two streams.
///
/// The virtual sink and source are *not* described with this string alone: their
/// `node.description` is `"FxSound (<Output|Input>)"` in the system language — see
/// [`sink_description`] / [`source_description`] — so the two can be told apart in a device list.
pub const SINK_DESCRIPTION: &str = "FxSound";

/// `node.description` of the playback stream (output direction, NODE 2). Not localised: it is an
/// internal node UIs group under the sink through the link-group.
pub const OUTPUT_STREAM_DESCRIPTION: &str = "FxSound output";

/// `node.description` of the capture stream (input direction, NODE 1). Not localised, as above.
pub const CAPTURE_STREAM_DESCRIPTION: &str = "FxSound capture";

/// What the engine tells the user, translated, when one Bluetooth headset is the target of both
/// lanes ([`AudioToUi::Warning`], `docs/0.4.0-upstream.md` U9): the headset runs its call profile
/// while anything records from it through FxSound, and the music lane's sink goes with it — one
/// channel, 8 to 32 kHz depending on the codec, 16 kHz on nearly every headset.
///
/// The English text is the key [`fxsound_core::i18n::tr`] looks the translation up by. Public so
/// the app's translation tables and their audit can name it: the audit reads the crates that draw
/// text, and this string reaches the screen from here.
pub const ONE_HEADSET_ON_BOTH_LANES: &str =
    "Using this headset's microphone switches it to call quality: music plays in mono at 16 kHz";

/// The `node.name` of FxSound's virtual device for a direction — the value written into that
/// direction's `default.configured.audio.*` key.
#[must_use]
pub const fn our_node_name(direction: DeviceDirection) -> &'static str {
    match direction {
        DeviceDirection::Output => SINK_NODE_NAME,
        DeviceDirection::Input => SOURCE_NODE_NAME,
    }
}

/// The largest quantum the process callbacks are sized for, in frames
/// (`docs/spec/12-audio-io.md` §24). A larger graph quantum is processed in several passes rather
/// than being truncated.
pub const MAX_QUANTUM_FRAMES: usize = 2048;

/// The quantum FxSound asks for by default: 512 frames, ≈10.7 ms at 48 kHz, the `Normal` row of
/// the spec's latency table.
pub const DEFAULT_QUANTUM_FRAMES: u32 = 512;

/// Sample rate used until the server tells us otherwise.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// Ring capacity in frames, `8 × MAX_QUANTUM_FRAMES` — 512 KiB at 8 channels
/// (`docs/spec/12-audio-io.md` §24).
pub const RING_CAPACITY_FRAMES: usize = 8 * MAX_QUANTUM_FRAMES;

/// How many [`DspEvent`]s may be in flight before the oldest is dropped.
pub const EVENT_QUEUE_LEN: usize = 64;

/// How long [`AudioEngine::start`] waits for the *first* connection attempt before handing the
/// caller a handle anyway and letting the engine keep retrying in the background.
///
/// Connecting to a socket that is not there fails in microseconds, so in the failure case this
/// timeout is never approached; it only bounds the pathological "server accepted the socket but
/// never replied" case.
pub const START_TIMEOUT: Duration = Duration::from_millis(1500);

/// Everything that can go wrong, in 1:1 correspondence with the states the Windows GUI already
/// knows how to render.
///
/// The comments give the `sndDevices.h:74-143` code each variant replaces, so
/// `FxController`'s existing error branches port without re-deriving the mapping.
///
/// Two of those states have no variant: `-57 SND_DEVICES_NO_VALID_PLAYBACK_DEVICE` ("no stereo
/// output") and `-58 SND_DEVICES_ASK_USER_SELECT_PLAYBACK_DEVICE` ("the chosen output is mono,
/// pick another"). Both existed only to refuse mono playback devices, a workaround for a Windows
/// driver bug (`sndDevices.h:32-39`); PipeWire drives a mono device perfectly well, so the port
/// attaches to one instead (`docs/spec/12-audio-io.md`, open question 6).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    /// No `Audio/Sink` node exists other than ours. ≡ `209 SND_DEVICES_NO_REAL_DEVICES_FOUND`.
    #[error("no output devices present")]
    NoOutputDevices,
    /// No `Audio/Source` node exists other than ours. The input-direction twin of
    /// [`AudioError::NoOutputDevices`]; Windows had no capture mode and so no code for it.
    #[error("no input devices present")]
    NoInputDevices,
    /// The requested device is not in the graph. ≡ `-2 SND_DEVICES_DEVICE_NOT_PRESENT`.
    #[error("selected device is not present")]
    DeviceNotPresent,
    /// The device is present but refused the stream. ≡ `-54` plus `playbackDeviceIsUnavailable`.
    #[error("audio device is unavailable")]
    DeviceUnavailable,
    /// PipeWire could not be reached at all — no socket, no `XDG_RUNTIME_DIR`, or the connection
    /// was refused. There is no Windows analogue: the driver was always present.
    #[error("PipeWire is not available: {0}")]
    PipewireUnavailable(String),
    /// The connection dropped after having worked. The engine is already retrying.
    #[error("lost connection to PipeWire")]
    PipewireDisconnected,
    /// The server would not agree a format we can process. ≡ `-35` / `-36`.
    #[error("format negotiation failed")]
    FormatNegotiation,
}

/// Map the legacy `buffer_ms` setting onto a PipeWire quantum.
///
/// The Windows range is 10…100 ms (`sndDevices.h:200-201`, clamped at
/// `sndDevicesSet.cpp:541-545`); the result is a power of two in `[256, 2048]` because graph-wide
/// quantum negotiation behaves best with them. Note that PipeWire takes the *minimum* requested
/// latency across the graph, clamped to `default.clock.min-quantum … max-quantum`, so this can
/// only ever lower the graph quantum, never raise it.
#[must_use]
pub fn quantum_for_ms(ms: u32, rate: u32) -> u32 {
    let ms = ms.clamp(10, 100);
    let rate = rate.clamp(8_000, MAX_SAMPLE_RATE);
    let frames = u64::from(ms) * u64::from(rate) / 1000;
    let frames = u32::try_from(frames).unwrap_or(MAX_QUANTUM_FRAMES as u32);
    (frames.next_power_of_two() >> 1).clamp(256, MAX_QUANTUM_FRAMES as u32)
}

/// The audio backend. Owns the PipeWire thread and everything on it.
///
/// There is no public constructor other than [`AudioEngine::start`], which hands back an
/// [`EngineHandle`] that owns this. The GUI never touches an `AudioEngine` directly, because
/// every PipeWire object it transitively owns is thread-affine.
pub struct AudioEngine {
    control: pipewire::channel::Sender<UiToAudio>,
    join: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Spawn the audio thread.
    ///
    /// Returns as soon as the first connection attempt has resolved, which for the two outcomes
    /// that matter is immediate: a missing server fails at `connect(2)`, and a present one is
    /// connected in well under a millisecond. Everything after that — registry enumeration,
    /// device selection, creating the two nodes, taking the session default — happens in the
    /// background and is reported through [`EngineHandle::try_recv`].
    ///
    /// The engine starts with the output lane enabled and the input lane detached. The output lane
    /// attaches to the device the port of `sndDevicesImplementDeviceRules` picks
    /// ([`devices::choose_device`]) and, as soon as its nodes are up, makes `fxsound_sink` the
    /// configured default sink — remembering the previous default so it can be handed back
    /// (`docs/spec/12-audio-io.md` §21). Send [`UiToAudio::SelectDevice`] to attach a lane to a
    /// specific device — a microphone enables the input lane beside the output lane, never instead
    /// of it — [`UiToAudio::DetachLane`] to switch a lane off, and [`UiToAudio::SetAsDefault`]
    /// with `want: false` to keep a lane's default where it is.
    ///
    /// # Errors
    /// [`AudioError::PipewireUnavailable`] if there is no PipeWire session to connect to, or if
    /// the thread could not be spawned. Later failures are not errors here: the engine keeps
    /// retrying with the backoff in `docs/spec/12-audio-io.md` §22 and reports state through
    /// [`AudioToUi`].
    ///
    /// [`UiToAudio::SelectDevice`]: fxsound_core::messages::UiToAudio::SelectDevice
    /// [`UiToAudio::DetachLane`]: fxsound_core::messages::UiToAudio::DetachLane
    pub fn start() -> Result<EngineHandle, AudioError> {
        Self::start_with(None, None, aec::WEBRTC_LIBRARY)
    }

    /// [`AudioEngine::start`], naming the language the virtual nodes are described in.
    ///
    /// `language` is one of `fxsound_core::i18n::LANGUAGES`' codes — the UI's effective language,
    /// so `FxSound (Вывод)` in the sound settings matches a Russian FxSound window even when the
    /// desktop locale says otherwise. `None` (plain [`AudioEngine::start`]) reads the locale.
    pub fn start_with_language(language: &str) -> Result<EngineHandle, AudioError> {
        Self::start_with(None, Some(language), aec::WEBRTC_LIBRARY)
    }

    /// [`AudioEngine::start`], against a named PipeWire socket rather than the session default.
    ///
    /// Crate-internal because the public surface is fixed, but it is the seam the tests use: a
    /// remote name that does not exist exercises the whole spawn/connect/report path — including
    /// the error mapping and the thread shutdown — without a server and without creating a single
    /// node in the user's live graph.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn start_with_remote(remote: Option<&str>) -> Result<EngineHandle, AudioError> {
        Self::start_with(remote, None, aec::WEBRTC_LIBRARY)
    }

    /// [`Self::start_with_remote`], with the echo canceller running `library` rather than WebRTC.
    ///
    /// For the tests: `aec/libspa-aec-null` passes the microphone through untouched, which is all a
    /// test of where the canceller's nodes go needs, and a library that does not exist is how a
    /// test makes the load fail the way a missing `libspa-aec-webrtc` would.
    #[cfg(test)]
    pub(crate) fn start_with_canceller(
        remote: Option<&str>,
        library: &'static str,
    ) -> Result<EngineHandle, AudioError> {
        Self::start_with(remote, None, library)
    }

    fn start_with(
        remote: Option<&str>,
        language: Option<&str>,
        aec_library: &'static str,
    ) -> Result<EngineHandle, AudioError> {
        // Whether the *socket* exists is left to `pw_context_connect`, which resolves
        // `remote.name` itself and reports the failure precisely. What has to be checked first is
        // the thing PipeWire cannot report usefully: no runtime directory at all, which is what a
        // bare TTY or a Flatpak without the `pipewire` socket permission looks like
        // (`docs/spec/12-audio-io.md` §22).
        engine::preflight(std::env::var_os("XDG_RUNTIME_DIR").as_deref())?;

        let (mut handle, mut config, ready) = EngineHandle::wire(remote, language);
        config.aec_library = aec_library;
        let join = std::thread::Builder::new()
            .name("fxsound-audio".to_owned())
            .spawn(move || engine::run(config))
            .map_err(|e| AudioError::PipewireUnavailable(e.to_string()))?;
        handle.engine.join = Some(join);

        match ready.recv_timeout(START_TIMEOUT) {
            // Connected, or still trying — either way the caller gets a working handle.
            Ok(Ok(())) | Err(RecvTimeoutError::Timeout) => Ok(handle),
            Ok(Err(error)) => {
                handle.shutdown();
                Err(error)
            }
            Err(RecvTimeoutError::Disconnected) => {
                handle.shutdown();
                Err(AudioError::PipewireUnavailable(
                    "the audio thread exited before it connected".to_owned(),
                ))
            }
        }
    }

    /// Ask the thread to stop and wait for it. Idempotent.
    fn stop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        // If the send fails the loop is already gone; joining is still the right move.
        let _ = self.control.send(UiToAudio::Shutdown);
        if join.join().is_err() {
            log::error!("the FxSound audio thread panicked");
        }
    }
}

impl std::fmt::Debug for AudioEngine {
    /// Hand-written because `pipewire::channel::Sender` is not `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("running", &self.join.is_some())
            .finish()
    }
}

impl Drop for AudioEngine {
    /// Dropping the handle must never leave the virtual sink behind, so the thread is always
    /// joined — the PipeWire connection, and with it both nodes, is torn down before this returns.
    fn drop(&mut self) {
        self.stop();
    }
}

/// The GUI's view of the running engine: cheap, `Send`, safe to poll every frame.
///
/// Deliberately not `Clone`: the parameter and meter paths are single-producer and
/// single-consumer by construction, which is what makes them wait-free.
pub struct EngineHandle {
    engine: AudioEngine,
    params: Input<DspParams>,
    input_params: Input<InputDspParams>,
    /// Each lane's meters, through a buffer of its own.
    meters: PerDirection<Output<Meters>>,
    /// Each lane's event queue.
    events: PerDirection<Sender<DspEvent>>,
    notifications: Receiver<AudioToUi>,
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle")
            .field("engine", &self.engine)
            .field("pending_notifications", &self.notifications.len())
            .finish_non_exhaustive()
    }
}

impl EngineHandle {
    /// Every path between the GUI and the audio thread, both ends, with no thread yet: the handle
    /// (its engine not yet joined to anything), the thread's [`engine::Config`], and the receiver
    /// the thread reports its first connection attempt on.
    ///
    /// The one place the two sides are paired, so a lane's meters buffer and event queue are
    /// created together with the lane's ends of them and cannot be crossed over. Also the seam the
    /// tests use to drive both lanes' DSP through a real handle without a server.
    fn wire(
        remote: Option<&str>,
        language: Option<&str>,
    ) -> (Self, engine::Config, Receiver<Result<(), AudioError>>) {
        let (params_in, params_out) = TripleBuffer::new(&DspParams::default()).split();
        let (input_params_in, input_params_out) =
            TripleBuffer::new(&InputDspParams::default()).split();
        let (meters_in, meters_out) =
            PerDirection::from_fn(|_| TripleBuffer::new(&Meters::default()).split()).unzip();
        let (events_tx, events_rx) =
            PerDirection::from_fn(|_| crossbeam_channel::bounded(EVENT_QUEUE_LEN)).unzip();
        let (ui_tx, ui_rx) = crossbeam_channel::unbounded();
        let (control_tx, control_rx) = pipewire::channel::channel();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let config = engine::Config {
            remote: remote.map(str::to_owned),
            language: language.map(str::to_owned),
            control: control_rx,
            notify: ui_tx,
            params: params_out,
            input_params: input_params_out,
            meters: meters_in,
            events: events_rx,
            ready: ready_tx,
            aec_library: aec::WEBRTC_LIBRARY,
        };
        let handle = Self {
            engine: AudioEngine {
                control: control_tx,
                join: None,
            },
            params: params_in,
            input_params: input_params_in,
            meters: meters_out,
            events: events_tx,
            notifications: ui_rx,
        };
        (handle, config, ready_rx)
    }

    /// Publish a new parameter snapshot.
    ///
    /// Wait-free: [`triple_buffer::Input::write`] is a move into a spare buffer plus one atomic
    /// swap, so this is safe to call on every GUI frame and cannot make the audio thread wait. If
    /// the audio thread has not read the previous snapshot yet, that snapshot is simply
    /// superseded — parameters are state, not events.
    pub fn set_params(&mut self, mut params: DspParams) {
        // The single gate between everything that can produce a snapshot — sliders, the command
        // line, a preset file, `settings.toml` — and the filter designs. Doing it here rather
        // than in each producer means a new caller cannot forget it, and it costs one pass over a
        // `Copy` struct on the GUI thread, never on the audio thread.
        params.sanitise();
        self.params.write(params);
    }

    /// Publish a new snapshot for the microphone chain.
    ///
    /// The sibling of [`Self::set_params`], with the same wait-free guarantee and the same single
    /// gate: [`InputDspParams::sanitise`] runs here so that no producer of a voice preset can
    /// forget it. Separate from the output snapshot because the two chains share nothing but the
    /// equalizer — publishing both through one struct would mean every music preset carried a gate
    /// threshold, and the audio thread would have to know which half of its parameters to ignore.
    ///
    /// Publishing while the input lane has no nodes is harmless and deliberate: the snapshot is
    /// state, so the voice chain reads a current one the moment the lane is attached, and
    /// attaching needs no handshake.
    pub fn set_input_params(&mut self, mut params: InputDspParams) {
        params.sanitise();
        self.input_params.write(params);
    }

    /// Fire a one-shot event at one lane's chain: filter reset, spectrum reset, processed-time
    /// reset, capture-statistics reset.
    ///
    /// The lane is named here rather than in the event because each lane has its own queue, read
    /// by its own chain and by nothing else: a reset meant for the microphone cannot clear the
    /// music chain's history, whichever lanes are running. An event for a lane with no nodes is
    /// applied on the audio thread's main loop, so it is neither lost nor left waiting.
    ///
    /// Unlike a parameter snapshot these must not be coalesced, so they go through a bounded
    /// queue. If the queue is full — which needs 64 unconsumed events, i.e. an audio thread that
    /// is not running — the event is dropped and logged rather than blocking the GUI.
    pub fn send_event(&self, direction: DeviceDirection, event: DspEvent) {
        match self.events.get(direction).try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => {
                log::warn!(
                    "dropping {event:?} for the {} lane: the audio thread is not draining its \
                     event queue",
                    direction.key()
                );
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// The most recent meters the lane's chain published.
    ///
    /// Each lane publishes through a buffer of its own, so the music chain's spectrum and the
    /// voice chain's gate reduction never overwrite each other. A lane that is not running keeps
    /// the last meters it published — [`Meters::default`] until it has run at all.
    ///
    /// Never blocks and never waits: one atomic swap. Returns the last published value again when
    /// nothing new has arrived, so a GUI that polls faster than the audio callback just redraws
    /// the same frame.
    #[must_use]
    pub fn meters(&mut self, direction: DeviceDirection) -> Meters {
        *self.meters.get_mut(direction).read()
    }

    /// Send a control-plane request. Non-blocking; wakes the PipeWire loop.
    pub fn send(&self, message: UiToAudio) {
        if self.engine.control.send(message).is_err() {
            log::warn!("the FxSound audio thread is gone; control message dropped");
        }
    }

    /// Take the next control-plane notification, if any. Never blocks.
    #[must_use]
    pub fn try_recv(&self) -> Option<AudioToUi> {
        self.notifications.try_recv().ok()
    }

    /// Stop the engine and wait for the PipeWire thread to finish tearing down.
    ///
    /// Tearing down in order matters: every session default FxSound holds — the sink, the source,
    /// or both — is handed back to a real device *before* the nodes are destroyed, so there is
    /// never a window in which a default points at a node that no longer exists
    /// (`docs/spec/12-audio-io.md` §21.5). The hand-back is confirmed by a server round trip
    /// before the socket closes — libpipewire does not flush on disconnect — so this returns only
    /// once `default.configured.audio.*` really has been rewritten, or after a short bounded wait
    /// when the server is not there to answer. It is therefore also what a `SIGTERM`/`SIGINT`
    /// handler in the application must reach before the process exits.
    pub fn shutdown(mut self) {
        self.engine.stop();
    }
}

#[cfg(test)]
mod graph_churn;

#[cfg(test)]
mod tests {
    use super::*;

    const fn assert_send<T: Send>() {}

    #[test]
    fn the_handle_can_be_moved_to_the_gui_thread() {
        assert_send::<EngineHandle>();
        assert_send::<AudioEngine>();
    }

    /// The task's central real-time requirement, asserted against the types rather than against
    /// behaviour: if the parameter path is a `triple_buffer::Input`/`Output` pair of a `Copy`
    /// payload, then by construction the audio thread's read is one atomic swap — no mutex, no
    /// allocation, no `Drop` to run.
    #[test]
    fn the_parameter_snapshot_path_is_wait_free_and_lock_free_by_construction() {
        fn param_channel_is_a_triple_buffer(handle: &mut EngineHandle) {
            let _: &mut Input<DspParams> = &mut handle.params;
            let _: &mut Input<InputDspParams> = &mut handle.input_params;
            // Both lanes, not just the one a single-lane engine used to have: each lane's meters
            // come back through a triple buffer of its own, and each lane's events go out through
            // a sender of its own.
            for direction in DeviceDirection::ALL {
                let _: &mut Output<Meters> = handle.meters.get_mut(direction);
                let _: &Sender<DspEvent> = handle.events.get(direction);
            }
        }
        let _ = param_channel_is_a_triple_buffer;

        // A bounded channel of non-zero capacity is crossbeam's array flavour: pre-allocated,
        // and `try_recv` on the audio thread never allocates and never parks. Checked per lane.
        let (handle, _config, _ready) = EngineHandle::wire(None, None);
        for (direction, events) in handle.events.iter() {
            assert_eq!(
                events.capacity(),
                Some(EVENT_QUEUE_LEN),
                "the {} lane's event queue must be a bounded array channel",
                direction.key()
            );
        }

        // A payload that owns heap memory would make the audio thread's `write`/`read` drop an
        // allocation. `Copy` rules that out for good. The microphone chain's snapshot is held to
        // the same standard as the music chain's — it reaches the same thread by the same path.
        const fn assert_copy<T: Copy>() {}
        assert_copy::<DspParams>();
        assert_copy::<InputDspParams>();
        assert_copy::<Meters>();

        // And the mechanism itself: the newest snapshot always arrives, and reading never blocks
        // even while the writer is hammering.
        let (mut input, mut output) = TripleBuffer::new(&DspParams::default()).split();
        let writer = std::thread::spawn(move || {
            for i in 0..10_000_u32 {
                input.write(DspParams {
                    master_gain_db: i as f32,
                    ..DspParams::default()
                });
            }
            input
        });
        let mut seen = 0_u32;
        for _ in 0..10_000 {
            seen = seen.max(output.read().master_gain_db as u32);
        }
        let mut input = writer.join().expect("writer thread");
        input.write(DspParams {
            master_gain_db: -1.0,
            ..DspParams::default()
        });
        assert_eq!(
            output.read().master_gain_db,
            -1.0,
            "the reader must always end up with the most recently written snapshot"
        );
        assert!(seen <= 9_999);
    }

    /// A handle wired to both lanes' DSP, as `engine::run` builds it, with no thread and no
    /// server: the GUI's ends and the audio thread's ends of the same paths.
    fn wired_lanes() -> (EngineHandle, PerDirection<lane_dsp::LaneDsp>) {
        let (handle, config, _ready) = EngineHandle::wire(None, None);
        let (lanes, _handover) = lane_dsp::build(
            config.params,
            config.input_params,
            config.meters,
            config.events,
        );
        (handle, lanes)
    }

    /// Run a block of a steady tone through a lane, so its counters have something to reset.
    fn run_a_block(dsp: &mut lane_dsp::LaneDsp) {
        let block: Vec<u8> = (0..512 * 2)
            .flat_map(|i| (0.25 * ((i / 2) as f32 * 0.05).sin()).to_le_bytes())
            .collect();
        dsp.refresh();
        dsp.process_bytes(&block, 2)
            .expect("a quantum of stereo fits the scratch");
    }

    #[test]
    fn an_event_sent_to_one_lane_is_never_consumed_by_the_other_lanes_engine() {
        let (handle, mut lanes) = wired_lanes();
        for (_, dsp) in lanes.iter_mut() {
            dsp.set_format(48_000.0, 2);
            run_a_block(dsp);
            assert!(dsp.meters().processed_samples > 0);
        }

        // A reset for the microphone. The music chain runs first and must leave it where it is.
        handle.send_event(DeviceDirection::Input, DspEvent::ResetProcessedTime);
        lanes.output.refresh();
        assert!(
            lanes.output.meters().processed_samples > 0,
            "the music chain consumed an event addressed to the voice chain"
        );
        lanes.input.refresh();
        assert_eq!(
            lanes.input.meters().processed_samples,
            0,
            "the voice chain never received its own event"
        );

        // And the other way round.
        run_a_block(&mut lanes.input);
        handle.send_event(DeviceDirection::Output, DspEvent::ResetProcessedTime);
        lanes.input.refresh();
        assert!(
            lanes.input.meters().processed_samples > 0,
            "the voice chain consumed an event addressed to the music chain"
        );
        lanes.output.refresh();
        assert_eq!(lanes.output.meters().processed_samples, 0);
    }

    #[test]
    fn an_event_that_fills_one_lanes_queue_leaves_the_other_lanes_queue_empty() {
        let (handle, config, _ready) = EngineHandle::wire(None, None);
        for _ in 0..EVENT_QUEUE_LEN {
            handle.send_event(DeviceDirection::Output, DspEvent::ResetFilterState);
        }
        assert!(config.events.output.is_full());
        assert!(
            config.events.input.is_empty(),
            "the voice chain's queue has room for its own events whatever the music chain's holds"
        );
        // Full is dropped and logged, never blocking the GUI, and still never spills over.
        handle.send_event(DeviceDirection::Output, DspEvent::ResetFilterState);
        assert!(config.events.input.is_empty());
        handle.send_event(DeviceDirection::Input, DspEvent::ResetCaptureStats);
        assert_eq!(config.events.input.len(), 1);
    }

    #[test]
    fn each_lanes_meters_are_published_through_its_own_buffer_and_do_not_overwrite_the_others() {
        let (mut handle, mut lanes) = wired_lanes();
        assert_eq!(handle.meters(DeviceDirection::Output), Meters::default());
        assert_eq!(handle.meters(DeviceDirection::Input), Meters::default());

        // Two rates no default could be mistaken for, one per lane.
        lanes.output.set_format(44_100.0, 2);
        lanes.input.set_format(96_000.0, 2);
        lanes.output.publish_meters();
        assert_eq!(handle.meters(DeviceDirection::Output).sample_rate, 44_100);
        assert_eq!(
            handle.meters(DeviceDirection::Input),
            Meters::default(),
            "the music chain's meters reached the voice chain's buffer"
        );

        lanes.input.publish_meters();
        assert_eq!(handle.meters(DeviceDirection::Input).sample_rate, 96_000);
        assert_eq!(
            handle.meters(DeviceDirection::Output).sample_rate,
            44_100,
            "the voice chain's meters overwrote the music chain's"
        );

        // A lane that publishes again moves only its own reading.
        lanes.output.set_format(48_000.0, 2);
        lanes.output.publish_meters();
        assert_eq!(handle.meters(DeviceDirection::Output).sample_rate, 48_000);
        assert_eq!(handle.meters(DeviceDirection::Input).sample_rate, 96_000);
    }

    #[test]
    fn the_buffer_length_setting_maps_onto_power_of_two_quanta() {
        // The Windows range, `sndDevices.h:200-201`.
        assert_eq!(quantum_for_ms(10, 48_000), 256);
        assert_eq!(quantum_for_ms(100, 48_000), 2048);
        // Out-of-range values clamp exactly as `sndDevicesSetBufferSizeMilliSecs` does.
        assert_eq!(quantum_for_ms(0, 48_000), quantum_for_ms(10, 48_000));
        assert_eq!(quantum_for_ms(10_000, 48_000), quantum_for_ms(100, 48_000));
        // The compiled-in Windows default of 80 ms lands on the spec's `Safe` row.
        assert_eq!(quantum_for_ms(80, 48_000), 2048);
        // Every result is a power of two inside the range the ring is sized for.
        for ms in 1..=200 {
            for rate in [44_100, 48_000, 96_000, 192_000] {
                let q = quantum_for_ms(ms, rate);
                assert!(q.is_power_of_two(), "{ms}ms @ {rate}Hz gave {q}");
                assert!((256..=MAX_QUANTUM_FRAMES as u32).contains(&q));
            }
        }
    }

    #[test]
    fn the_node_names_the_default_metadata_will_carry_are_stable() {
        assert_eq!(SINK_NODE_NAME, "fxsound_sink");
        assert_eq!(OUTPUT_NODE_NAME, "fxsound_output");
        assert_eq!(CAPTURE_NODE_NAME, "fxsound_capture");
        assert_eq!(SOURCE_NODE_NAME, "fxsound_source");
        assert_eq!(LINK_GROUP, "fxsound");
        assert_eq!(link_group(DeviceDirection::Output), LINK_GROUP);
        assert_eq!(link_group(DeviceDirection::Input), INPUT_LINK_GROUP);
        assert_ne!(
            LINK_GROUP, INPUT_LINK_GROUP,
            "one group for both lanes keeps the speakers running for as long as the microphone is"
        );
        assert_eq!(our_node_name(DeviceDirection::Output), SINK_NODE_NAME);
        assert_eq!(our_node_name(DeviceDirection::Input), SOURCE_NODE_NAME);
        // The echo canceller's three, which are ours as much as the lanes' four: never a device.
        assert_eq!(AEC_CAPTURE_NODE_NAME, "fxsound_aec_capture");
        assert_eq!(AEC_MONITOR_NODE_NAME, "fxsound_aec_monitor");
        assert_eq!(AEC_SOURCE_NODE_NAME, "fxsound_aec_source");
        assert_eq!(AEC_LINK_GROUP, "fxsound-aec");
        assert_eq!(
            OUR_NODE_NAMES.to_vec(),
            [LANE_NODE_NAMES.as_slice(), AEC_NODE_NAMES.as_slice()].concat(),
            "every node of ours is either a lane's or the canceller's"
        );
        // Names are matched by string and written into metadata: ASCII, no spaces, all distinct.
        for name in OUR_NODE_NAMES {
            assert!(name.is_ascii() && !name.contains(' '), "{name}");
        }
        let mut sorted = OUR_NODE_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), OUR_NODE_NAMES.len());
        // The descriptions, by contrast, are for people and carry the localised direction.
        assert_eq!(SINK_DESCRIPTION, "FxSound");
        assert_eq!(OUTPUT_STREAM_DESCRIPTION, "FxSound output");
        assert_eq!(CAPTURE_STREAM_DESCRIPTION, "FxSound capture");
        assert_eq!(
            node_description(DeviceDirection::Output, Some("ru")),
            "FxSound (Вывод)"
        );
        assert_eq!(
            node_description(DeviceDirection::Input, Some("ru")),
            "FxSound (Ввод)"
        );
    }

    /// Constructing against a socket that does not exist must return an error rather than panic,
    /// and must not leave a thread or a node behind. Nothing in this test touches the live graph.
    #[test]
    fn starting_without_a_pipewire_server_returns_an_error_instead_of_panicking() {
        let error = AudioEngine::start_with_remote(Some("fxsound-no-such-socket-for-tests"))
            .expect_err("connecting to a socket that does not exist must fail");
        let AudioError::PipewireUnavailable(detail) = &error else {
            panic!("expected PipewireUnavailable, got {error:?}");
        };
        assert!(
            !detail.is_empty(),
            "the message shown to the user must say something"
        );
        assert!(error.to_string().starts_with("PipeWire is not available"));
    }

    #[test]
    fn a_missing_runtime_directory_is_reported_rather_than_retried_forever() {
        use std::ffi::OsStr;

        assert!(matches!(
            engine::preflight(None),
            Err(AudioError::PipewireUnavailable(_))
        ));
        assert!(matches!(
            engine::preflight(Some(OsStr::new(""))),
            Err(AudioError::PipewireUnavailable(_))
        ));
        assert!(matches!(
            engine::preflight(Some(OsStr::new("/definitely/not/a/runtime/dir"))),
            Err(AudioError::PipewireUnavailable(_))
        ));
        // A directory that does exist passes; whether a server is listening in it is
        // `pw_context_connect`'s business.
        assert!(engine::preflight(Some(OsStr::new("/tmp"))).is_ok());
    }
}
