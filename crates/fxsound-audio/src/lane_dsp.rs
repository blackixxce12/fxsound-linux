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
use crate::{DEFAULT_SAMPLE_RATE, MAX_CHANNELS, MAX_QUANTUM_FRAMES, MIN_CHANNELS};

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
        self.engine.apply(self.params.read());
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
        self.engine.apply(self.params.read());
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

/// A de-serialisation buffer for the largest block either chain is ever handed: 2048 frames of
/// eight channels (`docs/spec/12-audio-io.md` §24). Allocated once, on the main loop.
fn worst_case_scratch() -> Vec<f32> {
    vec![0.0; MAX_QUANTUM_FRAMES * MAX_CHANNELS as usize]
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
}
