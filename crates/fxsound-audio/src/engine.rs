//! The PipeWire thread: the two nodes, the DSP process callback, and the session default.
//!
//! Everything in this module runs on one thread that [`crate::AudioEngine::start`] spawns, because
//! nothing in `pipewire` or `libspa` is `Send` — a repo-wide grep for `unsafe impl Send` in
//! `pipewire-0.10.1/src/` returns zero hits. Within that thread there are two execution contexts
//! and the difference between them is the whole safety story of this file:
//!
//! * **The main loop.** Registry events, format negotiation, node creation, metadata writes, the
//!   200 ms supervisor tick. May allocate, log and block.
//! * **The data thread.** The two `process()` callbacks, put there by
//!   [`StreamFlags::RT_PROCESS`]. `module-rt` has already promoted it to `SCHED_FIFO` through
//!   RTKit, so a `malloc`, a mutex or a panic here does not degrade gracefully — it xruns the
//!   whole graph or aborts the process and takes the user's audio with it.
//!
//! The two never share a `RefCell`. The main loop's mutable state lives in [`Shared`], which no
//! `process()` closure can reach; the data thread's state lives in the streams' user data, and
//! the only things that cross between them are, per lane, a wait-free ring of `AtomicU32`
//! samples, a handful of counters, and the lane's own paths to the GUI — the `triple_buffer`
//! endpoints that carry its parameters in and its meters out, and its bounded event queue
//! (`crate::lane_dsp`).
//!
//! # One engine, two lanes
//!
//! The engine keeps one [`Lane`] per [`DeviceDirection`] (`docs/0.4.0-design.md` §1.1), and the
//! two run at the same time. In the output lane NODE 1 is the virtual sink and NODE 2 the
//! playback stream; in the input lane NODE 1 is a capture stream on the chosen microphone and
//! NODE 2 the virtual source (`crate` docs, "Topology"). Each lane owns everything its pair
//! touches — the ring, the counters, the stream flags, the DSP — and everything the main loop
//! decides about it: whether it is enabled, its backoff, its error, its claim on the session
//! default, what the GUI was last told. So a microphone that fails backs off on its own while the
//! speakers keep playing, and nothing one lane does can be undone by the other.
//!
//! Only the node *properties*, the metadata keys and the bookkeeping differ between the lanes:
//! NODE 1 always runs [`on_sink_process`] (DSP in place, push to the ring) and NODE 2 always runs
//! [`on_output_process`] (pop the ring), so the real-time code is the same in both lanes and
//! nothing in it knows there is another one.
//!
//! # The 200 ms supervisor
//!
//! `AudioPassthruPrivate::processTimer` (`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:359`)
//! polled every 100 ms because Windows gave it no event for "the processing thread died". PipeWire
//! gives us events for everything, so the timer here does much less: it drains counters, publishes
//! changes to the GUI, and drives the reconnect state machine — the socket's, and each lane's own.
//! It is still a timer rather than pure event handling for one reason — it is the natural home for
//! the "don't hammer" backoff of `docs/spec/12-audio-io.md` §22, the direct descendant of the
//! `INVALID_HANDLE_VALUE` pause sentinel at `AudioPassthruPrivate.cpp:452-460`.
//!
//! # Idle
//!
//! The output lane's NODE 2 is a playback stream linked to the user's real speakers, and a linked
//! stream keeps its device running — so in 0.3.0, FxSound kept the speakers, the graph and both of
//! its own process callbacks running for as long as it was open, playing silence into a device
//! WirePlumber would otherwise have suspended (`docs/0.4.0-design.md` §12). NODE 2 has nothing to
//! play unless something plays into NODE 1, so it should run exactly when NODE 1 does, and there
//! are two ways to get that, depending on the server:
//!
//! * **PipeWire 0.3.68 and later** schedules the members of a `node.link-group` together: a node
//!   made runnable by a link of its own makes the rest of its group runnable too (`run_nodes` in
//!   `src/pipewire/context.c`). So NODE 2 is declared `node.passive`: its link to the speakers no
//!   longer keeps them awake, and it runs whenever an application's link into NODE 1 makes NODE 1
//!   run — in the same cycle, with no help from this thread. It is `module-loopback`'s virtual-sink
//!   recipe, and WirePlumber's own audio-group loopback is built the same way.
//! * **Older servers** knew the link-group only as a hint to WirePlumber, so a passive NODE 2 would
//!   never be woken by NODE 1's clients: FxSound would fall silent. There NODE 2 stays an ordinary
//!   stream and the main loop paces it by hand, as `docs/0.4.0-design.md` §1.3 describes —
//!   `set_active(false)` once NODE 1 has been paused for `SLEEP_AFTER`, `set_active(true)` the
//!   moment it streams again (`SecondNodePace`). That only works on a server this old: on a newer
//!   one, a NODE 2 that is not passive and runs keeps its whole group running, NODE 1 included,
//!   so NODE 1 would never pause and NODE 2 never be put to sleep.
//!
//! Either way the ring re-primes, and the next sound starts the way a pair's first sound does:
//! from an empty ring that fills to its target before anything is heard. A NODE 2 paced by hand
//! gets there by itself, playing the ring dry while it waits out `SLEEP_AFTER`. A passive one
//! cannot. It stops in the same cycle as NODE 1, with the ring's cushion — the block and a
//! half NODE 1 had processed and NODE 2 not yet played — still in it, and it next runs whenever
//! something plays into NODE 1 again: minutes later, perhaps, and another application. Played
//! then, the cushion would put the tail of the last sound in front of the next one. So NODE 1's
//! `Paused` marks the ring stale, and NODE 2's first `pop` after it skips what was left
//! (`SampleRing::mark_stale`). What that costs is the same block and a half of a player that was
//! merely paused and resumed — 16 ms at the quantum FxSound asks for — which it resumes that much
//! further on than it stopped.
//!
//! The input lane idles the same way with the two nodes' parts swapped (`docs/0.4.0-upstream.md`
//! U19). Its device-facing stream is NODE 1, the capture stream, and on a server that runs a
//! link-group together that stream is passive: the microphone's link to it no longer keeps the
//! microphone running. A recorder's link to NODE 2, the virtual source, makes the source runnable,
//! the group makes the capture stream runnable with it, and the capture stream's link makes the
//! microphone run — all in one cycle, as on the output side. Measured on PipeWire 1.6.8 in a
//! private daemon
//! (`graph_churn::sleep::a_microphone_runs_only_while_something_records_from_fxsound_input`):
//! with nothing recording, the capture stream and the microphone stay idle; with a recorder linked,
//! both run and the recorder gets the processed microphone; unlinked, both are idle again. So the
//! microphone — its light, a desktop's "recording" indicator, a Bluetooth headset's call profile —
//! is held only while something records from FxSound (Input). It is how WirePlumber holds its own
//! Bluetooth microphone, whose capture half is passive in the same way
//! (`monitors/bluez/create-loopback-node.lua`). The ring is marked stale when the capture stream
//! pauses, as above, so a recording started later does not open with the end of the last one.
//!
//! On an older server a passive capture stream would never be woken by a recorder, and it stays an
//! ordinary stream that holds the microphone for as long as the lane is on, as it did everywhere
//! before 0.4.0. Pacing it by hand, the output lane's answer there, would keep it inactive until
//! the source reported `Streaming`, so every recording would start with the blocks the capture
//! stream missed while it was being woken; and every distribution FxSound is packaged for ships a
//! server that does not need it.
//!
//! What records from the source is usually an application. While the app holds the microphone
//! awake ([`UiToAudio::KeepInputAwake`]: the calibration wizard, the microphone meters) it is the
//! engine itself: a recording stream of its own on the source ([`KeepAwake`]), which runs the pair
//! and the microphone exactly as an application would. It is also the one thing that wakes a
//! Bluetooth headset's microphone. WirePlumber 0.5 switches a headset to its call profile only for
//! a recording stream with no `node.link-group` that reaches the headset's loopback microphone
//! (`device/autoswitch-bluetooth-profile.lua`, `isBluetoothLoopbackSourceNodeLinkedToStream`),
//! through filters if need be, and the capture stream carries the input lane's group. The stream of
//! our own carries none, and is found through the source's group to the capture stream and on to
//! the headset, the way a call recording from FxSound (Input) is. Taking the group off the capture
//! stream instead would have woken the headset too, but the group is what keeps WirePlumber from
//! linking the capture stream to our own source (`docs/spec/12-audio-io.md` §28.3), and a pair
//! would have had to be rebuilt to change it.
//!
//! All of this holds while echo cancellation is on, too. The canceller records the speakers'
//! monitor on the microphone's clock, so while it runs the speakers, the microphone and the output
//! pair run with it (`docs/0.4.0-design.md` §7, and `aec`). But PipeWire's module makes its own
//! capture and monitor streams passive, and the capture stream that records the canceller's source
//! is passive as above, so nothing in that chain runs it by itself: it runs while something records
//! from FxSound (Input), and sleeps, canceller loaded, when the recording stops
//! (`graph_churn::echo_cancellation_holds_nothing_awake_while_nothing_records_from_fxsound_input`).
//! Before U19 the capture stream's link to the canceller ran all of it for as long as the lane was
//! on.
//!
//! # Volume
//!
//! What a desktop's slider sets on a lane's virtual node is applied by the lane's DSP after its
//! chain — the part above unity in front of it — and remembered per real device and port
//! (`crate::volume`, `docs/spec/12-audio-io.md` §19.7.1). The main loop's part is small and lives
//! in one section below: read every `Props` write in the virtual node's `param_changed`
//! ([`take_props`]), report the volume once it holds still ([`watch_volume`]), choose the volume a
//! new pair starts at ([`volume_for_pair`]) — or a pair whose device moved to another port
//! ([`follow_port`]) — and write it to the node through the session's second connection
//! ([`publish_volume`]).
//!
//! # A device that blinks
//!
//! A Bluetooth headset switches between A2DP and its call profile whenever something starts or
//! stops recording from it (WirePlumber 0.5.17, `device/autoswitch-bluetooth-profile.lua`), and
//! every switch removes its sink and adds it back under the same `node.name` about half a second
//! later; an ALSA card switched to another profile does the same to its nodes. In 0.3.0 both halves
//! of the graph reacted to the gap. WirePlumber moved the playback stream onto the fallback device
//! by itself, and the next supervisor tick found the target gone and rebuilt the pair on whatever
//! else there was — the laptop's speakers, for the length of the switch (`docs/0.4.0-upstream.md`,
//! U8). Now neither does:
//!
//! * Each lane's device-facing stream is `node.dont-reconnect`, `node.dont-fallback` and
//!   `node.linger` ([`stream_props`]): WirePlumber links it to its target once, never moves it,
//!   and leaves it unlinked and waiting while the target is gone.
//! * When a lane's target goes while the card it belongs to — PipeWire's `Device` object — stays,
//!   the lane's rules wait up to [`RETURN_WAIT`] for it to come back ([`Hold`]) — unless the card
//!   adds a node of the lane's direction under another name meanwhile, which is the target renamed
//!   by the profile switch, and ends the wait ([`Hold::renamed`]).
//! * Any node that goes while its card stays counts as seen by the rules of its direction until
//!   [`RETURN_WAIT`] is up, whichever lane was on it ([`Departure`]): back under its name, it is no
//!   device just plugged in, and takes no lane from the device it is on.
//! * A pair is attached only to the very node it was built on, `object.serial` and all
//!   ([`is_same_node`]). WirePlumber never links a stream it has linked once, so a node back under
//!   its old name gets a new pair, and the new pair's stream is linked to it.
//!
//! A node that goes with its card, or that has no card — a virtual sink — is gone, and the lane
//! moves at once, as before: on the tick after the node goes, or, when the registry names the node
//! before its card, on the tick after the card does ([`remove_card`]). So does a lane whose wait
//! is up.
//!
//! # Sleep
//!
//! The app tells the engine when logind says the system is about to sleep and when it has woken
//! ([`UiToAudio::SystemSleeping`], `docs/0.4.0-upstream.md` U13), as upstream's controller mutes
//! its engine on suspend and unmutes it on resume (`FxController.cpp:2145-2159`). Going to sleep,
//! both lanes fall silent after their chains — the snapshots' own `mute` path, set from here
//! rather than by the GUI ([`Lane::system_mute`]) — and no device rules run, so nothing chooses a
//! device from a graph that is coming apart under the suspend. On waking, both chains' filter
//! history is cleared, and every per-application route's with them (see "Applications"), and
//! both lanes' rules run with the wait of "A device that blinks" in front
//! of them: a lane whose device is not back yet waits up to [`RETURN_WAIT`] for it
//! ([`Lane::wake_wait`]), card or no card, because a Bluetooth headset reconnects a few seconds
//! after the system does. A lane is heard again once its rules have attached it, or after
//! [`WAKE_MUTE`] at the latest. A wake that never comes is given up after [`SLEEP_LIMIT`], so a
//! lost signal cannot leave FxSound silent. There is no sleep inhibitor, by upstream's decision
//! (its PR #533).
//!
//! # Applications
//!
//! Beside the devices, the registry announces every application that plays or records, and the
//! engine keeps them for the per-application presets (`docs/0.4.0-apps.md`, `crate::app_streams`):
//! each player's and recorder's stream, bound for the properties its registry global leaves out —
//! its binary, its target, whether it may move — and each client, bound for what its streams do
//! not say about themselves. FxSound's own streams are left out by name. The GUI hears the list on
//! the supervisor's tick, whole, and only when it has changed ([`AudioToUi::AppStreams`]), so an
//! application that starts is one message however many events announced it.
//!
//! An application the app has given a preset of its own ([`UiToAudio::SetAppRoutes`]) is moved
//! onto a *route*: another pair of FxSound's nodes on its lane's device, running that preset
//! (`route_pairs`, planned by `crate::app_routes`). The move is a `target.object` key in the
//! `default` metadata, which WirePlumber follows; deleting it moves the application back. Routes
//! follow their lanes — built beside a lane's pair, rebuilt with it on another device, taken down
//! with it — and go once nothing has used them for a while. On the way out the keys are deleted
//! with the hand-back of the defaults, confirmed by the same `sync`, before any node goes.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use fxsound_core::messages::{
    AudioToUi, DspEvent, DspParams, InputDspParams, Meters, TargetVolume, UiToAudio,
};
use fxsound_core::{AudioDevice, AudioStatus, DeviceDirection};
use fxsound_dsp::{ChainSpec, InputEngine};
use libspa::param::audio::{AudioFormat, AudioInfoRaw};
use libspa::param::format::{MediaSubtype, MediaType};
use libspa::param::format_utils;
use libspa::pod::Pod;
use libspa::utils::result::AsyncSeq;
use pipewire as pw;
use pw::properties::{PropertiesBox, properties};
use pw::stream::{StreamFlags, StreamState};
use triple_buffer::{Input, Output};

use crate::aec::{self, EchoCancel, Side};
use crate::app_streams::{self, AppStreams, StreamNode, Tracked};
use crate::devices::{
    self, BluezFacts, Card, ChannelMap, DeviceInfo, FormFactor, Preference, SelectionMemory,
};
use crate::lane_dsp::{self, ChainHandover, LaneDsp};
use crate::per_direction::PerDirection;
use crate::routes::{self, CardRoutes, Route, RouteList};
use crate::volume::{self, Debounce, LaneVolume, NodeVolume, PropsUpdate};
use crate::{
    AEC_SOURCE_NODE_NAME, AudioError, CAPTURE_NODE_NAME, CAPTURE_STREAM_DESCRIPTION,
    DEFAULT_QUANTUM_FRAMES, DEFAULT_SAMPLE_RATE, KEEP_AWAKE_NODE_NAME,
    KEEP_AWAKE_STREAM_DESCRIPTION, MAX_CHANNELS, MAX_QUANTUM_FRAMES, MIN_CHANNELS,
    ONE_HEADSET_ON_BOTH_LANES, OUTPUT_NODE_NAME, OUTPUT_STREAM_DESCRIPTION, RING_CAPACITY_FRAMES,
    SINK_DESCRIPTION, SINK_NODE_NAME, SOURCE_NODE_NAME, is_fxsound_node, link_group, locale,
    our_node_name,
};

/// The rate the capture stream asks for, whatever the microphone runs at.
///
/// RNNoise exists at 48 kHz and nowhere else, and it sits in front of every stage that measures a
/// level — so a preset voiced with it on means something different at any other rate. Asking for
/// one rate and letting PipeWire resample is what makes a voice preset mean one thing everywhere.
const CAPTURE_RATE: u32 = DEFAULT_SAMPLE_RATE;

/// How often the supervisor runs. Also the shortest possible reconnect interval, which is the
/// "never retry faster than 200 ms" floor of `docs/spec/12-audio-io.md` §22.
pub(crate) const SUPERVISOR_PERIOD: Duration = Duration::from_millis(200);

/// How many supervisor ticks — five seconds of them — a claim on a default that no lane stands
/// behind waits for the GUI to take the device list before it is handed back regardless
/// ([`Shared::gui_has_had_its_chance`]). A GUI takes it within a tick of its loop; this is for an
/// engine nobody reads from, which has no lane to attach on its behalf and nothing to wait for.
const GUI_PATIENCE_TICKS: u32 = 25;

/// Retry backoff in milliseconds, then flat at the last value. The socket and each lane keep their
/// own count against it.
const BACKOFF_MS: [u64; 6] = [200, 400, 800, 1600, 3200, 5000];

/// How long to wait before the next try, after `attempts` failed ones.
fn backoff(attempts: u32) -> Duration {
    let ms = BACKOFF_MS
        .get(attempts as usize)
        .copied()
        .unwrap_or(BACKOFF_MS[BACKOFF_MS.len() - 1]);
    Duration::from_millis(ms)
}

/// How long the exit path waits for the server to acknowledge the hand-back of the session
/// default before closing the socket regardless. A live server answers a `sync` in well under a
/// millisecond; this only ever elapses when the server is gone, and then there is nothing left
/// to hand the default back on.
const RELEASE_TIMEOUT: Duration = Duration::from_millis(300);

/// Sequence number of the `sync` that confirms the exit hand-back. **Must differ from the `0`
/// [`connect`] uses** for the registry barrier: `done` events carry their `sync`'s number back,
/// and an unanswered registry sync — shutdown right after a reconnect — would otherwise pass for
/// the confirmation before the metadata write had reached the server.
const RELEASE_SEQ: i32 = 1;

/// Sequence number of the `sync` put behind the binds the registry's first dump called for
/// ([`Barrier::Metadata`]), apart from the registry's `0` and [`RELEASE_SEQ`] as those two are
/// apart from each other. The barrier waits for the number the `sync` returned, which is the one
/// its `done` carries back.
const METADATA_SEQ: i32 = 2;

/// How long the output lane's NODE 1 has to have been paused before its NODE 2 is put to sleep,
/// on a server that does not do that by itself ([`SecondNodePace`]).
///
/// Not at once, for two reasons. NODE 2 still has the ring's tail to play when NODE 1 stops —
/// up to four target fills, a quarter of a second at the largest quantum — and put to sleep on top
/// of it, it would play that stale tail as the first thing it hears when it wakes. And a player
/// that closes its stream at the end of a track and opens another for the next one pauses NODE 1
/// for a moment it has no business sleeping through. A second is long enough for both and short
/// beside the five seconds WirePlumber waits before it suspends the speakers themselves.
pub(crate) const SLEEP_AFTER: Duration = Duration::from_secs(1);

/// The first PipeWire whose scheduler runs the members of a `node.link-group` together, which is
/// what lets the output lane's NODE 2 be passive (module docs, "Idle"). Found by reading
/// `src/pipewire/context.c` release by release: 0.3.67 has no `run_nodes` at all, 0.3.68 has it
/// with the link-group walk that every release since has kept.
const LINK_GROUPS_SCHEDULED_SINCE: (u32, u32, u32) = (0, 3, 68);

/// Target ring fill, in quanta. 1.5 × quantum per `docs/spec/12-audio-io.md` §19.6; expressed in
/// halves so it stays integer arithmetic.
const TARGET_FILL_HALF_QUANTA: usize = 3;

/// `priority.session` for the virtual sink and the virtual source.
///
/// Deliberately **below** a typical ALSA node (the internal analogue sink on the development
/// machine advertises `1009`). The spec's §20 table suggests `1010`, which would let policy
/// auto-select FxSound on a machine where the user has never picked a default — and its own open
/// question 2 then recommends `500` instead, "never wins implicitly". Given that `CLAUDE.md`
/// treats silently changing the user's audio routing as the worst failure this subsystem can
/// have, `500` is the right value: FxSound becomes the default only through the explicit,
/// reversible metadata write in [`claim_default`], never through WirePlumber's own ranking.
const NODE_PRIORITY_SESSION: &str = "500";

/// How long a lane waits for the node it is attached to, when that node goes and its card stays
/// ([`Hold`]).
///
/// Long enough for what a Bluetooth headset does under WirePlumber 0.5.17 every time something
/// starts or stops recording from it: its sink is removed and comes back under the same name once
/// the new profile's link is up, about half a second later (`docs/0.4.0-upstream.md`, U8). Short
/// enough that a node that really has gone for good — a card switched by the user to a profile
/// with no output, an HDMI port whose screen went to sleep — leaves the lane playing into nothing
/// for no longer than a user would wait before reaching for the volume.
pub(crate) const RETURN_WAIT: Duration = Duration::from_millis(2500);

/// How long a lane stays silent after the system wakes when its rules have not attached it by then
/// (module docs, "Sleep"; `docs/0.4.0-upstream.md` U13).
///
/// A lane whose device is back at once is heard again the tick its rules have run — the next
/// supervisor tick, within a fifth of a second of the wake. One still waiting for its device is
/// heard after this: shorter than [`RETURN_WAIT`], because what it would hold back is a pair
/// playing into a device that is not there yet, which is silent anyway, and a user who pressed a
/// key to wake the machine to music should not wait on a headset that may never come back.
pub(crate) const WAKE_MUTE: Duration = Duration::from_secs(2);

/// How long the system may say it is going to sleep without sleeping, or waking, before the engine
/// stops believing it (module docs, "Sleep").
///
/// Measured on the monotonic clock, which stands still while the system is suspended, so a sleep
/// of any length costs nothing against it: all it counts is the time the system spends awake
/// between logind's two signals. logind gives a delay inhibitor five seconds by default
/// (`InhibitDelayMaxSec`) before it suspends, and the signal that the system has woken follows
/// the wake at once. So this only runs out when that second signal is lost on its way — a suspend
/// that failed and a bus or an app that missed saying so — and then it is what keeps FxSound from
/// staying silent and its device rules from staying frozen until the next sleep.
pub(crate) const SLEEP_LIMIT: Duration = Duration::from_secs(60);

/// A lane waiting for the node it was attached to (`docs/0.4.0-upstream.md`, U8): the node went,
/// but the card it belongs to stayed, so it is a card between profiles and the node is expected
/// back under its name. Until it is, until [`RETURN_WAIT`] is up, or until the card goes after all
/// ([`remove_card`]), the lane's rules do not run, and nothing chooses another device for it.
///
/// Without this, the next supervisor tick found the target gone and moved the lane to whatever
/// else there was — for a Bluetooth headset switching to its call profile, the laptop's speakers,
/// for the half second the switch takes and then back. With it the lane's pair stays as it is:
/// its device-facing stream is left unlinked and waiting by WirePlumber, which neither moves it
/// nor destroys it ([`stream_props`]), and when the node comes back the rules find it under a
/// serial the pair was not built on and rebuild on it ([`is_same_node`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hold {
    /// `node.name` of the node the lane waits for.
    target: String,
    /// When the lane stops waiting and chooses another device.
    until: Instant,
    /// The node's `device.id` and `api.bluez5.address`, kept from the node that went: what ties
    /// the wait to its card ([`Self::card_present`]).
    card_id: Option<u32>,
    bluez_address: Option<String>,
    /// The names of the card's other nodes of the lane's direction that it had when the node went:
    /// those still listed, and those that went a moment before it and are expected back
    /// ([`Departure::expected`]). What the card had besides it, so that a node it adds afterwards
    /// can be told from one it merely puts back ([`Self::renamed`]).
    known: Vec<String>,
}

impl Hold {
    /// What a lane whose pair is attached to `attached_to` does when `removed` goes, with `cards`
    /// still in the graph, `devices` the nodes left and `departed` the lane's record of the nodes
    /// of its direction that went before it ([`Lane::departures`]): wait for it, or `None` to
    /// choose another device at once.
    ///
    /// Only the lane's own target is waited for — any other node going is news for the rules as
    /// usual — and only when its card stayed ([`DeviceInfo::card_present`]). A hold already running
    /// for the same node keeps its deadline, so a node that comes and goes faster than the rules
    /// run cannot keep the lane waiting for ever.
    ///
    /// What the card had besides the node ([`Self::known`]) is read from both lists, because a
    /// card between profiles removes its nodes one at a time, in the order of its devices: the
    /// nodes before the lane's are already gone when the lane's goes, and only its departures still
    /// name them. Read from `devices` alone, a lane on a UCM card's headphones or HDMI output knew
    /// nothing of the speakers that went just before, took them coming back for its own node
    /// renamed, and was moved to another device while its own was still on its way.
    fn after_removal(
        removed: &DeviceInfo,
        attached_to: Option<&str>,
        cards: &[Card],
        devices: &[DeviceInfo],
        departed: &[Departure],
        running: Option<&Self>,
        now: Instant,
    ) -> Option<Self> {
        if attached_to != Some(removed.name.as_str()) || !removed.card_present(cards) {
            return None;
        }
        if let Some(running) = running
            && running.target == removed.name
            && now < running.until
        {
            return Some(running.clone());
        }
        let mut hold = Self {
            target: removed.name.clone(),
            until: now + RETURN_WAIT,
            card_id: removed.card_id,
            bluez_address: removed.bluez_address.clone(),
            known: Vec::new(),
        };
        let listed = devices
            .iter()
            .filter(|device| hold.is_sibling(device, removed.direction))
            .map(|device| device.name.clone());
        let expected = departed
            .iter()
            .filter(|departure| departure.expected(now) && hold.left_the_card(departure))
            .map(|departure| departure.name.clone());
        let mut known: Vec<String> = Vec::new();
        for name in listed.chain(expected) {
            if !known.contains(&name) {
                known.push(name);
            }
        }
        hold.known = known;
        Some(hold)
    }

    /// Whether the lane still waits: its node is not back, and the wait is not up.
    fn waits(&self, now: Instant, back: bool) -> bool {
        !back && now < self.until
    }

    /// A node of the card the lane waits on, of the lane's `direction`, that the card has added
    /// since the node went — the node back under another name — among `devices`.
    ///
    /// An ALSA card switched to another profile removes its nodes and adds the new profile's, and
    /// a node whose path changed comes back renamed: `analog-stereo` as `analog-surround-51`, the
    /// speakers as the HDMI output. Its old name never returns, and a lane that waited for it would
    /// sit on an unlinked stream, silent, for the whole of [`RETURN_WAIT`]. Once the card shows a
    /// node of the lane's direction it did not have when the node went, it is not coming back as
    /// it was: the wait ends and the rules run on the device list as it is, where the renamed node
    /// counts as a device just plugged in — rule 5 unranked, or its place in the ranking, which for
    /// a name the ranking does not hold is after every ranked device unless new devices go first.
    /// So the renamed node takes the lane only when nothing the rules put above it is listed; a
    /// ranking that holds a device on another card sends the lane there.
    ///
    /// A node the card had already — a UCM card's other sinks, which a profile applied again
    /// removes and puts back one by one under their own names, before the lane's node or after
    /// it ([`Self::after_removal`]) — is no rename, and leaves the lane waiting for its own.
    fn renamed<'d>(
        &self,
        devices: &'d [DeviceInfo],
        direction: DeviceDirection,
    ) -> Option<&'d DeviceInfo> {
        devices
            .iter()
            .find(|device| self.is_sibling(device, direction) && !self.known.contains(&device.name))
    }

    /// Whether `device` is another node of `direction` on the card the waited-for node belonged
    /// to — tied to it by `device.id` or by the Bluetooth address, as a card is ([`Card::owns`]).
    fn is_sibling(&self, device: &DeviceInfo, direction: DeviceDirection) -> bool {
        device.direction == direction
            && self.ties(
                &device.name,
                device.card_id,
                device.bluez_address.as_deref(),
            )
    }

    /// Whether `departure` is another node that went from the card the waited-for node belonged
    /// to, tied to it as [`Self::is_sibling`] ties one still listed. It is one of the lane's
    /// departures, so it is of the lane's direction already.
    fn left_the_card(&self, departure: &Departure) -> bool {
        self.ties(
            &departure.name,
            departure.card_id,
            departure.bluez_address.as_deref(),
        )
    }

    /// Whether the node `name`, of the card `card_id` or the Bluetooth device `bluez_address`, is
    /// not the waited-for node but is of its card.
    fn ties(&self, name: &str, card_id: Option<u32>, bluez_address: Option<&str>) -> bool {
        name != self.target
            && ((self.card_id.is_some() && card_id == self.card_id)
                || (self.bluez_address.is_some() && bluez_address == self.bluez_address.as_deref()))
    }

    /// Whether the card the node belonged to is still among `cards`, asked as the node was
    /// ([`DeviceInfo::card_present`]).
    ///
    /// A hold begins only with its card there, so this turns false only when the card goes — and a
    /// card that goes after its node is no card between profiles. It is a headset switched off, a
    /// USB card pulled out, and the node is not coming back. The two go in one batch of registry
    /// events, and the batch can name the node first: a Bluetooth device's nodes go as its
    /// transports are released, before the device itself. Then the node's going finds the card
    /// still listed and begins a hold that only the card's going can show to be wrong. (Named the
    /// other way round, the node finds no card and no hold begins.)
    fn card_present(&self, cards: &[Card]) -> bool {
        cards
            .iter()
            .any(|card| card.owns(self.card_id, self.bluez_address.as_deref()))
    }
}

/// A node that went while its card stayed, whichever lane — if any — was on it
/// (`docs/0.4.0-upstream.md`, U8): a card between profiles, the node expected back under its name.
///
/// A [`Hold`] keeps the lane that was attached to the node from choosing another device; this
/// keeps the node from being taken for a new one when it comes back, by the lane of its direction
/// whether or not that lane was on it. Every run of a lane's rules replaces what they remember as
/// seen (`Lane::previous_names`), and a run between the node's going and its coming back — the
/// supervisor's tick is 200 ms, a Bluetooth profile switch takes about a second — would forget it:
/// back, it would count as a device just plugged in, and one ranked above the lane's device, or any
/// device at all when nothing is ranked and nothing picked, takes the lane. A headset switching to
/// its call profile would take the music from the speakers the user picked, in mono at 16 kHz, on
/// every call. On Windows the endpoints stay through a profile switch, and upstream never sees an
/// arrival. So a departed node is counted as seen until [`RETURN_WAIT`] is up
/// ([`Self::expected`]); one that stays away longer, and then comes, has arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Departure {
    /// `node.name` of the node that went.
    name: String,
    /// When it went.
    left: Instant,
    /// When it came back under its name, while it was still expected.
    back: Option<Instant>,
    /// Its `device.id` and `api.bluez5.address`, as a [`Hold`] keeps them: a card that goes after
    /// its node takes the departure with it ([`remove_card`]).
    card_id: Option<u32>,
    bluez_address: Option<String>,
}

impl Departure {
    /// The departure of `removed`, when its card is still among `cards`. `None` otherwise: a node
    /// that goes with its card, or on none, is gone.
    fn of(removed: &DeviceInfo, cards: &[Card], now: Instant) -> Option<Self> {
        removed.card_present(cards).then(|| Self {
            name: removed.name.clone(),
            left: now,
            back: None,
            card_id: removed.card_id,
            bluez_address: removed.bluez_address.clone(),
        })
    }

    /// Whether the node still counts as seen: it went no longer than [`RETURN_WAIT`] ago.
    fn expected(&self, now: Instant) -> bool {
        now < self.left + RETURN_WAIT
    }

    /// Whether the node came back within the wait, no longer than [`RETURN_WAIT`] ago: a
    /// [`UiToAudio::SelectDevice`] naming it now is the app announcing its saved device again
    /// because it was listed again ([`select_device`]), not a pick.
    fn just_back(&self, now: Instant) -> bool {
        self.back.is_some_and(|back| now < back + RETURN_WAIT)
    }

    /// Whether this departure says anything any more.
    fn over(&self, now: Instant) -> bool {
        !self.expected(now) && !self.just_back(now)
    }

    /// Whether its card is still among `cards`, asked as the node was ([`Hold::card_present`]).
    fn card_present(&self, cards: &[Card]) -> bool {
        cards
            .iter()
            .any(|card| card.owns(self.card_id, self.bluez_address.as_deref()))
    }
}

/// Whether a device is the very node a pair was built on: its `node.name` **and** its
/// `object.serial`.
///
/// The name alone cannot tell. A Bluetooth headset switching profile — or any node removed and
/// added back — comes back under the same name as a new node, and a device-facing stream that is
/// `node.dont-reconnect` is never linked again by WirePlumber once it has been linked
/// (`linking/prepare-link.lua:71-76`): the stream was *handled*, the link went with the old node,
/// and a pair kept because the name matched would play into nothing. A new serial makes the rules
/// rebuild it, and the new pair's stream is a new one WirePlumber links. When the server says no
/// serial the name decides, as it did before.
fn is_same_node(built_name: &str, built_serial: Option<u64>, device: &DeviceInfo) -> bool {
    built_name == device.name && built_serial == device.object_serial
}

/// Where the process callbacks meet: a wait-free single-producer single-consumer ring of samples.
///
/// The two nodes are *not* linked to each other — `node.link-group` exists precisely to stop
/// WirePlumber linking them — so PipeWire gives no ordering guarantee between their `process()`
/// calls, and they may in principle land on different data loops. A ring with a target fill of
/// 1.5 quanta absorbs that, exactly as the Windows loop kept the WASAPI capture ring half full
/// (`sndDevicesDoCapture.cpp:125`, `:152-156`).
///
/// # Why atomics rather than a shared `&mut [f32]`
///
/// A conventional SPSC ring hands the producer and consumer disjoint `&mut` views of one buffer,
/// which needs `unsafe`. This crate denies `unsafe_code` everywhere but the echo canceller's FFI
/// seam (`lib.rs`), and certainly in a process callback, so each slot is an `AtomicU32` holding
/// `f32::to_bits`. On every target this project supports, a `Relaxed` atomic load or store
/// of a `u32` compiles to the same instruction as a plain one — no fence, no lock — so the cost is
/// the lost auto-vectorisation on the copy, not the copy itself. Correctness in exchange for a few
/// nanoseconds per quantum is the right trade in a callback that must never be wrong.
///
/// Capacity is a power of two so the wrap is a mask, and the two cursors are free-running counters
/// that are only ever incremented — the producer owns `write`, the consumer owns `read`, and
/// neither ever writes the other's.
#[derive(Debug)]
pub(crate) struct SampleRing {
    slots: Box<[AtomicU32]>,
    mask: usize,
    write: AtomicUsize,
    read: AtomicUsize,
    channels: AtomicUsize,
    target_fill_frames: AtomicUsize,
    /// The Windows "don't start playing until the capture ring is half full" rule
    /// (`sndDevicesDoCapture.cpp:129-140`), which is also what stops a start-up click.
    primed: AtomicBool,
    /// [`Self::mark_stale`] asked the consumer to skip everything up to `stale_until` on its next
    /// [`Self::pop`]. Written by the main loop, taken by the consumer with a `swap`, so each mark
    /// is acted on once.
    stale: AtomicBool,
    /// Where the write cursor stood when the ring was marked stale: everything before it belongs
    /// to a sound that has ended.
    stale_until: AtomicUsize,
    dropped_frames: AtomicU64,
    underrun_frames: AtomicU64,
    resyncs: AtomicU64,
}

impl SampleRing {
    /// Allocate the ring once, for the worst case in `docs/spec/12-audio-io.md` §24:
    /// 8 × 2048 frames × 8 channels × 4 bytes = 512 KiB. It is never resized afterwards.
    pub(crate) fn new() -> Self {
        let capacity = RING_CAPACITY_FRAMES * MAX_CHANNELS as usize;
        debug_assert!(capacity.is_power_of_two());
        let slots = (0..capacity)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            mask: capacity - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            channels: AtomicUsize::new(0),
            target_fill_frames: AtomicUsize::new(DEFAULT_QUANTUM_FRAMES as usize),
            primed: AtomicBool::new(false),
            stale: AtomicBool::new(false),
            stale_until: AtomicUsize::new(0),
            dropped_frames: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
        }
    }

    /// Adopt a new format and start again from empty. **Main loop only, and only while nothing
    /// drains the ring** — [`build_nodes`] calls it after the lane's previous pair has gone and
    /// before its new NODE 2 exists, and nothing else may.
    ///
    /// Starting from empty means moving the read cursor, and the read cursor is the consumer's.
    /// Under a running NODE 2 that is two writers on one cursor — the one thing the whole ring's
    /// wait-freedom rests on never happening. 0.3.0 also called this from NODE 1's
    /// `param_changed`, which fires again whenever that node renegotiates, with NODE 2 still
    /// popping on the data thread; [`adopt_sink_format`] now leaves the ring alone.
    pub(crate) fn reconfigure(&self, channels: usize, quantum_frames: usize) {
        self.channels.store(channels, Ordering::Relaxed);
        self.target_fill_frames.store(
            quantum_frames * TARGET_FILL_HALF_QUANTA / 2,
            Ordering::Relaxed,
        );
        self.primed.store(false, Ordering::Relaxed);
        // A mark left by the previous pair's NODE 1 describes samples this reset drops anyway.
        self.stale.store(false, Ordering::Relaxed);
        let write = self.write.load(Ordering::Relaxed);
        self.read.store(write, Ordering::Release);
    }

    /// Have the consumer's next [`Self::pop`] skip everything pushed so far, and prime again
    /// before it plays anything. Main loop: NODE 1's `state_changed`, when a pair whose NODE 2 is
    /// passive stops (module docs, "Idle").
    ///
    /// Such a pair stops in one cycle, both nodes together, and leaves the ring's cushion behind:
    /// the last block and a half of a sound that has ended. The next thing to wake the pair may
    /// be another application minutes later, and without this the first thing it played would be
    /// that tail.
    ///
    /// It cannot empty the ring itself. Emptying means moving the read cursor, and the read
    /// cursor is the consumer's — the one rule the ring's wait-freedom rests on, and the reason
    /// [`Self::reconfigure`] is main-loop-only as well. So it only records where the *write*
    /// cursor stands, which is the producer's to move and anyone's to read, and leaves the
    /// skipping to the consumer. Recording a position rather than asking for "everything" is also
    /// what keeps a block NODE 1 pushes in the cycle that wakes the pair: that one is the new
    /// sound's.
    pub(crate) fn mark_stale(&self) {
        self.stale_until
            .store(self.write.load(Ordering::Acquire), Ordering::Relaxed);
        self.stale.store(true, Ordering::Release);
    }

    /// Whether a [`Self::mark_stale`] is still waiting for the consumer.
    #[cfg(test)]
    pub(crate) fn stale_pending(&self) -> bool {
        self.stale.load(Ordering::Acquire)
    }

    /// How many channels the samples currently in the ring are interleaved at.
    pub(crate) fn channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed)
    }

    /// Append interleaved samples. Returns how many were taken.
    ///
    /// Real-time safe and wait-free: bounded work, no allocation, no branch that can panic. On
    /// overrun — which means the consumer has stalled — the frames that do not fit are dropped and
    /// counted rather than blocking, because a producer that waits here stalls every application
    /// feeding the sink.
    pub(crate) fn push(&self, samples: &[f32]) -> usize {
        let channels = self.channels.load(Ordering::Relaxed).max(1);
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let used = write.wrapping_sub(read);
        let free = self.slots.len().saturating_sub(used);
        // Never leave a partial frame in the ring: the consumer reads whole frames.
        let take = samples.len().min(free) / channels * channels;

        for (i, &sample) in samples.iter().take(take).enumerate() {
            if let Some(slot) = self.slots.get(write.wrapping_add(i) & self.mask) {
                slot.store(sample.to_bits(), Ordering::Relaxed);
            }
        }
        self.write
            .store(write.wrapping_add(take), Ordering::Release);

        let dropped = (samples.len() - take) / channels;
        if dropped > 0 {
            self.dropped_frames
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        take
    }

    /// Fill `out` completely, with zeros where the ring has nothing. Returns how many real samples
    /// were copied.
    ///
    /// Real-time safe and wait-free. Two rules beyond the obvious, both inherited from the
    /// Windows loop:
    ///
    /// * **Priming.** Nothing is emitted until the ring has reached its target fill, and an empty
    ///   ring un-primes so the next start is clean again. That is `playbackIsActive`
    ///   (`sndDevicesDoCapture.cpp:129-149`) without the `IAudioClient::Stop()` — PipeWire
    ///   suspends an idle node for us, so there is nothing to stop.
    /// * **Latency recovery.** If the ring has grown past four times its target — a quantum
    ///   change, or the producer briefly outrunning us — the *consumer* skips the excess. Doing it
    ///   here rather than in `push` keeps the ring strictly single-writer-per-cursor, which is
    ///   what makes the whole thing wait-free.
    /// * **Stale samples.** What [`Self::mark_stale`] marked is skipped, for the same reason, and
    ///   the ring primes again: the next sound starts the way a pair just built starts.
    pub(crate) fn pop(&self, out: &mut [f32]) -> usize {
        let channels = self.channels.load(Ordering::Relaxed).max(1);
        let mut read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let mut available = write.wrapping_sub(read);

        // A plain load first, so the cycles with no mark — all of them but one — cost no
        // read-modify-write.
        if self.stale.load(Ordering::Relaxed) && self.stale.swap(false, Ordering::Acquire) {
            // The cursors are free-running and never further apart than the ring is long, so a
            // mark behind `read` — nothing it covers is left — comes out of the subtraction far
            // larger than that, and is ignored. A mark ahead of the `write` loaded above means
            // the producer moved in between, and everything visible here is older than it.
            let stale = self.stale_until.load(Ordering::Relaxed).wrapping_sub(read);
            if (1..=self.slots.len()).contains(&stale) {
                let skip = stale.min(available) / channels * channels;
                read = read.wrapping_add(skip);
                available -= skip;
                self.read.store(read, Ordering::Release);
                self.primed.store(false, Ordering::Relaxed);
            }
        }

        // The cushion has to be measured against the block the consumer actually takes, not
        // against the quantum the nodes were *built* with. Those are the same number only while
        // the graph runs at `DEFAULT_QUANTUM_FRAMES`: `node.force-quantum`, a Bluetooth or USB
        // device that raises it, or a low-CPU configuration all move the real quantum without
        // changing the format, so nothing rebuilds the nodes and nothing calls `reconfigure`.
        // Sized from a fixed 512 this ring measured 21504 underrun frames and 20 resyncs at a
        // 2048-frame quantum; told the truth it measures zero at every quantum from 256 to 2048.
        //
        // `out.len()` is that truth and it is already in hand, so the target is derived here
        // rather than plumbed in — no atomics to coordinate, no `reconfigure` from the real-time
        // thread (it is main-loop only), and it adapts on the first callback after a change
        // rather than on the next supervisor tick. The stored value is kept in step for
        // `fill_frames` instrumentation and the supervisor's log.
        // The target only ever grows inside one format: shrinking it the moment PipeWire hands
        // over a short block would let the cushion collapse and then underrun on the next full
        // one, which is the transition glitch this is meant to remove. `reconfigure` puts it back
        // to the configured quantum whenever the nodes are rebuilt, so it cannot creep for ever.
        let block_frames = (out.len() / channels).max(1);
        let want_frames = block_frames * TARGET_FILL_HALF_QUANTA / 2;
        let target_frames = self
            .target_fill_frames
            .fetch_max(want_frames, Ordering::Relaxed)
            .max(want_frames);
        let target = target_frames * channels;

        if !self.primed.load(Ordering::Relaxed) {
            if available < target {
                for sample in out.iter_mut() {
                    *sample = 0.0;
                }
                // Priming silence is still silence the device played. Counting it keeps the
                // supervisor's underrun figure honest about the start-up gap instead of
                // reporting a clean stream that began with a hole.
                self.underrun_frames
                    .fetch_add((out.len() / channels) as u64, Ordering::Relaxed);
                return 0;
            }
            self.primed.store(true, Ordering::Relaxed);
        }

        let limit = target.saturating_mul(4).max(channels);
        if available > limit {
            let skip = (available - limit) / channels * channels;
            read = read.wrapping_add(skip);
            available -= skip;
            self.resyncs.fetch_add(1, Ordering::Relaxed);
        }

        let take = out.len().min(available) / channels * channels;
        for (i, sample) in out.iter_mut().take(take).enumerate() {
            *sample = self
                .slots
                .get(read.wrapping_add(i) & self.mask)
                .map_or(0.0, |slot| f32::from_bits(slot.load(Ordering::Relaxed)));
        }
        for sample in out.iter_mut().skip(take) {
            *sample = 0.0;
        }
        self.read.store(read.wrapping_add(take), Ordering::Release);

        if take < out.len() {
            self.underrun_frames
                .fetch_add(((out.len() - take) / channels) as u64, Ordering::Relaxed);
            // Any short read means the cushion is gone, not just a completely empty one. Staying
            // primed after a partial read leaves a consumer that is persistently a few frames
            // short clicking on every cycle; dropping out of primed costs one silent block and
            // then the cushion is back.
            self.primed.store(false, Ordering::Relaxed);
        }
        take
    }

    /// Frames currently buffered. Main-loop instrumentation; see `docs/spec/12-audio-io.md` open
    /// question 3, which asks for the fill level to be watched from day one.
    pub(crate) fn fill_frames(&self) -> usize {
        let channels = self.channels.load(Ordering::Relaxed).max(1);
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        write.wrapping_sub(read) / channels
    }
}

/// Numbers the data thread publishes for the main loop. Atomics only: a `process()` callback may
/// not log, so it counts instead and the supervisor does the talking.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    sink_cycles: AtomicU64,
    output_cycles: AtomicU64,
    frames_processed: AtomicU64,
    /// One of the pair's nodes negotiated a channel count the ring was not built for — NODE 2 on
    /// every cycle it refuses to play, NODE 1 once when its format arrives. Audio is muted until
    /// the supervisor rebuilds the nodes, because reinterpreting the ring at the wrong stride is
    /// the one failure here that could be genuinely unpleasant to listen to.
    format_mismatches: AtomicU64,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    /// Frames of delay the DSP is adding right now.
    ///
    /// Written by the audio thread every cycle and read by the supervisor, because it *changes*:
    /// switching the denoiser on adds ten milliseconds. The figure published to PipeWire when the
    /// nodes were built would otherwise stay put, and a recording application would be told the
    /// stream is ten milliseconds tighter than it is — which shows up as lip-sync drift nobody can
    /// diagnose.
    dsp_latency_frames: AtomicU32,
}

impl Counters {
    /// The counters of a lane that has never had a pair of nodes. The format starts as the one a
    /// status is assumed to have until something is negotiated ([`AudioStatus::default`]), so a
    /// lane that never runs has nothing to report and reports nothing.
    fn new() -> Self {
        Self {
            sample_rate: AtomicU32::new(DEFAULT_SAMPLE_RATE),
            channels: AtomicU32::new(MIN_CHANNELS),
            ..Self::default()
        }
    }
}

/// Stream state, published by `state_changed` (main loop) for the supervisor to act on.
///
/// `state_changed` deliberately touches nothing but this: `Stream::connect` can emit it
/// synchronously, and a callback that reached for [`Shared`] at that moment would be re-entering a
/// `RefCell` the supervisor already holds.
#[derive(Debug, Default)]
pub(crate) struct StreamStatus {
    sink_error: AtomicBool,
    output_error: AtomicBool,
    output_streaming: AtomicBool,
    /// What NODE 1's last state asks of NODE 2, as a [`Wish`]. Written on every state NODE 1
    /// reports, in both lanes; only an output pair that is paced by hand acts on it
    /// ([`pace_second_node`]).
    second_wish: AtomicU8,
    /// How many states NODE 1 has reported since the pair was built, so the main loop can tell
    /// "paused all along" from "paused, played and paused again" between two looks.
    first_changes: AtomicU32,
}

impl StreamStatus {
    /// Forget what the lane's previous pair said about itself. Main loop, once that pair is gone:
    /// an error flag it raised on its way out describes nodes that no longer exist, and acting on
    /// it would tear down the *next* pair for nothing.
    fn clear(&self) {
        self.sink_error.store(false, Ordering::Relaxed);
        self.output_error.store(false, Ordering::Relaxed);
        self.output_streaming.store(false, Ordering::Relaxed);
        self.second_wish
            .store(Wish::AsYouWere as u8, Ordering::Relaxed);
        self.first_changes.store(0, Ordering::Relaxed);
    }

    /// NODE 1 reported `state`. Its `state_changed`, on the main loop — which may be inside
    /// `Stream::connect`, with [`Shared`] borrowed by whoever is building the pair, and is why the
    /// wish goes through atomics rather than straight to NODE 2.
    fn first_node_moved(&self, state: &StreamState) {
        self.second_wish
            .store(second_node_wish(state) as u8, Ordering::Relaxed);
        self.first_changes.fetch_add(1, Ordering::Relaxed);
    }
}

/// What NODE 2 should be doing, going by NODE 1's last state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Wish {
    /// NODE 1 says nothing either way: it is not on the server yet, it is leaving it, or it is in
    /// error and about to be rebuilt with its partner.
    AsYouWere = 0,
    /// Something plays into NODE 1.
    Run = 1,
    /// Nothing does.
    Sleep = 2,
}

impl Wish {
    /// Read back what [`StreamStatus::first_node_moved`] stored. Anything else is the cleared
    /// value, which asks for nothing.
    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Run,
            2 => Self::Sleep,
            _ => Self::AsYouWere,
        }
    }
}

/// What a state of NODE 1 asks of NODE 2.
///
/// A `pw_stream` is `Paused` from the moment the server has bound it (`proxy_bound_props` in
/// `src/pipewire/stream.c`) until the graph starts it, and again whenever the graph pauses it — the
/// last client linked into it went away or went quiet — or a session manager suspends it; it is
/// `Streaming` exactly while the graph runs it. So the two are the whole answer, and the states
/// either side of them — connecting, disconnecting, failed — are no answer at all.
const fn second_node_wish(first: &StreamState) -> Wish {
    match first {
        StreamState::Streaming => Wish::Run,
        StreamState::Paused => Wish::Sleep,
        StreamState::Unconnected | StreamState::Connecting | StreamState::Error(_) => {
            Wish::AsYouWere
        }
    }
}

/// NODE 2's schedule in an output pair the server does not idle by itself (module docs, "Idle").
///
/// Pure bookkeeping, so it can be tested without a server: [`Self::next`] says what to tell
/// NODE 2, and [`pace_second_node`] tells it. It wakes NODE 2 the moment NODE 1 streams, because
/// every block NODE 1 pushes before NODE 2 runs again is latency the ring then carries for the
/// rest of the stream. It puts NODE 2 to sleep only once NODE 1 has asked for it for
/// [`SLEEP_AFTER`] without changing its mind, for the reasons given there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SecondNodePace {
    /// What NODE 2 was last told. It is connected active.
    active: bool,
    /// Since when NODE 1 has been asking for sleep, and how many states it had reported when that
    /// was first seen. A count that has moved means it streamed in between, and the wait starts
    /// again.
    sleep_asked: Option<(u32, Instant)>,
}

impl SecondNodePace {
    /// A NODE 2 that has just been connected, and so is active.
    const fn new() -> Self {
        Self {
            active: true,
            sleep_asked: None,
        }
    }

    /// The `set_active` to make now, if any, given NODE 1's latest wish and how many states it has
    /// reported. Nothing changes until [`Self::told`] confirms the call was made.
    fn next(&mut self, wish: Wish, changes: u32, now: Instant) -> Option<bool> {
        match wish {
            Wish::AsYouWere => None,
            Wish::Run => {
                self.sleep_asked = None;
                (!self.active).then_some(true)
            }
            Wish::Sleep if !self.active => None,
            Wish::Sleep => match self.sleep_asked {
                Some((seen, since)) if seen == changes => {
                    (now.saturating_duration_since(since) >= SLEEP_AFTER).then_some(false)
                }
                _ => {
                    self.sleep_asked = Some((changes, now));
                    None
                }
            },
        }
    }

    /// NODE 2 has been told to be `active`.
    const fn told(&mut self, active: bool) {
        self.active = active;
        self.sleep_asked = None;
    }
}

/// User data of NODE 1 — the virtual sink, or the capture stream in the input lane. Either way it
/// is the node the DSP runs in, and the DSP it runs is its lane's.
pub(crate) struct SinkData {
    dsp: Option<LaneDsp>,
    ring: Arc<SampleRing>,
    counters: Arc<Counters>,
    status: Arc<StreamStatus>,
    format: AudioInfoRaw,
    /// Negotiated channel count, latched by `param_changed`. Zero until the format is agreed, and
    /// zero again if it was agreed at a stride the ring was not built for — which is what keeps
    /// `process()` from pushing anything until the pair is rebuilt.
    channels: usize,
    /// The recycle channel of the lane `dsp` belongs to — never the other lane's, so the state
    /// comes back to the slot it was taken from.
    recycle: Sender<LaneDsp>,
    /// The lane's volume: read once a block for the gains the DSP applies after its chain, and —
    /// in the output lane, whose NODE 1 is the virtual sink — written from this node's `Props`.
    volume: Arc<LaneVolume>,
    /// Whether this node is its lane's virtual node, whose `Props` are the volume a desktop
    /// sets: the sink, not the input lane's capture stream, whose `Props` are nobody's slider.
    virtual_node: bool,
    /// The lane's silence while the system sleeps ([`Lane::system_mute`]): one load a block,
    /// handed to the DSP beside the snapshot's own `mute`.
    system_mute: Arc<AtomicBool>,
    /// How many fades in the lane's volume had asked for when this node last looked
    /// ([`LaneVolume::request_fade`]); one more is a fade to start on the next block.
    fades_seen: u64,
}

impl SinkData {
    /// NODE 1's user data for a lane's DSP, wired to that same lane: its ring, its counters, its
    /// stream flags, and its recycle channel to send the DSP home on.
    ///
    /// The lane is chosen from the DSP itself rather than from whatever the caller thinks the
    /// pair's direction is, so the only way to get a lane's state back into the other lane's
    /// slot — or to feed the voice chain's output into the speakers' ring — would be to build it
    /// as the other lane's to begin with.
    fn new(shared: &Shared, dsp: LaneDsp) -> Self {
        let lane = shared.lanes.get(dsp.direction());
        Self {
            ring: Arc::clone(&lane.ring),
            counters: Arc::clone(&lane.counters),
            status: Arc::clone(&lane.status),
            format: AudioInfoRaw::new(),
            channels: 0,
            recycle: lane.recycle.0.clone(),
            volume: Arc::clone(&lane.volume),
            virtual_node: dsp.direction() == DeviceDirection::Output,
            system_mute: Arc::clone(&lane.system_mute),
            // A new pair fades in by itself; a fade asked for before it is not its own.
            fades_seen: lane.volume.fades(),
            dsp: Some(dsp),
        }
    }
}

impl Drop for SinkData {
    /// Hand the DSP side back to the main loop so the next pair of nodes can reuse it. Runs on the
    /// main loop, when the supervisor drops the stream listener.
    fn drop(&mut self) {
        if let Some(dsp) = self.dsp.take()
            && let Err(error) = self.recycle.send(dsp)
        {
            log::error!(
                "could not recycle the {} lane's DSP state; parameters will stop being applied",
                error.into_inner().direction().key()
            );
        }
    }
}

/// User data of NODE 2 — the playback stream, or the virtual source in the input lane.
pub(crate) struct OutData {
    ring: Arc<SampleRing>,
    counters: Arc<Counters>,
    status: Arc<StreamStatus>,
    format: AudioInfoRaw,
    channels: usize,
    scratch: Vec<f32>,
    /// The lane's volume, written from this node's `Props` when it is the virtual source; `None`
    /// on the output lane's playback stream, whose `Props` are nobody's slider.
    volume: Option<Arc<LaneVolume>>,
}

/// What [`crate::AudioEngine::start`] hands the thread.
pub(crate) struct Config {
    pub(crate) remote: Option<String>,
    /// The UI language for the node descriptions; `None` means the desktop locale.
    pub(crate) language: Option<String>,
    pub(crate) control: pw::channel::Receiver<UiToAudio>,
    pub(crate) notify: Sender<AudioToUi>,
    pub(crate) params: Output<DspParams>,
    pub(crate) input_params: Output<InputDspParams>,
    /// Each lane's meters buffer, written by that lane's chain only.
    pub(crate) meters: PerDirection<Input<Meters>>,
    /// Each lane's event queue, read by that lane's chain only.
    pub(crate) events: PerDirection<Receiver<DspEvent>>,
    /// A second sending end of each lane's event queue, beside the GUI's, for the events the
    /// engine sends a chain itself ([`Shared::lane_events`]).
    pub(crate) lane_events: PerDirection<Sender<DspEvent>>,
    pub(crate) ready: Sender<Result<(), AudioError>>,
    /// The canceller library echo cancellation loads: WebRTC's, except in the tests
    /// (`AudioEngine::start_with_canceller`).
    pub(crate) aec_library: &'static str,
    /// The app's remembered per-target volumes ([`crate::StartOptions::target_volumes`]), in place
    /// before the thread connects, so the first pair of either lane already has them.
    pub(crate) target_volumes: Vec<TargetVolume>,
    /// Each lane's device ranking ([`crate::StartOptions::output_priority`]), in place before the
    /// thread connects, so each lane's first choice of device is already made by rank.
    pub(crate) device_priority: PerDirection<crate::DevicePriority>,
    /// WirePlumber's `stream-properties` file, to read the level it kept for our nodes before
    /// 0.4.0 from ([`volume::inherited`]); `None` to read nothing, which is what the tests do.
    pub(crate) wireplumber_state: Option<std::path::PathBuf>,
    /// How long a per-application route nobody uses is kept ([`crate::app_routes::ROUTE_IDLE`]);
    /// shorter in the tests, which would otherwise wait ten seconds to see one go.
    pub(crate) route_idle: Duration,
}

/// One lane's two PipeWire nodes and everything that must die with them.
///
/// Field order is drop order and drop order matters: a `StreamListener` removes a `spa_hook` from
/// a list that lives inside the stream, so every listener is declared before the stream it hooks.
struct Nodes {
    /// The input pair's recorder of its own, while the app holds the microphone awake
    /// ([`KeepAwake`]). First, so it goes before the source it records from: a recorder that
    /// outlived its source for a moment would be one WirePlumber looks for another target for.
    /// `None` in every output pair, and in an input pair nobody holds awake.
    keep_awake: Option<KeepAwake>,
    /// A proxy for the pair's virtual node, bound on the session's volume connection once its
    /// registry announces the node ([`adopt_own_node`]): the one way to write the node's `Props` —
    /// the volume it starts at — with `pipewire` 0.10.1, which binds no `pw_stream_set_param`, and
    /// the way to read what its adapter makes of the volume ([`PropsUpdate::clamped`]).
    own: Option<OwnNode>,
    /// Whether the volume the pair started at has been written to the virtual node's `Props`, so
    /// that every slider shows it. Until then the lane already applies it — the DSP reads it from
    /// the lane — but a desktop would show unity. Written to [`Self::own`], and so cleared again
    /// when another node takes its place ([`still_published`]).
    volume_published: bool,
    /// The lane volume's change count once the pair had its starting volume: a remembered volume
    /// that arrives later ([`UiToAudio::SeedTargetVolumes`]) replaces it only while nothing else
    /// has changed it since.
    volume_changes_at_build: u64,
    _first_listener: pw::stream::StreamListener<SinkData>,
    _second_listener: pw::stream::StreamListener<OutData>,
    /// Kept reachable rather than merely alive: the supervisor republishes this node's
    /// `ProcessLatency` when the DSP's delay changes under it.
    first: pw::stream::StreamRc,
    /// Kept reachable so that an output pair paced by hand can put it to sleep and wake it.
    second: pw::stream::StreamRc,
    /// `node.name` of the real device NODE 2 renders to, or NODE 1 captures from.
    target: String,
    /// `object.serial` of that device's node when the pair was built: what tells it from a node
    /// that comes back under the same name ([`is_same_node`]).
    target_serial: Option<u64>,
    /// The port of `target` the lane's volume is remembered under ([`target_port`]): the one it
    /// was on when the pair was built, or the one it has moved to since ([`follow_port`]). `None`
    /// while its card names none.
    port: Option<String>,
    /// The node the input lane's NODE 1 records from instead of `target` itself: the echo
    /// canceller's source, while echo cancellation runs for this microphone ([`aec::route`]).
    /// `None` in every other pair. What the lane is attached to is still `target` — the canceller
    /// is a stage in front of the microphone, not another device.
    via: Option<&'static str>,
    /// What both nodes were declared at.
    format: PairFormat,
    /// NODE 2's schedule, in an output pair whose server does not run a link-group together and
    /// so cannot idle it by itself (module docs, "Idle"). `None` in every other pair: the input
    /// lane's, whose capture stream is passive or left running, and an output pair whose NODE 2 is
    /// passive.
    pace: Option<SecondNodePace>,
}

/// A recording stream of FxSound's own on the input lane's virtual source, made while the app
/// holds the microphone awake ([`UiToAudio::KeepInputAwake`], module docs, "Idle").
///
/// It stands in for an application recording from FxSound (Input), and is built to look like one
/// to both halves of the graph. To the server: an ordinary, non-passive capture stream whose link
/// to the source makes the source runnable, and with it the passive capture stream and the
/// microphone. To WirePlumber: a `Stream/Input/Audio` with no `node.link-group`, not a monitor —
/// the one kind of stream `device/autoswitch-bluetooth-profile.lua` switches a Bluetooth headset to
/// its call profile for, found from the source through the input lane's group to the headset's
/// loopback microphone. What it records is thrown away.
///
/// Part of the pair ([`Nodes::keep_awake`]): a new pair's source is a new node, and the recorder is
/// made anew with it rather than left for WirePlumber to relink, which it never does for a
/// `node.dont-reconnect` stream it has linked once. Field order is drop order, as for [`Nodes`].
struct KeepAwake {
    _listener: pw::stream::StreamListener<()>,
    _stream: pw::stream::StreamRc,
}

impl KeepAwake {
    /// Make the recorder for an input pair of `format`, asking for the pair's `latency`.
    ///
    /// Targeted at the source by name, which is the one the pair was just built with: the old
    /// pair's source went before this one was made. `node.dont-fallback` and `node.linger` keep it
    /// waiting, rather than linked to some other source or destroyed, if WirePlumber handles it
    /// before the source is linkable; `node.dont-reconnect` and `node.dont-move` keep it where it
    /// was put — nobody else's default and no metadata write may take the recorder elsewhere.
    fn build(
        core: &pw::core::CoreRc,
        format: &PairFormat,
        latency: &str,
    ) -> Result<Self, AudioError> {
        let stream = pw::stream::StreamRc::new(
            core.clone(),
            KEEP_AWAKE_NODE_NAME,
            keep_awake_props(latency),
        )
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(|_stream, _, _old, new| {
                log::debug!("{KEEP_AWAKE_NODE_NAME}: {new:?}");
            })
            .process(on_keep_awake_process)
            .register()
            .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;
        let values = format_pod(format.rate, format.channels, &format.positions);
        let Some(pod) = Pod::from_bytes(&values) else {
            return Err(AudioError::FormatNegotiation);
        };
        stream
            .connect(
                libspa::utils::Direction::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut [pod],
            )
            .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;
        Ok(Self {
            _listener: listener,
            _stream: stream,
        })
    }
}

/// The format a lane's pair runs at: decided once per build, declared on both of its nodes, and
/// kept with the pair.
///
/// Kept so the rules can tell a pair that is still right from one built against what its device
/// *used* to be. The target's name alone cannot: a card switched to another profile keeps its
/// `node.name` and changes its channel count, a device picked before its node info arrived was
/// built on the stereo fallback and turns out to be 7.1, and a graph forced to another rate is the
/// same sink at a different rate. 0.3.0 compared names only, so none of those ever rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PairFormat {
    /// The target's channel count, clamped to `2..=8` (`sndDevices.h:190-191`).
    channels: u32,
    rate: u32,
    /// The target's own channel positions when the clamp keeps its count, and PipeWire's default
    /// layout for the clamped count otherwise — a mono device's pair runs `FL,FR`
    /// ([`ChannelMap::resized`]).
    positions: ChannelMap,
    /// What the voice chain is told the microphone really carries
    /// ([`DeviceInfo::native_rate`]) — the input lane's alone; `None` in an output pair, whose
    /// chain has no use for it. Part of the format so that a pair built before its microphone's
    /// info said it was a Bluetooth headset — the registry global does not say — is rebuilt for
    /// it, the way one built before its channel count arrived is. Once that info is in it stays:
    /// a microphone names its codec, or never does, from its first info on.
    source_rate: Option<u32>,
}

impl PairFormat {
    /// The format a pair attached to `target` runs at, with the graph's clock at `graph_rate`.
    ///
    /// The target's channel count clamped to `2..=8`; its own channel positions when the clamp
    /// keeps its count, PipeWire's default layout for the clamped count otherwise (a mono device
    /// runs `FL,FR`, and the adapter converts between that and the device's `MONO`); and — for
    /// the output lane — the device's rate when it publishes one and the graph's clock rate when
    /// it does not, which is most ALSA sinks. The input lane asks for [`CAPTURE_RATE`] whatever the
    /// microphone runs at, and lets PipeWire resample. RNNoise exists at 48 kHz and nowhere else,
    /// and a voice preset has to mean one thing on every device — a preset whose denoiser
    /// silently drops out on a 44.1 kHz microphone is a preset describing half its own sound. The
    /// cost is a resampler in the graph on devices that are not already at 48 kHz, which is a
    /// little latency and a little CPU on a path that carries one or two channels.
    fn for_target(target: &DeviceInfo, graph_rate: u32) -> Self {
        let channels = target.clamped_channels();
        let rate = match target.direction {
            DeviceDirection::Input => CAPTURE_RATE,
            DeviceDirection::Output => target.rate.unwrap_or(graph_rate),
        };
        Self {
            channels,
            rate,
            positions: target.positions.resized(channels),
            source_rate: match target.direction {
                DeviceDirection::Input => target.native_rate_hz(),
                DeviceDirection::Output => None,
            },
        }
    }
}

/// What the `settings` metadata object says about the graph's clock.
///
/// Two keys, because PipeWire has two: `clock.rate` is the rate the graph runs at when nothing says
/// otherwise, and `clock.force-rate` — what `pw-metadata -n settings 0 clock.force-rate 44100`
/// writes — overrides it until it is set back to `0`. They are kept apart rather than folded into
/// one number as they arrive: the order they arrive in is the server's, and a `force-rate` of `0`
/// has to put the plain rate back, not be ignored as out of range and leave the forced one in
/// place for the rest of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GraphClock {
    /// `clock.rate`.
    rate: u32,
    /// `clock.force-rate`, while it forces anything.
    forced: Option<u32>,
}

impl Default for GraphClock {
    /// What a graph that has said nothing yet is assumed to run at.
    fn default() -> Self {
        Self {
            rate: DEFAULT_SAMPLE_RATE,
            forced: None,
        }
    }
}

impl GraphClock {
    /// The rate the graph runs at now.
    const fn rate(self) -> u32 {
        match self.forced {
            Some(rate) => rate,
            None => self.rate,
        }
    }

    /// Adopt one key of the `settings` object. Keys about anything but the rate are ignored; so is
    /// a `clock.rate` that is not a rate, whereas a `clock.force-rate` that is not one — `0`, or
    /// the key deleted — means nothing is forced.
    fn learn(&mut self, key: &str, value: Option<&str>) {
        let rate = value
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|rate| (8_000..=crate::MAX_SAMPLE_RATE).contains(rate));
        match key {
            "clock.rate" => {
                if let Some(rate) = rate {
                    self.rate = rate;
                }
            }
            "clock.force-rate" => self.forced = rate,
            _ => {}
        }
    }
}

/// A bound node, held only to receive its `info` event ([`Shared::node_probes`]).
///
/// A struct rather than a `(Node, NodeListener)` tuple for its drop order. The listener unhooks
/// itself from a list that lives inside the proxy, so it has to go while the proxy is still there,
/// and fields drop in declaration order — a tuple drops the proxy first. 0.3.0 got the order right
/// in exactly one place, an explicit drain on disconnect; a device unplugged and a node announced
/// again under its id both dropped the tuple as it was.
struct NodeProbe {
    _listener: pw::node::NodeListener,
    _node: pw::node::Node,
}

/// A bound proxy for one of the lane's own virtual nodes ([`Nodes::own`]), in the drop order of a
/// [`NodeProbe`].
struct OwnNode {
    _listener: pw::node::NodeListener,
    node: pw::node::Node,
    /// The node's registry id, which its events are matched to the lane's current pair by.
    id: u32,
}

/// A bound `Device` object, held only to receive its `info` event ([`Shared::card_probes`]), for
/// the same reason and in the same drop order as a [`NodeProbe`].
struct CardProbe {
    _listener: pw::device::DeviceListener,
    _device: pw::device::Device,
}

/// A bound application stream or client, held only to receive its `info` event
/// ([`Shared::app_probes`]), which knows whether the server has let go of what it is bound to.
///
/// That is what decides when it may be dropped. Dropping a proxy the server has not removed sends
/// the server a `destroy` for it, and a proxy whose bind failed names an object the server never
/// made: it answers with a *core* error, "unknown resource", and a core error restarts the whole
/// connection. And a bind fails whenever the object goes between its announcement and the bind,
/// which a `pw-dump`, a `pactl` or a notification sound can do: they live for milliseconds. Seen
/// on PipeWire 1.6.8 when these probes were first dropped as their global went:
/// `graph_churn::echo_cancellation_holds_nothing_awake_while_nothing_records_from_fxsound_input`,
/// which polls the graph with `pw-dump`, had the engine reconnect under it on every run — "no
/// global 30 any more" on the bind, then "unknown resource 14 op:7" on the core — and
/// `live_session` now builds the race on purpose. A bind that succeeded is removed by the server
/// before the registry announces that its object has gone; one that failed, only with the failure,
/// just after. So a probe whose object has gone is dropped at once if the server has removed it,
/// and otherwise retired until it has ([`Shared::retired_probes`]).
struct AppProbe {
    _bound: Bound,
    /// Set by the proxy's `removed` event: the server has removed the object — the bind's own
    /// failure included — and dropping the proxy now sends nothing.
    removed: Rc<Cell<bool>>,
}

/// What an [`AppProbe`] is bound to, each with its listeners declared before its proxy, for the
/// reason [`NodeProbe`] gives.
enum Bound {
    Stream {
        _info: pw::node::NodeListener,
        _events: pw::proxy::ProxyListener,
        _node: pw::node::Node,
    },
    Client {
        _info: pw::client::ClientListener,
        _events: pw::proxy::ProxyListener,
        _client: pw::client::Client,
    },
}

/// One PipeWire connection. Replaced wholesale on a reconnect.
///
/// The lanes' nodes are not in here: each lane keeps its own pair. They are made on this
/// connection all the same, and every stream holds a reference to its core, so a pair outliving
/// its session would hold the dead connection open under the new one. [`close_session`] is the
/// one way a session ends, and it takes both lanes' pairs with it.
///
/// Every listener is declared before the proxy it hooks, for the reason [`NodeProbe`] gives.
struct Session {
    /// The connection the lanes write their own virtual nodes' volume through
    /// ([`adopt_own_node`]), and its registry. A second connection because the server stops
    /// reading a client that sets a param on a node another client owns until that owner answers
    /// (`node_set_param` in `impl-node.c`, `pw_impl_client_set_busy`): asked on the connection the
    /// node belongs to, the owner is the client that has just been stopped, the answer is never
    /// read, and the connection deadlocks — every later write to the node with it. `None` when it
    /// could not be made; the lanes then play the volume they start at without desktops being told.
    _volume_registry_listener: Option<pw::registry::Listener>,
    _volume_registry: Option<pw::registry::RegistryRc>,
    _volume_core: Option<pw::core::CoreRc>,
    _metadata_listener: Option<pw::metadata::MetadataListener>,
    metadata: Option<pw::metadata::Metadata>,
    /// The `settings` object, only ever read — but held, because what is read from it arrives as
    /// events on this proxy ([`on_global`]).
    _settings_listener: Option<pw::metadata::MetadataListener>,
    _settings: Option<pw::metadata::Metadata>,
    _registry_listener: pw::registry::Listener,
    /// Only held so the proxy outlives its listener; the `global` callback owns its own clone.
    _registry: pw::registry::RegistryRc,
    _core_listener: pw::core::Listener,
    core: pw::core::CoreRc,
}

/// Where the connection currently is, for the GUI's "reconnecting…" state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Disconnected,
    Connecting,
    Running,
}

/// How far a connection has got towards knowing the graph well enough to act on it
/// ([`Shared::ready`]): two round trips, one behind the other. A `sync`'s `done` comes back only
/// once the server has answered everything sent before it.
///
/// The registry's dump is not enough on its own. It announces the `default` and `settings`
/// metadata objects, but what they hold is sent only in answer to their bind, which the dump
/// itself calls for ([`on_global`]) — one round trip after the registry's `done`. And every lane
/// decision reads it: the current default steers the choice of device, the configured one is what
/// a claim remembers as displaced, and the graph's clock sets an output pair's rate. A pair built
/// in that gap is built without them, and rebuilt a moment later on what they say.
///
/// Worse, a claim on a default is told apart by whether the lane had a pair when the key was read
/// ([`on_metadata_property`]). A key that names FxSound's node before this connection has built
/// it is nobody's pick in this session; one read after may be the user's. A pair built first
/// would turn the claim a lane that opted out while the connection was down still has to hand
/// back into the user's own pick, and keep it for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Barrier {
    /// The registry's first dump is on its way: [`connect`]'s `sync` has not come back.
    Registry,
    /// The dump is in, and every bind it called for has been sent. This `sync` went after them,
    /// so it comes back after what they were answered with ([`METADATA_SEQ`]).
    Metadata(AsyncSeq),
    /// Both are in.
    Passed,
}

/// What the `default` metadata object says about one direction (`docs/spec/12-audio-io.md` §21).
#[derive(Debug, Default)]
struct DefaultState {
    /// `default.audio.<sink|source>` — what is in effect *now*. WirePlumber's; read only.
    current: Option<String>,
    /// `default.configured.audio.<sink|source>` — the user's choice, or ours. The only key this
    /// module ever writes.
    configured: Option<String>,
    /// True while the configured key names our node. Kept in step with the metadata events, so it
    /// is also true when the user picked FxSound by hand in their sound settings.
    holding: bool,
    /// The configured key names our node, and no lane of ours stands behind that claim: it was
    /// read while the lane was detached, or while it had opted out of the default and had no node
    /// for anyone to have picked — and had not been picked by the user before the connection went
    /// ([`Lane::kept_by_hand`]). So nobody chose it in this session — it is a claim a killed run
    /// left behind, or one this run could not hand back because the connection was down when the
    /// lane was detached or opted out. Handed back by [`hand_back_disowned_defaults`], once the
    /// GUI has had its chance to attach the lane after all ([`ListDelivery`]).
    disowned: bool,
}

/// How far the GUI has got with the device lists it is sent: whether it has had its chance to act
/// on the latest one ([`Shared::gui_has_had_its_chance`]).
///
/// What the app does with a device list is attach the lanes it remembers. The saved microphone
/// switches the input lane on the first time it is listed, and nothing else ever will. So a claim
/// on a default that no lane stands behind *yet* — a killed run's, found at the start — can be one
/// the app is about to stand behind. Handed back before the app has seen the list, it moves every
/// application recording from the default source onto the microphone, and back onto FxSound's
/// source a moment later, when the lane the app attaches claims it again.
///
/// Told apart by the channel itself rather than by a delay: the app reads its notifications once
/// per turn of its loop, and a window that takes seconds to come up the first time reads nothing
/// until it has. It acts on a device list as it takes it, and sends the lane's attachment in the
/// same breath, so once a tick has seen the list taken, the attachment has had a whole tick to
/// arrive.
#[derive(Debug, Default)]
struct ListDelivery {
    /// Every notification sent over this run, queued or not. Less the channel's length, what the
    /// GUI has taken off it — and one that could not be queued, with nobody listening, is nothing
    /// to wait for.
    sent: Cell<u64>,
    /// `sent` just after the latest device list was queued: the GUI has that list once it has
    /// taken this many.
    list: u64,
    /// Supervisor ticks spent waiting for the GUI to take it.
    waited: u32,
    /// An earlier tick saw the GUI holding the latest list.
    taken: bool,
}

impl ListDelivery {
    /// A device list has just been queued: whatever was known about the last one is not about
    /// this one.
    fn list_sent(&mut self) {
        self.list = self.sent.get();
        self.waited = 0;
        self.taken = false;
    }
}

/// One direction's worth of engine state (`docs/0.4.0-design.md` §1.1): its pair of nodes, its
/// DSP, its ring, its counters, and everything the main loop decides about it.
///
/// Everything in here is the lane's alone. The other lane's copy of each field is a different
/// value in a different place, so no path through the supervisor can read one lane's error flag,
/// backoff or status and act on the other's.
struct Lane {
    /// Whether the lane should have a pair of nodes. The output lane starts enabled — FxSound in
    /// front of the speakers is the Windows behaviour — and the input lane only once a microphone
    /// is picked. A disabled lane never builds nodes, whatever the registry says.
    enabled: bool,
    /// The lane's pair, while it has one. Dropped with the session ([`close_session`]).
    nodes: Option<Nodes>,
    /// The lane's DSP while it is on the main loop — between pairs of nodes, or for the whole time
    /// the lane has none. `None` while a NODE 1 holds it in its user data.
    dsp: Option<LaneDsp>,
    /// The lane's own way home for its DSP: NODE 1's user data sends on it when it is dropped, and
    /// [`drain_recycled_dsp`] puts what arrives back here. One per lane rather than one shared, so
    /// a recycled DSP cannot land in the other lane's slot.
    recycle: (Sender<LaneDsp>, Receiver<LaneDsp>),
    /// Where this lane's two process callbacks meet. Each lane has its own: the voice chain's
    /// output must never be popped into the speakers.
    ring: Arc<SampleRing>,
    counters: Arc<Counters>,
    status: Arc<StreamStatus>,
    /// The device names of this lane's direction seen at its previous rules run —
    /// `pwszIDPreviousRealDevices` (`audiopassthru/include/sndDevices.h:349`) — and the nodes of
    /// the direction still expected back from a profile switch ([`Departure`]).
    previous_names: Vec<String>,
    /// Nodes of this lane's direction that went while their card stayed, whether or not the lane
    /// was on them ([`Departure`]). About the graph rather than the lane: kept while the lane is
    /// detached, and forgotten with the connection.
    departures: Vec<Departure>,
    /// Whether to take the session default for this lane's direction once its nodes are up.
    /// `true` unless a caller opted out with [`UiToAudio::SetAsDefault`] with `want: false`.
    want_default: bool,
    /// The connection went while the default of this lane's direction named its node, and the
    /// lane had opted out of the default — so it named FxSound because the user picked it in
    /// their sound settings, not because this lane claimed it ([`disconnect`]). The reconnect
    /// finds that key before it has rebuilt the pair; this is what tells it the claim is the
    /// user's to keep rather than one to hand back ([`on_metadata_property`]).
    ///
    /// Given up by whatever would have handed the default back had the connection been up: the
    /// lane opting out again or opting in ([`UiToAudio::SetAsDefault`]), and the pair ending with
    /// its claim ([`teardown_nodes`]) — a detach, or no device left to attach to.
    kept_by_hand: bool,
    /// The device this lane's last pair was built on, kept through the gaps between pairs, so
    /// that a pair rebuilt on the same device is known for a repair rather than an attachment,
    /// and leaves the default as it found it ([`apply_rules`]). Forgotten when the pair is torn
    /// down with its claim ([`teardown_nodes`]), and when the connection goes while the lane held
    /// the default ([`disconnect`]), so that the next pair claims it again.
    last_target: Option<String>,
    /// Something this lane's device choice depends on changed. Only ever set on an enabled lane.
    needs_rules: bool,
    /// The node this lane's pair is attached to went while its card stayed, and the lane waits for
    /// it rather than choosing another device ([`Hold`]). Ends when the node comes back, when the
    /// wait is up, and whenever the pair is not coming back as it was anyway: a device the user
    /// picks, a detach, a connection that goes.
    hold: Option<Hold>,
    /// Consecutive failed tries at this lane's pair, for its own backoff
    /// (`docs/spec/12-audio-io.md` §22). The socket keeps a separate count.
    attempts: u32,
    /// No rules run for this lane before this instant.
    next_attempt: Instant,
    /// When this lane's current pair was built; `None` while it has none. What the backoff and the
    /// last error are forgiven against ([`Lane::forgive_if_stable`]): the time a pair has actually
    /// been up, not the time a rebuild was first allowed, which can be much earlier — a rebuild
    /// waits for the connection's [`Barrier`] as well as for its backoff.
    built_at: Option<Instant>,
    /// Every format mismatch this lane has seen.
    ///
    /// The supervisor `swap`s the live counter to zero to decide whether to rebuild, which is the
    /// right thing for a trigger and the wrong thing for a report: a user asking why their audio
    /// dropped out wants the total, not whatever has happened since the last 200 ms tick.
    format_mismatches_total: u64,
    /// The delay last declared on this lane's NODE 1, in frames.
    published_latency: u32,
    last_status: AudioStatus,
    last_sink_cycles: u64,
    last_underruns: u64,
    /// The last error the GUI was told about for this lane, so the same error is not told again
    /// while it is still the same failure ([`Shared::report_error`]). Forgotten once a pair has
    /// proved itself, on a new choice of device, on detach and on every connect.
    last_error: Option<AudioError>,
    /// What the GUI was last told this lane is attached to ([`AudioToUi::Attached`]).
    attached: Option<String>,
    /// The volume of the lane's virtual node, shared with its DSP (`crate::volume`).
    volume: Arc<LaneVolume>,
    /// Holds a volume report back until the volume has stopped moving.
    volume_debounce: Debounce,
    /// The volume the lane's last pair ended at: what a target never seen before is never
    /// louder than ([`volume::for_new_pair`]).
    last_volume: Option<NodeVolume>,
    /// Silence after the lane's chain, set by the engine rather than by the GUI's snapshot: while
    /// the system sleeps, and after it wakes until the lane has been attached again or
    /// [`WAKE_MUTE`] is up (module docs, "Sleep"). Shared with the lane's NODE 1, whose DSP reads
    /// it once a block beside the snapshot's own `mute` and silences the same way. Kept across
    /// pairs, like the ring: a pair built while the system sleeps is born silent.
    system_mute: Arc<AtomicBool>,
    /// The system has just woken, and until this instant the lane's rules wait for the device the
    /// lane was on rather than choose another, if that device is not back yet (module docs,
    /// "Sleep"). A [`Hold`] for every lane at once, without the card: a Bluetooth headset's card
    /// goes with the suspend and comes back with its reconnection, and a hold that ended with its
    /// card would never wait for it.
    wake_wait: Option<Instant>,
    /// The system has just woken, and the lane stays silent until its rules have run and left it
    /// attached, or until this instant at the latest ([`WAKE_MUTE`]).
    wake_mute_until: Option<Instant>,
}

impl Lane {
    fn new(enabled: bool, dsp: Option<LaneDsp>) -> Self {
        Self {
            enabled,
            nodes: None,
            dsp,
            recycle: crossbeam_channel::unbounded(),
            ring: Arc::new(SampleRing::new()),
            counters: Arc::new(Counters::new()),
            status: Arc::new(StreamStatus::default()),
            previous_names: Vec::new(),
            departures: Vec::new(),
            want_default: true,
            kept_by_hand: false,
            last_target: None,
            needs_rules: false,
            hold: None,
            attempts: 0,
            next_attempt: Instant::now(),
            built_at: None,
            format_mismatches_total: 0,
            published_latency: 0,
            last_status: AudioStatus::default(),
            last_sink_cycles: 0,
            last_underruns: 0,
            last_error: None,
            attached: None,
            volume: Arc::new(LaneVolume::new()),
            volume_debounce: Debounce::default(),
            last_volume: None,
            system_mute: Arc::new(AtomicBool::new(false)),
            wake_wait: None,
            wake_mute_until: None,
        }
    }

    /// Try this lane's pair again later: after 200 ms the first time, twice as long each time
    /// after that, never longer than five seconds.
    fn retry_later(&mut self, now: Instant) {
        self.next_attempt = now + backoff(self.attempts);
        self.attempts = self.attempts.saturating_add(1);
        self.needs_rules = true;
    }

    /// Forget the backoff, and the error it was serving, once the lane's pair has stayed up for as
    /// long as the wait that came before it.
    ///
    /// Not on the build itself: creating two streams succeeds on almost anything, and the failure
    /// that needs backing off from — a device that refuses the stream — arrives as an error a
    /// moment *later*. Forgiven at build time, that pair would be retried every 200 ms for ever
    /// and the GUI told about it on every retry; forgiven only once it has proved itself, each
    /// quick failure doubles the wait, the whole run of them is reported once, and a pair that has
    /// recovered starts from 200 ms again, its next failure news again.
    ///
    /// Timed from the build, not from when the rebuild was allowed: a rebuild held up by a
    /// registry that was not ready yet would otherwise be forgiven the moment it came up, having
    /// proved nothing.
    fn forgive_if_stable(&mut self, now: Instant) {
        let Some(built_at) = self.built_at else {
            return;
        };
        // `retry_later` has already counted the failure the last wait was for, so the wait before
        // this pair is one step back down the table. With no failure behind it, the shortest one:
        // an error reported by the rules is forgotten once a pair has been up for a tick.
        if now >= built_at + backoff(self.attempts.saturating_sub(1)) {
            self.attempts = 0;
            self.last_error = None;
        }
    }

    /// Record what the lane is attached to now. Returns what to tell the GUI when that differs
    /// from what it was last told, and `None` when there is nothing new to say.
    fn note_attachment(&mut self, now: Option<&str>) -> Option<Option<String>> {
        if self.attached.as_deref() == now {
            return None;
        }
        self.attached = now.map(str::to_owned);
        Some(self.attached.clone())
    }
}

/// Everything the main loop mutates. Reachable only from main-loop callbacks — never from
/// `process()`.
struct Shared {
    state: State,
    session: Option<Session>,
    /// How much of the graph this session has been told: once [`Barrier::Passed`], a device choice
    /// made now is made against the whole graph and the defaults it names, rather than whatever
    /// part of them has arrived so far.
    barrier: Barrier,
    /// The output lane and the input lane. Both can run at once.
    lanes: PerDirection<Lane>,
    /// Every sink and source the registry reports, both directions, keyed by registry global id.
    devices: Vec<DeviceInfo>,
    /// One bound proxy per device, kept alive only to receive the node's `info` event.
    ///
    /// The registry global for a node carries `node.name`, `node.description` and `media.class`
    /// and nothing else — verified against a live daemon — so `audio.channels` and
    /// `audio.position` are simply not knowable from the registry. They live in the node's own
    /// info, which arrives once the node is bound. Without this, every device looked like it had
    /// an unknown channel count, `clamped_channels()` answered the stereo minimum for all of
    /// them, and a 5.1 or 7.1 device was driven as a stereo pair with the rest of its channels
    /// silent — the whole `ChannelMap` path was unreachable in practice.
    ///
    /// Bound on the session's core, so they belong to the session: emptied when it closes
    /// ([`close_session`]) and again as the next one is made ([`connect`]), so a probe on a dead
    /// core is never kept, let alone kept beside a new one for the same device.
    node_probes: std::collections::HashMap<u32, NodeProbe>,
    /// Every card the registry reports — PipeWire's `Device` objects — each carrying its registry
    /// global id ([`Card::object_id`], searched for rather than keyed): what a lane asks, when its
    /// target node goes, to tell a card between profiles from a device that has gone ([`Hold`]).
    cards: Vec<Card>,
    /// One bound proxy per card, kept alive only to receive its `info` event. A Bluetooth card's
    /// address is not in its registry global, only in its info. Belongs to the session like
    /// [`Self::node_probes`], and emptied with them.
    card_probes: std::collections::HashMap<u32, CardProbe>,
    /// What each card has said about its ports, keyed by the card's registry global id: whether
    /// anything is plugged in behind each of its nodes ([`DeviceInfo::available`], `crate::routes`).
    /// Sent to the card's probe as its `param` events; belongs to the session like the probes, and
    /// emptied with them — and a card's with the card.
    card_routes: std::collections::HashMap<u32, CardRoutes>,
    /// Every application stream in the graph, both directions, with what their clients say, and
    /// what the GUI was last told of them (`crate::app_streams`, `docs/0.4.0-apps.md`). Belongs to
    /// the session, like the probes that feed it, and is emptied with them.
    apps: AppStreams,
    /// One bound proxy per application stream and per client, keyed by registry global id, kept
    /// alive only to receive their `info` events: a stream's binary, its target and whether it may
    /// move are not in its registry global, and a native stream's binary and a Flatpak's id are its
    /// client's properties, only in the client's info (`crate::app_streams`, "Where an application
    /// says who it is"). Apart from [`Self::node_probes`], which are the devices', and emptied with
    /// them.
    app_probes: std::collections::HashMap<u32, AppProbe>,
    /// Probes whose object has left the registry before the server removed them: a bind that
    /// failed because the object had already gone. Dropped once the server has removed them, on the
    /// next supervisor tick ([`AppProbe`] says why not before), and with the session.
    retired_probes: Vec<AppProbe>,
    /// The session defaults, one per direction.
    defaults: PerDirection<DefaultState>,
    /// Whether the GUI has had its chance to attach a lane from the device list, before a claim
    /// no lane stands behind is handed back ([`ListDelivery`]).
    delivery: ListDelivery,
    /// The graph's clock, as the `settings` metadata object reports it: the rate an output pair is
    /// built at when its sink does not publish one of its own ([`PairFormat::for_target`]).
    clock: GraphClock,
    /// The Windows registry slots, one set per direction, so trying a microphone never forgets
    /// which speakers the user had.
    memory: PerDirection<SelectionMemory>,
    /// The user's ranking of each direction's devices, and whether they have just picked one
    /// ([`UiToAudio::SetDevicePriority`], [`UiToAudio::SelectDevice`]). Kept across reconnects, like
    /// [`Self::memory`]: it is the user's, not the server's.
    preference: PerDirection<Preference>,

    needs_publish: bool,
    restart_requested: bool,
    /// Consecutive failed connections, for the socket's backoff. Each lane counts its own.
    connect_attempts: u32,
    next_connect: Instant,
    /// The `sync` the exit path put behind its hand-back of the session defaults, until the
    /// server's `done` for it arrives ([`release_defaults_before_exit`]).
    release_pending: Option<AsyncSeq>,

    /// The stage ordering the voice preset names ([`UiToAudio::SetInputChain`]); `voice` until
    /// one does. What the voice engine runs, or is about to.
    input_spec: ChainSpec,
    /// `input_spec` changed and the voice engine has not been told yet: a replacement still has
    /// to be sent over ([`reconcile_input_chain`]), or the input lane's DSP is on its way back
    /// through the recycle channel and is brought up to date when it arrives.
    input_spec_pending: bool,
    handover: ChainHandover,
    notify: Sender<AudioToUi>,
    remote: Option<String>,
    /// See [`Config::language`].
    language: Option<String>,

    last_devices: Vec<AudioDevice>,
    /// The last error about the connection as a whole, rather than about one lane.
    connection_error: Option<AudioError>,
    /// The server runs the members of a `node.link-group` together, so an output pair's NODE 2
    /// can be passive ([`LINK_GROUPS_SCHEDULED_SINCE`]). Learned from the server's core info on
    /// every connect, and `false` until it has arrived — the answer that is merely less idle,
    /// never silent, when it is wrong.
    ///
    /// A cell of its own, shared with the core listener, rather than a field that listener would
    /// have to borrow `Shared` to set. The info event is the only place the answer is learned,
    /// once per connection, and a borrow that happened to be taken when it came would lose it for
    /// the whole connection — with nothing to show for it but an output lane paced by hand on a
    /// server where that never lets it sleep.
    link_groups_scheduled: Rc<Cell<bool>>,
    /// Wakes the main loop to [`pace_second_node`] the moment an output lane's NODE 1 reports a
    /// state, rather than on the next tick. `None` until [`run`] attaches its receiver — so in
    /// tests, which pace by hand — and then the supervisor alone does the pacing.
    wake: Option<pw::channel::Sender<()>>,
    /// Echo cancellation: whether it is wanted, and the module while one is loaded
    /// (`docs/0.4.0-design.md` §7). The module is unloaded explicitly wherever the session ends
    /// ([`close_session`]), so it never outlives the connection it was loaded beside, nor the
    /// context it was loaded into.
    aec: EchoCancel,
    /// The output and input targets the user was last warned are one Bluetooth headset
    /// ([`warn_of_one_headset_on_both_lanes`]), for as long as the lanes stay on them.
    headset_warned: Option<(String, String)>,
    /// The volume of each virtual node per real target, both directions: seeded from the settings
    /// file ([`UiToAudio::SeedTargetVolumes`]) and kept up to date with every volume reported
    /// since, so a device the lane comes back to within a run gets the level it had without a
    /// round trip through the app. Kept across reconnects: it is the user's, not the server's.
    target_volumes: Vec<TargetVolume>,
    /// What WirePlumber kept for each virtual node before 0.4.0 took its volume over, read once at
    /// start ([`volume::inherited`]): the level a lane with no history at all — no pair yet this
    /// run, nothing remembered for its direction — starts no louder than ([`volume_for_pair`]).
    inherited_volumes: PerDirection<Option<NodeVolume>>,
    /// The app holds the microphone awake ([`UiToAudio::KeepInputAwake`]): the input pair records
    /// its own source while it has one ([`KeepAwake`], [`reconcile_keep_awake`]). Kept across
    /// pairs and reconnects: it is the app's to change, not the server's.
    keep_input_awake: bool,
    /// Since when the system has said it is about to sleep ([`UiToAudio::SystemSleeping`]); `None`
    /// while it is awake. While it is set, both lanes are silent and no device rules run (module
    /// docs, "Sleep").
    asleep_since: Option<Instant>,
    /// A sending end of each lane's event queue — the GUI holds the others — for the one event the
    /// engine sends a chain itself: clearing its filter history when the system wakes. `None` in
    /// the tests that build a [`Shared`] without a handle behind it.
    lane_events: PerDirection<Option<Sender<DspEvent>>>,
    /// The per-application routes (`docs/0.4.0-apps.md`, `route_pairs`): the rules the app sent,
    /// the routes each lane runs for them, and what the `default` metadata says about where each
    /// application's stream goes. The rules are kept across reconnects; the routes belong to the
    /// session, and go with it ([`close_session`]).
    routes: route_pairs::Routes,
}

impl Shared {
    /// The state of a thread that has not connected yet: the output lane enabled, the input lane
    /// waiting for a microphone.
    fn new(
        notify: Sender<AudioToUi>,
        remote: Option<String>,
        language: Option<String>,
        dsp: PerDirection<Option<LaneDsp>>,
        handover: ChainHandover,
    ) -> Self {
        let PerDirection { output, input } = dsp;
        Self {
            state: State::Disconnected,
            session: None,
            barrier: Barrier::Registry,
            lanes: PerDirection {
                output: Lane::new(true, output),
                input: Lane::new(false, input),
            },
            devices: Vec::new(),
            node_probes: std::collections::HashMap::new(),
            cards: Vec::new(),
            card_probes: std::collections::HashMap::new(),
            card_routes: std::collections::HashMap::new(),
            apps: AppStreams::default(),
            app_probes: std::collections::HashMap::new(),
            retired_probes: Vec::new(),
            defaults: PerDirection::default(),
            delivery: ListDelivery::default(),
            clock: GraphClock::default(),
            memory: PerDirection::default(),
            preference: PerDirection::default(),
            needs_publish: false,
            restart_requested: false,
            connect_attempts: 0,
            next_connect: Instant::now(),
            release_pending: None,
            input_spec: ChainSpec::voice(),
            input_spec_pending: false,
            handover,
            notify,
            remote,
            language,
            last_devices: Vec::new(),
            connection_error: None,
            link_groups_scheduled: Rc::new(Cell::new(false)),
            wake: None,
            aec: EchoCancel::new(aec::WEBRTC_LIBRARY),
            headset_warned: None,
            target_volumes: Vec::new(),
            inherited_volumes: PerDirection::default(),
            keep_input_awake: false,
            asleep_since: None,
            lane_events: PerDirection::default(),
            routes: route_pairs::Routes::new(crate::app_routes::ROUTE_IDLE),
        }
    }

    fn notify(&self, message: AudioToUi) {
        self.delivery.sent.set(self.delivery.sent.get() + 1);
        if self.notify.send(message).is_err() {
            log::debug!("no GUI is listening for audio notifications");
        }
    }

    /// Report a lane's error once: not on every 200 ms tick it persists, and not again on every
    /// rebuild of a pair that keeps failing. The same error is news again only once a pair has
    /// proved itself ([`Lane::forgive_if_stable`]). "The same" is [`same_failure`]'s.
    fn report_error(&mut self, direction: DeviceDirection, error: AudioError) {
        let lane = self.lanes.get_mut(direction);
        if lane
            .last_error
            .as_ref()
            .is_some_and(|last| same_failure(last, &error))
        {
            log::debug!("audio engine, {} lane, still: {error}", direction.key());
            return;
        }
        log::warn!("audio engine, {} lane: {error}", direction.key());
        lane.last_error = Some(error.clone());
        self.notify(AudioToUi::Error {
            direction: Some(direction),
            message: error.to_string(),
        });
    }

    /// Report an error about the connection itself — no lane can do anything about it — once,
    /// however many reconnects it takes to go away.
    fn report_connection_error(&mut self, error: AudioError) {
        if self
            .connection_error
            .as_ref()
            .is_some_and(|last| same_failure(last, &error))
        {
            log::debug!("audio engine, still: {error}");
            return;
        }
        log::warn!("audio engine: {error}");
        self.connection_error = Some(error.clone());
        self.notify(AudioToUi::Error {
            direction: None,
            message: error.to_string(),
        });
    }

    /// Whether a device choice can be made and acted on right now: connected, and the registry's
    /// first dump is in, with what the metadata objects it announced hold ([`Barrier`]).
    fn ready(&self) -> bool {
        self.session.is_some() && self.barrier == Barrier::Passed
    }

    /// Whether the GUI has had its chance to attach a lane from the device list ([`ListDelivery`]):
    /// no newer list is waiting to be sent, and an earlier tick than this one saw the latest taken
    /// off the channel — or it has gone untaken for [`GUI_PATIENCE_TICKS`]. Once per supervisor
    /// tick: the ticks are what it counts.
    ///
    /// An earlier tick, and not this one, because the GUI takes the list first and sends the
    /// attachment after it. A tick that saw the list gone could run between the two, and the
    /// attachment it would miss is on its way; the next tick is 200 ms later, and finds it made.
    fn gui_has_had_its_chance(&mut self) -> bool {
        // Something changed that the GUI has not been sent yet — a device appeared, one the app
        // may be waiting for. This tick's publish sends it.
        if self.needs_publish {
            return false;
        }
        if self.delivery.taken {
            return true;
        }
        let taken_off = self
            .delivery
            .sent
            .get()
            .saturating_sub(self.notify.len() as u64);
        let delivery = &mut self.delivery;
        if taken_off >= delivery.list {
            delivery.taken = true;
            return false;
        }
        delivery.waited = delivery.waited.saturating_add(1);
        delivery.waited >= GUI_PATIENCE_TICKS
    }

    /// Ask for the rules to run for a lane, if it is enabled. A detached lane ignores the registry
    /// entirely — a microphone plugged in while the input lane is off must not switch it on.
    fn mark_lane_for_rules(&mut self, direction: DeviceDirection) {
        let lane = self.lanes.get_mut(direction);
        if lane.enabled {
            lane.needs_rules = true;
        }
    }

    /// [`Self::mark_lane_for_rules`] for both lanes.
    fn mark_enabled_lanes_for_rules(&mut self) {
        for direction in DeviceDirection::ALL {
            self.mark_lane_for_rules(direction);
        }
    }
}

/// Whether two errors are the same failure, for the purpose of telling the GUI about it once.
///
/// By kind, not by text. [`AudioError::PipewireUnavailable`] carries whatever libpipewire said at
/// the time, and one outage is not always described the same way twice — a socket that refuses,
/// then a socket that is not there, then a stream the half-started server would not create.
/// Compared whole, every change of wording was news, and a server slow to come back told the user
/// again each time its answer changed. The first description is the one they get; the rest go to
/// the debug log.
fn same_failure(a: &AudioError, b: &AudioError) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Republish a lane's NODE 1 `ProcessLatency` when the DSP's delay has moved under it.
///
/// The figure is fixed at build time for the output chain — the limiter's look-ahead does not
/// change — but the microphone chain's denoiser is a per-preset switch worth ten milliseconds, and
/// a preset can be chosen at any moment. Without this, a recording application keeps the number it
/// was told when the nodes were built, and a ten-millisecond error in a voice stream is exactly
/// the kind of lip-sync drift nobody can diagnose from the outside.
///
/// Once per supervisor tick, so the correction is at most 200 ms late, and only when the number
/// actually changed: `update_params` on an unchanged pod would be churn in the graph.
fn republish_latency(shared: &mut Shared, direction: DeviceDirection) {
    let lane = shared.lanes.get_mut(direction);
    let current = lane.counters.dsp_latency_frames.load(Ordering::Relaxed);
    let Some(nodes) = lane.nodes.as_ref() else {
        return;
    };
    if current == lane.published_latency || current == 0 {
        return;
    }

    let values = process_latency_pod(current as usize);
    let Some(pod) = libspa::pod::Pod::from_bytes(&values) else {
        log::warn!("could not build a ProcessLatency pod for {current} frames");
        return;
    };
    if let Err(error) = nodes.first.update_params(&mut [pod]) {
        log::warn!("could not republish the DSP latency: {error}");
        return;
    }
    log::info!(
        "{} lane: DSP latency is now {current} frames (was {})",
        direction.key(),
        lane.published_latency
    );
    lane.published_latency = current;
}

/// Whether a server reporting `version` runs the members of a `node.link-group` together
/// ([`LINK_GROUPS_SCHEDULED_SINCE`]). A version that cannot be read is taken to be older: that
/// answer, when wrong, costs idle power, where the other would cost the user their sound.
fn schedules_link_groups(version: &str) -> bool {
    let mut parts = version.trim().split('.').map(|part| {
        let digits = part
            .find(|c: char| !c.is_ascii_digit())
            .map_or(part, |end| &part[..end]);
        digits.parse::<u32>().ok()
    });
    let (Some(Some(major)), Some(Some(minor)), Some(Some(micro))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    (major, minor, micro) >= LINK_GROUPS_SCHEDULED_SINCE
}

/// The word for a direction in log lines.
const fn noun(direction: DeviceDirection) -> &'static str {
    match direction {
        DeviceDirection::Output => "sink",
        DeviceDirection::Input => "source",
    }
}

/// Fail fast when there is no session bus to connect to at all.
///
/// `docs/spec/12-audio-io.md` §22 asks for this explicitly: without `XDG_RUNTIME_DIR` there is
/// nowhere for a PipeWire socket to be — a bare TTY, or a Flatpak without the `pipewire` socket
/// permission — and retrying on a backoff forever would just hide the real problem. Whether a
/// server is actually listening is left to `pw_context_connect`, which resolves `remote.name`
/// itself and says so precisely.
///
/// # Errors
/// [`AudioError::PipewireUnavailable`] when the directory is unset, empty or not a directory.
pub(crate) fn preflight(runtime_dir: Option<&OsStr>) -> Result<(), AudioError> {
    let Some(dir) = runtime_dir.filter(|d| !d.is_empty()) else {
        return Err(AudioError::PipewireUnavailable(
            "XDG_RUNTIME_DIR is not set, so there is no PipeWire socket to connect to".to_owned(),
        ));
    };
    if !std::path::Path::new(dir).is_dir() {
        return Err(AudioError::PipewireUnavailable(format!(
            "XDG_RUNTIME_DIR ({}) is not a directory",
            dir.to_string_lossy()
        )));
    }
    Ok(())
}

/// The thread body.
pub(crate) fn run(config: Config) {
    let Config {
        remote,
        language,
        control,
        notify,
        params,
        input_params,
        meters,
        events,
        lane_events,
        ready,
        aec_library,
        target_volumes,
        device_priority,
        wireplumber_state,
        route_idle,
    } = config;

    pw::init();

    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(error) => {
            let _ = ready.send(Err(AudioError::PipewireUnavailable(format!(
                "could not create a PipeWire main loop: {error}"
            ))));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(context) => context,
        Err(error) => {
            let _ = ready.send(Err(AudioError::PipewireUnavailable(format!(
                "could not create a PipeWire context: {error}"
            ))));
            return;
        }
    };

    // Both lanes' DSP starts on the main loop; each goes out to a NODE 1 when its lane gets a
    // pair of nodes and comes back through its own recycle channel when the pair goes.
    let (dsp, handover) = lane_dsp::build(params, input_params, meters, events);
    let shared = Rc::new(RefCell::new(Shared::new(
        notify,
        remote,
        language,
        PerDirection {
            output: Some(dsp.output),
            input: Some(dsp.input),
        },
        handover,
    )));
    {
        let mut state = shared.borrow_mut();
        state.aec = EchoCancel::new(aec_library);
        state.lane_events = PerDirection {
            output: Some(lane_events.output),
            input: Some(lane_events.input),
        };
        // Before the first connection, and so before any pair: neither lane can build on a volume
        // chosen without what the app remembers, or without what WirePlumber kept — nor choose
        // its first device without the user's ranking.
        state.target_volumes = remembered_volumes(target_volumes);
        start_ranked(&mut state, device_priority);
        state.routes.set_idle(route_idle);
        if let Some(path) = wireplumber_state {
            state.inherited_volumes = volume::inherited_from(&path);
            for (direction, inherited) in state.inherited_volumes.iter() {
                if let Some(inherited) = inherited {
                    log::info!(
                        "{} lane: WirePlumber kept {:?}{} for our node before 0.4.0",
                        direction.key(),
                        inherited.effective(inherited.channel_volumes.len()),
                        if inherited.mute { ", muted" } else { "" }
                    );
                }
            }
        }
    }

    let control_source = control.attach(mainloop.loop_(), {
        let shared = Rc::clone(&shared);
        let mainloop = mainloop.clone();
        move |message| handle_control(&shared, &mainloop, message)
    });

    let wake_source = attach_wake(mainloop.loop_(), &shared);

    // The first attempt is made synchronously so `AudioEngine::start` can report it. Connecting
    // to a socket that is not there fails at `connect(2)`, so this does not delay start-up.
    let first = connect(&shared, &context);
    let _ = ready.send(first.as_ref().copied().map_err(Clone::clone));
    if let Err(error) = first {
        shared.borrow_mut().report_connection_error(error);
    }

    let timer = mainloop.loop_().add_timer({
        let shared = Rc::clone(&shared);
        let context = context.clone();
        move |_| supervise(&shared, &context)
    });
    let _ = timer.update_timer(Some(SUPERVISOR_PERIOD), Some(SUPERVISOR_PERIOD));

    mainloop.run();

    // Teardown order is the one `docs/spec/12-audio-io.md` §21.5 insists on: hand the session
    // defaults — the sink, the source, both when both lanes hold theirs — back to real devices
    // *first*, while our metadata proxy is still alive, and only then destroy the nodes.
    // Otherwise there is a window in which a default names a node that no longer exists.
    //
    // Nothing else may run while that happens: a supervisor tick would re-run the rules and take
    // a default straight back, and a late control message could do the same.
    //
    // `close_session` also unloads the echo canceller, which is the last thing loaded into the
    // context and has to go before it does: the context is dropped when this function returns.
    drop(timer);
    drop(control_source);
    drop(wake_source);
    release_defaults_before_exit(&shared, &mainloop);
    close_session(&mut shared.borrow_mut());
    log::info!("FxSound audio thread stopped");
}

/// Hear an output lane's NODE 1 start and stop streaming the moment it does, rather than on the
/// next tick, and pace its NODE 2 accordingly when the pair is paced by hand (module docs, "Idle").
/// Gives `shared` the sending end, which each such pair's NODE 1 takes a copy of.
///
/// The channel's lock is held while the callback runs, and NODE 1's `state_changed` is the only
/// sender, so the callback must never do anything to a NODE 1 — it only ever touches NODE 2.
fn attach_wake<'l>(
    loop_: &'l pw::loop_::Loop,
    shared: &Rc<RefCell<Shared>>,
) -> pw::channel::AttachedReceiver<'l, ()> {
    let (wake, woken) = pw::channel::channel::<()>();
    let attached = woken.attach(loop_, {
        let shared = Rc::clone(shared);
        move |()| {
            // Nothing holds the state between two turns of the loop, but a callback cannot prove
            // it; were it held, the wish would wait in the lane's atomics for the next tick, which
            // paces every lane itself.
            if let Ok(mut shared) = shared.try_borrow_mut() {
                let now = Instant::now();
                for direction in DeviceDirection::ALL {
                    pace_second_node(&mut shared, direction, now);
                }
            }
        }
    });
    shared.borrow_mut().wake = Some(wake);
    attached
}

/// The exit hand-back, made to actually arrive: every session default FxSound holds, and every
/// application stream it moved onto a per-application route ([`route_pairs::move_everything_back`]).
///
/// `Metadata::set_property` does not write to the socket. libpipewire queues the message and only
/// sends it from the loop, once the fd reports writable (`module-protocol-native.c`:
/// `on_client_need_flush` arms `SPA_IO_OUT`, `on_remote_data` flushes), and `pw_core_disconnect`
/// destroys that source without flushing. So a write issued after `MainLoop::run` has returned
/// never leaves the process unless the loop is pumped by hand — which is what this does. It
/// issues the write, puts a `sync` behind it, and iterates until the server's `done` for that
/// sync proves the write was processed, or until [`RELEASE_TIMEOUT`] says the server is not
/// answering and the socket may as well be closed.
///
/// Called with no callback on the stack — after `run()`, with the supervisor and the control
/// channel already detached — so the `borrow_mut` cannot collide with anything, and the only
/// callbacks the pump can reach (`done`, the metadata echo of our own writes, stream state) all
/// `try_borrow_mut` and never touch the default, nor write a stream's target.
fn release_defaults_before_exit(
    shared: &Rc<RefCell<Shared>>,
    mainloop: &pw::main_loop::MainLoopRc,
) {
    let pending = {
        let mut guard = shared.borrow_mut();
        // The defaults, and every application stream FxSound moved onto a route: both are
        // metadata writes, and one `sync` behind them confirms both, before any node goes.
        let released = release_all_defaults(&mut guard);
        let moved_back = route_pairs::move_everything_back(&mut guard);
        if !released && !moved_back {
            return;
        }
        let Some(session) = guard.session.as_ref() else {
            return;
        };
        match session.core.sync(RELEASE_SEQ) {
            Ok(seq) => seq,
            Err(error) => {
                log::warn!("could not confirm the exit hand-back: {error}");
                return;
            }
        }
    };
    shared.borrow_mut().release_pending = Some(pending);

    let deadline = Instant::now() + RELEASE_TIMEOUT;
    if pump_until_release_confirmed(shared, mainloop.loop_(), deadline) {
        log::debug!("the server confirmed the exit hand-back");
    } else {
        log::warn!(
            "the server did not confirm the exit hand-back within {RELEASE_TIMEOUT:?}; closing \
             the connection regardless"
        );
    }
}

/// Iterate `loop_` until [`confirm_release`] has cleared the pending sync, or until `deadline`.
/// Returns whether the confirmation arrived.
fn pump_until_release_confirmed(
    shared: &Rc<RefCell<Shared>>,
    loop_: &pw::loop_::Loop,
    deadline: Instant,
) -> bool {
    while shared.borrow().release_pending.is_some() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        // `iterate` enters and leaves the loop itself, exactly as `pw_main_loop_run` did around
        // each of its own iterations; a `remaining` under a millisecond becomes a non-blocking
        // poll, which is fine this close to the deadline.
        loop_.iterate(pw::loop_::Timeout::Finite(remaining));
    }
    true
}

/// The `done` half of the exit hand-back: whether `seq` answers the release sync. Any other
/// `done` — the registry barrier of [`connect`] in particular — is not ours.
fn confirm_release(shared: &mut Shared, seq: AsyncSeq) -> bool {
    if shared.release_pending == Some(seq) {
        shared.release_pending = None;
        return true;
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------------------------

fn handle_control(
    shared: &Rc<RefCell<Shared>>,
    mainloop: &pw::main_loop::MainLoopRc,
    message: UiToAudio,
) {
    if matches!(message, UiToAudio::Shutdown) {
        mainloop.quit();
        return;
    }
    let Ok(mut shared) = shared.try_borrow_mut() else {
        log::error!("dropping {message:?}: the audio supervisor is already running");
        return;
    };
    control(&mut shared, message);
}

/// Act on one control message. Everything but `Shutdown`, which needs the loop and is handled by
/// [`handle_control`] before this is reached.
///
/// A message that names a lane touches that lane and nothing else: picking a microphone while the
/// speakers are being processed leaves the speakers exactly as they were.
fn control(shared: &mut Shared, message: UiToAudio) {
    match message {
        UiToAudio::SelectDevice {
            node_name,
            direction,
        } => select_device(shared, direction, node_name),
        UiToAudio::DetachLane(direction) => detach_lane(shared, direction),
        UiToAudio::RescanDevices => {
            shared.needs_publish = true;
            shared.mark_enabled_lanes_for_rules();
        }
        UiToAudio::SetAsDefault { direction, want } => {
            let lane = shared.lanes.get_mut(direction);
            lane.want_default = want;
            // Either way the lane has spoken for its default since the user's pick. Opted in, it
            // claims it; opted out, what it holds goes back — here and now, or, with the
            // connection down, on the reconnect that finds it ([`hand_back_disowned_defaults`]).
            lane.kept_by_hand = false;
            if want {
                // With no nodes yet the claim happens when they come up; writing the key now
                // would point the default at a node that does not exist. Those nodes may be a
                // repair of the pair the lane had — a rebuild after a failure, which on its own
                // leaves the default as it finds it — so the lane is told its next pair attaches.
                if shared.lanes.get(direction).nodes.is_some() {
                    claim_default(shared, direction);
                } else {
                    shared.lanes.get_mut(direction).last_target = None;
                }
            } else {
                release_default(shared, direction);
            }
        }
        UiToAudio::Restart => {
            shared.restart_requested = true;
        }
        UiToAudio::SetEchoCancel(want) => set_echo_cancel(shared, want),
        UiToAudio::SetInputChain(name) => set_input_chain(shared, &name),
        UiToAudio::SeedRememberedDefaults { output, input } => {
            // Only ever fills a gap. If this run has already displaced something, that is the
            // fresher truth and the settings file's copy is stale by a whole session.
            //
            // This is the whole repair for a lane that comes back, and it is smaller than it looks.
            // A process that starts and finds the default naming `fxsound_sink` adopts that claim
            // as its own — which is harmless *provided it knows what to hand back to*, and that
            // knowledge is exactly what the killed run took with it. Seeding it from disk is what
            // turns the next clean exit into the repair. There is deliberately no immediate
            // hand-back for such a lane: it is enabled and wants the default, its nodes come up
            // under the name the key holds, and the claim is no longer provably stale.
            //
            // A lane that does *not* come back — the input lane, detached until a microphone is
            // picked, on a start with that microphone unplugged — has no nodes to stand behind a
            // claim, and hands it back to what is seeded here as soon as that is present
            // (`hand_back_disowned_defaults`) — and the app has seen a device list, and so had
            // its chance to attach the lane after all (`ListDelivery`).
            //
            // What this does not cover, stated plainly: FxSound killed and never started again.
            // Nothing inside the process can repair that.
            for (direction, name) in [
                (DeviceDirection::Output, output),
                (DeviceDirection::Input, input),
            ] {
                // One of our own nodes is never the default from before FxSound, whatever a
                // settings file says: 0.4.0's first cut could remember the echo canceller's
                // source (`claim_default`), and a file written by it still names that node.
                if name.is_empty() || is_ours(&name) {
                    continue;
                }
                let memory = shared.memory.get_mut(direction);
                if memory.original_default.is_empty() {
                    memory.original_default.clone_from(&name);
                }
                if memory.most_recent_default.is_empty() {
                    memory.most_recent_default = name;
                }
            }
        }
        UiToAudio::SetDevicePriority {
            direction,
            names,
            new_devices_first,
        } => {
            set_device_priority(shared, direction, names, new_devices_first);
        }
        UiToAudio::SeedTargetVolumes(volumes) => seed_target_volumes(shared, volumes),
        UiToAudio::SystemSleeping(true) => go_to_sleep(shared, Instant::now()),
        UiToAudio::SystemSleeping(false) => wake_up(shared, Instant::now()),
        UiToAudio::KeepInputAwake(awake) => keep_input_awake(shared, awake),
        UiToAudio::SetAppRoutes(routes) => route_pairs::set_rules(shared, routes),
        UiToAudio::Shutdown => unreachable!("handled by handle_control"),
    }
    // A detached microphone lane, another microphone or other speakers: whatever the message
    // changed, a canceller that no longer fits goes now rather than on the next tick, and a capture
    // stream recording from it goes back to the microphone with it. Loading one needs the context,
    // which only the supervisor has; it follows within 200 ms.
    reconcile_echo_cancel(shared, None);
    // And a microphone pair the message built — or rebuilt, for the canceller — is held awake at
    // once when it is to be, rather than a tick later.
    reconcile_keep_awake(shared);
    // And the per-application routes follow at once too: new rules, a lane moved to another
    // device, a lane detached — whose streams go back and whose routes go with it.
    route_pairs::reconcile(shared, Instant::now());
}

/// Switch echo cancellation on or off, and answer with an [`AudioToUi::EchoCancel`] saying where
/// it stands — even when nothing changed, because whoever asked is waiting to hear.
///
/// On, the canceller is loaded by the next supervisor tick, once the input lane has a pair: there
/// is nothing to cancel for until it does. Off, the module is unloaded now, and the input lane's
/// capture stream is rebuilt on the microphone in the same call ([`reconcile_echo_cancel`]).
fn set_echo_cancel(shared: &mut Shared, want: bool) {
    log::info!("echo cancellation {}", if want { "on" } else { "off" });
    shared.aec.set_on(want);
    reconcile_echo_cancel(shared, None);
    let (running, detail) = shared.aec.answer();
    shared.notify(AudioToUi::EchoCancel { running, detail });
}

/// Where a lane is, for the echo canceller: detached, attached to a device, or in between.
fn canceller_side(lane: &Lane) -> Side<'_> {
    match (&lane.nodes, lane.enabled) {
        (_, false) => Side::Off,
        (Some(nodes), true) => Side::On(&nodes.target),
        (None, true) => Side::Between,
    }
}

/// Load, reload or unload the echo canceller to fit the two lanes ([`EchoCancel::reconcile`]).
/// `context` is the supervisor's; without it — from a control message — only unloading happens.
///
/// A canceller that goes here — switched off, reloaded for other speakers or another microphone,
/// or let go after going by itself — takes the input lane's capture stream off its source in the
/// same call: if the lane's pair was recording from it, the lane's rules run now and rebuild the
/// pair on the microphone. Now, and not on the next tick, because until a new pair is built the
/// lane records nothing, and nothing else will change that. The capture stream is
/// `node.dont-reconnect`, `node.dont-fallback` and `node.linger` ([`stream_props`]): WirePlumber
/// linked it once, to the canceller's source, and with that source gone it leaves the stream
/// unlinked and waiting for good — it never links a stream it has handled again, not to the
/// microphone and not to a reloaded canceller's source, which is a new node under the old name.
/// (Without those keys it would be worse: WirePlumber would link it to whatever else it found —
/// past FxSound's own default source, which `canLink` refuses because the two share
/// `fxsound-input`, to the best of the rest, which can be another microphone altogether.)
///
/// The rebuild is a repair of the pair, on the microphone it was already on, so it leaves the
/// default source as it finds it (`Lane::last_target`): a canceller reloaded because the *speakers*
/// changed must not take back a default source the user had moved away from FxSound.
///
/// Rebuilt after the unload rather than before it, and that is the same thing: the module closes
/// a connection of its own as it goes, so its source's going reaches the server at once, while
/// the new pair is sent on the engine's connection only when the loop next turns — after the
/// unload, whichever of the two calls came first. What matters is that no turn of the loop passes
/// between them.
fn reconcile_echo_cancel(shared: &mut Shared, context: Option<&pw::context::Context>) {
    let now = Instant::now();
    let retry_after = backoff(shared.aec.attempts());
    let ready = shared.ready();
    let Shared {
        aec, lanes, remote, ..
    } = shared;
    // Its own connection goes to the server this engine's does, and only once this engine's is up.
    let connection = context
        .filter(|_| ready)
        .map(|context| (context, remote.as_deref()));
    aec.reconcile(
        canceller_side(&lanes.input),
        canceller_side(&lanes.output),
        connection,
        retry_after,
        now,
    );

    let stranded = shared
        .lanes
        .input
        .nodes
        .as_ref()
        .is_some_and(|nodes| nodes.via.is_some() && shared.aec.route(&nodes.target).is_none());
    if stranded {
        if ready {
            apply_rules(shared, DeviceDirection::Input);
        } else {
            shared.mark_lane_for_rules(DeviceDirection::Input);
        }
        // The rules have followed the canceller's going: the supervisor has nothing to add.
        shared.aec.route_moved();
    }
}

/// Attach a lane to the device the user picked, enabling the lane if it was detached.
///
/// Windows expressed user choice by forcing the system default (`AudioPassthruPrivate.cpp:657`)
/// and then letting the rules pick it up. Recording the preference instead is what
/// `docs/spec/12-audio-io.md` open question 8 asks for: it does the same thing without mutating
/// global state.
///
/// The rules run here and now rather than on the next supervisor tick, and the GUI hears what the
/// lane is attached to as soon as they have: the user is looking at the device list waiting for
/// the pick to take, and 200 ms is long enough to see. Only when the connection has not passed
/// its [`Barrier`] yet are they left to the tick after it, because a choice made against half a
/// graph would report "no input devices" for a microphone that is merely not listed yet.
fn select_device(shared: &mut Shared, direction: DeviceDirection, node_name: String) {
    let repeated = repeats_what_the_lane_has(shared, direction, &node_name, Instant::now());
    shared.memory.get_mut(direction).user_selected = node_name;
    // With a ranking, the pick outranks it this once ([`Preference::fresh_pick`]) — unless it is
    // no pick at all.
    if !repeated {
        shared.preference.get_mut(direction).fresh_pick = true;
    }
    let lane = shared.lanes.get_mut(direction);
    if !lane.enabled {
        log::info!("enabling the {} lane", direction.key());
        lane.enabled = true;
        // What the lane saw the last time it ran is stale by however long it was off; an empty
        // snapshot keeps rule 5 from treating every device plugged in since as the new one.
        lane.previous_names.clear();
    }
    // A choice the user just made is not a retry: whatever backoff the lane was serving is over,
    // and if this choice fails too, the user who made it is told, whatever failed before it.
    lane.attempts = 0;
    lane.next_attempt = Instant::now();
    lane.last_error = None;
    // Nor is the lane still waiting for a node that went, or for its device after a wake: the
    // user has said where it goes.
    lane.hold = None;
    lane.wake_wait = None;
    if shared.ready() {
        shared.lanes.get_mut(direction).needs_rules = false;
        apply_rules(shared, direction);
        publish_attachment(shared, direction);
    } else {
        shared.lanes.get_mut(direction).needs_rules = true;
    }
}

/// Whether a [`UiToAudio::SelectDevice`] naming `name` for the enabled lane of `direction` only
/// repeats what the engine has, and so is no pick to put above the ranking
/// ([`Preference::fresh_pick`]):
///
/// * the device the lane is attached to, and last committed to, already — there is nothing to
///   pick, and a pick of it would keep a better-ranked device arriving in the same run off it;
/// * the device picked before, back a moment ago from a profile switch it went through while its
///   card stayed ([`Departure::just_back`]). The app announces the device saved in its settings
///   again every time it is listed again, and the engine cannot tell that announcement from a
///   click — but a headset back from switching to its call profile is what makes one, and taken
///   as a pick it would take the lane from a better-ranked device on every call. The app is to
///   stop announcing it (Phase C part 2); this keeps the lane where the ranking has it until then.
///   The price is a click on that same device within [`RETURN_WAIT`] of its return, which is
///   taken as no more than what the ranking says.
///
/// A lane that is off is never repeated to: the message is what switches it on.
fn repeats_what_the_lane_has(
    shared: &Shared,
    direction: DeviceDirection,
    name: &str,
    now: Instant,
) -> bool {
    let lane = shared.lanes.get(direction);
    let memory = shared.memory.get(direction);
    if !lane.enabled {
        return false;
    }
    let attached = memory.most_recent_playback == name
        && lane
            .nodes
            .as_ref()
            .is_some_and(|nodes| nodes.target == name);
    let echoed = memory.user_selected == name
        && lane
            .departures
            .iter()
            .any(|departure| departure.name == name && departure.just_back(now));
    attached || echoed
}

/// Take the user's ranking of a lane's devices ([`UiToAudio::SetDevicePriority`]), and let the
/// lane's rules run on it.
///
/// A new ranking moves nothing by itself: the lane's device stays until a better-ranked one arrives
/// or it goes, as dragging a row of upstream's priority list moves nothing
/// (`FxOutputPreference.cpp:238-262`). The rules are asked all the same, because an empty ranking
/// is "follow the system" again, and the Windows rules may well choose otherwise — the session
/// default, most often.
///
/// The one exception is a ranking coming into force: the lane had none, and its device is not one
/// the user picked. That device was the Windows rules' choice — the system's default, before the
/// ranking reached the engine, or while the app followed the system — and the ranking's own choice
/// replaces it on the next run ([`Preference::start_over`]), as upstream starts on its preferred
/// device whenever the saved one is not there.
fn set_device_priority(
    shared: &mut Shared,
    direction: DeviceDirection,
    names: Vec<String>,
    new_devices_first: bool,
) {
    let memory = shared.memory.get(direction);
    let picked =
        !memory.user_selected.is_empty() && memory.user_selected == memory.most_recent_playback;
    let preference = shared.preference.get_mut(direction);
    let coming_into_force = preference.ranking.is_empty();
    if !rank_devices(preference, names, new_devices_first) {
        return;
    }
    if preference.ranking.is_empty() {
        log::info!("{} lane: following the system's default", direction.key());
        preference.start_over = false;
    } else {
        log::info!(
            "{} lane: {} devices ranked, {} first; a device not ranked yet goes {}",
            direction.key(),
            preference.ranking.len(),
            preference.ranking[0],
            if new_devices_first { "first" } else { "last" }
        );
        if coming_into_force && !picked {
            preference.start_over = true;
        }
    }
    shared.mark_lane_for_rules(direction);
}

/// Each lane's ranking as the engine starts with it ([`crate::StartOptions::output_priority`]):
/// in force from the first run of the rules, which then choose by rank from the start — there is
/// no device yet for the ranking to come into force on ([`Preference::start_over`]).
fn start_ranked(shared: &mut Shared, priority: PerDirection<crate::DevicePriority>) {
    let PerDirection { output, input } = priority;
    for (direction, priority) in [
        (DeviceDirection::Output, output),
        (DeviceDirection::Input, input),
    ] {
        let preference = shared.preference.get_mut(direction);
        if rank_devices(preference, priority.names, priority.new_devices_first)
            && !preference.ranking.is_empty()
        {
            log::info!(
                "{} lane: starts with {} devices ranked, {} first",
                direction.key(),
                preference.ranking.len(),
                preference.ranking[0]
            );
        }
    }
}

/// Put a ranking and its policy for devices it does not name into `preference`, as
/// [`UiToAudio::SetDevicePriority`] or [`crate::StartOptions`] carries them. Whether anything
/// changed.
///
/// A node of ours is never a device, and an empty name names nothing: both are left out.
fn rank_devices(preference: &mut Preference, names: Vec<String>, new_devices_first: bool) -> bool {
    let ranking: Vec<String> = names
        .into_iter()
        .filter(|name| !name.is_empty() && !is_ours(name))
        .collect();
    if preference.ranking == ranking && preference.new_devices_first == new_devices_first {
        return false;
    }
    preference.ranking = ranking;
    preference.new_devices_first = new_devices_first;
    true
}

/// Hand a lane's default back, destroy its nodes and disable it (`docs/0.4.0-design.md` §1.3).
///
/// Always answered with an [`AudioToUi::Attached`] carrying `None` — even for a lane that had no
/// nodes to destroy — so the GUI that asked is told, not left to infer it from silence. The other
/// lane is not touched: its pair, its default and its backoff are exactly as they were.
fn detach_lane(shared: &mut Shared, direction: DeviceDirection) {
    log::info!("detaching the {} lane", direction.key());
    // The default goes back before the nodes go, as on every other path that ends a pair.
    teardown_nodes(shared, direction);
    let lane = shared.lanes.get_mut(direction);
    lane.enabled = false;
    lane.needs_rules = false;
    lane.previous_names.clear();
    lane.attempts = 0;
    lane.last_error = None;
    lane.attached = None;
    // A lane that is off has nothing to be heard or waited for after a wake. While the system
    // sleeps it stays silent with the other, and the wake finds it off.
    lane.wake_wait = None;
    lane.wake_mute_until = None;
    if shared.asleep_since.is_none() {
        lane.system_mute.store(false, Ordering::Relaxed);
    }
    // A pick not yet honoured is not honoured by a lane that is off.
    shared.preference.get_mut(direction).fresh_pick = false;
    shared.notify(AudioToUi::Attached {
        direction,
        node_name: None,
    });
}

// ---------------------------------------------------------------------------------------------
// Sleep, and a microphone held awake
// ---------------------------------------------------------------------------------------------

/// The system is about to sleep (module docs, "Sleep"): silence both lanes and freeze their rules
/// until it wakes.
///
/// Both lanes, enabled or not: a lane the user attaches in the moments before the suspend must not
/// be heard either, and one that is off hears nothing anyway. A second `true` before a `false`
/// changes nothing — the sleep began with the first.
fn go_to_sleep(shared: &mut Shared, now: Instant) {
    if shared.asleep_since.is_none() {
        log::info!("the system is going to sleep: both lanes fall silent until it wakes");
        shared.asleep_since = Some(now);
    }
    for (_, lane) in shared.lanes.iter_mut() {
        lane.system_mute.store(true, Ordering::Relaxed);
        // A wake that had not finished is overtaken by this sleep, and the next wake starts afresh.
        lane.wake_wait = None;
        lane.wake_mute_until = None;
    }
}

/// The system has woken (module docs, "Sleep"): clear both lanes' chain history and every
/// route's ([`route_pairs::wake_up`]), let both lanes' rules run with a wait for their devices in
/// front of them, and hear each lane again once it is attached.
///
/// A wake with no sleep before it — the app saying so at start, or saying it twice — changes
/// nothing. [`SLEEP_LIMIT`] calls this too, when no wake has come.
fn wake_up(shared: &mut Shared, now: Instant) {
    if shared.asleep_since.take().is_none() {
        log::debug!("told the system woke, with no sleep before it");
        return;
    }
    log::info!("the system woke: both lanes look for their devices again");
    for direction in DeviceDirection::ALL {
        // The filters, the leveller and the denoiser last saw the world from before the suspend;
        // the first block after it must not ring with that. Through the lane's own event queue, so
        // it lands on the block that is next whichever thread the chain is on.
        if let Some(events) = shared.lane_events.get(direction)
            && events.try_send(DspEvent::ResetFilterState).is_err()
        {
            log::warn!(
                "could not clear the {} lane's filter history after the wake",
                direction.key()
            );
        }
        let lane = shared.lanes.get_mut(direction);
        // What the lane was waiting for before the sleep was timed on a clock that stood still
        // through it, and against a graph the suspend has since taken apart.
        lane.hold = None;
        if lane.enabled {
            lane.wake_wait = Some(now + RETURN_WAIT);
            lane.wake_mute_until = Some(now + WAKE_MUTE);
            // The graph may have changed under the lane in any way while it slept; a backoff it was
            // serving belongs to the world before.
            lane.next_attempt = now;
            lane.needs_rules = true;
        } else {
            lane.system_mute.store(false, Ordering::Relaxed);
        }
    }
    // A chain on the main loop has nothing else to apply its events.
    apply_idle_lane_events(shared);
    // A route's chain saw what its lane's saw, and forgets it with the lane's.
    route_pairs::wake_up(shared);
}

/// Take the system to have woken without saying so, once it has been going to sleep, awake, for
/// longer than [`SLEEP_LIMIT`] ([`wake_up`]). Once per supervisor tick.
fn give_up_on_sleep(shared: &mut Shared, now: Instant) {
    let Some(since) = shared.asleep_since else {
        return;
    };
    if now.saturating_duration_since(since) < SLEEP_LIMIT {
        return;
    }
    log::warn!(
        "the system said it was going to sleep {SLEEP_LIMIT:?} ago and never said it woke; \
         both lanes carry on as if it had"
    );
    wake_up(shared, now);
}

/// Whether a lane's rules wait for the device it was on because the system has just woken: the
/// wait is not up, the lane is on a device, and that device is not back yet ([`Lane::wake_wait`]).
/// A wait that is up is let go of here.
fn waits_after_wake(shared: &mut Shared, direction: DeviceDirection, now: Instant) -> bool {
    let lane = shared.lanes.get(direction);
    let Some(until) = lane.wake_wait else {
        return false;
    };
    if now >= until {
        shared.lanes.get_mut(direction).wake_wait = None;
        return false;
    }
    let on = lane
        .nodes
        .as_ref()
        .map(|nodes| nodes.target.as_str())
        .or(lane.last_target.as_deref());
    on.is_some_and(|on| {
        !shared
            .devices
            .iter()
            .any(|device| device.direction == direction && device.name == on)
    })
}

/// Hear a lane again after the wake once it has earned it: it is `attached` — it has a pair, and no
/// rules are owed, which after a wake means they have run since, because the wake asked for them
/// ([`wake_up`]) — or [`WAKE_MUTE`] is up. Never while the system is asleep again.
fn settle_wake_mute(shared: &mut Shared, direction: DeviceDirection, now: Instant, attached: bool) {
    let sleeping = shared.asleep_since.is_some();
    let lane = shared.lanes.get_mut(direction);
    let Some(until) = lane.wake_mute_until else {
        return;
    };
    if sleeping || !(attached || now >= until) {
        return;
    }
    lane.wake_mute_until = None;
    lane.system_mute.store(false, Ordering::Relaxed);
    log::info!(
        "{} lane: heard again after the wake{}",
        direction.key(),
        if attached {
            ""
        } else {
            ", before its device was back"
        }
    );
}

/// Hold the microphone awake or let it sleep ([`UiToAudio::KeepInputAwake`]): the input pair
/// records its own source while it is held ([`KeepAwake`]). Takes effect on the pair up now, if
/// there is one, and on every pair built while it stays held.
fn keep_input_awake(shared: &mut Shared, awake: bool) {
    if shared.keep_input_awake != awake {
        log::info!(
            "{}",
            if awake {
                "holding the microphone awake"
            } else {
                "letting the microphone sleep while nothing records from FxSound (Input)"
            }
        );
    }
    shared.keep_input_awake = awake;
    reconcile_keep_awake(shared);
}

/// Give the input pair its own recorder while the microphone is held awake, and take it away when
/// it is not ([`KeepAwake`]). From the control message, after every input pair is built, and on
/// every tick — which is also what retries a recorder that could not be made.
fn reconcile_keep_awake(shared: &mut Shared) {
    let want = shared.keep_input_awake;
    let Some(core) = shared.session.as_ref().map(|session| session.core.clone()) else {
        return;
    };
    let Some(nodes) = shared.lanes.input.nodes.as_mut() else {
        return;
    };
    match (want, nodes.keep_awake.is_some()) {
        (true, false) => {
            let latency = pair_latency(nodes.format.rate);
            match KeepAwake::build(&core, &nodes.format, &latency) {
                Ok(keep_awake) => {
                    log::debug!("{KEEP_AWAKE_NODE_NAME} records {SOURCE_NODE_NAME}");
                    nodes.keep_awake = Some(keep_awake);
                }
                Err(error) => log::warn!(
                    "could not make the stream that holds the microphone awake ({error}); \
                     trying again on the next tick"
                ),
            }
        }
        (false, true) => nodes.keep_awake = None,
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Connection and the reconnect state machine
// ---------------------------------------------------------------------------------------------

fn connect(
    shared: &Rc<RefCell<Shared>>,
    context: &pw::context::ContextRc,
) -> Result<(), AudioError> {
    let (remote, link_groups_scheduled) = {
        let shared = shared.borrow();
        (
            shared.remote.clone(),
            Rc::clone(&shared.link_groups_scheduled),
        )
    };
    // Whatever the last server was, this one has not said yet.
    link_groups_scheduled.set(false);
    let remote_for_volume = remote.clone();
    let props = remote.map(|name| {
        properties! {
            *pw::keys::REMOTE_NAME => name,
        }
    });

    let core = context.connect_rc(props).map_err(|error| {
        AudioError::PipewireUnavailable(format!("could not connect to PipeWire: {error}"))
    })?;

    let core_listener = core
        .add_listener_local()
        .info(move |info| {
            // The server's version, not this library's: the scheduler that decides what runs is
            // the server's. Sent once on connect, ahead of the registry dump, so it is known
            // before any pair is built on this connection.
            let scheduled = schedules_link_groups(info.version());
            log::info!(
                "PipeWire {}: {}",
                info.version(),
                if scheduled {
                    "the playback stream and the capture stream are passive, and sleep with the \
                     sink and the source"
                } else {
                    "the playback stream is put to sleep by hand while the sink has no clients, \
                     and the capture stream holds the microphone while the input lane is on"
                }
            );
            link_groups_scheduled.set(scheduled);
        })
        .error({
            let shared = Rc::clone(shared);
            move |id, _seq, res, message| {
                if id != pw::core::PW_ID_CORE {
                    log::debug!("PipeWire error on object {id}: {message} ({res})");
                    return;
                }
                log::warn!("PipeWire core error: {message} ({res})");
                if let Ok(mut shared) = shared.try_borrow_mut() {
                    shared.restart_requested = true;
                }
            }
        })
        .done({
            let shared = Rc::clone(shared);
            move |id, seq| {
                if id != pw::core::PW_ID_CORE {
                    return;
                }
                let Ok(mut shared) = shared.try_borrow_mut() else {
                    return;
                };
                if confirm_release(&mut shared, seq) || !pass_barrier(&mut shared, seq) {
                    return;
                }
                // The registry's first dump has been delivered, and what the metadata objects in
                // it hold: it is now meaningful to choose a device, for every lane that wants one.
                shared.mark_enabled_lanes_for_rules();
                shared.needs_publish = true;
            }
        })
        .register();

    let registry = core.get_registry_rc().map_err(|error| {
        AudioError::PipewireUnavailable(format!("could not get the PipeWire registry: {error}"))
    })?;

    let registry_listener = registry
        .add_listener_local()
        .global({
            let shared = Rc::clone(shared);
            let registry = registry.clone();
            move |global| on_global(&shared, &registry, global)
        })
        .global_remove({
            let shared = Rc::clone(shared);
            move |id| on_global_remove(&shared, id)
        })
        .register();

    let _ = core.sync(0);

    let (volume_core, volume_registry, volume_registry_listener) =
        match volume_connection(shared, context, remote_for_volume) {
            Ok((core, registry, listener)) => (Some(core), Some(registry), Some(listener)),
            Err(error) => {
                log::warn!(
                    "could not connect a second time to write FxSound's own volume, so desktops \
                     will not see the volume a device starts at: {error}"
                );
                (None, None, None)
            }
        };

    let mut guard = shared.borrow_mut();
    // Everything learned from a server is learned again from this one. The probes went with the
    // last session ([`close_session`]); emptying them here as well means a probe can only ever
    // belong to the connection being made, whatever path led here. The clock is assumed to be the
    // default until this server's `settings` object says otherwise.
    guard.devices.clear();
    guard.node_probes.clear();
    guard.cards.clear();
    guard.card_probes.clear();
    guard.card_routes.clear();
    forget_app_streams(&mut guard);
    guard.aec.forget_sources();
    guard.defaults = PerDirection::default();
    guard.clock = GraphClock::default();
    guard.state = State::Connecting;
    guard.barrier = Barrier::Registry;
    guard.session = Some(Session {
        _volume_registry_listener: volume_registry_listener,
        _volume_registry: volume_registry,
        _volume_core: volume_core,
        _metadata_listener: None,
        metadata: None,
        _settings_listener: None,
        _settings: None,
        _registry_listener: registry_listener,
        _registry: registry,
        _core_listener: core_listener,
        core,
    });
    guard.connection_error = None;
    // A new connection is a fresh start for every lane: whatever backoff a lane was serving was
    // against a server that is gone, and an error it reported is re-reported if it still holds.
    let now = Instant::now();
    for (_, lane) in guard.lanes.iter_mut() {
        lane.previous_names.clear();
        lane.departures.clear();
        lane.attempts = 0;
        lane.next_attempt = now;
        lane.last_error = None;
        lane.hold = None;
    }
    Ok(())
}

/// The second connection a session makes, to write its own virtual nodes' volume through
/// ([`Session::_volume_core`]), with a registry that looks out for those nodes and nothing else.
fn volume_connection(
    shared: &Rc<RefCell<Shared>>,
    context: &pw::context::ContextRc,
    remote: Option<String>,
) -> Result<
    (
        pw::core::CoreRc,
        pw::registry::RegistryRc,
        pw::registry::Listener,
    ),
    pw::Error,
> {
    let props = remote.map(|name| {
        properties! {
            *pw::keys::REMOTE_NAME => name,
        }
    });
    let core = context.connect_rc(props)?;
    let registry = core.get_registry_rc()?;
    let listener = registry
        .add_listener_local()
        .global({
            let shared = Rc::clone(shared);
            let registry = registry.clone();
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let name = global
                    .props
                    .and_then(|props| props.get(*pw::keys::NODE_NAME));
                let Some(direction) = virtual_node_direction(name) else {
                    return;
                };
                if let Ok(mut guard) = shared.try_borrow_mut() {
                    adopt_own_node(&mut guard, &shared, &registry, global, direction);
                }
            }
        })
        .register();
    Ok((core, registry, listener))
}

/// End the connection and everything made on it, in the only order that is safe.
///
/// Both lanes' pairs first: every stream holds a reference to the core, so a pair left behind
/// would keep the dead connection open under the next one — and dropping a pair is also what
/// hands its DSP back through the lane's recycle channel. Then the node and card probes, each
/// listener before its proxy ([`NodeProbe`]), while the core they were bound on is still there to
/// destroy them. Then the session itself. Each lane keeps whether it is enabled, its memory and
/// its DSP, so whatever comes next — a reconnect, or the end of the thread — finds the lanes as
/// the user left them.
fn close_session(shared: &mut Shared) {
    // The echo canceller first. It is on a connection of its own (`aec`), so nothing here depends
    // on it, but it was loaded for this server and these devices, and it is loaded again for the
    // next ones once the lanes are back. Its source goes from the registry with the rest.
    shared.aec.unload();
    shared.aec.forget_sources();
    // The routes' pairs, beside the lanes', on this connection too. Their keys in the metadata are
    // the server's now: moved back on the way out already ([`release_defaults_before_exit`]), and
    // found again by the next connection otherwise.
    route_pairs::close(shared);
    for direction in DeviceDirection::ALL {
        retire_volume(shared, direction);
    }
    for (_, lane) in shared.lanes.iter_mut() {
        lane.nodes = None;
        lane.built_at = None;
        lane.status.clear();
        // A node the lane was waiting for is waited for on this server only: the next one lists
        // its devices afresh, and the rules choose among them.
        lane.hold = None;
    }
    shared.node_probes.clear();
    shared.card_probes.clear();
    shared.card_routes.clear();
    forget_app_streams(shared);
    shared.session = None;
    shared.barrier = Barrier::Registry;
    drain_recycled_dsp(shared);
}

/// One `done` on the way through [`Barrier`]. Returns whether it was the last one: whether the
/// graph is known well enough, as of now, to choose devices against.
///
/// The registry's `done` sends the second `sync`. Every global of the dump has been handled by
/// the time it arrives, so every bind the dump called for is queued ahead of it, and the server
/// answers them first. With no session to send it on, or a `sync` that cannot be sent, the
/// barrier is passed there and then: rules that might run before the defaults are read are
/// better than rules that never run.
fn pass_barrier(shared: &mut Shared, seq: AsyncSeq) -> bool {
    match shared.barrier {
        Barrier::Registry => {
            let sync = shared
                .session
                .as_ref()
                .map(|session| session.core.sync(METADATA_SEQ));
            match sync {
                Some(Ok(pending)) => {
                    shared.barrier = Barrier::Metadata(pending);
                    false
                }
                Some(Err(error)) => {
                    log::warn!("could not wait for the session defaults to be read: {error}");
                    shared.barrier = Barrier::Passed;
                    true
                }
                None => {
                    shared.barrier = Barrier::Passed;
                    true
                }
            }
        }
        Barrier::Metadata(pending) if pending == seq => {
            shared.barrier = Barrier::Passed;
            true
        }
        Barrier::Metadata(_) | Barrier::Passed => false,
    }
}

/// Tear the connection down and arm the backoff.
fn disconnect(shared: &mut Shared, reason: &str) {
    if shared.session.is_none() && shared.state == State::Disconnected {
        return;
    }
    log::info!("tearing down the PipeWire connection: {reason}");
    // No hand-back from here. The connection goes in this same callback, and libpipewire only
    // writes to the socket from the loop (see `release_defaults_before_exit`), so a metadata
    // write issued now would never leave the process. That is harmless: this path is only
    // reached when the connection is already broken — the core error means the server is gone
    // and the keys with it as far as we can reach them, WirePlumber falls back on its own once
    // our nodes vanish, and the reconnect claims the defaults again (`claim_default` skips the
    // stale value that still names us). What the session knew about the defaults dies with it;
    // `connect` reads them afresh.
    //
    // Which lanes held theirs is kept, as the one thing the rebuild needs to know: a lane that
    // held the default claims it again with its first pair — the server may have lost the key
    // with everything else — and a lane whose default the user had moved away from FxSound
    // rebuilds on the same device as a repair and leaves it where the user put it
    // (`Lane::last_target`). A lane detached or opted out before the reconnect hands back what
    // the reconnect finds instead ([`hand_back_disowned_defaults`]).
    //
    // Except where the claim was never the lane's to give: an opted-out lane whose node the user
    // made the default in their sound settings. The reconnect reads that key before the pair is
    // back, and without this would take it for one no lane stands behind (`Lane::kept_by_hand`).
    for direction in DeviceDirection::ALL {
        let state = shared.defaults.get(direction);
        let lane = shared.lanes.get_mut(direction);
        if state.holding {
            lane.last_target = None;
        }
        lane.kept_by_hand = state.holding && !state.disowned && lane.enabled && !lane.want_default;
    }
    shared.defaults = PerDirection::default();
    // Both lanes' pairs go with the connection. Each lane stays enabled or detached as it was, so
    // the reconnect rebuilds exactly the lanes that were running.
    close_session(shared);
    shared.state = State::Disconnected;
    shared.devices.clear();
    shared.cards.clear();
    shared.card_routes.clear();
    for (_, lane) in shared.lanes.iter_mut() {
        lane.previous_names.clear();
        lane.departures.clear();
    }

    shared.next_connect = Instant::now() + backoff(shared.connect_attempts);
    shared.connect_attempts = shared.connect_attempts.saturating_add(1);
    shared.notify(AudioToUi::Disconnected {
        reason: reason.to_owned(),
    });
}

/// The 200 ms supervisor.
fn supervise(shared: &Rc<RefCell<Shared>>, context: &pw::context::ContextRc) {
    let should_connect = {
        let Ok(mut guard) = shared.try_borrow_mut() else {
            return;
        };

        // 1. Recover any DSP state a torn-down stream handed back, and any voice engine the
        //    audio thread retired; hand over a chain the preset named if that is still owed;
        //    apply the events of lanes that have no audio thread to apply them.
        drain_recycled_dsp(&mut guard);
        reconcile_input_chain(&mut guard);
        apply_idle_lane_events(&mut guard);
        drop_removed_probes(&mut guard);

        // 2. A core error or an explicit Restart: the connection itself goes.
        if guard.restart_requested {
            guard.restart_requested = false;
            disconnect(&mut guard, "restart requested");
        }

        // 3. The echo canceller: let a module that went by itself go, load the one the lanes call
        //    for, and when whether it runs has changed, have the input lane's rules move the
        //    capture stream onto its source or back onto the microphone — before the lanes' step,
        //    so the move is made in this same tick. A move back because the module went has been
        //    made already, inside `reconcile_echo_cancel`.
        reconcile_echo_cancel(&mut guard, Some(context));
        if guard.aec.route_moved() {
            guard.mark_lane_for_rules(DeviceDirection::Input);
        }

        // 3b. A claim on a default that no lane stands behind goes back, once the graph is known
        //     well enough to say where to, and the GUI has had its chance to attach the lane that
        //     would stand behind it.
        if guard.ready() && guard.gui_has_had_its_chance() {
            hand_back_disowned_defaults(&mut guard);
        }

        // 3c. A sleep whose wake was never announced is over all the same.
        let now = Instant::now();
        give_up_on_sleep(&mut guard, now);

        // 4. Each lane on its own: its streams' errors, its format check, its rules, its
        //    published delay.
        for direction in DeviceDirection::ALL {
            supervise_lane(&mut guard, direction, now);
        }
        reconcile_keep_awake(&mut guard);

        // 4b. The per-application routes, after the lanes they follow: the streams onto their
        //     routes and back, the routes built, moved with their lane, and taken down.
        route_pairs::reconcile(&mut guard, now);

        // 5. Tell the GUI what changed.
        publish(&mut guard);

        guard.session.is_none() && now >= guard.next_connect
    };

    if should_connect
        && let Err(error) = connect(shared, context)
        && let Ok(mut guard) = shared.try_borrow_mut()
    {
        guard.report_connection_error(error);
        guard.next_connect = Instant::now() + backoff(guard.connect_attempts);
        guard.connect_attempts = guard.connect_attempts.saturating_add(1);
    }
}

/// One lane's share of the supervisor tick.
///
/// Nothing here reaches the other lane. A stream in error takes down its own pair and nothing
/// else — not the socket, which the other lane's pair is still running on, and not the other
/// pair — and the lane tries again on its own backoff, 200 ms doubling to five seconds, while the
/// other lane plays on. The default is kept through the gap, as on every transient rebuild: the
/// same node is about to come back under the same name.
fn supervise_lane(shared: &mut Shared, direction: DeviceDirection, now: Instant) {
    let lane = shared.lanes.get_mut(direction);
    if !lane.enabled {
        return;
    }

    // a. Either node of the pair went into error.
    let first_failed = lane.status.sink_error.swap(false, Ordering::Relaxed);
    let second_failed = lane.status.output_error.swap(false, Ordering::Relaxed);
    if first_failed || second_failed {
        log::warn!(
            "the {} lane's {} reported an error; rebuilding it after {:?}",
            direction.key(),
            if first_failed { "NODE 1" } else { "NODE 2" },
            backoff(lane.attempts)
        );
        drop_nodes(shared, direction);
        shared.lanes.get_mut(direction).retry_later(now);
        shared.report_error(direction, AudioError::DeviceUnavailable);
    }

    // b. The two nodes negotiated different formats: rebuild rather than play at the wrong
    //    stride — on the lane's backoff, like a stream in error. The new pair declares the format
    //    the old one failed to agree on, so a server that disagrees once disagrees again, and
    //    0.3.0 handed it a fresh pair on every tick for as long as it did. Not reported as an
    //    error: the lane's status carries every mismatch, so one that a rebuild cures stays a
    //    number rather than a notification.
    let lane = shared.lanes.get_mut(direction);
    let mismatches = lane.counters.format_mismatches.swap(0, Ordering::Relaxed);
    lane.format_mismatches_total += mismatches;
    if mismatches > 0 {
        log::warn!(
            "the {} lane's two nodes negotiated different formats; rebuilding them after {:?}",
            direction.key(),
            backoff(lane.attempts)
        );
        drop_nodes(shared, direction);
        shared.lanes.get_mut(direction).retry_later(now);
    }

    // c. Rules and node creation, once the lane's backoff allows — and not while the lane waits
    //    for the node it was attached to, which went with its card still here ([`Hold`]). What
    //    asked for the rules meanwhile stays asked, and they run the tick the node is back or the
    //    wait is up.
    let lane = shared.lanes.get_mut(direction);
    lane.forgive_if_stable(now);
    if lane.needs_rules
        && now >= lane.next_attempt
        && shared.ready()
        && !held(shared, direction, now)
    {
        shared.lanes.get_mut(direction).needs_rules = false;
        apply_rules(shared, direction);
    }

    // c2. After a wake, the lane is heard again once its rules have run and left it on a device —
    //     not waiting, not backing off, not without one — whether they ran just now or on a device
    //     the user picked in between; or once its time is up.
    let lane = shared.lanes.get(direction);
    let attached = lane.nodes.is_some() && !lane.needs_rules;
    settle_wake_mute(shared, direction, now, attached);

    // d. Keep the published delay honest. Switching the denoiser on adds ten milliseconds, and
    //    only the audio thread knows it happened.
    republish_latency(shared, direction);

    // e. Put a NODE 2 paced by hand to sleep once its NODE 1 has been paused long enough — the
    //    one step of the pacing that waits for time rather than for an event — and catch any wake
    //    the channel could not deliver.
    pace_second_node(shared, direction, now);

    // f. Follow the target's port with the volume, if it has moved to another; then report the
    //    virtual node's volume for its target, once it has stopped moving.
    follow_port(shared, direction);
    watch_volume(shared, direction);
}

/// Whether a lane's rules wait: while the system sleeps, just after it wakes for the device the
/// lane was on ([`waits_after_wake`]), and for the node its pair was attached to ([`Hold`]). A hold
/// that has ended — its node is back, its card has added it back under another name
/// ([`Hold::renamed`]), or the wait is up — is let go of here, and the log says which.
fn held(shared: &mut Shared, direction: DeviceDirection, now: Instant) -> bool {
    // Nothing chooses a device while the system sleeps, nor, just after it wakes, before the
    // lane's own device has had its chance to come back (module docs, "Sleep").
    if shared.asleep_since.is_some() || waits_after_wake(shared, direction, now) {
        return true;
    }
    let Some(hold) = shared.lanes.get(direction).hold.as_ref() else {
        return false;
    };
    let back = shared
        .devices
        .iter()
        .any(|device| device.direction == direction && device.name == hold.target);
    if !back && let Some(renamed) = hold.renamed(&shared.devices, direction) {
        log::info!(
            "{} came back as {}: the {} lane stops waiting for its old name",
            hold.target,
            renamed.name,
            direction.key()
        );
        shared.lanes.get_mut(direction).hold = None;
        return false;
    }
    if hold.waits(now, back) {
        return true;
    }
    if back {
        log::info!(
            "{} is back; the {} lane attaches to it again",
            hold.target,
            direction.key()
        );
    } else {
        log::info!(
            "{} did not come back within {RETURN_WAIT:?}; the {} lane chooses another device",
            hold.target,
            direction.key()
        );
    }
    shared.lanes.get_mut(direction).hold = None;
    false
}

/// Tell a lane's NODE 2 to sleep or wake, if its pair is paced by hand and NODE 1 has asked for it
/// ([`SecondNodePace`]). Main loop: from the wake channel as soon as NODE 1 reports a state, and
/// from every supervisor tick.
///
/// A pair whose server idles it by itself, and the input lane's pair, have no pace and are left
/// alone.
fn pace_second_node(shared: &mut Shared, direction: DeviceDirection, now: Instant) {
    let lane = shared.lanes.get_mut(direction);
    let Some(nodes) = lane.nodes.as_mut() else {
        return;
    };
    let Some(pace) = nodes.pace.as_mut() else {
        return;
    };
    let wish = Wish::from_code(lane.status.second_wish.load(Ordering::Relaxed));
    let changes = lane.status.first_changes.load(Ordering::Relaxed);
    let Some(active) = pace.next(wish, changes, now) else {
        return;
    };
    // The ring needs nothing from here (module docs, "Idle"): by the time NODE 2 is put to sleep
    // it has played the ring dry and dropped out of priming (`SampleRing::pop`).
    match nodes.second.set_active(active) {
        Ok(()) => {
            pace.told(active);
            if active {
                log::debug!(
                    "{} lane: something plays into NODE 1; NODE 2 wakes",
                    direction.key()
                );
            } else {
                log::debug!(
                    "{} lane: nothing plays into NODE 1; NODE 2 sleeps",
                    direction.key()
                );
            }
        }
        // Asked again on the next tick: NODE 1's wish is still in the lane's atomics.
        Err(error) => log::warn!(
            "could not {} the {} lane's NODE 2: {error}",
            if active { "wake" } else { "put to sleep" },
            direction.key()
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------

fn on_global(
    shared: &Rc<RefCell<Shared>>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&libspa::utils::dict::DictRef>,
) {
    let Some(props) = global.props else {
        return;
    };
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };

    match global.type_ {
        pw::types::ObjectType::Node => {
            // Every node of ours, by id and serial, and by name for which of them carry what
            // FxSound plays: what an application's stream that names its target by number is
            // found to name, and whether a recorder that does records FxSound
            // (`crate::app_streams`).
            let name = props.get(*pw::keys::NODE_NAME).unwrap_or_default();
            if is_fxsound_node(name) {
                let serial = props.get("object.serial").and_then(|s| s.parse().ok());
                guard.apps.own_node_appeared(global.id, serial, name);
                // A route's virtual node: the serial its streams are moved onto.
                route_pairs::node_appeared(&mut guard, global.id, serial, name);
            }
            // An application's stream: a player or a recorder. Never a device either.
            if let Some(stream) = StreamNode::from_props(global.id, &|key: &str| props.get(key)) {
                track_stream(&mut guard, shared, registry, global, stream);
                return;
            }
            // The echo canceller's source: what makes echo cancellation *running*, and the input
            // lane's cue to record from it (`supervise`). Never a device, like the rest of ours.
            if props.get(*pw::keys::NODE_NAME) == Some(AEC_SOURCE_NODE_NAME) {
                log::debug!("the echo canceller's source appeared ({})", global.id);
                guard.aec.source_appeared(global.id);
                return;
            }
            // Our own nodes are in the registry too; `from_props` drops them by name so none of
            // them can ever become a target.
            let Some(device) = DeviceInfo::from_props(global.id, &|key: &str| props.get(key))
            else {
                return;
            };
            log::debug!(
                "{} appeared: {} ({})",
                noun(device.direction),
                device.description,
                device.name
            );
            let object_id = device.object_id;
            add_device(&mut guard, device);

            // Ask the node what it is actually made of. This is the only way to learn it; see
            // `Shared::node_probes`.
            match registry.bind::<pw::node::Node, _>(global) {
                Ok(node) => {
                    let listener = node
                        .add_listener_local()
                        .info({
                            let shared = Rc::clone(shared);
                            move |info| {
                                let Some(props) = info.props() else {
                                    return;
                                };
                                // Read the channels, their positions and the Bluetooth address
                                // out here rather than handing the dictionary on: its lifetime is
                                // the callback's, and the handler wants to hold a mutable borrow
                                // of `Shared` across the update.
                                let channels = props
                                    .get("audio.channels")
                                    .and_then(|value| value.parse::<u32>().ok())
                                    .unwrap_or(0);
                                let positions = props
                                    .get("audio.position")
                                    .and_then(ChannelMap::parse)
                                    .filter(|map| map.len() == channels as usize);
                                let get = |key: &str| props.get(key);
                                let address = devices::bluez_address(&get);
                                on_node_address(&shared, object_id, address);
                                on_node_profile_device(
                                    &shared,
                                    object_id,
                                    devices::profile_device(&get),
                                );
                                // Only an info that carries the properties says anything about the
                                // Bluetooth link; one for a state change carries none, and would
                                // read as a node that is not Bluetooth.
                                if info.change_mask().contains(pw::node::NodeChangeMask::PROPS) {
                                    on_node_bluetooth(
                                        &shared,
                                        object_id,
                                        BluezFacts::from_props(&get),
                                        FormFactor::from_props(&get),
                                    );
                                }
                                on_node_info(&shared, object_id, channels, positions);
                            }
                        })
                        .register();
                    guard.node_probes.insert(
                        object_id,
                        NodeProbe {
                            _listener: listener,
                            _node: node,
                        },
                    );
                }
                Err(err) => {
                    log::debug!("could not bind node {object_id} to read its format: {err}")
                }
            }
        }
        pw::types::ObjectType::Device => {
            // A card: nothing to attach to, only something a node can belong to ([`Hold`]).
            let card = Card::from_props(global.id, &|key: &str| props.get(key));
            let id = card.object_id;
            let bluetooth = card.bluetooth;
            guard.cards.retain(|known| known.object_id != id);
            guard.cards.push(card);
            if bluetooth {
                adopt_bluetooth_card(&mut guard, id);
            }
            // A Bluetooth card's address is in its info, not in the registry global. And what is
            // plugged in behind each of a card's nodes is in its routes (`crate::routes`), which it
            // sends as `param` events: all of them once subscribed, and all again on every change.
            match registry.bind::<pw::device::Device, _>(global) {
                Ok(device) => {
                    let listener = device
                        .add_listener_local()
                        .info({
                            let shared = Rc::clone(shared);
                            move |info| {
                                if let Some(props) = info.props() {
                                    let card = Card::from_props(id, &|key: &str| props.get(key));
                                    on_card_info(&shared, card);
                                }
                            }
                        })
                        .param({
                            let shared = Rc::clone(shared);
                            move |_seq, param_type, index, _next, param| {
                                let Some(list) = RouteList::of(param_type) else {
                                    return;
                                };
                                if let Some(route) = param.and_then(Route::from_pod) {
                                    on_card_route(&shared, id, list, index, route);
                                }
                            }
                        })
                        .register();
                    device.subscribe_params(&RouteList::PARAMS);
                    guard.card_probes.insert(
                        id,
                        CardProbe {
                            _listener: listener,
                            _device: device,
                        },
                    );
                }
                Err(err) => log::debug!("could not bind card {id} to read its address: {err}"),
            }
        }
        pw::types::ObjectType::Client => {
            // A connection: who the applications behind its streams are, where the streams do
            // not say it themselves (`crate::app_streams`).
            let key = app_streams::app_key(&|key: &str| props.get(key));
            track_client(&mut guard, shared, registry, global, key);
        }
        pw::types::ObjectType::Metadata => {
            let name = props.get("metadata.name").unwrap_or_default();
            if name != "default" && name != "settings" {
                return;
            }
            let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) else {
                log::debug!("could not bind the `{name}` metadata object");
                return;
            };
            let default = name == "default";
            let listener = metadata
                .add_listener_local()
                .property({
                    let shared = Rc::clone(shared);
                    move |subject, key, _type, value| {
                        on_metadata_event(&shared, default, subject, key, value);
                        0
                    }
                })
                .register();
            let Some(session) = guard.session.as_mut() else {
                return;
            };
            // Each slot's listener before its proxy: an object announced again — a session
            // manager that restarted — replaces what is there, and the old listener has to unhook
            // itself while the proxy it hooks still exists ([`NodeProbe`]).
            if name == "default" {
                session._metadata_listener = Some(listener);
                session.metadata = Some(metadata);
            } else {
                // The `settings` object is only ever read, but it has to be *kept*. Its properties
                // are not in the registry global; they arrive after the bind, as events on this
                // proxy. 0.3.0 dropped the proxy here, on the grounds that the values arrive in
                // the initial burst — they do, addressed to a proxy that no longer existed, so
                // `clock.rate` never reached the engine and every sink that does not publish a
                // rate of its own got an output pair at 48 kHz, whatever the graph ran at.
                session._settings_listener = Some(listener);
                session._settings = Some(metadata);
            }
        }
        _ => {}
    }
}

/// A device joined the graph, or reported itself again under the same id.
///
/// Only the lane of the device's direction can care, and only while it is enabled: a new sink
/// has nothing to say to the microphone's lane, and a microphone plugged in while the input lane
/// is detached must not attach it.
fn add_device(shared: &mut Shared, mut device: DeviceInfo) {
    // A node on a Bluetooth card is Bluetooth from its registry global on, which says nothing
    // else about it: WirePlumber 0.5's microphone is a headset's before its info arrives.
    if device.card_id.is_some_and(|id| {
        shared
            .cards
            .iter()
            .any(|card| card.object_id == id && card.bluetooth)
    }) {
        device.on_bluetooth_card();
    }
    // What its card has said already decides whether it can be heard; a registry global carries no
    // `card.profile.device`, so for one that arrives now it is the node's info that brings it.
    device.available = routes::node_available(
        device.card_id.and_then(|id| shared.card_routes.get(&id)),
        device.profile_device,
    );
    let direction = device.direction;
    note_return(shared, &device, Instant::now());
    shared
        .devices
        .retain(|existing| existing.object_id != device.object_id);
    shared.devices.push(device);
    shared.needs_publish = true;
    shared.mark_lane_for_rules(direction);
}

/// An application's stream joined the graph: note it, and bind it for the properties its registry
/// global does not carry — its binary, its target, whether it may move (`crate::app_streams`,
/// "Where an application says who it is"). It is reported on the tick after its info arrives.
fn track_stream(
    guard: &mut Shared,
    shared: &Rc<RefCell<Shared>>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&libspa::utils::dict::DictRef>,
    stream: StreamNode,
) {
    let id = stream.id;
    route_pairs::stream_appeared(guard, id, stream.direction);
    guard.apps.stream_appeared(stream);
    let node = match registry.bind::<pw::node::Node, _>(global) {
        Ok(node) => node,
        Err(error) => {
            log::debug!("could not bind application stream {id} to read who it is: {error}");
            guard.apps.stream_complete_as_is(id);
            return;
        }
    };
    let info = node
        .add_listener_local()
        .info({
            let shared = Rc::clone(shared);
            move |info| {
                // Only an info that carries the properties says anything about who the stream is;
                // one for a state change would read as a stream that says nothing at all.
                if !info.change_mask().contains(pw::node::NodeChangeMask::PROPS) {
                    return;
                }
                if let Some(props) = info.props() {
                    on_stream_info(&shared, id, &|key: &str| props.get(key));
                }
            }
        })
        .register();
    let removed = Rc::new(Cell::new(false));
    let events = probe_events(shared, pw::proxy::ProxyT::upcast_ref(&node), id, &removed);
    keep_probe(
        guard,
        id,
        AppProbe {
            _bound: Bound::Stream {
                _info: info,
                _events: events,
                _node: node,
            },
            removed,
        },
    );
}

/// An application stream's info arrived, with every property.
fn on_stream_info<'a>(
    shared: &Rc<RefCell<Shared>>,
    id: u32,
    get: &impl Fn(&str) -> Option<&'a str>,
) {
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    if guard.apps.stream_info(id, get)
        && let Some(line) = guard.apps.describe(id)
    {
        log::debug!("{line}");
    }
}

/// A client joined the graph: note what its registry global says about who it is — a name at most
/// — and bind it for the rest, which only its info carries.
fn track_client(
    guard: &mut Shared,
    shared: &Rc<RefCell<Shared>>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&libspa::utils::dict::DictRef>,
    key: fxsound_core::AppKey,
) {
    let id = global.id;
    guard.apps.client_appeared(id, key);
    let client = match registry.bind::<pw::client::Client, _>(global) {
        Ok(client) => client,
        Err(error) => {
            log::debug!("could not bind client {id} to read who it is: {error}");
            guard.apps.client_complete_as_is(id);
            return;
        }
    };
    let info = client
        .add_listener_local()
        .info({
            let shared = Rc::clone(shared);
            move |info| {
                if !info
                    .change_mask()
                    .contains(pw::client::ClientChangeMask::PROPS)
                {
                    return;
                }
                if let Some(props) = info.props()
                    && let Ok(mut guard) = shared.try_borrow_mut()
                {
                    guard.apps.client_info(id, &|key: &str| props.get(key));
                }
            }
        })
        .register();
    let removed = Rc::new(Cell::new(false));
    let events = probe_events(shared, pw::proxy::ProxyT::upcast_ref(&client), id, &removed);
    keep_probe(
        guard,
        id,
        AppProbe {
            _bound: Bound::Client {
                _info: info,
                _events: events,
                _client: client,
            },
            removed,
        },
    );
}

/// The proxy events an [`AppProbe`] needs: `removed`, which says it may be dropped without a word
/// to the server, and `error`, a bind the server refused. A stream or a client whose info will
/// never come is told as far as its registry global goes rather than waited for — unless it has
/// gone, which is what the refusal usually means, and then there is nothing left to tell.
fn probe_events(
    shared: &Rc<RefCell<Shared>>,
    proxy: &pw::proxy::Proxy,
    id: u32,
    removed: &Rc<Cell<bool>>,
) -> pw::proxy::ProxyListener {
    proxy
        .add_listener_local()
        .removed({
            let removed = Rc::clone(removed);
            move || removed.set(true)
        })
        .error({
            let shared = Rc::clone(shared);
            move |_seq, res, message| {
                log::debug!("could not read who {id} is: {message} ({res})");
                if let Ok(mut guard) = shared.try_borrow_mut() {
                    guard.apps.stream_complete_as_is(id);
                    guard.apps.client_complete_as_is(id);
                }
            }
        })
        .register()
}

/// Keep a new probe under its global's id. One already there — an id the server has handed on
/// without the registry saying the old object went, which it does not do — is retired, not
/// dropped.
fn keep_probe(shared: &mut Shared, id: u32, probe: AppProbe) {
    if let Some(old) = shared.app_probes.insert(id, probe) {
        retire(shared, old);
    }
}

/// The object an application stream's or a client's probe was bound to has left the registry: drop
/// the probe, or retire it until the server has removed it ([`AppProbe`]).
fn retire_probe(shared: &mut Shared, id: u32) {
    if let Some(probe) = shared.app_probes.remove(&id) {
        retire(shared, probe);
    }
}

fn retire(shared: &mut Shared, probe: AppProbe) {
    if !probe.removed.get() {
        shared.retired_probes.push(probe);
    }
}

/// Drop every retired probe the server has removed since the last tick ([`AppProbe`]).
fn drop_removed_probes(shared: &mut Shared) {
    shared.retired_probes.retain(|probe| !probe.removed.get());
}

/// The session is going, or a new one starting: every application stream, every client and their
/// probes belong to the one before. The GUI hears the list is empty on the next tick, if it had
/// been told of any. A probe dropped here may still send the server a `destroy` it answers with a
/// core error, but on a connection that is closing, whose listener goes with it.
fn forget_app_streams(shared: &mut Shared) {
    shared.app_probes.clear();
    shared.retired_probes.clear();
    shared.apps.clear();
}

fn on_global_remove(shared: &Rc<RefCell<Shared>>, id: u32) {
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    // An application's stream or a client goes with its probe; a node of ours is forgotten here
    // and may still be something below — the echo canceller's source.
    match guard.apps.remove(id) {
        Some(Tracked::Stream) => {
            log::debug!("application stream {id} went");
            retire_probe(&mut guard, id);
            route_pairs::stream_removed(&mut guard, id);
            return;
        }
        Some(Tracked::Client) => {
            retire_probe(&mut guard, id);
            return;
        }
        Some(Tracked::Own) => route_pairs::node_removed(&mut guard, id),
        None => {}
    }
    if guard.aec.source_removed(id) {
        log::debug!("the echo canceller's source went away ({id})");
        return;
    }
    if remove_card(&mut guard, id) {
        return;
    }
    // Every bound node's probe goes with its node, a device or not: one of WirePlumber's internal
    // Bluetooth nodes is bound like any other before its info says what it is, and is no device
    // by the time it goes ([`forget_internal_node`]).
    guard.node_probes.remove(&id);
    let Some(removed) = remove_device(&mut guard, id) else {
        return;
    };
    log::debug!(
        "{} disappeared: {} ({})",
        noun(removed.direction),
        removed.description,
        removed.name
    );
}

/// A registry global went away. Returns the device it was, when it was one.
///
/// The target may have just been unplugged. Re-running the rules attaches that lane's pair to
/// another device of its direction (`docs/spec/12-audio-io.md` §22); the other lane is not asked,
/// because nothing it depends on changed.
fn remove_device(shared: &mut Shared, id: u32) -> Option<DeviceInfo> {
    let index = shared.devices.iter().position(|d| d.object_id == id)?;
    let removed = shared.devices.remove(index);
    shared.node_probes.remove(&id);
    shared.needs_publish = true;
    shared.mark_lane_for_rules(removed.direction);
    let now = Instant::now();
    hold_for_return(shared, &removed, now);
    note_departure(shared, &removed, now);
    Some(removed)
}

/// A node went: when its card stays, remember it as expected back, for the lane of its direction
/// whether or not that lane is on it ([`Departure`]). A node that goes again starts its wait
/// afresh — it is the same device blinking, however often it does. Departures that say nothing
/// any more are let go of here too.
fn note_departure(shared: &mut Shared, removed: &DeviceInfo, now: Instant) {
    let departure = Departure::of(removed, &shared.cards, now);
    let departures = &mut shared.lanes.get_mut(removed.direction).departures;
    departures.retain(|known| known.name != removed.name && !known.over(now));
    if let Some(departure) = departure {
        departures.push(departure);
    }
}

/// A node was added: if it is one that went while its card stayed and is still expected, it is
/// back ([`Departure::just_back`]).
fn note_return(shared: &mut Shared, device: &DeviceInfo, now: Instant) {
    for departure in &mut shared.lanes.get_mut(device.direction).departures {
        if departure.name == device.name && departure.expected(now) {
            departure.back = Some(now);
        }
    }
}

/// A registry global went away. Returns whether it was a card.
///
/// A lane waiting for a node of the card waits no more, and its rules run on the next tick: the
/// node went because its card did, not because the card is changing profile
/// ([`Hold::card_present`]). Without this, a headset switched off — its sink reported gone before
/// its card, in the same batch — left the lane playing into nothing for the whole of
/// [`RETURN_WAIT`].
fn remove_card(shared: &mut Shared, id: u32) -> bool {
    let Some(index) = shared.cards.iter().position(|card| card.object_id == id) else {
        return false;
    };
    shared.cards.remove(index);
    shared.card_probes.remove(&id);
    // Its ports went with it. A node of it still listed — it goes in the same batch — is one no
    // card speaks for any more, and heard like any such node.
    shared.card_routes.remove(&id);
    refresh_availability(shared, |device| device.card_id == Some(id));
    log::debug!("card {id} went away");
    for direction in DeviceDirection::ALL {
        let lane = shared.lanes.get_mut(direction);
        // A node of it that went a moment before is not coming back: it went with its card.
        lane.departures
            .retain(|departure| departure.card_present(&shared.cards));
        let Some(hold) = lane.hold.take_if(|hold| !hold.card_present(&shared.cards)) else {
            continue;
        };
        log::info!(
            "{} went with its card: the {} lane stops waiting for it and chooses another device",
            hold.target,
            direction.key()
        );
        shared.mark_lane_for_rules(direction);
    }
    true
}

/// A node went. When it is the one its lane is on, and the card it belongs to is still here, the
/// lane waits for it to come back rather than have the rules move it elsewhere on the next tick
/// ([`Hold`]).
///
/// "The one its lane is on" is the pair's target, or — between pairs, while a pair that failed
/// waits out its backoff — the device the last pair was built on. A node the lane is not on
/// changes nothing about a hold the lane already has: it is news for the rules, which run once
/// the hold ends.
fn hold_for_return(shared: &mut Shared, removed: &DeviceInfo, now: Instant) {
    let direction = removed.direction;
    let lane = shared.lanes.get(direction);
    let on = lane
        .nodes
        .as_ref()
        .map(|nodes| nodes.target.as_str())
        .or(lane.last_target.as_deref());
    if !lane.enabled || on != Some(removed.name.as_str()) {
        return;
    }
    // The lane's departures do not name `removed` yet ([`note_departure`] runs after this), only
    // the nodes that went before it.
    let hold = Hold::after_removal(
        removed,
        on,
        &shared.cards,
        &shared.devices,
        &lane.departures,
        lane.hold.as_ref(),
        now,
    );
    match &hold {
        Some(hold) if lane.hold.as_ref() != Some(hold) => log::info!(
            "{} went, but its card is still here: the {} lane waits up to {RETURN_WAIT:?} for it \
             to come back",
            removed.name,
            direction.key()
        ),
        Some(_) => {}
        None => log::info!(
            "{} went, and nothing it belonged to is left: the {} lane chooses another device",
            removed.name,
            direction.key()
        ),
    }
    shared.lanes.get_mut(direction).hold = hold;
}

/// A bound card reported its info: keep its Bluetooth address, which its registry global does not
/// carry, and whether it is Bluetooth at all. An info that does not say leaves what is known as it
/// is — a card's info is sent again on every change to its profiles and routes, and says what
/// changed.
fn on_card_info(shared: &Rc<RefCell<Shared>>, reported: Card) {
    if reported.bluez_address.is_none() && !reported.bluetooth {
        return;
    }
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    let Some(card) = guard
        .cards
        .iter_mut()
        .find(|card| card.object_id == reported.object_id)
    else {
        return;
    };
    if reported.bluez_address.is_some() {
        card.bluez_address = reported.bluez_address;
    }
    // The registry global says `device.api` already, so this is news only for a card announced
    // without it; kept once known, like the address.
    let newly_bluetooth = reported.bluetooth && !card.bluetooth;
    card.bluetooth |= reported.bluetooth;
    if newly_bluetooth {
        adopt_bluetooth_card(&mut guard, reported.object_id);
    }
}

/// A card turned out to be Bluetooth: every node that names it in `device.id` is a Bluetooth
/// node, whatever its own properties have had time to say ([`DeviceInfo::on_bluetooth_card`]).
fn adopt_bluetooth_card(shared: &mut Shared, card_id: u32) {
    let changed: Vec<u32> = shared
        .devices
        .iter_mut()
        .filter(|device| device.card_id == Some(card_id))
        .filter_map(|device| device.on_bluetooth_card().then_some(device.object_id))
        .collect();
    for object_id in changed {
        device_reclassified(shared, object_id);
    }
}

/// A bound node reported its properties. What they say about the Bluetooth link behind it — which
/// its registry global does not carry — is taken ([`DeviceInfo::learn_bluetooth`]), and one of
/// WirePlumber's internal Bluetooth nodes stops being a device ([`forget_internal_node`]).
fn on_node_bluetooth(
    shared: &Rc<RefCell<Shared>>,
    object_id: u32,
    reported: BluezFacts,
    form_factor: FormFactor,
) {
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    if reported.internal {
        forget_internal_node(&mut guard, object_id);
        return;
    }
    let Some(device) = guard.devices.iter_mut().find(|d| d.object_id == object_id) else {
        return;
    };
    if device.learn_bluetooth(reported, form_factor) {
        device_reclassified(&mut guard, object_id);
    }
}

/// What a device is has changed — a headset's now, a plain microphone no more — while its format
/// has not: the GUI's list says so, and a pair attached to it that was built for what it was
/// before is rebuilt for what it is ([`PairFormat::source_rate`]).
fn device_reclassified(shared: &mut Shared, object_id: u32) {
    let Some(device) = shared.devices.iter().find(|d| d.object_id == object_id) else {
        return;
    };
    log::debug!(
        "{}: {}{}",
        device.name,
        device.form_factor.key(),
        match device.native_rate_hz() {
            Some(rate) if device.bluez_headset => format!(", a Bluetooth headset at {rate} Hz"),
            _ => String::new(),
        }
    );
    let wanted = PairFormat::for_target(device, shared.clock.rate());
    let stale = shared
        .lanes
        .get(device.direction)
        .nodes
        .as_ref()
        .is_some_and(|nodes| nodes.target == device.name && nodes.format != wanted);
    let (name, direction) = (device.name.clone(), device.direction);
    shared.needs_publish = true;
    if stale {
        log::info!(
            "{name} carries {}, not what the {} lane's pair was built for; rebuilding it",
            wanted.source_rate.map_or_else(
                || "the stream's rate".to_owned(),
                |rate| format!("{rate} Hz")
            ),
            direction.key()
        );
        shared.mark_lane_for_rules(direction);
    }
}

/// A node the registry listed as a device turned out, from its info, to be one of WirePlumber's
/// internal Bluetooth nodes ([`BluezFacts::internal`]): the SCO source behind WirePlumber 0.5's
/// loopback microphone, which carries the same `bluez_input.<addr>.0` name WirePlumber 0.4 gave the
/// microphone itself. It leaves the list without a trace — no wait for it to come back, as a
/// device that went would get — and its lane's rules run again, in case they took it in the moment
/// before its info arrived. The node's probe stays until the node goes: this runs inside the
/// probe's own callback.
fn forget_internal_node(shared: &mut Shared, object_id: u32) {
    let Some(index) = shared.devices.iter().position(|d| d.object_id == object_id) else {
        return;
    };
    let node = shared.devices.remove(index);
    log::debug!(
        "{} is one of WirePlumber's internal Bluetooth nodes, not a device",
        node.name
    );
    shared.needs_publish = true;
    shared.mark_lane_for_rules(node.direction);
}

/// A bound node reported its Bluetooth address, which its registry global does not carry either:
/// the second way to tell which card it belongs to ([`DeviceInfo::card_present`]).
fn on_node_address(shared: &Rc<RefCell<Shared>>, object_id: u32, address: Option<String>) {
    let Some(address) = address else {
        return;
    };
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    if let Some(device) = guard.devices.iter_mut().find(|d| d.object_id == object_id) {
        device.bluez_address = Some(address);
    }
}

/// A bound node reported which device of its card it is — `card.profile.device`, which its
/// registry global does not carry — and so which of its card's ports says whether it can be heard.
fn on_node_profile_device(
    shared: &Rc<RefCell<Shared>>,
    object_id: u32,
    profile_device: Option<u32>,
) {
    let Some(profile_device) = profile_device else {
        return;
    };
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    let Some(device) = guard.devices.iter_mut().find(|d| d.object_id == object_id) else {
        return;
    };
    if device.profile_device == Some(profile_device) {
        return;
    }
    device.profile_device = Some(profile_device);
    refresh_availability(&mut guard, |device| device.object_id == object_id);
    for direction in DeviceDirection::ALL {
        follow_port(&mut guard, direction);
    }
}

/// A card reported one of its routes: keep it ([`CardRoutes::learn`]), and tell the rules of any
/// lane whose devices on the card became heard or went silent.
fn on_card_route(
    shared: &Rc<RefCell<Shared>>,
    card_id: u32,
    list: RouteList,
    index: u32,
    route: Route,
) {
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    // Only a listed card is kept a list for: [`remove_card`] is what empties one, and it only
    // knows the cards the registry announced.
    if !guard.cards.iter().any(|card| card.object_id == card_id) {
        return;
    }
    guard
        .card_routes
        .entry(card_id)
        .or_default()
        .learn(list, index, route);
    refresh_availability(&mut guard, |device| device.card_id == Some(card_id));
    // Headphones plugged into a sink that was the speakers: the volume follows at once, not a tick
    // later with the headphones playing at the speakers' level in between.
    for direction in DeviceDirection::ALL {
        follow_port(&mut guard, direction);
    }
}

/// Draw [`DeviceInfo::available`] again for the devices `which` picks, from what their cards have
/// said ([`routes::node_available`]).
///
/// A device that became heard, or went silent, is news for its lane's rules: a monitor plugged in
/// is an HDMI sink arriving — which, ranked above the lane's device, takes the lane — and one
/// unplugged is the lane's device gone. Nothing else is: the GUI's list shows the node either way.
fn refresh_availability(shared: &mut Shared, which: impl Fn(&DeviceInfo) -> bool) {
    let mut changed = Vec::new();
    for device in shared.devices.iter_mut().filter(|device| which(device)) {
        let available = routes::node_available(
            device.card_id.and_then(|id| shared.card_routes.get(&id)),
            device.profile_device,
        );
        if device.available != available {
            device.available = available;
            changed.push((device.direction, device.name.clone(), available));
        }
    }
    for (direction, name, available) in changed {
        if available {
            log::info!("{name}: something is plugged in behind it now");
        } else {
            log::info!(
                "{name}: nothing is plugged in behind it; passed over while anything is heard"
            );
        }
        shared.mark_lane_for_rules(direction);
    }
}

/// A bound node reported its format. Fill in what the registry could not tell us — and whatever
/// has changed since.
///
/// Arrives once per node shortly after it appears, and again on every change to the node's info:
/// its state, as it is suspended and woken, which says nothing new about its format; and a profile
/// switch on an ALSA card, which really does change the channel count under a running stream.
/// 0.3.0 took only the first report, which made it immune to the first kind and deaf to the
/// second: a card switched from stereo to 5.1 stayed stereo to FxSound until it was unplugged. So
/// each report is compared with what is known instead. The same format again is churn and changes
/// nothing; a different one is the device's new truth.
fn on_node_info(
    shared: &Rc<RefCell<Shared>>,
    object_id: u32,
    channels: u32,
    positions: Option<ChannelMap>,
) {
    // A report without a channel count says nothing about the format; what was learned stands.
    if channels == 0 {
        return;
    }
    let positions = positions.unwrap_or_else(|| ChannelMap::default_for(channels));
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    let Some(device) = guard.devices.iter_mut().find(|d| d.object_id == object_id) else {
        return;
    };
    if device.channels == channels && device.positions == positions {
        return;
    }
    let before = device.channels;
    device.channels = channels;
    device.positions = positions;
    let device = device.clone();

    log::debug!(
        "{}: {channels} channels, {}",
        device.name,
        device.positions.to_property_value()
    );
    guard.needs_publish = true;

    // Rebuild a pair attached to this device when it was built for something else: a device
    // selected before its info arrived, whose pair runs the stereo fallback, or a card whose
    // profile just changed. Only the device a lane's pair is attached to matters — reacting to
    // every device of the direction would rebuild the running stream because some *other* sink
    // reported something — and only a change the pair's format actually follows: nine channels
    // and ten are both the eight the pair is clamped to.
    let wanted = PairFormat::for_target(&device, guard.clock.rate());
    let stale = guard
        .lanes
        .get(device.direction)
        .nodes
        .as_ref()
        .is_some_and(|nodes| nodes.target == device.name && nodes.format != wanted);
    if stale {
        log::info!(
            "{} reports {channels} channels (it had {before}); rebuilding the {} lane's pair for it",
            device.name,
            device.direction.key()
        );
        guard.mark_lane_for_rules(device.direction);
    }
}

/// One property event of the `default` or the `settings` metadata object (`default` says which).
///
/// Keys about one object rather than about the session are, in the `default` object, where each
/// application stream is to go ([`route_pairs`]); a `None` key clears every key of the subject.
/// Everything about the session is on the core's id, and [`on_metadata_property`]'s.
fn on_metadata_event(
    shared: &Rc<RefCell<Shared>>,
    default: bool,
    subject: u32,
    key: Option<&str>,
    value: Option<&str>,
) {
    if subject == pw::core::PW_ID_CORE {
        on_metadata_property(shared, key, value);
        return;
    }
    if default
        && key.is_none_or(|key| key == crate::app_routes::TARGET_OBJECT_KEY)
        && let Ok(mut guard) = shared.try_borrow_mut()
    {
        route_pairs::metadata_target(&mut guard, subject, value.filter(|_| key.is_some()));
    }
}

fn on_metadata_property(shared: &Rc<RefCell<Shared>>, key: Option<&str>, value: Option<&str>) {
    let Some(key) = key else {
        return;
    };
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    for direction in DeviceDirection::ALL {
        if key == devices::default_key(direction) {
            let current = value.and_then(devices::parse_default_node_name);
            if direction == DeviceDirection::Output {
                // A recorder of the default sink's monitor records FxSound exactly while this is
                // FxSound's sink (`crate::app_streams`).
                guard.apps.default_sink_is(current.as_deref());
            }
            guard.defaults.get_mut(direction).current = current;
            guard.needs_publish = true;
            // A direction's default moving is news for that direction's lane and for nothing
            // else: the default sink says nothing about which microphone to hear.
            guard.mark_lane_for_rules(direction);
            return;
        }
        if key == devices::configured_default_key(direction) {
            let configured = value.and_then(devices::parse_default_node_name);
            let holding = configured.as_deref() == Some(our_node_name(direction));
            // A key naming a node of ours that does not exist, for a lane that is not going to
            // claim it. The user cannot have picked a node that is not there, so it is stale.
            // A lane that will claim it — enabled, and wanting the default — adopts it instead:
            // its pair is about to come up under that name, and claiming it would change
            // nothing. And a key that names a node of ours that *is* there was picked by hand,
            // whatever the lane wants, and is left to the user — as is one the user had picked
            // before the connection went, read back before the pair is ([`Lane::kept_by_hand`]).
            //
            // "Before the pair is" is a promise, not a race: after a connect, no lane builds a
            // pair until what the `default` object holds has been read ([`Barrier`]).
            let lane = guard.lanes.get(direction);
            let stands_behind = lane.enabled && (lane.want_default || lane.kept_by_hand);
            let disowned = holding && lane.nodes.is_none() && !stands_behind;
            let state = guard.defaults.get_mut(direction);
            state.holding = holding;
            state.disowned = disowned;
            state.configured = configured;
            return;
        }
    }
    // The `settings` object. A graph that moved to another rate is news for the output lane only,
    // and only for a pair on a sink that publishes no rate of its own — the rules find out which
    // ([`apply_rules`] compares the pair's format). The capture stream asks for 48 kHz whatever
    // the clock does.
    let before = guard.clock.rate();
    guard.clock.learn(key, value);
    let now = guard.clock.rate();
    if now != before {
        log::info!("the graph's clock runs at {now} Hz (was {before} Hz)");
        guard.mark_lane_for_rules(DeviceDirection::Output);
    }
}

// ---------------------------------------------------------------------------------------------
// The session default
// ---------------------------------------------------------------------------------------------

/// Whether `node_name` is one of the nodes FxSound makes ([`is_fxsound_node`]): a lane's, the echo
/// canceller's, a per-application route's. Never a default to remember, nor one to hand back to:
/// a route's sink is an `Audio/Sink` too, and one WirePlumber fell back to must not be written to
/// the settings file as the device the user had.
fn is_ours(node_name: &str) -> bool {
    is_fxsound_node(node_name)
}

/// Become the session default for a lane's direction, politely: remember what was there first.
///
/// Each lane holds its own claim — `default.configured.audio.sink` for the output lane,
/// `default.configured.audio.source` for the input lane — so claiming one never touches the
/// other.
fn claim_default(shared: &mut Shared, direction: DeviceDirection) {
    let ours = our_node_name(direction);
    if shared.defaults.get(direction).holding {
        return;
    }
    // Read and persist before writing (`docs/spec/12-audio-io.md` §21.1). The configured key is
    // the user's own choice and is preferred; the current key is what WirePlumber picked when
    // nobody chose. A stale configured value that already names us — a previous FxSound that was
    // killed rather than quit, which WirePlumber's state file preserves — is skipped, so the
    // memory never says "the default before us was us". `original_default` is only ever filled
    // once; `most_recent_default` follows the live value.
    //
    // "Us" is every node FxSound makes, not only this lane's virtual device. The echo canceller's
    // source is an `Audio/Source` with a session priority of its own, and WirePlumber falls back
    // to it as the current default source whenever the configured one has vanished and nothing
    // else is left — a microphone unplugged while the input lane held the default. Taken here, it
    // would go to the settings file as the device to hand the default back to: a node that is gone
    // the moment echo cancellation is off.
    let previous = {
        let state = shared.defaults.get(direction);
        [state.configured.as_deref(), state.current.as_deref()]
            .into_iter()
            .flatten()
            .find(|name| !is_ours(name))
            .map(str::to_owned)
    };
    if let Some(previous) = previous {
        let memory = shared.memory.get_mut(direction);
        if memory.original_default.is_empty() {
            memory.original_default.clone_from(&previous);
        }
        memory.most_recent_default.clone_from(&previous);
        // Say it out loud, so it reaches the settings file. This memory lives on the audio
        // thread's heap and is exactly what a kill destroys.
        shared.notify(AudioToUi::RememberedDefault {
            direction,
            node_name: previous,
        });
    }
    if write_configured_default(shared, direction, ours) {
        shared.defaults.get_mut(direction).holding = true;
        log::info!("FxSound is now the default {}", noun(direction));
    }
}

/// Hand one direction's default back to a real device. Returns whether the key was written.
///
/// Only ever writes a device we actually remember and that is actually present
/// (`sndDevicesRestoreDefaultDevice`, `sndDevicesSetupDevices.cpp:617-630`); with nothing to hand
/// back to, the key is left alone so WirePlumber picks by priority rather than being pointed at a
/// node that is about to vanish.
fn release_default(shared: &mut Shared, direction: DeviceDirection) -> bool {
    if !shared.defaults.get(direction).holding {
        return false;
    }
    let candidate = devices::restore_default_candidate(
        shared.memory.get(direction),
        &shared.devices,
        direction,
    );
    let written = match candidate {
        Some(candidate) => {
            let written = write_configured_default(shared, direction, &candidate);
            if written {
                log::info!(
                    "handing the default {} back to {candidate}",
                    noun(direction)
                );
            }
            written
        }
        None => {
            log::warn!(
                "nothing remembered to hand the default {} back to; leaving it to WirePlumber",
                noun(direction)
            );
            false
        }
    };
    shared.defaults.get_mut(direction).holding = false;
    written
}

/// Hand back every claim on a default that no lane of ours stands behind
/// ([`DefaultState::disowned`]).
///
/// There are two ways to come by one. A run that was killed while a lane held the default leaves
/// the key naming a node that died with it, and WirePlumber's state file keeps it that way; if the
/// lane that held it is not attached this time — the microphone is not plugged in, so the input
/// lane is never switched on — nothing else would ever hand it back, and the default source would
/// name nothing for the whole session. And a lane detached, or opted out of the default, while the
/// connection was down could not hand its claim back then (`disconnect` forgets which defaults
/// were held), and finds it waiting when the connection returns.
///
/// Only once there is somewhere to hand it to: with none of the devices the lane remembers
/// present, the key is left as it is and asked about again on the next tick, rather than released
/// into nothing and forgotten — the device being plugged back in is exactly what it waits for. A
/// lane that has since been attached, and wants the default, stands behind the claim again and
/// adopts it. And the supervisor asks only once the GUI has had its chance to attach that lane
/// from the device list ([`Shared::gui_has_had_its_chance`]), so that a claim about to be adopted
/// is not handed back first.
fn hand_back_disowned_defaults(shared: &mut Shared) {
    for direction in DeviceDirection::ALL {
        let state = shared.defaults.get(direction);
        if !(state.disowned && state.holding) {
            continue;
        }
        let lane = shared.lanes.get(direction);
        if lane.enabled && lane.want_default {
            shared.defaults.get_mut(direction).disowned = false;
            continue;
        }
        let somewhere = devices::restore_default_candidate(
            shared.memory.get(direction),
            &shared.devices,
            direction,
        );
        if somewhere.is_none() {
            continue;
        }
        log::info!(
            "the default {} names FxSound, and no lane is claiming it",
            noun(direction)
        );
        release_default(shared, direction);
        shared.defaults.get_mut(direction).disowned = false;
    }
}

/// [`release_default`] for both directions — the exit path, where every default we hold, one per
/// lane that claimed its own, has to go back before the metadata proxy does. Returns whether
/// anything was written, i.e. whether there is a write to wait for.
fn release_all_defaults(shared: &mut Shared) -> bool {
    let output = release_default(shared, DeviceDirection::Output);
    let input = release_default(shared, DeviceDirection::Input);
    output || input
}

/// Write `default.configured.audio.sink` / `.source` — never `default.audio.*`, which is
/// WirePlumber's to own (`docs/spec/12-audio-io.md` §21.6).
fn write_configured_default(shared: &Shared, direction: DeviceDirection, node_name: &str) -> bool {
    let Some(metadata) = shared.session.as_ref().and_then(|s| s.metadata.as_ref()) else {
        log::warn!(
            "no `default` metadata object (a bare pipewire with no session manager?); \
             the user will have to select FxSound in their sound settings"
        );
        return false;
    };
    metadata.set_property(
        0,
        devices::configured_default_key(direction),
        Some("Spa:String:JSON"),
        Some(&devices::default_node_value(node_name)),
    );
    true
}

// ---------------------------------------------------------------------------------------------
// Device selection and node creation, per lane
// ---------------------------------------------------------------------------------------------

/// Take back whatever DSP state a dropped NODE 1 handed through its lane's recycle channel, into
/// that lane's slot.
///
/// `SinkData::drop` sends synchronously on an unbounded channel, so right after a `Nodes` is
/// dropped the state is already waiting here; draining before every `build_nodes` is what lets a
/// pair be rebuilt within one supervisor tick instead of failing once and waiting for the next.
fn drain_recycled_dsp(shared: &mut Shared) {
    for (direction, lane) in shared.lanes.iter_mut() {
        while let Ok(dsp) = lane.recycle.1.try_recv() {
            debug_assert_eq!(
                dsp.direction(),
                direction,
                "a lane's recycle channel only ever carries that lane's DSP"
            );
            lane.dsp = Some(dsp);
        }
    }
}

/// Apply the events waiting for every lane whose DSP is on the main loop.
///
/// Each lane has its own event queue, and a lane with no nodes has no audio thread draining it.
/// Left alone, a queue the GUI keeps writing to — a power toggle resets both chains — would fill
/// and start dropping events long before the lane was attached again. Applied here instead, on
/// the main loop where the DSP is, the event has the effect it would have had on the next block,
/// and the queue stays empty for when the lane does run.
fn apply_idle_lane_events(shared: &mut Shared) {
    for (_, lane) in shared.lanes.iter_mut() {
        if let Some(dsp) = lane.dsp.as_mut() {
            dsp.drain_events();
        }
    }
}

/// Drop the voice engines the audio thread has retired. This is the whole point of the return
/// channel: the drop happens here, on the main loop, and never in `process()`.
fn drain_retired_chains(shared: &mut Shared) {
    while shared.handover.retired.try_recv().is_ok() {}
}

/// Run the voice chain a preset names. The name is resolved here, once, and an unknown one runs
/// the `voice` chain and says so: a preset written for a later version that knows more chains
/// must still load (`fxsound_preset::input::CHAIN_NAMES` is the list this build matches, and
/// `ChainSpec::by_name` is what it matches against).
fn set_input_chain(shared: &mut Shared, name: &str) {
    let spec = ChainSpec::by_name(name).unwrap_or_else(|| {
        log::warn!("the voice preset names a chain this build does not know ({name:?}); running the voice chain");
        ChainSpec::voice()
    });
    if spec == shared.input_spec {
        return;
    }
    log::info!("voice chain: {name}");
    shared.input_spec = spec;
    shared.input_spec_pending = true;
    reconcile_input_chain(shared);
}

/// Bring the voice engine up to date with `input_spec`, wherever the input lane's DSP is.
///
/// Three places it can be. On the main loop — the input lane has no nodes, whatever the output
/// lane is doing: rebuilt in place, which is the only place a rebuild is allowed. With the input
/// lane's pair running: a replacement is built here at that pair's negotiated format and sent
/// over; the audio thread swaps it in on its next block and retires the old one back to this loop.
/// On its way back through the recycle channel: owed, and settled in place on the tick that finds
/// it back — or by the `build_nodes` that takes it next, whichever comes first — so
/// `input_spec_pending` stays set until then and this is a cheap check per tick.
///
/// The input lane only, throughout: the output lane runs the music chain, which has no stages to
/// reorder, and its pair has nothing to say about the voice engine's format.
///
/// Called from the control message and from every supervisor tick, so a replacement that found
/// the slot full — the user picked two presets within one block — goes on the next tick.
fn reconcile_input_chain(shared: &mut Shared) {
    drain_retired_chains(shared);
    if !shared.input_spec_pending {
        return;
    }
    let spec = shared.input_spec;
    let lane = &mut shared.lanes.input;
    if let Some(dsp) = lane.dsp.as_mut().and_then(LaneDsp::as_input_mut) {
        dsp.set_spec(spec);
        shared.input_spec_pending = false;
        return;
    }
    if lane.nodes.is_none() {
        return;
    }
    let rate = lane.counters.sample_rate.load(Ordering::Relaxed) as f32;
    let channels = lane.counters.channels.load(Ordering::Relaxed) as usize;
    let engine = Box::new(InputEngine::new_with_spec(
        rate,
        MAX_QUANTUM_FRAMES,
        channels,
        spec,
    ));
    match shared.handover.replacement.try_send(engine) {
        Ok(()) => shared.input_spec_pending = false,
        Err(TrySendError::Full(_)) => {
            // The previous replacement has not been adopted yet; this one is dropped here, on the
            // main loop, and rebuilt on the next tick.
        }
        Err(TrySendError::Disconnected(_)) => {
            // The receiver lives in the DSP state, which is never dropped while the engine runs;
            // if it is gone, so is the recycle path, and that error has already been logged.
            shared.input_spec_pending = false;
        }
    }
}

/// Destroy a lane's pair of nodes and recover its DSP state, **keeping** the lane's default. For
/// the transient rebuilds — a new target, a format mismatch, a stream in error — where the pair is
/// about to come straight back under the same name. The other lane's pair is not touched.
fn drop_nodes(shared: &mut Shared, direction: DeviceDirection) {
    retire_volume(shared, direction);
    let lane = shared.lanes.get_mut(direction);
    lane.nodes = None;
    lane.built_at = None;
    lane.status.clear();
    // Mismatches the pair counted on its way out belong in the report, not in a trigger that
    // would tear down the next pair for them.
    lane.format_mismatches_total += lane.counters.format_mismatches.swap(0, Ordering::Relaxed);
    drain_recycled_dsp(shared);
}

/// Hand a lane's default back, *then* destroy its nodes — the order `docs/spec/12-audio-io.md`
/// §21.5 requires. For the cases where the pair is not coming back as it was: the lane detached,
/// or its rules ended in an error. Only this lane's default is handed back.
fn teardown_nodes(shared: &mut Shared, direction: DeviceDirection) {
    release_default(shared, direction);
    drop_nodes(shared, direction);
    // Whatever pair comes next attaches afresh, and claims afresh. A default the user had given
    // it by hand went back with this one: a reconnect that still finds it hands it back as well.
    let lane = shared.lanes.get_mut(direction);
    lane.last_target = None;
    lane.kept_by_hand = false;
    // And it waits for nothing: there is no pair left to keep for a node that went.
    lane.hold = None;
}

/// What one run of a lane's rules chose, and what it spent choosing it ([`choose`]).
struct Choice {
    selection: Result<devices::Selection, AudioError>,
    /// The user's pick was still fresh ([`Preference::fresh_pick`]).
    fresh_pick: bool,
    /// The ranking had just come into force ([`Preference::start_over`]).
    start_over: bool,
}

/// The choosing half of [`apply_rules`]: ask the device rules for the lane's device, then
/// remember what they saw, and spend the pick and the ranking's coming into force they have now
/// had their run on. [`apply_rules`] gives either back when the pair it builds on the choice
/// fails, so that the retry chooses the same way.
fn choose(shared: &mut Shared, direction: DeviceDirection) -> Choice {
    let ours = our_node_name(direction);
    let selection = devices::choose_device(
        &shared.devices,
        direction,
        ours,
        shared.defaults.get(direction).current.as_deref(),
        &shared.lanes.get(direction).previous_names,
        shared.memory.get(direction),
        shared.preference.get(direction),
    );
    // What the rules chose among, so that a device the next run finds among them and not here —
    // one plugged in, or one that has become heard while another could be — is a new one to
    // them. When nothing could be heard the silent nodes are already here, and a port that wakes
    // is not new: it is simply the only candidate. And every node of the direction that went
    // while its card stayed, until its wait is up: back under its name, it is the device it was,
    // not one just plugged in ([`Departure`]).
    let now = Instant::now();
    let mut seen: Vec<String> = devices::candidates(&shared.devices, direction, ours)
        .into_iter()
        .map(|d| d.name.clone())
        .collect();
    let lane = shared.lanes.get_mut(direction);
    lane.departures.retain(|departure| !departure.over(now));
    for departure in &lane.departures {
        if departure.expected(now) && !seen.contains(&departure.name) {
            seen.push(departure.name.clone());
        }
    }
    lane.previous_names = seen;
    // The user's pick has had its run. It stays fresh only if the pair built on it fails, so that
    // the retry honours it too. So has a ranking that just came into force.
    let preference = shared.preference.get_mut(direction);
    Choice {
        selection,
        fresh_pick: std::mem::take(&mut preference.fresh_pick),
        start_over: std::mem::take(&mut preference.start_over),
    }
}

/// Choose a lane's device and make sure its pair is attached to it (`docs/spec/12-audio-io.md`
/// §19.5, per direction as §28.5 describes).
///
/// A disabled lane is left alone, whatever asked: it has no pair and must not grow one.
fn apply_rules(shared: &mut Shared, direction: DeviceDirection) {
    if !shared.lanes.get(direction).enabled {
        return;
    }
    // A lane waiting for the node it was on chooses nothing until the node is back or the wait is
    // up ([`Hold`]), whoever asked — the supervisor does not ask meanwhile, but the echo canceller
    // going does. The ask is kept for when it ends.
    if held(shared, direction, Instant::now()) {
        shared.lanes.get_mut(direction).needs_rules = true;
        return;
    }
    let Choice {
        selection,
        fresh_pick,
        start_over,
    } = choose(shared, direction);

    let selection = match selection {
        Ok(selection) => selection,
        Err(error) => {
            // No device to attach to: the pair goes, and so does this lane's claim on the default —
            // leaving `default.configured.audio.*` pointing at a node that no longer exists is the
            // one thing §21 forbids. Nothing to retry: the registry says when a device appears.
            teardown_nodes(shared, direction);
            shared.report_error(direction, error);
            return;
        }
    };

    let Some(target) = shared
        .devices
        .iter()
        .find(|d| d.direction == direction && d.name == selection.target)
        .cloned()
    else {
        shared.report_error(direction, AudioError::DeviceNotPresent);
        return;
    };

    // Attached already means attached to this device *at the format it wants now*. The name
    // alone is not enough: a card that switched profile, a device whose info arrived after its
    // pair was built on the stereo fallback, and a graph forced to another rate all keep the name
    // and change the format, and in 0.3.0 the name matched, the rules returned here, and the
    // rebuild `on_node_info` asked for never happened.
    let format = PairFormat::for_target(&target, shared.clock.rate());
    // The microphone itself, or the echo canceller's source in front of it while echo cancellation
    // runs for this microphone. A pair recording the wrong one of the two is rebuilt like a pair on
    // the wrong device: the capture stream's target is fixed when it is made.
    let route = match direction {
        DeviceDirection::Input => shared.aec.route(&target.name),
        DeviceDirection::Output => None,
    };
    // And attached to this very node, not only to one of its name: a node that went and came back
    // — a Bluetooth headset between profiles — is a new node, and WirePlumber never links a pair's
    // old stream to it ([`is_same_node`]).
    let already = shared
        .lanes
        .get(direction)
        .nodes
        .as_ref()
        .is_some_and(|nodes| {
            is_same_node(&nodes.target, nodes.target_serial, &target)
                && nodes.format == format
                && nodes.via == route
        });
    if already {
        // Nothing to do. An error this lane reported is forgotten once its pair has proved
        // itself (`Lane::forgive_if_stable`), not because the rules ran again in the meantime.
        return;
    }

    // Rebuilding replaces both nodes. The old pair goes first so its DSP state — filter history,
    // scratch — comes back through the recycle channel for the new pair to adopt, and so the
    // server never sees two nodes with the same `node.name`. The default is kept: the same node
    // is about to reappear under the same name, and `default.configured.audio.*` survives the gap.
    drop_nodes(shared, direction);
    match build_nodes(shared, &target, format, route) {
        Ok(nodes) => {
            log::info!(
                "{} {} ({} ch @ {} Hz){}",
                match direction {
                    DeviceDirection::Output => "rendering to",
                    DeviceDirection::Input => "capturing from",
                },
                target.description,
                nodes.format.channels,
                nodes.format.rate,
                if nodes.via.is_some() {
                    ", through the echo canceller"
                } else {
                    ""
                }
            );
            // The lane's error and backoff stay until this pair has proved itself
            // (`Lane::forgive_if_stable`): one that fails again a moment from now is the same
            // failure, not news, and not a reason to start again from 200 ms.
            let lane = shared.lanes.get_mut(direction);
            lane.nodes = Some(nodes);
            lane.built_at = Some(Instant::now());
            // A pair rebuilt on the device the lane was already on is a repair — after a stream
            // error or a format change, or the echo canceller coming or going under the capture
            // stream, which the *other* lane's device can cause — and leaves the default exactly
            // as it found it. Still ours if it was: the claim survives the gap under the same
            // name. The user's if they had moved it away from FxSound meanwhile, which the lane's
            // own rules respect (the `already` return above) and a repair must not undo. Only a
            // pair attached to a device claims.
            let repair = lane.last_target.as_deref() == Some(target.name.as_str());
            lane.last_target = Some(target.name.clone());
            shared.state = State::Running;
            shared.connect_attempts = 0;
            // Claim before committing: `claim_default` records the default that was there *before*
            // us in `original_default` / `most_recent_default`, and `commit` only fills those slots
            // when they are still empty — so this order keeps them honest on a first run.
            if shared.lanes.get(direction).want_default && !repair {
                claim_default(shared, direction);
            }
            devices::commit(shared.memory.get_mut(direction), &selection);
        }
        Err(error) => {
            // Not left for an unrelated registry event to retry: nothing may ever come, and the
            // lane would sit without a pair until the user unplugged something.
            shared.report_error(direction, error);
            shared.lanes.get_mut(direction).retry_later(Instant::now());
            if fresh_pick && selection.target == shared.memory.get(direction).user_selected {
                shared.preference.get_mut(direction).fresh_pick = true;
            }
            // Nor has the ranking's choice been made until a pair stands on it: the retry
            // chooses by rank again, not the device the Windows rules had.
            shared.preference.get_mut(direction).start_over |= start_over;
        }
    }
}

/// Create both nodes of the lane of the target's direction, on that lane's ring, counters and DSP.
///
/// `format` — [`PairFormat::for_target`], decided once by the rules — is declared on **both**
/// nodes so the ring is always read at the stride it was written at. That replaces §8 of the
/// Windows design wholesale — no `IPolicyConfigVista::SetDeviceFormat`, no rate pushed onto a
/// driver, no zero-order-hold upsampler. If the real device wants something else, the adapter in
/// front of it converts — which is also how a mono microphone arrives here as the stereo pair the
/// DSP runs on, and how the output lane's stereo pair reaches a mono headset: NODE 1 stays stereo
/// ([`DeviceInfo::clamped_channels`]) and the playback stream, which leaves `stream.dont-remix`
/// off, is down-mixed by its adapter. Nothing is refused for being mono: the Windows rules refused
/// a mono sink only to dodge a bug in their own driver (`sndDevices.h:32-39`).
///
/// `via` is the node the input lane's capture stream records from instead of the microphone — the
/// echo canceller's source ([`aec::route`]) — or `None` to record the microphone itself. The pair's
/// format is the microphone's either way: the canceller runs at 48 kHz too, and PipeWire converts
/// its channels to the pair's as it does the microphone's.
fn build_nodes(
    shared: &mut Shared,
    target: &DeviceInfo,
    format: PairFormat,
    via: Option<&'static str>,
) -> Result<Nodes, AudioError> {
    let Some(session) = shared.session.as_ref() else {
        return Err(AudioError::PipewireDisconnected);
    };
    let core = session.core.clone();

    let direction = target.direction;
    let PairFormat {
        channels,
        rate,
        positions,
        source_rate,
    } = format;
    let quantum = DEFAULT_QUANTUM_FRAMES.min(MAX_QUANTUM_FRAMES as u32);
    let latency = pair_latency(rate);

    let (passive, pace) = idle_plan(direction, shared.link_groups_scheduled.get());
    let wake = pace.and(shared.wake.clone());

    // ---- NODE 1: the node the DSP runs in ----------------------------------------------------
    //
    // Output: the virtual sink. A `pw_stream` with `media.class = "Audio/Sink"` *is* a sink node;
    // there is nothing else to do to make applications able to render into it, and nothing to
    // autoconnect — WirePlumber links clients *to* it.
    //
    // Input: a capture stream on the chosen microphone, targeted by name and autoconnected, and
    // passive where the server runs the pair together (U19).
    let (first_name, first_props, first_flags) = match direction {
        DeviceDirection::Output => (
            SINK_NODE_NAME,
            virtual_node_props(
                direction,
                shared.language.as_deref(),
                channels,
                rate,
                &positions,
                &latency,
            ),
            StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
        DeviceDirection::Input => (
            CAPTURE_NODE_NAME,
            stream_props(direction, via.unwrap_or(&target.name), &latency, passive),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
    };
    let first = pw::stream::StreamRc::new(core.clone(), first_name, first_props)
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    // Only now take the lane's DSP state: from here on every failure path drops it inside
    // `SinkData`, whose `Drop` hands it straight back through the lane's recycle channel.
    drain_recycled_dsp(shared);
    let Some(mut dsp) = shared.lanes.get_mut(direction).dsp.take() else {
        log::error!(
            "the {} lane's DSP state has not come back from its previous pair of nodes",
            direction.key()
        );
        return Err(AudioError::PipewireDisconnected);
    };
    // The engine is on the main loop, so a voice chain the preset named while it was away with
    // the previous pair can be built in place — a no-op when it already runs that chain.
    if let Some(input) = dsp.as_input_mut() {
        input.set_spec(shared.input_spec);
        shared.input_spec_pending = false;
    }
    dsp.set_format(rate as f32, channels as usize);
    // The capture stream negotiates 48 kHz whatever the microphone runs at, so the format alone
    // cannot tell the voice chain how much bandwidth is in the signal; the device's properties
    // can, and the adaptive de-esser places its corner from them (`docs/0.4.0-design.md` §6).
    // The music chain has no use for it and its lane ignores it.
    dsp.set_source_rate(source_rate.map(|rate| rate as f32));
    // The subwoofer, the front pair and the side of every channel (the balance's, audit #44): the
    // music chain's business, ignored by the voice chain's lane, which has no stage that mixes one
    // channel into another.
    dsp.set_layout(&positions);
    // Read before the engine is handed to the node's user data, where it can no longer be reached
    // from the main loop.
    let dsp_latency_frames = dsp.latency_frames();
    // `set_format` returns early when neither the rate nor the channel count moved, so switching
    // between two devices that are both 48 kHz stereo — the common case — would otherwise carry
    // the previous device's filter history, reverb tail and leveller gain straight into the new
    // one. The engine is recycled deliberately, but its *state* should not be.
    dsp.reset();
    // The volume this pair starts at (U10): the target's own, or no louder than the lane was just
    // playing. The lane has it before the DSP is handed over, so the pair's first block is already
    // at it — faded in from silence — and the node's `Props` are told once its proxy is bound
    // ([`publish_volume`]).
    let port = target_port(shared, target);
    let pair_volume = volume_for_pair(
        shared,
        direction,
        &target.name,
        port.as_deref(),
        channels as usize,
    );
    let lane_volume = Arc::clone(&shared.lanes.get(direction).volume);
    lane_volume.begin_pair(channels as usize, &pair_volume);
    dsp.set_volume(&lane_volume.gains());
    dsp.begin_pair();

    // The one place the ring is reconfigured, because it is the one moment nothing reads it: the
    // lane's previous pair has gone, and this pair's NODE 2 does not exist yet.
    let lane = shared.lanes.get(direction);
    lane.ring.reconfigure(channels as usize, quantum as usize);
    lane.counters.sample_rate.store(rate, Ordering::Relaxed);
    lane.counters.channels.store(channels, Ordering::Relaxed);

    let first_data = SinkData::new(shared, dsp);
    let first_listener = first
        .add_local_listener_with_user_data(first_data)
        .state_changed(move |_stream, data, _old, new| {
            log::debug!("{first_name}: {new:?}");
            data.status.first_node_moved(&new);
            if passive && matches!(new, StreamState::Paused) {
                // The pair stopped as one — a passive device-facing stream stops in the same cycle
                // as the rest of its group — and what NODE 2 had not taken from the ring yet is the
                // tail of a sound, or of a recording, that has ended (module docs, "Idle").
                data.ring.mark_stale();
            }
            if let Some(wake) = &wake {
                // Only fails once the main loop is gone, and then there is nothing left to pace.
                let _ = wake.send(());
            }
            if let StreamState::Error(message) = &new {
                log::warn!("{first_name} error: {message}");
                data.status.sink_error.store(true, Ordering::Relaxed);
            }
        })
        .param_changed(on_sink_param)
        .process(on_sink_process)
        .register()
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    let first_values = format_pod(rate, channels, &positions);
    let Some(first_pod) = Pod::from_bytes(&first_values) else {
        return Err(AudioError::FormatNegotiation);
    };
    // A sink and a capture stream both *receive* audio: `Direction::Input` in either case.
    first
        .connect(
            libspa::utils::Direction::Input,
            None,
            first_flags,
            &mut [first_pod],
        )
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    // Declare the delay the DSP adds, now that the node exists. Reported on NODE 1 because that
    // is where the processing happens. A failure here is not worth refusing to play over: the
    // audio path is fine, the graph's latency arithmetic is merely as wrong as it was before.
    let latency_values = process_latency_pod(dsp_latency_frames);
    match Pod::from_bytes(&latency_values) {
        Some(pod) => {
            if let Err(error) = first.update_params(&mut [pod]) {
                log::warn!("could not declare the processing latency: {error}");
            }
        }
        None => log::warn!("could not build the processing-latency parameter"),
    }
    shared.lanes.get_mut(direction).published_latency =
        u32::try_from(dsp_latency_frames).unwrap_or(u32::MAX);

    // ---- NODE 2: the node that drains the ring -----------------------------------------------
    //
    // Output: the playback stream, targeted at the chosen sink and autoconnected.
    //
    // Input: the virtual source. `media.class = "Audio/Source"` on an output-direction
    // `pw_stream` is exactly what `pw-loopback --playback-props=media.class=Audio/Source` does to
    // make a virtual microphone; applications connect to it, so no AUTOCONNECT.
    let (second_name, second_props, second_flags) = match direction {
        DeviceDirection::Output => (
            OUTPUT_NODE_NAME,
            stream_props(direction, &target.name, &latency, passive),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
        DeviceDirection::Input => (
            SOURCE_NODE_NAME,
            virtual_node_props(
                direction,
                shared.language.as_deref(),
                channels,
                rate,
                &positions,
                &latency,
            ),
            StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
    };
    let second = pw::stream::StreamRc::new(core, second_name, second_props)
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    let lane = shared.lanes.get(direction);
    let second_data = OutData {
        ring: Arc::clone(&lane.ring),
        counters: Arc::clone(&lane.counters),
        status: Arc::clone(&lane.status),
        format: AudioInfoRaw::new(),
        channels: 0,
        scratch: vec![0.0; MAX_QUANTUM_FRAMES * MAX_CHANNELS as usize],
        volume: (direction == DeviceDirection::Input).then(|| Arc::clone(&lane.volume)),
    };
    let second_listener = second
        .add_local_listener_with_user_data(second_data)
        .state_changed(move |_stream, data, _old, new| {
            log::debug!("{second_name}: {new:?}");
            data.status
                .output_streaming
                .store(matches!(new, StreamState::Streaming), Ordering::Relaxed);
            if let StreamState::Error(message) = &new {
                log::warn!("{second_name} error: {message}");
                data.status.output_error.store(true, Ordering::Relaxed);
            }
        })
        .param_changed(on_output_param)
        .process(on_output_process)
        .register()
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    let second_values = format_pod(rate, channels, &positions);
    let Some(second_pod) = Pod::from_bytes(&second_values) else {
        return Err(AudioError::FormatNegotiation);
    };
    // A playback stream and a source both *emit* audio: `Direction::Output` in either case.
    second
        .connect(
            libspa::utils::Direction::Output,
            None,
            second_flags,
            &mut [second_pod],
        )
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    Ok(Nodes {
        // Made by [`reconcile_keep_awake`] once the pair is the lane's, when it is to be held.
        keep_awake: None,
        own: None,
        // A node PipeWire has just made is at unity already; there is nothing to tell it.
        volume_published: pair_volume.same_as(&NodeVolume::default(), channels as usize),
        volume_changes_at_build: lane_volume.changes(),
        _first_listener: first_listener,
        _second_listener: second_listener,
        first,
        second,
        target: target.name.clone(),
        target_serial: target.object_serial,
        port,
        via,
        format,
        pace,
    })
}

/// The `node.latency` a pair at `rate` asks for: the quantum FxSound runs at, at that rate. Asked of
/// both nodes and of anything made with the pair ([`KeepAwake`]), so nothing of FxSound's pulls the
/// graph's quantum below what the pair was built for.
fn pair_latency(rate: u32) -> String {
    format!(
        "{}/{rate}",
        DEFAULT_QUANTUM_FRAMES.min(MAX_QUANTUM_FRAMES as u32)
    )
}

/// How a new pair keeps its device-facing stream from running while nothing uses the lane (module
/// docs, "Idle"): whether that stream is declared passive, and the pace an output pair's NODE 2 is
/// kept to by hand when it cannot be.
///
/// Where the server runs a link-group together, the device-facing stream is passive in both lanes:
/// the output lane's playback stream (NODE 2), which then runs only while something plays into the
/// sink, and the input lane's capture stream (NODE 1), which then runs only while something records
/// from the source (U19). On an older server the playback stream is paced by hand and the capture
/// stream is left running, holding the microphone for as long as the lane is on.
fn idle_plan(
    direction: DeviceDirection,
    link_groups_scheduled: bool,
) -> (bool, Option<SecondNodePace>) {
    match direction {
        _ if link_groups_scheduled => (true, None),
        DeviceDirection::Input => (false, None),
        DeviceDirection::Output => (false, Some(SecondNodePace::new())),
    }
}

/// The properties of FxSound's virtual device — the sink of the output lane, the source of
/// the input lane (`docs/spec/12-audio-io.md` §20 NODE 1, §28.2 NODE 2).
///
/// Property spellings are the verified ones from `docs/api/pipewire-0.10-rust.md` §14 — every key
/// after the first dot is hyphenated, and `audio.position` has no Rust constant because it has no
/// `PW_KEY_` either. The description is the localised `"FxSound (<Output|Input>)"`; the name is
/// the fixed ASCII one the metadata carries.
///
/// Three keys are about the node's volume (`crate::volume`). `state.restore-props = false` keeps
/// WirePlumber from restoring one volume for the node whatever it renders to
/// (`node/state-stream.lua`, the condition at the top of its restore and store hooks) — the lane
/// remembers one per target instead, as WirePlumber does for its own loopback nodes
/// (`monitors/bluez/create-loopback-node.lua`). What WirePlumber had saved for the node until then
/// stays in its state file, and is read once as the level a lane with no history starts no louder
/// than ([`volume::inherited`]). And `channelmix.min-volume` and `max-volume` at
/// unity make the adapter keep every volume a desktop writes and apply none of it, so the lane
/// can apply it after its chain rather than have the volume leveller undo it.
fn virtual_node_props(
    direction: DeviceDirection,
    language: Option<&str>,
    channels: u32,
    rate: u32,
    positions: &ChannelMap,
    latency: &str,
) -> PropertiesBox {
    let media_class = match direction {
        DeviceDirection::Output => devices::SINK_MEDIA_CLASS,
        DeviceDirection::Input => devices::SOURCE_MEDIA_CLASS,
    };
    // The UI's language when the app named one, the desktop locale otherwise.
    let system = locale::system_language();
    let description = locale::node_description(direction, language.or(system.as_deref()));
    properties! {
        *pw::keys::MEDIA_CLASS        => media_class,
        *pw::keys::MEDIA_TYPE         => "Audio",
        *pw::keys::NODE_NAME          => our_node_name(direction),
        *pw::keys::NODE_DESCRIPTION   => description.as_str(),
        *pw::keys::NODE_NICK          => description.as_str(),
        *pw::keys::NODE_VIRTUAL       => "true",
        *pw::keys::NODE_LINK_GROUP    => link_group(direction),
        *pw::keys::NODE_WANT_DRIVER   => "true",
        *pw::keys::NODE_ALWAYS_PROCESS => "false",
        *pw::keys::NODE_LATENCY       => latency,
        *pw::keys::AUDIO_CHANNELS     => channels.to_string(),
        *pw::keys::AUDIO_RATE         => rate.to_string(),
        *pw::keys::AUDIO_FORMAT       => "F32",
        *pw::keys::APP_NAME           => "FxSound",
        *pw::keys::APP_ID             => crate::APP_ID,
        "audio.position"              => positions.to_property_value(),
        "device.class"                => "sound",
        "priority.session"            => NODE_PRIORITY_SESSION,
        "priority.driver"             => "0",
        "monitor.channel-volumes"     => "false",
        volume::MIN_VOLUME_KEY        => volume::UNITY,
        volume::MAX_VOLUME_KEY        => volume::UNITY,
        "state.restore-props"         => "false",
        "media.icon-name"             => "fxsound",
        "application.icon-name"       => "fxsound",
    }
}

/// The properties of the stream that touches the real device — the playback stream of the output
/// lane, the capture stream of the input lane (`docs/spec/12-audio-io.md` §20 NODE 2, §28.2
/// NODE 1). `target` is the real device's `node.name`; `passive` says whether the stream's link
/// to it may keep it awake (module docs, "Idle"). WirePlumber links it to `target` once and never
/// moves it (module docs, "A device that blinks").
fn stream_props(
    direction: DeviceDirection,
    target: &str,
    latency: &str,
    passive: bool,
) -> PropertiesBox {
    let (media_class, category, name, description) = match direction {
        DeviceDirection::Output => (
            "Stream/Output/Audio",
            "Playback",
            OUTPUT_NODE_NAME,
            OUTPUT_STREAM_DESCRIPTION,
        ),
        DeviceDirection::Input => (
            "Stream/Input/Audio",
            "Capture",
            CAPTURE_NODE_NAME,
            CAPTURE_STREAM_DESCRIPTION,
        ),
    };
    let mut props = properties! {
        *pw::keys::MEDIA_CLASS         => media_class,
        *pw::keys::MEDIA_TYPE          => "Audio",
        *pw::keys::MEDIA_CATEGORY      => category,
        // Production, not Music/Communication: a Music stream can be corked or ducked by role
        // policies, and FxSound is the thing everything else is playing (or recording) through.
        *pw::keys::MEDIA_ROLE          => "Production",
        *pw::keys::MEDIA_NAME          => SINK_DESCRIPTION,
        *pw::keys::NODE_NAME           => name,
        *pw::keys::NODE_DESCRIPTION    => description,
        // The same group as the virtual node. Without it WirePlumber links this stream straight
        // back into our own sink (or our own source) the moment that node becomes the default,
        // and the graph feeds back.
        *pw::keys::NODE_LINK_GROUP     => link_group(direction),
        *pw::keys::NODE_AUTOCONNECT    => "true",
        // Where this stream plays is the rules' choice and nobody else's, and these three say so
        // to WirePlumber (module docs, "A device that blinks"). As 0.5.17 reads them:
        //
        // `node.dont-reconnect`: once linked, the stream is never moved. Not onto a new default
        // while its link stands (`linking/prepare-link.lua:63-67`: "dont-reconnect, not
        // moving"). Not onto the fallback device when its target goes — the link goes with the
        // target, and a stream that has been linked once is not linked again (`:71-76`) — and,
        // by the same check, not back onto its target when that comes back. A node that comes
        // back is a new node, and the rules rebuild the pair on it ([`is_same_node`]). 0.3.0 left
        // this `false`, and WirePlumber moved the stream onto the laptop's speakers by itself the
        // moment a Bluetooth headset switched profile.
        //
        // `node.dont-fallback`: a target that is not there when the stream is first linked is
        // waited for, not replaced by the default device (`find-defined-target.lua:116-128`).
        //
        // `node.linger`: and the stream is kept while it waits. Without it, both of those places
        // send the stream an error and destroy it instead (`find-defined-target.lua:117-123`,
        // `prepare-link.lua:106-119`), and a pair built a moment before its target went would
        // lose its device-facing stream under it.
        *pw::keys::NODE_DONT_RECONNECT => "true",
        "node.dont-fallback"           => "true",
        "node.linger"                  => "true",
        // WirePlumber keeps one volume per application for its streams, not per device
        // (`node/state-stream.lua`: `formKey` is the media class and `application.id`), stores
        // every `Props` change of a stream under it and restores it onto every new stream with the
        // same key. This stream's key is FxSound's own, and the capture stream shares it with the
        // recorder a mixer lists as "FxSound microphone check" ([`keep_awake_props`]). A level or
        // a mute set there would come back on every pair built after it, on every device, where
        // nothing in FxSound can see it: a microphone lane muted for good by one click in the
        // Recording tab. The stream stays at what PipeWire makes it — unity, unmuted — and the
        // volume anyone means is the virtual node's (`crate::volume`).
        "state.restore-props"          => "false",
        // Passive, the playback stream's link no longer keeps the speakers running; what runs it
        // is the virtual sink, whenever an application's link makes that run, because the server
        // runs a link-group together. The capture stream the same way round: its link no longer
        // keeps the microphone running, and what runs it is the virtual source, whenever a
        // recorder's link makes that run (U19). That is true from PipeWire 0.3.68 on, and only
        // there does the caller ask for it. On an older server the two nodes are not linked in
        // the graph — the ring between them is invisible to it — and nothing would ever wake a
        // passive stream but its device running for somebody else: FxSound would play, or
        // record, nothing.
        *pw::keys::NODE_PASSIVE        => if passive { "true" } else { "false" },
        *pw::keys::NODE_LATENCY        => latency,
        // Remixing stays on: it is what lets the pair run stereo on a device that is not. The
        // adapter down-mixes the playback stream into a mono headset and up-mixes a mono
        // microphone into the capture stream, and the session manager sets the stream's ports up
        // at the device's own layout only while this is off.
        *pw::keys::STREAM_DONT_REMIX   => "false",
        *pw::keys::TARGET_OBJECT       => target,
        *pw::keys::APP_NAME            => "FxSound",
        *pw::keys::APP_ID              => crate::APP_ID,
    };
    if direction == DeviceDirection::Input {
        // Capture the microphone itself, not the monitor of a sink.
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "false");
    }
    props
}

/// The properties of the recorder that holds the microphone awake ([`KeepAwake`]).
///
/// Everything WirePlumber 0.5.17 looks at before it switches a Bluetooth headset to its call
/// profile (`device/autoswitch-bluetooth-profile.lua`, the `link-added` hook and
/// `isBluetoothLoopbackSourceNodeLinkedToStream`) is set to what an application's recording stream
/// has: `media.class = Stream/Input/Audio`, **no** `node.link-group` — the whole point, since the
/// pair's capture stream has one and is passed over for it — and neither `stream.monitor` nor
/// `bluez5.loopback`. It is not passive, or it would wake nothing.
///
/// And WirePlumber remembers nothing of its volume (`state.restore-props = false`). A desktop lists
/// it among the applications recording, and its key in WirePlumber's stream memory — the media
/// class and `application.id` (`node/state-stream.lua`, `formKey`) — is the capture stream's: a
/// mute or a level set on it there would be restored onto the next pair's capture stream, whose
/// adapter applies it to the microphone before the chain. Dropping `application.id` would not
/// help, since the key falls back to `application.name`.
fn keep_awake_props(latency: &str) -> PropertiesBox {
    properties! {
        *pw::keys::MEDIA_CLASS         => "Stream/Input/Audio",
        *pw::keys::MEDIA_TYPE          => "Audio",
        *pw::keys::MEDIA_CATEGORY      => "Capture",
        *pw::keys::MEDIA_ROLE          => "Production",
        *pw::keys::MEDIA_NAME          => KEEP_AWAKE_STREAM_DESCRIPTION,
        *pw::keys::NODE_NAME           => KEEP_AWAKE_NODE_NAME,
        *pw::keys::NODE_DESCRIPTION    => KEEP_AWAKE_STREAM_DESCRIPTION,
        *pw::keys::NODE_AUTOCONNECT    => "true",
        *pw::keys::NODE_PASSIVE        => "false",
        *pw::keys::NODE_DONT_RECONNECT => "true",
        "node.dont-fallback"           => "true",
        "node.dont-move"               => "true",
        "node.linger"                  => "true",
        "state.restore-props"          => "false",
        *pw::keys::NODE_LATENCY        => latency,
        *pw::keys::TARGET_OBJECT       => SOURCE_NODE_NAME,
        *pw::keys::STREAM_CAPTURE_SINK => "false",
        *pw::keys::APP_NAME            => "FxSound",
        *pw::keys::APP_ID              => crate::APP_ID,
    }
}

/// The keep-awake recorder's `process()`: hand every buffer straight back. Being linked and run is
/// all it is for; what the source played into it has been measured already, by the lane's own
/// meters on the capture stream.
///
/// Same real-time rules as [`on_sink_process`]: dropping a `Buffer` queues it back to the stream,
/// and there are only ever as many to drop as the stream has buffers.
fn on_keep_awake_process(stream: &pw::stream::Stream, _: &mut ()) {
    while let Some(buffer) = stream.dequeue_buffer() {
        drop(buffer);
    }
}

/// Serialise an `SPA_TYPE_OBJECT_Format` / `SPA_PARAM_EnumFormat` pod for interleaved F32.
///
/// `AudioInfoRaw` starts out flagged `UNPOSITIONED` and only clears the flag when
/// `position[0] != 0` (`libspa-0.10.1/src/param/audio/raw.rs:70-74`), which is why
/// [`ChannelMap::to_spa_position`] never yields a zero in slot 0.
/// A `SPA_PARAM_ProcessLatency` pod carrying a fixed delay in frames.
///
/// Without this, the only latency FxSound ever declares is the static `node.latency` property,
/// which describes the buffering, not the processing — so a recording application is told the
/// virtual microphone is instantaneous. Today the figure is small (the limiter's 0.75 ms
/// look-ahead), but it is the quantity a voice chain grows: a 10 ms denoiser block plus a VAD
/// grace ring would otherwise make FxSound lie to OBS and Discord by enough to show up as
/// lip-sync drift the user cannot diagnose.
fn process_latency_pod(frames: usize) -> Vec<u8> {
    use libspa::pod::{Object, Property, PropertyFlags, Value};

    let frames = u32::try_from(frames).unwrap_or(u32::MAX);
    libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: libspa::utils::SpaTypes::ObjectParamProcessLatency.as_raw(),
            id: libspa::param::ParamType::ProcessLatency.as_raw(),
            properties: vec![Property {
                key: libspa::sys::SPA_PARAM_PROCESS_LATENCY_rate,
                flags: PropertyFlags::empty(),
                value: Value::Int(frames as i32),
            }],
        }),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

fn format_pod(rate: u32, channels: u32, positions: &ChannelMap) -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(channels);
    info.set_position(positions.to_spa_position());

    libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &libspa::pod::Value::Object(libspa::pod::Object {
            type_: libspa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: libspa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The volume of the virtual nodes (`crate::volume`, U10 and U11)
// ---------------------------------------------------------------------------------------------

/// The app's remembered per-target volumes as the engine keeps them: entries that cannot be
/// replayed gone ([`TargetVolume::sanitised`]), and of two for the same direction, target and port
/// the later, as the app's own `remember_target_volume` would have it.
///
/// An entry with neither a level nor a mute goes too. `sanitised` keeps an entry with no
/// `channel_volumes` as a remembered mute — which is what [`volume::for_new_pair`] makes of it —
/// but one that is not muted either says nothing about its device, and is the same as no entry.
fn remembered_volumes(volumes: Vec<TargetVolume>) -> Vec<TargetVolume> {
    let mut kept: Vec<TargetVolume> = Vec::with_capacity(volumes.len());
    for entry in volumes.into_iter().filter_map(TargetVolume::sanitised) {
        if entry.channel_volumes.is_empty() && !entry.mute {
            log::info!(
                "{} lane: a remembered volume for {} has no level, and is left out",
                entry.direction.key(),
                entry.target
            );
            continue;
        }
        kept.retain(|known| !known.same_place(&entry));
        kept.push(entry);
    }
    log::info!("{} remembered per-device volumes", kept.len());
    kept
}

/// Take the app's remembered per-target volumes ([`UiToAudio::SeedTargetVolumes`]), replacing
/// whatever the engine held — a later seed is the settings file as it now is
/// ([`remembered_volumes`]).
///
/// The memory normally comes with the engine ([`crate::StartOptions::target_volumes`]), before any
/// pair. A seed is a replacement for it, and a lane may have built its pair by the time one
/// arrives — on a volume chosen without it. A pair whose volume nothing has changed since is given
/// its target's remembered one now, and the app is told so: a pair up for a supervisor tick or two
/// has reported the volume it started at, and the app's last word for the device must be the one
/// it now plays at. A pair a desktop has moved meanwhile keeps what the user set.
fn seed_target_volumes(shared: &mut Shared, volumes: Vec<TargetVolume>) {
    shared.target_volumes = remembered_volumes(volumes);

    for direction in DeviceDirection::ALL {
        let lane = shared.lanes.get(direction);
        let Some(nodes) = lane.nodes.as_ref() else {
            continue;
        };
        if lane.volume.changes() != nodes.volume_changes_at_build {
            continue;
        }
        let Some(entry) =
            remembered_volume(shared, direction, &nodes.target, nodes.port.as_deref())
        else {
            continue;
        };
        let channels = lane.volume.channels();
        let Some(volume) = reseeded(entry, &lane.volume.snapshot(), channels) else {
            continue;
        };
        let entry = entry.clone();
        lane.volume.replace(&volume);
        let changes = lane.volume.changes();
        if let Some(nodes) = shared.lanes.get_mut(direction).nodes.as_mut() {
            nodes.volume_changes_at_build = changes;
            nodes.volume_published = false;
        }
        publish_volume(shared, direction);
        shared.notify(AudioToUi::TargetVolume(entry));
    }
}

/// What a seed that arrived after the pair was built makes of the pair's volume `current`, given
/// the entry remembered for its target: the volume to replace it with, or `None` when the entry
/// would change nothing ([`seed_target_volumes`]).
///
/// What the pair is at stands in for the lane's last volume: a remembered mute with no level
/// mutes it where it is, rather than at the unity of an empty list. "Nothing" is judged on what is
/// kept, level and mute apart ([`NodeVolume::same_as`]), not on what is heard: a pair muted at
/// one level and remembered muted at another sounds the same now, and would play at the wrong
/// one the moment it is unmuted — and report that level back as the device's.
fn reseeded(entry: &TargetVolume, current: &NodeVolume, channels: usize) -> Option<NodeVolume> {
    let volume = volume::for_new_pair(Some(entry), Some(current), channels);
    (!volume.same_as(current, channels)).then_some(volume)
}

/// The remembered volume of the lane of `direction` on `target`'s port `port` — `None` for a
/// device whose card names no port — if there is one.
fn remembered_volume<'a>(
    shared: &'a Shared,
    direction: DeviceDirection,
    target: &str,
    port: Option<&str>,
) -> Option<&'a TargetVolume> {
    shared
        .target_volumes
        .iter()
        .find(|entry| entry.is_for(direction, target, port.unwrap_or_default()))
}

/// The port `device` is on, as its card last said: the name of the card's active route for the
/// node ([`routes::node_port`]). `None` for a node on no card, or one whose card has not named its
/// port.
fn target_port(shared: &Shared, device: &DeviceInfo) -> Option<String> {
    routes::node_port(
        device.card_id.and_then(|id| shared.card_routes.get(&id)),
        device.profile_device,
    )
    .map(str::to_owned)
}

/// The volume a new pair of the lane of `direction` starts at on `target`, on its port `port`
/// ([`volume::for_new_pair`]): the target's remembered one, or else no louder than the lane's last
/// pair — or, before the lane has had one this run, than the quietest level remembered for any
/// device of its direction — or, with nothing remembered for its direction either, than the level
/// WirePlumber kept for the lane's node before 0.4.0. Without that last, the first 0.4.0 run after
/// an upgrade would start at unity, however far down 0.3.0 had been left.
fn volume_for_pair(
    shared: &Shared,
    direction: DeviceDirection,
    target: &str,
    port: Option<&str>,
    channels: usize,
) -> NodeVolume {
    let last = shared
        .lanes
        .get(direction)
        .last_volume
        .clone()
        .or_else(|| volume::quietest(&shared.target_volumes, direction))
        .or_else(|| shared.inherited_volumes.get(direction).clone());
    volume::for_new_pair(
        remembered_volume(shared, direction, target, port),
        last.as_ref(),
        channels,
    )
}

/// Remember `volume` as the lane of `direction`'s on `target`'s port `port`, and tell the app —
/// when it differs from what is remembered already, so that a volume nobody moved is never
/// reported, and one that is reported is reported once.
fn report_target_volume(
    shared: &mut Shared,
    direction: DeviceDirection,
    target: &str,
    port: Option<&str>,
    volume: &NodeVolume,
    channels: usize,
) {
    let Some(entry) = volume.to_target(direction, target, port, channels) else {
        return;
    };
    if remembered_volume(shared, direction, target, port) == Some(&entry) {
        return;
    }
    log::info!(
        "{} lane: remembering {:?}{} for {target}{}",
        direction.key(),
        entry.channel_volumes,
        if entry.mute { ", muted" } else { "" },
        port.map(|port| format!(" on {port}")).unwrap_or_default()
    );
    shared
        .target_volumes
        .retain(|known| !known.same_place(&entry));
    shared.target_volumes.push(entry.clone());
    shared.notify(AudioToUi::TargetVolume(entry));
}

/// The supervisor's look at a lane's volume: reported for the pair's target once it has held still
/// for a tick ([`Debounce`]).
fn watch_volume(shared: &mut Shared, direction: DeviceDirection) {
    let lane = shared.lanes.get_mut(direction);
    let changes = lane.volume.changes();
    if !lane.volume_debounce.settled(changes) {
        return;
    }
    let Some((target, port)) = lane
        .nodes
        .as_ref()
        .map(|nodes| (nodes.target.clone(), nodes.port.clone()))
    else {
        return;
    };
    let (volume, channels) = (lane.volume.snapshot(), lane.volume.channels());
    report_target_volume(
        shared,
        direction,
        &target,
        port.as_deref(),
        &volume,
        channels,
    );
}

/// A lane's pair is going: whatever its volume came to is remembered for its target now, without
/// waiting for the tick — the next pair may be on another device — and becomes the level a device
/// never seen before is never louder than.
fn retire_volume(shared: &mut Shared, direction: DeviceDirection) {
    let lane = shared.lanes.get_mut(direction);
    let Some((target, port)) = lane
        .nodes
        .as_ref()
        .map(|nodes| (nodes.target.clone(), nodes.port.clone()))
    else {
        return;
    };
    let (volume, channels) = (lane.volume.snapshot(), lane.volume.channels());
    lane.volume_debounce.settled(lane.volume.changes());
    lane.last_volume = Some(volume.clone());
    report_target_volume(
        shared,
        direction,
        &target,
        port.as_deref(),
        &volume,
        channels,
    );
}

/// What the volume of the lane of `direction`, at `current` on `target`, becomes when the target
/// is found on the port `ports.1` after `ports.0` ([`follow_port`]): the volume to apply now, or
/// `None` to keep it as it is.
///
/// Moved from a port: the level is remembered — and reported — for the port it leaves, and
/// becomes the level the lane was last playing at; the new port gets its own remembered level, or,
/// never seen, no more than that ([`volume::for_new_pair`]). Named for the first time: nothing is
/// remembered for a port the pair never knew, and the new port's own level replaces the pair's
/// only while nothing has moved it since the pair was built (`untouched`), as a late seed does
/// ([`reseeded`]).
fn volume_on_port(
    shared: &mut Shared,
    direction: DeviceDirection,
    target: &str,
    ports: (Option<&str>, &str),
    current: &NodeVolume,
    channels: usize,
    untouched: bool,
) -> Option<NodeVolume> {
    let (left, port) = ports;
    let Some(left) = left else {
        log::debug!("{} lane: {target} is on {port}", direction.key());
        return remembered_volume(shared, direction, target, Some(port))
            .filter(|_| untouched)
            .and_then(|entry| reseeded(entry, current, channels));
    };
    log::info!(
        "{} lane: {target} moved from {left} to {port}; its volume follows",
        direction.key()
    );
    report_target_volume(shared, direction, target, Some(left), current, channels);
    let lane = shared.lanes.get_mut(direction);
    lane.volume_debounce.settled(lane.volume.changes());
    lane.last_volume = Some(current.clone());
    Some(volume::for_new_pair(
        remembered_volume(shared, direction, target, Some(port)),
        Some(current),
        channels,
    ))
}

/// The lane's device changed its port under the pair — headphones plugged into a sink that was
/// the speakers — and FxSound's volume follows, as it follows a new device, without the pair being
/// rebuilt (`crate::volume`, "Per target").
///
/// On a card that is not UCM — a plain HDA card, most desktops and many laptops — the speakers
/// and the headphones are two ports of one node, and plugging headphones in only switches the
/// node's active route: the rules find the same target and keep the pair. Keyed by the node's name
/// alone, the volume stayed where the speakers had it, and the headphones got it at full scale
/// times whatever level WirePlumber restored for their port — upstream's #615 once more. So the
/// level the lane had is remembered for the port it leaves, the one it goes to is looked up as a
/// new pair's would be — its own if it has one, never louder than before if not — written to the
/// virtual node for every slider to show, and faded in from silence.
///
/// A port that is not known yet when the pair is built and is named a moment later changes no
/// device: the pair keeps the volume it started at, unless nothing has moved it since and the new
/// port has one of its own. And a port that stops being named — a card between two sendings of its
/// routes — is no change either: the next sending names one.
fn follow_port(shared: &mut Shared, direction: DeviceDirection) {
    let lane = shared.lanes.get(direction);
    let Some(nodes) = lane.nodes.as_ref() else {
        return;
    };
    let Some(port) = shared
        .devices
        .iter()
        .find(|device| {
            device.direction == direction
                && is_same_node(&nodes.target, nodes.target_serial, device)
        })
        .and_then(|device| target_port(shared, device))
    else {
        return;
    };
    if nodes.port.as_deref() == Some(port.as_str()) {
        return;
    }
    let target = nodes.target.clone();
    let left = nodes.port.clone();
    let untouched = lane.volume.changes() == nodes.volume_changes_at_build;
    let (current, channels) = (lane.volume.snapshot(), lane.volume.channels());
    let volume = volume_on_port(
        shared,
        direction,
        &target,
        (left.as_deref(), &port),
        &current,
        channels,
        untouched,
    );

    let lane = shared.lanes.get_mut(direction);
    if let Some(volume) = &volume {
        lane.volume.replace(volume);
        // What the DSP ramps to, it now fades in to from silence, on the next block.
        lane.volume.request_fade();
    }
    let changes = lane.volume.changes();
    if let Some(nodes) = lane.nodes.as_mut() {
        nodes.port = Some(port);
        if volume.is_some() {
            nodes.volume_changes_at_build = changes;
            nodes.volume_published = false;
        }
    }
    if volume.is_some() {
        publish_volume(shared, direction);
    }
}

/// Write the volume a lane's pair started at to its virtual node's `Props`, once there is a proxy
/// to write it through ([`adopt_own_node`]) — the node the lane *made*, never the device it
/// renders to. What is written is the lane's volume as it is now, so a desktop that moved the
/// slider in the moment before is not overruled. Its echo comes back through `param_changed` as a
/// write that changes nothing.
fn publish_volume(shared: &mut Shared, direction: DeviceDirection) {
    let lane = shared.lanes.get_mut(direction);
    let (volume, channels) = (lane.volume.snapshot(), lane.volume.channels());
    let Some(nodes) = lane.nodes.as_mut() else {
        return;
    };
    let Some(own) = nodes.own.as_ref() else {
        return;
    };
    if nodes.volume_published {
        return;
    }
    let bytes = volume::props_pod(&volume, channels);
    let Some(pod) = Pod::from_bytes(&bytes) else {
        log::warn!("could not build a Props pod for {volume:?}");
        return;
    };
    own.node.set_param(libspa::param::ParamType::Props, 0, pod);
    nodes.volume_published = true;
    log::info!(
        "{} lane: {}{} starts at {:?}{}",
        direction.key(),
        nodes.target,
        nodes
            .port
            .as_deref()
            .map(|port| format!(" on {port}"))
            .unwrap_or_default(),
        volume.effective(channels),
        if volume.mute { ", muted" } else { "" }
    );
}

/// The lane a virtual node belongs to, by its `node.name`: `fxsound_sink`'s or `fxsound_source`'s.
fn virtual_node_direction(node_name: Option<&str>) -> Option<DeviceDirection> {
    match node_name? {
        SINK_NODE_NAME => Some(DeviceDirection::Output),
        SOURCE_NODE_NAME => Some(DeviceDirection::Input),
        _ => None,
    }
}

/// The stream of a pair that is its lane's virtual node: NODE 1 of the output lane, NODE 2 of the
/// input lane.
const fn virtual_stream(nodes: &Nodes, direction: DeviceDirection) -> &pw::stream::StreamRc {
    match direction {
        DeviceDirection::Output => &nodes.first,
        DeviceDirection::Input => &nodes.second,
    }
}

/// The volume connection's registry announced one of our virtual nodes ([`volume_connection`]):
/// bind a proxy for it on that connection, if it is the lane's current pair's, to write the pair's
/// starting volume through ([`publish_volume`]) and to read what the node's adapter makes of a
/// volume ([`on_own_props`]).
///
/// The id is checked against the stream's own when the stream knows it. Before it does, the name
/// has to do, and the name can be a dead node's: a pair rebuilt before this registry announced the
/// last pair's node — the second connection reads its socket after the first has torn that pair
/// down — adopts the node that went, and the volume written to it reaches nothing. So a node
/// adopted in place of another is written to again ([`still_published`]); the real node is
/// announced after the dead one, and replaces it.
fn adopt_own_node(
    shared: &mut Shared,
    handle: &Rc<RefCell<Shared>>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&libspa::utils::dict::DictRef>,
    direction: DeviceDirection,
) {
    let id = global.id;
    let Some(nodes) = shared.lanes.get(direction).nodes.as_ref() else {
        return;
    };
    // `SPA_ID_INVALID` until the server has told the stream which global it is.
    let known = virtual_stream(nodes, direction).node_id();
    if known != u32::MAX && known != id {
        return;
    }
    let node = match registry.bind::<pw::node::Node, _>(global) {
        Ok(node) => node,
        Err(error) => {
            log::warn!(
                "could not bind the {} lane's own node, so desktops will not see the volume it \
                 starts at: {error}",
                direction.key()
            );
            return;
        }
    };
    let listener = node
        .add_listener_local()
        .param({
            let handle = Rc::clone(handle);
            move |_seq, param_type, _index, _next, param| {
                if param_type != libspa::param::ParamType::Props {
                    return;
                }
                if let Some(update) = param.and_then(PropsUpdate::from_pod) {
                    on_own_props(&handle, direction, id, &update);
                }
            }
        })
        .register();
    node.subscribe_params(&[libspa::param::ParamType::Props]);
    if let Some(nodes) = shared.lanes.get_mut(direction).nodes.as_mut() {
        let previous = nodes.own.as_ref().map(|own| own.id);
        nodes.volume_published = still_published(nodes.volume_published, previous, id);
        nodes.own = Some(OwnNode {
            _listener: listener,
            node,
            id,
        });
    }
    publish_volume(shared, direction);
}

/// Whether a pair's volume still counts as written to its virtual node once the node `adopted` is
/// the lane's, in place of the one it had adopted before, `previous`.
///
/// Only a node adopted before could have been written to, so the first keeps what the pair was
/// built with — written already when it starts at the unity a new node has. Another node in its
/// place is written to again: the one before was a pair's that had gone ([`adopt_own_node`]).
/// Left unwritten, the new node would show unity while the lane plays the lower level it started
/// at, and the next volume key would step up from unity — from 0.2 to 1.05, a device change that
/// raises the volume after all. Writing a node that has the volume already is harmless: its echo
/// changes nothing.
const fn still_published(published: bool, previous: Option<u32>, adopted: u32) -> bool {
    match previous {
        Some(previous) if previous != adopted => false,
        _ => published,
    }
}

/// The whole of a virtual node's `Props`, as the server lists them: what says whether the node's
/// adapter keeps its volume at unity ([`PropsUpdate::clamped`]), and so whether the lane's DSP
/// applies the volume or leaves it to the adapter.
///
/// Nothing else decides it: a write lists only what it sets ([`take_props`]). With no second
/// connection nothing does, and the lane goes on applying the volume, as the adapter of every
/// PipeWire since 0.3.72 expects.
///
/// The volume fields are not taken from here. Every write reaches `param_changed` first, in order
/// ([`take_props`]); this is the server's echo of the result, a round trip later, and during a
/// slider drag it can arrive behind the next write and would put an older level back.
fn on_own_props(
    handle: &Rc<RefCell<Shared>>,
    direction: DeviceDirection,
    id: u32,
    update: &PropsUpdate,
) {
    let Ok(guard) = handle.try_borrow() else {
        return;
    };
    let lane = guard.lanes.get(direction);
    let ours = lane
        .nodes
        .as_ref()
        .and_then(|nodes| nodes.own.as_ref())
        .is_some_and(|own| own.id == id);
    let Some(clamped) = update.clamped.filter(|_| ours) else {
        return;
    };
    if lane.volume.post_dsp() != clamped {
        if clamped {
            log::info!(
                "{} lane: the volume is applied after the chain",
                direction.key()
            );
        } else {
            log::info!(
                "{} lane: this PipeWire's adapter applies the volume itself (it is older than \
                 0.3.72), so the chain hears it turned down",
                direction.key()
            );
        }
    }
    lane.volume.set_post_dsp(clamped);
}

// ---------------------------------------------------------------------------------------------
// The two process callbacks — RT thread from here down
// ---------------------------------------------------------------------------------------------

/// NODE 1's `param_changed`. Main loop, never the data thread — which is exactly why the
/// process callbacks can assume `channels` is constant for the life of a buffer.
///
/// Two kinds of parameter arrive here: the format the node negotiated, and — on the output lane's
/// virtual sink — every `Props` write a desktop makes, which is its volume ([`take_props`]).
fn on_sink_param(_stream: &pw::stream::Stream, data: &mut SinkData, id: u32, param: Option<&Pod>) {
    if data.virtual_node {
        take_props(&data.volume, id, param);
    }
    adopt_sink_format(data, id, param);
}

/// A `Props` write on a lane's virtual node, as `param_changed` hands it over: the stream's
/// follower sees every write before the adapter does (`audioadapter.c`, `impl_node_set_param`),
/// so this is the volume a desktop set, as it set it — a slider's `channelVolumes`, a mute key's
/// `mute`, never the whole object. Kept in the lane, where the DSP applies it and the supervisor
/// reports it.
///
/// Whether the adapter clamps is not read here, even from a write that carries `params`: a write
/// names only the settings it changes — `pw-cli set-param fxsound_sink Props '{ params = [
/// "channelmix.normalize" true ] }'` lists neither volume bound — so it would read as an adapter
/// that does not clamp, and the lane would stop applying a volume the adapter still keeps at
/// unity. Only the node's whole `Props` say it ([`on_own_props`]); a write that does move a
/// bound changes them, and they come back through there.
///
/// Main loop. Only the lane's atomics are written, never the DSP's own state: unlike a format
/// change, a volume change arrives while the data thread is processing.
fn take_props(volume: &LaneVolume, id: u32, param: Option<&Pod>) {
    if id != libspa::param::ParamType::Props.as_raw() {
        return;
    }
    let Some(update) = param.and_then(PropsUpdate::from_pod) else {
        return;
    };
    if volume.update(&update) {
        log::debug!("the virtual node's volume is now {:?}", volume.snapshot());
    }
}

/// Latch the format NODE 1 negotiated — without touching the ring.
///
/// This is not a once-per-pair event. A node can negotiate again at any point in its life —
/// PipeWire clears a suspended node's format and sets it again when the node wakes, and a session
/// manager suspends a sink a few seconds after its last client leaves — and NODE 2 goes on popping
/// the ring on the data thread through all of it. 0.3.0 reconfigured the ring here, which writes
/// the read cursor NODE 2 owns. The ring needs nothing from this callback in any case:
/// [`build_nodes`] sized it for the one format both nodes declare, and the format negotiated is
/// that one.
///
/// Unless it is not — and then it is the same failure as NODE 2 negotiating a stride the ring was
/// not built for: counted as a format mismatch for the supervisor, which rebuilds the pair on the
/// lane's backoff, and in the meantime nothing is pushed, because `channels` stays zero.
fn adopt_sink_format(data: &mut SinkData, id: u32, param: Option<&Pod>) {
    let Some((rate, channels)) = parse_audio_format(&mut data.format, id, param) else {
        return;
    };
    let ring_channels = data.ring.channels();
    if channels != ring_channels {
        data.channels = 0;
        data.counters
            .format_mismatches
            .fetch_add(1, Ordering::Relaxed);
        log::warn!(
            "NODE 1 negotiated {channels} ch @ {rate} Hz, but its ring carries {ring_channels} ch; \
             muting it until the pair is rebuilt"
        );
        return;
    }
    data.channels = channels;
    data.counters.sample_rate.store(rate, Ordering::Relaxed);
    data.counters
        .channels
        .store(channels as u32, Ordering::Relaxed);
    if let Some(dsp) = data.dsp.as_mut() {
        dsp.set_format(rate as f32, channels);
    }
    log::info!("NODE 1 negotiated {channels} ch @ {rate} Hz");
}

/// NODE 2's `param_changed`: the format it negotiated, and — on the input lane's virtual source
/// — its volume ([`take_props`]).
fn on_output_param(_stream: &pw::stream::Stream, data: &mut OutData, id: u32, param: Option<&Pod>) {
    if let Some(volume) = &data.volume {
        take_props(volume, id, param);
    }
    let Some((rate, channels)) = parse_audio_format(&mut data.format, id, param) else {
        return;
    };
    data.channels = channels;
    log::info!("NODE 2 negotiated {channels} ch @ {rate} Hz");
}

fn parse_audio_format(
    info: &mut AudioInfoRaw,
    id: u32,
    param: Option<&Pod>,
) -> Option<(u32, usize)> {
    // A `None` param means "clear the format" (`examples/audio-capture.rs:81-84`).
    let param = param?;
    if id != libspa::param::ParamType::Format.as_raw() {
        return None;
    }
    let (media_type, media_subtype) = format_utils::parse_format(param).ok()?;
    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
        return None;
    }
    info.parse(param).ok()?;
    let channels = (info.channels() as usize).min(MAX_CHANNELS as usize);
    let rate = info.rate();
    if channels == 0 || rate == 0 {
        return None;
    }
    Some((rate, channels))
}

/// NODE 1's `process()`: everything an application rendered into FxSound — or everything the
/// microphone produced — DSP'd in place and pushed to the ring.
///
/// Runs on PipeWire's data thread under `SCHED_FIFO`. Every rule in
/// `docs/api/pipewire-0.10-rust.md` §15 applies: no allocation, no lock, no logging, no `unwrap`,
/// no slice index that can be out of range. Every fallible step is an `Option` handled with
/// `let … else { return }`, which is also what keeps the buffer's `Drop` — and therefore
/// `pw_stream_queue_buffer` — running on every path out.
fn on_sink_process(stream: &pw::stream::Stream, data: &mut SinkData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some(chunk_data) = buffer.datas_mut().first_mut() else {
        return;
    };

    // `Data::data()` hands back `maxsize` bytes, not the valid ones, so the chunk has to be read
    // first — and it borrows immutably, so it has to be read into locals before `data()` takes the
    // mutable borrow.
    let offset = chunk_data.chunk().offset() as usize;
    let size = chunk_data.chunk().size() as usize;
    let corrupted = chunk_data
        .chunk()
        .flags()
        .contains(libspa::buffer::ChunkFlags::CORRUPTED);

    let channels = data.channels;
    if channels == 0 || size == 0 || corrupted {
        // A zero-sized chunk is PipeWire's silent packet, the analogue of
        // `AUDCLNT_BUFFERFLAGS_SILENT` (`sndDevicesDoCapture.cpp:186-191`). Pushing nothing lets
        // the ring drain and NODE 2 emit silence, which is the same outcome with less work. No
        // channels is no format yet, or one the ring was not built for ([`adopt_sink_format`]).
        return;
    }
    let Some(dsp) = data.dsp.as_mut() else {
        return;
    };

    // Parameters are state: take the newest snapshot, discard anything in between. Events are
    // not: drain them all. The node's volume is state too, and read the same way: nine loads. So
    // is the silence the engine holds the lane in while the system sleeps: one more.
    dsp.refresh();
    // The fade count first: it is written after the gains it fades in to, so a count read here
    // comes with gains at least that new ([`LaneVolume::fades`]).
    let fades = data.volume.fades();
    dsp.set_volume(&data.volume.gains());
    dsp.set_system_mute(data.system_mute.load(Ordering::Relaxed));
    // The target's port changed under the pair and the volume with it (`follow_port`): the new
    // level fades in from silence, as a new pair's does.
    if fades != data.fades_seen {
        data.fades_seen = fades;
        dsp.fade_in();
    }

    let Some(bytes) = chunk_data.data() else {
        return;
    };
    let Some(valid) = bytes.get(offset..offset.saturating_add(size)) else {
        return;
    };

    let block_bytes = dsp.block_bytes();
    let mut frames = 0_u64;
    for block in valid.chunks(block_bytes) {
        let Some(processed) = dsp.process_bytes(block, channels) else {
            break;
        };
        frames += (processed.len() / channels) as u64;
        data.ring.push(processed);
    }

    dsp.publish_meters();
    // Cheap, and it has to be here: the latency changes when a preset switches the denoiser on,
    // and the supervisor is the only thing that can tell PipeWire about it.
    data.counters.dsp_latency_frames.store(
        u32::try_from(dsp.latency_frames()).unwrap_or(u32::MAX),
        Ordering::Relaxed,
    );
    data.counters.sink_cycles.fetch_add(1, Ordering::Relaxed);
    data.counters
        .frames_processed
        .fetch_add(frames, Ordering::Relaxed);
}

/// NODE 2's `process()`: drain the lane's ring into the buffer the real device is about to play —
/// or, in the input lane, into the buffer an application is about to record.
///
/// Same real-time rules as [`on_sink_process`]. The DSP is deliberately *not* run here: if this
/// node underruns we emit silence rather than re-processing stale samples, and NODE 1 is the node
/// whose format we control (`docs/spec/12-audio-io.md` §19.2).
fn on_output_process(stream: &pw::stream::Stream, data: &mut OutData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested() as usize;
    let Some(chunk_data) = buffer.datas_mut().first_mut() else {
        return;
    };

    let channels = data.channels;
    let stride = channels * std::mem::size_of::<f32>();
    // Refuse to play at a stride the ring was not written at: that would be the one bug here that
    // is genuinely unpleasant to listen to. The supervisor rebuilds both nodes when it sees this.
    let mismatched = channels == 0 || channels != data.ring.channels();

    let written = if mismatched {
        if channels != 0 {
            data.counters
                .format_mismatches
                .fetch_add(1, Ordering::Relaxed);
        }
        0
    } else if let Some(bytes) = chunk_data.data() {
        let capacity_frames = bytes.len() / stride;
        let frames = if requested == 0 {
            capacity_frames
        } else {
            requested.min(capacity_frames)
        };

        let mut done = 0_usize;
        while done < frames {
            let take = (frames - done).min(MAX_QUANTUM_FRAMES);
            let Some(scratch) = data.scratch.get_mut(..take * channels) else {
                break;
            };
            data.ring.pop(scratch);
            let start = done * stride;
            let Some(target) = bytes.get_mut(start..start + take * stride) else {
                break;
            };
            let (words, _) = target.as_chunks_mut::<{ std::mem::size_of::<f32>() }>();
            for (raw, &sample) in words.iter_mut().zip(scratch.iter()) {
                *raw = sample.to_le_bytes();
            }
            done += take;
        }
        done
    } else {
        0
    };

    let chunk = chunk_data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = (written * stride) as u32;

    data.counters.output_cycles.fetch_add(1, Ordering::Relaxed);
    if data.status.output_streaming.load(Ordering::Relaxed) {
        // nothing to do; the load exists so the field is not dead weight in release builds
    }
}

// ---------------------------------------------------------------------------------------------
// Publishing to the GUI
// ---------------------------------------------------------------------------------------------

/// Tell the GUI what changed since the last tick: each lane's status and attachment, and the
/// device list. Nothing is sent that the GUI was already told.
fn publish(shared: &mut Shared) {
    for direction in DeviceDirection::ALL {
        publish_lane(shared, direction);
    }
    warn_of_one_headset_on_both_lanes(shared);

    // Echo cancellation starting, stopping or failing, once per change.
    if let Some((running, detail)) = shared.aec.news() {
        shared.notify(AudioToUi::EchoCancel { running, detail });
    }

    if shared.needs_publish {
        shared.needs_publish = false;
        let devices = published_devices(shared);
        if devices != shared.last_devices {
            shared.last_devices.clone_from(&devices);
            shared.notify(AudioToUi::Devices(devices));
            shared.delivery.list_sent();
        }
    }

    publish_app_streams(shared);
}

/// Tell the GUI which applications play and record, when that has changed since it was last told
/// ([`AppStreams::news`]): the whole list, at most once a tick, however many registry and info
/// events changed it since the last.
///
/// Not while a session's first look at the graph is still coming in ([`Shared::ready`]). The
/// streams that were there before FxSound connected are announced in one burst and their infos
/// just after it, all before the barrier is passed — every bind the dump called for is answered
/// ahead of its second `sync` — so the GUI hears of them in one list rather than in instalments.
/// With no session at all, the list is empty, and says so once.
fn publish_app_streams(shared: &mut Shared) {
    if shared.session.is_some() && !shared.ready() {
        return;
    }
    if let Some(streams) = shared.apps.news() {
        log::debug!("{} application streams in the graph", streams.len());
        shared.notify(AudioToUi::AppStreams(streams));
    }
}

/// One lane's [`AudioToUi::Attached`] and [`AudioToUi::Status`], each only when it changed.
///
/// The attachment comes first: it is what the GUI shows as the selected device, and a status that
/// arrived before it would describe a device the window does not know the lane is on yet.
fn publish_lane(shared: &mut Shared, direction: DeviceDirection) {
    publish_attachment(shared, direction);

    let lane = shared.lanes.get_mut(direction);
    let cycles = lane.counters.sink_cycles.load(Ordering::Relaxed);
    let processing = cycles != lane.last_sink_cycles;
    lane.last_sink_cycles = cycles;
    let rate = lane.counters.sample_rate.load(Ordering::Relaxed).max(1);
    let status = AudioStatus {
        processing: processing && lane.nodes.is_some(),
        sample_rate: rate,
        channels: lane.counters.channels.load(Ordering::Relaxed) as u16,
        processed_secs: lane.counters.frames_processed.load(Ordering::Relaxed) / u64::from(rate),
        // The ring's own account of how it is coping. Cumulative since the lane was created, and
        // published rather than only logged: a user whose audio crackles can now say how often,
        // and a test can assert on it.
        dropped_frames: lane.ring.dropped_frames.load(Ordering::Relaxed),
        underrun_frames: lane.ring.underrun_frames.load(Ordering::Relaxed),
        resyncs: lane.ring.resyncs.load(Ordering::Relaxed),
        format_mismatches: lane.format_mismatches_total,
    };
    let status = (status != lane.last_status).then(|| {
        lane.last_status = status;
        status
    });

    // Ring health, per `docs/spec/12-audio-io.md` open question 3: instrument the fill level from
    // day one so drift is detected in the field rather than guessed at.
    let underruns = lane.ring.underrun_frames.load(Ordering::Relaxed);
    if underruns != lane.last_underruns {
        log::debug!(
            "{} ring: fill {} frames, underruns {underruns}, dropped {}, resyncs {}",
            direction.key(),
            lane.ring.fill_frames(),
            lane.ring.dropped_frames.load(Ordering::Relaxed),
            lane.ring.resyncs.load(Ordering::Relaxed),
        );
        lane.last_underruns = underruns;
    }

    if let Some(status) = status {
        shared.notify(AudioToUi::Status { direction, status });
    }
}

/// Tell the GUI what a lane is attached to, if that is not what it was last told — a new target,
/// or `None` once the pair is gone for whatever reason.
///
/// Separate from the status so that a pick can be answered the moment it takes
/// ([`select_device`]) without also judging `processing` over a fraction of a tick, which would
/// flap on a long quantum.
fn publish_attachment(shared: &mut Shared, direction: DeviceDirection) {
    let lane = shared.lanes.get_mut(direction);
    // Every tick asks, and nearly every tick the answer is the one already given: compared in
    // place, so a lane that is merely running does not copy its target's name five times a second.
    if lane.attached.as_deref() == lane.nodes.as_ref().map(|nodes| nodes.target.as_str()) {
        return;
    }
    let target = lane.nodes.as_ref().map(|nodes| nodes.target.clone());
    if let Some(node_name) = lane.note_attachment(target.as_deref()) {
        shared.notify(AudioToUi::Attached {
            direction,
            node_name,
        });
    }
}

/// Warn the user, once per attachment, when both lanes are on one Bluetooth headset — its sink
/// and its microphone ([`DeviceInfo::same_bluetooth_device`]) — with [`ONE_HEADSET_ON_BOTH_LANES`]
/// in the language in effect.
///
/// Nothing is wrong, so it is not an error, and nothing is refused: it may be exactly what the
/// user wants for a call. But the moment anything records from FxSound (Input), WirePlumber
/// switches the headset to its call profile, and the music lane's sink comes back as one channel
/// at the call codec's rate — no stereo, and every EQ band above half of it dead
/// (`docs/0.4.0-upstream.md` U9, the review's risk chain, step 7). Nobody would guess why.
///
/// "Once per attachment" is judged by what the lanes told the GUI they are attached to
/// ([`Lane::attached`]), so it follows the user's choices rather than the pairs' comings and
/// goings: a lane that is merely between pairs — a repair, a profile switch it waits out, a
/// reconnect — changes nothing, while a lane that detaches or moves to another device ends the
/// warning, and coming back to the headset is a new attachment that warns again. A target not in
/// the device list — a sink between profiles — is judged when it is back.
fn warn_of_one_headset_on_both_lanes(shared: &mut Shared) {
    let (output, input) = (&shared.lanes.output, &shared.lanes.input);
    if !output.enabled || !input.enabled {
        shared.headset_warned = None;
        return;
    }
    let (Some(output), Some(input)) = (output.attached.as_deref(), input.attached.as_deref())
    else {
        return;
    };
    if shared
        .headset_warned
        .as_ref()
        .is_some_and(|(o, i)| o == output && i == input)
    {
        return;
    }
    let find = |direction: DeviceDirection, name: &str| {
        shared
            .devices
            .iter()
            .find(|device| device.direction == direction && device.name == name)
    };
    let one_headset = match (
        find(DeviceDirection::Output, output),
        find(DeviceDirection::Input, input),
    ) {
        (Some(sink), Some(microphone)) => sink.same_bluetooth_device(microphone, &shared.cards),
        _ => false,
    };
    let told = one_headset.then(|| (output.to_owned(), input.to_owned()));
    shared.headset_warned = None;
    let Some(told) = told else {
        return;
    };
    log::info!(
        "{} and {} are one Bluetooth headset: its music drops to call quality while anything \
         records from FxSound (Input)",
        told.0,
        told.1
    );
    shared.notify(AudioToUi::Warning {
        direction: None,
        message: fxsound_core::i18n::tr(ONE_HEADSET_ON_BOTH_LANES),
    });
    shared.headset_warned = Some(told);
}

/// The device list as the GUI wants it: every output sorted by description, then every input
/// sorted by description. The GUI draws its section headers off that grouping, so the order is
/// part of the contract. Every device is listed, mono ones included: a Bluetooth headset in its
/// call profile is an output like any other, and one the user may well want to pick.
fn published_devices(shared: &Shared) -> Vec<AudioDevice> {
    let mut published = Vec::with_capacity(shared.devices.len());
    for (direction, default) in shared.defaults.iter() {
        let default = default.current.as_deref();
        let mut group: Vec<AudioDevice> = shared
            .devices
            .iter()
            .filter(|d| d.direction == direction)
            .map(|d| d.to_audio_device(default))
            .collect();
        group.sort_by(|a, b| a.description.cmp(&b.description));
        published.extend(group);
    }
    published
}

mod route_pairs;

#[cfg(test)]
mod live_session;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lane_dsp::tests::lanes_for_tests;

    /// A thread's state before it has connected: no session, nothing held. Nothing in here
    /// touches PipeWire.
    fn shared_for_tests() -> Shared {
        let (notify, _) = crossbeam_channel::unbounded();
        Shared::new(
            notify,
            None,
            None,
            PerDirection::default(),
            ChainHandover::new().0,
        )
    }

    /// A thread between pairs of nodes: both lanes' DSP state is on the main loop.
    fn shared_with_dsp_for_tests() -> Shared {
        let (notify, _) = crossbeam_channel::unbounded();
        let (dsp, handover) = lanes_for_tests();
        Shared::new(
            notify,
            None,
            None,
            PerDirection {
                output: Some(dsp.output),
                input: Some(dsp.input),
            },
            handover,
        )
    }

    /// The chain the input lane's DSP runs, when that DSP is on the main loop.
    fn input_spec_on_the_main_loop(shared: &mut Shared) -> Option<ChainSpec> {
        shared
            .lanes
            .input
            .dsp
            .as_mut()
            .and_then(LaneDsp::as_input_mut)
            .map(|dsp| dsp.engine().spec())
    }

    fn ring_for(channels: usize, quantum: usize) -> SampleRing {
        let ring = SampleRing::new();
        ring.reconfigure(channels, quantum);
        ring
    }

    /// The ring is the one structure both `process()` callbacks touch, so its wait-freedom is the
    /// wait-freedom of the audio path. Asserted structurally: every field is an atomic or an
    /// immutable box, so there is nothing to lock and nothing to allocate.
    #[test]
    fn the_ring_is_wait_free_and_lock_free_by_construction() {
        const fn assert_shareable<T: Send + Sync>() {}
        assert_shareable::<SampleRing>();
        assert_shareable::<Counters>();
        assert_shareable::<StreamStatus>();

        let ring = ring_for(2, 512);
        // Sized once for the worst case, never resized.
        assert_eq!(ring.slots.len(), RING_CAPACITY_FRAMES * 8);
        assert_eq!(ring.slots.len(), ring.mask + 1);
        assert!(ring.slots.len().is_power_of_two());
        assert_eq!(ring.slots.len() * 4, 512 * 1024, "512 KiB, per spec §24");
    }

    #[test]
    fn samples_come_back_out_of_the_ring_exactly_as_they_went_in() {
        // The quantum and the block the consumer asks for are the same number in production —
        // `on_output_process` pops exactly the frames PipeWire requested — so the fixture says so
        // too. 16-frame quantum, target fill 1.5 quanta = 24 frames = 48 samples.
        let ring = ring_for(2, 16);
        let input: Vec<f32> = (0..48).map(|i| i as f32 * 0.25 - 4.0).collect();
        assert_eq!(ring.push(&input), 48);

        // One cycle is one quantum: 16 frames = 32 samples.
        let mut out = vec![0.0; 32];
        assert_eq!(ring.pop(&mut out), 32);
        assert_eq!(
            out,
            input[..32],
            "bit patterns must survive the AtomicU32 round trip"
        );
        // Including the awkward ones. A fresh ring, because the one above still holds the tail
        // of its input and the odd values would come out behind it.
        let odd = [f32::MIN, -0.0, 0.0, f32::MAX, 1.0e-30, -1.0e-30];
        let ring = ring_for(2, 4); // target fill = 6 frames = 12 samples
        let mut pushed = odd.to_vec();
        pushed.extend_from_slice(&[0.0; 10]); // pad past the target so the ring primes
        ring.push(&pushed);
        let mut back = [1.0_f32; 8];
        assert_eq!(ring.pop(&mut back), 8);
        assert_eq!(
            back[..6].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            odd.map(f32::to_bits).to_vec()
        );
    }

    #[test]
    fn the_consumer_stays_silent_until_the_ring_reaches_its_target_fill() {
        // Windows would not start the render client until the capture ring was half full
        // (`sndDevicesDoCapture.cpp:129-140`); this is the same rule.
        let ring = ring_for(2, 8); // target fill = 12 frames = 24 samples
        ring.push(&[1.0; 16]); // 8 frames — not enough
        let mut out = vec![9.0; 16];
        assert_eq!(ring.pop(&mut out), 0);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "must emit silence, not garbage"
        );

        ring.push(&[1.0; 16]); // now 16 frames, past the target
        let mut out = vec![0.0; 16];
        assert_eq!(ring.pop(&mut out), 16);
        assert!(out.iter().all(|&s| s == 1.0));
    }

    #[test]
    fn a_stalled_consumer_makes_the_producer_drop_frames_rather_than_block() {
        let ring = ring_for(2, 512);
        let capacity_samples = ring.slots.len();
        let block = vec![0.5; capacity_samples];
        assert_eq!(ring.push(&block), capacity_samples, "the first block fits");
        assert_eq!(
            ring.push(&[1.0, 1.0]),
            0,
            "a full ring takes nothing more; push must never wait"
        );
        assert_eq!(ring.dropped_frames.load(Ordering::Relaxed), 1);
    }

    /// `build_nodes` always configures the ring for `DEFAULT_QUANTUM_FRAMES`, but the graph
    /// quantum moves without a format change — `node.force-quantum`, a Bluetooth or USB device
    /// that raises it, a low-CPU configuration — and nothing rebuilds the nodes when it does. A
    /// cushion and a resync limit derived from the wrong number then fight the stream for as long
    /// as it lasts.
    #[test]
    fn a_steady_stream_is_clean_at_every_quantum_not_just_the_one_the_nodes_were_built_with() {
        const CHANNELS: usize = 2;
        for graph_quantum in [256_usize, 512, 1024, 2048] {
            let ring = ring_for(CHANNELS, DEFAULT_QUANTUM_FRAMES as usize);
            let block = vec![0.25_f32; graph_quantum * CHANNELS];
            let mut out = vec![0.0_f32; graph_quantum * CHANNELS];

            // The producer runs before the consumer is linked, which is what fills the cushion at
            // startup; after that the two run one block each per graph cycle.
            for _ in 0..3 {
                ring.push(&block);
            }
            for _ in 0..200 {
                ring.pop(&mut out);
                ring.push(&block);
            }

            assert_eq!(
                ring.underrun_frames.load(Ordering::Relaxed),
                0,
                "a steady {graph_quantum}-frame stream underran"
            );
            assert_eq!(
                ring.resyncs.load(Ordering::Relaxed),
                0,
                "a steady {graph_quantum}-frame stream was resynced, which drops audio"
            );
            assert!(
                out.iter().all(|&s| (s - 0.25).abs() < 1e-6),
                "a steady {graph_quantum}-frame stream produced something other than the signal"
            );
        }
    }

    #[test]
    fn a_partial_underrun_is_zero_filled_and_counted() {
        let ring = ring_for(2, 6); // target fill = 9 frames = 18 samples
        ring.push(&[1.0; 18]); // 9 frames: exactly enough to prime
        let mut out = [9.0_f32; 12]; // asking for 6 frames
        assert_eq!(
            ring.pop(&mut out),
            12,
            "primed, so the whole block is served"
        );
        assert_eq!(ring.underrun_frames.load(Ordering::Relaxed), 0);

        // Three frames are left; the next block of six is served short.
        let mut out = [9.0_f32; 12];
        assert_eq!(ring.pop(&mut out), 6);
        assert!(out[..6].iter().all(|&s| s == 1.0));
        assert!(
            out[6..].iter().all(|&s| s == 0.0),
            "the tail must be silence, not the caller's stale buffer"
        );
        assert_eq!(ring.underrun_frames.load(Ordering::Relaxed), 3);
        assert!(
            !ring.primed.load(Ordering::Relaxed),
            "a short read means the cushion is gone, so it must re-buffer rather than click \
             its way through every later cycle"
        );
    }

    #[test]
    fn a_dry_ring_re_primes_before_playing_again() {
        let ring = ring_for(2, 4); // target fill = 6 frames = 12 samples
        ring.push(&[1.0; 12]);
        let mut out = [0.0_f32; 8]; // 4 frames per cycle
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(ring.pop(&mut out), 4, "only two frames were left");
        assert!(!ring.primed.load(Ordering::Relaxed));

        ring.push(&[1.0; 8]); // 4 frames, below the 6-frame target
        assert_eq!(ring.pop(&mut out), 0, "still refilling");
        ring.push(&[1.0; 8]); // 8 frames total, past the target
        assert_eq!(ring.pop(&mut out), 8);
    }

    /// A passive NODE 2 stops in the same cycle as NODE 1 with the cushion still in the ring, and
    /// next runs whenever something plays into NODE 1 again — which may be another application,
    /// minutes later (module docs, "Idle"). What wakes it is heard from its start, after a
    /// re-prime like a new pair's, and never behind the tail of the sound before it.
    #[test]
    fn a_ring_marked_stale_skips_what_the_last_sound_left_and_primes_again_for_the_next() {
        let ring = ring_for(2, 4); // target fill = 6 frames = 12 samples
        let mut out = [0.0_f32; 8]; // 4 frames per cycle

        // A sound plays, and the pair stops with some of it still in the ring.
        ring.push(&[0.5; 16]);
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(ring.fill_frames(), 4, "the cushion the pair stopped with");
        ring.mark_stale();
        assert!(ring.stale_pending());

        // Another sound wakes it, and NODE 1 is first in the cycle: a block of the new sound is
        // in the ring before NODE 2's first pop.
        ring.push(&[-0.25; 8]);
        assert_eq!(
            ring.pop(&mut out),
            0,
            "a woken pair re-primes, as a new one does"
        );
        assert_eq!(out, [0.0; 8]);
        assert!(!ring.stale_pending(), "a mark is acted on once");
        assert_eq!(
            ring.fill_frames(),
            4,
            "only the old sound was skipped, not the new one's first block"
        );

        ring.push(&[-0.25; 8]);
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(
            out, [-0.25; 8],
            "the tail of the last sound was played to the next one"
        );
    }

    /// The mark records a position, not "whatever is in the ring": a pair that stopped with the
    /// ring already dry loses nothing of the next sound to it, and a mark taken late skips only
    /// what was pushed before it, never a block pushed after it.
    ///
    /// A late mark does cost the new sound what it had pushed by then: when NODE 1's `Paused`
    /// reaches the main loop only after the next sound has started, the blocks of that sound
    /// already in the ring are skipped with the old tail, since the ring cannot tell the two
    /// apart. This pins the bound on that, not its absence.
    #[test]
    fn a_stale_mark_skips_nothing_pushed_after_it() {
        let ring = ring_for(2, 4); // target fill = 6 frames = 12 samples
        let mut out = [0.0_f32; 8];
        ring.push(&[0.5; 12]);
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(ring.pop(&mut out), 4, "played dry before the pair stopped");
        ring.mark_stale();

        ring.push(&[-0.25; 12]); // exactly the target
        assert_eq!(
            ring.pop(&mut out),
            8,
            "a dry ring's mark delayed the next sound"
        );
        assert_eq!(out, [-0.25; 8]);

        // A mark taken after the new sound had started: the ring holds only what was pushed
        // before it, and a later block stays.
        let ring = ring_for(2, 4);
        ring.push(&[0.5; 8]);
        ring.mark_stale();
        ring.push(&[-0.25; 16]);
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(out, [-0.25; 8], "the samples before the mark were played");
        assert_eq!(ring.fill_frames(), 4, "samples after the mark were skipped");
    }

    #[test]
    fn rebuilding_a_ring_forgets_a_stale_mark_it_had_not_acted_on() {
        let ring = ring_for(2, 4);
        ring.push(&[0.5; 16]);
        ring.mark_stale();
        ring.reconfigure(2, 4);
        assert!(!ring.stale_pending());
    }

    #[test]
    fn the_consumer_drops_the_oldest_audio_when_latency_runs_away() {
        // A quantum change or a briefly faster producer must not leave a permanently deeper
        // buffer: the consumer skips forward rather than letting latency accumulate.
        let ring = ring_for(2, 4); // target 6 frames, limit 24 frames = 48 samples
        let stale: Vec<f32> = (0..200).map(|i| i as f32).collect();
        ring.push(&stale);
        assert_eq!(ring.fill_frames(), 100);

        let mut out = [0.0_f32; 8];
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(ring.resyncs.load(Ordering::Relaxed), 1);
        assert!(
            out[0] >= 152.0,
            "the samples handed over must be the newest, not the oldest: got {}",
            out[0]
        );
        assert!(
            ring.fill_frames() <= 24,
            "latency must be back under the limit"
        );
    }

    #[test]
    fn the_ring_only_ever_holds_whole_frames() {
        let ring = ring_for(6, 64);
        // An odd number of samples cannot be stored as whole 6-channel frames.
        assert_eq!(ring.push(&[1.0; 7]), 6);
        assert_eq!(ring.push(&[1.0; 5]), 0);
        assert_eq!(ring.fill_frames(), 1);
    }

    /// Every sample the producer pushes is its own index in the stream, so a frame always starts
    /// on an even one: a consumer that ever read from the middle of a frame — after a skip, a
    /// resync or a wrap — would see an odd sample where a frame's first belongs, or two samples of
    /// one frame that are not neighbours. Neither thread waits for the other at any point, which
    /// is the "neither block" half: the test finishes.
    ///
    /// The consumer runs until the producer is done and the ring is dry, not for a fixed number of
    /// pops, so a loaded machine that schedules it before the producer starts still sees audio.
    #[test]
    fn a_producer_and_a_consumer_on_two_threads_neither_block_nor_lose_alignment() {
        use std::sync::atomic::AtomicBool;

        let ring = Arc::new(ring_for(2, 8));
        let done = Arc::new(AtomicBool::new(false));
        let producer = {
            let ring = Arc::clone(&ring);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut next = 0.0_f32;
                for _ in 0..2_000 {
                    let block: Vec<f32> = (0..64).map(|i| next + i as f32).collect();
                    let pushed = ring.push(&block);
                    next += pushed as f32;
                    if pushed < block.len() {
                        // Full: let the consumer in, rather than dropping every later block too.
                        std::thread::yield_now();
                    }
                }
                done.store(true, Ordering::Release);
                next
            })
        };
        let consumer = {
            let ring = Arc::clone(&ring);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut out = vec![0.0_f32; 64];
                let mut total = 0_usize;
                let mut misaligned = Vec::new();
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let finished = done.load(Ordering::Acquire);
                    let got = ring.pop(&mut out);
                    for &[first, second] in out[..got].as_chunks::<2>().0 {
                        if first % 2.0 != 0.0 || second != first + 1.0 {
                            misaligned.push((first, second));
                        }
                    }
                    total += got;
                    if (finished && got == 0) || Instant::now() > deadline {
                        break;
                    }
                    if got == 0 {
                        std::thread::yield_now();
                    }
                }
                (total, misaligned)
            })
        };
        let produced = producer.join().expect("producer");
        let (consumed, misaligned) = consumer.join().expect("consumer");
        assert!(consumed > 0, "the consumer must have seen real audio");
        assert_eq!(consumed % 2, 0, "only whole frames are handed over");
        assert!(
            produced >= consumed as f32,
            "the consumer cannot have read more than was written"
        );
        assert!(
            misaligned.is_empty(),
            "frames read from the middle: {:?}",
            &misaligned[..misaligned.len().min(8)]
        );
        assert!(ring.fill_frames() * 2 <= ring.slots.len());
    }

    #[test]
    fn reconfiguring_the_ring_discards_audio_in_the_old_format() {
        let ring = ring_for(2, 8);
        ring.push(&[1.0; 64]);
        assert_eq!(ring.fill_frames(), 32);
        ring.reconfigure(6, 8);
        assert_eq!(ring.fill_frames(), 0);
        assert_eq!(ring.channels(), 6);
        assert!(!ring.primed.load(Ordering::Relaxed));
    }

    #[test]
    fn the_backoff_never_retries_faster_than_two_hundred_milliseconds() {
        assert_eq!(BACKOFF_MS[0], 200);
        assert_eq!(SUPERVISOR_PERIOD, Duration::from_millis(200));
        for pair in BACKOFF_MS.windows(2) {
            assert!(pair[1] > pair[0], "the backoff must grow: {pair:?}");
        }
        assert_eq!(*BACKOFF_MS.last().expect("non-empty"), 5000);
        assert!(BACKOFF_MS.iter().all(|&ms| ms >= 200));
    }

    #[test]
    fn only_streaming_wakes_node_two_and_only_paused_puts_it_to_sleep() {
        assert_eq!(second_node_wish(&StreamState::Streaming), Wish::Run);
        assert_eq!(second_node_wish(&StreamState::Paused), Wish::Sleep);
        // On the way to the server, on the way off it, or failed: nothing to go by, and a NODE 1 in
        // error takes its NODE 2 down with it anyway.
        for state in [
            StreamState::Unconnected,
            StreamState::Connecting,
            StreamState::Error("gone".to_owned()),
        ] {
            assert_eq!(second_node_wish(&state), Wish::AsYouWere, "{state:?}");
        }
    }

    #[test]
    fn node_one_s_wish_crosses_to_the_main_loop_intact_and_a_new_pair_starts_with_none() {
        let status = StreamStatus::default();
        assert_eq!(
            Wish::from_code(status.second_wish.load(Ordering::Relaxed)),
            Wish::AsYouWere,
            "a pair that has said nothing asks for nothing"
        );
        for (state, wish) in [
            (StreamState::Connecting, Wish::AsYouWere),
            (StreamState::Paused, Wish::Sleep),
            (StreamState::Streaming, Wish::Run),
            (StreamState::Paused, Wish::Sleep),
        ] {
            status.first_node_moved(&state);
            assert_eq!(
                Wish::from_code(status.second_wish.load(Ordering::Relaxed)),
                wish
            );
        }
        assert_eq!(status.first_changes.load(Ordering::Relaxed), 4);

        status.clear();
        assert_eq!(
            Wish::from_code(status.second_wish.load(Ordering::Relaxed)),
            Wish::AsYouWere,
            "the next pair must not be put to sleep on the word of the last one"
        );
        assert_eq!(status.first_changes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn node_two_sleeps_only_once_node_one_has_been_paused_for_the_whole_wait() {
        let start = Instant::now();
        let mut pace = SecondNodePace::new();
        assert!(pace.active, "NODE 2 is connected active");

        // NODE 1 comes up paused: nothing plays into it yet.
        assert_eq!(pace.next(Wish::Sleep, 1, start), None, "not at once");
        assert_eq!(
            pace.next(Wish::Sleep, 1, start + SLEEP_AFTER / 2),
            None,
            "not half way through the wait"
        );
        assert_eq!(pace.next(Wish::Sleep, 1, start + SLEEP_AFTER), Some(false));
        // Nothing is taken as done until the call has been made: asked again, it says so again.
        assert_eq!(
            pace.next(Wish::Sleep, 1, start + SLEEP_AFTER * 2),
            Some(false)
        );
        pace.told(false);
        assert_eq!(
            pace.next(Wish::Sleep, 1, start + SLEEP_AFTER * 3),
            None,
            "asleep is asleep"
        );
    }

    #[test]
    fn node_two_wakes_the_moment_node_one_streams() {
        let start = Instant::now();
        let mut pace = SecondNodePace::new();
        pace.told(false);

        assert_eq!(
            pace.next(Wish::Run, 2, start),
            Some(true),
            "no waiting on the way up: every block NODE 1 pushes before NODE 2 runs is latency"
        );
        pace.told(true);
        assert_eq!(pace.next(Wish::Run, 2, start), None, "awake is awake");
    }

    #[test]
    fn a_pause_that_ended_and_began_again_between_two_looks_starts_the_wait_again() {
        let start = Instant::now();
        let mut pace = SecondNodePace::new();
        assert_eq!(pace.next(Wish::Sleep, 1, start), None);

        // Two more states went by unseen — it streamed and paused again — just before the wait
        // would have run out. The pause that counts is the one that began a moment ago.
        let later = start + SLEEP_AFTER;
        assert_eq!(pace.next(Wish::Sleep, 3, later), None);
        assert_eq!(
            pace.next(Wish::Sleep, 3, later + SLEEP_AFTER / 2),
            None,
            "the wait restarted"
        );
        assert_eq!(pace.next(Wish::Sleep, 3, later + SLEEP_AFTER), Some(false));

        // And a stream that was seen cancels a wait in progress outright.
        let mut pace = SecondNodePace::new();
        assert_eq!(pace.next(Wish::Sleep, 1, start), None);
        assert_eq!(pace.next(Wish::Run, 2, start + SLEEP_AFTER / 2), None);
        assert_eq!(pace.next(Wish::Sleep, 3, start + SLEEP_AFTER), None);
        assert_eq!(
            pace.next(Wish::Sleep, 3, start + SLEEP_AFTER * 2),
            Some(false)
        );
    }

    #[test]
    fn a_node_one_that_says_nothing_leaves_node_two_as_it_is() {
        let start = Instant::now();
        for active in [true, false] {
            let mut pace = SecondNodePace::new();
            pace.told(active);
            for later in [Duration::ZERO, SLEEP_AFTER * 10] {
                assert_eq!(pace.next(Wish::AsYouWere, 1, start + later), None);
            }
            assert_eq!(pace.active, active);
        }
    }

    #[test]
    fn a_passive_playback_stream_is_asked_of_every_server_that_runs_a_link_group_together() {
        for version in [
            "0.3.68",
            "0.3.85",
            "1.0.5",
            "1.2.7",
            "1.6.8",
            "2.0.0",
            " 1.6.8 ",
            "1.4.2-rc1",
        ] {
            assert!(schedules_link_groups(version), "{version}");
        }
        // Older servers, and anything that cannot be read as a version, pace NODE 2 by hand:
        // guessing wrong that way costs idle power, guessing wrong the other way costs the sound.
        for version in ["0.3.65", "0.3.67", "0.2.99", "", "1.6", "pipewire", "x.y.z"] {
            assert!(!schedules_link_groups(version), "{version:?}");
        }
    }

    #[test]
    fn both_lanes_idle_where_the_server_runs_a_link_group_together_and_only_the_speakers_by_hand_elsewhere()
     {
        assert_eq!(
            idle_plan(DeviceDirection::Output, true),
            (true, None),
            "a server that runs a link-group together idles a passive NODE 2 by itself"
        );
        assert_eq!(
            idle_plan(DeviceDirection::Input, true),
            (true, None),
            "and a passive capture stream the same way, woken by whatever records from the source"
        );
        assert_eq!(
            idle_plan(DeviceDirection::Output, false),
            (false, Some(SecondNodePace::new())),
            "an older one gets an ordinary NODE 2, paced by hand"
        );
        assert_eq!(
            idle_plan(DeviceDirection::Input, false),
            (false, None),
            "and an ordinary capture stream, left running: nothing would wake a passive one there"
        );
    }

    #[test]
    fn the_stream_that_holds_the_microphone_awake_is_one_wireplumber_switches_a_headset_for() {
        pw::init();
        let props = keep_awake_props("512/48000");
        // What `device/autoswitch-bluetooth-profile.lua` (WirePlumber 0.5.17) asks of a stream
        // before it switches a headset to its call profile for it.
        assert_eq!(props.get("media.class"), Some("Stream/Input/Audio"));
        assert_eq!(
            props.get("node.link-group"),
            None,
            "a stream in a link-group is passed over, as the pair's capture stream is"
        );
        assert_eq!(props.get("stream.monitor"), None);
        assert_eq!(props.get("bluez5.loopback"), None);
        // And what the server asks of a link before it runs what is behind it.
        assert_eq!(
            props.get("node.passive"),
            Some("false"),
            "a passive recorder would wake nothing"
        );
        // Recording FxSound (Input) and nothing else, for good.
        assert_eq!(props.get("target.object"), Some(SOURCE_NODE_NAME));
        assert_eq!(props.get("node.autoconnect"), Some("true"));
        for key in [
            "node.dont-reconnect",
            "node.dont-fallback",
            "node.dont-move",
            "node.linger",
        ] {
            assert_eq!(props.get(key), Some("true"), "{key}");
        }
        assert_eq!(props.get("stream.capture.sink"), Some("false"));
        assert_eq!(
            props.get("state.restore-props"),
            Some("false"),
            "a mute set on the recorder in a mixer must not be restored onto the capture stream, \
             which WirePlumber keeps under the same key"
        );
        assert_eq!(props.get("node.name"), Some(KEEP_AWAKE_NODE_NAME));
        assert_eq!(
            props.get("node.description"),
            Some(KEEP_AWAKE_STREAM_DESCRIPTION)
        );
        assert_eq!(props.get("media.name"), Some(KEEP_AWAKE_STREAM_DESCRIPTION));
        assert_eq!(props.get("application.name"), Some("FxSound"));
        assert_eq!(props.get("node.latency"), Some("512/48000"));
        assert!(
            is_ours(KEEP_AWAKE_NODE_NAME),
            "never a device, never a default to remember"
        );
    }

    #[test]
    fn the_format_pod_round_trips_through_libspa() {
        let positions = ChannelMap::default_for(6);
        let bytes = format_pod(48_000, 6, &positions);
        assert!(!bytes.is_empty(), "serialising the format pod must succeed");

        let pod = Pod::from_bytes(&bytes).expect("a valid pod");
        let (media_type, media_subtype) =
            format_utils::parse_format(pod).expect("an audio/raw format object");
        assert_eq!(media_type, MediaType::Audio);
        assert_eq!(media_subtype, MediaSubtype::Raw);

        let mut parsed = AudioInfoRaw::new();
        parsed
            .parse(pod)
            .expect("the pod describes a raw audio format");
        assert_eq!(parsed.rate(), 48_000);
        assert_eq!(parsed.channels(), 6);
        assert_eq!(parsed.format(), AudioFormat::F32LE);
        assert_eq!(
            &parsed.position()[..6],
            positions.ids(),
            "the channel map must survive serialisation, or PipeWire silently remixes"
        );
    }

    /// The property sets are pure functions of the direction and the target, so the exact
    /// spellings `docs/spec/12-audio-io.md` §20 and §28.2 prescribe can be asserted without a
    /// server. `pw::init()` is idempotent and creates no connection.
    #[test]
    fn the_four_nodes_carry_the_properties_the_spec_prescribes() {
        pw::init();
        let positions = ChannelMap::default_for(2);

        let sink = virtual_node_props(
            DeviceDirection::Output,
            None,
            2,
            48_000,
            &positions,
            "512/48000",
        );
        assert_eq!(sink.get("media.class"), Some("Audio/Sink"));
        assert_eq!(sink.get("node.name"), Some(SINK_NODE_NAME));
        assert_eq!(sink.get("node.link-group"), Some(crate::LINK_GROUP));
        assert_eq!(sink.get("node.virtual"), Some("true"));
        assert_eq!(sink.get("node.want-driver"), Some("true"));
        assert_eq!(sink.get("audio.channels"), Some("2"));
        assert_eq!(sink.get("audio.rate"), Some("48000"));
        assert_eq!(sink.get("audio.format"), Some("F32"));
        assert_eq!(sink.get("audio.position"), Some("FL,FR"));
        assert_eq!(sink.get("priority.session"), Some(NODE_PRIORITY_SESSION));
        assert_eq!(sink.get("priority.driver"), Some("0"));
        assert_eq!(sink.get("device.class"), Some("sound"));
        let description = sink.get("node.description").expect("a description");
        assert!(description.starts_with("FxSound ("), "{description}");
        assert_eq!(sink.get("node.nick"), Some(description));

        let source = virtual_node_props(
            DeviceDirection::Input,
            None,
            2,
            48_000,
            &positions,
            "512/48000",
        );
        assert_eq!(
            source.get("media.class"),
            Some("Audio/Source"),
            "not Audio/Source/Virtual"
        );
        assert_eq!(source.get("node.name"), Some(SOURCE_NODE_NAME));
        assert_eq!(source.get("node.link-group"), Some(crate::INPUT_LINK_GROUP));
        assert_eq!(source.get("node.virtual"), Some("true"));
        assert_eq!(source.get("node.want-driver"), Some("true"));
        assert_eq!(source.get("audio.channels"), Some("2"));
        assert_eq!(source.get("priority.session"), Some(NODE_PRIORITY_SESSION));
        assert_ne!(
            source.get("node.description"),
            sink.get("node.description"),
            "the two virtual nodes must be distinguishable in a device list"
        );

        let output = stream_props(DeviceDirection::Output, "alsa_output.x", "512/48000", true);
        assert_eq!(output.get("media.class"), Some("Stream/Output/Audio"));
        assert_eq!(output.get("media.category"), Some("Playback"));
        assert_eq!(output.get("media.role"), Some("Production"));
        assert_eq!(output.get("node.name"), Some(OUTPUT_NODE_NAME));
        assert_eq!(
            output.get("node.description"),
            Some(OUTPUT_STREAM_DESCRIPTION)
        );
        assert_eq!(
            output.get("node.link-group"),
            sink.get("node.link-group"),
            "the playback stream is in its sink's group"
        );
        assert_eq!(output.get("node.passive"), Some("true"));
        assert_eq!(
            stream_props(DeviceDirection::Output, "alsa_output.x", "512/48000", false)
                .get("node.passive"),
            Some("false"),
            "a server that does not run a link-group together gets an ordinary stream"
        );
        assert_eq!(output.get("node.autoconnect"), Some("true"));
        assert_eq!(output.get("target.object"), Some("alsa_output.x"));
        assert_eq!(output.get("stream.capture.sink"), None);

        let capture = stream_props(DeviceDirection::Input, "alsa_input.mic", "512/48000", true);
        assert_eq!(capture.get("media.class"), Some("Stream/Input/Audio"));
        assert_eq!(capture.get("media.category"), Some("Capture"));
        assert_eq!(capture.get("media.role"), Some("Production"));
        assert_eq!(capture.get("node.name"), Some(CAPTURE_NODE_NAME));
        assert_eq!(
            capture.get("node.description"),
            Some(CAPTURE_STREAM_DESCRIPTION)
        );
        assert_eq!(
            capture.get("node.link-group"),
            source.get("node.link-group"),
            "the capture stream is in its source's group"
        );
        assert_ne!(
            capture.get("node.link-group"),
            output.get("node.link-group"),
            "one group for both lanes runs the speakers for as long as the microphone runs"
        );
        assert_eq!(
            capture.get("node.passive"),
            Some("true"),
            "the microphone runs only while something records from the source (U19)"
        );
        assert_eq!(
            stream_props(DeviceDirection::Input, "alsa_input.mic", "512/48000", false)
                .get("node.passive"),
            Some("false"),
            "a server that does not run a link-group together gets an ordinary stream"
        );
        assert_eq!(capture.get("node.autoconnect"), Some("true"));
        assert_eq!(capture.get("target.object"), Some("alsa_input.mic"));
        assert_eq!(
            capture.get("stream.capture.sink"),
            Some("false"),
            "capture the microphone, not a sink monitor"
        );
    }

    #[test]
    fn both_virtual_nodes_keep_their_volume_from_wireplumber_and_their_adapter_at_unity() {
        pw::init();
        let positions = ChannelMap::default_for(2);
        for direction in DeviceDirection::ALL {
            let props = virtual_node_props(direction, None, 2, 48_000, &positions, "512/48000");
            let name = our_node_name(direction);
            assert_eq!(
                props.get("state.restore-props"),
                Some("false"),
                "WirePlumber would restore one volume for {name} whatever it is attached to"
            );
            assert_eq!(props.get("channelmix.min-volume"), Some("1.0"), "{name}");
            assert_eq!(props.get("channelmix.max-volume"), Some("1.0"), "{name}");
        }
        for direction in DeviceDirection::ALL {
            let props = stream_props(direction, "alsa.x", "512/48000", false);
            assert_eq!(
                props.get("channelmix.max-volume"),
                None,
                "a device-facing stream's volume is the adapter's, as it always was"
            );
            assert_eq!(
                props.get("state.restore-props"),
                Some("false"),
                "and WirePlumber's one volume for FxSound's streams is not restored onto it: a \
                 mute left on the capture stream would silence every microphone after it"
            );
        }
    }

    // ---- the virtual nodes' volume (U10, U11)

    fn remembered(direction: DeviceDirection, target: &str, volumes: &[f32]) -> TargetVolume {
        TargetVolume {
            direction,
            target: target.to_owned(),
            port: String::new(),
            channel_volumes: volumes.to_vec(),
            mute: false,
        }
    }

    fn at(volumes: &[f32]) -> NodeVolume {
        NodeVolume {
            volume: 1.0,
            channel_volumes: volumes.to_vec(),
            mute: false,
        }
    }

    fn volume_reports(messages: &Receiver<AudioToUi>) -> Vec<TargetVolume> {
        drained(messages)
            .into_iter()
            .filter_map(|message| match message {
                AudioToUi::TargetVolume(volume) => Some(volume),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_seed_replaces_the_memory_drops_what_cannot_be_replayed_and_keeps_the_later_of_two() {
        let (mut shared, _) = shared_with_messages();
        shared.target_volumes = vec![remembered(DeviceDirection::Output, "old", &[0.5])];
        control(
            &mut shared,
            UiToAudio::SeedTargetVolumes(vec![
                remembered(DeviceDirection::Output, "speakers", &[0.2, 0.2]),
                remembered(DeviceDirection::Output, "broken", &[f32::NAN, 0.3]),
                remembered(DeviceDirection::Output, "", &[0.3]),
                remembered(DeviceDirection::Output, "speakers", &[0.4, 0.4]),
                remembered(DeviceDirection::Input, "speakers", &[0.9]),
            ]),
        );
        assert_eq!(
            shared.target_volumes,
            [
                remembered(DeviceDirection::Output, "speakers", &[0.4, 0.4]),
                remembered(DeviceDirection::Input, "speakers", &[0.9]),
            ],
            "the same name in the other direction is another device"
        );
    }

    #[test]
    fn a_seed_keeps_one_volume_per_port_of_the_same_sink() {
        let (mut shared, _) = shared_with_messages();
        let on = |port: &str, level: f32| TargetVolume {
            port: port.to_owned(),
            ..remembered(DeviceDirection::Output, "hda", &[level, level])
        };
        control(
            &mut shared,
            UiToAudio::SeedTargetVolumes(vec![
                on("analog-output-speaker", 1.0),
                on("analog-output-headphones", 0.2),
                on("analog-output-speaker", 0.8),
            ]),
        );
        assert_eq!(
            shared.target_volumes,
            [
                on("analog-output-headphones", 0.2),
                on("analog-output-speaker", 0.8)
            ],
            "the speakers and the headphones of one sink are two devices; of two for one port, the \
             later"
        );
    }

    #[test]
    fn a_seed_leaves_out_an_entry_with_no_level_and_keeps_one_that_remembers_a_mute() {
        let (mut shared, _) = shared_with_messages();
        let mut bare_mute = remembered(DeviceDirection::Output, "tv", &[]);
        bare_mute.mute = true;
        control(
            &mut shared,
            UiToAudio::SeedTargetVolumes(vec![
                remembered(DeviceDirection::Output, "hand-edited", &[]),
                remembered(DeviceDirection::Output, "headphones", &[0.4, 0.4]),
                bare_mute.clone(),
            ]),
        );
        assert_eq!(
            shared.target_volumes,
            [
                remembered(DeviceDirection::Output, "headphones", &[0.4, 0.4]),
                bare_mute
            ],
            "an entry with neither a level nor a mute says nothing about its device"
        );
    }

    #[test]
    fn a_hand_edited_entry_with_no_level_neither_raises_its_own_device_nor_silences_a_new_one() {
        // Put in place past the seed's filter, as a later path to the memory might: what a pair
        // starts at must not depend on the filter having run.
        let (mut shared, _) = shared_with_messages();
        shared.target_volumes = vec![
            remembered(DeviceDirection::Output, "hand-edited", &[]),
            remembered(DeviceDirection::Output, "headphones", &[0.4, 0.4]),
        ];
        shared.lanes.output.last_volume = Some(at(&[0.3, 0.3]));
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "hand-edited", None, 2).effective(2),
            [0.3, 0.3],
            "the entry's own device: not the unity of an empty list"
        );
        shared.lanes.output.last_volume = None;
        let new = volume_for_pair(&shared, DeviceDirection::Output, "usb-dac", None, 2);
        assert_eq!(
            new.effective(2),
            [0.4, 0.4],
            "a device never seen: the quietest level remembered, not the silence of no level"
        );
        assert!(!new.mute);
    }

    #[test]
    fn a_first_pair_with_nothing_remembered_is_no_louder_than_what_wireplumber_kept_for_0_3_0() {
        // 0.3.0 at about −24 dB, in WirePlumber's memory; the app's own memory still empty.
        let (mut shared, _) = shared_with_messages();
        shared.inherited_volumes.output = Some(at(&[0.064, 0.064]));
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "speakers", None, 2).effective(2),
            [0.064, 0.064],
            "not the unity a new node is made with: 24 dB louder than the user left it"
        );
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Input, "microphone", None, 2),
            NodeVolume::default(),
            "each lane by what WirePlumber kept for its own node"
        );
        shared.inherited_volumes.output = Some(at(&[2.0, 2.0]));
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "speakers", None, 2).effective(2),
            [1.0, 1.0],
            "and never above unity: it is a ceiling, not a level to replay"
        );
    }

    #[test]
    fn what_wireplumber_kept_gives_way_to_anything_the_lane_or_the_app_knows() {
        let (mut shared, _) = shared_with_messages();
        shared.inherited_volumes.output = Some(at(&[0.064, 0.064]));
        shared.target_volumes = vec![remembered(
            DeviceDirection::Output,
            "headphones",
            &[0.5, 0.5],
        )];
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "headphones", None, 2).effective(2),
            [0.5, 0.5],
            "a remembered device: its own level"
        );
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "usb-dac", None, 2).effective(2),
            [0.5, 0.5],
            "a device never seen: the quietest the app remembers, which is newer than 0.3.0"
        );
        shared.target_volumes.clear();
        shared.lanes.output.last_volume = Some(at(&[0.7, 0.7]));
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "usb-dac", None, 2).effective(2),
            [0.7, 0.7],
            "a lane that has had a pair this run: that pair's level"
        );
    }

    #[test]
    fn wireplumbers_key_for_our_nodes_is_made_from_the_properties_they_carry() {
        // `formKey` in `node/state-stream.lua`: the media class, then `application.id`.
        pw::init();
        let positions = ChannelMap::default_for(2);
        for direction in DeviceDirection::ALL {
            let props = virtual_node_props(direction, None, 2, 48_000, &positions, "512/48000");
            let media_class = props.get("media.class").expect("a media class");
            let app_id = props.get("application.id").expect("an application id");
            assert_eq!(
                volume::wireplumber_key(direction),
                format!("{media_class}:application.id:{app_id}")
            );
            assert!(
                !volume::wireplumber_key(direction).contains([' ', '=', '[', ']', '\\']),
                "a key WirePlumber would have escaped in its file"
            );
        }
        assert_eq!(
            volume::wireplumber_key(DeviceDirection::Output),
            "Audio/Sink:application.id:com.fxsound.FxSound",
            "the key 0.3.0's sink was saved under"
        );
    }

    #[test]
    fn a_new_pair_on_a_remembered_target_starts_at_that_targets_volume() {
        let (mut shared, _) = shared_with_messages();
        shared.target_volumes = vec![remembered(
            DeviceDirection::Output,
            "headphones",
            &[0.3, 0.3],
        )];
        shared.lanes.output.last_volume = Some(at(&[0.8, 0.8]));
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "headphones", None, 2).effective(2),
            [0.3, 0.3]
        );
    }

    #[test]
    fn a_new_pair_on_a_target_never_seen_is_no_louder_than_the_lanes_last_pair() {
        let (mut shared, _) = shared_with_messages();
        shared.target_volumes = vec![remembered(
            DeviceDirection::Output,
            "headphones",
            &[0.1, 0.1],
        )];
        shared.lanes.output.last_volume = Some(at(&[0.4, 0.4]));
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "usb-dac", None, 2).effective(2),
            [0.4, 0.4],
            "the lane's last pair, not the quietest remembered device, once there has been one"
        );
    }

    #[test]
    fn a_first_pair_on_a_target_never_seen_is_no_louder_than_the_quietest_remembered_device() {
        let (mut shared, _) = shared_with_messages();
        shared.target_volumes = vec![
            remembered(DeviceDirection::Output, "speakers", &[0.9, 0.9]),
            remembered(DeviceDirection::Output, "headphones", &[0.2, 0.2]),
            remembered(DeviceDirection::Input, "microphone", &[0.05]),
        ];
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Output, "usb-dac", None, 2).effective(2),
            [0.2, 0.2]
        );
        assert_eq!(
            volume_for_pair(&shared, DeviceDirection::Input, "webcam", None, 2).effective(2),
            [0.05, 0.05],
            "each lane by its own direction's memory"
        );
        let (fresh, _) = shared_with_messages();
        assert_eq!(
            volume_for_pair(&fresh, DeviceDirection::Output, "usb-dac", None, 2),
            NodeVolume::default(),
            "nothing remembered anywhere: the unity a new node has"
        );
    }

    #[test]
    fn a_volume_is_reported_once_and_remembered_for_its_target_and_lane() {
        let (mut shared, messages) = shared_with_messages();
        report_target_volume(
            &mut shared,
            DeviceDirection::Output,
            "speakers",
            None,
            &at(&[0.5, 0.25]),
            2,
        );
        report_target_volume(
            &mut shared,
            DeviceDirection::Output,
            "speakers",
            None,
            &at(&[0.5, 0.25]),
            2,
        );
        assert_eq!(
            volume_reports(&messages),
            [remembered(
                DeviceDirection::Output,
                "speakers",
                &[0.5, 0.25]
            )],
            "the same volume again is nothing to report"
        );
        report_target_volume(
            &mut shared,
            DeviceDirection::Input,
            "speakers",
            None,
            &at(&[0.5]),
            1,
        );
        let muted = NodeVolume {
            mute: true,
            ..at(&[0.5, 0.25])
        };
        report_target_volume(
            &mut shared,
            DeviceDirection::Output,
            "speakers",
            None,
            &muted,
            2,
        );
        let mut expected_mute = remembered(DeviceDirection::Output, "speakers", &[0.5, 0.25]);
        expected_mute.mute = true;
        assert_eq!(
            volume_reports(&messages),
            [
                remembered(DeviceDirection::Input, "speakers", &[0.5]),
                expected_mute.clone()
            ]
        );
        assert_eq!(
            shared.target_volumes,
            [
                remembered(DeviceDirection::Input, "speakers", &[0.5]),
                expected_mute
            ],
            "one entry per direction and target, the newest"
        );
    }

    #[test]
    fn the_master_scalar_is_folded_into_the_reported_channel_volumes() {
        let (mut shared, messages) = shared_with_messages();
        let volume = NodeVolume {
            volume: 0.5,
            ..at(&[0.8, 0.4])
        };
        report_target_volume(
            &mut shared,
            DeviceDirection::Output,
            "speakers",
            None,
            &volume,
            2,
        );
        assert_eq!(
            volume_reports(&messages),
            [remembered(DeviceDirection::Output, "speakers", &[0.4, 0.2])]
        );
    }

    #[test]
    fn a_lane_without_a_pair_has_no_volume_to_report_or_retire() {
        let (mut shared, messages) = shared_with_messages();
        shared.lanes.output.volume.begin_pair(2, &at(&[0.3, 0.3]));
        for _ in 0..3 {
            watch_volume(&mut shared, DeviceDirection::Output);
        }
        retire_volume(&mut shared, DeviceDirection::Output);
        assert!(volume_reports(&messages).is_empty());
        assert_eq!(shared.lanes.output.last_volume, None);
    }

    #[test]
    fn a_seed_with_no_pair_up_changes_no_lanes_volume() {
        let (mut shared, messages) = shared_with_messages();
        let before = shared.lanes.output.volume.changes();
        control(
            &mut shared,
            UiToAudio::SeedTargetVolumes(vec![remembered(
                DeviceDirection::Output,
                "speakers",
                &[0.2, 0.2],
            )]),
        );
        assert_eq!(shared.lanes.output.volume.changes(), before);
        assert!(volume_reports(&messages).is_empty(), "a seed is not news");
    }

    #[test]
    fn a_seed_whose_muted_level_differs_from_the_muted_pairs_replaces_the_level() {
        // The first pair came up before the seed, muted at 0.9 — where a 0.3.0 install was left,
        // or where the run's last pair was.
        let current = NodeVolume {
            mute: true,
            ..at(&[0.9, 0.9])
        };
        let entry = TargetVolume {
            mute: true,
            ..remembered(DeviceDirection::Output, "speakers", &[0.1, 0.1])
        };
        assert_eq!(
            current.gains(2),
            NodeVolume::remembered(&entry).gains(2),
            "the two sound alike while muted"
        );
        let replaced = reseeded(&entry, &current, 2).expect("the level differs, so it is replaced");
        assert_eq!(replaced.effective(2), [0.1, 0.1]);
        assert!(replaced.mute, "and stays muted");

        let same = NodeVolume {
            mute: true,
            ..at(&[0.1, 0.1])
        };
        assert_eq!(
            reseeded(&entry, &same, 2),
            None,
            "a pair muted at the remembered level already is left alone"
        );
        let unmuted = at(&[0.1, 0.1]);
        assert!(
            reseeded(&entry, &unmuted, 2).is_some_and(|volume| volume.mute),
            "the same level, but the device is remembered muted"
        );
    }

    /// A `Props` write as `param_changed` hands it over: `channel_volumes` when it sets them, and
    /// `params` — key, value, key, value — when it carries any.
    fn props_write(
        channel_volumes: Option<&[f32]>,
        params: Option<Vec<(&str, libspa::pod::Value)>>,
    ) -> Vec<u8> {
        use libspa::pod::{Object, Property, PropertyFlags, Value, ValueArray};

        let property = |key: u32, value: Value| Property {
            key,
            flags: PropertyFlags::empty(),
            value,
        };
        let mut properties = Vec::new();
        if let Some(volumes) = channel_volumes {
            properties.push(property(
                libspa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(volumes.to_vec())),
            ));
        }
        if let Some(params) = params {
            let fields = params
                .into_iter()
                .flat_map(|(key, value)| [Value::String(key.to_owned()), value])
                .collect();
            properties.push(property(
                libspa::sys::SPA_PROP_params,
                Value::Struct(fields),
            ));
        }
        libspa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(Object {
                type_: libspa::sys::SPA_TYPE_OBJECT_Props,
                id: libspa::param::ParamType::Props.as_raw(),
                properties,
            }),
        )
        .expect("a Props pod")
        .0
        .into_inner()
    }

    /// `take_props` as NODE 1's `param_changed` calls it, with `bytes` as the write.
    fn written(volume: &LaneVolume, bytes: &[u8]) {
        take_props(
            volume,
            libspa::param::ParamType::Props.as_raw(),
            Some(Pod::from_bytes(bytes).expect("a pod")),
        );
    }

    #[test]
    fn a_write_of_another_adapter_setting_leaves_the_volume_after_the_chain() {
        use libspa::pod::Value;

        let volume = LaneVolume::new();
        volume.begin_pair(2, &at(&[0.2, 0.2]));
        let before = volume.changes();
        // `pw-cli set-param fxsound_sink Props '{ params = [ "channelmix.normalize" true ] }'`
        let normalize = props_write(
            None,
            Some(vec![("channelmix.normalize", Value::Bool(true))]),
        );
        assert_eq!(
            Pod::from_bytes(&normalize)
                .and_then(PropsUpdate::from_pod)
                .and_then(|update| update.clamped),
            Some(false),
            "read on its own, a write that lists neither bound looks like an adapter that does \
             not clamp"
        );
        written(&volume, &normalize);
        assert!(
            volume.post_dsp(),
            "the adapter still keeps the volume at unity, so the lane must go on applying it"
        );
        assert_eq!(volume.gains()[..2], [0.2, 0.2]);
        assert_eq!(volume.changes(), before, "no volume was set");
    }

    #[test]
    fn a_slider_write_that_also_carries_params_sets_the_volume_and_nothing_else() {
        use libspa::pod::Value;

        let volume = LaneVolume::new();
        volume.begin_pair(2, &at(&[1.0, 1.0]));
        written(
            &volume,
            &props_write(
                Some(&[0.25, 0.25]),
                Some(vec![("monitor.channel-volumes", Value::Bool(false))]),
            ),
        );
        assert!(volume.post_dsp());
        assert_eq!(volume.gains()[..2], [0.25, 0.25]);
    }

    #[test]
    fn a_write_that_moves_one_volume_bound_leaves_the_clamp_to_the_nodes_own_props() {
        use libspa::pod::Value;

        let volume = LaneVolume::new();
        volume.begin_pair(2, &at(&[0.5, 0.5]));
        // Moving one bound off unity does make the adapter apply the volume, and the node's whole
        // `Props` say so a round trip later. The write cannot: it does not say where the other
        // bound is — and a write of the upper bound alone, at unity, would read the same way.
        for write in [
            props_write(
                None,
                Some(vec![(volume::MIN_VOLUME_KEY, Value::Float(0.0))]),
            ),
            props_write(
                None,
                Some(vec![(volume::MAX_VOLUME_KEY, Value::Float(1.0))]),
            ),
        ] {
            written(&volume, &write);
            assert!(volume.post_dsp());
            assert_eq!(volume.gains()[..2], [0.5, 0.5]);
        }
    }

    #[test]
    fn the_first_node_a_pair_adopts_keeps_whether_its_volume_needed_writing() {
        assert!(
            still_published(true, None, 41),
            "a pair at unity has nothing to tell a node PipeWire has just made"
        );
        assert!(!still_published(false, None, 41));
    }

    #[test]
    fn a_node_adopted_in_place_of_a_dead_one_is_written_to_again() {
        assert!(
            !still_published(true, Some(40), 41),
            "the write went to the node of a pair that had gone; the new node would show unity \
             while the lane plays lower, and a volume key would raise it from there"
        );
        assert!(!still_published(false, Some(40), 41));
    }

    #[test]
    fn the_same_node_announced_again_is_not_written_to_twice() {
        assert!(still_published(true, Some(41), 41));
        assert!(!still_published(false, Some(41), 41));
    }

    #[test]
    fn the_virtual_nodes_are_told_apart_from_every_other_node_by_name() {
        assert_eq!(
            virtual_node_direction(Some(SINK_NODE_NAME)),
            Some(DeviceDirection::Output)
        );
        assert_eq!(
            virtual_node_direction(Some(SOURCE_NODE_NAME)),
            Some(DeviceDirection::Input)
        );
        for other in [
            OUTPUT_NODE_NAME,
            CAPTURE_NODE_NAME,
            AEC_SOURCE_NODE_NAME,
            "alsa_output.pci",
        ] {
            assert_eq!(virtual_node_direction(Some(other)), None, "{other}");
        }
        assert_eq!(virtual_node_direction(None), None);
    }

    #[test]
    fn both_device_facing_streams_are_linked_once_never_moved_and_kept_while_away() {
        pw::init();
        for (direction, target) in [
            (DeviceDirection::Output, "bluez_output.00_11_22_33_44_55.1"),
            (DeviceDirection::Input, "bluez_input.00_11_22_33_44_55.0"),
        ] {
            for passive in [true, false] {
                let props = stream_props(direction, target, "512/48000", passive);
                let key = direction.key();
                assert_eq!(
                    props.get("node.dont-reconnect"),
                    Some("true"),
                    "{key}: WirePlumber must not move the stream onto the fallback device"
                );
                assert_eq!(
                    props.get("node.dont-fallback"),
                    Some("true"),
                    "{key}: a target that is away is waited for, not replaced by the default"
                );
                assert_eq!(
                    props.get("node.linger"),
                    Some("true"),
                    "{key}: a stream waiting for its target is kept, not destroyed"
                );
                assert_eq!(
                    props.get("target.object"),
                    Some(target),
                    "{key}: the stream still names its target"
                );
                assert_eq!(props.get("node.autoconnect"), Some("true"));
            }
        }
        // The virtual nodes are linked *to* by applications and carry none of it.
        let positions = ChannelMap::default_for(2);
        for direction in DeviceDirection::ALL {
            let virtual_node =
                virtual_node_props(direction, None, 2, 48_000, &positions, "512/48000");
            for key in ["node.dont-reconnect", "node.dont-fallback", "node.linger"] {
                assert_eq!(virtual_node.get(key), None, "{}: {key}", direction.key());
            }
        }
    }

    #[test]
    fn a_missing_runtime_directory_is_rejected_before_any_thread_is_spawned() {
        assert!(preflight(None).is_err());
        assert!(preflight(Some(OsStr::new("/tmp"))).is_ok());
    }

    // ---- the exit hand-back ------------------------------------------------------------------

    /// With nothing held there is nothing to write, so the exit must not wait for a confirmation
    /// that can never come. A default we believe we hold but have no connection to hand back on is
    /// reported and forgotten, not written.
    #[test]
    fn with_nothing_held_there_is_nothing_to_hand_back_or_wait_for() {
        let mut shared = shared_for_tests();
        assert!(!release_all_defaults(&mut shared));

        shared.defaults.output.holding = true;
        shared.defaults.input.holding = true;
        assert!(!release_all_defaults(&mut shared));
        assert!(!shared.defaults.output.holding);
        assert!(!shared.defaults.input.holding);
    }

    /// `done` events carry their `sync`'s sequence number back. The registry barrier of `connect`
    /// uses `0`; only the release sync may confirm the hand-back, and only once.
    #[test]
    fn only_the_release_sync_confirms_the_exit_hand_back() {
        assert_ne!(RELEASE_SEQ, 0, "must not collide with the registry sync");
        let mut shared = shared_for_tests();
        assert!(!confirm_release(
            &mut shared,
            AsyncSeq::from_seq(RELEASE_SEQ)
        ));

        shared.release_pending = Some(AsyncSeq::from_seq(RELEASE_SEQ));
        assert!(!confirm_release(&mut shared, AsyncSeq::from_seq(0)));
        assert!(
            shared.release_pending.is_some(),
            "the registry sync is not ours"
        );
        assert!(confirm_release(
            &mut shared,
            AsyncSeq::from_seq(RELEASE_SEQ)
        ));
        assert!(shared.release_pending.is_none());
        assert!(!confirm_release(
            &mut shared,
            AsyncSeq::from_seq(RELEASE_SEQ)
        ));
    }

    /// Once the registry's `done` has sent the second `sync`, only that `sync`'s `done` lets the
    /// rules run, and only once: a stray `done` neither lets them run early nor asks every lane
    /// for its rules again once they have.
    #[test]
    fn only_the_sync_behind_the_metadata_binds_lets_the_rules_run() {
        assert_ne!(METADATA_SEQ, 0, "must not collide with the registry sync");
        assert_ne!(
            METADATA_SEQ, RELEASE_SEQ,
            "must not collide with the release sync"
        );
        let mut shared = shared_for_tests();
        assert_eq!(shared.barrier, Barrier::Registry);

        let behind_the_binds = AsyncSeq::from_seq(METADATA_SEQ);
        shared.barrier = Barrier::Metadata(behind_the_binds);
        for stray in [0, RELEASE_SEQ] {
            assert!(!pass_barrier(&mut shared, AsyncSeq::from_seq(stray)));
            assert_eq!(shared.barrier, Barrier::Metadata(behind_the_binds));
        }
        assert!(pass_barrier(&mut shared, behind_the_binds));
        assert_eq!(shared.barrier, Barrier::Passed);
        assert!(!pass_barrier(&mut shared, behind_the_binds));
        assert!(!pass_barrier(&mut shared, AsyncSeq::from_seq(0)));
        assert_eq!(shared.barrier, Barrier::Passed);
    }

    /// With no connection to send the second `sync` on, the registry's `done` is the last one:
    /// rules that might run before the defaults are read are better than rules that never run.
    #[test]
    fn a_barrier_with_no_connection_to_wait_on_does_not_hold_the_rules_for_ever() {
        let mut shared = shared_for_tests();
        assert!(pass_barrier(&mut shared, AsyncSeq::from_seq(0)));
        assert_eq!(shared.barrier, Barrier::Passed);
    }

    /// The pump returns as soon as there is nothing pending, and a server that never answers
    /// cannot hold the exit past the deadline. The loop here is a bare epoll loop with no
    /// connection behind it — the "server" is simply absent.
    #[test]
    fn the_exit_hand_back_waits_for_the_confirmation_but_never_past_the_deadline() {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).expect("a main loop needs no server");
        let shared = Rc::new(RefCell::new(shared_for_tests()));

        let started = Instant::now();
        assert!(pump_until_release_confirmed(
            &shared,
            mainloop.loop_(),
            started + Duration::from_secs(5),
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "nothing pending: no waiting"
        );

        shared.borrow_mut().release_pending = Some(AsyncSeq::from_seq(RELEASE_SEQ));
        let deadline = Instant::now() + Duration::from_millis(30);
        assert!(!pump_until_release_confirmed(
            &shared,
            mainloop.loop_(),
            deadline
        ));
        assert!(
            Instant::now() >= deadline,
            "gives the server the whole window"
        );
        assert!(
            deadline.elapsed() < Duration::from_secs(2),
            "but not much more"
        );
        assert!(
            shared.borrow().release_pending.is_some(),
            "unanswered stays unanswered"
        );

        assert!(
            RELEASE_TIMEOUT <= Duration::from_secs(1),
            "a dead server must not stall exit"
        );
    }

    // ---- §4 of the 0.4.0 design: a voice preset names its chain, and the name reaches the engine

    /// Between pairs of nodes the engine is on the main loop and is rebuilt there, in place: the
    /// only place a rebuild is allowed, and no hand-over is needed.
    #[test]
    fn a_preset_naming_the_podcast_chain_rebuilds_the_voice_engine_between_pairs() {
        let mut shared = shared_with_dsp_for_tests();
        assert_eq!(shared.input_spec, ChainSpec::voice());

        set_input_chain(&mut shared, "podcast");

        let dsp = shared
            .lanes
            .input
            .dsp
            .as_mut()
            .and_then(LaneDsp::as_input_mut)
            .expect("the engine is on the main loop");
        assert_eq!(dsp.engine().spec(), ChainSpec::podcast());
        assert!(
            dsp.engine().chain().gate().is_none(),
            "the podcast ordering has no gate"
        );
        assert_eq!(shared.input_spec, ChainSpec::podcast());
        assert!(
            !shared.input_spec_pending,
            "nothing is owed: the rebuild happened here"
        );
        // The same name again changes nothing and owes nothing.
        set_input_chain(&mut shared, "podcast");
        assert!(!shared.input_spec_pending);
    }

    #[test]
    fn a_chain_name_this_build_does_not_know_runs_the_voice_chain() {
        let mut shared = shared_with_dsp_for_tests();
        set_input_chain(&mut shared, "podcast");
        set_input_chain(&mut shared, "karaoke");
        assert_eq!(shared.input_spec, ChainSpec::voice());
        let dsp = shared
            .lanes
            .input
            .dsp
            .as_mut()
            .and_then(LaneDsp::as_input_mut)
            .expect("the engine is on the main loop");
        assert_eq!(dsp.engine().spec(), ChainSpec::voice());
        assert!(
            dsp.engine().chain().gate().is_some(),
            "the voice chain gates"
        );
    }

    /// The engine is away with a pair of nodes when the preset changes, and comes back through
    /// the recycle channel when that pair goes: the chain is owed until then and settled in place
    /// on the tick that finds the engine here, which is what `build_nodes` relies on too.
    #[test]
    fn a_chain_named_while_the_engine_is_away_is_owed_until_it_comes_back() {
        let mut shared = shared_for_tests();
        set_input_chain(&mut shared, "streaming");
        assert!(shared.input_spec_pending);
        assert_eq!(shared.input_spec, ChainSpec::streaming());

        let (dsp, _handover) = lanes_for_tests();
        shared
            .lanes
            .input
            .recycle
            .0
            .send(dsp.input)
            .expect("the main loop is listening");
        drain_recycled_dsp(&mut shared);
        reconcile_input_chain(&mut shared);

        assert!(!shared.input_spec_pending);
        assert_eq!(
            input_spec_on_the_main_loop(&mut shared),
            Some(ChainSpec::streaming()),
            "recovered, and rebuilt in place"
        );
    }

    /// The two lanes' DSP are apart now, so a voice chain named while the *output* pair runs no
    /// longer has to wait for that pair to come down: the voice engine is on the main loop, and
    /// is rebuilt there at once.
    #[test]
    fn a_voice_chain_named_while_the_music_lane_is_away_is_rebuilt_in_place_at_once() {
        let mut shared = shared_with_dsp_for_tests();
        let music = shared.lanes.output.dsp.take().expect("on the main loop");

        set_input_chain(&mut shared, "podcast");

        assert!(!shared.input_spec_pending, "nothing owed");
        assert_eq!(
            input_spec_on_the_main_loop(&mut shared),
            Some(ChainSpec::podcast())
        );
        drop(music);
    }

    // ---- §1.3 of the 0.4.0 design: each lane's DSP is its own

    /// A NODE 1 hands its DSP back when its pair goes, and the DSP must land in the slot of the
    /// lane it was taken from — a voice chain in the music lane's slot would be run on the next
    /// output pair. Both lanes away at once, brought back one at a time, in both orders.
    #[test]
    fn the_recycled_dsp_returns_to_its_own_lane() {
        for first in DeviceDirection::ALL {
            let mut shared = shared_with_dsp_for_tests();
            let node_ones = PerDirection::from_fn(|direction| {
                let dsp = shared
                    .lanes
                    .get_mut(direction)
                    .dsp
                    .take()
                    .expect("on the main loop");
                SinkData::new(&shared, dsp)
            });
            assert!(shared.lanes.output.dsp.is_none() && shared.lanes.input.dsp.is_none());

            let PerDirection { output, input } = node_ones;
            let (first_node, second_node) = match first {
                DeviceDirection::Output => (output, input),
                DeviceDirection::Input => (input, output),
            };
            drop(first_node);
            drain_recycled_dsp(&mut shared);
            assert_eq!(
                shared.lanes.get(first).dsp.as_ref().map(LaneDsp::direction),
                Some(first),
                "the {} lane's DSP came home",
                first.key()
            );
            assert!(
                shared.lanes.get(first.other()).dsp.is_none(),
                "…and nothing arrived in the {} lane's slot, whose DSP is still away",
                first.other().key()
            );

            drop(second_node);
            drain_recycled_dsp(&mut shared);
            for (direction, lane) in shared.lanes.iter() {
                assert_eq!(lane.dsp.as_ref().map(LaneDsp::direction), Some(direction));
            }
        }
    }

    /// A lane with no nodes has no audio thread reading its event queue. The main loop applies
    /// what arrives instead, so a GUI that keeps resetting both chains — every power toggle does —
    /// never fills the idle lane's queue, and a lane whose DSP is away with a pair is left for its
    /// own audio thread.
    #[test]
    fn an_event_for_a_lane_with_no_nodes_is_applied_on_the_main_loop_rather_than_left_to_pile_up() {
        let (handle_events, events) = PerDirection::from_fn(|_| {
            crossbeam_channel::bounded::<DspEvent>(crate::EVENT_QUEUE_LEN)
        })
        .unzip();
        let (_, params) = triple_buffer::TripleBuffer::new(&DspParams::default()).split();
        let (_, input_params) =
            triple_buffer::TripleBuffer::new(&InputDspParams::default()).split();
        let meters = PerDirection::from_fn(|_| {
            triple_buffer::TripleBuffer::new(&Meters::default())
                .split()
                .0
        });
        let (dsp, handover) = lane_dsp::build(params, input_params, meters, events);
        let (notify, _) = crossbeam_channel::unbounded();
        let mut shared = Shared::new(
            notify,
            None,
            None,
            PerDirection {
                output: None,
                input: Some(dsp.input),
            },
            handover,
        );

        for _ in 0..crate::EVENT_QUEUE_LEN {
            handle_events
                .input
                .try_send(DspEvent::ResetFilterState)
                .expect("room in the voice lane's queue");
        }
        handle_events
            .output
            .try_send(DspEvent::ResetSpectrum)
            .expect("room in the music lane's queue");
        apply_idle_lane_events(&mut shared);

        assert!(
            handle_events.input.is_empty(),
            "the idle voice lane's events were applied on the main loop"
        );
        assert_eq!(
            handle_events.output.len(),
            1,
            "the music lane's DSP is away with a pair; its event waits for that pair's thread"
        );
        drop(dsp.output);
    }

    // ---- §1 of the 0.4.0 design: two lanes, side by side -------------------------------------

    /// A thread that has not connected, with the receiving end of its notifications kept, so a
    /// test can see exactly what the GUI would have been told.
    fn shared_with_messages() -> (Shared, Receiver<AudioToUi>) {
        let (notify, messages) = crossbeam_channel::unbounded();
        let shared = Shared::new(
            notify,
            None,
            None,
            PerDirection::default(),
            ChainHandover::new().0,
        );
        (shared, messages)
    }

    fn drained(messages: &Receiver<AudioToUi>) -> Vec<AudioToUi> {
        messages.try_iter().collect()
    }

    /// A device as the registry would report it: a stereo sink, or a microphone.
    fn device(object_id: u32, name: &str, direction: DeviceDirection) -> DeviceInfo {
        let media_class = match direction {
            DeviceDirection::Output => devices::SINK_MEDIA_CLASS,
            DeviceDirection::Input => devices::SOURCE_MEDIA_CLASS,
        };
        DeviceInfo::from_props(object_id, &|key: &str| match key {
            "media.class" => Some(media_class),
            "node.name" => Some(name),
            "audio.channels" => Some("2"),
            _ => None,
        })
        .expect("a sink or a source that is not one of ours")
    }

    /// Everything the main loop decides about a lane, in a form two lanes can be compared in.
    #[derive(Debug, PartialEq)]
    struct LaneState {
        enabled: bool,
        needs_rules: bool,
        previous_names: Vec<String>,
        want_default: bool,
        attempts: u32,
        next_attempt: Instant,
        built_at: Option<Instant>,
        last_error: Option<AudioError>,
        attached: Option<String>,
        last_status: AudioStatus,
    }

    fn lane_state(shared: &Shared, direction: DeviceDirection) -> LaneState {
        let lane = shared.lanes.get(direction);
        LaneState {
            enabled: lane.enabled,
            needs_rules: lane.needs_rules,
            previous_names: lane.previous_names.clone(),
            want_default: lane.want_default,
            attempts: lane.attempts,
            next_attempt: lane.next_attempt,
            built_at: lane.built_at,
            last_error: lane.last_error.clone(),
            attached: lane.attached.clone(),
            last_status: lane.last_status,
        }
    }

    /// An output lane that has been running a while: attached, holding the default sink, with a
    /// failure behind it and an opinion about the default.
    fn give_the_output_lane_a_history(shared: &mut Shared) {
        let lane = &mut shared.lanes.output;
        lane.previous_names = vec!["alsa_output.pci".to_owned()];
        lane.want_default = false;
        lane.attempts = 2;
        lane.next_attempt = Instant::now() + Duration::from_secs(3);
        lane.built_at = Some(Instant::now());
        lane.last_error = Some(AudioError::DeviceUnavailable);
        lane.attached = Some("alsa_output.pci".to_owned());
        lane.last_status = AudioStatus {
            sample_rate: 44_100,
            ..AudioStatus::default()
        };
        shared.memory.output.user_selected = "alsa_output.pci".to_owned();
        shared.defaults.output.holding = true;
    }

    #[test]
    fn the_output_lane_starts_enabled_and_the_input_lane_waits_for_a_microphone() {
        let shared = shared_for_tests();
        assert!(
            shared.lanes.output.enabled,
            "FxSound in front of the speakers is the Windows behaviour"
        );
        assert!(
            !shared.lanes.input.enabled,
            "a microphone is only processed once the user picks one"
        );
        for (direction, lane) in shared.lanes.iter() {
            assert!(lane.nodes.is_none(), "{} lane", direction.key());
            assert!(lane.want_default, "{} lane", direction.key());
        }
        assert!(
            !Arc::ptr_eq(&shared.lanes.output.ring, &shared.lanes.input.ring),
            "each lane's callbacks meet in a ring of their own"
        );
        assert!(!Arc::ptr_eq(
            &shared.lanes.output.counters,
            &shared.lanes.input.counters
        ));
    }

    #[test]
    fn selecting_a_microphone_leaves_the_output_lanes_state_untouched() {
        let (mut shared, messages) = shared_with_messages();
        give_the_output_lane_a_history(&mut shared);
        let output_before = lane_state(&shared, DeviceDirection::Output);
        let memory_before = shared.memory.output.clone();

        control(
            &mut shared,
            UiToAudio::SelectDevice {
                node_name: "alsa_input.usb-fifine".to_owned(),
                direction: DeviceDirection::Input,
            },
        );

        assert_eq!(
            lane_state(&shared, DeviceDirection::Output),
            output_before,
            "picking a microphone changed something about the speakers"
        );
        assert_eq!(shared.memory.output, memory_before);
        assert!(
            shared.defaults.output.holding,
            "the default sink is still ours"
        );

        let input = &shared.lanes.input;
        assert!(input.enabled, "picking a microphone enables its lane");
        assert!(
            input.needs_rules,
            "with no graph yet, the rules run as soon as there is one"
        );
        assert_eq!(shared.memory.input.user_selected, "alsa_input.usb-fifine");
        assert_eq!(
            drained(&messages),
            [],
            "nothing to tell the GUI until the lane is attached"
        );
    }

    #[test]
    fn selecting_a_device_is_not_a_retry_and_starts_the_lanes_backoff_afresh() {
        let mut shared = shared_for_tests();
        shared.lanes.input.enabled = true;
        shared.lanes.input.attempts = 5;
        shared.lanes.input.next_attempt = Instant::now() + Duration::from_secs(5);
        shared.lanes.input.last_error = Some(AudioError::DeviceUnavailable);
        control(
            &mut shared,
            UiToAudio::SelectDevice {
                node_name: "alsa_input.usb-fifine".to_owned(),
                direction: DeviceDirection::Input,
            },
        );
        assert_eq!(shared.lanes.input.attempts, 0);
        assert!(shared.lanes.input.next_attempt <= Instant::now());
        assert_eq!(
            shared.lanes.input.last_error, None,
            "if the new choice fails too, the user who made it hears about it"
        );
    }

    #[test]
    fn detaching_a_lane_hands_back_only_its_own_default_and_says_it_is_attached_to_nothing() {
        let (mut shared, messages) = shared_with_messages();
        give_the_output_lane_a_history(&mut shared);
        let output_before = lane_state(&shared, DeviceDirection::Output);
        shared.lanes.input.enabled = true;
        shared.lanes.input.attached = Some("alsa_input.usb-fifine".to_owned());
        shared.lanes.input.needs_rules = true;
        shared.lanes.input.previous_names = vec!["alsa_input.usb-fifine".to_owned()];
        shared.defaults.input.holding = true;

        control(&mut shared, UiToAudio::DetachLane(DeviceDirection::Input));

        let input = &shared.lanes.input;
        assert!(!input.enabled);
        assert!(!input.needs_rules, "a detached lane has no rules to run");
        assert!(input.nodes.is_none());
        assert!(
            !shared.defaults.input.holding,
            "the default source was handed back"
        );
        assert!(
            shared.defaults.output.holding,
            "the default sink is none of the input lane's business"
        );
        assert_eq!(lane_state(&shared, DeviceDirection::Output), output_before);
        assert_eq!(
            drained(&messages),
            [AudioToUi::Attached {
                direction: DeviceDirection::Input,
                node_name: None,
            }]
        );

        // Asked again, it answers again: the GUI that sent it is told, not left to infer it.
        control(&mut shared, UiToAudio::DetachLane(DeviceDirection::Input));
        assert_eq!(
            drained(&messages),
            [AudioToUi::Attached {
                direction: DeviceDirection::Input,
                node_name: None,
            }]
        );
    }

    // ---- §7 of the 0.4.0 design: echo cancellation -----------------------------------------

    #[test]
    fn switching_echo_cancellation_is_always_answered_even_with_nothing_to_cancel_yet() {
        let (mut shared, messages) = shared_with_messages();
        let not_running = AudioToUi::EchoCancel {
            running: false,
            detail: String::new(),
        };

        // On, with no microphone lane: nothing to cancel for, and nothing wrong. The canceller is
        // loaded once there is a microphone pair, by the supervisor — never from here.
        control(&mut shared, UiToAudio::SetEchoCancel(true));
        assert_eq!(drained(&messages), std::slice::from_ref(&not_running));
        // Asked again, it answers again: whoever sent it is waiting to hear.
        control(&mut shared, UiToAudio::SetEchoCancel(true));
        assert_eq!(drained(&messages), std::slice::from_ref(&not_running));
        publish(&mut shared);
        assert!(
            !drained(&messages)
                .iter()
                .any(|m| matches!(m, AudioToUi::EchoCancel { .. })),
            "what was answered is not news on the next tick"
        );

        control(&mut shared, UiToAudio::SetEchoCancel(false));
        assert_eq!(drained(&messages), [not_running]);
    }

    #[test]
    fn an_engine_never_asked_for_echo_cancellation_never_mentions_it() {
        let (mut shared, messages) = shared_with_messages();
        control(
            &mut shared,
            UiToAudio::SelectDevice {
                node_name: "alsa_input.usb-fifine".to_owned(),
                direction: DeviceDirection::Input,
            },
        );
        publish(&mut shared);
        assert!(
            !drained(&messages)
                .iter()
                .any(|m| matches!(m, AudioToUi::EchoCancel { .. })),
            "off is what the GUI assumes until it asks"
        );
    }

    #[test]
    fn the_canceller_sees_each_lane_as_off_between_pairs_or_on_a_device() {
        let mut shared = shared_for_tests();
        assert_eq!(canceller_side(&shared.lanes.input), Side::Off);
        assert_eq!(
            canceller_side(&shared.lanes.output),
            Side::Between,
            "enabled, with no pair yet"
        );
        shared.lanes.input.enabled = true;
        assert_eq!(canceller_side(&shared.lanes.input), Side::Between);
        shared.lanes.output.enabled = false;
        assert_eq!(canceller_side(&shared.lanes.output), Side::Off);
    }

    #[test]
    fn with_no_canceller_running_the_microphone_is_recorded_directly() {
        let mut shared = shared_for_tests();
        shared.aec.set_on(true);
        // On, but nothing loaded and no source in the graph: the capture stream is aimed at the
        // microphone, as it is with echo cancellation off.
        assert_eq!(shared.aec.route("alsa_input.usb-fifine"), None);
        shared.aec.source_appeared(42);
        assert_eq!(
            shared.aec.route("alsa_input.usb-fifine"),
            None,
            "a source with no module of ours behind it is not recorded from"
        );
        assert!(
            !shared.aec.route_moved(),
            "and nothing has moved for the input lane's rules to follow"
        );
    }

    #[test]
    fn a_source_appearing_asks_only_the_input_lane_for_its_rules() {
        let mut shared = shared_for_tests();
        shared.lanes.input.enabled = true;

        add_device(
            &mut shared,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        assert!(shared.lanes.input.needs_rules);
        assert!(
            !shared.lanes.output.needs_rules,
            "a new microphone has nothing to say to the speakers' lane"
        );

        shared.lanes.input.needs_rules = false;
        add_device(
            &mut shared,
            device(41, "alsa_output.pci", DeviceDirection::Output),
        );
        assert!(shared.lanes.output.needs_rules);
        assert!(!shared.lanes.input.needs_rules);

        shared.lanes.output.needs_rules = false;
        let removed = remove_device(&mut shared, 40).expect("a device with that id");
        assert_eq!(removed.name, "alsa_input.usb-fifine");
        assert!(shared.lanes.input.needs_rules, "the target may have gone");
        assert!(!shared.lanes.output.needs_rules);
        assert!(remove_device(&mut shared, 40).is_none());
    }

    #[test]
    fn a_microphone_plugged_in_while_the_input_lane_is_detached_does_not_wake_it() {
        let mut shared = shared_for_tests();
        add_device(
            &mut shared,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        assert!(!shared.lanes.input.needs_rules);
        assert!(!shared.lanes.input.enabled);

        control(&mut shared, UiToAudio::RescanDevices);
        assert!(
            shared.lanes.output.needs_rules,
            "a rescan asks the running lane"
        );
        assert!(!shared.lanes.input.needs_rules, "…and not the detached one");
    }

    /// The headset's card, as the registry reports it once its info has arrived.
    const HEADSET_CARD: u32 = 60;
    const HEADSET_ADDRESS: &str = "00:11:22:33:44:55";
    const HEADSET_SINK: &str = "bluez_output.00_11_22_33_44_55.1";
    const SPEAKERS: &str = "alsa_output.pci-0000_00_1f.3.analog-stereo";

    fn headset_card() -> Card {
        Card {
            object_id: HEADSET_CARD,
            bluez_address: Some(HEADSET_ADDRESS.to_owned()),
            bluetooth: true,
        }
    }

    /// The headset's sink under registry id `object_id` and serial `serial`, on the headset's card.
    fn headset_sink(object_id: u32, serial: u64) -> DeviceInfo {
        DeviceInfo {
            object_serial: Some(serial),
            card_id: Some(HEADSET_CARD),
            bluez_address: Some(HEADSET_ADDRESS.to_owned()),
            ..device(object_id, HEADSET_SINK, DeviceDirection::Output)
        }
    }

    /// An output lane on the headset — as the rules left it between pairs, which is all a test
    /// without a server can build — beside the laptop's speakers, with the headset's card listed.
    fn output_lane_on_the_headset() -> (Shared, Receiver<AudioToUi>) {
        let (mut shared, messages) = shared_with_messages();
        shared.cards.push(headset_card());
        add_device(&mut shared, headset_sink(70, 70));
        add_device(&mut shared, device(57, SPEAKERS, DeviceDirection::Output));
        shared.lanes.output.last_target = Some(HEADSET_SINK.to_owned());
        shared.lanes.output.needs_rules = false;
        (shared, messages)
    }

    #[test]
    fn a_node_back_under_its_name_with_a_new_serial_is_not_the_node_a_pair_was_built_on() {
        let back = headset_sink(74, 74);
        assert!(is_same_node(HEADSET_SINK, Some(74), &back));
        assert!(
            !is_same_node(HEADSET_SINK, Some(70), &back),
            "the same name on a new node: WirePlumber will not link the old stream to it"
        );
        assert!(!is_same_node(SPEAKERS, Some(74), &back), "another device");
        assert!(
            !is_same_node(HEADSET_SINK, None, &back),
            "a pair built without a serial on a device that has one now is not known to be on it"
        );
        let unnumbered = DeviceInfo {
            object_serial: None,
            ..back
        };
        assert!(
            is_same_node(HEADSET_SINK, None, &unnumbered),
            "with no serial on either side the name decides, as it did before"
        );
    }

    #[test]
    fn a_target_that_goes_while_its_card_stays_is_waited_for() {
        let now = Instant::now();
        let hold = Hold::after_removal(
            &headset_sink(70, 70),
            Some(HEADSET_SINK),
            &[headset_card()],
            &[],
            &[],
            None,
            now,
        );
        assert_eq!(
            hold,
            Some(Hold {
                target: HEADSET_SINK.to_owned(),
                until: now + RETURN_WAIT,
                card_id: Some(HEADSET_CARD),
                bluez_address: Some(HEADSET_ADDRESS.to_owned()),
                known: Vec::new(),
            }),
            "the wait keeps what tied the node to its card, for when the card goes too"
        );
        assert!(
            RETURN_WAIT >= Duration::from_secs(2) && RETURN_WAIT <= Duration::from_secs(3),
            "long enough for a profile switch, short enough not to be mistaken for a hang"
        );
    }

    #[test]
    fn a_target_that_goes_with_its_card_or_has_no_card_is_not_waited_for() {
        let now = Instant::now();
        assert_eq!(
            Hold::after_removal(
                &headset_sink(70, 70),
                Some(HEADSET_SINK),
                &[],
                &[],
                &[],
                None,
                now
            ),
            None,
            "the headset went with its sink: it has gone, not changed profile"
        );
        let virtual_sink = device(90, "easyeffects_sink", DeviceDirection::Output);
        assert_eq!(
            Hold::after_removal(
                &virtual_sink,
                Some("easyeffects_sink"),
                &[headset_card()],
                &[],
                &[],
                None,
                now
            ),
            None,
            "a node on no card has nothing to come back with"
        );
    }

    #[test]
    fn a_bluetooth_target_is_waited_for_when_only_its_address_ties_it_to_its_card() {
        let now = Instant::now();
        let unnumbered = DeviceInfo {
            card_id: None,
            ..headset_sink(70, 70)
        };
        assert!(
            Hold::after_removal(
                &unnumbered,
                Some(HEADSET_SINK),
                &[headset_card()],
                &[],
                &[],
                None,
                now
            )
            .is_some()
        );
    }

    #[test]
    fn only_the_node_a_lane_is_on_is_waited_for() {
        let now = Instant::now();
        let cards = [headset_card()];
        assert_eq!(
            Hold::after_removal(
                &headset_sink(70, 70),
                Some(SPEAKERS),
                &cards,
                &[],
                &[],
                None,
                now
            ),
            None,
            "the headset going is news for the rules of a lane on the speakers"
        );
        assert_eq!(
            Hold::after_removal(&headset_sink(70, 70), None, &cards, &[], &[], None, now),
            None,
            "a lane on nothing waits for nothing"
        );
    }

    #[test]
    fn a_node_that_goes_again_before_the_rules_saw_it_back_keeps_the_first_deadline() {
        let first = Instant::now();
        let cards = [headset_card()];
        let running = Hold::after_removal(
            &headset_sink(70, 70),
            Some(HEADSET_SINK),
            &cards,
            &[],
            &[],
            None,
            first,
        )
        .expect("waited for");
        let again = Hold::after_removal(
            &headset_sink(74, 74),
            Some(HEADSET_SINK),
            &cards,
            &[],
            &[],
            Some(&running),
            first + Duration::from_secs(1),
        );
        assert_eq!(
            again.as_ref(),
            Some(&running),
            "a node that blinks faster than the rules run cannot keep the lane waiting for ever"
        );

        let later = first + RETURN_WAIT + Duration::from_millis(1);
        let afresh = Hold::after_removal(
            &headset_sink(74, 74),
            Some(HEADSET_SINK),
            &cards,
            &[],
            &[],
            Some(&running),
            later,
        );
        assert_eq!(
            afresh.map(|hold| hold.until),
            Some(later + RETURN_WAIT),
            "a hold whose time was up is no deadline to keep: the next departure waits afresh"
        );
    }

    #[test]
    fn a_hold_ends_when_its_node_is_back_or_its_time_is_up() {
        let now = Instant::now();
        let hold = Hold {
            target: HEADSET_SINK.to_owned(),
            until: now + RETURN_WAIT,
            card_id: Some(HEADSET_CARD),
            bluez_address: None,
            known: Vec::new(),
        };
        assert!(hold.waits(now, false));
        assert!(hold.waits(now + RETURN_WAIT - Duration::from_millis(1), false));
        assert!(
            !hold.waits(now, true),
            "back: the rules attach to it at once"
        );
        assert!(!hold.waits(now + RETURN_WAIT, false), "time is up");
    }

    #[test]
    fn a_target_that_goes_with_its_card_still_listed_holds_its_own_lane_and_no_other() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        shared.lanes.input.enabled = true;
        shared.lanes.input.last_target = Some("alsa_input.mic".to_owned());
        add_device(
            &mut shared,
            device(58, "alsa_input.mic", DeviceDirection::Input),
        );

        let removed = remove_device(&mut shared, 70).expect("the headset's sink");
        assert_eq!(removed.name, HEADSET_SINK);
        let hold = shared.lanes.output.hold.clone().expect("the lane waits");
        assert_eq!(hold.target, HEADSET_SINK);
        assert!(
            shared.lanes.output.needs_rules,
            "the rules are still asked for, for when the wait ends"
        );
        assert_eq!(
            shared.lanes.input.hold, None,
            "the microphone's lane is not on it"
        );

        let now = Instant::now();
        assert!(held(&mut shared, DeviceDirection::Output, now));
        assert!(!held(&mut shared, DeviceDirection::Input, now));
        assert!(
            shared.lanes.output.hold.is_some(),
            "a hold that still holds is kept"
        );
        assert!(!held(&mut shared, DeviceDirection::Output, hold.until));
        assert_eq!(
            shared.lanes.output.hold, None,
            "a hold whose time is up is let go of"
        );
    }

    #[test]
    fn a_target_back_under_its_name_ends_the_wait_at_once() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        remove_device(&mut shared, 70);
        assert!(held(&mut shared, DeviceDirection::Output, Instant::now()));

        // Back half a second later, as a new node: a new id and a new serial.
        add_device(&mut shared, headset_sink(74, 74));
        assert!(!held(&mut shared, DeviceDirection::Output, Instant::now()));
        assert_eq!(shared.lanes.output.hold, None);
        assert!(shared.lanes.output.needs_rules);
    }

    #[test]
    fn a_target_that_goes_with_its_card_moves_its_lane_on_the_next_tick() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        shared.cards.clear();
        remove_device(&mut shared, 70);
        assert_eq!(shared.lanes.output.hold, None);
        assert!(!held(&mut shared, DeviceDirection::Output, Instant::now()));
        assert!(shared.lanes.output.needs_rules);
    }

    #[test]
    fn a_card_that_goes_after_its_node_ends_the_wait_for_it() {
        // A headset switched off: its sink is reported gone first, while its card is still
        // listed, and the card a moment later in the same batch.
        let (mut shared, _messages) = output_lane_on_the_headset();
        remove_device(&mut shared, 70);
        assert!(shared.lanes.output.hold.is_some(), "the sink alone went");
        shared.lanes.output.needs_rules = false;

        assert!(remove_card(&mut shared, HEADSET_CARD));
        assert_eq!(
            shared.lanes.output.hold, None,
            "with its card gone the headset is gone, not between profiles"
        );
        assert!(shared.cards.is_empty());
        assert!(
            shared.lanes.output.needs_rules,
            "the rules are asked for again, so the next tick moves the lane"
        );
        assert!(!held(&mut shared, DeviceDirection::Output, Instant::now()));
    }

    #[test]
    fn a_card_that_goes_after_a_node_tied_to_it_by_address_alone_ends_the_wait_too() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        // The same sink reported again, without a `device.id`: it replaces the one listed.
        add_device(
            &mut shared,
            DeviceInfo {
                card_id: None,
                ..headset_sink(70, 70)
            },
        );
        remove_device(&mut shared, 70);
        assert!(shared.lanes.output.hold.is_some());

        assert!(remove_card(&mut shared, HEADSET_CARD));
        assert_eq!(shared.lanes.output.hold, None);
    }

    #[test]
    fn a_card_the_waited_for_node_does_not_belong_to_leaves_the_wait_as_it_was() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        shared.cards.push(Card {
            object_id: 45,
            bluez_address: None,
            bluetooth: false,
        });
        remove_device(&mut shared, 70);
        let hold = shared.lanes.output.hold.clone();
        assert!(hold.is_some());
        shared.lanes.output.needs_rules = false;

        assert!(remove_card(&mut shared, 45), "the speakers' card went");
        assert_eq!(
            shared.lanes.output.hold, hold,
            "the headset's card is still here"
        );
        assert!(!shared.lanes.output.needs_rules);
        assert_eq!(shared.cards, [headset_card()]);
    }

    #[test]
    fn a_card_going_ends_only_the_wait_of_the_lane_on_one_of_its_nodes() {
        // Both lanes wait: the output lane for the headset's sink, the input lane for a USB
        // microphone whose card is changing profile.
        let (mut shared, _messages) = output_lane_on_the_headset();
        shared.cards.push(Card {
            object_id: 80,
            bluez_address: None,
            bluetooth: false,
        });
        shared.lanes.input.enabled = true;
        shared.lanes.input.last_target = Some("alsa_input.usb-fifine".to_owned());
        add_device(
            &mut shared,
            DeviceInfo {
                card_id: Some(80),
                ..device(81, "alsa_input.usb-fifine", DeviceDirection::Input)
            },
        );
        remove_device(&mut shared, 70);
        remove_device(&mut shared, 81);
        let microphones_wait = shared.lanes.input.hold.clone();
        assert!(shared.lanes.output.hold.is_some());
        assert!(microphones_wait.is_some());

        assert!(remove_card(&mut shared, HEADSET_CARD));
        assert_eq!(shared.lanes.output.hold, None);
        assert_eq!(
            shared.lanes.input.hold, microphones_wait,
            "the microphone's card did not go"
        );
    }

    #[test]
    fn a_global_that_is_no_card_is_left_to_the_rest_of_the_registry_handling() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        assert!(
            !remove_card(&mut shared, 70),
            "the headset's sink is a node"
        );
        assert_eq!(shared.cards, [headset_card()]);
        assert_eq!(shared.devices.len(), 2, "and it is still listed");
    }

    #[test]
    fn a_card_that_goes_before_its_node_leaves_nothing_to_wait_for() {
        // The same batch named the other way round.
        let (mut shared, _messages) = output_lane_on_the_headset();
        assert!(remove_card(&mut shared, HEADSET_CARD));
        remove_device(&mut shared, 70);
        assert_eq!(shared.lanes.output.hold, None);
        assert!(shared.lanes.output.needs_rules);
    }

    #[test]
    fn another_node_going_leaves_a_lanes_wait_as_it_was() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        remove_device(&mut shared, 70);
        let hold = shared.lanes.output.hold.clone();
        assert!(hold.is_some());
        remove_device(&mut shared, 57);
        assert_eq!(shared.lanes.output.hold, hold);
    }

    #[test]
    fn a_detached_lane_waits_for_nothing() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        shared.lanes.output.enabled = false;
        remove_device(&mut shared, 70);
        assert_eq!(shared.lanes.output.hold, None);
    }

    #[test]
    fn the_rules_of_a_lane_that_waits_choose_nothing_and_stay_asked_for() {
        let (mut shared, messages) = output_lane_on_the_headset();
        remove_device(&mut shared, 70);
        shared.lanes.output.needs_rules = false;

        apply_rules(&mut shared, DeviceDirection::Output);
        assert!(shared.lanes.output.needs_rules);
        assert!(
            shared.lanes.output.previous_names.is_empty(),
            "the rules did not run"
        );
        assert!(
            drained(&messages).is_empty(),
            "nothing was chosen, so nothing failed to be built"
        );

        // Once the wait is over the same call runs them — and, with no server here, fails to
        // build on what it chose, which is how a test can see that it chose.
        shared.lanes.output.hold = None;
        apply_rules(&mut shared, DeviceDirection::Output);
        assert_eq!(
            shared.lanes.output.previous_names,
            [SPEAKERS, HEADSET_SINK],
            "the headset is still expected back within the wait, and counts as seen"
        );
        assert!(drained(&messages).iter().any(|message| matches!(
            message,
            AudioToUi::Error {
                direction: Some(DeviceDirection::Output),
                ..
            }
        )));
    }

    #[test]
    fn a_device_the_user_picks_or_a_detach_ends_the_wait_for_the_one_that_went() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        remove_device(&mut shared, 70);
        assert!(shared.lanes.output.hold.is_some());
        select_device(&mut shared, DeviceDirection::Output, SPEAKERS.to_owned());
        assert_eq!(shared.lanes.output.hold, None);

        let (mut shared, _messages) = output_lane_on_the_headset();
        remove_device(&mut shared, 70);
        detach_lane(&mut shared, DeviceDirection::Output);
        assert_eq!(shared.lanes.output.hold, None);
    }

    // ---- Sleep (U13) and a microphone held awake (U19) --------------------------------------

    /// The engine's own mute of each lane, `(output, input)`, as its NODE 1 would read it.
    fn silenced(shared: &Shared) -> (bool, bool) {
        (
            shared.lanes.output.system_mute.load(Ordering::Relaxed),
            shared.lanes.input.system_mute.load(Ordering::Relaxed),
        )
    }

    /// Give the engine its sending ends of both lanes' event queues, as the handle does, and hand
    /// the test the chains' ends.
    fn with_lane_queues(shared: &mut Shared) -> PerDirection<Receiver<DspEvent>> {
        let (senders, receivers) =
            PerDirection::from_fn(|_| crossbeam_channel::bounded(crate::EVENT_QUEUE_LEN)).unzip();
        shared.lane_events = PerDirection {
            output: Some(senders.output),
            input: Some(senders.input),
        };
        receivers
    }

    /// The output lane on the headset, the system gone to sleep, and the headset gone with it,
    /// card and all — what a suspend does to a Bluetooth device.
    fn headset_gone_in_the_sleep() -> (Shared, Receiver<AudioToUi>) {
        let (mut shared, messages) = output_lane_on_the_headset();
        control(&mut shared, UiToAudio::SystemSleeping(true));
        remove_device(&mut shared, 70);
        assert!(remove_card(&mut shared, HEADSET_CARD));
        drained(&messages);
        (shared, messages)
    }

    #[test]
    fn going_to_sleep_silences_both_lanes_whether_or_not_they_are_on() {
        let mut shared = shared_for_tests();
        assert_eq!(silenced(&shared), (false, false), "awake, nothing is held");
        assert!(!shared.lanes.input.enabled);

        control(&mut shared, UiToAudio::SystemSleeping(true));
        assert_eq!(silenced(&shared), (true, true));
        let since = shared.asleep_since.expect("asleep");
        control(&mut shared, UiToAudio::SystemSleeping(true));
        assert_eq!(
            shared.asleep_since,
            Some(since),
            "the sleep began with the first word of it"
        );
    }

    #[test]
    fn no_device_is_chosen_while_the_system_sleeps() {
        // Awake, a headset that goes with its card moves the lane at once. Asleep, nothing moves
        // it: the rules are asked, and kept asked for the wake.
        let (mut shared, messages) = headset_gone_in_the_sleep();
        assert_eq!(shared.lanes.output.hold, None, "no card, so no hold");
        apply_rules(&mut shared, DeviceDirection::Output);
        assert!(shared.lanes.output.needs_rules);
        assert!(
            shared.lanes.output.previous_names.is_empty(),
            "the rules did not run"
        );
        assert!(drained(&messages).is_empty(), "and nothing was built");
        assert!(held(
            &mut shared,
            DeviceDirection::Output,
            Instant::now() + RETURN_WAIT * 10
        ));
    }

    #[test]
    fn waking_clears_both_chains_history_through_their_own_queues() {
        let mut shared = shared_for_tests();
        let queues = with_lane_queues(&mut shared);
        control(&mut shared, UiToAudio::SystemSleeping(true));
        assert!(
            queues.output.is_empty() && queues.input.is_empty(),
            "nothing is cleared on the way down"
        );
        control(&mut shared, UiToAudio::SystemSleeping(false));
        for (direction, queue) in queues.iter() {
            assert_eq!(
                queue.try_iter().collect::<Vec<_>>(),
                [DspEvent::ResetFilterState],
                "the {} lane",
                direction.key()
            );
        }
    }

    #[test]
    fn a_chain_on_the_main_loop_has_its_history_cleared_by_the_wake_itself() {
        // Both queues wired as the handle wires them: the engine's senders, the lanes' DSP at the
        // other end. The output lane's DSP is on the main loop, between pairs; the input lane's is
        // away with a pair, on an audio thread this test does not run.
        let (_, params) = triple_buffer::TripleBuffer::new(&DspParams::default()).split();
        let (_, input_params) =
            triple_buffer::TripleBuffer::new(&InputDspParams::default()).split();
        let meters = PerDirection::from_fn(|_| {
            triple_buffer::TripleBuffer::new(&fxsound_core::messages::Meters::default())
                .split()
                .0
        });
        let (senders, receivers) =
            PerDirection::from_fn(|_| crossbeam_channel::bounded(crate::EVENT_QUEUE_LEN)).unzip();
        let (dsp, handover) = lane_dsp::build(params, input_params, meters, receivers);
        let (notify, _) = crossbeam_channel::unbounded();
        let mut shared = Shared::new(
            notify,
            None,
            None,
            PerDirection {
                output: Some(dsp.output),
                input: None,
            },
            handover,
        );
        let _away_with_its_pair = dsp.input;
        shared.lane_events = PerDirection {
            output: Some(senders.output.clone()),
            input: Some(senders.input.clone()),
        };

        control(&mut shared, UiToAudio::SystemSleeping(true));
        control(&mut shared, UiToAudio::SystemSleeping(false));
        assert_eq!(
            senders.output.len(),
            0,
            "applied on the main loop, where the chain is"
        );
        assert_eq!(
            senders.input.len(),
            1,
            "sent, and waiting for the block the audio thread runs next"
        );
    }

    /// A route kept for a stream FxSound does not move after its preset's rule went
    /// (`app_routes::Plan::kept`) runs the preset as it is when a rule names it again — its new
    /// parameters, not the ones it was left with.
    #[test]
    fn a_kept_route_whose_preset_is_named_again_runs_it_as_it_is_now() {
        use crate::app_routes::{Candidate, RouteSlot, Rules};
        use fxsound_core::AppKey;
        use fxsound_core::messages::{AppRoute, DspParams, RouteParams};

        let rule = |gain: f32| AppRoute {
            direction: DeviceDirection::Output,
            app: AppKey {
                name: "Game".to_owned(),
                ..AppKey::default()
            },
            preset: "Gaming".to_owned(),
            params: RouteParams::Output(DspParams {
                master_gain_db: gain,
                ..DspParams::default()
            }),
            chain: String::new(),
        };
        let preset = |gain: f32| {
            Rules::new(vec![rule(gain)])
                .0
                .preset(DeviceDirection::Output, "Gaming")
                .expect("named")
        };
        let stream = |id: u32, name: &str, on_route: Option<RouteSlot>| Candidate {
            id,
            direction: DeviceDirection::Output,
            app: AppKey {
                name: name.to_owned(),
                ..AppKey::default()
            },
            movable: true,
            on_route,
        };
        let slot = RouteSlot::new(DeviceDirection::Output, 1);
        let now = Instant::now();
        let mut shared = shared_for_tests();

        control(&mut shared, UiToAudio::SetAppRoutes(vec![rule(1.0)]));
        let plan = shared
            .routes
            .plan_for_tests(&[stream(40, "Game", None)], now);
        assert_eq!(plan.build, vec![(slot, "Gaming".to_owned())]);
        shared.routes.add_for_tests(slot, preset(1.0));

        // The rule goes while a tester that names the route's node itself plays through it.
        control(&mut shared, UiToAudio::SetAppRoutes(Vec::new()));
        let tester = stream(44, "Tester", Some(slot));
        let plan = shared
            .routes
            .plan_for_tests(std::slice::from_ref(&tester), now);
        assert_eq!(plan.kept, vec![(slot, "Gaming".to_owned())]);
        assert!(plan.teardown.is_empty());

        // Named again, saved in the meantime with other parameters.
        control(&mut shared, UiToAudio::SetAppRoutes(vec![rule(-6.0)]));
        assert_eq!(
            shared.routes.params_for_tests(slot),
            Some(preset(-6.0).params)
        );
        let plan = shared
            .routes
            .plan_for_tests(&[stream(40, "Game", None), tester], now);
        assert!(
            plan.build.is_empty(),
            "the kept route is the preset's route"
        );
        assert_eq!(plan.assigned, vec![(40, slot)]);
    }

    /// A route's preset: the music chain's busy parameters, or the voice chain's defaults.
    fn route_preset(direction: DeviceDirection, name: &str) -> crate::app_routes::RoutePreset {
        use fxsound_core::messages::RouteParams;
        crate::app_routes::RoutePreset {
            direction,
            name: name.to_owned(),
            params: match direction {
                DeviceDirection::Output => {
                    RouteParams::Output(crate::lane_dsp::tests::busy_output_params(false))
                }
                DeviceDirection::Input => RouteParams::Input(InputDspParams::default()),
            },
            chain: String::new(),
        }
    }

    #[test]
    fn waking_clears_every_routes_chain_history_with_its_lanes() {
        use crate::app_routes::RouteSlot;
        use crate::lane_dsp::tests::{busy_output_params, largest_difference, run_block};
        use fxsound_core::messages::RouteParams;

        // Two routes: a game's on the speakers, whose pair has played for a while and has since
        // been dropped, its DSP sent home; and a call's on the microphone, whose DSP is away with
        // its pair, on an audio thread this test does not run.
        let mut shared = shared_for_tests();
        let _lanes = with_lane_queues(&mut shared);
        let game = RouteSlot::new(DeviceDirection::Output, 1);
        let call = RouteSlot::new(DeviceDirection::Input, 1);
        shared
            .routes
            .add_for_tests(game, route_preset(DeviceDirection::Output, "Gaming"));
        shared
            .routes
            .add_for_tests(call, route_preset(DeviceDirection::Input, "Calls"));
        let mut played = shared
            .routes
            .take_dsp_for_tests(game)
            .expect("built on the main loop");
        played.set_format(48_000.0, 2);
        for index in 0..40 {
            run_block(&mut played, index);
        }
        shared.routes.send_dsp_home_for_tests(game, played);
        let mut away = shared
            .routes
            .take_dsp_for_tests(call)
            .expect("built on the main loop");

        control(&mut shared, UiToAudio::SystemSleeping(true));
        assert_eq!(
            shared.routes.events_waiting(),
            [(game, 0), (call, 0)],
            "nothing is cleared on the way down"
        );
        control(&mut shared, UiToAudio::SystemSleeping(false));
        assert_eq!(
            shared.routes.events_waiting(),
            [(game, 0), (call, 1)],
            "the game's reset applied on the main loop, where its chain is; the call's waiting \
             for the block its audio thread runs next"
        );

        // The game's chain starts its next pair where a route built only now would.
        let mut woken = shared
            .routes
            .take_dsp_for_tests(game)
            .expect("home, on the main loop");
        let (mut fresh, _, _, _fresh_events) = lane_dsp::route(
            RouteParams::Output(busy_output_params(false)),
            ChainSpec::voice(),
        );
        fresh.set_format(48_000.0, 2);
        let difference = largest_difference(&run_block(&mut woken, 40), &run_block(&mut fresh, 40));
        assert!(difference < 1e-6, "{difference}");
        shared.routes.send_dsp_home_for_tests(game, woken);

        // Another sleep and wake before the call's pair has run a block: its reset is still
        // waiting, and one is as good as two.
        control(&mut shared, UiToAudio::SystemSleeping(true));
        control(&mut shared, UiToAudio::SystemSleeping(false));
        assert_eq!(shared.routes.events_waiting(), [(game, 0), (call, 1)]);
        away.refresh();
        assert_eq!(
            shared.routes.events_waiting(),
            [(game, 0), (call, 0)],
            "taken on the call's next block"
        );
    }

    #[test]
    fn a_wake_with_no_sleep_before_it_changes_nothing() {
        let mut shared = shared_for_tests();
        let queues = with_lane_queues(&mut shared);
        shared.lanes.output.needs_rules = false;
        control(&mut shared, UiToAudio::SystemSleeping(false));
        assert!(queues.output.is_empty() && queues.input.is_empty());
        assert_eq!(silenced(&shared), (false, false));
        assert!(!shared.lanes.output.needs_rules);
        assert_eq!(shared.lanes.output.wake_mute_until, None);
        assert_eq!(shared.lanes.output.wake_wait, None);
    }

    #[test]
    fn waking_asks_every_lane_that_is_on_for_its_rules_at_once_and_forgets_what_it_waited_for_before()
     {
        let (mut shared, _messages) = output_lane_on_the_headset();
        // A node that went with its card still here: a hold, timed before the sleep.
        remove_device(&mut shared, 70);
        assert!(shared.lanes.output.hold.is_some());
        shared.lanes.output.next_attempt = Instant::now() + Duration::from_secs(5);
        shared.lanes.output.needs_rules = false;
        control(&mut shared, UiToAudio::SystemSleeping(true));

        let woke = Instant::now();
        wake_up(&mut shared, woke);
        let lane = &shared.lanes.output;
        assert_eq!(
            lane.hold, None,
            "timed on a clock that stood still through the sleep"
        );
        assert!(lane.needs_rules);
        assert!(
            lane.next_attempt <= woke,
            "a backoff from before the sleep is not served after it"
        );
        assert_eq!(lane.wake_wait, Some(woke + RETURN_WAIT));
        assert_eq!(lane.wake_mute_until, Some(woke + WAKE_MUTE));
        // The input lane is off: nothing to wait for, and nothing to hold silent.
        assert_eq!(shared.lanes.input.wake_wait, None);
        assert_eq!(shared.lanes.input.wake_mute_until, None);
        assert!(!shared.lanes.input.needs_rules);
        assert_eq!(silenced(&shared), (true, false));
    }

    #[test]
    fn after_a_wake_a_lane_waits_for_its_device_even_with_its_card_gone() {
        let (mut shared, messages) = headset_gone_in_the_sleep();
        let woke = Instant::now();
        wake_up(&mut shared, woke);
        assert!(
            held(&mut shared, DeviceDirection::Output, woke),
            "the headset is given its chance to reconnect"
        );
        apply_rules(&mut shared, DeviceDirection::Output);
        assert!(
            shared.lanes.output.previous_names.is_empty(),
            "nothing is chosen meanwhile"
        );
        assert!(drained(&messages).is_empty());

        // It reconnects under its name, a new node on a new card: the wait is over the moment it
        // is back, and the rules run on it.
        shared.cards.push(headset_card());
        add_device(&mut shared, headset_sink(81, 81));
        assert!(!held(
            &mut shared,
            DeviceDirection::Output,
            woke + Duration::from_millis(1500)
        ));
    }

    #[test]
    fn after_a_wake_a_device_that_does_not_come_back_is_given_up_on_once_the_wait_is_up() {
        let (mut shared, _messages) = headset_gone_in_the_sleep();
        let woke = Instant::now();
        wake_up(&mut shared, woke);
        assert!(held(
            &mut shared,
            DeviceDirection::Output,
            woke + RETURN_WAIT - Duration::from_millis(1)
        ));
        assert!(!held(
            &mut shared,
            DeviceDirection::Output,
            woke + RETURN_WAIT
        ));
        assert_eq!(
            shared.lanes.output.wake_wait, None,
            "a wait that is up is let go of"
        );
    }

    #[test]
    fn after_a_wake_a_lane_whose_device_is_there_is_not_kept_waiting() {
        let (mut shared, _messages) = output_lane_on_the_headset();
        control(&mut shared, UiToAudio::SystemSleeping(true));
        let woke = Instant::now();
        wake_up(&mut shared, woke);
        assert!(!held(&mut shared, DeviceDirection::Output, woke));
    }

    #[test]
    fn a_device_the_user_picks_after_a_wake_ends_the_wait_there_and_then() {
        let (mut shared, _messages) = headset_gone_in_the_sleep();
        wake_up(&mut shared, Instant::now());
        select_device(&mut shared, DeviceDirection::Output, SPEAKERS.to_owned());
        assert_eq!(shared.lanes.output.wake_wait, None);
        assert!(!held(&mut shared, DeviceDirection::Output, Instant::now()));
    }

    #[test]
    fn a_lane_is_heard_again_the_tick_its_rules_leave_it_attached() {
        let mut shared = shared_for_tests();
        control(&mut shared, UiToAudio::SystemSleeping(true));
        let woke = Instant::now();
        wake_up(&mut shared, woke);
        let tick = woke + SUPERVISOR_PERIOD;
        settle_wake_mute(&mut shared, DeviceDirection::Output, tick, false);
        assert!(
            silenced(&shared).0,
            "not attached yet, and the time is not up"
        );
        settle_wake_mute(&mut shared, DeviceDirection::Output, tick, true);
        assert!(!silenced(&shared).0);
        assert_eq!(shared.lanes.output.wake_mute_until, None);
    }

    #[test]
    fn a_lane_still_waiting_for_its_device_is_heard_again_after_two_seconds() {
        let (mut shared, _messages) = headset_gone_in_the_sleep();
        let woke = Instant::now();
        wake_up(&mut shared, woke);
        settle_wake_mute(
            &mut shared,
            DeviceDirection::Output,
            woke + WAKE_MUTE - Duration::from_millis(1),
            false,
        );
        assert!(silenced(&shared).0);
        settle_wake_mute(
            &mut shared,
            DeviceDirection::Output,
            woke + WAKE_MUTE,
            false,
        );
        assert!(!silenced(&shared).0);
        assert!(
            WAKE_MUTE < RETURN_WAIT,
            "heard again before the wait for the device is given up, not after"
        );
    }

    #[test]
    fn a_lane_is_never_heard_again_while_the_system_sleeps() {
        let mut shared = shared_for_tests();
        control(&mut shared, UiToAudio::SystemSleeping(true));
        let woke = Instant::now();
        wake_up(&mut shared, woke);
        // Back to sleep before the lane had settled: the next wake starts afresh, and nothing
        // unmutes the lane in between, however long it takes.
        go_to_sleep(&mut shared, woke + SUPERVISOR_PERIOD);
        assert_eq!(shared.lanes.output.wake_mute_until, None);
        assert_eq!(shared.lanes.output.wake_wait, None);
        settle_wake_mute(
            &mut shared,
            DeviceDirection::Output,
            woke + WAKE_MUTE * 10,
            true,
        );
        assert_eq!(silenced(&shared), (true, true));
    }

    #[test]
    fn a_lane_detached_after_the_wake_is_not_left_silent_for_when_it_comes_back() {
        let mut shared = shared_for_tests();
        control(&mut shared, UiToAudio::SystemSleeping(true));
        wake_up(&mut shared, Instant::now());
        assert!(silenced(&shared).0);
        detach_lane(&mut shared, DeviceDirection::Output);
        assert!(!silenced(&shared).0);
        assert_eq!(shared.lanes.output.wake_mute_until, None);
        assert_eq!(shared.lanes.output.wake_wait, None);

        // Detached while the system sleeps, it stays silent with the other lane until the wake,
        // which finds it off and lets it go.
        let mut shared = shared_for_tests();
        control(&mut shared, UiToAudio::SystemSleeping(true));
        detach_lane(&mut shared, DeviceDirection::Output);
        assert_eq!(silenced(&shared), (true, true));
        control(&mut shared, UiToAudio::SystemSleeping(false));
        assert_eq!(silenced(&shared), (false, false));
    }

    #[test]
    fn a_sleep_whose_wake_is_never_heard_is_given_up_on_after_the_limit() {
        let mut shared = shared_for_tests();
        let queues = with_lane_queues(&mut shared);
        let slept = Instant::now();
        go_to_sleep(&mut shared, slept);
        give_up_on_sleep(&mut shared, slept + SLEEP_LIMIT - Duration::from_millis(1));
        assert!(shared.asleep_since.is_some());
        assert!(queues.output.is_empty());

        give_up_on_sleep(&mut shared, slept + SLEEP_LIMIT);
        assert_eq!(shared.asleep_since, None);
        assert_eq!(
            queues.output.try_iter().collect::<Vec<_>>(),
            [DspEvent::ResetFilterState],
            "woken as a wake would"
        );
        assert!(shared.lanes.output.wake_mute_until.is_some());
        assert!(
            SLEEP_LIMIT > Duration::from_secs(5) * 4,
            "well past logind's own delay before a suspend"
        );
    }

    #[test]
    fn holding_the_microphone_awake_is_kept_for_the_pairs_to_come_and_let_go_of_on_request() {
        // With no server there is no pair to give a recorder to; the wish is what is kept, for
        // every pair built while it stands.
        let mut shared = shared_for_tests();
        assert!(!shared.keep_input_awake);
        control(&mut shared, UiToAudio::KeepInputAwake(true));
        assert!(shared.keep_input_awake);
        control(&mut shared, UiToAudio::SystemSleeping(true));
        control(&mut shared, UiToAudio::SystemSleeping(false));
        assert!(shared.keep_input_awake, "a sleep does not let go of it");
        control(&mut shared, UiToAudio::KeepInputAwake(false));
        assert!(!shared.keep_input_awake);
    }

    #[test]
    fn per_application_rules_are_kept_without_a_server_and_build_nothing_until_there_is_one() {
        use fxsound_core::messages::{AppRoute, DspParams, RouteParams};

        let (mut shared, messages) = shared_with_messages();
        let route = AppRoute {
            direction: DeviceDirection::Output,
            app: fxsound_core::AppKey {
                binary: "bf6.exe".to_owned(),
                name: "Battlefield 6".to_owned(),
                flatpak: String::new(),
            },
            preset: "Gaming".to_owned(),
            params: RouteParams::Output(DspParams::default()),
            chain: String::new(),
        };
        control(&mut shared, UiToAudio::SetAppRoutes(vec![route]));
        assert_eq!(
            shared.routes.rule_count(),
            1,
            "kept for the connection to come"
        );
        assert_eq!(
            shared.routes.counts(),
            (0, 0),
            "with no graph there is no route"
        );
        control(&mut shared, UiToAudio::SetAppRoutes(Vec::new()));
        assert_eq!(shared.routes.rule_count(), 0);
        assert!(drained(&messages).is_empty(), "nothing to report");
        assert!(shared.lanes.output.nodes.is_none());
        assert!(shared.lanes.input.nodes.is_none());
        assert!(!shared.needs_publish, "and no device list to republish");
    }

    /// An application's stream as the main loop builds it: from its registry global, then its
    /// info, both of them `media.class`, `application.name` and `extra`.
    fn app_stream(id: u32, class: &str, name: &str, extra: &[(&str, &str)]) -> StreamNode {
        let mut pairs = vec![("media.class", class), ("application.name", name)];
        pairs.extend_from_slice(extra);
        let get = |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
        };
        let mut stream = StreamNode::from_props(id, &get).expect("an application's stream");
        stream.learn(&get);
        stream
    }

    /// Every application list among `messages`, in order.
    fn app_reports(messages: &[AudioToUi]) -> Vec<Vec<fxsound_core::AppStream>> {
        messages
            .iter()
            .filter_map(|message| match message {
                AudioToUi::AppStreams(streams) => Some(streams.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn application_streams_are_reported_on_the_tick_and_only_when_they_change() {
        let (mut shared, messages) = shared_with_messages();
        shared.apps.stream_appeared(app_stream(
            87,
            "Stream/Output/Audio",
            "Battlefield 6",
            &[("application.process.binary", "bf6.exe")],
        ));
        shared
            .apps
            .stream_appeared(app_stream(91, "Stream/Input/Audio", "Discord", &[]));
        assert!(
            drained(&messages).is_empty(),
            "nothing is said between ticks"
        );

        publish(&mut shared);
        let reports = app_reports(&drained(&messages));
        assert_eq!(reports.len(), 1, "two streams, one report");
        let listed: Vec<(u32, DeviceDirection, &str, &str)> = reports[0]
            .iter()
            .map(|stream| {
                (
                    stream.id,
                    stream.direction,
                    stream.app.binary.as_str(),
                    stream.app.name.as_str(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                (87, DeviceDirection::Output, "bf6.exe", "Battlefield 6"),
                (91, DeviceDirection::Input, "", "Discord"),
            ]
        );
        assert!(reports[0].iter().all(|stream| stream.route.is_none()));

        publish(&mut shared);
        assert!(
            app_reports(&drained(&messages)).is_empty(),
            "the same list is not sent twice"
        );

        assert_eq!(shared.apps.remove(87), Some(Tracked::Stream));
        publish(&mut shared);
        let reports = app_reports(&drained(&messages));
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0]
                .iter()
                .map(|stream| stream.id)
                .collect::<Vec<_>>(),
            vec![91]
        );
    }

    #[test]
    fn a_lost_connection_empties_the_application_list_and_says_so_once() {
        let (mut shared, messages) = shared_with_messages();
        shared.state = State::Connecting;
        shared
            .apps
            .stream_appeared(app_stream(87, "Stream/Output/Audio", "mpv", &[]));
        publish(&mut shared);
        assert_eq!(app_reports(&drained(&messages)).len(), 1);

        disconnect(&mut shared, "the server went away");
        publish(&mut shared);
        assert_eq!(
            app_reports(&drained(&messages)),
            vec![Vec::new()],
            "no stream of a server that is gone is running"
        );
        publish(&mut shared);
        assert!(app_reports(&drained(&messages)).is_empty());
    }

    #[test]
    fn fxsounds_sink_becoming_the_default_lists_the_recorders_of_the_default_monitor() {
        let (shared, messages) = shared_with_messages();
        let shared = Rc::new(RefCell::new(shared));
        shared.borrow_mut().apps.stream_appeared(app_stream(
            95,
            "Stream/Input/Audio",
            "OBS",
            &[("stream.capture.sink", "true")],
        ));
        publish(&mut shared.borrow_mut());
        assert!(
            app_reports(&drained(&messages)).is_empty(),
            "it records the speakers' monitor, not FxSound"
        );

        on_metadata_property(
            &shared,
            Some(devices::default_key(DeviceDirection::Output)),
            Some(r#"{"name":"fxsound_sink"}"#),
        );
        publish(&mut shared.borrow_mut());
        let reports = app_reports(&drained(&messages));
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].len(), 1);
        assert_eq!(reports[0][0].app.name, "OBS");
        assert_eq!(reports[0][0].direction, DeviceDirection::Input);
        {
            let guard = shared.borrow();
            let obs = guard.apps.stream(95).expect("tracked");
            assert_eq!(
                guard.apps.pin(obs),
                Some(app_streams::Pin::Monitor),
                "listed, and never moved onto a microphone's route"
            );
        }

        // The default source says nothing about a sink's monitor.
        on_metadata_property(
            &shared,
            Some(devices::default_key(DeviceDirection::Input)),
            Some(r#"{"name":"fxsound_source"}"#),
        );
        publish(&mut shared.borrow_mut());
        assert!(app_reports(&drained(&messages)).is_empty());

        on_metadata_property(
            &shared,
            Some(devices::default_key(DeviceDirection::Output)),
            Some(r#"{"name":"alsa_output.pci"}"#),
        );
        publish(&mut shared.borrow_mut());
        assert_eq!(app_reports(&drained(&messages)), vec![Vec::new()]);
    }

    #[test]
    fn a_cards_address_and_a_nodes_are_learned_from_their_info_and_kept_through_silence() {
        let (shared, _messages) = shared_with_messages();
        let shared = Rc::new(RefCell::new(shared));
        // As the registry announces it: `device.api` is in the global, the address is not.
        shared.borrow_mut().cards.push(Card {
            object_id: HEADSET_CARD,
            bluez_address: None,
            bluetooth: true,
        });
        add_device(
            &mut shared.borrow_mut(),
            DeviceInfo {
                bluez_address: None,
                ..headset_sink(70, 70)
            },
        );

        on_card_info(&shared, headset_card());
        on_node_address(&shared, 70, Some(HEADSET_ADDRESS.to_owned()));
        assert_eq!(shared.borrow().cards, [headset_card()]);
        assert_eq!(
            shared.borrow().devices[0].bluez_address.as_deref(),
            Some(HEADSET_ADDRESS)
        );

        // A route or profile change sends the info again without the address — without any
        // properties at all.
        on_card_info(
            &shared,
            Card {
                object_id: HEADSET_CARD,
                bluez_address: None,
                bluetooth: false,
            },
        );
        on_node_address(&shared, 70, None);
        assert_eq!(shared.borrow().cards, [headset_card()]);
        assert_eq!(
            shared.borrow().devices[0].bluez_address.as_deref(),
            Some(HEADSET_ADDRESS)
        );
    }

    // ---- U9: WirePlumber 0.5's Bluetooth microphone, and one headset on both lanes -----------

    /// WirePlumber 0.5's microphone for the headset: the loopback, named by the address.
    const LOOPBACK: &str = "bluez_input.00:11:22:33:44:55";
    /// The headset's SCO source: WirePlumber 0.5's internal node, WirePlumber 0.4's microphone.
    const SCO_SOURCE: &str = "bluez_input.00_11_22_33_44_55.0";
    const LAPTOP_MICROPHONE: &str = "alsa_input.pci-0000_00_1f.3.analog-stereo";

    /// A source as the registry announces it: a name, a class, the card it names, and nothing
    /// that says Bluetooth.
    fn announced_source(object_id: u32, name: &str, card: Option<u32>) -> DeviceInfo {
        let card = card.map(|id| id.to_string());
        DeviceInfo::from_props(object_id, &|key: &str| match key {
            "media.class" => Some(devices::SOURCE_MEDIA_CLASS),
            "node.name" => Some(name),
            "device.id" => card.as_deref(),
            _ => None,
        })
        .expect("a source that is not one of ours")
    }

    /// What a node's info says, read the way the engine's info callback reads it.
    fn info_of(pairs: &[(&str, &str)]) -> (BluezFacts, FormFactor) {
        let get = |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        (BluezFacts::from_props(&get), FormFactor::from_props(&get))
    }

    /// WirePlumber 0.5's loopback microphone's info (`create-loopback-node.lua:44-55`).
    fn loopback_info() -> (BluezFacts, FormFactor) {
        info_of(&[
            ("media.class", "Audio/Source"),
            ("node.name", LOOPBACK),
            ("bluez5.loopback", "true"),
            ("device.id", "60"),
        ])
    }

    /// Both lanes enabled, the headset's card listed, and its sink, its microphone, the laptop's
    /// speakers and the laptop's microphone announced — the microphone with its info in.
    fn both_lanes_beside_a_headset() -> (Rc<RefCell<Shared>>, Receiver<AudioToUi>) {
        let (mut shared, messages) = shared_with_messages();
        shared.lanes.input.enabled = true;
        shared.cards.push(headset_card());
        shared.cards.push(Card {
            object_id: 45,
            bluez_address: None,
            bluetooth: false,
        });
        add_device(&mut shared, headset_sink(70, 70));
        add_device(
            &mut shared,
            DeviceInfo {
                card_id: Some(45),
                ..device(57, SPEAKERS, DeviceDirection::Output)
            },
        );
        add_device(
            &mut shared,
            announced_source(73, LOOPBACK, Some(HEADSET_CARD)),
        );
        add_device(
            &mut shared,
            announced_source(58, LAPTOP_MICROPHONE, Some(45)),
        );
        let shared = Rc::new(RefCell::new(shared));
        let (facts, form_factor) = loopback_info();
        on_node_bluetooth(&shared, 73, facts, form_factor);
        (shared, messages)
    }

    /// Put each lane on a device, as `publish_attachment` records it once the lane's pair is up.
    fn attach(shared: &Rc<RefCell<Shared>>, output: Option<&str>, input: Option<&str>) {
        let mut guard = shared.borrow_mut();
        guard.lanes.output.attached = output.map(str::to_owned);
        guard.lanes.input.attached = input.map(str::to_owned);
    }

    /// The warnings the engine has sent since the last look.
    fn warnings(messages: &Receiver<AudioToUi>) -> Vec<(Option<DeviceDirection>, String)> {
        drained(messages)
            .into_iter()
            .filter_map(|message| match message {
                AudioToUi::Warning { direction, message } => Some((direction, message)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn wireplumber_05s_microphone_is_a_headsets_from_its_registry_global_on_a_bluetooth_card() {
        let (mut shared, _messages) = shared_with_messages();
        shared.cards.push(headset_card());
        add_device(
            &mut shared,
            announced_source(73, LOOPBACK, Some(HEADSET_CARD)),
        );
        let microphone = &shared.devices[0];
        assert!(microphone.bluez.card);
        assert!(microphone.bluez_headset);
        assert_eq!(microphone.form_factor, devices::FormFactor::Headset);
        assert_eq!(microphone.native_rate(), Some(16_000.0));

        // A microphone on a card that is not Bluetooth is only a microphone.
        shared.cards.push(Card {
            object_id: 45,
            bluez_address: None,
            bluetooth: false,
        });
        add_device(
            &mut shared,
            announced_source(58, LAPTOP_MICROPHONE, Some(45)),
        );
        let laptop = &shared.devices[1];
        assert!(!laptop.bluez_headset);
        assert_eq!(laptop.form_factor, devices::FormFactor::Microphone);
    }

    #[test]
    fn a_card_that_turns_out_to_be_bluetooth_makes_its_microphone_a_headsets() {
        let (mut shared, _messages) = shared_with_messages();
        // A card announced without `device.api`, and its microphone.
        shared.cards.push(Card {
            object_id: HEADSET_CARD,
            bluez_address: None,
            bluetooth: false,
        });
        add_device(
            &mut shared,
            announced_source(73, LOOPBACK, Some(HEADSET_CARD)),
        );
        shared.needs_publish = false;
        assert!(!shared.devices[0].bluez_headset);

        let shared = Rc::new(RefCell::new(shared));
        on_card_info(&shared, headset_card());
        let guard = shared.borrow();
        assert!(guard.cards[0].bluetooth);
        assert!(guard.devices[0].bluez_headset);
        assert!(guard.needs_publish, "the GUI's list shows it as a headset");
    }

    #[test]
    fn a_microphones_info_tells_the_engine_it_is_wireplumber_05s_loopback() {
        let (shared, _messages) = shared_with_messages();
        let shared = Rc::new(RefCell::new(shared));
        // On no card the engine knows of: only the info can say.
        add_device(
            &mut shared.borrow_mut(),
            announced_source(73, LOOPBACK, None),
        );
        shared.borrow_mut().needs_publish = false;
        assert!(!shared.borrow().devices[0].bluez_headset);

        let (facts, form_factor) = loopback_info();
        on_node_bluetooth(&shared, 73, facts, form_factor);
        let guard = shared.borrow();
        assert!(guard.devices[0].bluez_headset);
        assert_eq!(guard.devices[0].form_factor, devices::FormFactor::Headset);
        assert!(guard.needs_publish);
        assert_eq!(
            published_devices(&guard)[0].form_factor,
            "headset",
            "and the GUI is shown a headset"
        );
    }

    #[test]
    fn an_internal_sco_source_leaves_the_list_once_its_info_says_so_and_is_not_waited_for() {
        let (mut shared, _messages) = shared_with_messages();
        shared.cards.push(headset_card());
        shared.lanes.input.enabled = true;
        add_device(
            &mut shared,
            announced_source(71, SCO_SOURCE, Some(HEADSET_CARD)),
        );
        add_device(
            &mut shared,
            announced_source(73, LOOPBACK, Some(HEADSET_CARD)),
        );
        shared.lanes.input.needs_rules = false;
        shared.lanes.output.needs_rules = false;
        shared.needs_publish = false;
        let shared = Rc::new(RefCell::new(shared));

        on_node_bluetooth(
            &shared,
            71,
            info_of(&[
                ("media.class", "Audio/Source"),
                ("node.name", SCO_SOURCE),
                ("api.bluez5.profile", "headset-head-unit"),
                ("api.bluez5.codec", "msbc"),
                ("api.bluez5.internal", "true"),
                ("bluez5.loopback", "false"),
            ])
            .0,
            devices::FormFactor::Headset,
        );
        {
            let guard = shared.borrow();
            let names: Vec<&str> = guard.devices.iter().map(|d| d.name.as_str()).collect();
            assert_eq!(names, [LOOPBACK], "WirePlumber's own node is no microphone");
            assert!(guard.needs_publish);
            assert!(
                guard.lanes.input.needs_rules,
                "in case the rules took it before its info arrived"
            );
            assert!(
                !guard.lanes.output.needs_rules,
                "nothing the output lane cares about"
            );
            assert_eq!(
                guard.lanes.input.hold, None,
                "and it is not a device that went"
            );
        }

        // When it goes for good — the headset back on A2DP — there is nothing left to take.
        on_global_remove(&shared, 71);
        assert_eq!(shared.borrow().devices.len(), 1);
    }

    #[test]
    fn a_microphone_that_turns_out_to_be_a_headset_asks_for_a_pair_of_its_own() {
        let plain = announced_source(73, LOOPBACK, None);
        let mut headset = plain.clone();
        let (facts, form_factor) = loopback_info();
        headset.learn_bluetooth(facts, form_factor);
        let before = PairFormat::for_target(&plain, DEFAULT_SAMPLE_RATE);
        let after = PairFormat::for_target(&headset, DEFAULT_SAMPLE_RATE);
        assert_eq!(before.source_rate, None);
        assert_eq!(after.source_rate, Some(16_000));
        assert_ne!(
            before, after,
            "a pair built before the info arrived was told the wrong bandwidth, and is rebuilt"
        );
        assert_eq!(
            (before.channels, before.rate, before.positions),
            (after.channels, after.rate, after.positions),
            "nothing else about the pair changes"
        );

        // The music lane's pair has no voice chain to tell: a headset's sink changes nothing.
        let sink = headset_sink(70, 70);
        let call = DeviceInfo {
            bluez_headset: true,
            ..sink.clone()
        };
        assert_eq!(
            PairFormat::for_target(&sink, DEFAULT_SAMPLE_RATE),
            PairFormat::for_target(&call, DEFAULT_SAMPLE_RATE)
        );
    }

    #[test]
    fn one_headset_on_both_lanes_is_warned_about_once_per_attachment() {
        let (shared, messages) = both_lanes_beside_a_headset();
        drained(&messages);

        attach(&shared, Some(HEADSET_SINK), Some(LOOPBACK));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        let told = warnings(&messages);
        assert_eq!(
            told,
            [(None, fxsound_core::i18n::tr(ONE_HEADSET_ON_BOTH_LANES))],
            "one warning, about both lanes, in the language in effect"
        );
        assert!(told[0].1.contains("16 kHz"));

        // Every tick after says nothing new.
        for _ in 0..3 {
            warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        }
        assert_eq!(warnings(&messages), []);

        // A lane between pairs — a repair, a profile switch it waits out, a reconnect — is no new
        // attachment.
        attach(&shared, None, Some(LOOPBACK));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        attach(&shared, Some(HEADSET_SINK), Some(LOOPBACK));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(warnings(&messages), []);

        // The music moved to the speakers and back: a new attachment, and a new warning.
        attach(&shared, Some(SPEAKERS), Some(LOOPBACK));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(warnings(&messages), []);
        attach(&shared, Some(HEADSET_SINK), Some(LOOPBACK));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(warnings(&messages).len(), 1);

        // The microphone lane detached and attached again: the same.
        shared.borrow_mut().lanes.input.enabled = false;
        attach(&shared, Some(HEADSET_SINK), None);
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        shared.borrow_mut().lanes.input.enabled = true;
        attach(&shared, Some(HEADSET_SINK), Some(LOOPBACK));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(warnings(&messages).len(), 1);
    }

    #[test]
    fn a_headsets_sink_beside_another_microphone_is_nothing_to_warn_about() {
        let (shared, messages) = both_lanes_beside_a_headset();
        drained(&messages);
        for (output, input) in [
            (HEADSET_SINK, LAPTOP_MICROPHONE),
            (SPEAKERS, LOOPBACK),
            // One sound card, and not a Bluetooth one.
            (SPEAKERS, LAPTOP_MICROPHONE),
        ] {
            attach(&shared, Some(output), Some(input));
            warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
            assert_eq!(warnings(&messages), [], "{output} and {input}");
        }
        // Nor while only one lane runs.
        attach(&shared, Some(HEADSET_SINK), None);
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(warnings(&messages), []);
    }

    #[test]
    fn a_headsets_sink_between_profiles_is_judged_once_it_is_back() {
        let (shared, messages) = both_lanes_beside_a_headset();
        drained(&messages);
        remove_device(&mut shared.borrow_mut(), 70);
        attach(&shared, Some(HEADSET_SINK), Some(LOOPBACK));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(
            warnings(&messages),
            [],
            "the sink is not in the list to judge"
        );

        add_device(&mut shared.borrow_mut(), headset_sink(74, 74));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(warnings(&messages).len(), 1);
    }

    #[test]
    fn wireplumber_04s_microphone_and_its_headsets_sink_are_warned_about_too() {
        let (mut shared, messages) = shared_with_messages();
        shared.lanes.input.enabled = true;
        add_device(
            &mut shared,
            DeviceInfo {
                card_id: None,
                ..headset_sink(70, 70)
            },
        );
        // WirePlumber 0.4 names the SCO source by address, profile and codec, and no card here.
        add_device(&mut shared, announced_source(71, SCO_SOURCE, None));
        let shared = Rc::new(RefCell::new(shared));
        // Both ends of the SCO link say so in their info, and nowhere else.
        let (facts, form_factor) = info_of(&[
            ("api.bluez5.address", HEADSET_ADDRESS),
            ("api.bluez5.profile", "headset-head-unit"),
            ("api.bluez5.codec", "cvsd"),
        ]);
        for id in [70, 71] {
            on_node_address(&shared, id, Some(HEADSET_ADDRESS.to_owned()));
            on_node_bluetooth(&shared, id, facts, form_factor);
        }
        assert_eq!(
            shared.borrow().devices[1].native_rate(),
            Some(8_000.0),
            "its codec's rate"
        );
        drained(&messages);

        attach(&shared, Some(HEADSET_SINK), Some(SCO_SOURCE));
        warn_of_one_headset_on_both_lanes(&mut shared.borrow_mut());
        assert_eq!(warnings(&messages).len(), 1, "one address, one headset");
    }

    #[test]
    fn a_default_that_moves_is_news_for_its_own_lane_only() {
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        shared.borrow_mut().lanes.input.enabled = true;

        on_metadata_property(
            &shared,
            Some(devices::default_key(DeviceDirection::Input)),
            Some(r#"{"name":"alsa_input.usb-fifine"}"#),
        );
        {
            let guard = shared.borrow();
            assert!(guard.lanes.input.needs_rules);
            assert!(!guard.lanes.output.needs_rules);
            assert_eq!(
                guard.defaults.input.current.as_deref(),
                Some("alsa_input.usb-fifine")
            );
        }

        shared.borrow_mut().lanes.input.needs_rules = false;
        on_metadata_property(
            &shared,
            Some(devices::default_key(DeviceDirection::Output)),
            Some(r#"{"name":"alsa_output.pci"}"#),
        );
        let guard = shared.borrow();
        assert!(guard.lanes.output.needs_rules);
        assert!(!guard.lanes.input.needs_rules);
    }

    #[test]
    fn a_node_error_on_one_lane_backs_off_that_lane_only() {
        let (mut shared, messages) = shared_with_messages();
        shared.lanes.input.enabled = true;
        let output_before = lane_state(&shared, DeviceDirection::Output);

        let start = Instant::now();
        shared
            .lanes
            .input
            .status
            .sink_error
            .store(true, Ordering::Relaxed);
        for direction in DeviceDirection::ALL {
            supervise_lane(&mut shared, direction, start);
        }

        let input = &shared.lanes.input;
        assert_eq!(input.attempts, 1);
        assert_eq!(input.next_attempt, start + Duration::from_millis(200));
        assert!(input.needs_rules, "the pair is coming back, after the wait");
        assert!(
            !input.status.sink_error.load(Ordering::Relaxed),
            "the flag was acted on once"
        );
        assert_eq!(
            lane_state(&shared, DeviceDirection::Output),
            output_before,
            "the microphone failing changed something about the speakers"
        );
        assert_eq!(
            shared.state,
            State::Disconnected,
            "the connection is not the lane's to drop"
        );
        assert_eq!(
            drained(&messages),
            [AudioToUi::Error {
                direction: Some(DeviceDirection::Input),
                message: AudioError::DeviceUnavailable.to_string(),
            }],
            "reported against the lane, and no disconnection"
        );

        // Every failure that follows doubles the wait, up to five seconds, and is not re-reported.
        let mut waits = Vec::new();
        for _ in 0..8 {
            let now = shared.lanes.input.next_attempt;
            shared
                .lanes
                .input
                .status
                .output_error
                .store(true, Ordering::Relaxed);
            supervise_lane(&mut shared, DeviceDirection::Input, now);
            waits.push((shared.lanes.input.next_attempt - now).as_millis());
        }
        assert_eq!(waits, [400, 800, 1600, 3200, 5000, 5000, 5000, 5000]);
        assert_eq!(drained(&messages), []);
        assert_eq!(lane_state(&shared, DeviceDirection::Output), output_before);
    }

    #[test]
    fn a_failed_build_is_retried_on_the_lanes_backoff_rather_than_left_for_the_registry() {
        let (mut shared, messages) = shared_with_messages();
        shared.lanes.input.enabled = true;
        add_device(
            &mut shared,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        shared.lanes.input.needs_rules = false;

        // No connection, so the build fails after the rules have chosen the microphone.
        let before = Instant::now();
        apply_rules(&mut shared, DeviceDirection::Input);

        let input = &shared.lanes.input;
        assert!(input.nodes.is_none());
        assert!(input.needs_rules, "a retry is scheduled");
        assert_eq!(input.attempts, 1);
        assert!(input.next_attempt >= before + Duration::from_millis(200));
        assert_eq!(
            drained(&messages),
            [AudioToUi::Error {
                direction: Some(DeviceDirection::Input),
                message: AudioError::PipewireDisconnected.to_string(),
            }]
        );
        assert!(!shared.lanes.output.needs_rules);
        assert_eq!(shared.lanes.output.attempts, 0);
    }

    #[test]
    fn a_disabled_lane_never_builds_nodes() {
        let (mut shared, messages) = shared_with_messages();
        add_device(
            &mut shared,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        // However it came to be asked.
        shared.lanes.input.needs_rules = true;

        supervise_lane(&mut shared, DeviceDirection::Input, Instant::now());
        apply_rules(&mut shared, DeviceDirection::Input);

        assert!(shared.lanes.input.nodes.is_none());
        assert!(
            shared.lanes.input.previous_names.is_empty(),
            "the rules never ran"
        );
        assert_eq!(shared.memory.input, SelectionMemory::default());
        assert_eq!(drained(&messages), [], "not even a failed attempt");

        // The same call on the enabled lane does reach the build — and fails it, with no server.
        shared.lanes.input.enabled = true;
        apply_rules(&mut shared, DeviceDirection::Input);
        assert_eq!(shared.lanes.input.previous_names, ["alsa_input.usb-fifine"]);
        assert_eq!(drained(&messages).len(), 1);
    }

    #[test]
    fn status_and_attachment_are_published_per_lane_and_only_when_they_change() {
        let (mut shared, messages) = shared_with_messages();

        publish(&mut shared);
        assert_eq!(
            drained(&messages),
            [],
            "two lanes that never ran have nothing to report"
        );

        shared
            .lanes
            .input
            .counters
            .sample_rate
            .store(16_000, Ordering::Relaxed);
        publish(&mut shared);
        assert_eq!(
            drained(&messages),
            [AudioToUi::Status {
                direction: DeviceDirection::Input,
                status: AudioStatus {
                    sample_rate: 16_000,
                    ..AudioStatus::default()
                },
            }]
        );
        publish(&mut shared);
        assert_eq!(drained(&messages), [], "unchanged: not sent again");

        shared
            .lanes
            .output
            .counters
            .channels
            .store(8, Ordering::Relaxed);
        publish(&mut shared);
        assert_eq!(
            drained(&messages),
            [AudioToUi::Status {
                direction: DeviceDirection::Output,
                status: AudioStatus {
                    channels: 8,
                    ..AudioStatus::default()
                },
            }],
            "the output lane's change, and nothing about the input lane"
        );

        // A lane the GUI believes attached whose pair has gone is reported as attached to
        // nothing, once.
        shared.lanes.output.attached = Some("alsa_output.pci".to_owned());
        publish(&mut shared);
        assert_eq!(
            drained(&messages),
            [AudioToUi::Attached {
                direction: DeviceDirection::Output,
                node_name: None,
            }]
        );
        publish(&mut shared);
        assert_eq!(drained(&messages), []);
    }

    #[test]
    fn an_attachment_is_reported_when_the_target_changes_and_not_otherwise() {
        let mut lane = Lane::new(true, None);
        assert_eq!(lane.note_attachment(None), None, "nothing to nothing");
        assert_eq!(
            lane.note_attachment(Some("t_71")),
            Some(Some("t_71".to_owned()))
        );
        assert_eq!(lane.note_attachment(Some("t_71")), None);
        assert_eq!(
            lane.note_attachment(Some("t_stereo")),
            Some(Some("t_stereo".to_owned())),
            "a new target is news"
        );
        assert_eq!(lane.note_attachment(None), Some(None), "so is the teardown");
        assert_eq!(lane.note_attachment(None), None);
    }

    #[test]
    fn each_lane_holds_its_own_claim_on_the_default() {
        let mut shared = shared_for_tests();
        shared.defaults.output.holding = true;
        shared.defaults.input.holding = true;

        control(
            &mut shared,
            UiToAudio::SetAsDefault {
                direction: DeviceDirection::Input,
                want: false,
            },
        );
        assert!(!shared.lanes.input.want_default);
        assert!(!shared.defaults.input.holding, "handed back");
        assert!(shared.lanes.output.want_default);
        assert!(
            shared.defaults.output.holding,
            "not the input lane's to give"
        );

        control(
            &mut shared,
            UiToAudio::SetAsDefault {
                direction: DeviceDirection::Input,
                want: true,
            },
        );
        assert!(shared.lanes.input.want_default);
        assert!(
            !shared.defaults.input.holding,
            "with no nodes, the claim waits for them rather than naming a node that is not there"
        );
    }

    #[test]
    fn opting_back_in_between_pairs_makes_the_next_pair_claim_even_on_the_same_device() {
        let mut shared = shared_for_tests();
        shared.lanes.input.enabled = true;
        shared.lanes.input.want_default = false;
        // The pair was up on this microphone, failed, and is waiting out its backoff: its rebuild
        // would be a repair, which leaves the default alone.
        shared.lanes.input.last_target = Some("alsa_input.usb-fifine".to_owned());
        control(
            &mut shared,
            UiToAudio::SetAsDefault {
                direction: DeviceDirection::Input,
                want: true,
            },
        );
        assert_eq!(
            shared.lanes.input.last_target, None,
            "the opt-in would have waited for a pair that never claims"
        );
    }

    #[test]
    fn both_defaults_are_handed_back_on_exit() {
        let mut shared = shared_for_tests();
        shared.lanes.input.enabled = true;
        shared.defaults.output.holding = true;
        shared.defaults.input.holding = true;
        // With no metadata object there is nothing to write to, so nothing to wait for; what
        // matters here is that neither claim survives the exit path. `graph_churn` watches the
        // real writes arrive.
        release_all_defaults(&mut shared);
        assert!(!shared.defaults.output.holding);
        assert!(!shared.defaults.input.holding);
    }

    /// The configured key of `direction`, as the `default` metadata object would report it.
    fn configured_key_names(shared: &Rc<RefCell<Shared>>, direction: DeviceDirection, name: &str) {
        on_metadata_property(
            shared,
            Some(devices::configured_default_key(direction)),
            Some(&devices::default_node_value(name)),
        );
    }

    #[test]
    fn a_claim_found_for_a_detached_lane_is_handed_back_once_there_is_a_device_to_hand_it_to() {
        // A killed run held the default source; this one starts with that microphone unplugged,
        // so the input lane stays detached. What the settings file remembered is seeded.
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        control(
            &mut shared.borrow_mut(),
            UiToAudio::SeedRememberedDefaults {
                output: String::new(),
                input: "alsa_input.usb-fifine".to_owned(),
            },
        );
        configured_key_names(&shared, DeviceDirection::Input, SOURCE_NODE_NAME);
        configured_key_names(&shared, DeviceDirection::Output, SINK_NODE_NAME);
        let mut guard = shared.borrow_mut();
        assert!(guard.defaults.input.holding);
        assert!(
            guard.defaults.input.disowned,
            "nobody picked a node that is not there"
        );
        assert!(
            !guard.defaults.output.disowned,
            "the output lane stands behind its claim: its pair is about to come up under that name"
        );

        // Nothing remembered is present yet: the claim is not released into nothing.
        hand_back_disowned_defaults(&mut guard);
        assert!(guard.defaults.input.holding, "released with nowhere to go");

        // The microphone is plugged back in: the claim goes back to it, and only that one.
        add_device(
            &mut guard,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        hand_back_disowned_defaults(&mut guard);
        assert!(!guard.defaults.input.holding, "handed back");
        assert!(!guard.defaults.input.disowned);
        assert!(
            guard.defaults.output.holding,
            "the default sink is none of the input lane's business"
        );
    }

    #[test]
    fn a_lane_attached_since_adopts_the_claim_instead_of_handing_it_back() {
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        configured_key_names(&shared, DeviceDirection::Input, SOURCE_NODE_NAME);
        let mut guard = shared.borrow_mut();
        guard.memory.input.most_recent_default = "alsa_input.usb-fifine".to_owned();
        add_device(
            &mut guard,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        assert!(guard.defaults.input.disowned);

        // The microphone is picked before the supervisor gets to it.
        control(
            &mut guard,
            UiToAudio::SelectDevice {
                node_name: "alsa_input.usb-fifine".to_owned(),
                direction: DeviceDirection::Input,
            },
        );
        hand_back_disowned_defaults(&mut guard);
        assert!(
            guard.defaults.input.holding,
            "the lane wants the default and its pair is coming: handing it back would only have \
             the pair take it again"
        );
        assert!(!guard.defaults.input.disowned);
    }

    #[test]
    fn an_opt_out_that_arrived_with_the_connection_down_is_honoured_when_it_comes_back() {
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        {
            let mut guard = shared.borrow_mut();
            guard.lanes.input.enabled = true;
            guard.memory.input.user_selected = "alsa_input.usb-fifine".to_owned();
            add_device(
                &mut guard,
                device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
            );
            // No connection, so nothing is known to be held and nothing can be handed back now.
            control(
                &mut guard,
                UiToAudio::SetAsDefault {
                    direction: DeviceDirection::Input,
                    want: false,
                },
            );
            assert!(!guard.defaults.input.holding);
        }

        // The connection is back, and the key still names FxSound's source.
        configured_key_names(&shared, DeviceDirection::Input, SOURCE_NODE_NAME);
        let mut guard = shared.borrow_mut();
        assert!(guard.defaults.input.disowned);
        hand_back_disowned_defaults(&mut guard);
        assert!(!guard.defaults.input.holding, "the opt-out was ignored");
    }

    #[test]
    fn a_mono_headset_is_listed_among_the_outputs_the_gui_may_pick() {
        // The list used to leave a mono output out altogether, because the rules would have
        // refused it: a headset in its call profile simply vanished from the picker.
        let (mut shared, messages) = shared_with_messages();
        let headset = DeviceInfo::from_props(50, &|key: &str| match key {
            "media.class" => Some(devices::SINK_MEDIA_CLASS),
            "node.name" => Some("bluez_output.00_11_22_33_44_55.1"),
            "node.description" => Some("Headset"),
            "api.bluez5.profile" => Some("headset-head-unit"),
            "audio.channels" => Some("1"),
            _ => None,
        })
        .expect("a sink that is not one of ours");
        assert!(headset.is_mono());
        add_device(&mut shared, headset);
        add_device(&mut shared, device(51, "speakers", DeviceDirection::Output));
        add_device(
            &mut shared,
            device(52, "alsa_input.pci", DeviceDirection::Input),
        );

        let listed: Vec<(String, DeviceDirection, String)> = published_devices(&shared)
            .into_iter()
            .map(|d| (d.name, d.direction, d.form_factor))
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "bluez_output.00_11_22_33_44_55.1".to_owned(),
                    DeviceDirection::Output,
                    "headset".to_owned()
                ),
                (
                    "speakers".to_owned(),
                    DeviceDirection::Output,
                    "unknown".to_owned()
                ),
                (
                    "alsa_input.pci".to_owned(),
                    DeviceDirection::Input,
                    "microphone".to_owned()
                ),
            ],
            "every output by description, the mono one included, then every input"
        );

        publish(&mut shared);
        let sent = drained(&messages)
            .into_iter()
            .find_map(|message| match message {
                AudioToUi::Devices(devices) => Some(devices),
                _ => None,
            });
        assert!(
            sent.is_some_and(|devices| devices
                .iter()
                .any(|d| d.name == "bluez_output.00_11_22_33_44_55.1")),
            "the GUI is told about it"
        );
    }

    #[test]
    fn a_claim_no_lane_stands_behind_waits_for_the_gui_to_have_seen_the_device_list() {
        // The app attaches the input lane from the device list, the first time the saved
        // microphone is on it. Until it has seen the list, a claim no lane stands behind yet may be
        // one it is about to stand behind.
        let (mut shared, messages) = shared_with_messages();
        add_device(
            &mut shared,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        assert!(
            !shared.gui_has_had_its_chance(),
            "the microphone has not even been listed to the GUI yet"
        );
        publish(&mut shared);
        assert!(!shared.gui_has_had_its_chance(), "listed, but not read");
        assert!(
            drained(&messages)
                .iter()
                .any(|message| matches!(message, AudioToUi::Devices(_))),
            "the GUI reads the list"
        );
        assert!(
            !shared.gui_has_had_its_chance(),
            "the tick that sees the list taken can come between the GUI taking it and the \
             attachment it sends straight after"
        );
        assert!(shared.gui_has_had_its_chance());
        assert!(shared.gui_has_had_its_chance(), "and it stays had");

        // Another microphone plugged in: that list is owed to the GUI as well.
        add_device(
            &mut shared,
            device(41, "alsa_input.pci", DeviceDirection::Input),
        );
        assert!(!shared.gui_has_had_its_chance(), "not listed yet");
        publish(&mut shared);
        assert!(!shared.gui_has_had_its_chance(), "not read yet");
        drained(&messages);
        assert!(!shared.gui_has_had_its_chance());
        assert!(shared.gui_has_had_its_chance());
    }

    #[test]
    fn a_device_list_nobody_reads_holds_a_claim_back_for_five_seconds_and_no_longer() {
        let (mut shared, _unread) = shared_with_messages();
        add_device(
            &mut shared,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        publish(&mut shared);
        for tick in 1..GUI_PATIENCE_TICKS {
            assert!(
                !shared.gui_has_had_its_chance(),
                "gave up on the GUI after {tick} ticks"
            );
        }
        assert!(
            shared.gui_has_had_its_chance(),
            "an engine nobody reads from held the claim for ever"
        );
        assert_eq!(
            SUPERVISOR_PERIOD * GUI_PATIENCE_TICKS,
            Duration::from_secs(5)
        );

        // With nobody listening at all, nothing is queued and there is no one to wait for.
        let mut shared = shared_for_tests();
        add_device(
            &mut shared,
            device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
        );
        publish(&mut shared);
        assert!(!shared.gui_has_had_its_chance());
        assert!(shared.gui_has_had_its_chance());
    }

    /// A lane attached to its microphone and opted out of the default, whose source the user made
    /// the default in their sound settings all the same — then the connection goes, and comes back
    /// with the microphone listed.
    fn opted_out_lane_the_user_gave_the_default_by_hand_across_a_disconnect(
        while_down: Option<UiToAudio>,
    ) -> Rc<RefCell<Shared>> {
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        {
            let mut guard = shared.borrow_mut();
            guard.state = State::Connecting;
            guard.lanes.input.enabled = true;
            guard.lanes.input.want_default = false;
            guard.memory.input.most_recent_default = "alsa_input.usb-fifine".to_owned();
            guard.defaults.input.holding = true;
            disconnect(&mut guard, "the server went away");
            if let Some(message) = while_down {
                control(&mut guard, message);
            }
            // The reconnect lists the microphone again, so a hand-back would have somewhere to go.
            add_device(
                &mut guard,
                device(40, "alsa_input.usb-fifine", DeviceDirection::Input),
            );
        }
        // …and reads the key back before the lane's pair has been rebuilt.
        configured_key_names(&shared, DeviceDirection::Input, SOURCE_NODE_NAME);
        shared
    }

    #[test]
    fn a_default_the_user_gave_an_opted_out_lane_by_hand_survives_a_reconnect() {
        let shared = opted_out_lane_the_user_gave_the_default_by_hand_across_a_disconnect(None);
        let mut guard = shared.borrow_mut();
        assert!(guard.defaults.input.holding);
        assert!(
            !guard.defaults.input.disowned,
            "the user's own pick was taken for a claim nobody stands behind"
        );
        hand_back_disowned_defaults(&mut guard);
        assert!(
            guard.defaults.input.holding,
            "a reconnect undid the default the user picked"
        );
    }

    #[test]
    fn a_default_given_by_hand_still_goes_back_when_the_lane_lets_go_of_it_with_the_connection_down()
     {
        for let_go in [
            UiToAudio::SetAsDefault {
                direction: DeviceDirection::Input,
                want: false,
            },
            UiToAudio::DetachLane(DeviceDirection::Input),
        ] {
            let what = format!("{let_go:?}");
            let shared =
                opted_out_lane_the_user_gave_the_default_by_hand_across_a_disconnect(Some(let_go));
            let mut guard = shared.borrow_mut();
            assert!(
                guard.defaults.input.disowned,
                "{what} with the connection down was forgotten by the reconnect"
            );
            hand_back_disowned_defaults(&mut guard);
            assert!(
                !guard.defaults.input.holding,
                "{what} would have handed the default back with the connection up"
            );
        }
    }

    #[test]
    fn a_key_the_user_moved_away_from_fxsound_is_nothing_to_hand_back() {
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        configured_key_names(&shared, DeviceDirection::Input, SOURCE_NODE_NAME);
        configured_key_names(&shared, DeviceDirection::Input, "alsa_input.usb-fifine");
        let guard = shared.borrow();
        assert!(!guard.defaults.input.holding);
        assert!(!guard.defaults.input.disowned);
    }

    #[test]
    fn the_echo_cancellers_source_is_never_remembered_as_the_default_from_before_fxsound() {
        // The microphone the input lane held the default source on was unplugged: the configured
        // key still names FxSound's source, and WirePlumber, with nothing else left, fell back to
        // the echo canceller's. The microphone comes back and its pair claims the default again.
        let (mut shared, messages) = shared_with_messages();
        shared.defaults.input.configured = Some(SOURCE_NODE_NAME.to_owned());
        shared.defaults.input.current = Some(AEC_SOURCE_NODE_NAME.to_owned());
        claim_default(&mut shared, DeviceDirection::Input);
        assert_eq!(
            shared.memory.input,
            SelectionMemory::default(),
            "one of our own nodes was remembered as the default from before FxSound"
        );
        assert!(
            !drained(&messages)
                .iter()
                .any(|m| matches!(m, AudioToUi::RememberedDefault { .. })),
            "…and sent to the settings file"
        );

        // A real device in either key is what is remembered.
        shared.defaults.input.current = Some("alsa_input.usb-fifine".to_owned());
        claim_default(&mut shared, DeviceDirection::Input);
        assert_eq!(
            shared.memory.input.original_default,
            "alsa_input.usb-fifine"
        );
        assert_eq!(
            drained(&messages),
            [AudioToUi::RememberedDefault {
                direction: DeviceDirection::Input,
                node_name: "alsa_input.usb-fifine".to_owned(),
            }]
        );
    }

    #[test]
    fn no_node_of_ours_is_ever_seeded_as_a_remembered_default() {
        let mut shared = shared_for_tests();
        for name in crate::OUR_NODE_NAMES {
            control(
                &mut shared,
                UiToAudio::SeedRememberedDefaults {
                    output: name.to_owned(),
                    input: name.to_owned(),
                },
            );
        }
        assert_eq!(shared.memory.output, SelectionMemory::default());
        assert_eq!(shared.memory.input, SelectionMemory::default());

        control(
            &mut shared,
            UiToAudio::SeedRememberedDefaults {
                output: "alsa_output.pci".to_owned(),
                input: "alsa_input.usb-fifine".to_owned(),
            },
        );
        assert_eq!(shared.memory.output.original_default, "alsa_output.pci");
        assert_eq!(
            shared.memory.input.most_recent_default,
            "alsa_input.usb-fifine"
        );
    }

    #[test]
    fn losing_the_connection_keeps_which_lanes_are_enabled() {
        let (mut shared, messages) = shared_with_messages();
        shared.state = State::Connecting;
        shared.lanes.input.enabled = true;
        shared.lanes.input.previous_names = vec!["alsa_input.usb-fifine".to_owned()];

        disconnect(&mut shared, "the server went away");

        assert!(shared.lanes.output.enabled);
        assert!(
            shared.lanes.input.enabled,
            "the reconnect rebuilds every lane that was running"
        );
        assert!(shared.lanes.input.previous_names.is_empty());
        assert_eq!(
            drained(&messages),
            [AudioToUi::Disconnected {
                reason: "the server went away".to_owned(),
            }]
        );

        // And a detached lane stays detached through it.
        shared.state = State::Connecting;
        shared.lanes.input.enabled = false;
        disconnect(&mut shared, "again");
        assert!(!shared.lanes.input.enabled);
    }

    #[test]
    fn after_a_reconnect_only_a_lane_that_held_the_default_claims_it_again() {
        let mut shared = shared_for_tests();
        shared.state = State::Connecting;
        shared.lanes.output.last_target = Some("alsa_output.pci".to_owned());
        shared.defaults.output.holding = true;
        shared.lanes.input.enabled = true;
        shared.lanes.input.last_target = Some("alsa_input.usb-fifine".to_owned());
        // The user had made the microphone itself the default source.
        shared.defaults.input.holding = false;

        disconnect(&mut shared, "the server went away");
        assert_eq!(
            shared.lanes.output.last_target, None,
            "the server may have lost the key with everything else: the next pair claims it"
        );
        assert_eq!(
            shared.lanes.input.last_target.as_deref(),
            Some("alsa_input.usb-fifine"),
            "the next pair on the microphone is a repair, and the default stays the user's"
        );

        // Detached, the lane forgets it: whatever it is attached to next, it attaches afresh.
        detach_lane(&mut shared, DeviceDirection::Input);
        assert_eq!(shared.lanes.input.last_target, None);
    }

    #[test]
    fn a_lane_without_a_pair_is_not_forgiven_its_backoff() {
        let mut lane = Lane::new(true, None);
        let start = Instant::now();
        lane.retry_later(start);
        lane.retry_later(start);
        assert_eq!(lane.attempts, 2);
        // A lane still waiting to rebuild has not proved anything, however long it has been.
        lane.forgive_if_stable(start + Duration::from_secs(60));
        assert_eq!(lane.attempts, 2);
    }

    #[test]
    fn a_pair_that_stays_up_as_long_as_the_wait_before_it_is_forgiven_its_backoff_and_its_error() {
        let mut lane = Lane::new(true, None);
        let start = Instant::now();
        lane.retry_later(start);
        lane.retry_later(start);
        lane.last_error = Some(AudioError::DeviceUnavailable);
        // The wait before the third try was the second step of the table, 400 ms.
        assert_eq!(lane.next_attempt, start + Duration::from_millis(400));

        // Built late — the registry was not ready when the wait ran out — which must not count
        // as time the pair was up.
        let built = start + Duration::from_secs(1);
        lane.built_at = Some(built);
        lane.forgive_if_stable(built + Duration::from_millis(399));
        assert_eq!(lane.attempts, 2, "up for less than the wait before it");
        assert_eq!(lane.last_error, Some(AudioError::DeviceUnavailable));

        lane.forgive_if_stable(built + Duration::from_millis(400));
        assert_eq!(lane.attempts, 0, "the next failure waits 200 ms again");
        assert_eq!(lane.last_error, None, "and is reported again");
    }

    #[test]
    fn a_pair_that_fails_sooner_than_the_wait_before_it_is_neither_forgiven_nor_reported_again() {
        let (mut shared, messages) = shared_with_messages();
        shared.lanes.input.enabled = true;
        let fail = |shared: &mut Shared, at: Instant| {
            shared
                .lanes
                .input
                .status
                .sink_error
                .store(true, Ordering::Relaxed);
            supervise_lane(shared, DeviceDirection::Input, at);
        };
        // What `apply_rules` leaves behind on a successful build, without a server to build on.
        let build = |shared: &mut Shared, at: Instant| {
            let lane = &mut shared.lanes.input;
            lane.built_at = Some(at);
            lane.needs_rules = false;
        };
        let reported = [AudioToUi::Error {
            direction: Some(DeviceDirection::Input),
            message: AudioError::DeviceUnavailable.to_string(),
        }];

        let start = Instant::now();
        fail(&mut shared, start);
        assert_eq!(drained(&messages), reported);

        // Rebuilt when the 200 ms wait runs out, and refused again 100 ms later.
        let built = shared.lanes.input.next_attempt;
        build(&mut shared, built);
        fail(&mut shared, built + Duration::from_millis(100));
        let input = &shared.lanes.input;
        assert_eq!(
            input.attempts, 2,
            "the wait doubles rather than starting again"
        );
        assert_eq!(
            input.next_attempt,
            built + Duration::from_millis(100) + Duration::from_millis(400)
        );
        assert_eq!(input.built_at, None, "the pair it was timing is gone");
        assert_eq!(
            drained(&messages),
            [],
            "the same failure as before, not news"
        );

        // A pair that then stays up long enough is forgiven by the tick, and its next failure is
        // news again, backed off from 200 ms.
        let built = shared.lanes.input.next_attempt;
        build(&mut shared, built);
        supervise_lane(
            &mut shared,
            DeviceDirection::Input,
            built + Duration::from_millis(400),
        );
        assert_eq!(shared.lanes.input.attempts, 0);
        let failed = built + Duration::from_secs(10);
        fail(&mut shared, failed);
        assert_eq!(drained(&messages), reported);
        assert_eq!(
            shared.lanes.input.next_attempt,
            failed + Duration::from_millis(200)
        );
    }

    // ---- the 0.3.0 audit's engine bugs (`docs/0.4.0-design.md` §1.3) --------------------------
    //
    // The ones that need a pair of nodes, a bound proxy or a server's `settings` object are in
    // `live_session`, against a private daemon.

    /// The output lane's NODE 1 user data, with the ring as [`build_nodes`] leaves it: stereo,
    /// sized for a 16-frame quantum, before either node exists.
    fn node_one_for_tests(shared: &mut Shared) -> SinkData {
        let dsp = shared.lanes.output.dsp.take().expect("on the main loop");
        shared.lanes.output.ring.reconfigure(2, 16);
        SinkData::new(shared, dsp)
    }

    /// A `Format` param as PipeWire hands it to `param_changed`.
    fn negotiated(channels: u32) -> Vec<u8> {
        format_pod(48_000, channels, &ChannelMap::default_for(channels))
    }

    const FORMAT: u32 = libspa::sys::SPA_PARAM_Format;

    #[test]
    fn a_format_negotiated_again_leaves_the_ring_and_its_read_cursor_alone() {
        let mut shared = shared_with_dsp_for_tests();
        let mut node_one = node_one_for_tests(&mut shared);
        let ring = Arc::clone(&node_one.ring);
        let stereo = negotiated(2);
        adopt_sink_format(&mut node_one, FORMAT, Pod::from_bytes(&stereo));
        assert_eq!(node_one.channels, 2);

        // The pair is running: NODE 1 has pushed, NODE 2 has primed and taken a block.
        ring.push(&[0.25; 96]);
        let mut block = [0.0; 32];
        assert_eq!(ring.pop(&mut block), 32);
        let fill = ring.fill_frames();
        assert_eq!(fill, 32);

        // The sink is suspended and woken: its format is cleared, and the same one negotiated
        // again, while NODE 2 goes on popping on the data thread.
        adopt_sink_format(&mut node_one, FORMAT, None);
        adopt_sink_format(&mut node_one, FORMAT, Pod::from_bytes(&stereo));

        assert_eq!(
            ring.fill_frames(),
            fill,
            "the main loop moved the read cursor, which is NODE 2's"
        );
        assert!(
            ring.primed.load(Ordering::Relaxed),
            "the cushion NODE 2 was playing from was thrown away"
        );
        assert_eq!(node_one.channels, 2);
        assert_eq!(
            shared
                .lanes
                .output
                .counters
                .format_mismatches
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn a_format_the_ring_was_not_built_for_mutes_node_one_and_counts_as_a_mismatch() {
        let mut shared = shared_with_dsp_for_tests();
        let mut node_one = node_one_for_tests(&mut shared);
        let ring = Arc::clone(&node_one.ring);
        ring.push(&[0.25; 64]);

        let surround = negotiated(6);
        adopt_sink_format(&mut node_one, FORMAT, Pod::from_bytes(&surround));

        assert_eq!(
            node_one.channels, 0,
            "NODE 1 would push six-channel frames into a stereo ring"
        );
        assert_eq!(
            ring.channels(),
            2,
            "the ring keeps the format both nodes declared"
        );
        assert_eq!(ring.fill_frames(), 32, "and what is in it");
        assert_eq!(
            shared
                .lanes
                .output
                .counters
                .format_mismatches
                .load(Ordering::Relaxed),
            1,
            "a mismatch, for the supervisor to rebuild the pair on"
        );
    }

    #[test]
    fn a_pair_whose_nodes_disagree_about_the_format_is_rebuilt_on_the_lanes_backoff() {
        let (mut shared, messages) = shared_with_messages();
        let mut now = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..7 {
            shared
                .lanes
                .output
                .counters
                .format_mismatches
                .store(2, Ordering::Relaxed);
            supervise_lane(&mut shared, DeviceDirection::Output, now);
            let lane = &shared.lanes.output;
            assert!(lane.needs_rules, "the pair is coming back, after the wait");
            waits.push((lane.next_attempt - now).as_millis());
            now = lane.next_attempt;
        }
        assert_eq!(
            waits,
            [200, 400, 800, 1600, 3200, 5000, 5000],
            "a server that keeps disagreeing got a fresh pair on every tick"
        );
        assert_eq!(
            shared.lanes.output.format_mismatches_total, 14,
            "every mismatch is in the report"
        );
        assert_eq!(
            drained(&messages),
            [],
            "a count in the lane's status, not a notification"
        );
        assert_eq!(shared.lanes.input.attempts, 0);
    }

    #[test]
    fn a_node_that_reports_a_new_channel_count_is_taken_at_its_word() {
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        add_device(
            &mut shared.borrow_mut(),
            device(41, "alsa_output.pci", DeviceDirection::Output),
        );
        shared.borrow_mut().needs_publish = false;
        assert_eq!(shared.borrow().devices[0].channels, 2);

        // The card is switched from its analog stereo profile to 5.1 surround.
        let surround = ChannelMap::parse("FL,FR,FC,LFE,RL,RR");
        on_node_info(&shared, 41, 6, surround);
        {
            let guard = shared.borrow();
            let card = &guard.devices[0];
            assert_eq!(
                card.channels, 6,
                "the first count a node reports is not its last"
            );
            assert_eq!(Some(card.positions), surround);
            assert!(guard.needs_publish, "and the device list says so");
        }

        // The same info again — the node was only suspended and woken — and a report that says
        // nothing about the format: neither is news.
        shared.borrow_mut().needs_publish = false;
        on_node_info(&shared, 41, 6, surround);
        on_node_info(&shared, 41, 0, None);
        let guard = shared.borrow();
        assert!(!guard.needs_publish);
        assert_eq!(guard.devices[0].channels, 6);
        assert_eq!(Some(guard.devices[0].positions), surround);
    }

    #[test]
    fn a_failure_is_reported_once_however_its_description_varies_from_one_attempt_to_the_next() {
        let (mut shared, messages) = shared_with_messages();
        let refused = AudioError::PipewireUnavailable(
            "could not connect to PipeWire: Connection refused".to_owned(),
        );
        shared.report_connection_error(refused.clone());
        for why in [
            "No such file or directory",
            "Connection refused",
            "Host is down",
        ] {
            shared.report_connection_error(AudioError::PipewireUnavailable(format!(
                "could not connect to PipeWire: {why}"
            )));
        }
        assert_eq!(
            drained(&messages),
            [AudioToUi::Error {
                direction: None,
                message: refused.to_string(),
            }],
            "one outage, one notification"
        );

        for why in ["Creation failed", "Invalid argument"] {
            shared.report_error(
                DeviceDirection::Input,
                AudioError::PipewireUnavailable(why.to_owned()),
            );
        }
        assert_eq!(drained(&messages).len(), 1, "a lane's failures too");

        // A different failure is news.
        shared.report_error(DeviceDirection::Input, AudioError::DeviceUnavailable);
        assert_eq!(drained(&messages).len(), 1);
    }

    #[test]
    fn a_forced_rate_overrides_the_graphs_clock_until_it_is_released() {
        let shared = Rc::new(RefCell::new(shared_for_tests()));
        shared.borrow_mut().lanes.input.enabled = true;
        let say = |key: &str, value: &str| on_metadata_property(&shared, Some(key), Some(value));
        let rate = || shared.borrow().clock.rate();
        let rules = || {
            let mut guard = shared.borrow_mut();
            let asked = PerDirection::from_fn(|direction| guard.lanes.get(direction).needs_rules);
            for (_, lane) in guard.lanes.iter_mut() {
                lane.needs_rules = false;
            }
            (asked.output, asked.input)
        };

        // What a server's `settings` object says first, in the order it says it.
        say("clock.rate", "48000");
        say("clock.force-rate", "0");
        assert_eq!(rate(), 48_000);
        assert_eq!(rules(), (false, false), "nothing moved");

        say("clock.force-rate", "44100");
        assert_eq!(rate(), 44_100);
        assert_eq!(
            rules(),
            (true, false),
            "the output pair may have to follow; the capture stream never does"
        );

        // The plain rate moving under a forced one changes nothing that runs.
        say("clock.rate", "96000");
        assert_eq!(rate(), 44_100);
        assert_eq!(rules(), (false, false));

        say("clock.force-rate", "0");
        assert_eq!(rate(), 96_000, "released, the plain rate is back");
        assert_eq!(rules(), (true, false));

        // Only a sink that publishes no rate of its own runs at the graph's.
        let mut sink = device(41, "alsa_output.pci", DeviceDirection::Output);
        assert_eq!(PairFormat::for_target(&sink, 44_100).rate, 44_100);
        sink.rate = Some(48_000);
        assert_eq!(PairFormat::for_target(&sink, 44_100).rate, 48_000);
        let microphone = device(40, "alsa_input.usb-fifine", DeviceDirection::Input);
        assert_eq!(
            PairFormat::for_target(&microphone, 44_100).rate,
            CAPTURE_RATE
        );
    }

    // ---- U4: the priority list, and the routes that say what can be heard ---------------------

    /// A UCM laptop's card (`crate::routes`' fixture has the whole of it).
    const UCM_CARD: u32 = 46;
    const UCM_SPEAKER: &str =
        "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Speaker__sink";
    const UCM_HDMI: &str =
        "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__HDMI1__sink";
    const UCM_HEADSET_MIC: &str =
        "alsa_input.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Mic2__source";
    const USB_DAC: &str = "alsa_output.usb-dac.analog-stereo";

    fn ucm_card() -> Card {
        Card {
            object_id: UCM_CARD,
            bluez_address: None,
            bluetooth: false,
        }
    }

    /// A node of the UCM card as its registry global announces it: its card, and not yet which of
    /// the card's devices it is.
    fn on_ucm_card(object_id: u32, name: &str, direction: DeviceDirection) -> DeviceInfo {
        DeviceInfo {
            card_id: Some(UCM_CARD),
            ..device(object_id, name, direction)
        }
    }

    /// The active route of card device `device`, on port `port`.
    fn active_route(port: u32, device: u32, available: routes::Availability) -> Route {
        Route {
            index: port,
            name: None,
            device: Some(device),
            devices: vec![device],
            available,
        }
    }

    fn with_ucm_card() -> (Rc<RefCell<Shared>>, Receiver<AudioToUi>) {
        let (shared, messages) = shared_with_messages();
        let shared = Rc::new(RefCell::new(shared));
        shared.borrow_mut().cards.push(ucm_card());
        (shared, messages)
    }

    fn heard(shared: &Rc<RefCell<Shared>>, name: &str) -> bool {
        shared
            .borrow()
            .devices
            .iter()
            .find(|device| device.name == name)
            .expect("the device is listed")
            .available
    }

    #[test]
    fn a_priority_list_is_kept_for_its_own_lane_and_asks_only_that_lanes_rules() {
        let (mut shared, _messages) = shared_with_messages();
        shared.lanes.input.enabled = true;
        let ranking = |names: &[&str]| UiToAudio::SetDevicePriority {
            direction: DeviceDirection::Input,
            names: names.iter().map(|&name| name.to_owned()).collect(),
            new_devices_first: false,
        };

        control(
            &mut shared,
            ranking(&[UCM_HEADSET_MIC, "fxsound_source", "", LAPTOP_MICROPHONE]),
        );
        assert_eq!(
            shared.preference.input.ranking,
            [UCM_HEADSET_MIC, LAPTOP_MICROPHONE],
            "our own node and an empty name are no devices to rank"
        );
        assert!(shared.preference.output.ranking.is_empty());
        assert!(shared.lanes.input.needs_rules);
        assert!(
            !shared.lanes.output.needs_rules,
            "the speakers' ranking did not change"
        );

        shared.lanes.input.needs_rules = false;
        control(
            &mut shared,
            ranking(&[UCM_HEADSET_MIC, "fxsound_source", "", LAPTOP_MICROPHONE]),
        );
        assert!(
            !shared.lanes.input.needs_rules,
            "the same list again is nothing new"
        );

        control(&mut shared, ranking(&[]));
        assert!(shared.preference.input.ranking.is_empty());
        assert!(
            shared.lanes.input.needs_rules,
            "following the system again: the Windows rules may choose otherwise"
        );
    }

    #[test]
    fn a_priority_list_for_a_detached_lane_is_kept_for_when_it_is_attached() {
        let (mut shared, _messages) = shared_with_messages();
        control(
            &mut shared,
            UiToAudio::SetDevicePriority {
                direction: DeviceDirection::Input,
                names: vec![LAPTOP_MICROPHONE.to_owned()],
                new_devices_first: false,
            },
        );
        assert_eq!(shared.preference.input.ranking, [LAPTOP_MICROPHONE]);
        assert!(
            !shared.lanes.input.needs_rules,
            "a detached lane runs no rules"
        );
    }

    #[test]
    fn a_pick_stays_fresh_until_a_pair_is_built_on_it_and_no_longer() {
        let (mut shared, _messages) = shared_with_messages();
        add_device(&mut shared, device(57, SPEAKERS, DeviceDirection::Output));
        add_device(&mut shared, device(58, USB_DAC, DeviceDirection::Output));
        control(
            &mut shared,
            UiToAudio::SetDevicePriority {
                direction: DeviceDirection::Output,
                names: vec![USB_DAC.to_owned(), SPEAKERS.to_owned()],
                new_devices_first: false,
            },
        );

        select_device(&mut shared, DeviceDirection::Output, SPEAKERS.to_owned());
        assert!(shared.preference.output.fresh_pick);
        // No server: the rules choose the pick over the ranking and fail to build it, and the
        // retry must honour the pick too.
        apply_rules(&mut shared, DeviceDirection::Output);
        assert!(
            shared.preference.output.fresh_pick,
            "the pair on the pick failed; the pick waits for the retry"
        );

        // A pick nothing is plugged into loses to the ranking, and is not kept for a retry.
        shared.devices[0].available = false;
        apply_rules(&mut shared, DeviceDirection::Output);
        assert!(!shared.preference.output.fresh_pick);

        select_device(&mut shared, DeviceDirection::Output, USB_DAC.to_owned());
        detach_lane(&mut shared, DeviceDirection::Output);
        assert!(
            !shared.preference.output.fresh_pick,
            "a lane switched off forgets the pick it had not honoured"
        );
    }

    #[test]
    fn a_node_whose_port_is_unplugged_goes_silent_and_asks_its_lanes_rules() {
        let (shared, _messages) = with_ucm_card();
        add_device(
            &mut shared.borrow_mut(),
            on_ucm_card(52, UCM_HDMI, DeviceDirection::Output),
        );
        shared.borrow_mut().lanes.output.needs_rules = false;

        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::No),
        );
        assert!(
            heard(&shared, UCM_HDMI),
            "which of the card's devices the node is has not arrived yet"
        );
        assert!(!shared.borrow().lanes.output.needs_rules);

        on_node_profile_device(&shared, 52, Some(2));
        assert!(!heard(&shared, UCM_HDMI));
        assert!(shared.borrow().lanes.output.needs_rules);

        // A monitor plugged in: the card sends its list again, from the start.
        shared.borrow_mut().lanes.output.needs_rules = false;
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::Yes),
        );
        assert!(heard(&shared, UCM_HDMI));
        assert!(shared.borrow().lanes.output.needs_rules);

        // The same list again — a volume change on the port — changes nothing the rules read.
        shared.borrow_mut().lanes.output.needs_rules = false;
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::Yes),
        );
        assert!(!shared.borrow().lanes.output.needs_rules);
    }

    #[test]
    fn a_microphone_going_silent_asks_only_the_input_lanes_rules() {
        let (shared, _messages) = with_ucm_card();
        {
            let mut guard = shared.borrow_mut();
            guard.lanes.input.enabled = true;
            add_device(
                &mut guard,
                on_ucm_card(56, UCM_HEADSET_MIC, DeviceDirection::Input),
            );
            add_device(
                &mut guard,
                on_ucm_card(51, UCM_SPEAKER, DeviceDirection::Output),
            );
            guard.lanes.input.needs_rules = false;
            guard.lanes.output.needs_rules = false;
        }
        on_node_profile_device(&shared, 56, Some(6));
        on_node_profile_device(&shared, 51, Some(1));
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            1,
            active_route(6, 1, routes::Availability::Unknown),
        );
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            6,
            active_route(5, 6, routes::Availability::No),
        );
        assert!(!heard(&shared, UCM_HEADSET_MIC));
        assert!(heard(&shared, UCM_SPEAKER));
        assert!(shared.borrow().lanes.input.needs_rules);
        assert!(
            !shared.borrow().lanes.output.needs_rules,
            "nothing about the speakers changed"
        );
    }

    #[test]
    fn a_node_announced_after_its_cards_routes_is_silent_from_its_first_info() {
        let (shared, _messages) = with_ucm_card();
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::No),
        );
        // `pw-dump`'s view of the node carries its card device; the registry's does not, and the
        // node's info brings it a moment later. Either way it is silent once both are known.
        add_device(
            &mut shared.borrow_mut(),
            DeviceInfo {
                profile_device: Some(2),
                ..on_ucm_card(52, UCM_HDMI, DeviceDirection::Output)
            },
        );
        assert!(!heard(&shared, UCM_HDMI));
    }

    #[test]
    fn routes_from_a_card_that_is_not_listed_are_not_kept() {
        let (shared, _messages) = shared_with_messages();
        let shared = Rc::new(RefCell::new(shared));
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::No),
        );
        assert!(shared.borrow().card_routes.is_empty());
    }

    #[test]
    fn a_card_that_goes_takes_its_routes_with_it() {
        let (shared, _messages) = with_ucm_card();
        add_device(
            &mut shared.borrow_mut(),
            DeviceInfo {
                profile_device: Some(2),
                ..on_ucm_card(52, UCM_HDMI, DeviceDirection::Output)
            },
        );
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::No),
        );
        assert!(!heard(&shared, UCM_HDMI));

        assert!(remove_card(&mut shared.borrow_mut(), UCM_CARD));
        assert!(shared.borrow().card_routes.is_empty());
        assert!(
            heard(&shared, UCM_HDMI),
            "no card speaks for it any more; it goes in the same batch"
        );
    }

    #[test]
    fn a_lost_connection_forgets_every_cards_routes() {
        let (shared, _messages) = with_ucm_card();
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::No),
        );
        assert_eq!(shared.borrow().card_routes.len(), 1);
        shared.borrow_mut().state = State::Connecting;
        disconnect(&mut shared.borrow_mut(), "the server went away");
        assert!(shared.borrow().card_routes.is_empty());
    }

    #[test]
    fn the_rules_remember_as_seen_only_the_devices_they_could_choose() {
        let (shared, _messages) = with_ucm_card();
        {
            let mut guard = shared.borrow_mut();
            add_device(
                &mut guard,
                on_ucm_card(51, UCM_SPEAKER, DeviceDirection::Output),
            );
            add_device(
                &mut guard,
                on_ucm_card(52, UCM_HDMI, DeviceDirection::Output),
            );
        }
        on_node_profile_device(&shared, 51, Some(1));
        on_node_profile_device(&shared, 52, Some(2));
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            1,
            active_route(6, 1, routes::Availability::Unknown),
        );
        on_card_route(
            &shared,
            UCM_CARD,
            RouteList::Active,
            2,
            active_route(0, 2, routes::Availability::No),
        );
        apply_rules(&mut shared.borrow_mut(), DeviceDirection::Output);
        assert_eq!(
            shared.borrow().lanes.output.previous_names,
            [UCM_SPEAKER],
            "so that the HDMI sink, once a monitor is plugged in, arrives as a new device"
        );
    }

    // ---- devices that come back, devices that arrive, rankings that arrive late

    const HDMI_MONITOR: &str = "alsa_output.pci-0000_01_00.1.hdmi-stereo";
    const ALSA_CARD: u32 = 40;
    const ANALOG_STEREO: &str = "alsa_output.pci-0000_00_1f.3.analog-stereo";
    const ANALOG_SURROUND: &str = "alsa_output.pci-0000_00_1f.3.analog-surround-51";
    const ANALOG_HDMI: &str = "alsa_output.pci-0000_00_1f.3.hdmi-stereo";

    /// One run of a lane's rules taken as far as a server would take it: what they chose, with the
    /// pair on it taken as built — committed, as [`apply_rules`] commits a pair that stands.
    fn run_and_commit(shared: &mut Shared, direction: DeviceDirection) -> String {
        let selection = choose(shared, direction)
            .selection
            .expect("a device to attach to");
        devices::commit(shared.memory.get_mut(direction), &selection);
        shared.lanes.get_mut(direction).last_target = Some(selection.target.clone());
        selection.target
    }

    fn rank_outputs(shared: &mut Shared, names: &[&str], new_devices_first: bool) {
        control(
            shared,
            UiToAudio::SetDevicePriority {
                direction: DeviceDirection::Output,
                names: names.iter().map(|&name| name.to_owned()).collect(),
                new_devices_first,
            },
        );
    }

    fn alsa_card() -> Card {
        Card {
            object_id: ALSA_CARD,
            bluez_address: None,
            bluetooth: false,
        }
    }

    fn on_alsa_card(object_id: u32, name: &str) -> DeviceInfo {
        DeviceInfo {
            card_id: Some(ALSA_CARD),
            ..device(object_id, name, DeviceDirection::Output)
        }
    }

    /// Speakers the user picked, and a Bluetooth headset ranked above them beside them.
    fn speakers_picked_beside_a_better_ranked_headset() -> (Shared, Receiver<AudioToUi>) {
        let (mut shared, messages) = shared_with_messages();
        shared.cards.push(headset_card());
        add_device(&mut shared, headset_sink(70, 70));
        add_device(&mut shared, device(57, SPEAKERS, DeviceDirection::Output));
        rank_outputs(&mut shared, &[HEADSET_SINK, SPEAKERS], false);
        select_device(&mut shared, DeviceDirection::Output, SPEAKERS.to_owned());
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            SPEAKERS
        );
        (shared, messages)
    }

    #[test]
    fn a_sink_that_leaves_with_its_card_and_comes_back_is_not_new_to_a_lane_that_is_not_on_it() {
        let (mut shared, _messages) = speakers_picked_beside_a_better_ranked_headset();

        // Something records from the headset's microphone: WirePlumber switches it to its call
        // profile, and its sink goes while its card stays.
        remove_device(&mut shared, 70);
        assert_eq!(shared.lanes.output.hold, None, "the lane is not on it");
        // The supervisor's tick runs the rules while it is away.
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            SPEAKERS
        );
        assert!(
            shared
                .lanes
                .output
                .previous_names
                .contains(&HEADSET_SINK.to_owned()),
            "the headset is still counted as seen"
        );

        // About a second later it is back, under the same name, as a new node.
        add_device(&mut shared, headset_sink(74, 74));
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            SPEAKERS,
            "back from a profile switch, the headset is no arrival: the speakers the user picked \
             keep the music, which would otherwise move to the headset in mono at 16 kHz"
        );
    }

    #[test]
    fn a_sink_back_from_a_profile_switch_is_no_newcomer_to_the_windows_rules_either() {
        let (mut shared, _messages) = shared_with_messages();
        shared.cards.push(headset_card());
        add_device(&mut shared, headset_sink(70, 70));
        add_device(&mut shared, device(57, SPEAKERS, DeviceDirection::Output));
        // Nothing ranked and nothing picked: the lane follows the system's default.
        shared.defaults.output.current = Some(SPEAKERS.to_owned());
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            SPEAKERS
        );
        remove_device(&mut shared, 70);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            SPEAKERS
        );
        add_device(&mut shared, headset_sink(74, 74));
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            SPEAKERS,
            "rule 5 takes a device just plugged in, not one back from a profile switch"
        );
    }

    #[test]
    fn a_sink_away_for_longer_than_the_wait_or_gone_with_its_card_arrives_when_it_comes() {
        for gone_for_good in [false, true] {
            let (mut shared, _messages) = speakers_picked_beside_a_better_ranked_headset();
            remove_device(&mut shared, 70);
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                SPEAKERS
            );
            if gone_for_good {
                // Switched off: its card goes after its sink.
                assert!(remove_card(&mut shared, HEADSET_CARD));
                assert!(shared.lanes.output.departures.is_empty());
                shared.cards.push(headset_card());
            } else {
                let departure = &mut shared.lanes.output.departures[0];
                departure.left = Instant::now()
                    .checked_sub(RETURN_WAIT + Duration::from_millis(1))
                    .expect("a clock that has run this long");
            }
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                SPEAKERS
            );
            assert!(
                !shared
                    .lanes
                    .output
                    .previous_names
                    .contains(&HEADSET_SINK.to_owned()),
                "no longer expected back"
            );
            add_device(&mut shared, headset_sink(74, 74));
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                HEADSET_SINK,
                "switched on again, it arrives, and is ranked above the speakers \
                 (gone with its card: {gone_for_good})"
            );
        }
    }

    #[test]
    fn a_node_that_goes_with_no_card_left_behind_is_never_expected_back() {
        let (mut shared, _messages) = shared_with_messages();
        add_device(&mut shared, headset_sink(70, 70));
        remove_device(&mut shared, 70);
        assert!(shared.lanes.output.departures.is_empty());

        shared.cards.push(headset_card());
        add_device(&mut shared, headset_sink(71, 71));
        remove_device(&mut shared, 71);
        assert_eq!(shared.lanes.output.departures.len(), 1);
        shared.lanes.output.previous_names = vec!["stale".to_owned()];
        disconnect_for_tests(&mut shared);
        assert!(
            shared.lanes.output.departures.is_empty(),
            "a lost connection forgets the graph, departures and all"
        );
    }

    /// Take the connection down, as a core error would.
    fn disconnect_for_tests(shared: &mut Shared) {
        shared.state = State::Connecting;
        disconnect(shared, "the server went away");
    }

    #[test]
    fn a_node_of_the_held_card_under_another_name_ends_the_wait_on_the_next_tick() {
        let (mut shared, _messages) = shared_with_messages();
        shared.cards.push(alsa_card());
        add_device(&mut shared, on_alsa_card(60, ANALOG_STEREO));
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            ANALOG_STEREO
        );

        // The user switches the card to 5.1 in pavucontrol: the stereo node goes, the card stays,
        // and the surround node arrives in the same batch.
        remove_device(&mut shared, 60);
        assert!(shared.lanes.output.hold.is_some(), "the lane waits");
        add_device(&mut shared, on_alsa_card(61, ANALOG_SURROUND));

        assert!(
            !held(&mut shared, DeviceDirection::Output, Instant::now()),
            "the stereo node is not coming back: the lane is not left silent for {RETURN_WAIT:?}"
        );
        assert_eq!(shared.lanes.output.hold, None);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            ANALOG_SURROUND
        );
    }

    #[test]
    fn a_node_renamed_under_a_ranking_ends_the_wait_and_takes_its_place_in_the_ranking() {
        // (ranking, new devices first, what the lane is on once the wait ends)
        let cases: [(&[&str], bool, &str); 4] = [
            // The renamed node is not ranked and goes after every ranked device: the best-ranked
            // one listed, on another card, takes the lane.
            (&[ANALOG_STEREO, USB_DAC], false, USB_DAC),
            // New devices first: the renamed node counts as just plugged in and goes first.
            (&[ANALOG_STEREO, USB_DAC], true, ANALOG_SURROUND),
            // Ranked by the user above the other card's device, it takes the lane by its place.
            (
                &[ANALOG_SURROUND, ANALOG_STEREO, USB_DAC],
                false,
                ANALOG_SURROUND,
            ),
            // Nothing else is listed: the renamed node is all there is.
            (&[ANALOG_STEREO], false, ANALOG_SURROUND),
        ];
        for (ranking, new_devices_first, expected) in cases {
            let (mut shared, _messages) = shared_with_messages();
            shared.cards.push(alsa_card());
            add_device(&mut shared, on_alsa_card(60, ANALOG_STEREO));
            if ranking.contains(&USB_DAC) {
                add_device(&mut shared, device(70, USB_DAC, DeviceDirection::Output));
            }
            rank_outputs(&mut shared, ranking, new_devices_first);
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                ANALOG_STEREO
            );

            // The user switches the card to 5.1 in pavucontrol.
            remove_device(&mut shared, 60);
            assert!(shared.lanes.output.hold.is_some(), "the lane waits");
            add_device(&mut shared, on_alsa_card(61, ANALOG_SURROUND));

            assert!(
                !held(&mut shared, DeviceDirection::Output, Instant::now()),
                "the stereo node is not coming back, whatever the ranking ({ranking:?})"
            );
            assert_eq!(shared.lanes.output.hold, None);
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                expected,
                "ranking {ranking:?}, new devices first: {new_devices_first}"
            );
        }
    }

    #[test]
    fn a_card_that_puts_its_other_nodes_back_under_their_own_names_keeps_the_wait() {
        let (mut shared, _messages) = shared_with_messages();
        shared.cards.push(alsa_card());
        add_device(&mut shared, on_alsa_card(60, ANALOG_STEREO));
        add_device(&mut shared, on_alsa_card(62, ANALOG_HDMI));
        shared.memory.output.user_selected = ANALOG_STEREO.to_owned();
        shared.preference.output.fresh_pick = true;
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            ANALOG_STEREO
        );

        // A profile applied again: every node of the card goes and comes back, one by one.
        remove_device(&mut shared, 60);
        let hold = shared.lanes.output.hold.clone().expect("the lane waits");
        assert_eq!(hold.known, [ANALOG_HDMI]);
        remove_device(&mut shared, 62);
        add_device(&mut shared, on_alsa_card(64, ANALOG_HDMI));
        assert!(
            held(&mut shared, DeviceDirection::Output, Instant::now()),
            "the HDMI node back under its own name is not the lane's node renamed"
        );
        // Another card's node arriving is not either.
        add_device(&mut shared, device(65, USB_DAC, DeviceDirection::Output));
        assert!(held(&mut shared, DeviceDirection::Output, Instant::now()));

        add_device(&mut shared, on_alsa_card(66, ANALOG_STEREO));
        assert!(!held(&mut shared, DeviceDirection::Output, Instant::now()));
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            ANALOG_STEREO
        );
    }

    #[test]
    fn a_card_that_removes_the_lanes_node_after_another_still_waits_for_its_own() {
        for ranked in [false, true] {
            let (mut shared, _messages) = shared_with_messages();
            shared.cards.push(alsa_card());
            add_device(&mut shared, on_alsa_card(60, ANALOG_STEREO));
            add_device(&mut shared, on_alsa_card(62, ANALOG_HDMI));
            if ranked {
                rank_outputs(&mut shared, &[ANALOG_STEREO, ANALOG_HDMI], false);
            } else {
                shared.memory.output.user_selected = ANALOG_STEREO.to_owned();
                shared.preference.output.fresh_pick = true;
            }
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                ANALOG_STEREO
            );

            // A profile applied again: ACP removes the card's nodes in the order of its devices,
            // the HDMI node before the lane's, and a supervisor tick may fall between the two.
            remove_device(&mut shared, 62);
            assert_eq!(shared.lanes.output.hold, None, "the lane is not on it");
            assert!(!held(&mut shared, DeviceDirection::Output, Instant::now()));
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                ANALOG_STEREO
            );
            remove_device(&mut shared, 60);
            let hold = shared.lanes.output.hold.clone().expect("the lane waits");
            assert_eq!(
                hold.known,
                [ANALOG_HDMI],
                "gone a moment before, the HDMI node is still one the card had (ranked: {ranked})"
            );

            // It adds them back in the same order, and a tick falls before the lane's own.
            add_device(&mut shared, on_alsa_card(64, ANALOG_HDMI));
            assert!(
                held(&mut shared, DeviceDirection::Output, Instant::now()),
                "the HDMI node back under its own name is not the lane's node renamed, whichever \
                 went first (ranked: {ranked})"
            );
            assert!(shared.lanes.output.hold.is_some());

            add_device(&mut shared, on_alsa_card(66, ANALOG_STEREO));
            assert!(!held(&mut shared, DeviceDirection::Output, Instant::now()));
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                ANALOG_STEREO,
                "the lane is back on its own node, never having been moved (ranked: {ranked})"
            );
        }
    }

    #[test]
    fn a_hold_knows_the_nodes_its_card_removed_just_before_and_no_others() {
        const IEC958: &str = "alsa_output.pci-0000_00_1f.3.iec958-stereo";
        let now = Instant::now();
        let went = |name: &str, card_id: Option<u32>, ago: Duration| Departure {
            name: name.to_owned(),
            left: now
                .checked_sub(ago)
                .expect("a clock that has run this long"),
            back: None,
            card_id,
            bluez_address: None,
        };
        let departed = [
            // Went a moment ago from the same card: known.
            went(IEC958, Some(ALSA_CARD), Duration::from_millis(3)),
            // Went and came back, and is listed as well: known once.
            Departure {
                back: Some(now),
                ..went(ANALOG_HDMI, Some(ALSA_CARD), Duration::from_millis(400))
            },
            // Went longer ago than a node is expected back: gone, and a newcomer if it comes.
            went(
                ANALOG_SURROUND,
                Some(ALSA_CARD),
                RETURN_WAIT + Duration::from_millis(1),
            ),
            // Went from another card: nothing to do with this one.
            went(USB_DAC, Some(ALSA_CARD + 1), Duration::from_millis(3)),
            // The node itself, gone and back once before: what the lane waits for, not a sibling.
            went(ANALOG_STEREO, Some(ALSA_CARD), Duration::from_millis(900)),
        ];
        let hold = Hold::after_removal(
            &on_alsa_card(60, ANALOG_STEREO),
            Some(ANALOG_STEREO),
            &[alsa_card()],
            &[on_alsa_card(62, ANALOG_HDMI)],
            &departed,
            None,
            now,
        )
        .expect("waited for");
        assert_eq!(hold.known, [ANALOG_HDMI, IEC958]);

        // A headset that names no card is tied to what went from it by its address.
        let unnumbered = DeviceInfo {
            card_id: None,
            ..headset_sink(70, 70)
        };
        let head_unit = Departure {
            bluez_address: Some(HEADSET_ADDRESS.to_owned()),
            ..went(
                "bluez_output.00_11_22_33_44_55.headset-head-unit",
                None,
                Duration::from_millis(3),
            )
        };
        let hold = Hold::after_removal(
            &unnumbered,
            Some(HEADSET_SINK),
            &[headset_card()],
            &[],
            &[head_unit, went(IEC958, None, Duration::from_millis(3))],
            None,
            now,
        )
        .expect("waited for");
        assert_eq!(
            hold.known,
            ["bluez_output.00_11_22_33_44_55.headset-head-unit"],
            "a node that went from no known card is nobody's sibling"
        );
    }

    #[test]
    fn a_headsets_rename_is_told_by_its_address_when_it_names_no_card() {
        let hold = Hold {
            target: HEADSET_SINK.to_owned(),
            until: Instant::now() + RETURN_WAIT,
            card_id: None,
            bluez_address: Some(HEADSET_ADDRESS.to_owned()),
            known: Vec::new(),
        };
        let renamed = DeviceInfo {
            card_id: None,
            bluez_address: Some(HEADSET_ADDRESS.to_owned()),
            ..device(
                80,
                "bluez_output.00_11_22_33_44_55.headset-head-unit",
                DeviceDirection::Output,
            )
        };
        let microphone = DeviceInfo {
            card_id: None,
            bluez_address: Some(HEADSET_ADDRESS.to_owned()),
            ..device(81, "bluez_input.00:11:22:33:44:55", DeviceDirection::Input)
        };
        let unknown = device(82, "alsa_output.unknown", DeviceDirection::Output);
        let devices = [microphone, unknown, renamed];
        assert_eq!(
            hold.renamed(&devices, DeviceDirection::Output)
                .map(|device| device.name.as_str()),
            Some("bluez_output.00_11_22_33_44_55.headset-head-unit"),
            "the microphone is the other lane's, and a node on no known card is nobody's sibling"
        );
    }

    #[test]
    fn a_device_plugged_in_goes_first_while_the_app_puts_new_devices_first() {
        for new_devices_first in [true, false] {
            let (mut shared, _messages) = shared_with_messages();
            add_device(&mut shared, device(57, SPEAKERS, DeviceDirection::Output));
            rank_outputs(&mut shared, &[SPEAKERS], new_devices_first);
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                SPEAKERS
            );

            // A USB DAC is plugged in. The rules run on its arrival before the app has ranked it.
            add_device(&mut shared, device(58, USB_DAC, DeviceDirection::Output));
            let expected = if new_devices_first { USB_DAC } else { SPEAKERS };
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                expected,
                "new devices first: {new_devices_first}"
            );

            // The app's list, with the DAC where its setting put it, changes nothing more.
            let ranking = if new_devices_first {
                [USB_DAC, SPEAKERS]
            } else {
                [SPEAKERS, USB_DAC]
            };
            rank_outputs(&mut shared, &ranking, new_devices_first);
            assert_eq!(
                run_and_commit(&mut shared, DeviceDirection::Output),
                expected
            );
        }
    }

    #[test]
    fn where_new_devices_go_is_news_for_the_rules_even_with_the_same_ranking() {
        let (mut shared, _messages) = shared_with_messages();
        rank_outputs(&mut shared, &[SPEAKERS], false);
        shared.lanes.output.needs_rules = false;
        rank_outputs(&mut shared, &[SPEAKERS], false);
        assert!(!shared.lanes.output.needs_rules, "nothing changed");
        rank_outputs(&mut shared, &[SPEAKERS], true);
        assert!(shared.preference.output.new_devices_first);
        assert!(shared.lanes.output.needs_rules);
    }

    #[test]
    fn a_saved_device_announced_again_after_a_profile_switch_does_not_beat_the_ranking() {
        let (mut shared, _messages) = shared_with_messages();
        shared.cards.push(headset_card());
        add_device(&mut shared, headset_sink(70, 70));
        rank_outputs(&mut shared, &[USB_DAC, HEADSET_SINK], false);
        // The user picked the headset once; the app announces it as the saved device.
        select_device(
            &mut shared,
            DeviceDirection::Output,
            HEADSET_SINK.to_owned(),
        );
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            HEADSET_SINK
        );
        // The DAC, ranked above it, is plugged in and takes the lane.
        add_device(&mut shared, device(58, USB_DAC, DeviceDirection::Output));
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            USB_DAC
        );

        // The headset switches profile: its sink goes and comes back.
        remove_device(&mut shared, 70);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            USB_DAC
        );
        add_device(&mut shared, headset_sink(74, 74));
        // The app sees its saved device listed again and announces it.
        select_device(
            &mut shared,
            DeviceDirection::Output,
            HEADSET_SINK.to_owned(),
        );
        assert!(
            !shared.preference.output.fresh_pick,
            "an announcement of the pick that went and came back is no new pick"
        );
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            USB_DAC,
            "the better-ranked DAC keeps the lane"
        );

        // Once the headset has been back a while, choosing it is a pick again.
        shared.lanes.output.departures[0].back = Instant::now().checked_sub(RETURN_WAIT);
        select_device(
            &mut shared,
            DeviceDirection::Output,
            HEADSET_SINK.to_owned(),
        );
        assert!(shared.preference.output.fresh_pick);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            HEADSET_SINK
        );
    }

    #[test]
    fn a_device_picked_for_the_first_time_or_to_switch_a_lane_on_is_always_a_pick() {
        let (mut shared, _messages) = shared_with_messages();
        shared.cards.push(headset_card());
        add_device(&mut shared, headset_sink(70, 70));
        remove_device(&mut shared, 70);
        add_device(&mut shared, headset_sink(74, 74));
        select_device(
            &mut shared,
            DeviceDirection::Output,
            HEADSET_SINK.to_owned(),
        );
        assert!(
            shared.preference.output.fresh_pick,
            "a device just back but never picked before is a pick"
        );

        shared.preference.output.fresh_pick = false;
        detach_lane(&mut shared, DeviceDirection::Output);
        select_device(
            &mut shared,
            DeviceDirection::Output,
            HEADSET_SINK.to_owned(),
        );
        assert!(
            shared.preference.output.fresh_pick,
            "the same device switching the lane back on is a pick"
        );
    }

    #[test]
    fn a_ranking_given_at_start_makes_the_first_choice_rather_than_the_session_default() {
        let (mut shared, _messages) = shared_with_messages();
        start_ranked(
            &mut shared,
            PerDirection {
                output: crate::DevicePriority {
                    names: vec![USB_DAC.to_owned(), HDMI_MONITOR.to_owned()],
                    new_devices_first: true,
                },
                input: crate::DevicePriority::default(),
            },
        );
        assert!(shared.preference.output.new_devices_first);
        assert!(shared.preference.input.ranking.is_empty());
        add_device(
            &mut shared,
            device(52, HDMI_MONITOR, DeviceDirection::Output),
        );
        add_device(&mut shared, device(58, USB_DAC, DeviceDirection::Output));
        shared.defaults.output.current = Some(HDMI_MONITOR.to_owned());
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            USB_DAC,
            "the ranking's first device, not the session default rule 2 would adopt"
        );
    }

    #[test]
    fn a_ranking_that_arrives_after_the_first_choice_takes_the_lane_from_the_session_default() {
        let (mut shared, _messages) = shared_with_messages();
        add_device(
            &mut shared,
            device(52, HDMI_MONITOR, DeviceDirection::Output),
        );
        add_device(&mut shared, device(58, USB_DAC, DeviceDirection::Output));
        shared.defaults.output.current = Some(HDMI_MONITOR.to_owned());
        // The registry was read before the app's ranking arrived: rule 2 adopts the default.
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            HDMI_MONITOR
        );

        rank_outputs(&mut shared, &[USB_DAC, HDMI_MONITOR], false);
        assert!(shared.preference.output.start_over);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            USB_DAC,
            "the ranking chooses as it would have at start"
        );
        assert!(!shared.preference.output.start_over, "once");

        // A reorder afterwards moves nothing, as upstream's list does not.
        rank_outputs(&mut shared, &[HDMI_MONITOR, USB_DAC], false);
        assert!(!shared.preference.output.start_over);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            USB_DAC
        );
    }

    #[test]
    fn a_ranking_that_arrives_after_the_user_picked_leaves_the_pick_where_it_is() {
        let (mut shared, _messages) = shared_with_messages();
        add_device(
            &mut shared,
            device(52, HDMI_MONITOR, DeviceDirection::Output),
        );
        add_device(&mut shared, device(58, USB_DAC, DeviceDirection::Output));
        select_device(
            &mut shared,
            DeviceDirection::Output,
            HDMI_MONITOR.to_owned(),
        );
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            HDMI_MONITOR
        );
        rank_outputs(&mut shared, &[USB_DAC, HDMI_MONITOR], false);
        assert!(!shared.preference.output.start_over);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            HDMI_MONITOR
        );
    }

    #[test]
    fn a_ranking_whose_first_choice_fails_to_build_chooses_by_rank_again_on_the_retry() {
        let (mut shared, _messages) = shared_with_messages();
        add_device(
            &mut shared,
            device(52, HDMI_MONITOR, DeviceDirection::Output),
        );
        add_device(&mut shared, device(58, USB_DAC, DeviceDirection::Output));
        shared.defaults.output.current = Some(HDMI_MONITOR.to_owned());
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            HDMI_MONITOR
        );
        rank_outputs(&mut shared, &[USB_DAC, HDMI_MONITOR], false);
        // No server: the pair on the DAC fails.
        apply_rules(&mut shared, DeviceDirection::Output);
        assert!(shared.preference.output.start_over);
        assert_eq!(
            run_and_commit(&mut shared, DeviceDirection::Output),
            USB_DAC
        );
    }

    // ---- one sink, two ports: the volume per port

    const SPEAKER_PORT: &str = "analog-output-speaker";
    const HEADPHONE_PORT: &str = "analog-output-headphones";

    fn on_port(port: &str, entry: TargetVolume) -> TargetVolume {
        TargetVolume {
            port: port.to_owned(),
            ..entry
        }
    }

    #[test]
    fn plugging_headphones_into_the_speakers_sink_remembers_the_speakers_and_restores_the_headphones()
     {
        let (mut shared, messages) = shared_with_messages();
        shared.target_volumes = vec![on_port(
            HEADPHONE_PORT,
            remembered(DeviceDirection::Output, ANALOG_STEREO, &[0.2, 0.2]),
        )];
        // The quiet built-in speakers, turned all the way up on FxSound's slider.
        let speakers = at(&[1.0, 1.0]);

        let volume = volume_on_port(
            &mut shared,
            DeviceDirection::Output,
            ANALOG_STEREO,
            (Some(SPEAKER_PORT), HEADPHONE_PORT),
            &speakers,
            2,
            false,
        )
        .expect("the port changed, so the volume does");
        assert_eq!(
            volume.effective(2),
            [0.2, 0.2],
            "the headphones' own level, not the speakers' full scale"
        );
        assert_eq!(
            volume_reports(&messages),
            [on_port(
                SPEAKER_PORT,
                remembered(DeviceDirection::Output, ANALOG_STEREO, &[1.0, 1.0])
            )],
            "the speakers' level is remembered for the speakers, for when the headphones come out"
        );
        assert_eq!(shared.target_volumes.len(), 2, "one entry per port");
        assert_eq!(shared.lanes.output.last_volume, Some(speakers));

        // Out again: back on the speakers, at theirs.
        let volume = volume_on_port(
            &mut shared,
            DeviceDirection::Output,
            ANALOG_STEREO,
            (Some(HEADPHONE_PORT), SPEAKER_PORT),
            &volume,
            2,
            false,
        )
        .expect("moved back");
        assert_eq!(volume.effective(2), [1.0, 1.0]);
    }

    #[test]
    fn a_port_never_heard_before_starts_no_louder_than_the_one_the_lane_leaves() {
        let (mut shared, _messages) = shared_with_messages();
        let volume = volume_on_port(
            &mut shared,
            DeviceDirection::Output,
            ANALOG_STEREO,
            (Some(SPEAKER_PORT), HEADPHONE_PORT),
            &at(&[0.4, 0.4]),
            2,
            false,
        )
        .expect("moved");
        assert_eq!(volume.effective(2), [0.4, 0.4]);
        let muted = NodeVolume {
            mute: true,
            ..at(&[0.4, 0.4])
        };
        let volume = volume_on_port(
            &mut shared,
            DeviceDirection::Output,
            ANALOG_STEREO,
            (Some(HEADPHONE_PORT), "analog-output-lineout"),
            &muted,
            2,
            false,
        )
        .expect("moved");
        assert!(volume.mute, "a mute carries over to a port never heard");
    }

    #[test]
    fn a_port_named_only_after_the_pair_was_built_changes_the_volume_only_if_nothing_moved_it() {
        let (mut shared, messages) = shared_with_messages();
        shared.target_volumes = vec![on_port(
            HEADPHONE_PORT,
            remembered(DeviceDirection::Output, ANALOG_STEREO, &[0.2, 0.2]),
        )];
        let started = at(&[0.7, 0.7]);
        assert_eq!(
            volume_on_port(
                &mut shared,
                DeviceDirection::Output,
                ANALOG_STEREO,
                (None, HEADPHONE_PORT),
                &started,
                2,
                true,
            )
            .map(|volume| volume.effective(2)),
            Some(vec![0.2, 0.2]),
            "the port's own level, for a pair nobody has moved"
        );
        assert_eq!(
            volume_on_port(
                &mut shared,
                DeviceDirection::Output,
                ANALOG_STEREO,
                (None, HEADPHONE_PORT),
                &started,
                2,
                false,
            ),
            None,
            "a level the user has set since is kept"
        );
        assert_eq!(
            volume_on_port(
                &mut shared,
                DeviceDirection::Output,
                ANALOG_STEREO,
                (None, SPEAKER_PORT),
                &started,
                2,
                true,
            ),
            None,
            "a port with nothing remembered keeps the pair where it started"
        );
        assert!(
            volume_reports(&messages).is_empty(),
            "nothing is remembered for a port the pair never knew it was on"
        );
        assert_eq!(shared.lanes.output.last_volume, None);
    }

    #[test]
    fn a_new_pair_starts_at_the_level_of_the_port_its_device_is_on() {
        let (shared, _messages) = with_ucm_card();
        {
            let mut guard = shared.borrow_mut();
            add_device(
                &mut guard,
                DeviceInfo {
                    profile_device: Some(1),
                    ..on_ucm_card(51, UCM_SPEAKER, DeviceDirection::Output)
                },
            );
            guard.target_volumes = vec![
                on_port(
                    HEADPHONE_PORT,
                    remembered(DeviceDirection::Output, UCM_SPEAKER, &[0.2, 0.2]),
                ),
                on_port(
                    SPEAKER_PORT,
                    remembered(DeviceDirection::Output, UCM_SPEAKER, &[0.9, 0.9]),
                ),
            ];
        }
        let port_now = |shared: &Rc<RefCell<Shared>>| {
            let guard = shared.borrow();
            let device = guard.devices[0].clone();
            target_port(&guard, &device)
        };
        assert_eq!(port_now(&shared), None, "the card has not named it yet");
        assert_eq!(
            volume_for_pair(
                &shared.borrow(),
                DeviceDirection::Output,
                UCM_SPEAKER,
                None,
                2
            )
            .effective(2),
            [0.2, 0.2],
            "with no port known, no port's level: the never-raise rule, from the quietest"
        );

        for (port, level) in [(HEADPHONE_PORT, 0.2), (SPEAKER_PORT, 0.9)] {
            on_card_route(
                &shared,
                UCM_CARD,
                RouteList::Active,
                1,
                Route {
                    name: Some(port.to_owned()),
                    ..active_route(6, 1, routes::Availability::Yes)
                },
            );
            let known = port_now(&shared);
            assert_eq!(known.as_deref(), Some(port));
            assert_eq!(
                volume_for_pair(
                    &shared.borrow(),
                    DeviceDirection::Output,
                    UCM_SPEAKER,
                    known.as_deref(),
                    2
                )
                .effective(2),
                [level, level],
                "{port}"
            );
        }
    }

    #[test]
    fn a_new_pair_does_not_take_up_a_fade_asked_of_the_pair_before_it() {
        let shared = shared_with_dsp_for_tests();
        shared.lanes.output.volume.request_fade();
        shared.lanes.output.volume.request_fade();
        let mut shared = shared;
        let dsp = shared.lanes.output.dsp.take().expect("on the main loop");
        let data = SinkData::new(&shared, dsp);
        assert_eq!(data.fades_seen, 2, "it fades in as a new pair, once");
        drop(data);
        drain_recycled_dsp(&mut shared);
        assert!(shared.lanes.output.dsp.is_some());
    }
}
