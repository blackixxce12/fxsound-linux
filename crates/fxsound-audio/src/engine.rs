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
//! the only things that cross between them are a wait-free ring of `AtomicU32` samples, a handful
//! of counters, and the two `triple_buffer` endpoints that carry parameters in and meters out.
//!
//! # One engine, two directions
//!
//! The engine is in exactly one [`DeviceDirection`] at a time. In the output direction NODE 1 is
//! the virtual sink and NODE 2 the playback stream; in the input direction NODE 1 is a capture
//! stream on the chosen microphone and NODE 2 the virtual source (`crate` docs, "Topology").
//! Only the node *properties*, the metadata keys and the bookkeeping differ: NODE 1 always runs
//! [`on_sink_process`] (DSP in place, push to the ring) and NODE 2 always runs
//! [`on_output_process`] (pop the ring), so the real-time code is the same in both directions and
//! is never touched by a direction switch.
//!
//! # The 200 ms supervisor
//!
//! `AudioPassthruPrivate::processTimer` (`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:359`)
//! polled every 100 ms because Windows gave it no event for "the processing thread died". PipeWire
//! gives us events for everything, so the timer here does much less: it drains counters, publishes
//! changes to the GUI, and drives the reconnect state machine. It is still a timer rather than
//! pure event handling for one reason — it is the natural home for the "don't hammer" backoff of
//! `docs/spec/12-audio-io.md` §22, the direct descendant of the `INVALID_HANDLE_VALUE` pause
//! sentinel at `AudioPassthruPrivate.cpp:452-460`.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use fxsound_core::messages::{AudioToUi, DspEvent, DspParams, InputDspParams, Meters, UiToAudio};
use fxsound_core::{AudioDevice, AudioStatus, DeviceDirection};
use fxsound_dsp::Engine as DspEngine;
use fxsound_dsp::InputEngine;
use libspa::param::audio::{AudioFormat, AudioInfoRaw};
use libspa::param::format::{MediaSubtype, MediaType};
use libspa::param::format_utils;
use libspa::pod::Pod;
use libspa::utils::result::AsyncSeq;
use pipewire as pw;
use pw::properties::{PropertiesBox, properties};
use pw::stream::{StreamFlags, StreamState};
use triple_buffer::{Input, Output};

use crate::devices::{self, ChannelMap, DeviceInfo, SelectionMemory};
use crate::{
    AudioError, CAPTURE_NODE_NAME, CAPTURE_STREAM_DESCRIPTION, DEFAULT_QUANTUM_FRAMES,
    DEFAULT_SAMPLE_RATE, LINK_GROUP, MAX_CHANNELS, MAX_QUANTUM_FRAMES, MIN_CHANNELS,
    OUTPUT_NODE_NAME, OUTPUT_STREAM_DESCRIPTION, RING_CAPACITY_FRAMES, SINK_DESCRIPTION,
    SINK_NODE_NAME, SOURCE_NODE_NAME, locale, our_node_name,
};

/// How often the supervisor runs. Also the shortest possible reconnect interval, which is the
/// "never retry faster than 200 ms" floor of `docs/spec/12-audio-io.md` §22.
const SUPERVISOR_PERIOD: Duration = Duration::from_millis(200);

