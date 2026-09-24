//! The whole signal chain in one object, driven from the audio callback.
//!
//! Mirrors the order in `dfxpProcessReal.cpp` and `Play32.c`, which
//! `docs/spec/10-dsp-effects.md` §11 draws in full:
//!
//! ```text
//! in ─► graphic EQ ─► master gain · balance ─► volume levelling ─► effect chain ─► spectrum tap ─► out
//!       └─── GraphicEq block: skipped while the EQ is off ────┘
//!
//! power off:
//! in ─► master gain · balance, while the EQ is on ─► spectrum tap ─► out
//! ```
//!
//! The equalizer's switch is the whole block's switch, not the filters' alone: master gain,
//! balance and the levelling stage all live inside the original's `sosProcessBuffer`, which
//! `dfxpProcessReal.cpp:143-157` does not call while the equalizer is off. That predates upstream
//! aad64c1, whose title only names the leveller: before it, the equalizer-off branch ran the
//! levelling alone and its comment kept "the other SOS gain stages" bypassed as well.
//!
//! Power off keeps the gain stage, as the original does, so that switching FxSound off does not
//! jump the volume — but not the way it does it. The original calls
//! `GraphicEqProcess_MasterGainOnly` (`dfxpProcessReal.cpp:158-169`), which multiplies by the
//! master gain alone (`SosProcess.cpp:500-516`), behind the same equalizer test. So a mix balanced
//! to one side recentred the moment FxSound went off, and on 5.1 or 7.1, where the test read
//! `(i_eq_on) && a || b || c` until aad64c1 bracketed it, the master gain came in with the
//! equalizer off too. Here power off applies the master gain *and* the balance, and exactly when
//! the powered path does: while the equalizer is on (audit report R3). The bypass is the powered
//! gain stage and nothing else, so switching FxSound off never moves the level by more than
//! Dynamic Boost's 0.3 dB ceiling or the balance by anything, with the equalizer on or off.
//!
//! The audit's option (b) read "whatever the equalizer says", and that is the one half of it not
//! taken. With the equalizer off the powered path plays neither the master gain nor the balance
//! (the block above, which the audit keeps), so a bypass that applied them would *bring them in*
//! on switching off: at −6 dB and a balance of +6 dB, −0.3 dB on both sides powered would become
//! −12 dB on the left and −6 dB on the right, the very step (b) was chosen to avoid. Taking that
//! step out the other way, by running the gain stage while powered with the equalizer off, would
//! undo the block the audit keeps (`docs/0.4.0-upstream.md`, U3).
//!
//! The balance works by side, not by index: every left-hand speaker is turned down together and
//! every right-hand one together, the centre and the subwoofer never (audit report #44). The
//! original only has a balance on stereo (`SosProcess.cpp:630-631`; its surround path has none,
//! `:840-908`), and the port's first cut applied it to channels 0 and 1 of any layout, so on 5.1
//! a balance of +10 dB turned down the front-left speaker and left the rear-left one playing.
//! The sides come from [`Engine::set_channel_sides`] when the layout is known and are inferred
//! from the channel count, the front pair and the subwoofer otherwise ([`default_sides`]).
//!
//! A new master gain or balance glides there over [`crate::smooth::GLIDE_SECONDS`] instead of
//! stepping between two samples, as every other gain and filter the user can move does (audit
//! report #11, [`crate::smooth`]): the original writes the gain straight into the float the audio
//! thread multiplies by, so a 2 dB step on the slider was a 2 dB step in the waveform, and a click.
//! The equalizer's switch fades the whole GraphicEq block in or out over the same 20 ms, mixing
//! its output with the audio it was given, for the same reason: with 62.5 Hz at +6 dB under a
//! 50 Hz tone at 0.3, switching the equalizer off moved the waveform by 0.143 between two samples,
//! and now by no more than the tone moves on its own.
//! The power switch is the one control that still acts between two samples: it is the listener's
//! A/B against the unprocessed sound, and a bypass that faded would mix the processed signal, a
//! look-ahead behind, with the dry one for 20 ms.
//!
//! Everything after construction is allocation-free. [`Engine::process`] is the only method the
//! real-time thread calls per buffer; the others are called from the same thread in response to a
//! parameter snapshot, and none of them allocate either.

use crate::biquad::Real;
use crate::effects::{Chain, MAX_BLOCK_FRAMES};
use crate::eq::GraphicEq;
use crate::leveller::VolumeLeveller;
use crate::smooth::{Ramp, glide_frames};
use crate::spectrum::SpectrumAnalyser;
use fxsound_core::messages::{DspEvent, DspParams, Meters};

/// Which side of the listener a speaker stands on, which is what the balance acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelSide {
    /// Front-left, side-left, rear-left and the like: turned down by a balance to the right.
    Left,
    /// Their right-hand mirrors: turned down by a balance to the left.
    Right,
    /// Centre, subwoofer, rear-centre, mono, and any position that is neither: the balance never
    /// touches it. The master gain still does.
    Centre,
}

/// The sides of PipeWire's default positions for a channel count — the layout a device that
/// publishes none gets (`FL,FR`; `FL,FR,LFE`; `FL,FR,RL,RR`; `FL,FR,FC,RL,RR`;
/// `FL,FR,FC,LFE,RL,RR`; `FL,FR,FC,LFE,RC,SL,SR`; `FL,FR,FC,LFE,RL,RR,SL,SR`), with one channel
/// read as mono.
#[must_use]
pub fn standard_sides(channels: usize) -> [ChannelSide; crate::biquad::MAX_CHANNELS] {
    use ChannelSide::{Centre as C, Left as L, Right as R};
    let layout: &[ChannelSide] = match channels {
        0 | 1 => &[C],
        2 => &[L, R],
        3 => &[L, R, C],
        4 => &[L, R, L, R],
        5 => &[L, R, C, L, R],
        6 => &[L, R, C, C, L, R],
        7 => &[L, R, C, C, C, L, R],
        _ => &[L, R, C, C, L, R, L, R],
    };
    let mut sides = [C; crate::biquad::MAX_CHANNELS];
    sides[..layout.len()].copy_from_slice(layout);
    sides
}

/// The sides the engine assumes when nobody has named them: [`standard_sides`] for the count,
/// as long as what the engine does know — the front pair and the subwoofer — sits where that
/// layout puts them; otherwise the device orders its channels some other way, and only the front
/// pair is balanced, rather than a guess turning down a speaker on the wrong side. Either way the
/// front pair is left and right and the subwoofer is centre.
#[must_use]
pub fn default_sides(
    channels: usize,
    lfe: Option<usize>,
    front_pair: Option<(usize, usize)>,
) -> [ChannelSide; crate::biquad::MAX_CHANNELS] {
    let channels = channels.clamp(1, crate::biquad::MAX_CHANNELS);
    let standard_lfe = match channels {
        3 => Some(2),
        6..=8 => Some(3),
        _ => None,
    };
    let standard_front = (channels >= 2).then_some((0, 1));
    let agrees = front_pair.is_none_or(|pair| Some(pair) == standard_front)
        && lfe.is_none_or(|index| Some(index) == standard_lfe);

    let mut sides = if agrees {
        standard_sides(channels)
    } else {
        [ChannelSide::Centre; crate::biquad::MAX_CHANNELS]
    };
    if let Some((left, right)) = front_pair.or(standard_front)
        && left < channels
        && right < channels
        && left != right
    {
        sides[left] = ChannelSide::Left;
        sides[right] = ChannelSide::Right;
    }
    if let Some(index) = lfe.filter(|index| *index < channels) {
        sides[index] = ChannelSide::Centre;
    }
    sides
}

/// The complete FxSound processing chain.
#[derive(Debug)]
pub struct Engine {
    eq: GraphicEq,
    leveller: VolumeLeveller,
    chain: Chain,
    spectrum: SpectrumAnalyser,

    sample_rate: Real,
    channels: usize,

    /// The gain stage: the master gain on every channel, with the balance's attenuation folded in
    /// on each side, and the glide to a new setting.
    gains: SideGains,
    /// Audio has gone through since the engine was built or last cleared. Until it has, a new
    /// master gain or balance lands at once: there is nothing to glide from.
    heard: bool,
    /// The GraphicEq block's share of the output: 1 while the equalizer is on, 0 while it is off,
    /// and a glide between the two when the switch moves.
    eq_block: Ramp,
    /// The audio a block the equalizer's switch is fading came in as, to mix its output with.
    dry: DryCopy,

    /// Cached so a snapshot that did not change a value does not force a redesign.
    applied: DspParams,
    /// Samples processed per channel since the last reset, for the "audio processed" counter.
    processed_samples: u64,
    peak_left: Real,
    peak_right: Real,
    active: bool,
    /// Index of the subwoofer channel in the current layout, when there is one.
    lfe_channel: Option<usize>,
    /// The front pair, when the layout names one.
    front_pair: Option<(usize, usize)>,
    /// The sides the layout's owner named, and for how many channels.
    named_sides: Option<([ChannelSide; crate::biquad::MAX_CHANNELS], usize)>,
    /// The sides the balance uses for `channels`: the named ones when they fit, else the default.
    sides: [ChannelSide; crate::biquad::MAX_CHANNELS],
}

impl Engine {
    /// Build an engine sized for the worst case it will be asked to handle.
    ///
    /// `max_block_frames` is capped at [`MAX_BLOCK_FRAMES`]; a larger block is processed correctly
    /// but the spectrum analyser only looks at the first `max_block_frames` of it.
    #[must_use]
    pub fn new(sample_rate: f32, max_block_frames: usize, channels: usize) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let max_block_frames = max_block_frames.clamp(1, MAX_BLOCK_FRAMES);
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(sample_rate);

