//! The DSP of each lane: what NODE 1's `process()` runs, together with the audio thread's ends of
//! the paths whose other ends the GUI's [`EngineHandle`](crate::EngineHandle) holds.
//!
//! # One holder per lane
//!
//! A lane is one direction's worth of engine state (`docs/0.4.0-design.md` §1.1), and its DSP
//! is the part that crosses into the data thread. So each lane gets its own holder —
//! [`OutputDsp`] runs the music chain, [`InputDsp`] the voice chain — and each owns *everything*
//! the chain is addressed through: its parameter snapshot, its meters buffer, its event queue and
//! its scratch. Nothing the GUI sends to one lane can reach the other lane's chain, and nothing
//! one chain measures can overwrite what the other published. That is a property of the types,
//! not of a switch that has to be set right: there is no `direction` field to consult and no
//! branch in the audio path that could pick the wrong chain.
//!
//! [`LaneDsp`] is the enum NODE 1's user data carries, because the callback is the same code in
//! both directions (`crate::engine`, "One engine, two directions"). Its methods forward to the
//! variant; the only branch left is the one the enum itself is.
//!
//! # Mute
//!
//! Both snapshots carry a `mute`, and the engine has one of its own for each lane, which it sets
//! while the system sleeps and until each lane is attached again after the wake (U13,
//! `crate::engine` "Sleep") — the GUI's snapshot is the GUI's, and the engine does not write it.
//! The lane is silent while either says so ([`LaneDsp::set_system_mute`]). Both are honoured here,
//! after the chain, rather than inside the DSP crate's engines: the chain still runs on every block —
//! its filters, leveller and denoiser keep following what comes in — and only what leaves for the
//! ring is replaced by silence. So unmuting joins a chain already in step with the programme, and
//! the engines stay what they were, a pure function of their input and their snapshot. The
//! meters still describe the chain's output, not the silence; nobody reads them while the system
//! sleeps. Upstream does the same thing one step later — its mute keeps capturing and processing
//! and only skips the playback call (`AudioPassthruPrivate.cpp:568`) — and its cut is as hard as
//! this one: a de-click ramp belongs with the fade in from silence a new pair gets ([`Tail`],
//! U10), which is where it would go.
//!
//! # After the chain
//!
//! Two gains follow the chain and the mute, in both lanes, and are one stage ([`Tail`]):
//!
//! * **The virtual node's volume** (U11). The adapter in front of a sink applies the node's
//!   volume before `process()`, where the volume leveller undoes up to eight of the twenty
//!   decibels a slider asks for (`crate::volume`). So the adapter's range is clamped to unity and
//!   the node's `channelVolumes` are applied here, after the chain, per channel. A change is
//!   ramped linearly across one block, from the gain the last block ended on, so a slider being
//!   dragged moves the level smoothly rather than in steps a block long.
//! * **A fade in from silence on a new pair** (U10): the first [`FADE_IN_SECONDS`] of whatever a
//!   new pair processes rise linearly from zero. A pair is new when the device changed, and
//!   whatever the new device's volume turns out to be, the first sound on it does not arrive as a
//!   step — nor does the tail of a limiter that last saw another device.
//!
//! # Real-time rules
//!
//! Everything reached from [`LaneDsp::refresh`], [`LaneDsp::process_bytes`] and
//! [`LaneDsp::publish_meters`] runs under `SCHED_FIFO`: no allocation, no lock, no logging, no
//! panic. The paths are triple buffers and bounded array channels, whose ends are wait-free; the
//! scratch is sized once for the worst case and never resized. The methods that build or drop
//! heap — [`InputDsp::set_spec`] above all — are main-loop only and say so.

use crossbeam_channel::{Receiver, Sender};
use fxsound_core::DeviceDirection;
use fxsound_core::messages::{DspEvent, DspParams, InputDspParams, Meters};
use fxsound_dsp::Engine as DspEngine;
use fxsound_dsp::{ChainSpec, InputEngine};
use triple_buffer::{Input, Output};

use crate::per_direction::PerDirection;
use crate::volume::CHANNELS;
use crate::{DEFAULT_SAMPLE_RATE, MAX_CHANNELS, MAX_QUANTUM_FRAMES, MIN_CHANNELS};

/// How long a new pair takes to rise from silence to its volume: inside the 20–50 ms the
/// upstream review asked for, long enough that a first sound is not a click and short enough to
/// be over before a listener could call it a fade.
pub(crate) const FADE_IN_SECONDS: f32 = 0.03;

/// The DSP state of one lane, kept across reconnects and pairs of nodes.
///
/// When PipeWire restarts — or the lane is re-attached to another device — the streams and
/// their user data are destroyed and rebuilt; the `triple_buffer` endpoints and the event queue
/// must not be, because their other halves live in the GUI's
/// [`EngineHandle`](crate::EngineHandle) and cannot be re-paired. So this is handed back to the
/// main loop by NODE 1's user data on `Drop`, through the lane's own recycle channel, and moved
/// into the next pair's user data. Keeping the engine with them is a bonus: its filter state and
/// its scratch survive a server restart too.
pub(crate) enum LaneDsp {
    Output(OutputDsp),
    Input(InputDsp),
}

impl LaneDsp {
    /// The lane this DSP belongs to — fixed at construction, never switched.
    pub(crate) const fn direction(&self) -> DeviceDirection {
        match self {
            Self::Output(_) => DeviceDirection::Output,
            Self::Input(_) => DeviceDirection::Input,
        }
    }

    /// The voice-chain holder, when this is the input lane's.
    pub(crate) const fn as_input_mut(&mut self) -> Option<&mut InputDsp> {
        match self {
            Self::Output(_) => None,
            Self::Input(dsp) => Some(dsp),
        }
    }

    pub(crate) fn set_format(&mut self, sample_rate: f32, channels: usize) {
        match self {
            Self::Output(dsp) => dsp.set_format(sample_rate, channels),
            Self::Input(dsp) => dsp.set_format(sample_rate, channels),
        }
        self.tail_mut().set_rate(sample_rate);
    }

    /// This lane's post-chain stage.
    const fn tail_mut(&mut self) -> &mut Tail {
        match self {
            Self::Output(dsp) => &mut dsp.tail,
            Self::Input(dsp) => &mut dsp.tail,
        }
    }

    /// The gains the node's volume asks of each channel, read from the lane's
    /// `crate::volume::LaneVolume` once a block. The block after this is ramped to them.
    #[inline]
    pub(crate) fn set_volume(&mut self, gains: &[f32; CHANNELS]) {
        self.tail_mut().set_targets(gains);
    }

    /// Whether the engine holds the lane silent, whatever the snapshot's own `mute` says: read by
    /// NODE 1 from the lane once a block, after [`Self::refresh`] (see the module's "Mute").
    #[inline]
    pub(crate) fn set_system_mute(&mut self, muted: bool) {
        match self {
            Self::Output(dsp) => dsp.system_muted = muted,
            Self::Input(dsp) => dsp.system_muted = muted,
        }
    }

    /// A new pair of nodes: start it from silence, and at its volume rather than ramping there from
    /// the gains the last pair ended on. Main loop, before the DSP is handed to the pair.
    pub(crate) fn begin_pair(&mut self) {
        self.tail_mut().begin_pair();
    }

    /// Where the subwoofer and the front pair sit in the negotiated layout. The output lane's
    /// business only — see [`OutputDsp::set_layout`]; the input lane has nothing to place.
    pub(crate) fn set_layout(&mut self, lfe: Option<usize>, front_pair: Option<(usize, usize)>) {
        if let Self::Output(dsp) = self {
            dsp.set_layout(lfe, front_pair);
        }
    }

