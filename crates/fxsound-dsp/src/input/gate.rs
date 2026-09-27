//! The gate: a downward expander, not a switch.
//!
//! A hard gate is a comparator — open or shut — and the complaint every review of one ends with is
//! the same: it chatters on word onsets and it deletes quiet sentence endings. The preset research
//! settled on 2:1 expansion instead, so fifteen decibels below the threshold costs fifteen
//! decibels of gain rather than everything, and the failure mode a hard gate has cannot occur.
//!
//! The field that makes it shippable is [`Gate::set_range_db`]. An expander with no cap on its
//! attenuation pumps the room floor in and out with every word, which is *more* audible than the
//! floor it was meant to hide; every reference implementation surveyed — LSP, Calf, OBS — caps it,
//! and the parameter set the design started from did not have the field at all. A preset asks for
//! about −14 dB: enough to push the floor under the programme, not enough for its coming and going
//! to be a sound of its own.
//!
//! The detector is the shared [`Follower`], so a preset's threshold says which quantity it is a
//! threshold *on* — the same number against peak and against RMS is three to seven decibels of
//! different behaviour.
//!
//! **The denoiser's voice probability can hold it open** (`vad_gate`). A gate that closes on a
//! quiet consonant the network was sure about is a gate that swallows the ends of words; with the
//! side-chain on, a probability above one half arms the hold timer exactly as an above-threshold
//! level would. It arms the hold and nothing else — the curve still measures the level — so the
//! threshold keeps meaning what it meant.
//!
//! Real-time safe: fixed state, no allocation, no branch on anything but its own numbers. Above
//! the threshold the curve is unity by construction, so the per-sample `powf` is skipped there;
//! a test holds the fast path to the slow one bit for bit.

use crate::biquad::{MAX_CHANNELS, Real};
use crate::input::detector::{Detection, Follower, coefficient, db_to_linear, linear_to_db};
use crate::input::processor::{AudioProcessor, ProcessContext, StageMeter};
use crate::input::sane_rate;
use fxsound_core::messages::InputDspParams;

/// The voice probability above which the side-chain arms the hold.
pub const VAD_OPEN: Real = 0.5;

/// The deepest attenuation a range may ask for. Past this it is a gate with extra steps, and the
/// design is on record that a gate is not what this stage is.
const MIN_RANGE_DB: Real = -90.0;

/// The gentle end of the ratio. `1.0` is a straight wire and is how a preset turns the stage off
/// without the chain having to special-case it.
const MIN_RATIO: Real = 1.0;
/// Past about this the curve is a switch again.
const MAX_RATIO: Real = 20.0;

pub struct Gate {
    detector: Follower,
    sample_rate: Real,

    threshold: Real,
    /// `ratio - 1`, which is the exponent the curve actually uses. Stored designed rather than
    /// recomputed per sample.
    exponent: Real,
    ratio: Real,
    range: Real,
    range_db: Real,

    attack_ms: Real,
    release_ms: Real,
    hold_ms: Real,
    attack_coeff: Real,
    release_coeff: Real,
    hold_frames: u32,

    /// Per-channel smoothed gain, and the frames left on the hold timer.
    gain: [Real; MAX_CHANNELS],
    hold_left: [u32; MAX_CHANNELS],

    enabled: bool,
    vad_gate: bool,
}

impl std::fmt::Debug for Gate {
    /// The design, not the per-channel state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("sample_rate", &self.sample_rate)
            .field("threshold_db", &linear_to_db(self.threshold))
            .field("ratio", &self.ratio)
            .field("range_db", &self.range_db)
            .field("attack_ms", &self.attack_ms)
            .field("hold_ms", &self.hold_ms)
            .field("release_ms", &self.release_ms)
            .field("enabled", &self.enabled)
            .field("vad_gate", &self.vad_gate)
            .finish()
    }
}