        let mut engine = Self {
            eq,
            leveller: VolumeLeveller::new(sample_rate),
            chain: Chain::new(sample_rate),
            spectrum: SpectrumAnalyser::new(sample_rate, max_block_frames),
            sample_rate,
            channels: channels.clamp(1, crate::biquad::MAX_CHANNELS),
            gains: SideGains::unity(),
            heard: false,
            eq_block: Ramp::new(1.0),
            dry: DryCopy::new(),
            applied: DspParams::default(),
            processed_samples: 0,
            peak_left: 0.0,
            peak_right: 0.0,
            active: false,
            lfe_channel: None,
            front_pair: None,
            named_sides: None,
            sides: [ChannelSide::Centre; crate::biquad::MAX_CHANNELS],
        };
        engine.refresh_sides();
        let params = DspParams::default();
        engine.apply_unconditionally(&params);
        engine
    }

    #[must_use]
    pub const fn sample_rate(&self) -> Real {
        self.sample_rate
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }

    /// The stream format changed. Redesigns every filter and clears all history.
    pub fn set_format(&mut self, sample_rate: f32, channels: usize) {
        let sample_rate = sample_rate.max(1.0);
        let channels = channels.clamp(1, crate::biquad::MAX_CHANNELS);
        if sample_rate == self.sample_rate && channels == self.channels {
            return;
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.eq.set_sample_rate(sample_rate);
        self.leveller.set_sample_rate(sample_rate);
        self.chain.set_sample_rate(sample_rate);
        self.spectrum.set_sample_rate(sample_rate);
        self.refresh_sides();
        self.reset();
    }

    /// Name the subwoofer channel, so the stages that must not touch it can skip it.
    ///
    /// Passed as an index rather than derived from the channel count, because a device is free to
    /// order its channels however it likes: the subwoofer sits at 3 in a standard 5.1 or 7.1
    /// layout, and elsewhere in several real ones.
    pub fn set_lfe_channel(&mut self, channel: Option<usize>) {
        self.lfe_channel = channel;
        self.chain.set_lfe_channel(channel);
        self.refresh_sides();
    }

    /// Name the front pair, so the two stereo-by-nature stages run over the right channels.
    ///
    /// `None` keeps the historical behaviour of using the first two, which is correct for every
    /// layout that starts `FL, FR` — that is, all the standard ones.
    pub fn set_front_pair(&mut self, pair: Option<(usize, usize)>) {
        self.front_pair = pair;
        self.chain.set_front_pair(pair);
        self.refresh_sides();
    }

    /// Name the side of every channel, from the device's own channel positions, so the balance
    /// turns down the speakers on one side of the room and nothing else.
    ///
    /// One entry per channel, in the device's order. `None`, or a list that does not have one
    /// entry per channel of the current format, leaves the engine to infer the sides
    /// ([`default_sides`]), which is right for every layout PipeWire makes up for a device that
    /// publishes none, but can only balance the front pair of one ordered some other way.
    pub fn set_channel_sides(&mut self, sides: Option<&[ChannelSide]>) {
        self.named_sides = sides.map(|sides| {
            let mut named = [ChannelSide::Centre; crate::biquad::MAX_CHANNELS];
            let len = sides.len().min(crate::biquad::MAX_CHANNELS);
            named[..len].copy_from_slice(&sides[..len]);
            (named, sides.len())
        });
        self.refresh_sides();
    }

    /// The sides the balance acts on, for the current format.
    #[must_use]
    pub fn channel_sides(&self) -> &[ChannelSide] {
        &self.sides[..self.channels]
    }

    fn refresh_sides(&mut self) {
        self.sides = match self.named_sides {
            Some((named, len)) if len == self.channels => named,
            _ => default_sides(self.channels, self.lfe_channel, self.front_pair),
        };
    }

    /// Adopt a parameter snapshot, skipping anything that has not changed.
    pub fn apply(&mut self, params: &DspParams) {
        if *params == self.applied {
            return;
        }
        self.apply_unconditionally(params);
    }

    fn apply_unconditionally(&mut self, params: &DspParams) {
        self.chain.apply(params);

        // The equalizer's switch fades the GraphicEq block in or out (see the module
        // documentation); the filters stay on until it has faded out, and come back on from rest
        // (`GraphicEq::set_enabled`) as it starts to fade in.
        self.eq_block.glide_to(
            if params.eq_on { 1.0 } else { 0.0 },
            glide_frames(self.sample_rate),
        );
        if !self.heard {
            self.eq_block.settle();
        }
        self.eq.set_enabled(self.eq_block_runs());
        self.eq.set_q_multiplier(params.filter_q);
        let (centers, boosts) = params.bands();
        if centers != self.eq.center_frequencies() || boosts != self.eq.boosts_db() {
            self.eq.set_bands(centers, boosts);
        }

        // Despite its name in the original API, this parameter is an abstract 0..=4 amount, not
        // decibels (`docs/spec/08-dsp-api.md` §8.4).
        self.leveller.set_amount(params.volume_leveling_db);

        // `10^(dB/20)`, applied per sample (`GraphicEqSet.cpp:101`); the balance attenuates one
        // side and never boosts the other (`GraphicEqSet.cpp:38-59`).
        let master = db_to_linear(params.master_gain_db);
        let (left, right) = balance_gains(params.balance);
        self.gains.glide_to(
            master * left,
            master * right,
            master,
            glide_frames(self.sample_rate),
        );
        if !self.heard {
            self.gains.settle();
        }

        self.applied = *params;
    }

    /// Act on a one-shot event.
    pub fn handle_event(&mut self, event: DspEvent) {
        match event {
            DspEvent::ResetFilterState => {
                self.eq.reset();
                self.leveller.reset();
                self.chain.reset();
            }
            DspEvent::ResetSpectrum => self.spectrum.reset(),
            DspEvent::ResetProcessedTime => self.processed_samples = 0,
            // The music chain keeps no capture statistics; the event is the microphone's.
            DspEvent::ResetCaptureStats => {}
        }
    }

    /// Clear every filter's history. A glide under way lands, and until audio goes through again
    /// a new value lands at once.
    pub fn reset(&mut self) {
        self.eq.reset();
        self.leveller.reset();
        self.chain.reset();
        self.spectrum.reset();
        self.peak_left = 0.0;
        self.peak_right = 0.0;
        self.gains.settle();
        self.eq_block.settle();
        self.eq.set_enabled(self.eq_block_runs());
        self.heard = false;
    }

    /// Latency the chain adds, in frames. Only the limiter's look-ahead contributes.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        self.chain.latency_frames()
    }

    /// Process one interleaved block in place.
    ///
    /// Real-time safe: no allocation, no locks, no IO, no panicking paths.
    pub fn process(&mut self, buffer: &mut [f32], channels: usize) {
        let channels = channels.clamp(1, crate::biquad::MAX_CHANNELS);
        if buffer.is_empty() {
            return;
        }

        // Nothing between the client's bytes and the filters validates a sample, and every stage
        // here has infinite memory: a NaN in a biquad's state reproduces itself forever, the
        // reverb tank latches one into its delay arena, one `+inf` pins the leveller's peak so
        // both ends of its gain ramp become exactly zero, and the same value freezes Dynamic
        // Boost's level estimator — a stage that is never bypassed. None of it recovers on its
        // own; the only escape is a preset change, and the meters hide it because `f32::min`
        // discards NaN. One branch per sample is a rounding error against the block budget.
        for sample in buffer.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }
        self.heard = true;

        let block_runs = self.eq_block_runs();
        if self.applied.power {
            // The GraphicEq block, whole or not at all (`dfxpProcessReal.cpp:143-157`), faded in
            // or out when the equalizer's switch moves. While it is skipped the equalizer's and
            // the leveller's state stand still, as the original's do, rather than being reset.
            if block_runs {
                self.run_eq_block(buffer, channels, true);
            } else {
                self.sit_out_eq_block();
            }
            self.chain.process(buffer, channels);
        } else {
            // Bypassed, the gain stage is the one that survives, and all of it: the master gain
            // and the balance (audit report R3). It survives exactly where the powered branch
            // above plays it, behind the equalizer's switch, so the power switch never moves the
            // level; the module documentation has the original's version and why it is not this.
            // The filters and the leveller stand still.
            self.eq.sit_out();
            self.leveller.sit_out();
            if block_runs {
                self.run_eq_block(buffer, channels, false);
            } else {
                self.gains.settle();
            }
        }
        if !self.eq_block_runs() {
            self.eq.set_enabled(false);
        }

        // A block that went in finite can still come out non-finite if a stage's own state has
        // blown up — a coefficient designed from a parameter that reached the engine before it
        // was sanitised, or a divergent filter at an extreme rate. Hand the device silence rather
        // than a NaN and clear the history, so the next block starts clean instead of inheriting
        // the failure for the rest of the session. This runs before the spectrum tap and the
        // meters, so neither the visualizer nor the GUI ever sees the bad block.
        if buffer.iter().any(|sample| !sample.is_finite()) {
            buffer.fill(0.0);
            self.reset();
        }

        self.spectrum.push(buffer, channels);
        self.measure(buffer, channels);
    }

    /// Whether the GraphicEq block plays: the equalizer is on, or its switch is still fading the
    /// block out.
    fn eq_block_runs(&self) -> bool {
        self.eq_block.target() > 0.0 || self.eq_block.value() > 0.0
    }

    /// A block the GraphicEq block does not play in: nothing hears its stages move.
    ///
    /// Each stage settles what it was gliding towards, so that when the block comes back it
    /// starts where it was set rather than playing out a glide the listener did not hear begin:
    /// the gain stage its gains, the equalizer its crossfades, and the leveller the let-down it
    /// starts when it is switched off (`GraphicEq::sit_out`, `VolumeLeveller::sit_out`).
    fn sit_out_eq_block(&mut self) {
        self.eq.sit_out();
        self.leveller.sit_out();
        self.gains.settle();
    }

    /// The GraphicEq block over one buffer — the equalizer, the gain stage and the leveller, or
    /// with FxSound off the gain stage alone — and while the equalizer's switch is fading, its
    /// output mixed with the audio it was given.
    ///
    /// The mix is linear, as every crossfade here is ([`crate::smooth::FadingSection`]). The dry
    /// copy holds a bounded stretch, so a buffer larger than that is taken a stretch at a time
    /// while the fade runs: 2 048 frames of stereo, 512 of 7.1. The leveller then sees those
    /// stretches as buffers, which only matters to how far ahead of a hit it can dip (its
    /// module documentation, audit #4), and only for the 20 ms of the fade.
    fn run_eq_block(&mut self, buffer: &mut [f32], channels: usize, filters: bool) {
        if !self.eq_block.is_gliding() {
            self.eq_block_pass(buffer, channels, filters);
            return;
        }
        let stretch = (self.dry.0.len() / channels).max(1) * channels;
        for part in buffer.chunks_mut(stretch) {
            self.dry.0[..part.len()].copy_from_slice(part);
            self.eq_block_pass(part, channels, filters);
            for (frame, dry) in part
                .chunks_exact_mut(channels)
                .zip(self.dry.0.chunks_exact(channels))
            {
                let share = self.eq_block.advance();
                // At 1 the frame is the block's own output, untouched; at 0 it is the input.
                if share < 1.0 {
                    for (sample, dry) in frame.iter_mut().zip(dry) {
                        *sample = dry + share * (*sample - dry);
                    }
                }
            }
        }
    }

    /// The GraphicEq block's stages, in order, at full strength.
    fn eq_block_pass(&mut self, buffer: &mut [f32], channels: usize, filters: bool) {
        if filters {
            self.eq.process(buffer, channels);
        }
        self.apply_gain_stage(buffer, channels);
        if filters {
            // The subwoofer is levelled with every other channel but kept out of the level
            // analysis: it carries a deliberately enormous share of the programme's energy, so
            // letting it into the statistics would pull the gain down on bass-heavy material for
            // reasons that have nothing to do with how loud the programme actually is. The stage
            // keeps its own 10 ms clock, whatever the quantum.
            self.leveller
                .process_with_lfe(buffer, channels, self.lfe_channel);
        }
    }

    /// The master gain and the balance attenuation, folded into one pass.
    ///
    /// Every channel takes the master gain; a left-hand one takes the left attenuation with it and
    /// a right-hand one the right (see the module documentation). Mono has no sides and takes the
    /// master gain alone. On stereo this is the gain stage the original folds into
    /// `sosProcessBuffer` (`SosProcess.cpp:583`, `:630-631`), and it is what this did before.
    ///
    /// While a glide runs each frame takes its own step of it; otherwise the factors are the
    /// constants they always were.
    fn apply_gain_stage(&mut self, buffer: &mut [f32], channels: usize) {
        if !self.gains.is_gliding() {
            self.apply_steady_gain_stage(buffer, channels);
            return;
        }
        let sides = if channels == self.channels {
            self.sides
        } else {
            standard_sides(channels)
        };
        for frame in buffer.chunks_exact_mut(channels) {
            let [left, right, centre] = self.gains.advance();
            if channels < 2 {
                for sample in frame.iter_mut() {
                    *sample *= centre;
                }
                continue;
            }
            for (sample, side) in frame.iter_mut().zip(sides) {
                *sample *= match side {
                    ChannelSide::Left => left,
                    ChannelSide::Right => right,
                    ChannelSide::Centre => centre,
                };
            }
        }
    }

    /// [`Engine::apply_gain_stage`] with nothing moving.
    fn apply_steady_gain_stage(&self, buffer: &mut [f32], channels: usize) {
        let [left, right, master] = self.gains.values();
        if master == 1.0 && left == 1.0 && right == 1.0 {
            return;
        }
        if channels < 2 {
            for sample in buffer.iter_mut() {
                *sample *= master;
            }
            return;
        }

        // A block in a format the engine was not told about (`process` clamps rather than
        // refuses) is balanced by the count's default layout.
        let sides = if channels == self.channels {
            self.sides
        } else {
            standard_sides(channels)
        };
        let mut gains = [master; crate::biquad::MAX_CHANNELS];
        for (gain, side) in gains.iter_mut().zip(sides).take(channels) {
            *gain = match side {
                ChannelSide::Left => left,
                ChannelSide::Right => right,
                ChannelSide::Centre => master,
            };
        }

        if channels == 2 {
            let (first, second) = (gains[0], gains[1]);
            for frame in buffer.as_chunks_mut::<2>().0 {
                frame[0] *= first;
                frame[1] *= second;
            }
        } else {
            for frame in buffer.chunks_exact_mut(channels) {
                for (sample, gain) in frame.iter_mut().zip(&gains) {
                    *sample *= gain;
                }
            }
        }
    }

    fn measure(&mut self, buffer: &[f32], channels: usize) {
        let frames = buffer.len() / channels;
        self.processed_samples = self.processed_samples.saturating_add(frames as u64);

        let mut peak_left = 0.0_f32;
        let mut peak_right = 0.0_f32;
        for frame in buffer.chunks_exact(channels) {
            peak_left = peak_left.max(frame[0].abs());
            if channels >= 2 {
                peak_right = peak_right.max(frame[1].abs());
            }
        }
        if channels < 2 {
            peak_right = peak_left;
        }

        // Decay the held peaks so the meters fall rather than latching.
        const DECAY: f32 = 0.85;
        self.peak_left = peak_left.max(self.peak_left * DECAY);
        self.peak_right = peak_right.max(self.peak_right * DECAY);
        self.active = peak_left > 1e-6 || peak_right > 1e-6;
    }

    /// What the GUI should draw. Cheap enough to call every buffer.
    #[must_use]
    pub fn meters(&self) -> Meters {
        Meters {
            spectrum: self.spectrum.bands(),
            peak_left: self.peak_left.min(1.0),
            peak_right: self.peak_right.min(1.0),
            processed_samples: self.processed_samples,
            sample_rate: self.sample_rate as u32,
            active: self.active,
            // The output chain has no gate, compressor or de-esser; the fields belong to the
            // microphone chain and stay at zero here rather than being left undefined.
            gate_reduction_db: 0.0,
            compressor_reduction_db: 0.0,
            deesser_reduction_db: 0.0,
            deesser_running: false,
            denoiser_running: false,
            voice_probability: 0.0,
            // Likewise the microphone's telemetry and the calibration accumulators.
            input_peak: 0.0,
            input_rms_db: 0.0,
            noise_floor_db: 0.0,
            denoise_reduction_db: 0.0,
            deesser_hz: 0.0,
            dereverb_reduction_db: 0.0,
            latency_frames: 0,
            capture_frames: 0,
            capture_sum_squares: 0.0,
            capture_peak: 0.0,
            capture_clipped: 0,
        }
    }

    /// The equalizer, for drawing its response curve.
    #[must_use]
    pub const fn equalizer(&self) -> &GraphicEq {
        &self.eq
    }
}

