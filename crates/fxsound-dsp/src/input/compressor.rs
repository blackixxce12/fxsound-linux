//! The compressor: feed-forward, soft knee, on the shared detector.
//!
//! What it is for on a voice is not loudness — the makeup gain after it does that — but evenness.
//! A talker moves ten to twenty decibels between a leaned-in aside and a laugh, and a listener on
//! the other end has one volume control for both. Three-to-one above −18 dB turns twenty decibels
//! of delivery into seven.
//!
//! The topology is the one in Giannoulis, Massberg and Reiss, *Digital Dynamic Range Compressor
//! Design — A Tutorial and Analysis* (JAES 60(6), 2012): detect a level, compute the gain the
//! static curve asks for in decibels, then smooth that gain rather than the level. Smoothing the
//! gain is what makes "20 ms attack" a statement about how fast the *gain* moves, which is what a
//! preset means by it and what a listener hears.
//!
//! The detector is [`Follower`], shared with [`super::gate`], so a preset's threshold says which
//! quantity it is a threshold on. This is not pedantry: peak and RMS against the same number differ
//! by three to seven decibels of gain reduction, which is the difference between Broadcaster's
//! 4.5:1 sounding like radio and sounding crushed.
//!
//! One measured limit, pinned by a test rather than left to be found in a preset: RMS detection
//! averages over ten milliseconds, so an attack shorter than that is the window rather than the
//! attack. Two presets in the set ask for 10 and 15 ms against RMS and are therefore a little
//! slower than they read — consistently, and they were voiced that way.
//!
//! Real-time safe: fixed state, no allocation. It does spend a logarithm and an exponential per
//! sample per channel — affordable here, where a microphone is one or two channels, and the reason
//! this stage is not offered on the output path.

use crate::biquad::{MAX_CHANNELS, Real};
use crate::input::detector::{Detection, Follower, coefficient, db_to_linear, linear_to_db};
use crate::input::sane_rate;

/// `1.0` is a straight wire; past about this the curve is a limiter, and the chain already has one
/// that does the job properly with look-ahead.
const MAX_RATIO: Real = 60.0;
/// Wider than this and the knee reaches below anything a voice preset sets a threshold at.
const MAX_KNEE_DB: Real = 24.0;

pub struct Compressor {
    detector: Follower,
    sample_rate: Real,

    threshold_db: Real,
    ratio: Real,
    /// `1/ratio − 1`, the slope of the curve above the threshold in dB per dB. Negative.
    slope: Real,
    knee_db: Real,

    attack_ms: Real,
    release_ms: Real,
    attack_coeff: Real,
    release_coeff: Real,

    gain: [Real; MAX_CHANNELS],
}

impl std::fmt::Debug for Compressor {
    /// The design, not the per-channel state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compressor")
            .field("sample_rate", &self.sample_rate)
            .field("threshold_db", &self.threshold_db)
            .field("ratio", &self.ratio)
            .field("knee_db", &self.knee_db)
            .field("attack_ms", &self.attack_ms)
            .field("release_ms", &self.release_ms)
            .finish()
    }
}

impl Compressor {
    /// The default is where most of the preset table sits: −18 dB, 3:1, 20 ms and 150 ms, RMS.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let sample_rate = sane_rate(sample_rate);
        let mut compressor = Self {
            detector: Follower::detector(sample_rate, Detection::Rms),
            sample_rate,
            threshold_db: -18.0,
            ratio: 3.0,
            slope: 1.0 / 3.0 - 1.0,
            // Soft, and the same for every preset in the set. A hard knee on a voice announces
            // itself on every syllable that crosses the threshold; six decibels of knee is the
            // usual compromise and is what makes 3:1 read as "steady" rather than as "compressed".
            knee_db: 6.0,
            attack_ms: 20.0,
            release_ms: 150.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            gain: [1.0; MAX_CHANNELS],
        };
        compressor.design();
        compressor
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

    /// The level above which gain starts coming off, in dBFS — of whichever quantity
    /// [`Self::set_detection`] selected.
    pub fn set_threshold_db(&mut self, db: Real) {
        self.threshold_db = if db.is_finite() { db.min(0.0) } else { -18.0 };
    }