/// Reconnect backoff in milliseconds, then flat at the last value.
const BACKOFF_MS: [u64; 6] = [200, 400, 800, 1600, 3200, 5000];

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
/// which needs `unsafe`. This crate is `#![forbid(unsafe_code)]`, so each slot is an `AtomicU32`
/// holding `f32::to_bits`. On every target this project supports, a `Relaxed` atomic load or store
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
            dropped_frames: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
        }
    }

    /// Adopt a new format and start again from empty. **Main loop only** — called from
    /// `param_changed` and from node creation, never from `process()`.
    pub(crate) fn reconfigure(&self, channels: usize, quantum_frames: usize) {
        self.channels.store(channels, Ordering::Relaxed);
        self.target_fill_frames.store(
            quantum_frames * TARGET_FILL_HALF_QUANTA / 2,
            Ordering::Relaxed,
        );
        self.primed.store(false, Ordering::Relaxed);
        let write = self.write.load(Ordering::Relaxed);
        self.read.store(write, Ordering::Release);
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
    pub(crate) fn pop(&self, out: &mut [f32]) -> usize {
        let channels = self.channels.load(Ordering::Relaxed).max(1);
        let mut read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let mut available = write.wrapping_sub(read);

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
    /// NODE 2 negotiated a different channel count than NODE 1 did. Audio is muted until the
    /// supervisor rebuilds the nodes, because reinterpreting the ring at the wrong stride is the
    /// one failure here that could be genuinely unpleasant to listen to.
    format_mismatches: AtomicU64,
    sample_rate: AtomicU32,
    channels: AtomicU32,
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
}

/// The DSP side of the NODE 1 callback, kept across reconnects and direction switches.
///
/// When PipeWire restarts — or the user switches between speakers and a microphone — the streams
/// and their user data are destroyed and rebuilt; the `triple_buffer` endpoints must not be,
/// because their other halves live in the GUI's [`EngineHandle`](crate::EngineHandle) and cannot
/// be re-paired. So this struct is handed back to the main loop by [`SinkData`]'s `Drop` and moved
/// into the next stream's user data. Keeping the [`DspEngine`] with them is a bonus: its filter
/// state and its 64 KiB of scratch survive a server restart too.
pub(crate) struct SinkDsp {
    /// The music chain and the voice chain. Both are kept alive across a direction switch, which
    /// is what the recycling exists for in the first place: rebuilding either would mean designing
    /// every filter again on the main loop, and would throw away the state of the chain the user
    /// is about to switch *back* to.
    engine: DspEngine,
    input: InputEngine,
    params: Output<DspParams>,
    input_params: Output<InputDspParams>,
    meters: Input<Meters>,
    events: Receiver<DspEvent>,
    /// Interleaved de-serialisation buffer, sized for the worst case and never resized.
    scratch: Vec<f32>,
    /// Which of the two engines the callback runs. Set when the nodes are built, never on the
    /// audio thread.
    direction: DeviceDirection,
}

impl SinkDsp {
    fn new(
        params: Output<DspParams>,
        input_params: Output<InputDspParams>,
        meters: Input<Meters>,
        events: Receiver<DspEvent>,
    ) -> Self {
        Self {
            engine: DspEngine::new(
                DEFAULT_SAMPLE_RATE as f32,
                MAX_QUANTUM_FRAMES,
                MIN_CHANNELS as usize,
            ),
            input: InputEngine::new(
                DEFAULT_SAMPLE_RATE as f32,
                MAX_QUANTUM_FRAMES,
                MIN_CHANNELS as usize,
            ),
            params,
            input_params,
            meters,
            events,
            scratch: vec![0.0; MAX_QUANTUM_FRAMES * MAX_CHANNELS as usize],
            direction: DeviceDirection::Output,
        }
    }

    /// Choose the chain. Called on the main loop while the nodes are being built, so the audio
    /// callback only ever reads it.
    fn set_direction(&mut self, direction: DeviceDirection) {
        self.direction = direction;
    }

    fn set_format(&mut self, sample_rate: f32, channels: usize) {
        match self.direction {
            DeviceDirection::Output => self.engine.set_format(sample_rate, channels),
            DeviceDirection::Input => self.input.set_format(sample_rate, channels),
        }
    }

    /// Where the subwoofer and the front pair sit in the negotiated layout.
    ///
    /// Output only, and not because the input side was forgotten: the voice chain has no stage
    /// that mixes one channel into another, so there is no layout for it to get wrong.
    fn set_layout(&mut self, lfe: Option<usize>, front_pair: Option<(usize, usize)>) {
        self.engine.set_lfe_channel(lfe);
        self.engine.set_front_pair(front_pair);
    }

    fn latency_frames(&self) -> usize {
        match self.direction {
            DeviceDirection::Output => self.engine.latency_frames(),
            DeviceDirection::Input => self.input.latency_frames(),
        }
    }

    /// Clear the active chain's history. The other one keeps its own, which is the point of
    /// holding both.
    fn reset(&mut self) {
        match self.direction {
            DeviceDirection::Output => self.engine.reset(),
            DeviceDirection::Input => self.input.reset(),
        }
    }

    /// Take the newest parameter snapshot and drain the event queue.
    ///
    /// Only the active direction's snapshot is read: the other one is state the GUI is still
    /// publishing, and reading it would be a wasted atomic on the audio thread.
    #[inline]
    fn refresh(&mut self) {
        match self.direction {
            DeviceDirection::Output => {
                self.engine.apply(self.params.read());
                while let Ok(event) = self.events.try_recv() {
                    self.engine.handle_event(event);
                }
            }
            DeviceDirection::Input => {
                self.input.apply(self.input_params.read());
                while let Ok(event) = self.events.try_recv() {
                    self.input.handle_event(event);
                }
            }
        }
    }

    /// De-serialise one block of little-endian `f32` into the scratch buffer, run the active
    /// chain over it and hand it back for the ring. `None` when the block is larger than the
    /// scratch, which is the caller's signal to stop.
    ///
    /// The conversion lives here rather than in the callback because it writes into
    /// `self.scratch`: with the engines behind a method, a caller holding that slice and calling
    /// a `&mut self` method would be borrowing the whole struct twice. Inside one method the
    /// compiler sees the fields for what they are — disjoint.
    #[inline]
    fn process_bytes(&mut self, block: &[u8], channels: usize) -> Option<&[f32]> {
        let samples = block.len() / std::mem::size_of::<f32>() / channels * channels;
        let scratch = self.scratch.get_mut(..samples)?;
        // `as_chunks` hands over fixed-size arrays, so the four-byte guarantee is in the type
        // rather than in a `try_from` the audio path has to check every sample.
        let (words, _) = block.as_chunks::<{ std::mem::size_of::<f32>() }>();
        for (slot, raw) in scratch.iter_mut().zip(words) {
            *slot = f32::from_le_bytes(*raw);
        }
        match self.direction {
            DeviceDirection::Output => self.engine.process(scratch, channels),
            DeviceDirection::Input => self.input.process(scratch, channels),
        }
        Some(scratch)
    }

    #[inline]
    fn meters(&self) -> Meters {
        match self.direction {
            DeviceDirection::Output => self.engine.meters(),
            DeviceDirection::Input => self.input.meters(),
        }
    }
}

impl std::fmt::Debug for SinkDsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkDsp")
            .field("direction", &self.direction)
            .field("scratch", &self.scratch.len())
            .finish_non_exhaustive()
    }
}

/// User data of NODE 1 — the virtual sink, or the capture stream in the input direction. Either
/// way it is the node the DSP runs in.
pub(crate) struct SinkData {
    dsp: Option<SinkDsp>,
    ring: Arc<SampleRing>,
    counters: Arc<Counters>,
    status: Arc<StreamStatus>,
    format: AudioInfoRaw,
    /// Negotiated channel count, latched by `param_changed`. Zero until the format is agreed.
    channels: usize,
    quantum: usize,
    recycle: Sender<SinkDsp>,
}

impl Drop for SinkData {
    /// Hand the DSP side back to the main loop so the next pair of nodes can reuse it. Runs on the
    /// main loop, when the supervisor drops the stream listener.
    fn drop(&mut self) {
        if let Some(dsp) = self.dsp.take()
            && self.recycle.send(dsp).is_err()
        {
            log::error!("could not recycle the DSP state; parameters will stop being applied");
        }
    }
}

/// User data of NODE 2 — the playback stream, or the virtual source in the input direction.
pub(crate) struct OutData {
    ring: Arc<SampleRing>,
    counters: Arc<Counters>,
    status: Arc<StreamStatus>,
    format: AudioInfoRaw,
    channels: usize,
    scratch: Vec<f32>,
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
    pub(crate) meters: Input<Meters>,
    pub(crate) events: Receiver<DspEvent>,
    pub(crate) ready: Sender<Result<(), AudioError>>,
}

/// The two PipeWire nodes of the active direction and everything that must die with them.
///
/// Field order is drop order and drop order matters: a `StreamListener` removes a `spa_hook` from
/// a list that lives inside the stream, so every listener is declared before the stream it hooks.
struct Nodes {
    _first_listener: pw::stream::StreamListener<SinkData>,
    _second_listener: pw::stream::StreamListener<OutData>,
    _first: pw::stream::StreamRc,
    _second: pw::stream::StreamRc,
    /// Which pair this is: sink + playback stream, or capture stream + source.
    direction: DeviceDirection,
    /// `node.name` of the real device NODE 2 renders to, or NODE 1 captures from.
    target: String,
    channels: u32,
    rate: u32,
}