    /// What the source really runs at. The input lane's business only — see
    /// [`InputDsp::set_source_rate`]; a sink's rate is the one NODE 1 runs at anyway.
    pub(crate) fn set_source_rate(&mut self, rate: Option<f32>) {
        if let Self::Input(dsp) = self {
            dsp.set_source_rate(rate);
        }
    }

    pub(crate) fn latency_frames(&self) -> usize {
        match self {
            Self::Output(dsp) => dsp.engine.latency_frames(),
            Self::Input(dsp) => dsp.engine.latency_frames(),
        }
    }

    /// Clear this lane's chain history. The other lane's is not reachable from here, which is the
    /// point.
    pub(crate) fn reset(&mut self) {
        match self {
            Self::Output(dsp) => dsp.engine.reset(),
            Self::Input(dsp) => dsp.reset(),
        }
    }

    /// Take the newest parameter snapshot and drain the event queue. Once per block, on the
    /// audio thread.
    #[inline]
    pub(crate) fn refresh(&mut self) {
        match self {
            Self::Output(dsp) => dsp.refresh(),
            Self::Input(dsp) => dsp.refresh(),
        }
    }

    /// Apply whatever events are waiting, without touching the parameters.
    ///
    /// For a lane whose DSP is on the main loop — no nodes — so that an event the GUI sent it is
    /// applied rather than left to pile up against [`crate::EVENT_QUEUE_LEN`] until the lane has a
    /// pair again. On the audio thread [`Self::refresh`] does the same as part of every block.
    pub(crate) fn drain_events(&mut self) {
        match self {
            Self::Output(dsp) => dsp.drain_events(),
            Self::Input(dsp) => dsp.drain_events(),
        }
    }

    /// The largest block [`Self::process_bytes`] takes in one call, in bytes.
    #[inline]
    pub(crate) const fn block_bytes(&self) -> usize {
        let samples = match self {
            Self::Output(dsp) => dsp.scratch.len(),
            Self::Input(dsp) => dsp.scratch.len(),
        };
        samples * size_of::<f32>()
    }

    /// De-serialise one block of little-endian `f32` into the scratch buffer, run this lane's
    /// chain over it and hand it back for the ring. `None` when the block is larger than the
    /// scratch or the channel count is zero, which is the caller's signal to stop.
    #[inline]
    pub(crate) fn process_bytes(&mut self, block: &[u8], channels: usize) -> Option<&[f32]> {
        match self {
            Self::Output(dsp) => dsp.process_bytes(block, channels),
            Self::Input(dsp) => dsp.process_bytes(block, channels),
        }
    }

    #[inline]
    pub(crate) fn meters(&self) -> Meters {
        match self {
            Self::Output(dsp) => dsp.engine.meters(),
            Self::Input(dsp) => dsp.engine.meters(),
        }
    }

    /// Hand this lane's current meters to the GUI, through this lane's own buffer. One atomic
    /// swap.
    #[inline]
    pub(crate) fn publish_meters(&mut self) {
        let meters = self.meters();
        match self {
            Self::Output(dsp) => dsp.meters.write(meters),
            Self::Input(dsp) => dsp.meters.write(meters),
        }
    }
}

impl std::fmt::Debug for LaneDsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output(dsp) => f
                .debug_struct("OutputDsp")
                .field("scratch", &dsp.scratch.len())
                .finish_non_exhaustive(),
            Self::Input(dsp) => f
                .debug_struct("InputDsp")
                .field("spec", &dsp.engine.spec())
                .field("parked", &dsp.parked.is_some())
                .field("scratch", &dsp.scratch.len())
                .finish_non_exhaustive(),
        }
    }
}

/// Both lanes' DSP, built from the audio thread's ends of the paths, together with the main
/// loop's ends of the voice chain's hand-over.
///
/// The one place a lane's paths are matched to its chain: from here on each travels only with
/// its own holder.
pub(crate) fn build(
    params: Output<DspParams>,
    input_params: Output<InputDspParams>,
    meters: PerDirection<Input<Meters>>,
    events: PerDirection<Receiver<DspEvent>>,
) -> (PerDirection<LaneDsp>, ChainHandover) {
    let (handover, replacement, retired) = ChainHandover::new();
    let dsp = PerDirection {
        output: LaneDsp::Output(OutputDsp::new(params, meters.output, events.output)),
        input: LaneDsp::Input(InputDsp::new(
            input_params,
            meters.input,
            events.input,
            replacement,
            retired,
        )),
    };
    (dsp, handover)
}

/// The output lane's DSP: the music chain and the paths that address it.
pub(crate) struct OutputDsp {
    /// Boxed like the voice engine, so the two holders are of a size and moving a [`LaneDsp`]
    /// through its recycle channel is a handful of words rather than a filter bank.
    engine: Box<DspEngine>,
    params: Output<DspParams>,
    meters: Input<Meters>,
    events: Receiver<DspEvent>,
    /// Interleaved de-serialisation buffer, sized for the worst case and never resized.
    scratch: Vec<f32>,
    /// The newest snapshot's `mute`, as of the last [`Self::refresh`]. See the module's "Mute".
    muted: bool,
    /// The engine's own mute for the lane, as of the last [`LaneDsp::set_system_mute`].
    system_muted: bool,
    /// The node's volume and a new pair's fade in. See the module's "After the chain".
    tail: Tail,
}

impl OutputDsp {
    fn new(params: Output<DspParams>, meters: Input<Meters>, events: Receiver<DspEvent>) -> Self {
        Self {
            engine: Box::new(DspEngine::new(
                DEFAULT_SAMPLE_RATE as f32,
                MAX_QUANTUM_FRAMES,
                MIN_CHANNELS as usize,
            )),
            params,
            meters,
            events,
            scratch: worst_case_scratch(),
            muted: false,
            system_muted: false,
            tail: Tail::new(),
        }
    }

    fn set_format(&mut self, sample_rate: f32, channels: usize) {
        self.engine.set_format(sample_rate, channels);
    }

    /// Where the subwoofer and the front pair sit in the negotiated layout.
    ///
    /// Output only, and not because the input side was forgotten: the voice chain has no stage
    /// that mixes one channel into another, so there is no layout for it to get wrong.
    fn set_layout(&mut self, lfe: Option<usize>, front_pair: Option<(usize, usize)>) {
        self.engine.set_lfe_channel(lfe);
        self.engine.set_front_pair(front_pair);
    }

    #[inline]
    fn refresh(&mut self) {
        let params = self.params.read();
        self.engine.apply(params);
        self.muted = params.mute;
        self.drain_events();
    }

    #[inline]
    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.engine.handle_event(event);
        }
    }

    /// See [`LaneDsp::process_bytes`]. The conversion lives in the holder rather than in the
    /// callback because it writes into `self.scratch`: a caller holding that slice and calling a
    /// `&mut self` method would be borrowing the whole struct twice. Inside one method the
    /// compiler sees the fields for what they are — disjoint.
    #[inline]
    fn process_bytes(&mut self, block: &[u8], channels: usize) -> Option<&[f32]> {
        let scratch = decode(&mut self.scratch, block, channels)?;
        self.engine.process(scratch, channels);
        silence_if(self.muted || self.system_muted, scratch);
        self.tail.apply(scratch, channels);
        Some(scratch)
    }
}