    /// Decibels in per decibel out, above the threshold. `1.0` is a straight wire.
    pub fn set_ratio(&mut self, ratio: Real) {
        let ratio = if ratio.is_finite() {
            ratio.clamp(1.0, MAX_RATIO)
        } else {
            3.0
        };
        self.ratio = ratio;
        self.slope = 1.0 / ratio - 1.0;
    }

    /// How many decibels wide the transition into the ratio is, centred on the threshold.
    ///
    /// Not a preset field today — every preset in the set leaves it at the default — but it is a
    /// number the gain computer needs, so it is a number the caller can see rather than one buried
    /// in the arithmetic.
    pub fn set_knee_db(&mut self, db: Real) {
        self.knee_db = if db.is_finite() {
            db.clamp(0.0, MAX_KNEE_DB)
        } else {
            6.0
        };
    }

    /// How fast the gain comes off and how fast it comes back, in milliseconds.
    pub fn set_times(&mut self, attack_ms: Real, release_ms: Real) {
        self.attack_ms = if attack_ms.is_finite() {
            attack_ms.max(0.0)
        } else {
            20.0
        };
        self.release_ms = if release_ms.is_finite() {
            release_ms.max(0.0)
        } else {
            150.0
        };
        self.design();
    }

    /// Which quantity the threshold is a threshold on.
    pub fn set_detection(&mut self, mode: Detection) {
        self.detector.set_mode(mode);
    }