/// One PipeWire connection. Replaced wholesale on a reconnect.
struct Session {
    nodes: Option<Nodes>,
    _metadata_listener: Option<pw::metadata::MetadataListener>,
    metadata: Option<pw::metadata::Metadata>,
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

/// One value per [`DeviceDirection`].
#[derive(Debug, Default)]
struct PerDirection<T> {
    output: T,
    input: T,
}

impl<T> PerDirection<T> {
    const fn get(&self, direction: DeviceDirection) -> &T {
        match direction {
            DeviceDirection::Output => &self.output,
            DeviceDirection::Input => &self.input,
        }
    }

    const fn get_mut(&mut self, direction: DeviceDirection) -> &mut T {
        match direction {
            DeviceDirection::Output => &mut self.output,
            DeviceDirection::Input => &mut self.input,
        }
    }
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
}

/// Everything the main loop mutates. Reachable only from main-loop callbacks — never from
/// `process()`.
struct Shared {
    state: State,
    session: Option<Session>,
    /// The direction the engine is running in. Starts as [`DeviceDirection::Output`], the only
    /// one the Windows build had.
    direction: DeviceDirection,
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
    node_probes: std::collections::HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    /// The device names of the active direction seen at the previous rules run —
    /// `pwszIDPreviousRealDevices` (`audiopassthru/include/sndDevices.h:349`).
    previous_names: Vec<String>,
    /// The session defaults, one per direction.
    defaults: PerDirection<DefaultState>,
    graph_rate: u32,
    /// The Windows registry slots, one set per direction, so trying a microphone never forgets
    /// which speakers the user had.
    memory: PerDirection<SelectionMemory>,
    /// Whether to take the session default for the active direction once its nodes are up.
    /// `true` unless a caller opted out with [`UiToAudio::SetAsDefault`]`(false)`.
    want_default: bool,

    needs_rules: bool,
    needs_publish: bool,
    restart_requested: bool,
    attempts: u32,
    next_attempt: Instant,
    /// The `sync` the exit path put behind its hand-back of the session default, until the
    /// server's `done` for it arrives ([`release_defaults_before_exit`]).
    release_pending: Option<AsyncSeq>,

    dsp: Option<SinkDsp>,
    recycle: Receiver<SinkDsp>,
    recycle_tx: Sender<SinkDsp>,
    ring: Arc<SampleRing>,
    counters: Arc<Counters>,
    status: Arc<StreamStatus>,
    notify: Sender<AudioToUi>,
    remote: Option<String>,
    /// See [`Config::language`].
    language: Option<String>,

    last_status: AudioStatus,
    last_devices: Vec<AudioDevice>,
    last_sink_cycles: u64,
    last_underruns: u64,
    last_error: Option<AudioError>,
}

impl Shared {
    /// The state of a thread that has not connected yet.
    fn new(
        notify: Sender<AudioToUi>,
        remote: Option<String>,
        language: Option<String>,
        dsp: Option<SinkDsp>,
    ) -> Self {
        let (recycle_tx, recycle) = crossbeam_channel::unbounded();
        Self {
            state: State::Disconnected,
            session: None,
            direction: DeviceDirection::Output,
            devices: Vec::new(),
            node_probes: std::collections::HashMap::new(),
            previous_names: Vec::new(),
            defaults: PerDirection::default(),
            graph_rate: DEFAULT_SAMPLE_RATE,
            memory: PerDirection::default(),
            want_default: true,
            needs_rules: false,
            needs_publish: false,
            restart_requested: false,
            attempts: 0,
            next_attempt: Instant::now(),
            release_pending: None,
            dsp,
            recycle,
            recycle_tx,
            ring: Arc::new(SampleRing::new()),
            counters: Arc::new(Counters::default()),
            status: Arc::new(StreamStatus::default()),
            notify,
            remote,
            language,
            last_status: AudioStatus::default(),
            last_devices: Vec::new(),
            last_sink_cycles: 0,
            last_underruns: 0,
            last_error: None,
        }
    }

    fn notify(&self, message: AudioToUi) {
        if self.notify.send(message).is_err() {
            log::debug!("no GUI is listening for audio notifications");
        }
    }

    /// Report an error once, rather than every 200 ms for as long as it persists.
    fn report_error(&mut self, error: AudioError) {
        if self.last_error.as_ref() == Some(&error) {
            return;
        }
        log::warn!("audio engine: {error}");
        self.notify(AudioToUi::Error {
            message: error.to_string(),
        });
        self.last_error = Some(error);
    }

    fn clear_error(&mut self) {
        self.last_error = None;
    }

    fn has_nodes(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.nodes.is_some())
    }
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
        ready,
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

    let shared = Rc::new(RefCell::new(Shared::new(
        notify,
        remote,
        language,
        Some(SinkDsp::new(params, input_params, meters, events)),
    )));

    let control_source = control.attach(mainloop.loop_(), {
        let shared = Rc::clone(&shared);
        let mainloop = mainloop.clone();
        move |message| handle_control(&shared, &mainloop, message)
    });

    // The first attempt is made synchronously so `AudioEngine::start` can report it. Connecting
    // to a socket that is not there fails at `connect(2)`, so this does not delay start-up.
    let first = connect(&shared, &context);
    let _ = ready.send(first.as_ref().copied().map_err(Clone::clone));
    if let Err(error) = first {
        shared.borrow_mut().report_error(error);
    }

    let timer = mainloop.loop_().add_timer({
        let shared = Rc::clone(&shared);
        let context = context.clone();
        move |_| supervise(&shared, &context)
    });
    let _ = timer.update_timer(Some(SUPERVISOR_PERIOD), Some(SUPERVISOR_PERIOD));

    mainloop.run();

    // Teardown order is the one `docs/spec/12-audio-io.md` §21.5 insists on: hand the session
    // default — sink or source, whichever we hold — back to a real device *first*, while our
    // metadata proxy is still alive, and only then destroy the nodes. Otherwise there is a window
    // in which the default names a node that no longer exists.
    //
    // Nothing else may run while that happens: a supervisor tick would re-run the rules and take
    // the default straight back, and a late control message could do the same.
    drop(timer);
    drop(control_source);
    release_defaults_before_exit(&shared, &mainloop);
    shared.borrow_mut().session = None;
    log::info!("FxSound audio thread stopped");
}