/// The input lane's DSP: the voice chain, the paths that address it, and the audio thread's ends
/// of the hand-over that swaps the chain for another one.
pub(crate) struct InputDsp {
    /// Boxed so that swapping it for a replacement is one pointer move on the audio thread — see
    /// [`Self::adopt_replacement`] — rather than a copy of nine stages and a spectrum analyser.
    ///
    /// The engine is also where the source rate lives ([`Self::set_source_rate`]): one copy, so a
    /// replacement inherits exactly what the running engine was told.
    engine: Box<InputEngine>,
    params: Output<InputDspParams>,
    meters: Input<Meters>,
    events: Receiver<DspEvent>,
    /// A voice engine built on the main loop for a chain the preset named, waiting for the audio
    /// thread to take it. Bounded, so `try_recv` is one atomic load per block and never allocates.
    replacement: Receiver<Box<InputEngine>>,
    /// The engine a replacement displaced, on its way back to the main loop to be dropped there:
    /// its stages own heap — eight network states in the denoiser alone — and freeing them is as
    /// much a real-time violation as allocating them was.
    retired: Sender<Box<InputEngine>>,
    /// A retired engine `retired` had no room for. Held rather than dropped, for the reason above,
    /// and handed back on a later block; while it is here no further replacement is taken, so the
    /// audio thread never owns more than two engines at once.
    parked: Option<Box<InputEngine>>,
    /// Interleaved de-serialisation buffer, sized for the worst case and never resized.
    scratch: Vec<f32>,
    /// The newest snapshot's `mute`, as of the last [`Self::refresh`]. See the module's "Mute".
    muted: bool,
    /// The engine's own mute for the lane, as of the last [`LaneDsp::set_system_mute`].
    system_muted: bool,
    /// The node's volume and a new pair's fade in. See the module's "After the chain".
    tail: Tail,
}

impl InputDsp {
    fn new(
        params: Output<InputDspParams>,
        meters: Input<Meters>,
        events: Receiver<DspEvent>,
        replacement: Receiver<Box<InputEngine>>,
        retired: Sender<Box<InputEngine>>,
    ) -> Self {
        Self {
            engine: Box::new(InputEngine::new(
                DEFAULT_SAMPLE_RATE as f32,
                MAX_QUANTUM_FRAMES,
                MIN_CHANNELS as usize,
            )),
            params,
            meters,
            events,
            replacement,
            retired,
            parked: None,
            scratch: worst_case_scratch(),
            muted: false,
            system_muted: false,
            tail: Tail::new(),
        }
    }

    /// The voice engine this lane runs, for tests that follow a chain through the main loop.
    #[cfg(test)]
    pub(crate) fn engine(&self) -> &InputEngine {
        &self.engine
    }

    /// Rebuild the voice chain for a spec, in place. **Main loop only**, and only while this
    /// struct is on the main loop — between pairs of nodes — because it allocates the new stages
    /// and drops the old ones; with a pair up the same change goes through [`ChainHandover`].
    ///
    /// Anything still waiting in `replacement` was built for a spec the main loop has since moved
    /// on from, and would otherwise be adopted by the next pair as if it were current; it is
    /// drained and dropped here, where dropping is allowed. So is a parked engine.
    pub(crate) fn set_spec(&mut self, spec: ChainSpec) {
        while self.replacement.try_recv().is_ok() {}
        self.parked = None;
        self.engine.set_spec(spec);
    }

    /// Take over a voice engine the main loop built for a chain the preset named, and send the
    /// one it displaces back to be dropped there.
    ///
    /// Real-time safe: `try_recv` and `try_send` on bounded channels, two pointer moves, and a
    /// `set_format` that is a no-op when the main loop built the replacement at the negotiated
    /// format (it reads the same counters `on_sink_format` writes) and a redesign — never an
    /// allocation — when it did not. The replacement's parameters catch up on the `apply` that
    /// follows in [`Self::refresh`], because it starts from the defaults and the snapshot is
    /// state. Nothing is dropped: a displaced engine that cannot be sent is parked, and while one
    /// is parked no further replacement is taken.
    #[inline]
    fn adopt_replacement(&mut self) {
        self.hand_back_retired();
        if self.parked.is_some() {
            return;
        }
        let Ok(mut fresh) = self.replacement.try_recv() else {
            return;
        };
        fresh.set_format(self.engine.sample_rate(), self.engine.channels());
        fresh.set_source_rate(self.engine.source_rate());
        self.parked = Some(std::mem::replace(&mut self.engine, fresh));
        self.hand_back_retired();
    }

    #[inline]
    fn hand_back_retired(&mut self) {
        if let Some(old) = self.parked.take()
            && let Err(err) = self.retired.try_send(old)
        {
            self.parked = Some(err.into_inner());
        }
    }

    fn set_format(&mut self, sample_rate: f32, channels: usize) {
        self.engine.set_format(sample_rate, channels);
    }

    /// What the target's properties say it really runs at — [`crate::DeviceInfo::native_rate`] —
    /// as opposed to the 48 kHz NODE 1 negotiates for every microphone. Set when the nodes are
    /// built, like the format; a replacement engine inherits it in [`Self::adopt_replacement`].
    fn set_source_rate(&mut self, rate: Option<f32>) {
        self.engine.set_source_rate(rate);
    }

    fn reset(&mut self) {
        self.engine.reset();
    }

    #[inline]
    fn refresh(&mut self) {
        self.adopt_replacement();
        let params = self.params.read();
        self.engine.apply(params);
        self.muted = params.mute;
        self.drain_events();
    }

    #[inline]
    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.engine.handle_event(event);
        }
    }

    /// See [`OutputDsp::process_bytes`].
    #[inline]
    fn process_bytes(&mut self, block: &[u8], channels: usize) -> Option<&[f32]> {
        let scratch = decode(&mut self.scratch, block, channels)?;
        self.engine.process(scratch, channels);
        silence_if(self.muted || self.system_muted, scratch);
        self.tail.apply(scratch, channels);
        Some(scratch)
    }
}

/// The main loop's ends of the voice-chain hand-over, the one thing a voice preset can change
/// that is not a parameter: the *set* of stages, which has to be allocated.
///
/// The pattern is the recycle channel's, in the other direction. The [`InputDsp`] lives in NODE
/// 1's user data while the input pair is up, out of the main loop's reach, so a chain it should
/// now be running is built there and sent over; the audio thread swaps it in and sends the old one
/// back through `retired` to be dropped on the main loop. Both channels are bounded — array
/// channels, whose `try_send` and `try_recv` never allocate — and small: one replacement in
/// flight is all a preset change ever needs, and a second one only means the user clicked twice
/// within a block.
pub(crate) struct ChainHandover {
    pub(crate) replacement: Sender<Box<InputEngine>>,
    pub(crate) retired: Receiver<Box<InputEngine>>,
}

impl ChainHandover {
    /// One replacement may wait for the audio thread at a time; the supervisor tries again on the
    /// next tick when the slot is still full.
    const REPLACEMENTS_IN_FLIGHT: usize = 1;
    /// Room for a retired engine and a parked one, so the audio thread's `try_send` cannot find
    /// the channel full as long as the main loop drains it before sending a replacement — which
    /// `reconcile_input_chain` does. `parked` covers the case anyway.
    pub(crate) const RETIRED_IN_FLIGHT: usize = 2;

    /// The main loop's ends, with the audio thread's `(replacement receiver, retired sender)` to
    /// hand to [`InputDsp::new`].
    pub(crate) fn new() -> (Self, Receiver<Box<InputEngine>>, Sender<Box<InputEngine>>) {
        let (replacement_tx, replacement_rx) =
            crossbeam_channel::bounded(Self::REPLACEMENTS_IN_FLIGHT);
        let (retired_tx, retired_rx) = crossbeam_channel::bounded(Self::RETIRED_IN_FLIGHT);
        (
            Self {
                replacement: replacement_tx,
                retired: retired_rx,
            },
            replacement_rx,
            retired_tx,
        )
    }
}

