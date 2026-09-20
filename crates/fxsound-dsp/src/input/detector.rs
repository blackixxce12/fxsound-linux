//! The level detector every dynamics stage in the voice chain shares.
//!
//! Splitting this out settles a question the preset review raised and could not answer from the
//! numbers alone: **peak and RMS detection of the same signal against the same threshold differ by
//! three to seven decibels of gain reduction**, so a preset that says "threshold −18 dB, ratio 3:1"
//! means two different things depending on which one is running. Making the detector part of the
//! parameter set rather than an implementation detail is what makes a preset unambiguous.
//!
//! Both modes are one-pole followers with separate attack and release coefficients, which is what
//! every reference implementation surveyed does — LSP, Calf and OBS all follow the same shape.

use crate::biquad::Real;

/// Which quantity the threshold is compared against.
///
/// Defined in [`fxsound_core`] rather than here, and re-exported, because it is part of what a
/// preset *says* and therefore has to be spellable by everything that reads or writes one — the
/// settings file, the parameter snapshot and the UI — not only by the stage that acts on it.
pub use fxsound_core::Detection;

/// A one-pole envelope follower with separate attack and release times.
#[derive(Clone, Copy, Debug)]
pub struct Follower {
    mode: Detection,
    sample_rate: Real,
    attack_ms: Real,
    release_ms: Real,
    attack_coeff: Real,
    release_coeff: Real,
    /// The RMS window, as a one-pole coefficient. Fixed rather than exposed: a preset that could
    /// move it would change what its own threshold means.
    mean_square_coeff: Real,
    /// Per-channel state: the running mean square, and the envelope itself.
    mean_square: [Real; crate::biquad::MAX_CHANNELS],
    envelope: [Real; crate::biquad::MAX_CHANNELS],
}

/// The RMS window, in milliseconds. 10 ms is about one syllable's onset and is the value LSP's
/// compressor and the WebRTC level estimator both settle on.
const RMS_WINDOW_MS: Real = 10.0;

/// Keeps the recursions out of denormals, the way the rest of the crate does.
const BIAS: Real = 1.0e-24;

/// How fast a dynamics stage's detector lets go, in milliseconds.
///
/// Not a preset field, and fixed for the same reason the RMS window is: it is not the stage's
/// release, it is how long the detector remembers a sample. Peak detection without it rectifies to
/// zero between the peaks of any periodic signal, which would leave the two modes differing in how
/// sticky they are on top of differing in what they measure. Matching the RMS window leaves exactly
/// one difference between them, which is the one a preset is choosing.
const DETECTOR_RELEASE_MS: Real = 10.0;

impl Follower {
    #[must_use]
    pub fn new(sample_rate: Real, mode: Detection, attack_ms: Real, release_ms: Real) -> Self {
        let mut follower = Self {
            mode,
            sample_rate: sample_rate.max(1.0),
            attack_ms,
            release_ms,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            mean_square_coeff: 0.0,
            mean_square: [0.0; crate::biquad::MAX_CHANNELS],
            envelope: [0.0; crate::biquad::MAX_CHANNELS],
        };
        follower.design();
        follower
    }

    /// The flavour the gate and the compressor both want: instant on the way up, so a transient is
    /// seen on the sample it arrives, and a short fixed memory on the way down. Their own attack
    /// and release times live on the gain, not here — two sets of time constants in one stage is
    /// two numbers for one audible behaviour.
    #[must_use]
    pub fn detector(sample_rate: Real, mode: Detection) -> Self {
        Self::new(sample_rate, mode, 0.0, DETECTOR_RELEASE_MS)
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        if sample_rate != self.sample_rate {
            self.sample_rate = sample_rate;
            self.design();
            self.reset();
        }
    }

    pub fn set_mode(&mut self, mode: Detection) {
        if mode != self.mode {
            self.mode = mode;
            self.reset();
        }
    }

    pub fn set_times(&mut self, attack_ms: Real, release_ms: Real) {
        let attack_ms = if attack_ms.is_finite() {
            attack_ms.max(0.0)
        } else {
            5.0
        };
        let release_ms = if release_ms.is_finite() {
            release_ms.max(0.0)
        } else {
            80.0
        };
        if attack_ms != self.attack_ms || release_ms != self.release_ms {
            self.attack_ms = attack_ms;
            self.release_ms = release_ms;
            self.design();
        }
    }

    fn design(&mut self) {
        self.attack_coeff = coefficient(self.attack_ms, self.sample_rate);
        self.release_coeff = coefficient(self.release_ms, self.sample_rate);
        self.mean_square_coeff = coefficient(RMS_WINDOW_MS, self.sample_rate);
    }

    pub fn reset(&mut self) {
        self.mean_square = [0.0; crate::biquad::MAX_CHANNELS];
        self.envelope = [0.0; crate::biquad::MAX_CHANNELS];
    }

    /// The current envelope for one channel, as a linear amplitude.
    #[inline]
    #[must_use]
    pub fn envelope(&self, channel: usize) -> Real {
        self.envelope.get(channel).copied().unwrap_or(0.0)
    }