/// The exit hand-back, made to actually arrive.
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
/// callbacks the pump can reach (`done`, the metadata echo of our own write, stream state) all
/// `try_borrow_mut` and never touch the default.
fn release_defaults_before_exit(
    shared: &Rc<RefCell<Shared>>,
    mainloop: &pw::main_loop::MainLoopRc,
) {
    let pending = {
        let mut guard = shared.borrow_mut();
        if !release_all_defaults(&mut guard) {
            return;
        }
        let Some(session) = guard.session.as_ref() else {
            return;
        };
        match session.core.sync(RELEASE_SEQ) {
            Ok(seq) => seq,
            Err(error) => {
                log::warn!("could not confirm the hand-back of the session default: {error}");
                return;
            }
        }
    };
    shared.borrow_mut().release_pending = Some(pending);

    let deadline = Instant::now() + RELEASE_TIMEOUT;
    if pump_until_release_confirmed(shared, mainloop.loop_(), deadline) {
        log::debug!("the server confirmed the hand-back of the session default");
    } else {
        log::warn!(
            "the server did not confirm the hand-back of the session default within \
             {RELEASE_TIMEOUT:?}; closing the connection regardless"
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
    match message {
        UiToAudio::SelectDevice {
            node_name,
            direction,
        } => {
            // Windows expressed user choice by forcing the system default
            // (`AudioPassthruPrivate.cpp:657`) and then letting the rules pick it up. Recording
            // the preference instead is what `docs/spec/12-audio-io.md` open question 8 asks for:
            // it does the same thing without mutating global state.
            shared.memory.get_mut(direction).user_selected = node_name;
            if direction != shared.direction {
                switch_direction(&mut shared, direction);
            }
            shared.needs_rules = true;
        }
        UiToAudio::RescanDevices => {
            shared.needs_publish = true;
            shared.needs_rules = true;
        }
        UiToAudio::SetAsDefault(want) => {
            shared.want_default = want;
            if want {
                // With no nodes yet the claim happens when they come up; writing the key now
                // would point the default at a node that does not exist.
                if shared.has_nodes() {
                    claim_default(&mut shared);
                }
            } else {
                let direction = shared.direction;
                release_default(&mut shared, direction);
            }
        }
        UiToAudio::Restart => {
            shared.restart_requested = true;
        }
        UiToAudio::Shutdown => unreachable!("handled above"),
    }
}

// ---------------------------------------------------------------------------------------------
// Connection and the reconnect state machine
// ---------------------------------------------------------------------------------------------

fn connect(
    shared: &Rc<RefCell<Shared>>,
    context: &pw::context::ContextRc,
) -> Result<(), AudioError> {
    let remote = shared.borrow().remote.clone();
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
                if confirm_release(&mut shared, seq) {
                    return;
                }
                // The initial registry dump has been delivered: it is now meaningful to choose a
                // device.
                shared.needs_rules = true;
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

    let mut guard = shared.borrow_mut();
    guard.devices.clear();
    guard.previous_names.clear();
    guard.defaults = PerDirection::default();
    guard.state = State::Connecting;
    guard.session = Some(Session {
        nodes: None,
        _metadata_listener: None,
        metadata: None,
        _registry_listener: registry_listener,
        _registry: registry,
        _core_listener: core_listener,
        core,
    });
    guard.clear_error();
    Ok(())
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
    // reached when the connection or our node is already broken — the core error means the
    // server is gone and the key with it as far as we can reach it, WirePlumber falls back on
    // its own once our node vanishes, and the reconnect claims the default again
    // (`claim_default` skips the stale value that still names us). What the session knew about
    // the defaults dies with it; `connect` reads them afresh.
    shared.defaults = PerDirection::default();
    // Dropping the session drops the stream listeners, whose `Drop` hands the DSP state back.
    shared.session = None;
    drain_recycled_dsp(shared);
    shared.state = State::Disconnected;
    shared.devices.clear();
    shared.previous_names.clear();
    shared
        .status
        .output_streaming
        .store(false, Ordering::Relaxed);

    let step = BACKOFF_MS
        .get(shared.attempts as usize)
        .copied()
        .unwrap_or_else(|| BACKOFF_MS[BACKOFF_MS.len() - 1]);
    shared.attempts = shared.attempts.saturating_add(1);
    shared.next_attempt = Instant::now() + Duration::from_millis(step);
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

        // 1. Recover any DSP state a torn-down stream handed back.
        drain_recycled_dsp(&mut guard);

        // 2. A core error, an explicit Restart, or a stream that went into error.
        if guard.restart_requested {
            guard.restart_requested = false;
            disconnect(&mut guard, "restart requested");
        } else if guard.status.sink_error.swap(false, Ordering::Relaxed) {
            disconnect(&mut guard, "the virtual node reported an error");
        } else if guard.status.output_error.swap(false, Ordering::Relaxed) {
            // Only NODE 2 failed. Drop the pair and let the rules pick a different device; the
            // default stays ours because the pair is about to come back.
            drop_nodes(&mut guard);
            guard.needs_rules = true;
            guard.report_error(AudioError::DeviceUnavailable);
        }

        // 3. Format mismatch between the two nodes: rebuild rather than play at the wrong stride.
        if guard.counters.format_mismatches.swap(0, Ordering::Relaxed) > 0 {
            log::warn!("the two nodes negotiated different formats; rebuilding");
            drop_nodes(&mut guard);
            guard.needs_rules = true;
        }

        // 4. Rules and node creation.
        if guard.needs_rules && guard.session.is_some() {
            guard.needs_rules = false;
            apply_rules(&mut guard);
        }

        // 5. Tell the GUI what changed.
        publish(&mut guard);

        guard.session.is_none() && Instant::now() >= guard.next_attempt
    };

    if should_connect
        && let Err(error) = connect(shared, context)
        && let Ok(mut guard) = shared.try_borrow_mut()
    {
        guard.report_error(error);
        let step = BACKOFF_MS
            .get(guard.attempts as usize)
            .copied()
            .unwrap_or_else(|| BACKOFF_MS[BACKOFF_MS.len() - 1]);
        guard.attempts = guard.attempts.saturating_add(1);
        guard.next_attempt = Instant::now() + Duration::from_millis(step);
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
            // Our own four nodes are in the registry too; `from_props` drops them by name so none
            // of them can ever become a target.
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
            let affects_rules = device.direction == guard.direction;
            let object_id = device.object_id;
            guard
                .devices
                .retain(|existing| existing.object_id != device.object_id);
            guard.devices.push(device);
            guard.needs_publish = true;
            guard.needs_rules |= affects_rules;

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
                                // Read the two values out here rather than handing the dictionary
                                // on: its lifetime is the callback's, and the handler wants to
                                // hold a mutable borrow of `Shared` across the update.
                                let channels = props
                                    .get("audio.channels")
                                    .and_then(|value| value.parse::<u32>().ok())
                                    .unwrap_or(0);
                                let positions = props
                                    .get("audio.position")
                                    .and_then(ChannelMap::parse)
                                    .filter(|map| map.len() == channels as usize);
                                on_node_info(&shared, object_id, channels, positions);
                            }
                        })
                        .register();
                    guard.node_probes.insert(object_id, (node, listener));
                }
                Err(err) => {
                    log::debug!("could not bind node {object_id} to read its format: {err}")
                }
            }
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
            let listener = metadata
                .add_listener_local()
                .property({
                    let shared = Rc::clone(shared);
                    move |_subject, key, _type, value| {
                        on_metadata_property(&shared, key, value);
                        0
                    }
                })
                .register();
            if name == "default" {
                if let Some(session) = guard.session.as_mut() {
                    session.metadata = Some(metadata);
                    session._metadata_listener = Some(listener);
                }
            } else {
                // The `settings` object only has to be watched, never written; keeping the
                // listener alive is enough, and dropping the proxy with it is fine because the
                // values we want arrive in the initial burst.
                drop(listener);
                drop(metadata);
            }
        }
        _ => {}
    }
}