/// The stage after the chain and the mute: the virtual node's volume, per channel, and the fade in
/// from silence that starts a new pair. See the module's "After the chain".
///
/// Real-time safe: fixed-size arrays and counters, a multiply per sample, no branch that can
/// panic. A channel past [`CHANNELS`] — which the pair's clamp to eight never produces — is left
/// as it is rather than indexed.
#[derive(Debug)]
pub(crate) struct Tail {
    /// The gain each channel ended the last block on.
    current: [f32; CHANNELS],
    /// The gain each channel is ramped to across the next block.
    target: [f32; CHANNELS],
    /// Frames of the fade in done so far; the fade is over once this reaches `fade_len`.
    fade_done: u32,
    /// The fade's length in frames, at the rate the pair runs.
    fade_len: u32,
    sample_rate: f32,
}

impl Tail {
    const fn new() -> Self {
        Self {
            current: [1.0; CHANNELS],
            target: [1.0; CHANNELS],
            fade_done: 0,
            fade_len: 0,
            sample_rate: DEFAULT_SAMPLE_RATE as f32,
        }
    }

    fn set_rate(&mut self, sample_rate: f32) {
        if sample_rate.is_finite() && sample_rate > 0.0 {
            self.sample_rate = sample_rate;
        }
    }

    /// Start a new pair: from silence, at the gains it was told last rather than ramping there.
    fn begin_pair(&mut self) {
        self.current = self.target;
        self.fade_len = ((self.sample_rate * FADE_IN_SECONDS) as u32).max(1);
        self.fade_done = 0;
    }

    #[inline]
    fn set_targets(&mut self, gains: &[f32; CHANNELS]) {
        for (target, &gain) in self.target.iter_mut().zip(gains) {
            // The lane's volume is sanitised where it is written; a gain that is not a number
            // could only come from a bug there, and silence is its only safe reading.
            *target = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
        }
    }

    /// Whether the fade in still has frames to go.
    const fn fading(&self) -> bool {
        self.fade_done < self.fade_len
    }

    /// Apply the ramp to the target gains and whatever is left of the fade in to one block of
    /// interleaved frames.
    #[inline]
    fn apply(&mut self, block: &mut [f32], channels: usize) {
        let Some(frames) = block.len().checked_div(channels) else {
            return;
        };
        if frames == 0 {
            return;
        }
        let ramping = self.current != self.target;
        if !ramping && !self.fading() && self.current.iter().all(|&gain| gain == 1.0) {
            return;
        }
        let step = 1.0 / frames as f32;
        for (index, frame) in block.chunks_exact_mut(channels).enumerate() {
            let fade = if self.fading() {
                let fade = self.fade_done as f32 / self.fade_len as f32;
                self.fade_done += 1;
                fade
            } else {
                1.0
            };
            // Reaches the target on the block's last frame.
            let along = (index + 1) as f32 * step;
            for ((sample, &from), &to) in frame.iter_mut().zip(&self.current).zip(&self.target) {
                *sample *= fade * (from + (to - from) * along);
            }
        }
        self.current = self.target;
    }
}

/// A de-serialisation buffer for the largest block either chain is ever handed: 2048 frames of
/// eight channels (`docs/spec/12-audio-io.md` §24). Allocated once, on the main loop.
fn worst_case_scratch() -> Vec<f32> {
    vec![0.0; MAX_QUANTUM_FRAMES * MAX_CHANNELS as usize]
}

/// Replace a processed block with silence while the lane is muted. After the chain, so the chain
/// has already seen the block; a `fill` of memory that exists, so nothing here can allocate or
/// panic.
#[inline]
fn silence_if(muted: bool, processed: &mut [f32]) {
    if muted {
        processed.fill(0.0);
    }
}

/// Read as many whole frames of little-endian `f32` out of `block` as fit, into the front of
/// `scratch`, and return that part of it. `None` when they do not fit or `channels` is zero.
#[inline]
fn decode<'a>(scratch: &'a mut [f32], block: &[u8], channels: usize) -> Option<&'a mut [f32]> {
    // `checked_div` rather than `/`: a zero channel count is the one input here that could panic,
    // and a panic on this thread takes the user's audio with it.
    let samples = (block.len() / size_of::<f32>()).checked_div(channels)? * channels;
    let scratch = scratch.get_mut(..samples)?;
    // `as_chunks` hands over fixed-size arrays, so the four-byte guarantee is in the type rather
    // than in a `try_from` the audio path has to check every sample.
    let (words, _) = block.as_chunks::<{ size_of::<f32>() }>();
    for (slot, raw) in scratch.iter_mut().zip(words) {
        *slot = f32::from_le_bytes(*raw);
    }
    Some(scratch)
}

#[cfg(test)]
pub(crate) mod tests {
    use fxsound_core::DeEsserMode;
    use triple_buffer::TripleBuffer;

    use super::*;
    use crate::DeviceInfo;

    /// Both lanes' DSP with nobody on the other end of their buffers, and the main loop's ends of
    /// the chain hand-over they were built with.
    pub(crate) fn lanes_for_tests() -> (PerDirection<LaneDsp>, ChainHandover) {
        let (_, params) = TripleBuffer::new(&DspParams::default()).split();
        let (_, input_params) = TripleBuffer::new(&InputDspParams::default()).split();
        let meters = PerDirection::from_fn(|_| TripleBuffer::new(&Meters::default()).split().0);
        let events =
            PerDirection::from_fn(|_| crossbeam_channel::bounded(crate::EVENT_QUEUE_LEN).1);
        build(params, input_params, meters, events)
    }

    /// The input lane's holder on its own, with its hand-over.
    fn input_dsp_for_tests() -> (InputDsp, ChainHandover) {
        let (lanes, handover) = lanes_for_tests();
        let LaneDsp::Input(dsp) = lanes.input else {
            unreachable!("the input slot holds the input lane's DSP");
        };
        (dsp, handover)
    }

    fn voice_engine(spec: ChainSpec) -> Box<InputEngine> {
        Box::new(InputEngine::new_with_spec(
            48_000.0,
            MAX_QUANTUM_FRAMES,
            1,
            spec,
        ))
    }

    /// The per-lane half of the crate's wait-free guarantee, asserted against the types: every
    /// path a holder reads or writes on the audio thread is a triple-buffer end or a bounded
    /// channel of its own, in both lanes. A lock or a shared queue here could only be added by
    /// changing a field's type, which this stops compiling.
    #[test]
    fn each_lanes_audio_thread_paths_are_wait_free_by_construction() {
        fn output_paths(dsp: &mut OutputDsp) {
            let _: &mut Output<DspParams> = &mut dsp.params;
            let _: &mut Input<Meters> = &mut dsp.meters;
            let _: &Receiver<DspEvent> = &dsp.events;
        }
        fn input_paths(dsp: &mut InputDsp) {
            let _: &mut Output<InputDspParams> = &mut dsp.params;
            let _: &mut Input<Meters> = &mut dsp.meters;
            let _: &Receiver<DspEvent> = &dsp.events;
            let _: &Receiver<Box<InputEngine>> = &dsp.replacement;
            let _: &Sender<Box<InputEngine>> = &dsp.retired;
        }
        let _ = (output_paths, input_paths);

        let (lanes, _handover) = lanes_for_tests();
        let LaneDsp::Input(input) = &lanes.input else {
            panic!("the input slot must hold the voice chain");
        };
        // Bounded, so `try_send` and `try_recv` never allocate a block and never park.
        assert_eq!(
            input.replacement.capacity(),
            Some(ChainHandover::REPLACEMENTS_IN_FLIGHT)
        );
        assert_eq!(
            input.retired.capacity(),
            Some(ChainHandover::RETIRED_IN_FLIGHT)
        );
        for (direction, dsp) in lanes.iter() {
            assert_eq!(dsp.direction(), direction, "each slot holds its own lane");
            assert_eq!(
                dsp.block_bytes(),
                MAX_QUANTUM_FRAMES * MAX_CHANNELS as usize * size_of::<f32>(),
                "sized for the worst case, once"
            );
        }
    }

