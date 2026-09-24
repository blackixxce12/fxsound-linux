//! Surround — a mid/side gain widener.
//!
//! Ports `dsp/ptechDsp/wide/Wide32/Wide32.c:214-250`, which theremino rewrote for FxSound 2.0.3.
//! The whole algorithm is four multiplies per frame:
//!
//! ```text
//! M  = (L + R) / 2
//! M' = M · (1 − 0.3·i)
//! L' = M' + (1 + 3·i)·(L − M)
//! R' = M' + (1 + 3·i)·(R − M)
//! ```
//!
//! It is zero-latency and allocation-free, and has no memory of the signal: the only state is the
//! two gains' glide to a new amount over [`crate::smooth::GLIDE_SECONDS`] (audit report #11),
//! where the original steps them between two samples. At unity gains the transform passes its
//! input, so the stage fades in from, and out to, the bypass.
//!
//! The pre-2025 algorithm — a Haas/dispersion widener with high-passed side channels and delay
//! lines — is still in the tree at `wide/Wide16/Wide16.c` but is unreachable in the 32-bit build,
//! so it is not ported.
//!
//! At the maximum setting a signal panned hard to one channel reaches `0.79·0.5 + 3.1·0.5 = 1.945`,
//! i.e. nearly 6 dB of headroom given away. That never audibly clips only because Dynamic Boost
//! runs after it and catches the overshoot.

use super::Effect;
use crate::biquad::Real;
use crate::smooth::{Ramp, glide_frames};

/// `DSP_WID_INTENSITY_MAX_VALUE · PLY_WIDENER_BOOST_MAX_SCALE = 1.0 · 0.7`
/// (`c_wid.h:30`, `c_play.h:93`).
const INTENSITY_MAX: Real = 0.7;
/// `Wide32.c:214` — how fast the side gain grows with intensity.
const SIDE_SLOPE: Real = 3.0;
/// `Wide32.c:215` — the matching mid attenuation.
const MID_SLOPE: Real = 0.3;

/// The stereo widener.
#[derive(Clone, Copy, Debug)]
pub struct Surround {
    amount: Real,
    intensity: Real,
    side_gain: Ramp,
    mid_gain: Ramp,
    /// Glide length at the stream's rate.
    glide: u32,
    /// Which channels the single stereo instance runs over; `None` means the first two.
    front_pair: Option<(usize, usize)>,
}

impl Surround {
    /// Which channels are the front pair. `None` falls back to the first two.
    pub fn set_front_pair(&mut self, pair: Option<(usize, usize)>) {
        self.front_pair = pair;
    }

    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let mut effect = Self {
            amount: 0.0,
            intensity: 0.0,
            side_gain: Ramp::new(1.0),
            mid_gain: Ramp::new(1.0),
            glide: glide_frames(sample_rate),
            front_pair: None,
        };
        effect.set_amount(0.0);
        effect
    }

    /// The widener's internal intensity, `0.0..=0.7`.
    #[must_use]
    pub const fn intensity(&self) -> Real {
        self.intensity
    }

    /// Gain applied to the side signal, `1.0..=3.1` — where a glide is heading, when one runs.
    #[must_use]
    pub const fn side_gain(&self) -> Real {
        self.side_gain.target()
    }

    /// Gain applied to the mid signal, `1.0..=0.79` — where a glide is heading, when one runs.
    #[must_use]
    pub const fn mid_gain(&self) -> Real {
        self.mid_gain.target()
    }

    fn is_gliding(&self) -> bool {
        self.side_gain.is_gliding() || self.mid_gain.is_gliding()
    }
}

impl Effect for Surround {
    fn set_sample_rate(&mut self, sample_rate: Real) {
        // Nothing but the glide's length depends on the sample rate.
        self.glide = glide_frames(sample_rate);
        self.reset();
    }

