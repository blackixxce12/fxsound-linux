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
//! The input lane is left running either way: the capture stream is what makes the microphone
//! produce anything, and a recorder may link to the virtual source at any moment.
//!
//! All of this holds while echo cancellation is off. While it is on, the canceller records the
//! speakers' monitor on the microphone's clock, and the speakers and the output pair run with it
//! (`docs/0.4.0-design.md` §7, and `aec`); switched off, they sleep again.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use fxsound_core::messages::{AudioToUi, DspEvent, DspParams, InputDspParams, Meters, UiToAudio};
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
use crate::devices::{self, ChannelMap, DeviceInfo, SelectionMemory};
use crate::lane_dsp::{self, ChainHandover, LaneDsp};
use crate::per_direction::PerDirection;
use crate::{
    AEC_SOURCE_NODE_NAME, AudioError, CAPTURE_NODE_NAME, CAPTURE_STREAM_DESCRIPTION,
    DEFAULT_QUANTUM_FRAMES, DEFAULT_SAMPLE_RATE, MAX_CHANNELS, MAX_QUANTUM_FRAMES, MIN_CHANNELS,
    OUR_NODE_NAMES, OUTPUT_NODE_NAME, OUTPUT_STREAM_DESCRIPTION, RING_CAPACITY_FRAMES,
    SINK_DESCRIPTION, SINK_NODE_NAME, SOURCE_NODE_NAME, link_group, locale, our_node_name,
};

/// The rate the capture stream asks for, whatever the microphone runs at.
///
/// RNNoise exists at 48 kHz and nowhere else, and it sits in front of every stage that measures a
/// level — so a preset voiced with it on means something different at any other rate. Asking for
/// one rate and letting PipeWire resample is what makes a voice preset mean one thing everywhere.
const CAPTURE_RATE: u32 = DEFAULT_SAMPLE_RATE;

/// How often the supervisor runs. Also the shortest possible reconnect interval, which is the
/// "never retry faster than 200 ms" floor of `docs/spec/12-audio-io.md` §22.
const SUPERVISOR_PERIOD: Duration = Duration::from_millis(200);

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
            dsp: Some(dsp),
            ring: Arc::clone(&lane.ring),
            counters: Arc::clone(&lane.counters),
            status: Arc::clone(&lane.status),
            format: AudioInfoRaw::new(),
            channels: 0,
            recycle: lane.recycle.0.clone(),
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
    pub(crate) ready: Sender<Result<(), AudioError>>,
    /// The canceller library echo cancellation loads: WebRTC's, except in the tests
    /// (`AudioEngine::start_with_canceller`).
    pub(crate) aec_library: &'static str,
}

/// One lane's two PipeWire nodes and everything that must die with them.
///
/// Field order is drop order and drop order matters: a `StreamListener` removes a `spa_hook` from
/// a list that lives inside the stream, so every listener is declared before the stream it hooks.
struct Nodes {
    _first_listener: pw::stream::StreamListener<SinkData>,
    _second_listener: pw::stream::StreamListener<OutData>,
    /// Kept reachable rather than merely alive: the supervisor republishes this node's
    /// `ProcessLatency` when the DSP's delay changes under it.
    first: pw::stream::StreamRc,
    /// Kept reachable so that an output pair paced by hand can put it to sleep and wake it.
    second: pw::stream::StreamRc,
    /// `node.name` of the real device NODE 2 renders to, or NODE 1 captures from.
    target: String,
    /// The node the input lane's NODE 1 records from instead of `target` itself: the echo
    /// canceller's source, while echo cancellation runs for this microphone ([`aec::route`]).
    /// `None` in every other pair. What the lane is attached to is still `target` — the canceller
    /// is a stage in front of the microphone, not another device.
    via: Option<&'static str>,
    /// What both nodes were declared at.
    format: PairFormat,
    /// NODE 2's schedule, in an output pair whose server does not run a link-group together and
    /// so cannot idle it by itself (module docs, "Idle"). `None` in every other pair: the input
    /// lane's, which is left running, and an output pair whose NODE 2 is passive.
    pace: Option<SecondNodePace>,
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
    /// The target's own layout at that count.
    positions: ChannelMap,
}