    #[test]
    fn a_block_is_decoded_in_whole_frames_and_a_zero_channel_count_is_refused_not_divided_by() {
        let samples = [0.5_f32, -0.25, 1.0];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut scratch = [0.0_f32; 8];
        assert_eq!(
            decode(&mut scratch, &bytes, 2).map(|s| s.to_vec()),
            Some(vec![0.5, -0.25]),
            "the odd sample is not half a frame of audio"
        );
        assert!(decode(&mut scratch, &bytes, 0).is_none());
        let mut small = [0.0_f32; 1];
        assert!(
            decode(&mut small, &bytes, 1).is_none(),
            "more than fits is the caller's signal to stop"
        );
    }

    // ---- §4 of the 0.4.0 design: the voice chain's hand-over, on the audio thread's side

    /// With a pair up the engine is out of the main loop's reach, so the replacement travels:
    /// built on the main loop, adopted by the audio thread on its next block at the format the
    /// outgoing engine was running, and the displaced one sent back to be dropped here.
    #[test]
    fn a_replacement_engine_is_adopted_on_the_next_block_and_the_old_one_comes_back() {
        let (mut dsp, handover) = input_dsp_for_tests();
        dsp.set_format(48_000.0, 2);
        dsp.set_source_rate(Some(16_000.0));

        // Built at a different format on purpose: the audio thread, not the builder, knows what
        // NODE 1 negotiated.
        let fresh = Box::new(InputEngine::new_with_spec(
            44_100.0,
            MAX_QUANTUM_FRAMES,
            1,
            ChainSpec::podcast(),
        ));
        handover
            .replacement
            .try_send(fresh)
            .expect("one slot, and it is empty");
        dsp.refresh();

        assert_eq!(dsp.engine.spec(), ChainSpec::podcast());
        assert!(dsp.engine.chain().gate().is_none());
        assert_eq!(dsp.engine.sample_rate(), 48_000.0);
        assert_eq!(dsp.engine.channels(), 2);
        assert_eq!(dsp.engine.source_rate(), Some(16_000.0));
        let old = handover
            .retired
            .try_recv()
            .expect("the displaced engine came back");
        assert_eq!(old.spec(), ChainSpec::voice());
        assert!(dsp.parked.is_none());
        assert!(
            handover.retired.try_recv().is_err(),
            "exactly one engine came back"
        );
    }

    /// The capture stream is at 48 kHz whatever the microphone is, so a resampled headset looks
    /// like any other source to `set_format`; the properties know better, and what they say has
    /// to reach the de-esser for the adaptive mode to have anything to adapt to.
    #[test]
    fn a_bluetooth_headsets_native_rate_reaches_the_voice_engines_de_esser() {
        let props = [
            ("media.class", "Audio/Source"),
            ("node.name", "bluez_input.00_11_22_33_44_55.0"),
            ("device.bus", "bluetooth"),
            ("api.bluez5.profile", "headset-head-unit"),
        ];
        let headset = DeviceInfo::from_props(7, &|key: &str| {
            props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
        })
        .expect("a headset is a device");
        assert_eq!(headset.native_rate(), Some(16_000.0));

        let (lanes, _handover) = lanes_for_tests();
        let mut dsp = lanes.input;
        dsp.set_format(DEFAULT_SAMPLE_RATE as f32, 2);
        let mut params = InputDspParams {
            deesser_mode: DeEsserMode::Adaptive,
            ..InputDspParams::default()
        };
        params.sanitise();
        dsp.as_input_mut()
            .expect("the input lane")
            .engine
            .apply(&params);
        assert_eq!(
            dsp.meters().deesser_hz,
            5_500.0,
            "at 48 kHz the preset's corner stands"
        );

        dsp.set_source_rate(headset.native_rate());
        assert_eq!(
            dsp.as_input_mut()
                .expect("the input lane")
                .engine
                .source_rate(),
            Some(16_000.0)
        );
        assert_eq!(
            dsp.meters().deesser_hz,
            4_000.0,
            "a quarter of the headset's 16 kHz, not the 5500 Hz the preset asked for"
        );

        // Back on a microphone that says nothing, the stream rate is all there is to know.
        dsp.set_source_rate(None);
        assert_eq!(dsp.meters().deesser_hz, 5_500.0);
    }

    /// The same, for the microphone WirePlumber 0.5 actually offers: the loopback in front of the
    /// SCO source, with `bluez5.loopback` and no `api.bluez5.*` key to read a profile or a codec
    /// from (`create-loopback-node.lua:44-55`). And for WirePlumber 0.4's SCO source on a headset
    /// that negotiated CVSD, whose 8 kHz leave no sibilance band for the adaptive de-esser to
    /// split off.
    #[test]
    fn wireplumber_05s_loopback_and_a_cvsd_headset_reach_the_de_esser_at_their_own_bandwidth() {
        let parse = |props: &[(&str, &str)]| {
            DeviceInfo::from_props(7, &|key: &str| {
                props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
            })
            .expect("a microphone is a device")
        };
        let loopback = parse(&[
            ("media.class", "Audio/Source"),
            ("node.name", "bluez_input.00:11:22:33:44:55"),
            ("bluez5.loopback", "true"),
            ("device.id", "60"),
        ]);
        let cvsd = parse(&[
            ("media.class", "Audio/Source"),
            ("node.name", "bluez_input.00_11_22_33_44_55.0"),
            ("api.bluez5.profile", "headset-head-unit"),
            ("api.bluez5.codec", "cvsd"),
        ]);
        let swb = parse(&[
            ("media.class", "Audio/Source"),
            ("node.name", "bluez_input.00_11_22_33_44_55.0"),
            ("api.bluez5.profile", "headset-head-unit"),
            ("api.bluez5.codec", "lc3_swb"),
        ]);

        let (lanes, _handover) = lanes_for_tests();
        let mut dsp = lanes.input;
        dsp.set_format(DEFAULT_SAMPLE_RATE as f32, 2);
        let mut params = InputDspParams {
            deesser_mode: DeEsserMode::Adaptive,
            ..InputDspParams::default()
        };
        params.sanitise();
        dsp.as_input_mut()
            .expect("the input lane")
            .engine
            .apply(&params);

        dsp.set_source_rate(loopback.native_rate());
        assert_eq!(
            dsp.meters().deesser_hz,
            4_000.0,
            "the loopback is a headset's, at the wide band"
        );
        dsp.set_source_rate(cvsd.native_rate());
        assert_eq!(
            dsp.meters().deesser_hz,
            0.0,
            "at 8 kHz there is no sibilance band, and the stage stands aside"
        );
        dsp.set_source_rate(swb.native_rate());
        assert_eq!(
            dsp.meters().deesser_hz,
            5_500.0,
            "32 kHz leaves room for the corner the preset asked for"
        );
    }