/// How many samples of a block the equalizer's switch is fading are kept dry at a time.
const DRY_SAMPLES: usize = 4_096;

/// The dry copy [`Engine::run_eq_block`] mixes with, allocated with the engine. Its own type so
/// that the engine's `Debug` does not print four thousand zeros.
struct DryCopy(Box<[f32]>);

impl DryCopy {
    fn new() -> Self {
        Self(vec![0.0; DRY_SAMPLES].into_boxed_slice())
    }
}

impl std::fmt::Debug for DryCopy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "DryCopy({} samples)", self.0.len())
    }
}

/// The gain stage's factor for each side of the room, gliding together: the master gain times the
/// balance's attenuation for that side, folded once when a snapshot arrives, so a glide lands on
/// exactly the factors a stage that never glided multiplies by. The centre's is the master gain.
#[derive(Clone, Copy, Debug)]
struct SideGains {
    left: Ramp,
    right: Ramp,
    centre: Ramp,
}

impl SideGains {
    const fn unity() -> Self {
        Self {
            left: Ramp::new(1.0),
            right: Ramp::new(1.0),
            centre: Ramp::new(1.0),
        }
    }

    fn glide_to(&mut self, left: Real, right: Real, centre: Real, frames: u32) {
        self.left.glide_to(left, frames);
        self.right.glide_to(right, frames);
        self.centre.glide_to(centre, frames);
    }

    const fn is_gliding(&self) -> bool {
        self.left.is_gliding() || self.right.is_gliding() || self.centre.is_gliding()
    }

    const fn settle(&mut self) {
        self.left.settle();
        self.right.settle();
        self.centre.settle();
    }

    #[inline(always)]
    fn advance(&mut self) -> [Real; 3] {
        [
            self.left.advance(),
            self.right.advance(),
            self.centre.advance(),
        ]
    }

    const fn values(&self) -> [Real; 3] {
        [self.left.value(), self.right.value(), self.centre.value()]
    }
}

/// dB to a linear amplitude factor.
#[inline]
#[must_use]
pub fn db_to_linear(db: Real) -> Real {
    if db == 0.0 {
        1.0
    } else {
        10_f32.powf(db / 20.0)
    }
}