impl PairFormat {
    /// The format a pair attached to `target` runs at, with the graph's clock at `graph_rate`.
    ///
    /// The target's channel count clamped to `2..=8`, its own channel positions, and — for the
    /// output lane — the device's rate when it publishes one and the graph's clock rate when it
    /// does not, which is most ALSA sinks. The input lane asks for [`CAPTURE_RATE`] whatever the
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

/// One PipeWire connection. Replaced wholesale on a reconnect.
///
/// The lanes' nodes are not in here: each lane keeps its own pair. They are made on this
/// connection all the same, and every stream holds a reference to its core, so a pair outliving
/// its session would hold the dead connection open under the new one. [`close_session`] is the
/// one way a session ends, and it takes both lanes' pairs with it.
///
/// Every listener is declared before the proxy it hooks, for the reason [`NodeProbe`] gives.
struct Session {
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
    /// `pwszIDPreviousRealDevices` (`audiopassthru/include/sndDevices.h:349`).
    previous_names: Vec<String>,
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
            want_default: true,
            kept_by_hand: false,
            last_target: None,
            needs_rules: false,
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
            defaults: PerDirection::default(),
            delivery: ListDelivery::default(),
            clock: GraphClock::default(),
            memory: PerDirection::default(),
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
        ready,
        aec_library,
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
    shared.borrow_mut().aec = EchoCancel::new(aec_library);

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
        // The upstream review's messages (`docs/0.4.0-upstream.md`). The API landed first, so the
        // app and the engine could be built against it in parallel; each is acted on by its own
        // item, and until then it is logged and otherwise ignored, which leaves the engine doing
        // exactly what it did before the message existed.
        UiToAudio::SetDevicePriority { direction, names } => {
            log::info!(
                "{} device priority: {} ranked (not acted on yet, U4)",
                direction.key(),
                names.len()
            );
        }
        UiToAudio::SeedTargetVolumes(volumes) => {
            log::info!(
                "{} remembered per-device volumes (not acted on yet, U10)",
                volumes.len()
            );
        }
        UiToAudio::SystemSleeping(sleeping) => {
            log::info!(
                "system {} (not acted on yet, U13)",
                if sleeping {
                    "going to sleep"
                } else {
                    "resumed"
                }
            );
        }
        UiToAudio::KeepInputAwake(awake) => {
            log::info!("keep the microphone awake: {awake} (not acted on yet, U19)");
        }
        UiToAudio::Shutdown => unreachable!("handled by handle_control"),
    }
    // A detached microphone lane, another microphone or other speakers: whatever the message
    // changed, a canceller that no longer fits goes now rather than on the next tick, and a capture
    // stream recording from it goes back to the microphone with it. Loading one needs the context,
    // which only the supervisor has; it follows within 200 ms.
    reconcile_echo_cancel(shared, None);
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
/// pair on the microphone. Now, and not on the next tick, because the stream must never be left
/// aimed at a source that has gone. It carries no `node.dont-fallback`, and under a session
/// manager a stream whose target vanishes is linked to whatever else it finds: the default source
/// first, which while the input lane holds the default is FxSound's own. WirePlumber's `canLink`
/// refuses that one (`fxsound_capture` and `fxsound_source` share `fxsound-input`), but not the
/// best of the rest it tries next, which can be another microphone altogether.
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
    shared.memory.get_mut(direction).user_selected = node_name;
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
    if shared.ready() {
        shared.lanes.get_mut(direction).needs_rules = false;
        apply_rules(shared, direction);
        publish_attachment(shared, direction);
    } else {
        shared.lanes.get_mut(direction).needs_rules = true;
    }
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
    shared.notify(AudioToUi::Attached {
        direction,
        node_name: None,
    });
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
                "PipeWire {}: the output lane's playback stream {}",
                info.version(),
                if scheduled {
                    "is passive and sleeps with the sink"
                } else {
                    "is put to sleep by hand while the sink has no clients"
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

    let mut guard = shared.borrow_mut();
    // Everything learned from a server is learned again from this one. The probes went with the
    // last session ([`close_session`]); emptying them here as well means a probe can only ever
    // belong to the connection being made, whatever path led here. The clock is assumed to be the
    // default until this server's `settings` object says otherwise.
    guard.devices.clear();
    guard.node_probes.clear();
    guard.aec.forget_sources();
    guard.defaults = PerDirection::default();
    guard.clock = GraphClock::default();
    guard.state = State::Connecting;
    guard.barrier = Barrier::Registry;
    guard.session = Some(Session {
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
        lane.attempts = 0;
        lane.next_attempt = now;
        lane.last_error = None;
    }
    Ok(())
}

/// End the connection and everything made on it, in the only order that is safe.
///
/// Both lanes' pairs first: every stream holds a reference to the core, so a pair left behind
/// would keep the dead connection open under the next one — and dropping a pair is also what
/// hands its DSP back through the lane's recycle channel. Then the node probes, each listener
/// before its proxy ([`NodeProbe`]), while the core they were bound on is still there to destroy
/// them. Then the session itself. Each lane keeps whether it is enabled, its memory and its DSP,
/// so whatever comes next — a reconnect, or the end of the thread — finds the lanes as the user
/// left them.
fn close_session(shared: &mut Shared) {
    // The echo canceller first. It is on a connection of its own (`aec`), so nothing here depends
    // on it, but it was loaded for this server and these devices, and it is loaded again for the
    // next ones once the lanes are back. Its source goes from the registry with the rest.
    shared.aec.unload();
    shared.aec.forget_sources();
    for (_, lane) in shared.lanes.iter_mut() {
        lane.nodes = None;
        lane.built_at = None;
        lane.status.clear();
    }
    shared.node_probes.clear();
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
    for (_, lane) in shared.lanes.iter_mut() {
        lane.previous_names.clear();
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

        // 4. Each lane on its own: its streams' errors, its format check, its rules, its
        //    published delay.
        let now = Instant::now();
        for direction in DeviceDirection::ALL {
            supervise_lane(&mut guard, direction, now);
        }

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

    // c. Rules and node creation, once the lane's backoff allows.
    let lane = shared.lanes.get_mut(direction);
    lane.forgive_if_stable(now);
    if lane.needs_rules && now >= lane.next_attempt && shared.ready() {
        shared.lanes.get_mut(direction).needs_rules = false;
        apply_rules(shared, direction);
    }

    // d. Keep the published delay honest. Switching the denoiser on adds ten milliseconds, and
    //    only the audio thread knows it happened.
    republish_latency(shared, direction);

    // e. Put a NODE 2 paced by hand to sleep once its NODE 1 has been paused long enough — the
    //    one step of the pacing that waits for time rather than for an event — and catch any wake
    //    the channel could not deliver.
    pace_second_node(shared, direction, now);
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
fn add_device(shared: &mut Shared, device: DeviceInfo) {
    let direction = device.direction;
    shared
        .devices
        .retain(|existing| existing.object_id != device.object_id);
    shared.devices.push(device);
    shared.needs_publish = true;
    shared.mark_lane_for_rules(direction);
}

fn on_global_remove(shared: &Rc<RefCell<Shared>>, id: u32) {
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    if guard.aec.source_removed(id) {
        log::debug!("the echo canceller's source went away ({id})");
        return;
    }
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
    Some(removed)
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

fn on_metadata_property(shared: &Rc<RefCell<Shared>>, key: Option<&str>, value: Option<&str>) {
    let Some(key) = key else {
        return;
    };
    let Ok(mut guard) = shared.try_borrow_mut() else {
        return;
    };
    for direction in DeviceDirection::ALL {
        if key == devices::default_key(direction) {
            guard.defaults.get_mut(direction).current =
                value.and_then(devices::parse_default_node_name);
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

/// Whether `node_name` is one of the nodes FxSound makes ([`OUR_NODE_NAMES`]): never a default
/// to remember, nor one to hand back to.
fn is_ours(node_name: &str) -> bool {
    OUR_NODE_NAMES.contains(&node_name)
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
}

/// Choose a lane's device and make sure its pair is attached to it (`docs/spec/12-audio-io.md`
/// §19.5, per direction as §28.5 describes).
///
/// A disabled lane is left alone, whatever asked: it has no pair and must not grow one.
fn apply_rules(shared: &mut Shared, direction: DeviceDirection) {
    if !shared.lanes.get(direction).enabled {
        return;
    }
    let ours = our_node_name(direction);
    let selection = devices::choose_device(
        &shared.devices,
        direction,
        ours,
        shared.defaults.get(direction).current.as_deref(),
        &shared.lanes.get(direction).previous_names,
        shared.memory.get(direction),
    );
    shared.lanes.get_mut(direction).previous_names = shared
        .devices
        .iter()
        .filter(|d| d.direction == direction)
        .map(|d| d.name.clone())
        .collect();

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
    let already = shared
        .lanes
        .get(direction)
        .nodes
        .as_ref()
        .is_some_and(|nodes| {
            nodes.target == target.name && nodes.format == format && nodes.via == route
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
/// DSP runs on.
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

    if target.is_refused_mono() {
        return Err(AudioError::NoValidOutput);
    }
    let direction = target.direction;
    let PairFormat {
        channels,
        rate,
        positions,
    } = format;
    let quantum = DEFAULT_QUANTUM_FRAMES.min(MAX_QUANTUM_FRAMES as u32);
    let latency = format!("{quantum}/{rate}");

    let (passive, pace) = idle_plan(direction, shared.link_groups_scheduled.get());
    let wake = pace.and(shared.wake.clone());

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
            stream_props(direction, via.unwrap_or(&target.name), &latency, false),
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
    dsp.set_source_rate(target.native_rate());
    // The subwoofer and the front pair: the music chain's business, ignored by the voice chain's
    // lane, which has no stage that mixes one channel into another.
    dsp.set_layout(positions.lfe_index(), positions.front_pair());
    // Read before the engine is handed to the node's user data, where it can no longer be reached
    // from the main loop.
    let dsp_latency_frames = dsp.latency_frames();
    // `set_format` returns early when neither the rate nor the channel count moved, so switching
    // between two devices that are both 48 kHz stereo — the common case — would otherwise carry
    // the previous device's filter history, reverb tail and leveller gain straight into the new
    // one. The engine is recycled deliberately, but its *state* should not be.
    dsp.reset();

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
                // A passive NODE 2 stopped in the same cycle, and what it had not played yet is
                // the tail of a sound that has ended (module docs, "Idle").
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
        first,
        second,
        target: target.name.clone(),
        via,
        format,
        pace,
    })
}

/// How a new pair keeps its NODE 2 from running while nothing plays into its NODE 1 (module docs,
/// "Idle"): whether NODE 2 is declared passive, and the pace it is kept to by hand when it is not.
///
/// The output lane's NODE 2 is passive where the server runs a link-group together and paced by
/// hand where it does not. The input lane's pair is left running: its NODE 1 is what makes the
/// microphone produce anything, and a recorder may link to its NODE 2 at any moment.
fn idle_plan(
    direction: DeviceDirection,
    link_groups_scheduled: bool,
) -> (bool, Option<SecondNodePace>) {
    match direction {
        DeviceDirection::Input => (false, None),
        DeviceDirection::Output if link_groups_scheduled => (true, None),
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

/// The properties of the stream that touches the real device — the playback stream of the output
/// lane, the capture stream of the input lane (`docs/spec/12-audio-io.md` §20 NODE 2, §28.2
/// NODE 1). `target` is the real device's `node.name`; `passive` says whether the stream's link
/// to it may keep it awake (module docs, "Idle").
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
        *pw::keys::NODE_DONT_RECONNECT => "false",
        // Passive, the playback stream's link no longer keeps the speakers running; what runs it
        // is the virtual sink, whenever an application's link makes that run, because the server
        // runs a link-group together. That is true from PipeWire 0.3.68 on, and only there does
        // the caller ask for it. On an older server the two nodes are not linked in the graph —
        // the ring between them is invisible to it — and nothing would ever wake a passive NODE 2
        // but the speakers running for somebody else: FxSound would play nothing. The capture
        // stream is never passive. It is what makes the microphone produce anything at all.
        *pw::keys::NODE_PASSIVE        => if passive { "true" } else { "false" },
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

/// NODE 1's `param_changed`. Main loop, never the data thread — which is exactly why the
/// process callbacks can assume `channels` is constant for the life of a buffer.
fn on_sink_format(_stream: &pw::stream::Stream, data: &mut SinkData, id: u32, param: Option<&Pod>) {
    adopt_sink_format(data, id, param);
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
        // the ring drain and NODE 2 emit silence, which is the same outcome with less work. No
        // channels is no format yet, or one the ring was not built for ([`adopt_sink_format`]).
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

/// The device list as the GUI wants it: every output sorted by description, then every input
/// sorted by description. The GUI draws its section headers off that grouping, so the order is
/// part of the contract. Mono *outputs* are left out (they could never be chosen); mono inputs
/// stay in.
fn published_devices(shared: &Shared) -> Vec<AudioDevice> {
    let mut published = Vec::with_capacity(shared.devices.len());
    for (direction, default) in shared.defaults.iter() {
        let default = default.current.as_deref();
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
    /// ring already dry, or whose `Paused` reaches the main loop only after the new sound has
    /// started, loses nothing of the new sound to it.
    #[test]
    fn a_stale_mark_never_costs_the_next_sound_a_sample() {
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
    fn only_the_output_lane_idles_and_only_an_old_server_needs_it_done_by_hand() {
        assert_eq!(
            idle_plan(DeviceDirection::Output, true),
            (true, None),
            "a server that runs a link-group together idles a passive NODE 2 by itself"
        );
        assert_eq!(
            idle_plan(DeviceDirection::Output, false),
            (false, Some(SecondNodePace::new())),
            "an older one gets an ordinary NODE 2, paced by hand"
        );
        for scheduled in [true, false] {
            assert_eq!(
                idle_plan(DeviceDirection::Input, scheduled),
                (false, None),
                "the microphone's pair is left running"
            );
        }
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

        let capture = stream_props(DeviceDirection::Input, "alsa_input.mic", "512/48000", false);
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
        assert_eq!(capture.get("node.passive"), Some("false"));
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
        for name in OUR_NODE_NAMES {
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
}