    /// The source rate is the voice chain's business; handed to the music chain's lane it goes
    /// nowhere, and the subwoofer layout handed to the voice chain's lane likewise.
    #[test]
    fn each_lane_ignores_the_setting_that_belongs_to_the_other() {
        let (mut lanes, _handover) = lanes_for_tests();
        let before = lanes.output.meters();
        lanes.output.set_source_rate(Some(16_000.0));
        assert_eq!(lanes.output.meters(), before);

        let spec = |dsp: &mut LaneDsp| dsp.as_input_mut().map(|d| d.engine.spec());
        lanes.input.set_layout(Some(3), Some((0, 1)));
        assert_eq!(spec(&mut lanes.input), Some(ChainSpec::voice()));
        assert_eq!(
            spec(&mut lanes.output),
            None,
            "the music lane has no voice chain"
        );
    }

    /// The audio thread never drops an engine. When the return channel is full — the main loop
    /// has not drained it yet — the displaced engine is parked, and no further replacement is
    /// taken until the parked one has gone back.
    #[test]
    fn a_displaced_engine_is_parked_rather_than_dropped_while_the_return_channel_is_full() {
        let (mut dsp, handover) = input_dsp_for_tests();
        for _ in 0..ChainHandover::RETIRED_IN_FLIGHT {
            dsp.retired
                .try_send(voice_engine(ChainSpec::voice()))
                .expect("room for this many");
        }

        handover
            .replacement
            .try_send(voice_engine(ChainSpec::podcast()))
            .expect("empty slot");
        dsp.refresh();
        assert_eq!(dsp.engine.spec(), ChainSpec::podcast(), "taken");
        assert!(
            dsp.parked
                .as_ref()
                .is_some_and(|e| e.spec() == ChainSpec::voice()),
            "…and the displaced engine parked, because nothing could take it"
        );

        // A second replacement waits while one is parked.
        handover
            .replacement
            .try_send(voice_engine(ChainSpec::broadcast()))
            .expect("the slot was emptied by the adoption");
        dsp.refresh();
        assert_eq!(dsp.engine.spec(), ChainSpec::podcast(), "not yet");
        assert!(dsp.parked.is_some());

        // The main loop drains; the parked engine goes back and the second replacement is taken.
        while handover.retired.try_recv().is_ok() {}
        dsp.refresh();
        assert_eq!(dsp.engine.spec(), ChainSpec::broadcast());
        assert!(dsp.parked.is_none());
        let back: Vec<ChainSpec> = std::iter::from_fn(|| handover.retired.try_recv().ok())
            .map(|engine| engine.spec())
            .collect();
        assert_eq!(back, [ChainSpec::voice(), ChainSpec::podcast()]);
    }

    /// A replacement still in flight when its pair comes down was built for a spec the main loop
    /// may since have moved on from. `set_spec` — the in-place path the next `build_nodes` takes
    /// — drains it here rather than letting the next pair adopt it as if it were current.
    #[test]
    fn a_replacement_left_in_flight_is_dropped_on_the_main_loop_not_adopted_by_the_next_pair() {
        let (mut dsp, handover) = input_dsp_for_tests();
        handover
            .replacement
            .try_send(voice_engine(ChainSpec::podcast()))
            .expect("empty slot");

        dsp.set_spec(ChainSpec::broadcast());
        assert_eq!(dsp.engine.spec(), ChainSpec::broadcast());

        dsp.refresh();
        assert_eq!(
            dsp.engine.spec(),
            ChainSpec::broadcast(),
            "the stale podcast engine was drained, not adopted"
        );
    }

    // ---- the `mute` both snapshots carry (U13): silence after the chain ----

    /// Both lanes with the GUI's ends of their parameter buffers kept, so a test can publish a
    /// snapshot the way `EngineHandle::set_params` does.
    struct Wired {
        lanes: PerDirection<LaneDsp>,
        params: Input<DspParams>,
        input_params: Input<InputDspParams>,
        _handover: ChainHandover,
    }

    fn wired() -> Wired {
        let (params_in, params) = TripleBuffer::new(&DspParams::default()).split();
        let (input_params_in, input_params) = TripleBuffer::new(&InputDspParams::default()).split();
        let meters = PerDirection::from_fn(|_| TripleBuffer::new(&Meters::default()).split().0);
        let events =
            PerDirection::from_fn(|_| crossbeam_channel::bounded(crate::EVENT_QUEUE_LEN).1);
        let (mut lanes, handover) = build(params, input_params, meters, events);
        for (_, dsp) in lanes.iter_mut() {
            dsp.set_format(48_000.0, 2);
        }
        Wired {
            lanes,
            params: params_in,
            input_params: input_params_in,
            _handover: handover,
        }
    }

    /// A 1 kHz tone at −6 dBFS on both channels, one 480-frame block starting at `block_index`,
    /// as the little-endian bytes NODE 1 is handed.
    fn tone_block(block_index: usize) -> Vec<u8> {
        const FRAMES: usize = 480;
        (0..FRAMES)
            .flat_map(|frame| {
                let n = (block_index * FRAMES + frame) as f32;
                let sample = 0.5 * (std::f32::consts::TAU * 1_000.0 * n / 48_000.0).sin();
                [sample, sample]
            })
            .flat_map(f32::to_le_bytes)
            .collect()
    }

    /// One block through a lane, as the callback runs it: refresh, then process.
    fn run_block(dsp: &mut LaneDsp, block_index: usize) -> Vec<f32> {
        dsp.refresh();
        dsp.process_bytes(&tone_block(block_index), 2)
            .expect("a block that fits")
            .to_vec()
    }

    fn is_silent(block: &[f32]) -> bool {
        block.iter().all(|sample| *sample == 0.0)
    }

    /// A snapshot of the music chain with state that remembers: a boosted band, the leveller, and
    /// every effect up — so a chain that stopped while muted would come back different.
    fn busy_output_params(mute: bool) -> DspParams {
        let mut params = DspParams {
            mute,
            effects: [0.6; fxsound_core::Effect::COUNT],
            volume_leveling_db: 2.0,
            ..DspParams::default()
        };
        params.band_boost_db[3] = 6.0;
        params.sanitise();
        params
    }

    #[test]
    fn a_muted_output_lane_hands_the_ring_silence() {
        let mut w = wired();
        let playing = run_block(&mut w.lanes.output, 0);
        assert!(!is_silent(&playing), "the tone gets through unmuted");

        w.params.write(busy_output_params(true));
        for index in 1..5 {
            assert!(
                is_silent(&run_block(&mut w.lanes.output, index)),
                "block {index}"
            );
        }

        w.params.write(busy_output_params(false));
        assert!(
            !is_silent(&run_block(&mut w.lanes.output, 5)),
            "and comes back on the first block after the unmute"
        );
    }

    #[test]
    fn a_muted_input_lane_hands_the_recorder_silence() {
        let mut w = wired();
        assert!(!is_silent(&run_block(&mut w.lanes.input, 0)));

        w.input_params.write(InputDspParams {
            mute: true,
            ..InputDspParams::default()
        });
        for index in 1..5 {
            assert!(
                is_silent(&run_block(&mut w.lanes.input, index)),
                "block {index}"
            );
        }

        w.input_params.write(InputDspParams::default());
        assert!(!is_silent(&run_block(&mut w.lanes.input, 5)));
    }