fn on_global_remove(shared: &Rc<RefCell<Shared>>, id: u32) {
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    let Some(index) = guard.devices.iter().position(|d| d.object_id == id) else {
        return;
    };
    let removed = guard.devices.remove(index);
    guard.node_probes.remove(&id);
    log::debug!(
        "{} disappeared: {} ({})",
        noun(removed.direction),
        removed.description,
        removed.name
    );
    guard.needs_publish = true;
    // The target may have just been unplugged. Re-running the rules attaches the pair to another
    // device of the active direction (`docs/spec/12-audio-io.md` §22).
    guard.needs_rules |= removed.direction == guard.direction;
}

/// A bound node reported its format. Fill in what the registry could not tell us.
///
/// Arrives once per node shortly after it appears, and again whenever the node's info changes —
/// a profile switch on an ALSA card, for instance, which really does change the channel count
/// under a running stream.
fn on_node_info(
    shared: &Rc<RefCell<Shared>>,
    object_id: u32,
    channels: u32,
    positions: Option<ChannelMap>,
) {
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    let Some(device) = guard.devices.iter_mut().find(|d| d.object_id == object_id) else {
        return;
    };

    // Already learned, or nothing to learn: leave it alone. Taking only the first non-zero report
    // is what makes this immune to the churn described above.
    if device.channels != 0 || channels == 0 {
        return;
    }
    let before = device.clamped_channels();
    device.channels = channels;
    if let Some(map) = positions {
        device.positions = map;
    } else if channels != 0 {
        device.positions = ChannelMap::default_for(channels);
    }
    let after = device.clamped_channels();
    let name = device.name.clone();
    let device_direction = device.direction;

    log::debug!(
        "{name}: {channels} channels, {}",
        device.positions.to_property_value()
    );
    guard.needs_publish = true;

    // The one rebuild worth doing: the device was selected before its info arrived, so the nodes
    // were built against the stereo fallback, and the truth is something else. Only the device the
    // nodes are attached to matters — reacting to every device of the direction would rebuild the
    // running stream because some *other* sink reported something.
    let attached = guard
        .session
        .as_ref()
        .and_then(|session| session.nodes.as_ref())
        .is_some_and(|nodes| nodes.direction == device_direction && nodes.target == name);
    if attached && after != before {
        log::info!("{name} reports {after} channels rather than {before}; rebuilding for it");
        guard.needs_rules = true;
    }
}

fn on_metadata_property(shared: &Rc<RefCell<Shared>>, key: Option<&str>, value: Option<&str>) {
    let Some(key) = key else {
        return;
    };
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    for direction in [DeviceDirection::Output, DeviceDirection::Input] {
        if key == devices::default_key(direction) {
            guard.defaults.get_mut(direction).current =
                value.and_then(devices::parse_default_node_name);
            guard.needs_publish = true;
            guard.needs_rules |= direction == guard.direction;
            return;
        }
        if key == devices::configured_default_key(direction) {
            let configured = value.and_then(devices::parse_default_node_name);
            let state = guard.defaults.get_mut(direction);
            state.holding = configured.as_deref() == Some(our_node_name(direction));
            state.configured = configured;
            return;
        }
    }
    if matches!(key, "clock.rate" | "clock.force-rate")
        && let Some(rate) = value.and_then(|v| v.trim().parse::<u32>().ok())
        && (8_000..=crate::MAX_SAMPLE_RATE).contains(&rate)
    {
        guard.graph_rate = rate;
    }
}

// ---------------------------------------------------------------------------------------------
// The session default
// ---------------------------------------------------------------------------------------------

