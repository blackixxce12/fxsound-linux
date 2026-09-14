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
//! It is stateless, zero-latency and allocation-free. The pre-2025 algorithm — a Haas/dispersion
//! widener with high-passed side channels and delay lines — is still in the tree at
//! `wide/Wide16/Wide16.c` but is unreachable in the 32-bit build, so it is not ported.
//!
//! At the maximum setting a signal panned hard to one channel reaches `0.79·0.5 + 3.1·0.5 = 1.945`,
//! i.e. nearly 6 dB of headroom given away. That never audibly clips only because Dynamic Boost
//! runs after it and catches the overshoot.

use super::Effect;
use crate::biquad::Real;

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
    side_gain: Real,
    mid_gain: Real,
}

impl Surround {
    #[must_use]
    pub fn new(_sample_rate: Real) -> Self {
        let mut effect = Self {
            amount: 0.0,
            intensity: 0.0,
            side_gain: 1.0,
            mid_gain: 1.0,
        };
        effect.set_amount(0.0);
        effect
    }

    /// The widener's internal intensity, `0.0..=0.7`.
    #[must_use]
    pub const fn intensity(&self) -> Real {
        self.intensity
    }

    /// Gain applied to the side signal, `1.0..=3.1`.
    #[must_use]
    pub const fn side_gain(&self) -> Real {
        self.side_gain
    }

    /// Gain applied to the mid signal, `1.0..=0.79`.
    #[must_use]
    pub const fn mid_gain(&self) -> Real {
        self.mid_gain
    }
}

impl Effect for Surround {
    fn set_sample_rate(&mut self, _sample_rate: Real) {
        // Nothing here depends on the sample rate.
    }

    fn set_amount(&mut self, amount: Real) {
        self.amount = amount.clamp(0.0, 1.0);
        // Linear in MIDI, with no music-mode warping (`dfxpComm.cpp:786-818`).
        self.intensity = INTENSITY_MAX * self.amount;
        self.side_gain = 1.0 + SIDE_SLOPE * self.intensity;
        self.mid_gain = 1.0 - MID_SLOPE * self.intensity;
    }

    fn amount(&self) -> Real {
        self.amount
    }

    fn is_active(&self) -> bool {
        self.amount != 0.0
    }

    fn reset(&mut self) {
        // Stateless.
    }

    fn process(&mut self, buffer: &mut [Real], channels: usize) {
        // The widener is inherently a stereo operation; on a mono or multichannel stream the
        // original only ever runs it over the front pair.
        if channels < 2 {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            let (left, right) = (frame[0], frame[1]);
            let mid = (left + right) * 0.5;
            let side_left = left - mid;
            let side_right = right - mid;
            let mid = mid * self.mid_gain;
            frame[0] = mid + self.side_gain * side_left;
            frame[1] = mid + self.side_gain * side_right;
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
            assert!((s.intensity() - intensity).abs() < 1e-3, "midi {midi} intensity");
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
        s.side_gain = 1.0;
        s.mid_gain = 1.0;
        let mut buffer = vec![0.37, -0.81];
        s.process(&mut buffer, 2);
        assert!((buffer[0] - 0.37).abs() < 1e-6);
        assert!((buffer[1] + 0.81).abs() < 1e-6);
    }
}