/// Balance in dB to a pair of per-channel attenuations.
///
/// Positive pans right by attenuating the left channel and vice versa; neither side is ever
/// boosted (`GraphicEqSet.cpp:38-59`).
#[inline]
#[must_use]
pub fn balance_gains(balance_db: Real) -> (Real, Real) {
    if balance_db > 0.0 {
        (db_to_linear(-balance_db), 1.0)
    } else if balance_db < 0.0 {
        (1.0, db_to_linear(balance_db))
    } else {
        (1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leveller::CEILING;
    use fxsound_core::Effect as EffectId;

    fn tone(frames: usize, channels: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let s = (n as f32 * 0.05).sin() * amplitude;
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    #[test]
    fn a_default_engine_is_close_to_transparent_once_its_latency_is_accounted_for() {
        let mut engine = Engine::new(48_000.0, 4096, 2);
        let latency = engine.latency_frames();
        // Dynamic Boost always runs and it has a look-ahead delay, so the output is shifted by
        // that much and scaled by its permanent -0.3 dBFS ceiling. Nothing else should move.
        assert!(latency > 0, "the limiter should report its look-ahead");

        let input = tone(4096, 2, 0.25);
        let mut buffer = input.clone();
        engine.process(&mut buffer, 2);

        let ceiling = 0.966_051_f32;
        for frame in latency..(4096 - latency) {
            for channel in 0..2 {
                let got = buffer[frame * 2 + channel];
                let want = input[(frame - latency) * 2 + channel] * ceiling;
                assert!(
                    (got - want).abs() < 0.01,
                    "frame {frame} channel {channel}: got {got}, expected about {want}"
                );
            }
        }
    }

    #[test]
    fn master_gain_is_applied_in_decibels() {
        let mut engine = Engine::new(48_000.0, 256, 2);
        // Power off isolates the gain stage from the effects.
        let params = DspParams {
            power: false,
            master_gain_db: -6.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = vec![1.0_f32; 8];
        engine.process(&mut buffer, 2);
        let expected = 10_f32.powf(-6.0 / 20.0);
        assert!((buffer[0] - expected).abs() < 1e-6, "got {}", buffer[0]);
    }

    #[test]
    fn master_gain_survives_a_bypass() {
        // The original keeps the master gain live when the engine is bypassed, so that switching
        // FxSound off does not jump the volume (`dfxpProcessReal.cpp:158-169`). It now carries the
        // balance as well (audit report R3), which has tests of its own below; this one pins the
        // half that has not changed.
        let mut engine = Engine::new(48_000.0, 256, 2);
        let params = DspParams {
            power: false,
            eq_on: true,
            master_gain_db: 6.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = vec![0.1_f32; 8];
        engine.process(&mut buffer, 2);
        assert!(
            buffer[0] > 0.15,
            "the bypass swallowed the master gain: {}",
            buffer[0]
        );
    }

    #[test]
    fn balance_attenuates_one_side_and_never_boosts_the_other() {
        let (left, right) = balance_gains(6.0);
        assert!(left < 1.0 && right == 1.0, "positive balance pans right");
        let (left, right) = balance_gains(-6.0);
        assert!(left == 1.0 && right < 1.0, "negative balance pans left");
        assert_eq!(balance_gains(0.0), (1.0, 1.0));
    }

    #[test]
    fn balance_reaches_the_audio_path() {
        // Powered on: the whole chain, where Dynamic Boost's look-ahead delays the output and its
        // ceiling scales both sides alike; the ratio between the sides is what the balance set.
        let mut engine = Engine::new(48_000.0, 4096, 2);
        let params = DspParams {
            balance: 20.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = tone(4096, 2, 0.5);
        engine.process(&mut buffer, 2);
        let left = channel_peak(&buffer, 2, 0);
        let right = channel_peak(&buffer, 2, 1);
        assert!(
            left < right,
            "left should be attenuated: {left} against {right}"
        );
        assert!(
            (left / right - 0.1).abs() < 0.005,
            "20 dB of balance should leave the left at a tenth of the right: {left} against {right}"
        );
        assert!(
            (right - 0.5 * 0.966_051).abs() < 0.01,
            "the right should only see Dynamic Boost's ceiling: {right}"
        );
    }

    #[test]
    fn an_unchanged_snapshot_is_not_reapplied() {
        let mut engine = Engine::new(48_000.0, 256, 2);
        let params = DspParams::default();
        engine.apply(&params);
        let before = engine.applied;
        engine.apply(&params);
        assert_eq!(before, engine.applied);
    }

    #[test]
    fn the_equalizer_curve_follows_the_snapshot() {
        let mut engine = Engine::new(48_000.0, 256, 2);
        let mut params = DspParams::default();
        params.band_boost_db[2] = 9.0;
        engine.apply(&params);
        let center = engine.equalizer().center_frequencies()[2];
        assert!(engine.equalizer().response_db(center) > 5.0);
    }

    #[test]
    fn the_processed_sample_counter_tracks_frames_and_resets() {
        let mut engine = Engine::new(48_000.0, 512, 2);
        let mut buffer = tone(480, 2, 0.1);
        engine.process(&mut buffer, 2);
        assert_eq!(engine.meters().processed_samples, 480);
        engine.process(&mut buffer, 2);
        assert_eq!(engine.meters().processed_samples, 960);
        engine.handle_event(DspEvent::ResetProcessedTime);
        assert_eq!(engine.meters().processed_samples, 0);
    }

    #[test]
    fn meters_report_silence_as_inactive() {
        let mut engine = Engine::new(48_000.0, 512, 2);
        let mut buffer = vec![0.0_f32; 960];
        engine.process(&mut buffer, 2);
        assert!(!engine.meters().active);

        let mut buffer = tone(480, 2, 0.5);
        engine.process(&mut buffer, 2);
        assert!(engine.meters().active);
        assert!(engine.meters().peak_left > 0.1);
    }

    #[test]
    fn a_format_change_is_safe_mid_stream() {
        let mut engine = Engine::new(48_000.0, 1024, 2);
        let mut params = DspParams::default();
        for id in EffectId::ALL {
            params.set_effect(id, 0.7);
        }
        engine.apply(&params);

        let mut buffer = tone(512, 2, 0.4);
        engine.process(&mut buffer, 2);
        engine.set_format(96_000.0, 2);
        assert_eq!(engine.sample_rate(), 96_000.0);
        let mut buffer = tone(512, 2, 0.4);
        engine.process(&mut buffer, 2);
        assert!(buffer.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn mono_streams_are_handled() {
        let mut engine = Engine::new(48_000.0, 512, 1);
        let params = DspParams {
            power: false,
            master_gain_db: -6.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = vec![1.0_f32; 64];
        engine.process(&mut buffer, 1);
        let expected = 10_f32.powf(-6.0 / 20.0);
        assert!(buffer.iter().all(|s| (s - expected).abs() < 1e-6));
    }

    /// 10 ms at 48 kHz: one step of the levelling stage's own clock, so every block is exactly
    /// one step and the stage does what the original does with a buffer, ramp and all
    /// (`SosProcess.cpp:375-379`). Other block sizes step on the same 10 ms clock (audit #2); where
    /// a step is split across calls, the ramp to its new gain starts on the first frame of the part
    /// of the call that completes the step — at a 256-frame quantum, frame 256 of the 480, 224
    /// frames before the step completes — rather than on the step's first frame.
    const LEVELLER_BLOCK: usize = 480;

    /// A phase-continuous 300 Hz stereo tone.
    ///
    /// Continuity matters for this stage in a way it does not for the rest of the chain: the
    /// levelling detector is a 120 Hz high-pass (`SosProcess.cpp:174-177`), so a phase jump at
    /// every block boundary would reach it as a transient the programme does not actually contain.
    fn continuous_tone(start_frame: usize, frames: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let n = (start_frame + i) as f64;
                let phase = std::f64::consts::TAU * 300.0 * n / 48_000.0;
                let value = (f64::from(amplitude) * phase.sin()) as f32;
                std::iter::repeat_n(value, 2)
            })
            .collect()
    }

    /// Runs a steady tone through a whole engine at the given levelling amount and returns the
    /// output peak over the last ten blocks — that is, once the levelling gain has settled.
    fn settled_output_peak(amount: f32, amplitude: f32, blocks: usize) -> f32 {
        let params = DspParams {
            volume_leveling_db: amount,
            ..DspParams::default()
        };
        settled_peak(&params, amplitude, blocks)
    }

    /// [`settled_output_peak`] for any snapshot.
    fn settled_peak(params: &DspParams, amplitude: f32, blocks: usize) -> f32 {
        let mut engine = Engine::new(48_000.0, LEVELLER_BLOCK, 2);
        engine.apply(params);

        let mut peak = 0.0_f32;
        for block in 0..blocks {
            let mut buffer = continuous_tone(block * LEVELLER_BLOCK, LEVELLER_BLOCK, amplitude);
            engine.process(&mut buffer, 2);
            if block + 10 >= blocks {
                peak = buffer.iter().fold(peak, |m, s| m.max(s.abs()));
            }
        }
        peak
    }

    #[test]
    fn the_levelling_stage_lifts_a_quiet_passage_and_holds_a_loud_one_down() {
        // The stage sits between the gain stage and the effect chain, so the honest way to see
        // what it did to the finished output is to run the identical chain twice and change
        // nothing but the amount.
        let quiet_off = settled_output_peak(0.0, 0.02, 400);
        let quiet_on = settled_output_peak(4.0, 0.02, 400);
        assert!(
            quiet_on > quiet_off * 2.5,
            "a -34 dBFS passage was not lifted: {quiet_off} -> {quiet_on}"
        );

        // A loud passage is *held*, not turned down towards the RMS target: `desired_gain` is
        // floored at the quiet gain floor, which is itself floored at unity
        // (`SosProcess.cpp:333`, `:458-461`), so the only thing that can pull the levelling gain
        // below 1.0 is the peak clamp at `:358-364`.
        let loud_off = settled_output_peak(0.0, 0.9, 400);
        let loud_on = settled_output_peak(4.0, 0.9, 400);
        assert!(
            loud_on <= loud_off * 1.02,
            "a -1 dBFS passage was boosted rather than held: {loud_off} -> {loud_on}"
        );
        assert!(loud_on <= CEILING, "the output broke full scale: {loud_on}");

        // Which is the point of the stage: the two passages come out far closer together than they
        // went in.
        let spread_off = loud_off / quiet_off;
        let spread_on = loud_on / quiet_on;
        assert!(
            spread_on < spread_off / 2.0,
            "levelling did not close the gap: {spread_off}:1 became {spread_on}:1"
        );
    }

    #[test]
    fn a_zero_amount_leveller_is_absent_from_the_path_bit_for_bit() {
        // Amount 0 is the shipping default (`fxsound/Source/GUI/FxController.h:48`), and
        // `SosProcess.cpp:146-150` returns before touching a sample. `Engine::process` calls the
        // stage whenever the equalizer is on, whatever the amount, so what has to be proved here
        // is that having it in the chain and not having it at all produce the same bits — not
        // merely the same audio. Anything that rode in from the previous buffer (a stale ramp, a
        // retained gain, a clamp at the ceiling) would show up as a mismatch somewhere in the
        // block.
        let mut params = DspParams {
            master_gain_db: -3.0,
            volume_leveling_db: 0.0,
            ..DspParams::default()
        };
        params.band_boost_db[3] = 6.0;

        let mut engine = Engine::new(48_000.0, 2048, 2);
        engine.apply(&params);
        let input = tone(2048, 2, 0.2);
        let mut through_engine = input.clone();
        engine.process(&mut through_engine, 2);

        // The same chain assembled by hand, with no leveller in it at all. Both snapshots are
        // applied in turn because `Engine::new` adopts the default one before `apply` adopts ours.
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        let mut chain = Chain::new(48_000.0);
        for snapshot in [&DspParams::default(), &params] {
            chain.apply(snapshot);
            eq.set_enabled(snapshot.eq_on);
            eq.set_q_multiplier(snapshot.filter_q);
            let (centers, boosts) = snapshot.bands();
            if centers != eq.center_frequencies() || boosts != eq.boosts_db() {
                eq.set_bands(centers, boosts);
            }
        }

        let mut reference = input;
        eq.process(&mut reference, 2);
        let (left, right) = balance_gains(params.balance);
        let left = db_to_linear(params.master_gain_db) * left;
        let right = db_to_linear(params.master_gain_db) * right;
        for frame in reference.as_chunks_mut::<2>().0 {
            frame[0] *= left;
            frame[1] *= right;
        }
        chain.process(&mut reference, 2);

        for (index, (got, want)) in through_engine.iter().zip(reference.iter()).enumerate() {
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "sample {index}: engine gave {got}, a leveller-free chain gave {want}"
            );
        }
    }

    // --- The equalizer's switch is the GraphicEq block's switch (U3) ---------------------------
    //
    // `dfxpProcessReal.cpp:143-157`: powered, the block — filters, master gain, balance, levelling
    // — runs only while the equalizer is on. Bypassed, the original runs the master gain alone
    // behind the same test (`:158-169`, `SosProcess.cpp:500-516`); here the master gain and the
    // balance run behind it (audit report R3), so the power switch never moves the level.

    /// A snapshot that gives every stage of the GraphicEq block something audible to do.
    fn busy_graphic_eq_block(eq_on: bool) -> DspParams {
        let mut params = DspParams {
            eq_on,
            master_gain_db: -9.0,
            balance: 12.0,
            volume_leveling_db: 4.0,
            ..DspParams::default()
        };
        for band in 0..10 {
            params.band_boost_db[band] = if band % 2 == 0 { 9.0 } else { -6.0 };
        }
        params
    }

    /// Everything a fresh engine hands back for `blocks` blocks of a continuous stereo tone.
    fn render_blocks(params: &DspParams, blocks: usize, amplitude: f32) -> Vec<f32> {
        let mut engine = Engine::new(48_000.0, LEVELLER_BLOCK, 2);
        engine.apply(params);
        let mut rendered = Vec::with_capacity(blocks * LEVELLER_BLOCK * 2);
        for block in 0..blocks {
            let mut buffer = continuous_tone(block * LEVELLER_BLOCK, LEVELLER_BLOCK, amplitude);
            engine.process(&mut buffer, 2);
            rendered.extend_from_slice(&buffer);
        }
        rendered
    }

    fn assert_same_bits(got: &[f32], want: &[f32], what: &str) {
        assert_eq!(got.len(), want.len(), "{what}: lengths differ");
        for (index, (got, want)) in got.iter().zip(want).enumerate() {
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "{what}: sample {index} is {got}, expected {want}"
            );
        }
    }

    fn largest_difference(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b)
            .fold(0.0_f32, |acc, (x, y)| acc.max((x - y).abs()))
    }

    #[test]
    fn turning_the_equalizer_off_takes_the_master_gain_the_balance_and_the_leveller_with_it() {
        // Upstream aad64c1. With the equalizer off, an engine whose block is set to do a great
        // deal and one whose block is set to do nothing must hand back the same bits.
        let busy = render_blocks(&busy_graphic_eq_block(false), 60, 0.05);
        let plain = render_blocks(
            &DspParams {
                eq_on: false,
                ..DspParams::default()
            },
            60,
            0.05,
        );
        assert_same_bits(&busy, &plain, "equalizer off");

        // And the fixture really does exercise the block: switched on, the same settings move the
        // output a long way.
        let on = render_blocks(&busy_graphic_eq_block(true), 60, 0.05);
        assert!(
            largest_difference(&on, &busy) > 0.01,
            "the fixture's block settings do nothing even with the equalizer on"
        );
    }

    #[test]
    fn with_the_equalizer_off_only_the_effect_chain_touches_the_signal() {
        // The effects are not part of the block (`dfxpProcessReal.cpp:174` onwards is outside it),
        // so the engine must equal the effect chain alone, bit for bit, block for block.
        let mut params = busy_graphic_eq_block(false);
        params.set_effect(EffectId::Fidelity, 0.4);
        params.set_effect(EffectId::Bass, 0.6);

        let through_engine = render_blocks(&params, 20, 0.2);

        let mut chain = Chain::new(48_000.0);
        chain.apply(&DspParams::default());
        chain.apply(&params);
        let mut reference = Vec::new();
        for block in 0..20 {
            let mut buffer = continuous_tone(block * LEVELLER_BLOCK, LEVELLER_BLOCK, 0.2);
            chain.process(&mut buffer, 2);
            reference.extend_from_slice(&buffer);
        }
        assert_same_bits(&through_engine, &reference, "equalizer off, effects on");

        // The effects themselves were live: without them the output is different.
        let without_effects = render_blocks(&busy_graphic_eq_block(false), 20, 0.2);
        assert!(largest_difference(&through_engine, &without_effects) > 1e-3);
    }

    #[test]
    fn a_quiet_passage_is_not_levelled_while_the_equalizer_is_off() {
        let levelled = |eq_on| {
            settled_peak(
                &DspParams {
                    eq_on,
                    volume_leveling_db: 4.0,
                    ..DspParams::default()
                },
                0.02,
                400,
            )
        };
        let unlevelled = settled_output_peak(0.0, 0.02, 400);

        assert!(
            levelled(true) > unlevelled * 2.5,
            "the fixture is wrong: levelling did not lift the passage with the equalizer on"
        );
        assert_eq!(
            levelled(false).to_bits(),
            unlevelled.to_bits(),
            "the leveller still ran with the equalizer off"
        );
    }

    /// What the gain stage alone does to a stereo buffer: the master gain on both sides and the
    /// balance's attenuation on one, each side's factor folded first, as the engine folds it.
    fn gain_stage_reference(input: &[f32], params: &DspParams) -> Vec<f32> {
        let master = db_to_linear(params.master_gain_db);
        let (left, right) = balance_gains(params.balance);
        let (left, right) = (master * left, master * right);
        input
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|frame| [frame[0] * left, frame[1] * right])
            .collect()
    }

    #[test]
    fn the_bypass_applies_the_master_gain_and_the_balance() {
        // Changed on purpose: audit report R3. This test was
        // `the_bypass_applies_the_master_gain_without_the_balance`: the original's bypass
        // multiplies by the master gain alone (`SosProcess.cpp:500-516`), so a mix balanced to
        // the right recentred the moment FxSound was switched off.
        let params = DspParams {
            power: false,
            master_gain_db: -6.0,
            balance: 20.0,
            ..DspParams::default()
        };
        let mut engine = Engine::new(48_000.0, 256, 2);
        engine.apply(&params);

        let input = vec![0.5_f32; 16];
        let mut buffer = input.clone();
        engine.process(&mut buffer, 2);
        assert_same_bits(&buffer, &gain_stage_reference(&input, &params), "bypassed");
        // -6 dB on the right, -26 dB on the left.
        assert!((buffer[1] - 0.5 * db_to_linear(-6.0)).abs() < 1e-7);
        assert!((buffer[0] - 0.5 * db_to_linear(-26.0)).abs() < 1e-7);
    }

    #[test]
    fn the_bypass_passes_the_audio_untouched_while_the_equalizer_is_off() {
        // Audit report R3 keeps this test as it was before 0.4.0: the powered path leaves the gain
        // stage out with the equalizer off, so the bypass does too, or switching FxSound off
        // would bring in a master gain and a balance that were not playing.
        let mut params = busy_graphic_eq_block(false);
        params.power = false;
        params.master_gain_db = 6.0;
        let mut engine = Engine::new(48_000.0, LEVELLER_BLOCK, 2);
        engine.apply(&params);

        for block in 0..10 {
            let input = continuous_tone(block * LEVELLER_BLOCK, LEVELLER_BLOCK, 0.3);
            let mut buffer = input.clone();
            engine.process(&mut buffer, 2);
            assert_same_bits(&buffer, &input, "bypassed with the equalizer off");
        }
    }

    /// Each side's settled level in dB, output against input, for a steady 300 Hz tone at
    /// −12 dBFS: well under Dynamic Boost's ceiling, so its limiter never acts.
    fn settled_side_levels(params: &DspParams) -> [f32; 2] {
        const AMPLITUDE: f32 = 0.25;
        let mut engine = Engine::new(48_000.0, LEVELLER_BLOCK, 2);
        engine.apply(params);
        let mut peaks = [0.0_f32; 2];
        for block in 0..100 {
            let mut buffer = continuous_tone(block * LEVELLER_BLOCK, LEVELLER_BLOCK, AMPLITUDE);
            engine.process(&mut buffer, 2);
            if block >= 80 {
                for (channel, peak) in peaks.iter_mut().enumerate() {
                    *peak = peak.max(channel_peak(&buffer, 2, channel));
                }
            }
        }
        peaks.map(|peak| 20.0 * (peak / AMPLITUDE).log10())
    }

    /// Master gain −6 dB and balance +6 dB, the audit's R3 scenario, powered and bypassed.
    fn power_switch_levels(eq_on: bool) -> ([f32; 2], [f32; 2]) {
        let params = |power| DspParams {
            power,
            eq_on,
            master_gain_db: -6.0,
            balance: 6.0,
            ..DspParams::default()
        };
        (
            settled_side_levels(&params(true)),
            settled_side_levels(&params(false)),
        )
    }

    #[test]
    fn switching_fxsound_off_with_the_equalizer_on_keeps_the_level_and_the_balance() {
        // Audit report R3. Powered, the gain stage gives -12 dB left and -6 dB right, and Dynamic
        // Boost's ceiling takes 0.3 dB off both. Switched off, the original kept the master gain
        // alone, -6 dB on both sides, so the left side jumped up 6.3 dB and the mix recentred.
        // Now the bypass keeps both: -12 and -6, within Dynamic Boost's 0.3 dB of the powered
        // level.
        let (powered, bypassed) = power_switch_levels(true);
        assert_levels(&powered, &[-12.3, -6.3], "powered, equalizer on");
        assert_levels(&bypassed, &[-12.0, -6.0], "bypassed, equalizer on");
    }

    #[test]
    fn switching_fxsound_off_with_the_equalizer_off_does_not_move_the_level() {
        // Audit report R3, the half of option (b) not taken. Powered with the equalizer off, the
        // gain stage is out with the rest of the block and Dynamic Boost's ceiling takes 0.3 dB
        // off both sides. A bypass that applied the master gain and the balance whatever the
        // equalizer says dropped the left side to -12 dB and the right to -6 dB, a step of 11.7
        // and 5.7 dB that Windows does not have. Now the bypass leaves them out as well: 0 dB on
        // both sides, the same 0.3 dB from the powered level as with the equalizer on.
        let (powered, bypassed) = power_switch_levels(false);
        assert_levels(&powered, &[-0.3, -0.3], "powered, equalizer off");
        assert_levels(&bypassed, &[0.0, 0.0], "bypassed, equalizer off");
    }

    #[test]
    fn the_power_switch_moves_neither_side_by_more_than_dynamic_boosts_ceiling() {
        // Audit report R3: master gain -6 dB and balance +6 dB. Whatever the equalizer switch
        // says, switching FxSound off lands each side within Dynamic Boost's 0.3 dB of where it
        // played powered, and the difference between the sides, the balance, does not move.
        for eq_on in [true, false] {
            let (powered, bypassed) = power_switch_levels(eq_on);
            for side in 0..2 {
                let step = (bypassed[side] - powered[side]).abs();
                assert!(
                    step < 0.35,
                    "equalizer {eq_on}, side {side}: the level moved {step} dB"
                );
            }
            let balance_moved = ((bypassed[0] - bypassed[1]) - (powered[0] - powered[1])).abs();
            assert!(
                balance_moved < 0.05,
                "equalizer {eq_on}: the balance moved {balance_moved} dB"
            );
        }
    }

    #[test]
    fn the_bypass_leaves_out_the_equalizer_curve_and_the_leveller() {
        // Only the gain stage survives a bypass; the curve and the levelling stay behind even
        // while the equalizer is on. Changed on purpose: audit report R3 — the gain stage now
        // carries the fixture's balance as well as its master gain.
        for master_gain_db in [0.0, -3.0] {
            let mut params = busy_graphic_eq_block(true);
            params.power = false;
            params.master_gain_db = master_gain_db;
            let mut engine = Engine::new(48_000.0, LEVELLER_BLOCK, 2);
            engine.apply(&params);

            for block in 0..10 {
                let input = continuous_tone(block * LEVELLER_BLOCK, LEVELLER_BLOCK, 0.02);
                let expected = gain_stage_reference(&input, &params);
                let mut buffer = input;
                engine.process(&mut buffer, 2);
                assert_same_bits(&buffer, &expected, "bypassed with the equalizer on");
            }
        }
    }

    #[test]
    fn the_bypass_applies_the_master_gain_to_every_channel_of_a_surround_layout() {
        // `sosProcessBuffer_MasterGainOnly` runs over `i_num_sample_sets * i_num_channels`
        // samples: the subwoofer and the rears take the gain exactly as the front pair does.
        // Changed on purpose: audit reports R3 and #44 — the balance comes along, and on 5.1 it
        // turns down both right-hand speakers, front and rear, and nothing else.
        let channels = 6;
        let mut engine = Engine::new(48_000.0, 1024, channels);
        engine.set_lfe_channel(Some(LFE));
        engine.apply(&DspParams {
            power: false,
            master_gain_db: -6.0,
            balance: -10.0,
            ..DspParams::default()
        });

        let input = tone(512, channels, 0.5);
        let mut buffer = input.clone();
        engine.process(&mut buffer, channels);
        let gain = db_to_linear(-6.0);
        let right = gain * balance_gains(-10.0).1;
        let expected: Vec<f32> = input
            .chunks_exact(channels)
            .flat_map(|frame| {
                frame.iter().enumerate().map(move |(channel, s)| {
                    // FL FR FC LFE RL RR: the right-hand pair is 1 and 5.
                    s * if channel == 1 || channel == 5 {
                        right
                    } else {
                        gain
                    }
                })
            })
            .collect();
        assert_same_bits(&buffer, &expected, "bypassed 5.1");
    }

    // --- Balance by side (audit report #44) ---------------------------------------------------

    /// Each channel's level in dB against the same engine with the balance centred, so the
    /// effects Dynamic Boost always applies cancel out.
    fn balance_levels(channels: usize, setup: impl Fn(&mut Engine), balance: f32) -> Vec<f32> {
        let render = |balance| {
            let mut engine = Engine::new(48_000.0, 4096, channels);
            setup(&mut engine);
            engine.apply(&DspParams {
                balance,
                ..DspParams::default()
            });
            let mut buffer = tone(4096, channels, 0.25);
            engine.process(&mut buffer, channels);
            buffer
        };
        let (balanced, centred) = (render(balance), render(0.0));
        (0..channels)
            .map(|channel| {
                20.0 * (channel_peak(&balanced, channels, channel)
                    / channel_peak(&centred, channels, channel))
                .log10()
            })
            .collect()
    }

    fn assert_levels(got: &[f32], want: &[f32], what: &str) {
        for (channel, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(
                (g - w).abs() < 0.01,
                "{what}: channel {channel} at {g:.2} dB, expected {w} dB (all: {got:?})"
            );
        }
    }

    #[test]
    fn a_balance_on_surround_turns_down_every_speaker_on_one_side() {
        // 5.1, balance +10 dB to the right. The port used to turn down channel 0 alone — the
        // front-left speaker at -10 dB and the rear-left one untouched — which on a surround
        // system is a speaker switched down, not a balance.
        let levels = balance_levels(6, |engine| engine.set_lfe_channel(Some(LFE)), 10.0);
        assert_levels(&levels, &[-10.0, 0.0, 0.0, 0.0, -10.0, 0.0], "5.1 right");

        let levels = balance_levels(6, |engine| engine.set_lfe_channel(Some(LFE)), -10.0);
        assert_levels(&levels, &[0.0, -10.0, 0.0, 0.0, 0.0, -10.0], "5.1 left");

        // 7.1: FL FR FC LFE RL RR SL SR — three speakers a side.
        let levels = balance_levels(8, |engine| engine.set_lfe_channel(Some(LFE)), 10.0);
        assert_levels(
            &levels,
            &[-10.0, 0.0, 0.0, 0.0, -10.0, 0.0, -10.0, 0.0],
            "7.1 right",
        );
    }

    #[test]
    fn stereo_and_mono_balance_as_they_always_did() {
        assert_levels(&balance_levels(2, |_| {}, 10.0), &[-10.0, 0.0], "stereo");
        assert_levels(&balance_levels(2, |_| {}, -10.0), &[0.0, -10.0], "stereo");
        assert_levels(&balance_levels(1, |_| {}, 10.0), &[0.0], "mono has no side");
    }

    #[test]
    fn a_device_that_names_its_sides_is_balanced_by_them() {
        // FL FC FR LFE SL SR: front right at index 2, as some devices order it.
        use ChannelSide::{Centre as C, Left as L, Right as R};
        let levels = balance_levels(
            6,
            |engine| {
                engine.set_front_pair(Some((0, 2)));
                engine.set_lfe_channel(Some(3));
                engine.set_channel_sides(Some(&[L, C, R, C, L, R]));
            },
            10.0,
        );
        assert_levels(&levels, &[-10.0, 0.0, 0.0, 0.0, -10.0, 0.0], "named sides");
    }

    #[test]
    fn an_unfamiliar_order_nobody_named_balances_the_front_pair_alone() {
        // The same device with only the front pair and the subwoofer known: the default layout's
        // guess would call the centre speaker "right", so only the pair the engine is sure of is
        // balanced.
        let levels = balance_levels(
            6,
            |engine| {
                engine.set_front_pair(Some((0, 2)));
                engine.set_lfe_channel(Some(3));
            },
            -10.0,
        );
        assert_levels(
            &levels,
            &[0.0, 0.0, -10.0, 0.0, 0.0, 0.0],
            "front pair only",
        );
    }

    #[test]
    fn the_default_sides_follow_pipewires_default_positions() {
        use ChannelSide::{Centre as C, Left as L, Right as R};
        let expected: [&[ChannelSide]; 8] = [
            &[C],
            &[L, R],
            &[L, R, C],
            &[L, R, L, R],
            &[L, R, C, L, R],
            &[L, R, C, C, L, R],
            &[L, R, C, C, C, L, R],
            &[L, R, C, C, L, R, L, R],
        ];
        for (index, want) in expected.iter().enumerate() {
            let channels = index + 1;
            let engine = Engine::new(48_000.0, 256, channels);
            assert_eq!(engine.channel_sides(), *want, "{channels} channels");
        }
        // Named sides of the wrong length are ignored rather than half-applied.
        let mut engine = Engine::new(48_000.0, 256, 6);
        engine.set_channel_sides(Some(&[R, L]));
        assert_eq!(engine.channel_sides(), expected[5]);
        // And a format change drops back to the default for the new count.
        engine.set_channel_sides(Some(&[R, L, C, C, R, L]));
        assert_eq!(engine.channel_sides(), &[R, L, C, C, R, L]);
        engine.set_format(48_000.0, 2);
        assert_eq!(engine.channel_sides(), &[L, R]);
    }

    #[test]
    fn everything_at_maximum_stays_finite_and_bounded() {
        let mut engine = Engine::new(48_000.0, 4096, 2);
        let mut params = DspParams::default();
        for id in EffectId::ALL {
            params.set_effect(id, 1.0);
        }
        for band in 0..10 {
            params.band_boost_db[band] = 12.0;
        }
        params.master_gain_db = 20.0;
        engine.apply(&params);

        let mut buffer = tone(4096, 2, 0.9);
        engine.process(&mut buffer, 2);
        assert!(buffer.iter().all(|s| s.is_finite()));
    }

    /// The three stages that latch a bad sample do it in three different ways — the biquads and
    /// the reverb tank go non-finite, the leveller goes silent, Dynamic Boost's gain sticks — so
    /// each one is measured on the quantity that actually moves.
    fn poison(buffer: &mut [f32], value: f32) {
        buffer[0] = value;
    }

    fn peak_of(buffer: &[f32]) -> f32 {
        buffer.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))
    }

    /// Pins the premise of the app-level fix: a power cycle does not clear everything, so
    /// something above the engine has to send `ResetFilterState`.
    ///
    /// `Chain::set_power` resets the five effects, so the reverb tail really does go — but the
    /// equalizer and the volume leveller are outside the chain and keep their state, and the
    /// leveller's is the one a listener notices, because its gain is built from seconds of
    /// history. If anyone ever makes `apply` reset on a false→true transition, this fails and the
    /// two fixes get reconciled in one place instead of quietly both existing.
    /// Peak of one channel of an interleaved buffer.
    fn channel_peak(buffer: &[f32], channels: usize, channel: usize) -> f32 {
        buffer
            .iter()
            .skip(channel)
            .step_by(channels)
            .fold(0.0_f32, |acc, s| acc.max(s.abs()))
    }

    /// A buffer that is silent except for one full-scale sample in one channel.
    fn impulse(frames: usize, channels: usize, channel: usize) -> Vec<f32> {
        let mut buffer = vec![0.0_f32; frames * channels];
        buffer[64 * channels + channel] = 1.0;
        buffer
    }

    /// Wrong routing is worse than a wrong equalizer curve: the listener hears the left channel on
    /// the right, or the centre from the subwoofer, and nothing in the interface suggests why.
    /// Nothing asserted this before, so every divergence was invisible to the test suite.
    #[test]
    fn an_impulse_stays_in_the_channel_it_was_put_in() {
        for channels in 1..=crate::biquad::MAX_CHANNELS {
            let mut engine = Engine::new(48_000.0, 4096, channels);
            // Every effect amount at zero. Surround and Ambience deliberately cross-mix the front
            // pair and get their own test below; what is left — the equalizer, the gain stage, the
            // leveller, and Dynamic Boost, which is never bypassed — must be strictly per-channel,
            // at any channel count.
            let mut params = DspParams {
                volume_leveling_db: 2.0,
                ..DspParams::default()
            };
            for band in 0..10 {
                params.band_boost_db[band] = if band % 2 == 0 { 6.0 } else { -6.0 };
            }
            engine.apply(&params);

            for source in 0..channels {
                engine.reset();
                let mut buffer = impulse(2048, channels, source);
                engine.process(&mut buffer, channels);

                assert!(
                    channel_peak(&buffer, channels, source) > 0.01,
                    "{channels}ch: the impulse vanished from channel {source}"
                );
                for other in (0..channels).filter(|c| *c != source) {
                    let leak = channel_peak(&buffer, channels, other);
                    assert!(
                        leak < 1e-6,
                        "{channels}ch: an impulse in channel {source} leaked {leak} into {other}"
                    );
                }
            }
        }
    }

    /// The two effects that are inherently stereo run one instance over the front pair, which is
    /// what the original does. That is a deliberate divergence from per-channel processing, so it
    /// is pinned rather than left to be rediscovered — and it is exactly why a device that reports
    /// its channels in a non-standard order is a routing hazard: "the front pair" is currently
    /// "channels 0 and 1", not "whichever channels are FL and FR".
    #[test]
    fn the_two_effects_that_mix_channels_only_touch_the_first_two() {
        for channels in 2..=crate::biquad::MAX_CHANNELS {
            for (effect, name) in [
                (EffectId::Surround, "Surround"),
                (EffectId::Ambience, "Ambience"),
            ] {
                let mut engine = Engine::new(48_000.0, 4096, channels);
                let mut params = DspParams::default();
                params.set_effect(effect, 1.0);
                engine.apply(&params);

                // An impulse in the left of the pair must reach the right of the pair.
                engine.reset();
                let mut buffer = impulse(2048, channels, 0);
                engine.process(&mut buffer, channels);
                assert!(
                    channel_peak(&buffer, channels, 1) > 1e-4,
                    "{channels}ch: {name} did not reach the other half of the front pair"
                );
                for rear in 2..channels {
                    assert!(
                        channel_peak(&buffer, channels, rear) < 1e-6,
                        "{channels}ch: {name} spilled the front pair into channel {rear}"
                    );
                }

                // And a rear channel must be left alone in both directions.
                engine.reset();
                let mut buffer = impulse(2048, channels, channels - 1);
                engine.process(&mut buffer, channels);
                if channels > 2 {
                    assert!(
                        channel_peak(&buffer, channels, 0) < 1e-6
                            && channel_peak(&buffer, channels, 1) < 1e-6,
                        "{channels}ch: {name} pulled a rear channel into the front pair"
                    );
                }
            }
        }
    }

    /// A 5.1 layout in the order every consumer device uses: FL FR FC LFE RL RR.
    const LFE: usize = 3;

    /// The failure this prevents is the one a listener notices instantly and cannot attribute to
    /// FxSound: a device that reports `FL, FC, FR, LFE, SL, SR` — front right at index 2, not 1 —
    /// had its stereo widener applied across front-left and *centre*, pulling dialogue out of the
    /// centre channel and into a phantom image.
    #[test]
    fn the_stereo_stages_follow_the_layout_rather_than_the_first_two_channels() {
        let channels = 6;
        const FL: usize = 0;
        const FC: usize = 1;
        const FR: usize = 2;

        for (effect, name) in [
            (EffectId::Surround, "Surround"),
            (EffectId::Ambience, "Ambience"),
        ] {
            let mut engine = Engine::new(48_000.0, 4096, channels);
            engine.set_front_pair(Some((FL, FR)));
            let mut params = DspParams::default();
            params.set_effect(effect, 1.0);
            engine.apply(&params);

            let mut buffer = impulse(2048, channels, FL);
            engine.process(&mut buffer, channels);

            assert!(
                channel_peak(&buffer, channels, FR) > 1e-4,
                "{name} did not reach front right at index {FR}"
            );
            assert!(
                channel_peak(&buffer, channels, FC) < 1e-6,
                "{name} reached the centre channel, which is the dialogue"
            );
        }
    }

    #[test]
    fn the_harmonic_generator_is_kept_out_of_the_subwoofer() {
        // `docs/spec/08-dsp-api.md:905-906` — the original sends Fidelity to the front, rear, side
        // and centre instances and explicitly not to the LFE. Putting sine-fold harmonics on the
        // one channel that exists to carry only the bottom two octaves is the clearest possible
        // case of the wrong signal in the wrong place.
        let channels = 6;
        let mut params = DspParams::default();
        params.set_effect(EffectId::Fidelity, 1.0);

        let mut without = Engine::new(48_000.0, 4096, channels);
        without.apply(&params);
        let mut with = Engine::new(48_000.0, 4096, channels);
        with.set_lfe_channel(Some(LFE));
        with.apply(&params);

        let mut a = tone(2048, channels, 0.5);
        let mut b = a.clone();
        let reference = a.clone();
        without.process(&mut a, channels);
        with.process(&mut b, channels);

        let latency = with.latency_frames();
        let sample = |buf: &[f32], frame: usize, ch: usize| buf[frame * channels + ch];
        let mut touched = 0.0_f32;
        let mut untouched = 0.0_f32;
        for frame in latency..2000 {
            let dry = sample(&reference, frame - latency, LFE);
            untouched = untouched.max((sample(&b, frame, LFE) - dry * 0.966_051).abs());
            touched = touched.max((sample(&a, frame, LFE) - dry * 0.966_051).abs());
        }
        assert!(
            touched > 1e-3,
            "the fixture is wrong: Fidelity did not change the subwoofer even without the exclusion"
        );
        assert!(
            untouched < 1e-4,
            "Fidelity still reached the subwoofer: {untouched} away from the dry signal"
        );

        // And the channels it is supposed to reach must still be reached.
        let mut front = 0.0_f32;
        for frame in latency..2000 {
            let dry = sample(&reference, frame - latency, 0);
            front = front.max((sample(&b, frame, 0) - dry * 0.966_051).abs());
        }
        assert!(
            front > 1e-3,
            "Fidelity stopped reaching the front channels too"
        );
    }

    #[test]
    fn the_subwoofer_does_not_drag_the_leveller_down_with_it() {
        // The LFE channel carries a deliberately enormous share of a film's energy. Letting it
        // into the level detector pulls the gain down on everything else for a reason that has
        // nothing to do with how loud the programme is.
        //
        // Changed on purpose: audit report #3. The fixture used to be a subwoofer at nine times
        // the fronts, which only worked because the original never levelled the subwoofer: now it
        // rides the fronts' gain, and a sub at 0.9 lifted by x4 would cross full scale, so the
        // peak safety — which has to count every channel the gain reaches — rightly holds the
        // whole mix down. What the test is about is the *statistics*, so the loud subwoofer here
        // stays under the ceiling once levelled, and the quiet one is silent: had the sub reached
        // the RMS, the fronts would come out 9.4 % apart. The fronts sit where the gain is below
        // its cap, so a leak could not hide behind the cap either.
        let channels = 6;
        let params = DspParams {
            volume_leveling_db: 4.0,
            ..DspParams::default()
        };

        let mut quiet_sub = Engine::new(48_000.0, 4096, channels);
        quiet_sub.set_lfe_channel(Some(LFE));
        quiet_sub.apply(&params);
        let mut loud_sub = Engine::new(48_000.0, 4096, channels);
        loud_sub.set_lfe_channel(Some(LFE));
        loud_sub.apply(&params);

        let mut last_quiet = Vec::new();
        let mut last_loud = Vec::new();
        for _ in 0..30 {
            let mut a = tone(2048, channels, 0.25);
            let mut b = a.clone();
            for frame in a.chunks_exact_mut(channels) {
                frame[LFE] = 0.0;
            }
            for frame in b.chunks_exact_mut(channels) {
                frame[LFE] *= 1.05;
            }
            quiet_sub.process(&mut a, channels);
            loud_sub.process(&mut b, channels);
            last_quiet = a;
            last_loud = b;
        }

        let front_quiet = channel_peak(&last_quiet, channels, 0);
        let front_loud = channel_peak(&last_loud, channels, 0);
        assert!(
            front_quiet > 0.25 * 2.5,
            "the fixture must level the fronts well up, got {front_quiet}"
        );
        assert!(
            channel_peak(&last_loud, channels, LFE) < CEILING,
            "the fixture must keep the levelled subwoofer under the ceiling"
        );
        assert!(
            (front_quiet - front_loud).abs() < front_quiet * 0.02,
            "a loud subwoofer moved the front channels: {front_loud} against {front_quiet}"
        );
    }

    #[test]
    fn a_quiet_surround_scene_keeps_its_subwoofer_level_with_the_fronts() {
        // Audit report #3. The original left the LFE at x1 while the leveller lifted a quiet scene
        // (`SosProcess.cpp:382-383`, `:908`): on this fixture the fronts came out 12.5 dB up and
        // the subwoofer 0.3 dB down, so the bass fell 12.8 dB behind the rest of the mix; now both
        // come out 12.8 dB up. Nothing after the leveller treats the two differently at the
        // default snapshot, so whatever the chain does to the fronts it does to the subwoofer.
        let channels = 6;
        let mut engine = Engine::new(48_000.0, 4096, channels);
        engine.set_lfe_channel(Some(LFE));
        engine.apply(&DspParams {
            volume_leveling_db: 4.0,
            ..DspParams::default()
        });

        let mut last = Vec::new();
        for _ in 0..100 {
            let mut block = tone(2048, channels, 0.05);
            engine.process(&mut block, channels);
            last = block;
        }

        let front_db = 20.0 * (channel_peak(&last, channels, 0) / 0.05).log10();
        let sub_db = 20.0 * (channel_peak(&last, channels, LFE) / 0.05).log10();
        assert!(
            front_db > 10.0,
            "the fixture must lift the scene by more than 10 dB, got {front_db} dB"
        );
        assert!(
            (front_db - sub_db).abs() < 0.1,
            "the subwoofer came out {sub_db:.2} dB against the fronts' {front_db:.2} dB"
        );
    }

    #[test]
    fn a_power_cycle_alone_does_not_clear_the_levellers_gain() {
        let params = DspParams {
            volume_leveling_db: 4.0,
            ..DspParams::default()
        };

        // Drive the leveller with a loud passage until its gain has settled downwards.
        let mut used = Engine::new(48_000.0, 4096, 2);
        used.apply(&params);
        for _ in 0..40 {
            let mut loud = tone(4096, 2, 0.9);
            used.process(&mut loud, 2);
        }

        let off = DspParams {
            power: false,
            ..params
        };
        used.apply(&off);
        used.apply(&params);

        let mut fresh = Engine::new(48_000.0, 4096, 2);
        fresh.apply(&params);

        let mut after_cycle = tone(4096, 2, 0.1);
        let mut from_fresh = tone(4096, 2, 0.1);
        used.process(&mut after_cycle, 2);
        fresh.process(&mut from_fresh, 2);

        assert!(
            (peak_of(&after_cycle) - peak_of(&from_fresh)).abs() > 1e-4,
            "the power cycle already produced a fresh leveller, so the app need not send \
             ResetFilterState"
        );

        used.handle_event(DspEvent::ResetFilterState);
        let mut after_reset = tone(4096, 2, 0.1);
        used.process(&mut after_reset, 2);
        assert!(
            (peak_of(&after_reset) - peak_of(&from_fresh)).abs() < 1e-4,
            "ResetFilterState did not bring the leveller back to its initial gain"
        );
    }

    #[test]
    fn one_bad_sample_does_not_poison_the_filters_for_the_rest_of_the_session() {
        // With Ambience up, a single non-finite sample used to reach the 663 kB delay arena and
        // every later block came out non-finite on both channels, for good.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut engine = Engine::new(48_000.0, 4096, 2);
            let mut params = DspParams::default();
            params.set_effect(EffectId::Ambience, 1.0);
            for band in 0..10 {
                params.band_boost_db[band] = 6.0;
            }
            engine.apply(&params);

            let mut first = tone(512, 2, 0.25);
            poison(&mut first, bad);
            engine.process(&mut first, 2);
            assert!(
                first.iter().all(|s| s.is_finite()),
                "{bad:?} reached the output"
            );

            for _ in 0..200 {
                let mut block = tone(512, 2, 0.25);
                engine.process(&mut block, 2);
                assert!(
                    block.iter().all(|s| s.is_finite()),
                    "{bad:?} was still poisoning the chain 200 blocks later"
                );
            }
        }
    }

    #[test]
    fn an_infinity_does_not_silence_the_volume_leveller_for_good() {
        // `+inf` pinned the side-chain peak, and the peak-safety division then made both ends of
        // the gain ramp exactly zero — finite, so nothing downstream could notice, and silent
        // until the next preset change.
        let mut engine = Engine::new(48_000.0, 4096, 2);
        let params = DspParams {
            volume_leveling_db: 4.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut warm = tone(4096, 2, 0.25);
        engine.process(&mut warm, 2);
        let reference = peak_of(&warm);
        assert!(reference > 0.01, "the reference block should not be silent");

        let mut bad = tone(4096, 2, 0.25);
        poison(&mut bad, f32::INFINITY);
        engine.process(&mut bad, 2);

        let mut recovered = 0.0_f32;
        for _ in 0..40 {
            let mut block = tone(4096, 2, 0.25);
            engine.process(&mut block, 2);
            recovered = peak_of(&block);
        }
        assert!(
            recovered > reference * 0.5,
            "the leveller stayed silent: {recovered} against a reference of {reference}"
        );
    }

    #[test]
    fn an_infinity_does_not_stick_dynamic_boosts_auto_gain() {
        // Dynamic Boost is never bypassed, so a stuck level estimator changed the output level
        // for every user with the default preset. The symptom was a permanent +1.06 factor.
        let mut clean = Engine::new(48_000.0, 4096, 2);
        let mut dirty = Engine::new(48_000.0, 4096, 2);

        for _ in 0..20 {
            let mut block = tone(4096, 2, 0.2);
            clean.process(&mut block, 2);
        }
        let mut bad = tone(4096, 2, 0.2);
        poison(&mut bad, f32::INFINITY);
        dirty.process(&mut bad, 2);
        for _ in 0..19 {
            let mut block = tone(4096, 2, 0.2);
            dirty.process(&mut block, 2);
        }

        let mut a = tone(4096, 2, 0.2);
        let mut b = tone(4096, 2, 0.2);
        clean.process(&mut a, 2);
        dirty.process(&mut b, 2);
        let (want, got) = (peak_of(&a), peak_of(&b));
        assert!(
            (got - want).abs() < want * 0.01,
            "the auto-gain did not come back: {got} against {want}"
        );
    }

    // --- Parameter glides (audit report #11) ----------------------------------------------------
    //
    // Each scenario was measured on the engine as it stood before the glides (0.4.0 up to c4c40fa),
    // and the comments quote those numbers beside what the engine does now: 48 kHz stereo, the
    // default snapshot except for what the test moves.

    /// A cosine, phase-continuous across calls, at `amplitude` on the left and
    /// `amplitude · right_gain` on the right. At 50 Hz and 480-frame blocks every block starts on a
    /// crest, so a step at a block boundary lands where the waveform is largest.
    fn crest_aligned(
        start_frame: usize,
        frames: usize,
        hz: f64,
        amplitude: f32,
        right_gain: f32,
    ) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let n = (start_frame + i) as f64;
                let value = (f64::from(amplitude)
                    * (std::f64::consts::TAU * hz * n / 48_000.0).cos())
                    as f32;
                [value, value * right_gain]
            })
            .collect()
    }

    /// What a fresh stereo engine hands back when `params_for(block)` is applied before each block.
    fn render_moving(
        block: usize,
        blocks: usize,
        tone: (f64, f32, f32),
        params_for: impl Fn(usize) -> DspParams,
    ) -> Vec<f32> {
        let (hz, amplitude, right_gain) = tone;
        let mut engine = Engine::new(48_000.0, block, 2);
        let mut rendered = Vec::with_capacity(block * blocks * 2);
        for index in 0..blocks {
            engine.apply(&params_for(index));
            let mut buffer = crest_aligned(index * block, block, hz, amplitude, right_gain);
            engine.process(&mut buffer, 2);
            rendered.extend_from_slice(&buffer);
        }
        rendered
    }

    fn left_channel(interleaved: &[f32]) -> Vec<f32> {
        interleaved.iter().step_by(2).copied().collect()
    }

    /// The largest change between two neighbouring samples.
    fn largest_step(signal: &[f32]) -> f32 {
        signal
            .windows(2)
            .fold(0.0_f32, |acc, pair| acc.max((pair[1] - pair[0]).abs()))
    }

    /// RMS and peak, in dBFS, of what lies above `corner_hz`, from frame `skip` on: a 12th-order
    /// Butterworth high-pass, six of the crate's own sections. A parameter moving smoothly under a
    /// low tone puts next to nothing up there; a step puts a click there.
    fn above(signal: &[f32], corner_hz: f32, skip: usize) -> (f32, f32) {
        let design = crate::biquad::calc_butterworth_highpass(48_000.0, corner_hz);
        let mut sections = [crate::biquad::Section::new(); 6];
        for section in &mut sections {
            section.coeffs = design;
        }
        let (mut sum, mut peak) = (0.0_f64, 0.0_f32);
        for (index, &sample) in signal.iter().enumerate() {
            let filtered = sections
                .iter_mut()
                .fold(sample, |x, section| section.tick_general(0, x));
            if index >= skip {
                sum += f64::from(filtered) * f64::from(filtered);
                peak = peak.max(filtered.abs());
            }
        }
        let rms = (sum / (signal.len() - skip) as f64).sqrt() as f32;
        (
            20.0 * rms.max(1e-12).log10(),
            20.0 * peak.max(1e-12).log10(),
        )
    }

    /// The steepest a cosine of this amplitude and frequency gets on its own, after Dynamic
    /// Boost's ceiling, with a hair of headroom for rounding: no glide may step further than this.
    fn own_slope(amplitude: f32, hz: f32) -> f32 {
        amplitude * crate::effects::dynamic_boost::MAX_OUTPUT * std::f32::consts::TAU * hz
            / 48_000.0
            * 1.1
    }

    #[test]
    fn a_two_decibel_master_gain_step_is_a_glide_and_not_a_click() {
        // The audit's scenario: Master Gain 0 → −2 dB under a 50 Hz tone at −6 dBFS. It used to
        // move the waveform by 0.0993 between two samples, a click whose part above 1 kHz peaked
        // at −24.9 dBFS; now no step is larger than the tone's own, 0.0032, and above 1 kHz the
        // peak is −81.5 dBFS.
        let rendered = render_moving(480, 40, (50.0, 0.5, 1.0), |block| DspParams {
            master_gain_db: if block >= 20 { -2.0 } else { 0.0 },
            ..DspParams::default()
        });
        let left = left_channel(&rendered);
        let steepest = largest_step(&left[4_800..]);
        assert!(
            steepest < own_slope(0.5, 50.0),
            "the gain stepped by {steepest}"
        );
        let (_, peak) = above(&left, 1_000.0, 4_800);
        assert!(peak < -70.0, "a click above 1 kHz at {peak} dBFS");
    }

    #[test]
    fn a_balance_step_is_a_glide_and_not_a_click() {
        // Balance 0 → +6 dB: the left side fell by 0.241 in one sample (−17.2 dBFS above 1 kHz);
        // now by no more than the tone's own 0.0032 (−73.9 dBFS).
        let rendered = render_moving(480, 40, (50.0, 0.5, 1.0), |block| DspParams {
            balance: if block >= 20 { 6.0 } else { 0.0 },
            ..DspParams::default()
        });
        let left = left_channel(&rendered);
        let steepest = largest_step(&left[4_800..]);
        assert!(
            steepest < own_slope(0.5, 50.0),
            "the balance stepped by {steepest}"
        );
        let (_, peak) = above(&left, 1_000.0, 4_800);
        assert!(peak < -65.0, "a click above 1 kHz at {peak} dBFS");
    }

    #[test]
    fn a_gain_glide_lands_on_exactly_what_a_stage_that_never_glided_plays() {
        // Bypassed with the equalizer on, the engine is the gain stage and nothing else, so once
        // the 20 ms glide has run the output must be the stage's own, bit for bit — and on the
        // way it moves one way only, from the old gain to the new.
        let before = DspParams {
            power: false,
            ..DspParams::default()
        };
        let after = DspParams {
            power: false,
            master_gain_db: -6.0,
            balance: 20.0,
            ..DspParams::default()
        };
        let mut engine = Engine::new(48_000.0, 480, 2);
        engine.apply(&before);
        let mut warm = vec![0.5_f32; 960];
        engine.process(&mut warm, 2);
        engine.apply(&after);

        let input = vec![0.5_f32; 2 * 960];
        let mut gliding = input.clone();
        engine.process(&mut gliding, 2);
        let left = left_channel(&gliding);
        assert!(
            left.windows(2).all(|pair| pair[1] <= pair[0]),
            "the glide turned back on itself"
        );
        assert!(left[0] > 0.49, "the glide skipped its start: {}", left[0]);

        for _ in 0..3 {
            let mut settled = input.clone();
            engine.process(&mut settled, 2);
            assert_same_bits(
                &settled,
                &gain_stage_reference(&input, &after),
                "after the glide",
            );
        }
    }

    #[test]
    fn a_snapshot_lands_at_once_on_an_engine_nobody_has_heard_yet() {
        // A glide hides a change from a listener; before the first sample there is no listener
        // and nothing to glide from, so the first snapshot of a stream, and the first after a
        // format change, play from their first sample. Once audio has gone through, a change
        // glides.
        let gain = |db: f32| DspParams {
            power: false,
            master_gain_db: db,
            ..DspParams::default()
        };
        let mut engine = Engine::new(48_000.0, 256, 2);
        engine.apply(&gain(-6.0));
        let mut block = vec![1.0_f32; 16];
        engine.process(&mut block, 2);
        assert_eq!(block[0].to_bits(), db_to_linear(-6.0).to_bits());

        engine.apply(&gain(-12.0));
        let mut block = vec![1.0_f32; 16];
        engine.process(&mut block, 2);
        assert!(
            block[0] > db_to_linear(-6.5),
            "a heard engine jumped: {}",
            block[0]
        );

        engine.set_format(96_000.0, 2);
        engine.apply(&gain(-18.0));
        let mut block = vec![1.0_f32; 16];
        engine.process(&mut block, 2);
        assert_eq!(block[0].to_bits(), db_to_linear(-18.0).to_bits());
    }

    /// A 62.5 Hz band dragged from 0 to +12 dB a decibel at a time, at the GUI's 60 frames a
    /// second (every 800 frames, as two 400-frame blocks).
    fn band_drag(block: usize) -> DspParams {
        let mut params = DspParams::default();
        params.band_boost_db[0] = (block.saturating_sub(10) / 2).min(12) as f32;
        params
    }

    #[test]
    fn dragging_an_equalizer_band_leaves_no_zipper_on_the_bass() {
        // Under a 25 Hz tone, every redesign of the band stepped the filter and the steps came out
        // as a buzz above 80 Hz, where the tone has nothing: −57.3 dBFS RMS, peaks of −43.7. The
        // crossfade takes that to −66.9 and −55.7.
        let rendered = render_moving(400, 60, (25.0, 0.3, 1.0), band_drag);
        let (rms, peak) = above(&left_channel(&rendered), 80.0, 4_000);
        assert!(
            rms < -63.0 && peak < -52.0,
            "zipper at {rms} dBFS RMS, {peak} peak"
        );

        // On the band's own frequency, what lands above 300 Hz: −84.0 RMS and −66.6 peak before,
        // −95.1 and −74.7 now.
        let rendered = render_moving(400, 60, (62.5, 0.15, 1.0), band_drag);
        let (rms, peak) = above(&left_channel(&rendered), 300.0, 4_000);
        assert!(
            rms < -90.0 && peak < -71.0,
            "zipper at {rms} dBFS RMS, {peak} peak"
        );
    }

    #[test]
    fn dragging_bass_leaves_no_zipper_under_it() {
        // Bass dragged from 0 to 10 a position at a time at the GUI's rate. Under a 30 Hz tone the
        // buzz above 100 Hz was −55.2 dBFS RMS with peaks of −40.3; it is −71.2 and −56.7 now. On
        // 90 Hz, above 300 Hz: −85.0 and −65.3 before, −100.0 and −78.5 now.
        let bass_drag = |block: usize| {
            let mut params = DspParams::default();
            let position = (block.saturating_sub(10) / 2).min(10);
            params.set_effect(EffectId::Bass, position as f32 / 10.0);
            params
        };
        let rendered = render_moving(400, 60, (30.0, 0.2, 1.0), bass_drag);
        let (rms, peak) = above(&left_channel(&rendered), 100.0, 4_000);
        assert!(
            rms < -66.0 && peak < -52.0,
            "zipper at {rms} dBFS RMS, {peak} peak"
        );

        let rendered = render_moving(400, 60, (90.0, 0.05, 1.0), bass_drag);
        let (rms, peak) = above(&left_channel(&rendered), 300.0, 4_000);
        assert!(
            rms < -95.0 && peak < -74.0,
            "zipper at {rms} dBFS RMS, {peak} peak"
        );
    }

    #[test]
    fn an_effect_switched_on_or_off_glides_instead_of_stepping() {
        // Each effect taken from 0 to its top position at block 20 under a 50 Hz tone, and back
        // to 0 at block 40. The largest step between two samples before the glides, on the way
        // up: Dynamic Boost 0.135 (+11.6 dB at once), Surround 0.294, Ambience 0.0497, Fidelity
        // 0.505. Now none moves further than the tone does on its own, at the level the effect
        // takes it to: +11.6 dB for Dynamic Boost, and ×2.52 for a left channel at 0.2 beside a
        // right at −0.1 that Surround widens.
        for (effect, amplitude, right_gain, settled_gain) in [
            (EffectId::DynamicBoost, 0.05, 1.0, 3.81),
            (EffectId::Surround, 0.2, -0.5, 2.53),
            (EffectId::Ambience, 0.5, 1.0, 1.0),
            (EffectId::Fidelity, 0.5, 1.0, 1.0),
        ] {
            let top = if effect == EffectId::DynamicBoost {
                0.6
            } else {
                1.0
            };
            let rendered = render_moving(480, 60, (50.0, amplitude, right_gain), |block| {
                let mut params = DspParams::default();
                params.set_effect(effect, if (20..40).contains(&block) { top } else { 0.0 });
                params
            });
            let left = left_channel(&rendered);
            let steepest = largest_step(&left[4_800..]);
            let bound = own_slope(amplitude * settled_gain, 50.0);
            assert!(
                steepest < bound,
                "{effect:?} stepped by {steepest}, more than the tone's own {bound}"
            );
        }
    }

    #[test]
    fn switching_volume_leveling_off_lets_the_lift_down_gently() {
        // A −34 dBFS tone lifted by 13.1 dB, then Volume Leveling 4 → 0: the level fell from
        // 0.087 to 0.019 between two samples (a step of 0.068). It now glides down over 20 ms, no
        // step larger than the tone's own, and settles on the same unlevelled 0.019.
        let rendered = render_moving(480, 220, (300.0, 0.02, 1.0), |block| DspParams {
            volume_leveling_db: if block >= 200 { 0.0 } else { 4.0 },
            ..DspParams::default()
        });
        let left = left_channel(&rendered);
        let lifted = left[199 * 480..200 * 480]
            .iter()
            .fold(0.0_f32, |acc, s| acc.max(s.abs()));
        assert!(lifted > 0.08, "the fixture was not lifted: {lifted}");
        let steepest = largest_step(&left[190 * 480..]);
        assert!(
            steepest < own_slope(lifted, 300.0),
            "the level fell by {steepest} in one sample"
        );
        let settled = left[210 * 480..]
            .iter()
            .fold(0.0_f32, |acc, s| acc.max(s.abs()));
        assert!(
            (settled - 0.02 * crate::effects::dynamic_boost::MAX_OUTPUT).abs() < 1e-4,
            "the stage did not let go: {settled}"
        );
    }

    /// The loudest sample of one 480-frame block of a mono signal.
    fn block_peak(signal: &[f32], block: usize) -> f32 {
        signal[block * 480..(block + 1) * 480]
            .iter()
            .fold(0.0_f32, |acc, s| acc.max(s.abs()))
    }

    #[test]
    fn levelling_switched_off_while_its_stage_is_left_out_does_not_burst_when_it_comes_back() {
        // The same −34 dBFS tone lifted by 13 dB; FxSound, or the equalizer, switched off at
        // block 200, Volume Leveling 4 → 0 while it is off or in the snapshot that switches it
        // back on, and the switch back on at block 220. The glide that lets the lift down only
        // plays while the stage runs, so it waited for the switch-on and played the old lift into
        // audio the listener had last heard unlevelled: 0.087, then 0.053, for a steady 0.019.
        // A stage left out now drops the glide and is switched off at once, as before the glides,
        // so the switch-on plays 0.019. (Switching FxSound off still drops the lift at once, as it
        // always has; the equalizer's switch fades it, which the test below measures.)
        for switch_is_power in [true, false] {
            for with_the_switch_on in [false, true] {
                let rendered = render_moving(480, 230, (300.0, 0.02, 1.0), |block| {
                    let off = (200..220).contains(&block);
                    let levelling_off = if with_the_switch_on {
                        block >= 220
                    } else {
                        block >= 205
                    };
                    DspParams {
                        power: !(off && switch_is_power),
                        eq_on: !(off && !switch_is_power),
                        volume_leveling_db: if levelling_off { 0.0 } else { 4.0 },
                        ..DspParams::default()
                    }
                });
                let left = left_channel(&rendered);
                let what = format!(
                    "{} switched, levelling off {}",
                    if switch_is_power {
                        "power"
                    } else {
                        "equalizer"
                    },
                    if with_the_switch_on {
                        "with the switch-on"
                    } else {
                        "while off"
                    }
                );
                assert!(
                    block_peak(&left, 199) > 0.08,
                    "{what}: the fixture was not lifted"
                );
                let loudest = (220..230)
                    .map(|block| block_peak(&left, block))
                    .fold(0.0_f32, f32::max);
                assert!(
                    loudest < 0.0205,
                    "{what}: the switch-on played {loudest} for a steady 0.0193"
                );
            }
        }
    }

    #[test]
    fn a_band_moved_while_fxsound_is_off_does_not_replay_the_old_curve_when_it_comes_back_on() {
        // A band at 31.25 Hz, +12 dB, under a tone there at 0.05; FxSound switched off at block 20,
        // the band set to 0 dB while it is off or in the snapshot that switches it back on, and
        // FxSound back on at block 40. The equalizer does not run while FxSound is off, so the
        // crossfade the change started waited for it and played 20 ms of the +12 dB curve at the
        // switch-on: 0.134, where before the glides it played 0.048. A change made while nobody
        // hears the equalizer now lands at once, and the switch-on plays 0.048 again.
        for with_the_switch_on in [false, true] {
            let rendered = render_moving(480, 50, (31.25, 0.05, 1.0), |block| {
                let mut params = DspParams {
                    power: !(20..40).contains(&block),
                    ..DspParams::default()
                };
                params.band_center_hz[0] = 31.25;
                let flat = if with_the_switch_on {
                    block >= 40
                } else {
                    block >= 25
                };
                params.band_boost_db[0] = if flat { 0.0 } else { 12.0 };
                params
            });
            let left = left_channel(&rendered);
            assert!(block_peak(&left, 19) > 0.1, "the fixture was not boosted");
            let loudest = (40..50)
                .map(|block| block_peak(&left, block))
                .fold(0.0_f32, f32::max);
            assert!(
                loudest < 0.05,
                "band flattened {}: the switch-on played {loudest}",
                if with_the_switch_on {
                    "with the switch-on"
                } else {
                    "while off"
                }
            );
        }
    }

    /// The default ten-band snapshot with 62.5 Hz at +6 dB.
    fn ten_bands_with_62_hz_up() -> DspParams {
        let mut params = DspParams::default();
        params.band_boost_db[0] = 6.0;
        params
    }

    /// The 31-band ladder with 63 Hz, its band nearest 62.5 Hz, at +6 dB.
    fn thirty_one_bands_with_63_hz_up() -> DspParams {
        let (centres, _, _) = crate::eq::band_table(31).expect("the 31-band ladder");
        let mut params = DspParams {
            num_bands: 31,
            ..DspParams::default()
        };
        params.band_center_hz[..31].copy_from_slice(centres);
        params.band_boost_db = [0.0; 32];
        params.band_boost_db[5] = 6.0;
        params
    }

    #[test]
    fn a_new_band_count_crossfades_the_whole_curve_instead_of_clicking() {
        // Ten bands with 62.5 Hz at +6 dB, then 31 with 63 Hz at +6 dB — a preset for the other
        // band count, or the user changing it — under a 50 Hz tone at 0.3. Every section used to
        // be cleared, so the old curve went between two samples and the new one started from
        // rest: a step of 0.143 and a click above 300 Hz peaking at −18.4 dBFS. The old ladder now
        // plays out beside the new one for 20 ms: no step larger than the tone's own, 0.0030, and
        // −67.8 dBFS. Back and forth, the ten-band curve remapped to 31 bands at block 20
        // (`remap_band_gains`, as the settings remap it) and ten again at block 21, 10 ms into the
        // first crossfade, the second change moves the new ladder to the newest section by
        // section: 0.444 and −8.4 dBFS before, 0.0044 and −59.6 dBFS now.
        let rendered = render_moving(480, 40, (50.0, 0.3, 1.0), |block| {
            if block >= 20 {
                thirty_one_bands_with_63_hz_up()
            } else {
                ten_bands_with_62_hz_up()
            }
        });
        let left = left_channel(&rendered);
        let steepest = largest_step(&left[4_800..]);
        assert!(
            steepest < own_slope(0.3 * 1.6, 50.0),
            "ten bands to 31 stepped by {steepest}"
        );
        let (_, peak) = above(&left, 300.0, 4_800);
        assert!(peak < -62.0, "ten bands to 31: a click at {peak} dBFS");

        let (centres, _, _) = crate::eq::band_table(31).expect("the 31-band ladder");
        let remapped = {
            let ten = ten_bands_with_62_hz_up();
            let gains = crate::eq::remap_band_gains(ten.bands().1, 31);
            let mut params = DspParams {
                num_bands: 31,
                ..ten
            };
            params.band_center_hz[..31].copy_from_slice(centres);
            params.band_boost_db[..31].copy_from_slice(&gains);
            params
        };
        let rendered = render_moving(480, 40, (50.0, 0.3, 1.0), |block| {
            if block == 20 {
                remapped
            } else {
                ten_bands_with_62_hz_up()
            }
        });
        let left = left_channel(&rendered);
        let steepest = largest_step(&left[4_800..]);
        assert!(steepest < 0.01, "there and back stepped by {steepest}");
        let (_, peak) = above(&left, 300.0, 4_800);
        assert!(peak < -55.0, "there and back: a click at {peak} dBFS");
    }

    #[test]
    fn switching_the_equalizer_off_or_on_fades_instead_of_stepping() {
        // 62.5 Hz at +6 dB under a 50 Hz tone at 0.3, the equalizer switched off at block 20 and
        // back on at block 40. Off, the curve went between two samples, a step of 0.1435 and a
        // click above 300 Hz at −18.4 dBFS; now the block fades out over 20 ms, no step larger
        // than the tone's own and −67.8 dBFS. Back on, it came in from rest already, −51.0 dBFS;
        // the fade takes that to −66.0.
        let mut engine = Engine::new(48_000.0, 480, 2);
        let mut rendered = Vec::new();
        for block in 0..60 {
            let mut params = ten_bands_with_62_hz_up();
            params.eq_on = !(20..40).contains(&block);
            engine.apply(&params);
            let mut buffer = crest_aligned(block * 480, 480, 50.0, 0.3, 1.0);
            engine.process(&mut buffer, 2);
            rendered.extend_from_slice(&buffer);
            if block == 22 {
                // Faded out, the block is out: the filters are switched off.
                assert!(!engine.equalizer().is_enabled());
            }
        }
        let left = left_channel(&rendered);
        let steepest = largest_step(&left[4_800..]);
        assert!(
            steepest < own_slope(0.3 * 1.6, 50.0),
            "the switch stepped by {steepest}"
        );
        let (_, off) = above(&left[..40 * 480], 300.0, 4_800);
        let (_, on) = above(&left, 300.0, 30 * 480);
        assert!(
            off < -62.0 && on < -62.0,
            "a click above 300 Hz at {off} dBFS switching off, {on} dBFS switching on"
        );

        // With the whole block busy — master gain −6 dB, balance +6 dB and Volume Leveling 2
        // beside the curve — under 50 Hz at 0.1, the switch moved the waveform by 0.0418 in one
        // sample; now by no more than the tone's own 0.0006.
        let rendered = render_moving(480, 260, (50.0, 0.1, 1.0), |block| {
            let mut params = ten_bands_with_62_hz_up();
            params.master_gain_db = -6.0;
            params.balance = 6.0;
            params.volume_leveling_db = 2.0;
            params.eq_on = !(220..240).contains(&block);
            params
        });
        let steepest = largest_step(&left_channel(&rendered)[210 * 480..]);
        assert!(
            steepest < own_slope(0.1, 50.0),
            "the busy block stepped by {steepest}"
        );
    }

    /// A xorshift for the stress tests: deterministic, and no dependency.
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / 16_777_216.0
        }

        fn between(&mut self, low: f32, high: f32) -> f32 {
            low + (high - low) * self.next()
        }
    }

    #[test]
    fn parameters_changing_every_block_never_break_the_ceiling() {
        // Every parameter moved at random before every 64-frame block for five seconds — faster
        // than any glide can finish, so every stage is always mid-glide and every design waits its
        // turn — on stereo and on 5.1. Nothing may go non-finite and nothing may leave the
        // chain above Dynamic Boost's ceiling.
        for channels in [2_usize, 6] {
            let mut engine = Engine::new(48_000.0, 64, channels);
            let mut random = Noise(0x9e37_79b9_7f4a_7c15 ^ channels as u64);
            let mut loudest = 0.0_f32;
            for _ in 0..(5 * 48_000 / 64) {
                let mut params = DspParams {
                    master_gain_db: random.between(-20.0, 20.0),
                    balance: random.between(-20.0, 20.0),
                    volume_leveling_db: random.between(0.0, 4.0),
                    filter_q: random.between(1.0, 3.0),
                    eq_on: random.next() > 0.1,
                    ..DspParams::default()
                };
                for band in 0..10 {
                    params.band_boost_db[band] = random.between(-12.0, 12.0);
                }
                for effect in EffectId::ALL {
                    params.set_effect(effect, random.next());
                }
                params.sanitise();
                engine.apply(&params);
                let mut block: Vec<f32> = (0..64 * channels)
                    .map(|_| random.between(-0.7, 0.7))
                    .collect();
                engine.process(&mut block, channels);
                assert!(block.iter().all(|s| s.is_finite()), "{channels} channels");
                loudest = block.iter().fold(loudest, |acc, s| acc.max(s.abs()));
            }
            assert!(
                loudest <= crate::effects::dynamic_boost::MAX_OUTPUT + 1e-6,
                "{channels} channels reached {loudest}"
            );
        }
    }
}