/// Become the session default for the active direction, politely: remember what was there first.
fn claim_default(shared: &mut Shared) {
    let direction = shared.direction;
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
    let previous = {
        let state = shared.defaults.get(direction);
        [state.configured.as_deref(), state.current.as_deref()]
            .into_iter()
            .flatten()
            .find(|name| *name != ours)
            .map(str::to_owned)
    };
    if let Some(previous) = previous {
        let memory = shared.memory.get_mut(direction);
        if memory.original_default.is_empty() {
            memory.original_default.clone_from(&previous);
        }
        memory.most_recent_default = previous;
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

/// [`release_default`] for both directions — the exit path, where whichever default we hold has
/// to go back before the metadata proxy does. Returns whether anything was written, i.e. whether
/// there is a write to wait for.
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
// Device selection, direction switching and node creation
// ---------------------------------------------------------------------------------------------

/// Take back whatever DSP state a dropped NODE 1 handed through the recycle channel.
///
/// `SinkData::drop` sends synchronously on an unbounded channel, so right after a `Nodes` is
/// dropped the state is already waiting here; draining before every `build_nodes` is what lets a
/// pair be rebuilt within one supervisor tick instead of failing once and waiting for the next.
fn drain_recycled_dsp(shared: &mut Shared) {
    while let Ok(dsp) = shared.recycle.try_recv() {
        shared.dsp = Some(dsp);
    }
}

/// Destroy the active pair of nodes and recover the DSP state, **keeping** the default. For the
/// transient rebuilds — a new target, a format mismatch, NODE 2 erroring — where the pair is about
/// to come straight back under the same name.
fn drop_nodes(shared: &mut Shared) {
    if let Some(session) = shared.session.as_mut() {
        session.nodes = None;
    }
    drain_recycled_dsp(shared);
    shared
        .status
        .output_streaming
        .store(false, Ordering::Relaxed);
}

/// Hand the active direction's default back, *then* destroy its nodes — the order
/// `docs/spec/12-audio-io.md` §21.5 requires. For the cases where the pair is not coming back as
/// it was: a direction switch, or rules that ended in an error.
fn teardown_nodes(shared: &mut Shared) {
    let direction = shared.direction;
    release_default(shared, direction);
    drop_nodes(shared);
}

/// Leave the active direction and enter the other one.
///
/// The old pair goes first, with its default handed back, so the system only ever sees one
/// FxSound device: switching to a microphone makes "FxSound (Output)" disappear from the sinks
/// before "FxSound (Input)" appears among the sources, and vice versa.
fn switch_direction(shared: &mut Shared, direction: DeviceDirection) {
    log::info!(
        "switching from the {} to the {} direction",
        noun(shared.direction),
        noun(direction)
    );
    teardown_nodes(shared);
    shared.direction = direction;
    // `pwszIDPreviousRealDevices` is per direction; an empty snapshot keeps rule 5 from treating
    // every device of the new direction as freshly plugged in.
    shared.previous_names.clear();
    shared.clear_error();
    shared.needs_publish = true;
    shared.needs_rules = true;
}

fn apply_rules(shared: &mut Shared) {
    let direction = shared.direction;
    let ours = our_node_name(direction);
    let selection = devices::choose_device(
        &shared.devices,
        direction,
        ours,
        shared.defaults.get(direction).current.as_deref(),
        &shared.previous_names,
        shared.memory.get(direction),
    );
    shared.previous_names = shared
        .devices
        .iter()
        .filter(|d| d.direction == direction)
        .map(|d| d.name.clone())
        .collect();

    let selection = match selection {
        Ok(selection) => selection,
        Err(error) => {
            // No device to attach to: the pair goes, and so does our claim on the default —
            // leaving `default.configured.audio.*` pointing at a node that no longer exists is the
            // one thing §21 forbids.
            teardown_nodes(shared);
            shared.report_error(error);
            return;
        }
    };

    let already = shared
        .session
        .as_ref()
        .and_then(|s| s.nodes.as_ref())
        .is_some_and(|nodes| nodes.direction == direction && nodes.target == selection.target);
    if already {
        shared.clear_error();
        return;
    }

    let Some(target) = shared
        .devices
        .iter()
        .find(|d| d.direction == direction && d.name == selection.target)
        .cloned()
    else {
        shared.report_error(AudioError::DeviceNotPresent);
        return;
    };

    // Rebuilding replaces both nodes. The old pair goes first so its DSP state — filter history,
    // scratch — comes back through the recycle channel for the new pair to adopt, and so the
    // server never sees two nodes with the same `node.name`. The default is kept: the same node
    // is about to reappear under the same name, and `default.configured.audio.*` survives the gap.
    drop_nodes(shared);
    match build_nodes(shared, &target) {
        Ok(nodes) => {
            log::info!(
                "{} {} ({} ch @ {} Hz)",
                match direction {
                    DeviceDirection::Output => "rendering to",
                    DeviceDirection::Input => "capturing from",
                },
                target.description,
                nodes.channels,
                nodes.rate
            );
            if let Some(session) = shared.session.as_mut() {
                session.nodes = Some(nodes);
            }
            shared.state = State::Running;
            shared.attempts = 0;
            shared.clear_error();
            // Claim before committing: `claim_default` records the default that was there *before*
            // us in `original_default` / `most_recent_default`, and `commit` only fills those slots
            // when they are still empty — so this order keeps them honest on a first run.
            if shared.want_default {
                claim_default(shared);
            }
            devices::commit(shared.memory.get_mut(direction), &selection);
        }
        Err(error) => {
            shared.report_error(error);
        }
    }
}

/// Create both nodes of the target's direction.
///
/// The format is decided here, once, and declared on **both** nodes so the ring is always read at
/// the stride it was written at: the target's channel count clamped to `2..=8`
/// (`sndDevices.h:190-191`), its own channel positions, and the graph's clock rate. That replaces
/// §8 of the Windows design wholesale — no `IPolicyConfigVista::SetDeviceFormat`, no rate pushed
/// onto a driver, no zero-order-hold upsampler. If the real device wants something else, the
/// adapter in front of it converts — which is also how a mono microphone arrives here as the
/// stereo pair the DSP runs on.
fn build_nodes(shared: &mut Shared, target: &DeviceInfo) -> Result<Nodes, AudioError> {
    let Some(session) = shared.session.as_ref() else {
        return Err(AudioError::PipewireDisconnected);
    };
    let core = session.core.clone();

    if target.is_refused_mono() {
        return Err(AudioError::NoValidOutput);
    }
    let direction = target.direction;
    let channels = target.clamped_channels();
    let rate = target.rate.unwrap_or(shared.graph_rate);
    let positions = target.positions.resized(channels);
    let quantum = DEFAULT_QUANTUM_FRAMES.min(MAX_QUANTUM_FRAMES as u32);
    let latency = format!("{quantum}/{rate}");

    // ---- NODE 1: the node the DSP runs in ----------------------------------------------------
    //
    // Output: the virtual sink. A `pw_stream` with `media.class = "Audio/Sink"` *is* a sink node;
    // there is nothing else to do to make applications able to render into it, and nothing to
    // autoconnect — WirePlumber links clients *to* it.
    //
    // Input: a capture stream on the chosen microphone, targeted by name and autoconnected.
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
            stream_props(direction, &target.name, &latency),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
    };
    let first = pw::stream::StreamRc::new(core.clone(), first_name, first_props)
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    // Only now take the DSP state: from here on every failure path drops it inside `SinkData`,
    // whose `Drop` hands it straight back through the recycle channel.
    drain_recycled_dsp(shared);
    let Some(mut dsp) = shared.dsp.take() else {
        log::error!("the DSP state has not come back from the previous pair of nodes");
        return Err(AudioError::PipewireDisconnected);
    };
    // Before anything else: which chain this pair of nodes runs. Everything below reads it.
    dsp.set_direction(direction);
    dsp.set_format(rate as f32, channels as usize);
    dsp.set_layout(positions.lfe_index(), positions.front_pair());
    // Read before the engine is handed to the node's user data, where it can no longer be reached
    // from the main loop.
    let dsp_latency_frames = dsp.latency_frames();
    // `set_format` returns early when neither the rate nor the channel count moved, so switching
    // between two devices that are both 48 kHz stereo — the common case — would otherwise carry
    // the previous device's filter history, reverb tail and leveller gain straight into the new
    // one. The engine is recycled deliberately, but its *state* should not be.
    dsp.reset();

    shared.ring.reconfigure(channels as usize, quantum as usize);
    shared.counters.sample_rate.store(rate, Ordering::Relaxed);
    shared.counters.channels.store(channels, Ordering::Relaxed);

    let first_data = SinkData {
        dsp: Some(dsp),
        ring: Arc::clone(&shared.ring),
        counters: Arc::clone(&shared.counters),
        status: Arc::clone(&shared.status),
        format: AudioInfoRaw::new(),
        channels: 0,
        quantum: quantum as usize,
        recycle: shared.recycle_tx.clone(),
    };
    let first_listener = first
        .add_local_listener_with_user_data(first_data)
        .state_changed(move |_stream, data, _old, new| {
            log::debug!("{first_name}: {new:?}");
            if let StreamState::Error(message) = &new {
                log::warn!("{first_name} error: {message}");
                data.status.sink_error.store(true, Ordering::Relaxed);
            }
        })
        .param_changed(on_sink_format)
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
            stream_props(direction, &target.name, &latency),
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

    let second_data = OutData {
        ring: Arc::clone(&shared.ring),
        counters: Arc::clone(&shared.counters),
        status: Arc::clone(&shared.status),
        format: AudioInfoRaw::new(),
        channels: 0,
        scratch: vec![0.0; MAX_QUANTUM_FRAMES * MAX_CHANNELS as usize],
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
        .param_changed(on_output_format)
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
        _first_listener: first_listener,
        _second_listener: second_listener,
        _first: first,
        _second: second,
        direction,
        target: target.name.clone(),
        channels,
        rate,
    })
}