impl Gate {
    /// The default is the one the preset table starts from: −45 dB, 2:1, −14 dB of range.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let sample_rate = sane_rate(sample_rate);
        let mut gate = Self {
            detector: Follower::detector(sample_rate, Detection::Rms),
            sample_rate,
            threshold: db_to_linear(-45.0),
            exponent: 1.0,
            ratio: 2.0,
            range: db_to_linear(-14.0),
            range_db: -14.0,
            // Fast enough that a word onset arrives with the gate already open — the classic
            // complaint about a gate is a clipped first consonant, and at 2:1 there is no reason to
            // be slow about it. Hold and release are OBS's defaults, which is where most of the
            // community chains this set was surveyed against get theirs.
            attack_ms: 5.0,
            release_ms: 150.0,
            hold_ms: 200.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            hold_frames: 0,
            gain: [1.0; MAX_CHANNELS],
            hold_left: [0; MAX_CHANNELS],
            enabled: true,
            vad_gate: false,
        };
        gate.design();
        gate
    }

    /// Switch the stage in or out. A transition resets it: coming back half-closed would be
    /// audible on the first word.
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            self.reset();
        }
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Let the voice probability handed to [`Gate::process_block`] arm the hold.
    pub fn set_vad_gate(&mut self, on: bool) {
        self.vad_gate = on;
    }

    #[must_use]
    pub const fn vad_gate(&self) -> bool {
        self.vad_gate
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sane_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.detector.set_sample_rate(sample_rate);
        self.design();
        self.reset();
    }

    /// The level below which the stage starts taking gain away, in dBFS.
    pub fn set_threshold_db(&mut self, db: Real) {
        let db = if db.is_finite() { db.min(0.0) } else { -45.0 };
        self.threshold = db_to_linear(db);
    }

    /// Decibels of attenuation per decibel below the threshold, plus one. `2.0` means fifteen
    /// decibels under costs fifteen; `1.0` is a straight wire.
    pub fn set_ratio(&mut self, ratio: Real) {
        let ratio = if ratio.is_finite() {
            ratio.clamp(MIN_RATIO, MAX_RATIO)
        } else {
            2.0
        };
        self.ratio = ratio;
        self.exponent = ratio - 1.0;
    }

    /// The cap on attenuation, in dB. Negative. Zero turns the stage into a wire.
    ///
    /// This is the field the design was missing. Without it the floor is pumped in and out at the
    /// rate of speech, which is more audible than the floor.
    pub fn set_range_db(&mut self, db: Real) {
        let db = if db.is_finite() {
            db.clamp(MIN_RANGE_DB, 0.0)
        } else {
            -14.0
        };
        self.range_db = db;
        self.range = db_to_linear(db);
    }

    /// How fast the gate opens and how slowly it closes, in milliseconds. These are times on the
    /// *gain*; the detector's own reaction is fixed.
    pub fn set_times(&mut self, attack_ms: Real, release_ms: Real) {
        self.attack_ms = if attack_ms.is_finite() {
            attack_ms.max(0.0)
        } else {
            5.0
        };
        self.release_ms = if release_ms.is_finite() {
            release_ms.max(0.0)
        } else {
            150.0
        };
        self.design();
    }

    /// How long the gate stays open after the signal was last above the threshold.
    ///
    /// This is what carries a voice across the gap between two words without the floor breathing
    /// in between, and it is why the release can be long without swallowing the next syllable.
    pub fn set_hold_ms(&mut self, ms: Real) {
        self.hold_ms = if ms.is_finite() { ms.max(0.0) } else { 200.0 };
        self.design();
    }

    /// Which quantity the threshold is a threshold on.
    pub fn set_detection(&mut self, mode: Detection) {
        self.detector.set_mode(mode);
    }

    fn design(&mut self) {
        self.attack_coeff = coefficient(self.attack_ms, self.sample_rate);
        self.release_coeff = coefficient(self.release_ms, self.sample_rate);
        self.hold_frames = (self.hold_ms / 1000.0 * self.sample_rate) as u32;
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        // Open, not shut: a chain that has just been reset should pass the first word, not expand
        // it while it works out that there is one.
        self.gain = [1.0; MAX_CHANNELS];
        self.hold_left = [0; MAX_CHANNELS];
    }

    /// The gain one channel is currently applying, as a linear amplitude.
    #[must_use]
    pub fn gain(&self, channel: usize) -> Real {
        self.gain.get(channel).copied().unwrap_or(1.0)
    }

    /// What a gain-reduction meter shows: decibels being taken away, as a positive number.
    #[must_use]
    pub fn reduction_db(&self, channel: usize) -> Real {
        -linear_to_db(self.gain(channel))
    }

    /// One interleaved frame, in place, with no voice probability to go on.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [Real]) {
        self.process_frame_with_vad(frame, 0.0);
    }

    /// One interleaved frame, in place. `vad` is the denoiser's voice probability for this
    /// frame, read only when the side-chain is on.
    #[inline]
    pub fn process_frame_with_vad(&mut self, frame: &mut [Real], vad: Real) {
        let voiced = self.vad_gate && vad > VAD_OPEN;
        // Channels past the supported count are left exactly as they arrived — the same contract
        // the limiter keeps, and the reason a nine-channel device is quiet rather than wrong. The
        // bound is enforced twice over: here, so the detector is not driven for a channel it has no
        // state for, and again by the lookups below, which is what the borrow checker wants anyway.
        for (channel, sample) in frame.iter_mut().enumerate().take(MAX_CHANNELS) {
            let level = self.detector.follow(channel, *sample);
            let above = level >= self.threshold;

            // The expander curve, in the linear domain. In decibels it reads
            //   gain_dB = clamp((level_dB − threshold_dB) · (ratio − 1), range_dB, 0)
            // and `(level/threshold)^(ratio−1)` is the same number with one transcendental instead
            // of two. Above the threshold the base is at least one, its power at least one, and
            // the clamp returns exactly unity — so the power is not computed there at all, which
            // is most of the time on a live microphone.
            let target = if above {
                1.0
            } else {
                (level / self.threshold)
                    .powf(self.exponent)
                    .clamp(self.range, 1.0)
            };

            let Some(gain) = self.gain.get_mut(channel) else {
                continue;
            };
            let Some(hold_left) = self.hold_left.get_mut(channel) else {
                continue;
            };

            if above || voiced {
                // Above the threshold is what arms the hold — or the network's word that this is
                // a voice, when a preset lets it count. A signal that is merely *less* expanded
                // than it was has not said anything worth holding open for.
                *hold_left = self.hold_frames;
            }

            if target > *gain {
                *gain += self.attack_coeff * (target - *gain);
            } else if *hold_left > 0 {
                *hold_left -= 1;
            } else {
                *gain += self.release_coeff * (target - *gain);
            }
            *sample *= *gain;
        }
    }

    /// A whole interleaved block, in place, with no voice probability to go on.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        self.process_block(buffer, channels, 0.0);
    }

    /// A whole interleaved block, in place. `vad` holds for the whole block: the denoiser
    /// reports one probability per ten-millisecond frame, and the block is the engine's.
    pub fn process_block(&mut self, buffer: &mut [Real], channels: usize, vad: Real) {
        if channels == 0 || buffer.is_empty() {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            self.process_frame_with_vad(frame, vad);
        }
    }
}