    fn design(&mut self) {
        self.attack_coeff = coefficient(self.attack_ms, self.sample_rate);
        self.release_coeff = coefficient(self.release_ms, self.sample_rate);
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.gain = [1.0; MAX_CHANNELS];
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

    /// The static curve, in decibels: how much gain this level asks for, before any smoothing.
    ///
    /// Separate and pure so it can be read on its own — the curve and the timing are the two halves
    /// of a compressor and confusing them is how "the ratio is wrong" turns out to be "the attack
    /// is long".
    #[must_use]
    pub fn curve_db(&self, level_db: Real) -> Real {
        let over = level_db - self.threshold_db;
        if 2.0 * over <= -self.knee_db {
            0.0
        } else if 2.0 * over.abs() <= self.knee_db {
            // The quadratic that joins unity to the ratio with a continuous first derivative.
            self.slope * (over + self.knee_db / 2.0).powi(2) / (2.0 * self.knee_db)
        } else {
            self.slope * over
        }
    }

    /// One interleaved frame, in place.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [Real]) {
        // Channels past the supported count are left exactly as they arrived, as everywhere else in
        // the chain.
        for (channel, sample) in frame.iter_mut().enumerate().take(MAX_CHANNELS) {
            let level = self.detector.follow(channel, *sample);
            let target = db_to_linear(self.curve_db(linear_to_db(level)));

            let Some(gain) = self.gain.get_mut(channel) else {
                continue;
            };
            // Downwards is the attack: the stage is taking gain away. The asymmetry is the whole
            // instrument — grab the transient, then let go slowly enough that the release is not
            // itself a sound.
            let coeff = if target < *gain {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            *gain += coeff * (target - *gain);

            *sample *= *gain;
        }
    }

    /// A whole interleaved block, in place.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || buffer.is_empty() {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            self.process_frame(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    /// A compressor with a hard knee and instant timing, so a test measures the curve alone.
    fn instant(threshold_db: Real, ratio: Real) -> Compressor {
        let mut compressor = Compressor::new(FS);
        compressor.set_threshold_db(threshold_db);
        compressor.set_ratio(ratio);
        compressor.set_knee_db(0.0);
        compressor.set_times(0.0, 0.0);
        compressor
    }

    /// Alternating full-swing samples, so peak and RMS are the same number and a test can say "ten
    /// decibels over the threshold" and mean it under either detector.
    fn drive(compressor: &mut Compressor, level_db: Real, seconds: Real) -> Real {
        let amplitude = db_to_linear(level_db);
        let frames = (FS * seconds) as usize;
        let mut last = 0.0;
        for n in 0..frames {
            let mut frame = [if n % 2 == 0 { amplitude } else { -amplitude }];
            compressor.process_frame(&mut frame);
            last = frame[0].abs();
        }
        last
    }

    #[test]
    fn below_the_threshold_it_is_a_straight_wire() {
        let mut compressor = instant(-20.0, 4.0);
        let out = drive(&mut compressor, -30.0, 0.3);
        let gap = linear_to_db(out) - (-30.0);
        assert!(gap.abs() < 0.01, "quiet speech was compressed by {gap} dB");
    }

    #[test]
    fn the_ratio_says_how_much_of_the_excess_survives() {
        // Ten decibels over the threshold at 3:1 should leave about three and a third.
        for ratio in [1.0, 2.0, 3.0, 4.0, 10.0] {
            let mut compressor = instant(-30.0, ratio);
            let out = linear_to_db(drive(&mut compressor, -20.0, 0.3));
            let want = -30.0 + 10.0 / ratio;
            assert!(
                (out - want).abs() < 0.1,
                "{ratio}:1 should land at {want} dBFS, got {out}"
            );
        }
    }

    #[test]
    fn twenty_decibels_of_delivery_become_seven() {
        // The purpose, stated as the listener experiences it: the gap between the aside and the
        // laugh, once both are above the threshold.
        let mut compressor = instant(-30.0, 3.0);
        let quiet = linear_to_db(drive(&mut compressor, -20.0, 0.3));
        let loud = linear_to_db(drive(&mut compressor, 0.0, 0.3));
        let gap = loud - quiet;
        assert!(
            (gap - 20.0 / 3.0).abs() < 0.2,
            "twenty decibels in should be under seven out, got {gap}"
        );
    }

    #[test]
    fn the_knee_starts_before_the_threshold_and_a_hard_knee_does_not() {
        let mut soft = instant(-20.0, 3.0);
        soft.set_knee_db(12.0);
        let hard = instant(-20.0, 3.0);

        // Six decibels under — the bottom of a twelve-decibel knee — is untouched by both.
        assert!(soft.curve_db(-26.0).abs() < 1.0e-6);
        assert!(hard.curve_db(-26.0).abs() < 1.0e-6);

        // At the threshold the hard knee has done nothing yet and the soft one is already working.
        assert!(hard.curve_db(-20.0).abs() < 1.0e-6);
        let at_threshold = soft.curve_db(-20.0);
        assert!(
            (at_threshold + 1.0).abs() < 0.05,
            "a 12 dB knee at 3:1 should be 1 dB down at the threshold, got {at_threshold}"
        );

        // And well above it, the two agree again — the knee is a transition, not a different ratio.
        assert!((soft.curve_db(0.0) - hard.curve_db(0.0)).abs() < 1.0e-4);
    }

    #[test]
    fn one_attack_constant_reaches_most_of_the_way_to_the_asked_for_gain() {
        let mut compressor = Compressor::new(FS);
        compressor.set_threshold_db(-20.0);
        compressor.set_ratio(4.0);
        compressor.set_knee_db(0.0);
        compressor.set_times(5.0, 500.0);
        // Peak detection, so the level is a step and the attack coefficient is the only thing
        // being measured. In RMS the window is itself slower than a 5 ms attack — see below.
        compressor.set_detection(Detection::Peak);

        // Full scale against a −20 dB threshold: twenty over, fifteen off, so the target is −15 dB.
        let target = db_to_linear(-15.0);
        drive(&mut compressor, 0.0, 0.005);
        let after = compressor.gain(0);
        let want = 1.0 - 0.632 * (1.0 - target);
        assert!(
            (after - want).abs() < 0.05,
            "one attack constant should be near {want}, got {after}"
        );
    }

    #[test]
    fn an_attack_shorter_than_the_rms_window_is_really_the_window() {
        // A real limit of the design, measured here rather than discovered in a preset. The RMS
        // detector averages over 10 ms, so a level step takes that long to be seen; asking for a
        // 5 ms attack on top of it does not produce a 5 ms attack. Two presets in the set ask for
        // 10 and 15 ms against RMS, which is at or just past the floor — they are slower than they
        // read, and consistently so, which is why the numbers were voiced by ear and kept.
        let settle = |mode| {
            let mut compressor = Compressor::new(FS);
            compressor.set_threshold_db(-20.0);
            compressor.set_ratio(4.0);
            compressor.set_knee_db(0.0);
            compressor.set_times(5.0, 500.0);
            compressor.set_detection(mode);
            drive(&mut compressor, 0.0, 0.005);
            compressor.gain(0)
        };
        let peak = settle(Detection::Peak);
        let rms = settle(Detection::Rms);
        assert!(
            rms > peak + 0.05,
            "the window should visibly hold the attack back: peak {peak}, rms {rms}"
        );
    }

    #[test]
    fn it_grabs_fast_and_lets_go_slowly() {
        let mut compressor = Compressor::new(FS);
        compressor.set_threshold_db(-20.0);
        compressor.set_ratio(4.0);
        compressor.set_knee_db(0.0);
        compressor.set_times(5.0, 400.0);

        drive(&mut compressor, 0.0, 0.2);
        let grabbed = compressor.gain(0);
        assert!(grabbed < 0.25, "it never grabbed: {grabbed}");

        // One attack's worth of silence barely moves it back.
        drive(&mut compressor, -60.0, 0.005);
        let moved = compressor.gain(0) - grabbed;
        assert!(
            moved < 0.05,
            "the release is as fast as the attack, it moved {moved}"
        );

        // A release constant and a half, and it has mostly let go.
        drive(&mut compressor, -60.0, 0.6);
        assert!(
            compressor.gain(0) > 0.75,
            "it never let go: {}",
            compressor.gain(0)
        );
    }

    #[test]
    fn channels_do_not_share_a_compressor() {
        let mut compressor = instant(-30.0, 4.0);
        for n in 0..24_000 {
            let loud = if n % 2 == 0 { 1.0 } else { -1.0 };
            let mut frame = [loud, loud * db_to_linear(-40.0)];
            compressor.process_frame(&mut frame);
        }
        assert!(
            compressor.reduction_db(0) > 20.0,
            "channel 0 was not caught"
        );
        assert!(
            compressor.reduction_db(1).abs() < 0.01,
            "channel 1 was dragged down with it: {}",
            compressor.reduction_db(1)
        );
    }

    #[test]
    fn channels_beyond_the_supported_count_pass_through_untouched() {
        let mut compressor = instant(-40.0, 8.0);
        let mut frame = [0.5; MAX_CHANNELS + 2];
        compressor.process_frame(&mut frame);
        for sample in &frame[MAX_CHANNELS..] {
            assert!(
                (sample - 0.5).abs() < 1.0e-12,
                "an unsupported channel was processed: {sample}"
            );
        }
    }

    #[test]
    fn peak_and_rms_detection_do_not_reach_the_same_reduction() {
        // Why the detector is part of a preset rather than an implementation detail. Sparse spikes
        // over a quiet bed: their peak is far above the threshold and their mean square is not.
        let mut reductions = Vec::new();
        for mode in [Detection::Peak, Detection::Rms] {
            let mut compressor = instant(-20.0, 4.0);
            compressor.set_detection(mode);
            for n in 0..48_000 {
                let mut frame = [if n % 240 == 0 { 0.9 } else { 0.01 }];
                compressor.process_frame(&mut frame);
            }
            // Read it at the spike, which is where the two disagree.
            let mut frame = [0.9];
            compressor.process_frame(&mut frame);
            reductions.push(compressor.reduction_db(0));
        }
        let gap = reductions[0] - reductions[1];
        assert!(
            gap.abs() > 3.0,
            "peak and rms landed within {gap} dB of each other, so the field would be a lie"
        );
    }

    #[test]
    fn a_non_finite_sample_does_not_latch_the_compressor() {
        // As in the gate: the stage carries no guard of its own because the detector clears its
        // recursion, so nothing non-finite reaches the smoothing. This test is what holds that.
        let mut compressor = instant(-20.0, 4.0);
        let mut frame = [Real::INFINITY];
        compressor.process_frame(&mut frame);
        let out = drive(&mut compressor, -30.0, 0.3);
        assert!(
            out.is_finite() && (linear_to_db(out) + 30.0).abs() < 0.1,
            "the compressor is stuck at {out}"
        );
    }
}