    /// Feed one sample and read the envelope back.
    ///
    /// Rises with the attack coefficient and falls with the release one, which is the asymmetry
    /// that makes a compressor grab quickly and let go slowly.
    #[inline]
    pub fn follow(&mut self, channel: usize, x: Real) -> Real {
        let Some(envelope) = self.envelope.get_mut(channel) else {
            return 0.0;
        };

        let level = match self.mode {
            Detection::Peak => x.abs(),
            Detection::Rms => {
                let Some(mean_square) = self.mean_square.get_mut(channel) else {
                    return 0.0;
                };
                *mean_square += self.mean_square_coeff * (x * x - *mean_square) + BIAS;
                // The mean square is its own recursion and latches independently of the envelope
                // below, so it needs its own escape: a single non-finite sample would otherwise
                // keep feeding infinity into a follower that resets itself every sample and
                // therefore never recovers.
                if !mean_square.is_finite() {
                    *mean_square = 0.0;
                }
                mean_square.max(0.0).sqrt()
            }
        };

        let coeff = if level > *envelope {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        *envelope += coeff * (level - *envelope) + BIAS;
        // A recursion with nowhere to go is a recursion that stays wrong for the session; the
        // engine sanitises its input, so this guards only what this stage can produce itself.
        if !envelope.is_finite() {
            *envelope = 0.0;
        }
        *envelope
    }
}

/// `1 − exp(−1 / (τ·fs))`, the usual one-pole step. A zero time gives an instant follower rather
/// than a division by zero.
///
/// Shared with the stages that smooth a *gain* with the same shape they detect a level with, so
/// that "5 ms attack" is one number with one meaning across the chain.
pub(super) fn coefficient(time_ms: Real, sample_rate: Real) -> Real {
    if time_ms <= 0.0 {
        return 1.0;
    }
    let tau = time_ms / 1000.0;
    1.0 - (-1.0 / (tau * sample_rate)).exp()
}

/// Linear amplitude to decibels, with a floor so silence is a number rather than `-inf`.
#[inline]
#[must_use]
pub fn linear_to_db(x: Real) -> Real {
    20.0 * x.max(1.0e-9).log10()
}

/// Decibels to linear amplitude.
#[inline]
#[must_use]
pub fn db_to_linear(db: Real) -> Real {
    10.0_f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    #[test]
    fn peak_and_rms_disagree_by_the_crest_factor_which_is_why_a_preset_must_say_which() {
        // A sine's RMS is 1/√2 of its peak — 3 dB down. On speech the gap is far larger, which is
        // the whole reason this had to become a parameter rather than a convention.
        let mut peak = Follower::new(FS, Detection::Peak, 1.0, 200.0);
        let mut rms = Follower::new(FS, Detection::Rms, 1.0, 200.0);
        let (mut p, mut r) = (0.0, 0.0);
        for n in 0..48_000 {
            let x = (n as Real * std::f32::consts::TAU * 200.0 / FS).sin();
            p = peak.follow(0, x);
            r = rms.follow(0, x);
        }
        // A one-pole follower releases a little between the peaks of a 200 Hz sine.
        assert!((p - 1.0).abs() < 0.05, "peak should track the peak: {p}");
        assert!(
            (r - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.05,
            "rms should track the rms: {r}"
        );
        let gap = linear_to_db(p) - linear_to_db(r);
        assert!((gap - 3.0).abs() < 0.5, "the gap should be 3 dB, got {gap}");
    }

    #[test]
    fn it_rises_on_the_attack_and_falls_on_the_release() {
        let mut f = Follower::new(FS, Detection::Peak, 5.0, 100.0);
        // One time constant of attack should reach about 63% of the target.
        let attack_frames = (FS * 0.005) as usize;
        for _ in 0..attack_frames {
            f.follow(0, 1.0);
        }
        let after_attack = f.envelope(0);
        assert!(
            (0.55..0.72).contains(&after_attack),
            "one attack constant should be near 63%, got {after_attack}"
        );

        // And release is slower: the same count of silent frames barely moves it.
        for _ in 0..attack_frames {
            f.follow(0, 0.0);
        }
        assert!(
            f.envelope(0) > after_attack * 0.9,
            "release should be much slower than attack, got {}",
            f.envelope(0)
        );
    }

    #[test]
    fn channels_do_not_share_an_envelope() {
        let mut f = Follower::new(FS, Detection::Peak, 1.0, 100.0);
        for _ in 0..4_000 {
            f.follow(0, 1.0);
            f.follow(1, 0.0);
        }
        assert!(f.envelope(0) > 0.9);
        assert!(f.envelope(1) < 0.01);
    }

    #[test]
    fn a_non_finite_sample_does_not_latch_the_follower() {
        let mut f = Follower::new(FS, Detection::Rms, 1.0, 50.0);
        f.follow(0, Real::INFINITY);
        for _ in 0..48_000 {
            f.follow(0, 0.1);
        }
        assert!(
            f.envelope(0).is_finite() && (f.envelope(0) - 0.1).abs() < 0.02,
            "the follower is stuck at {}",
            f.envelope(0)
        );
    }
}