    fn set_amount(&mut self, amount: Real) {
        self.amount = amount.clamp(0.0, 1.0);
        // Linear in MIDI, with no music-mode warping (`dfxpComm.cpp:786-818`).
        self.intensity = INTENSITY_MAX * self.amount;
        self.side_gain
            .glide_to(1.0 + SIDE_SLOPE * self.intensity, self.glide);
        self.mid_gain
            .glide_to(1.0 - MID_SLOPE * self.intensity, self.glide);
    }

    fn amount(&self) -> Real {
        self.amount
    }

    /// Switched on, or its gains still gliding back to unity after being switched off.
    fn is_active(&self) -> bool {
        self.amount != 0.0 || self.is_gliding()
    }

    fn settle(&mut self) {
        self.side_gain.settle();
        self.mid_gain.settle();
    }

    fn reset(&mut self) {
        // No memory of the signal; a glide under way is all there is to clear.
        self.settle();
    }

    fn process(&mut self, buffer: &mut [Real], channels: usize) {
        // The widener is inherently a stereo operation; on a mono or multichannel stream the
        // original only ever runs it over the front pair.
        if channels < 2 {
            return;
        }
        let (li, ri) = self.front_pair.unwrap_or((0, 1));
        let gliding = self.is_gliding();
        let (mut side_gain, mut mid_gain) = (self.side_gain.value(), self.mid_gain.value());
        for frame in buffer.chunks_exact_mut(channels) {
            if gliding {
                side_gain = self.side_gain.advance();
                mid_gain = self.mid_gain.advance();
            }
            let Some((left_slot, right_slot)) = super::pair_mut(frame, li, ri) else {
                continue;
            };
            let (left, right) = (*left_slot, *right_slot);
            let mid = (left + right) * 0.5;
            let side_left = left - mid;
            let side_right = right - mid;
            let mid = mid * mid_gain;
            *left_slot = mid + side_gain * side_left;
            *right_slot = mid + side_gain * side_right;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/spec/10-dsp-effects.md` §8.2.
    #[test]
    fn the_intensity_mapping_matches_the_reference_table() {
        let mut s = Surround::new(48_000.0);
        for (midi, intensity, side, mid) in [
            (0_u8, 0.0, 1.0, 1.0),
            (13, 0.0717, 1.2150, 0.9785),
            (64, 0.3528, 2.0583, 0.8942),
            (127, 0.7000, 3.1000, 0.7900),
        ] {
            s.set_amount(fxsound_core::scale::midi_to_value(midi));
            assert!(
                (s.intensity() - intensity).abs() < 1e-3,
                "midi {midi} intensity"
            );
            assert!((s.side_gain() - side).abs() < 1e-3, "midi {midi} side gain");
            assert!((s.mid_gain() - mid).abs() < 1e-3, "midi {midi} mid gain");
        }
    }

    #[test]
    fn the_maximum_side_gain_is_3_1_not_4() {
        // The source comment claims "3 to 5"; the intensity cap of 0.7 makes the real maximum 3.1.
        let mut s = Surround::new(48_000.0);
        s.set_amount(1.0);
        assert!((s.side_gain() - 3.1).abs() < 1e-6);
    }

    #[test]
    fn a_mono_signal_is_only_attenuated_never_widened() {
        // Identical channels have no side content, so only the mid gain applies.
        let mut s = Surround::new(48_000.0);
        s.set_amount(1.0);
        // Landed, as a chain nobody has heard yet lands it; what is measured is the amount's
        // gains, not the 20 ms glide to them (audit report #11).
        s.settle();
        let mut buffer = vec![0.5, 0.5, -0.25, -0.25];
        s.process(&mut buffer, 2);
        assert!((buffer[0] - 0.5 * s.mid_gain()).abs() < 1e-6);
        assert_eq!(buffer[0], buffer[1]);
        assert!((buffer[2] - -0.25 * s.mid_gain()).abs() < 1e-6);
    }

    #[test]
    fn out_of_phase_content_is_amplified() {
        let mut s = Surround::new(48_000.0);
        s.set_amount(1.0);
        // Landed, as a chain nobody has heard yet lands it; what is measured is the amount's
        // gains, not the 20 ms glide to them (audit report #11).
        s.settle();
        // Pure side signal: L = -R, so mid is zero.
        let mut buffer = vec![0.3, -0.3];
        s.process(&mut buffer, 2);
        assert!((buffer[0] - 0.3 * s.side_gain()).abs() < 1e-6);
        assert!((buffer[1] + 0.3 * s.side_gain()).abs() < 1e-6);
    }

    #[test]
    fn zero_amount_is_bypassed_by_the_chain_and_transparent_if_it_does_run() {
        let mut s = Surround::new(48_000.0);
        s.set_amount(0.0);
        // The exact bypass is the chain's job: at amount 0 it never calls process() at all, which
        // is what makes a zeroed slider bit-transparent.
        assert!(!s.is_active());

        // Run it anyway: the M/S decomposition at unity gains is the identity in exact arithmetic,
        // but (l+r)*0.5 followed by l-mid costs a rounding step, so allow one ULP.
        let original = [0.1_f32, -0.7, 0.33, 0.9];
        let mut buffer = original.to_vec();
        s.process(&mut buffer, 2);
        for (got, want) in buffer.iter().zip(&original) {
            assert!(
                (got - want).abs() <= f32::EPSILON * want.abs().max(1.0),
                "got {got}, expected {want}"
            );
        }
    }

    #[test]
    fn a_hard_panned_signal_gives_away_the_documented_headroom() {
        let mut s = Surround::new(48_000.0);
        s.set_amount(1.0);
        // Landed, as a chain nobody has heard yet lands it; what is measured is the amount's
        // gains, not the 20 ms glide to them (audit report #11).
        s.settle();
        let mut buffer = vec![1.0, 0.0];
        s.process(&mut buffer, 2);
        // 0.79*0.5 + 3.1*0.5 = 1.945
        assert!((buffer[0] - 1.945).abs() < 1e-3, "got {}", buffer[0]);
    }

    #[test]
    fn a_mono_stream_is_left_alone() {
        let mut s = Surround::new(48_000.0);
        s.set_amount(1.0);
        let original = vec![0.2, 0.4, 0.6];
        let mut buffer = original.clone();
        s.process(&mut buffer, 1);
        assert_eq!(buffer, original);
    }

    #[test]
    fn the_mid_side_transform_is_energy_consistent_at_unity() {
        // With intensity 0 the transform must reconstruct the input exactly, which is the standard
        // check that the M/S decomposition itself is right.
        let mut s = Surround::new(48_000.0);
        s.set_amount(0.0);
        s.intensity = 0.0;
        s.side_gain = Ramp::new(1.0);
        s.mid_gain = Ramp::new(1.0);
        let mut buffer = vec![0.37, -0.81];
        s.process(&mut buffer, 2);
        assert!((buffer[0] - 0.37).abs() < 1e-6);
        assert!((buffer[1] + 0.81).abs() < 1e-6);
    }

    #[test]
    fn taken_to_zero_surround_fades_out_over_twenty_milliseconds_and_is_then_bypassed_exactly() {
        // Audit report #11: the stage runs on through its fade to zero, then the chain skips it
        // and it touches nothing, as it did the moment it reached zero before.
        let mut stage = Surround::new(48_000.0);
        stage.set_amount(1.0);
        stage.settle();
        let tone = |frames: usize| -> Vec<Real> {
            (0..frames)
                .flat_map(|n| {
                    let s = (n as Real * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
                    [s, -0.5 * s]
                })
                .collect()
        };
        let mut playing = tone(4_800);
        stage.process(&mut playing, 2);
        stage.set_amount(0.0);
        assert!(stage.is_active(), "switched off, it went silent at once");
        let mut fading = tone(959);
        stage.process(&mut fading, 2);
        assert!(stage.is_active(), "the fade ended early");
        let mut last = tone(1);
        stage.process(&mut last, 2);
        assert!(!stage.is_active(), "the fade did not end after 20 ms");
    }
}