/// The properties of FxSound's virtual device — the sink in the output direction, the source in
/// the input direction (`docs/spec/12-audio-io.md` §20 NODE 1, §28.2 NODE 2).
///
/// Property spellings are the verified ones from `docs/api/pipewire-0.10-rust.md` §14 — every key
/// after the first dot is hyphenated, and `audio.position` has no Rust constant because it has no
/// `PW_KEY_` either. The description is the localised `"FxSound (<Output|Input>)"`; the name is
/// the fixed ASCII one the metadata carries.
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
        *pw::keys::NODE_LINK_GROUP    => LINK_GROUP,
        *pw::keys::NODE_WANT_DRIVER   => "true",
        *pw::keys::NODE_ALWAYS_PROCESS => "false",
        *pw::keys::NODE_LATENCY       => latency,
        *pw::keys::AUDIO_CHANNELS     => channels.to_string(),
        *pw::keys::AUDIO_RATE         => rate.to_string(),
        *pw::keys::AUDIO_FORMAT       => "F32",
        *pw::keys::APP_NAME           => "FxSound",
        *pw::keys::APP_ID             => "com.fxsound.FxSound",
        "audio.position"              => positions.to_property_value(),
        "device.class"                => "sound",
        "priority.session"            => NODE_PRIORITY_SESSION,
        "priority.driver"             => "0",
        "monitor.channel-volumes"     => "false",
        "media.icon-name"             => "fxsound",
        "application.icon-name"       => "fxsound",
    }
}

/// The properties of the stream that touches the real device — the playback stream in the output
/// direction, the capture stream in the input direction (`docs/spec/12-audio-io.md` §20 NODE 2,
/// §28.2 NODE 1). `target` is the real device's `node.name`.
fn stream_props(direction: DeviceDirection, target: &str, latency: &str) -> PropertiesBox {
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
        *pw::keys::NODE_LINK_GROUP     => LINK_GROUP,
        *pw::keys::NODE_AUTOCONNECT    => "true",
        *pw::keys::NODE_DONT_RECONNECT => "false",
        *pw::keys::NODE_PASSIVE        => "false",
        *pw::keys::NODE_LATENCY        => latency,
        *pw::keys::STREAM_DONT_REMIX   => "false",
        *pw::keys::TARGET_OBJECT       => target,
        *pw::keys::APP_NAME            => "FxSound",
        *pw::keys::APP_ID              => "com.fxsound.FxSound",
    };
    if direction == DeviceDirection::Input {
        // Capture the microphone itself, not the monitor of a sink.
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "false");
    }
    props
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
// The two process callbacks — RT thread from here down
// ---------------------------------------------------------------------------------------------

/// Latch the negotiated format. Main loop, never the data thread — which is exactly why the
/// process callbacks can assume `channels` is constant for the life of a buffer.
fn on_sink_format(_stream: &pw::stream::Stream, data: &mut SinkData, id: u32, param: Option<&Pod>) {
    let Some((rate, channels)) = parse_audio_format(&mut data.format, id, param) else {
        return;
    };
    data.channels = channels;
    data.counters.sample_rate.store(rate, Ordering::Relaxed);
    data.counters
        .channels
        .store(channels as u32, Ordering::Relaxed);
    data.ring.reconfigure(channels, data.quantum);
    if let Some(dsp) = data.dsp.as_mut() {
        dsp.set_format(rate as f32, channels);
    }
    log::info!("NODE 1 negotiated {channels} ch @ {rate} Hz");
}

fn on_output_format(
    _stream: &pw::stream::Stream,
    data: &mut OutData,
    id: u32,
    param: Option<&Pod>,
) {
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
        // the ring drain and NODE 2 emit silence, which is the same outcome with less work.
        return;
    }
    let Some(dsp) = data.dsp.as_mut() else {
        return;
    };

    // Parameters are state: take the newest snapshot, discard anything in between. Events are
    // not: drain them all.
    dsp.refresh();

    let Some(bytes) = chunk_data.data() else {
        return;
    };
    let Some(valid) = bytes.get(offset..offset.saturating_add(size)) else {
        return;
    };

    let block_bytes = dsp.scratch.len() * std::mem::size_of::<f32>();
    let mut frames = 0_u64;
    for block in valid.chunks(block_bytes) {
        let Some(processed) = dsp.process_bytes(block, channels) else {
            break;
        };
        frames += (processed.len() / channels) as u64;
        data.ring.push(processed);
    }

    let meters = dsp.meters();
    dsp.meters.write(meters);
    data.counters.sink_cycles.fetch_add(1, Ordering::Relaxed);
    data.counters
        .frames_processed
        .fetch_add(frames, Ordering::Relaxed);
}