    #[test]
    fn the_mute_silences_a_bypassed_lane_too() {
        // Power off still passes audio — the master gain survives a bypass — so a mute that lived
        // inside the chain's power branch would leak exactly the blocks a sleeping system sends.
        let mut w = wired();
        w.params.write(DspParams {
            power: false,
            mute: true,
            ..DspParams::default()
        });
        assert!(is_silent(&run_block(&mut w.lanes.output, 0)));
        w.input_params.write(InputDspParams {
            power: false,
            mute: true,
            ..InputDspParams::default()
        });
        assert!(is_silent(&run_block(&mut w.lanes.input, 0)));
    }

    #[test]
    fn the_output_chain_keeps_running_under_the_mute_so_the_unmute_joins_it_in_step() {
        // Two lanes fed the same programme, one muted for a while. If the mute had stopped the
        // chain, the filters, the leveller and the effects would resume from the state they held
        // when it began; running under it, they are exactly where the never-muted lane's are, and
        // the first block after the unmute is bit for bit the same.
        let mut muted = wired();
        let mut reference = wired();
        muted.params.write(busy_output_params(false));
        reference.params.write(busy_output_params(false));
        for index in 0..3 {
            run_block(&mut muted.lanes.output, index);
            run_block(&mut reference.lanes.output, index);
        }

        muted.params.write(busy_output_params(true));
        for index in 3..40 {
            assert!(is_silent(&run_block(&mut muted.lanes.output, index)));
            run_block(&mut reference.lanes.output, index);
        }

        muted.params.write(busy_output_params(false));
        for index in 40..43 {
            assert_eq!(
                run_block(&mut muted.lanes.output, index),
                run_block(&mut reference.lanes.output, index),
                "block {index}"
            );
        }
        // And the meters followed the chain, not the silence: the processed-time counter is the
        // same on both.
        assert_eq!(
            muted.lanes.output.meters().processed_samples,
            reference.lanes.output.meters().processed_samples
        );
    }

    #[test]
    fn the_voice_chain_and_the_calibration_counters_keep_running_under_the_mute() {
        let mut muted = wired();
        let mut reference = wired();
        let mute = InputDspParams {
            mute: true,
            ..InputDspParams::default()
        };
        for index in 0..3 {
            run_block(&mut muted.lanes.input, index);
            run_block(&mut reference.lanes.input, index);
        }
        muted.input_params.write(mute);
        for index in 3..40 {
            assert!(is_silent(&run_block(&mut muted.lanes.input, index)));
            run_block(&mut reference.lanes.input, index);
        }
        let (during, expected) = (muted.lanes.input.meters(), reference.lanes.input.meters());
        assert_eq!(
            during.capture_frames, expected.capture_frames,
            "the wizard's counters follow the microphone, not the mute"
        );
        assert!(during.input_peak > 0.4, "{}", during.input_peak);

        muted.input_params.write(InputDspParams::default());
        for index in 40..43 {
            assert_eq!(
                run_block(&mut muted.lanes.input, index),
                run_block(&mut reference.lanes.input, index),
                "block {index}"
            );
        }
    }

    #[test]
    fn one_lanes_mute_leaves_the_other_lane_playing() {
        // The two snapshots are separate paths; sleeping is both lanes muting, each through its
        // own, and a mute of one is never the other's.
        let mut w = wired();
        w.params.write(DspParams {
            mute: true,
            ..DspParams::default()
        });
        assert!(is_silent(&run_block(&mut w.lanes.output, 0)));
        assert!(!is_silent(&run_block(&mut w.lanes.input, 0)));

        let mut w = wired();
        w.input_params.write(InputDspParams {
            mute: true,
            ..InputDspParams::default()
        });
        assert!(is_silent(&run_block(&mut w.lanes.input, 0)));
        assert!(!is_silent(&run_block(&mut w.lanes.output, 0)));
    }

    #[test]
    fn a_lane_starts_unmuted_before_its_first_snapshot() {
        // The holder is built before any snapshot is read; its first block must not be silence
        // the user never asked for.
        let (lanes, _handover) = lanes_for_tests();
        let LaneDsp::Output(output) = &lanes.output else {
            unreachable!("the output slot holds the output lane's DSP");
        };
        assert!(!output.muted);
        let LaneDsp::Input(input) = &lanes.input else {
            unreachable!("the input slot holds the input lane's DSP");
        };
        assert!(!input.muted);
    }

    /// One block through a lane as NODE 1 runs it while the engine holds the lane `silent` or not:
    /// refresh, the engine's mute, then process.
    fn run_block_held(dsp: &mut LaneDsp, block_index: usize, silent: bool) -> Vec<f32> {
        dsp.refresh();
        dsp.set_system_mute(silent);
        dsp.process_bytes(&tone_block(block_index), 2)
            .expect("a block that fits")
            .to_vec()
    }

    #[test]
    fn the_engines_own_mute_silences_both_lanes_whatever_their_snapshots_say() {
        // The system is going to sleep: the engine silences both lanes itself, and the GUI's
        // snapshots — unmuted, and never written by the engine — have no say in it.
        let mut w = wired();
        for (direction, dsp) in w.lanes.iter_mut() {
            assert!(
                !is_silent(&run_block_held(dsp, 0, false)),
                "the {} lane plays before the sleep",
                direction.key()
            );
            for index in 1..5 {
                assert!(
                    is_silent(&run_block_held(dsp, index, true)),
                    "the {} lane, block {index}",
                    direction.key()
                );
            }
            assert!(
                !is_silent(&run_block_held(dsp, 5, false)),
                "the {} lane plays on the first block after the engine lets it",
                direction.key()
            );
        }
    }

    #[test]
    fn either_mute_is_enough_and_neither_lifts_the_other() {
        let mut w = wired();
        // The engine holds the lane silent; the app's own mute coming and going changes nothing.
        w.params.write(busy_output_params(true));
        assert!(is_silent(&run_block_held(&mut w.lanes.output, 0, true)));
        w.params.write(busy_output_params(false));
        assert!(
            is_silent(&run_block_held(&mut w.lanes.output, 1, true)),
            "the app unmuting does not wake a sleeping system's lane"
        );
        // And the other way: the engine letting go leaves a lane the app muted muted.
        w.input_params.write(InputDspParams {
            mute: true,
            ..InputDspParams::default()
        });
        assert!(is_silent(&run_block_held(&mut w.lanes.input, 0, true)));
        assert!(
            is_silent(&run_block_held(&mut w.lanes.input, 1, false)),
            "the wake does not unmute what the app muted"
        );
        w.input_params.write(InputDspParams::default());
        assert!(!is_silent(&run_block_held(&mut w.lanes.input, 2, false)));
    }

    #[test]
    fn the_chain_keeps_running_under_the_engines_mute_so_the_wake_joins_it_in_step() {
        let mut held = wired();
        let mut reference = wired();
        held.params.write(busy_output_params(false));
        reference.params.write(busy_output_params(false));
        for index in 0..3 {
            run_block_held(&mut held.lanes.output, index, false);
            run_block(&mut reference.lanes.output, index);
        }
        for index in 3..40 {
            assert!(is_silent(&run_block_held(
                &mut held.lanes.output,
                index,
                true
            )));
            run_block(&mut reference.lanes.output, index);
        }
        for index in 40..43 {
            assert_eq!(
                run_block_held(&mut held.lanes.output, index, false),
                run_block(&mut reference.lanes.output, index),
                "block {index}"
            );
        }
    }