impl AudioProcessor for Gate {
    fn prepare(&mut self, sample_rate: Real) {
        self.set_sample_rate(sample_rate);
    }

    fn apply(&mut self, params: &InputDspParams) {
        self.set_enabled(params.gate_on);
        self.set_threshold_db(params.gate_threshold_db);
        self.set_ratio(params.gate_ratio);
        self.set_range_db(params.gate_range_db);
        self.set_times(params.gate_attack_ms, params.gate_release_ms);
        self.set_hold_ms(params.gate_hold_ms);
        self.set_detection(params.gate_detection);
        self.set_vad_gate(params.vad_gate);
    }

    fn reset(&mut self) {
        Gate::reset(self);
    }

    fn is_active(&self) -> bool {
        self.enabled
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        if self.enabled {
            self.process_block(buffer, ctx.channels, ctx.voice_probability);
        }
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: self.reduction_db(0),
            running: self.enabled,
            aux: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    /// A gate with the times wound right down, so a test measures the curve rather than the
    /// smoothing that follows it.
    fn instant(threshold_db: Real, ratio: Real, range_db: Real) -> Gate {
        let mut gate = Gate::new(FS);
        gate.set_threshold_db(threshold_db);
        gate.set_ratio(ratio);
        gate.set_range_db(range_db);
        gate.set_times(0.0, 0.0);
        gate.set_hold_ms(0.0);
        gate
    }

    /// Run a steady signal of a given amplitude through and report the settled gain.
    ///
    /// The signal alternates sign at every sample, so its peak and its RMS are the same number and
    /// a test can say "ten decibels below the threshold" and mean it under either detector. A sine
    /// would not: its RMS sits 3 dB under its amplitude, which is a fact about crest factor rather
    /// than about the curve being measured — and the first draft of the ratio test below failed on
    /// exactly that.
    fn settled_gain(gate: &mut Gate, amplitude: Real, seconds: Real) -> Real {
        let frames = (FS * seconds) as usize;
        for n in 0..frames {
            let mut frame = [if n % 2 == 0 { amplitude } else { -amplitude }];
            gate.process_frame(&mut frame);
        }
        gate.gain(0)
    }

    #[test]
    fn a_voice_above_the_threshold_comes_out_exactly_as_it_went_in() {
        let mut gate = instant(-45.0, 2.0, -14.0);
        let gain = settled_gain(&mut gate, db_to_linear(-20.0), 0.5);
        assert!(
            (gain - 1.0).abs() < 1.0e-4,
            "the gate is working on the programme: {gain}"
        );
    }

    #[test]
    fn the_floor_is_pushed_down_but_never_further_than_the_range() {
        // Fifteen decibels below the threshold at 2:1 asks for fifteen decibels of attenuation, and
        // the range says fourteen. The cap is the whole point of the field.
        let mut gate = instant(-45.0, 2.0, -14.0);
        let gain = settled_gain(&mut gate, db_to_linear(-60.0), 0.5);
        let reduction = -linear_to_db(gain);
        assert!(
            (reduction - 14.0).abs() < 0.2,
            "expected the range to cap this at 14 dB, got {reduction}"
        );

        // And deeper silence cannot go further, which is what stops the floor breathing.
        let mut gate = instant(-45.0, 2.0, -14.0);
        let gain = settled_gain(&mut gate, db_to_linear(-90.0), 0.5);
        let reduction = -linear_to_db(gain);
        assert!(
            reduction <= 14.0 + 0.5,
            "the range was exceeded at -90 dB: {reduction}"
        );
    }

    #[test]
    fn the_ratio_says_how_many_decibels_a_decibel_below_the_threshold_costs() {
        // Ten decibels under, with the range wide open so only the curve is being measured.
        for (ratio, want) in [(1.0, 0.0), (2.0, 10.0), (3.0, 20.0), (4.0, 30.0)] {
            let mut gate = instant(-30.0, ratio, -60.0);
            let gain = settled_gain(&mut gate, db_to_linear(-40.0), 0.5);
            let reduction = -linear_to_db(gain);
            assert!(
                (reduction - want).abs() < 0.2,
                "{ratio}:1 ten decibels under should cost {want} dB, got {reduction}"
            );
        }
    }

    #[test]
    fn a_range_of_zero_is_a_straight_wire() {
        // How a preset ships the stage switched off — Flat and Studio both do — without the chain
        // needing a special case for it.
        let mut gate = instant(-20.0, 4.0, 0.0);
        let mut peak: Real = 0.0;
        for n in 0..4_800 {
            let x = (n as Real * 0.01).sin() * 1.0e-4;
            let mut frame = [x];
            gate.process_frame(&mut frame);
            peak = peak.max((frame[0] - x).abs());
        }
        assert!(peak < 1.0e-9, "a zero range still moved the signal: {peak}");
    }

    #[test]
    fn hold_carries_the_gate_across_the_gap_between_two_words() {
        let mut gate = Gate::new(FS);
        gate.set_threshold_db(-45.0);
        gate.set_range_db(-14.0);
        gate.set_times(1.0, 20.0);
        gate.set_hold_ms(200.0);

        // A word, then eighty milliseconds of room.
        settled_gain(&mut gate, db_to_linear(-20.0), 0.2);
        for _ in 0..(FS * 0.08) as usize {
            let mut frame = [0.0];
            gate.process_frame(&mut frame);
        }
        let held = gate.gain(0);
        assert!(
            held > 0.99,
            "the hold did not carry the gap, gain fell to {held}"
        );

        // Past the hold, with a release this short, it closes to the range.
        for _ in 0..(FS * 0.4) as usize {
            let mut frame = [0.0];
            gate.process_frame(&mut frame);
        }
        let closed = gate.gain(0);
        assert!(
            (closed - db_to_linear(-14.0)).abs() < 0.02,
            "past the hold it should reach the range, got {closed}"
        );
    }

    #[test]
    fn it_opens_faster_than_it_closes() {
        let mut gate = Gate::new(FS);
        gate.set_threshold_db(-45.0);
        gate.set_range_db(-20.0);
        gate.set_times(5.0, 150.0);
        gate.set_hold_ms(0.0);

        // Shut it on silence first.
        for _ in 0..(FS * 1.0) as usize {
            let mut frame = [0.0];
            gate.process_frame(&mut frame);
        }
        assert!(gate.gain(0) < db_to_linear(-19.0) * 1.05);

        // One attack constant of programme should be most of the way open.
        settled_gain(&mut gate, db_to_linear(-20.0), 0.005);
        let opened = gate.gain(0);
        assert!(opened > 0.6, "the attack was too slow: {opened}");

        // One release constant of silence is nowhere near as far along.
        let before = gate.gain(0);
        for _ in 0..(FS * 0.005) as usize {
            let mut frame = [0.0];
            gate.process_frame(&mut frame);
        }
        let closed = before - gate.gain(0);
        assert!(
            closed < (opened - db_to_linear(-20.0)) * 0.2,
            "release should be far slower than attack, it moved {closed}"
        );
    }

    #[test]
    fn channels_do_not_share_a_gate() {
        let mut gate = instant(-45.0, 2.0, -14.0);
        for n in 0..24_000 {
            let x = (n as Real * 0.05).sin() * db_to_linear(-20.0);
            let mut frame = [x, 0.0];
            gate.process_frame(&mut frame);
        }
        assert!(gate.gain(0) > 0.99, "channel 0: {}", gate.gain(0));
        assert!(
            (gate.gain(1) - db_to_linear(-14.0)).abs() < 0.01,
            "channel 1: {}",
            gate.gain(1)
        );
    }

    #[test]
    fn channels_beyond_the_supported_count_pass_through_untouched() {
        let mut gate = instant(-6.0, 4.0, -40.0);
        let mut frame = [1.0e-5; MAX_CHANNELS + 2];
        gate.process_frame(&mut frame);
        for sample in &frame[MAX_CHANNELS..] {
            assert!(
                (sample - 1.0e-5).abs() < 1.0e-12,
                "an unsupported channel was processed: {sample}"
            );
        }
    }

    #[test]
    fn peak_and_rms_detection_do_not_reach_the_same_gain() {
        // The reason the detector is a preset field rather than an implementation detail: the same
        // threshold against a crest-heavy signal means two different amounts of expansion.
        let mut spiky = Vec::with_capacity(4_800);
        for n in 0..48_000 {
            spiky.push(if n % 480 == 0 { 0.02 } else { 0.0005 });
        }

        let mut gains = Vec::new();
        for mode in [Detection::Peak, Detection::Rms] {
            let mut gate = instant(-40.0, 2.0, -30.0);
            gate.set_detection(mode);
            for &x in &spiky {
                let mut frame = [x];
                gate.process_frame(&mut frame);
            }
            gains.push(gate.gain(0));
        }
        let gap = linear_to_db(gains[0]) - linear_to_db(gains[1]);
        assert!(
            gap.abs() > 3.0,
            "peak and rms landed within {gap} dB of each other, so the field would be a lie"
        );
    }

    #[test]
    fn a_confident_voice_holds_the_gate_open_through_a_pause() {
        // The side-chain: a word, then a −60 dB pause the gate would close on, during which the
        // network keeps saying "voice". With `vad_gate` on the hold is re-armed every frame and
        // the gate never starts to release; with it off, the same pause closes it.
        let closed_after = |vad_gate: bool| {
            let mut gate = Gate::new(FS);
            gate.set_threshold_db(-45.0);
            gate.set_range_db(-14.0);
            gate.set_times(1.0, 20.0);
            gate.set_hold_ms(50.0);
            gate.set_vad_gate(vad_gate);
            settled_gain(&mut gate, db_to_linear(-20.0), 0.2);
            let pause = db_to_linear(-60.0);
            for n in 0..(FS * 0.5) as usize {
                let mut frame = [if n % 2 == 0 { pause } else { -pause }];
                gate.process_frame_with_vad(&mut frame, 0.95);
            }
            gate.gain(0)
        };
        let held = closed_after(true);
        assert!(
            held > 0.99,
            "the network's word did not hold the gate: {held}"
        );
        let closed = closed_after(false);
        assert!(
            closed < db_to_linear(-13.0),
            "without the side-chain the pause should close it: {closed}"
        );
    }

    #[test]
    fn a_probability_that_is_not_a_voice_does_not_arm_the_hold() {
        let mut gate = Gate::new(FS);
        gate.set_threshold_db(-45.0);
        gate.set_range_db(-14.0);
        gate.set_times(1.0, 20.0);
        gate.set_hold_ms(50.0);
        gate.set_vad_gate(true);
        settled_gain(&mut gate, db_to_linear(-20.0), 0.2);
        for _ in 0..(FS * 0.5) as usize {
            let mut frame = [0.0];
            gate.process_frame_with_vad(&mut frame, 0.3);
        }
        assert!(
            gate.gain(0) < db_to_linear(-13.0),
            "a probability under one half held the gate: {}",
            gate.gain(0)
        );
    }

    #[test]
    fn the_fast_path_above_the_threshold_is_the_slow_path_bit_for_bit() {
        // Above the threshold the curve is unity by construction; the fast path skips the
        // `powf` and has to land on exactly the number the full expression would.
        let gate = instant(-30.0, 3.0, -40.0);
        for level_db in [-29.9_f32, -20.0, -10.0, -3.0, 0.0] {
            let level = db_to_linear(level_db);
            let slow: Real = (level / gate.threshold)
                .powf(gate.exponent)
                .clamp(gate.range, 1.0);
            assert_eq!(
                slow.to_bits(),
                1.0_f32.to_bits(),
                "at {level_db} dB: {slow}"
            );
        }
        // And exactly at the threshold, where the branch flips.
        let slow: Real = (gate.threshold / gate.threshold)
            .powf(gate.exponent)
            .clamp(gate.range, 1.0);
        assert_eq!(slow.to_bits(), 1.0_f32.to_bits());
    }

    #[test]
    fn switched_off_it_passes_the_signal_through_and_says_so() {
        let mut gate = instant(-20.0, 4.0, -40.0);
        gate.set_enabled(false);
        assert!(!gate.is_enabled());
        assert!(!AudioProcessor::is_active(&gate));
        let ctx = ProcessContext {
            sample_rate: FS,
            channels: 1,
            voice_probability: 0.0,
        };
        let input = vec![1.0e-4; 4_800];
        let mut block = input.clone();
        AudioProcessor::process(&mut gate, &mut block, &ctx);
        assert_eq!(block, input);
        gate.set_enabled(true);
        AudioProcessor::process(&mut gate, &mut block, &ctx);
        assert!(block[4_000] < 1.0e-4, "switched back on it expands again");
    }

    #[test]
    fn a_non_finite_sample_does_not_latch_the_gate() {
        // There is no guard for this inside the stage, and that is deliberate: the detector clears
        // its own recursion, so every number reaching the smoothing below is already finite, and a
        // branch that cannot be taken is worse than no branch. An `assert!` in its place fired on
        // none of these tests. This test is what holds that invariant — if the detector's guard
        // ever goes, the gate finds out here rather than in someone's microphone.
        let mut gate = instant(-45.0, 2.0, -14.0);
        let mut frame = [Real::INFINITY];
        gate.process_frame(&mut frame);
        let gain = settled_gain(&mut gate, db_to_linear(-20.0), 0.5);
        assert!(
            gain.is_finite() && (gain - 1.0).abs() < 1.0e-3,
            "the gate is stuck at {gain}"
        );
    }
}