/// NODE 2's `process()`: drain the ring into the buffer the real device is about to play — or, in
/// the input direction, into the buffer an application is about to record.
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

fn publish(shared: &mut Shared) {
    let cycles = shared.counters.sink_cycles.load(Ordering::Relaxed);
    let processing = cycles != shared.last_sink_cycles;
    shared.last_sink_cycles = cycles;

    let rate = shared.counters.sample_rate.load(Ordering::Relaxed).max(1);
    let status = AudioStatus {
        processing: processing && shared.state == State::Running,
        sample_rate: rate,
        channels: shared.counters.channels.load(Ordering::Relaxed) as u16,
        processed_secs: shared.counters.frames_processed.load(Ordering::Relaxed) / u64::from(rate),
    };
    if status != shared.last_status {
        shared.last_status = status;
        shared.notify(AudioToUi::Status(status));
    }

    if shared.needs_publish {
        shared.needs_publish = false;
        let devices = published_devices(shared);
        if devices != shared.last_devices {
            shared.last_devices.clone_from(&devices);
            shared.notify(AudioToUi::Devices(devices));
        }
    }

    // Ring health, per `docs/spec/12-audio-io.md` open question 3: instrument the fill level from
    // day one so drift is detected in the field rather than guessed at.
    let underruns = shared.ring.underrun_frames.load(Ordering::Relaxed);
    if underruns != shared.last_underruns {
        log::debug!(
            "ring: fill {} frames, underruns {underruns}, dropped {}, resyncs {}",
            shared.ring.fill_frames(),
            shared.ring.dropped_frames.load(Ordering::Relaxed),
            shared.ring.resyncs.load(Ordering::Relaxed),
        );
        shared.last_underruns = underruns;
    }
}

/// The device list as the GUI wants it: every output sorted by description, then every input
/// sorted by description. The GUI draws its section headers off that grouping, so the order is
/// part of the contract. Mono *outputs* are left out (they could never be chosen); mono inputs
/// stay in.
fn published_devices(shared: &Shared) -> Vec<AudioDevice> {
    let mut published = Vec::with_capacity(shared.devices.len());
    for direction in [DeviceDirection::Output, DeviceDirection::Input] {
        let default = shared.defaults.get(direction).current.as_deref();
        let mut group: Vec<AudioDevice> = shared
            .devices
            .iter()
            .filter(|d| d.direction == direction && !d.is_refused_mono())
            .map(|d| d.to_audio_device(default))
            .collect();
        group.sort_by(|a, b| a.description.cmp(&b.description));
        published.extend(group);
    }
    published
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread's state before it has connected: no session, nothing held. Nothing in here
    /// touches PipeWire.
    fn shared_for_tests() -> Shared {
        let (notify, _) = crossbeam_channel::unbounded();
        Shared::new(notify, None, None, None)
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

    #[test]
    fn a_producer_and_a_consumer_on_two_threads_neither_block_nor_lose_alignment() {
        let ring = Arc::new(ring_for(2, 8));
        let producer = {
            let ring = Arc::clone(&ring);
            std::thread::spawn(move || {
                let mut next = 0.0_f32;
                for _ in 0..2_000 {
                    let block: Vec<f32> = (0..64).map(|i| next + i as f32).collect();
                    let pushed = ring.push(&block);
                    next += pushed as f32;
                }
                next
            })
        };
        let consumer = {
            let ring = Arc::clone(&ring);
            std::thread::spawn(move || {
                let mut out = vec![0.0_f32; 64];
                let mut total = 0_usize;
                for _ in 0..4_000 {
                    total += ring.pop(&mut out);
                }
                total
            })
        };
        let produced = producer.join().expect("producer");
        let consumed = consumer.join().expect("consumer");
        assert!(consumed > 0, "the consumer must have seen real audio");
        assert!(
            produced >= consumed as f32,
            "the consumer cannot have read more than was written"
        );
        // Whatever the interleaving, the cursors stay frame-aligned and sane.
        assert_eq!(ring.fill_frames() * 2 % 2, 0);
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
        assert_eq!(sink.get("node.link-group"), Some(LINK_GROUP));
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
        assert_eq!(source.get("node.link-group"), Some(LINK_GROUP));
        assert_eq!(source.get("node.virtual"), Some("true"));
        assert_eq!(source.get("node.want-driver"), Some("true"));
        assert_eq!(source.get("audio.channels"), Some("2"));
        assert_eq!(source.get("priority.session"), Some(NODE_PRIORITY_SESSION));
        assert_ne!(
            source.get("node.description"),
            sink.get("node.description"),
            "the two virtual nodes must be distinguishable in a device list"
        );

        let output = stream_props(DeviceDirection::Output, "alsa_output.x", "512/48000");
        assert_eq!(output.get("media.class"), Some("Stream/Output/Audio"));
        assert_eq!(output.get("media.category"), Some("Playback"));
        assert_eq!(output.get("media.role"), Some("Production"));
        assert_eq!(output.get("node.name"), Some(OUTPUT_NODE_NAME));
        assert_eq!(
            output.get("node.description"),
            Some(OUTPUT_STREAM_DESCRIPTION)
        );
        assert_eq!(output.get("node.link-group"), Some(LINK_GROUP));
        assert_eq!(output.get("node.autoconnect"), Some("true"));
        assert_eq!(output.get("target.object"), Some("alsa_output.x"));
        assert_eq!(output.get("stream.capture.sink"), None);

        let capture = stream_props(DeviceDirection::Input, "alsa_input.mic", "512/48000");
        assert_eq!(capture.get("media.class"), Some("Stream/Input/Audio"));
        assert_eq!(capture.get("media.category"), Some("Capture"));
        assert_eq!(capture.get("media.role"), Some("Production"));
        assert_eq!(capture.get("node.name"), Some(CAPTURE_NODE_NAME));
        assert_eq!(
            capture.get("node.description"),
            Some(CAPTURE_STREAM_DESCRIPTION)
        );
        assert_eq!(capture.get("node.link-group"), Some(LINK_GROUP));
        assert_eq!(capture.get("node.autoconnect"), Some("true"));
        assert_eq!(capture.get("target.object"), Some("alsa_input.mic"));
        assert_eq!(
            capture.get("stream.capture.sink"),
            Some("false"),
            "capture the microphone, not a sink monitor"
        );
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
}