    #[test]
    fn a_lane_starts_without_the_engines_mute() {
        let (lanes, _handover) = lanes_for_tests();
        let LaneDsp::Output(output) = &lanes.output else {
            unreachable!("the output slot holds the output lane's DSP");
        };
        assert!(!output.system_muted);
        let LaneDsp::Input(input) = &lanes.input else {
            unreachable!("the input slot holds the input lane's DSP");
        };
        assert!(!input.system_muted);
    }

    #[test]
    fn silencing_touches_only_the_processed_block() {
        let mut scratch = [0.25_f32; 8];
        let (block, rest) = scratch.split_at_mut(4);
        silence_if(true, block);
        assert_eq!(block, [0.0; 4]);
        assert_eq!(
            rest, [0.25; 4],
            "the scratch past the block is not the ring's"
        );
        silence_if(false, rest);
        assert_eq!(rest, [0.25; 4], "unmuted is a straight wire");
    }

    /// The music lane never takes a voice engine, however many are waiting: the hand-over's
    /// receiver is the voice lane's alone.
    #[test]
    fn a_replacement_voice_engine_is_never_taken_by_the_music_lane() {
        let (mut lanes, handover) = lanes_for_tests();
        handover
            .replacement
            .try_send(voice_engine(ChainSpec::podcast()))
            .expect("empty slot");
        lanes.output.refresh();
        assert!(
            handover.replacement.is_full(),
            "still waiting for the voice lane"
        );
        lanes.input.refresh();
        assert!(handover.replacement.is_empty(), "the voice lane took it");
        assert_eq!(
            lanes.input.as_input_mut().map(|d| d.engine.spec()),
            Some(ChainSpec::podcast())
        );
    }

    // ---- after the chain: the node's volume and a new pair's fade in (U10, U11)

    /// `frames` stereo frames of full-scale DC, so what comes out is the gain itself.
    fn ones(frames: usize) -> Vec<f32> {
        vec![1.0; frames * 2]
    }

    fn tail_at(rate: f32) -> Tail {
        let mut tail = Tail::new();
        tail.set_rate(rate);
        tail
    }

    fn left(block: &[f32]) -> Vec<f32> {
        block.iter().step_by(2).copied().collect()
    }

    #[test]
    fn a_new_pair_rises_linearly_from_silence_to_its_volume_in_thirty_milliseconds() {
        let mut tail = tail_at(48_000.0);
        tail.begin_pair();
        let mut heard = Vec::new();
        for _ in 0..4 {
            let mut block = ones(480);
            tail.apply(&mut block, 2);
            heard.extend(left(&block));
        }
        let len = 1_440;
        assert_eq!(heard[0], 0.0, "from silence");
        for (frame, &gain) in heard.iter().enumerate().take(len) {
            let expected = frame as f32 / len as f32;
            assert!((gain - expected).abs() < 1e-6, "frame {frame}: {gain}");
        }
        assert!(
            heard[len..].iter().all(|&gain| gain == 1.0),
            "and at its volume from 30 ms on, across the block boundaries"
        );
    }

    #[test]
    fn the_fade_in_lasts_thirty_milliseconds_at_the_rate_the_pair_runs() {
        let mut tail = tail_at(44_100.0);
        tail.begin_pair();
        let mut block = ones(2_048);
        tail.apply(&mut block, 2);
        let heard = left(&block);
        assert!(heard[1_322] < 1.0);
        assert_eq!(heard[1_323], 1.0, "1323 frames at 44.1 kHz");
    }

    #[test]
    fn a_volume_change_is_ramped_across_one_block_and_held_from_the_next() {
        let mut tail = tail_at(48_000.0);
        tail.set_targets(&[0.5; CHANNELS]);
        let mut block = ones(4);
        tail.apply(&mut block, 2);
        assert_eq!(left(&block), [0.875, 0.75, 0.625, 0.5]);
        let mut block = ones(4);
        tail.apply(&mut block, 2);
        assert_eq!(left(&block), [0.5; 4]);
    }

    #[test]
    fn each_channel_follows_its_own_volume() {
        let mut tail = tail_at(48_000.0);
        let mut gains = [1.0; CHANNELS];
        gains[1] = 0.25;
        tail.set_targets(&gains);
        tail.begin_pair();
        tail.fade_len = 0;
        let mut block = ones(3);
        tail.apply(&mut block, 2);
        assert_eq!(block, [1.0, 0.25, 1.0, 0.25, 1.0, 0.25]);
    }

    #[test]
    fn a_new_pair_starts_at_its_volume_rather_than_ramping_there_from_the_last_pairs() {
        let mut tail = tail_at(48_000.0);
        let mut block = ones(4);
        tail.apply(&mut block, 2);
        assert_eq!(block, ones(4), "the last pair at unity");

        tail.set_targets(&[0.25; CHANNELS]);
        tail.begin_pair();
        let mut block = ones(2_000);
        tail.apply(&mut block, 2);
        let heard = left(&block);
        assert!(
            (heard[720] - 0.125).abs() < 1e-6,
            "half way up the fade, at 0.25"
        );
        assert_eq!(heard[1_999], 0.25);
    }

    #[test]
    fn a_tail_at_unity_with_its_fade_over_leaves_the_block_bit_for_bit_alone() {
        let mut tail = tail_at(48_000.0);
        let mut block: Vec<f32> = (0..64).map(|i| (i as f32 * 0.37).sin()).collect();
        let before = block.clone();
        tail.apply(&mut block, 2);
        assert_eq!(block, before);
    }

    #[test]
    fn a_gain_that_is_not_a_number_or_below_zero_is_heard_as_silence() {
        let mut tail = tail_at(48_000.0);
        let mut gains = [f32::NAN; CHANNELS];
        gains[1] = -3.0;
        tail.set_targets(&gains);
        tail.begin_pair();
        tail.fade_len = 0;
        let mut block = ones(2);
        tail.apply(&mut block, 2);
        assert_eq!(block, [0.0; 4]);
    }

    #[test]
    fn an_empty_block_or_no_channels_is_left_alone_rather_than_divided_by() {
        let mut tail = tail_at(48_000.0);
        tail.set_targets(&[0.5; CHANNELS]);
        tail.begin_pair();
        let mut empty: [f32; 0] = [];
        tail.apply(&mut empty, 2);
        let mut block = ones(4);
        tail.apply(&mut block, 0);
        assert_eq!(block, ones(4));
        assert_eq!(
            tail.fade_done, 0,
            "nothing was played, so the fade has not begun"
        );
    }

    #[test]
    fn both_lanes_apply_the_node_volume_after_the_chain_and_fade_a_new_pair_in() {
        let mut w = wired();
        w.params.write(DspParams {
            power: false,
            ..DspParams::default()
        });
        w.input_params.write(InputDspParams {
            power: false,
            ..InputDspParams::default()
        });
        for (direction, dsp) in w.lanes.iter_mut() {
            dsp.set_volume(&[0.5; CHANNELS]);
            dsp.begin_pair();
            let first = run_block(dsp, 0);
            assert_eq!(
                first[..2],
                [0.0, 0.0],
                "the {} lane's first frame",
                direction.key()
            );
            // 1440 frames of fade: blocks 0, 1 and 2 of 480.
            for index in 1..3 {
                run_block(dsp, index);
            }
            let settled = run_block(dsp, 3);
            let dry: Vec<f32> = tone_block(3)
                .as_chunks::<4>()
                .0
                .iter()
                .map(|raw| f32::from_le_bytes(*raw))
                .collect();
            for (wet, dry) in settled.iter().zip(&dry) {
                assert!(
                    (wet - 0.5 * dry).abs() < 1e-6,
                    "the {} lane at half volume: {wet} for {dry}",
                    direction.key()
                );
            }
        }
    }
}
