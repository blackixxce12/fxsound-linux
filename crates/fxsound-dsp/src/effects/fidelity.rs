//! Fidelity — the aural exciter, labelled "Clarity" in some versions of the UI.
//!
//! Ports `dsp/ptechDsp/Aural/Aural032/Auralp32.c` as it is actually configured at run time. The
//! whole effect is one line once the constants are folded
//! (`docs/spec/10-dsp-effects.md` §6.3):
//!
//! ```text
//! out = in + 0.566930 · sin(drive · highpass(in))
//! ```
//!
//! A second-order Butterworth high-pass at a fixed 1745.4987 Hz feeds a sine waveshaper. At high
//! settings `drive · h` passes π/2 and the sine *folds back*, which is deliberate: it is what gives
//! the effect its character. Substituting `tanh` — the obvious "nicer" saturator — does not fold
//! and does not sound the same.

use super::Effect;
use crate::biquad::{Real, SOS_FLOAT_BIAS};

/// The high-pass corner, fixed by `DSP_PLAY_AURAL_TUNE_MIDI = 53` through an exponential
/// quantiser spanning 500 Hz..10 kHz: `500 · 20^(53/127)` (`docs/spec/10-dsp-effects.md` §6.2).
///
/// Sample-rate independent, and never exposed in the UI.
pub const CUTOFF_HZ: Real = 1745.4987;

/// `DSP_AURAL_DRIVE_MAX_VALUE · PLY_FIDELITY_INTENSITY_MAX_SCALE = 4.2411501 · 0.8`.
///
/// Written with the original's digits even though `f32` cannot hold them all, so the constant can
/// be grepped against the C source.
#[allow(clippy::excessive_precision)]
const DRIVE_MAX: Real = 3.3929201;
/// `DSP_AURAL_ODD_MAX_VALUE` — the odd-harmonic branch gain (`c_aural.h:69-76`).
const ODD_GAIN: Real = 1.5;
/// `DSP_AURAL_EVEN_MAX_VALUE` — the even branch ships switched off.
const EVEN_GAIN: Real = 0.0;
/// `Play32.c:266-267`. These sum to exactly 1.0.
const WET_GAIN: Real = 0.377_953;
const DRY_GAIN: Real = 0.622_047;

const SQRT2: Real = std::f32::consts::SQRT_2;
const TWO_PI: Real = std::f32::consts::TAU;

/// Per-channel high-pass history (`c_aural.h:57-65`).
#[derive(Clone, Copy, Debug, Default)]
struct HighPassState {
    x1: Real,
    x2: Real,
    y1: Real,
    y2: Real,
}

/// The aural exciter.
#[derive(Clone, Debug)]
pub struct Fidelity {
    state: [HighPassState; crate::biquad::MAX_CHANNELS],
    sample_rate: Real,
    amount: Real,
    drive: Real,
    gain: Real,
    a1: Real,
    a0: Real,
}

impl Fidelity {
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let mut effect = Self {
            state: [HighPassState::default(); crate::biquad::MAX_CHANNELS],
            sample_rate,
            amount: 0.0,
            drive: 0.0,
            gain: 0.0,
            a1: 0.0,
            a0: 0.0,
        };
        effect.design();
        effect
    }

    /// `filtDesign2ndButHighPass` (`dsp/ptutil/Filt/Fil12But.cpp:130-141`).
    ///
    /// A bilinear transform of `s²/(s² + √2ωs + ω²)` with **no frequency prewarping**, so the
    /// realised corner sits slightly below the nominal one and the error grows with `f/fs`. That
    /// is the original's behaviour and presets were voiced against it, so it is reproduced rather
    /// than corrected. `a1` and `a0` come out of the design already negated.
    fn design(&mut self) {
        let w = TWO_PI * CUTOFF_HZ / self.sample_rate;
        let w2 = w * w;
        let t = 1.0 / (4.0 + w2 + 2.0 * SQRT2 * w);
        self.gain = 4.0 * t;
        self.a1 = (8.0 - 2.0 * w2) * t;
        self.a0 = (2.0 * SQRT2 * w - 4.0 - w2) * t;
    }

    /// One sample of high-pass plus waveshaping for one channel.
    #[inline(always)]
    fn tick(&mut self, channel: usize, x: Real) -> Real {
        let state = &mut self.state[channel];

        let mut h = state.y1 * self.a1 + state.y2 * self.a0;
        state.y2 = state.y1;
        h += (x + SOS_FLOAT_BIAS - 2.0 * state.x1 + state.x2) * self.gain;
        state.y1 = h;
        state.x2 = state.x1;
        state.x1 = x;

        let driven = h * self.drive;
        let odd = driven.sin();
        // The even branch is a half-wave rectifier. It ships with a gain of zero, so it
        // contributes nothing; it is kept because the original's parameter slot still exists.
        let even = if driven > 0.0 { driven } else { 0.0 };

        let shaped = x + (EVEN_GAIN * even + ODD_GAIN * odd);
        shaped * WET_GAIN + DRY_GAIN * x
    }
}

impl Effect for Fidelity {
    fn set_sample_rate(&mut self, sample_rate: Real) {
        if sample_rate == self.sample_rate || sample_rate <= 0.0 {
            return;
        }
        self.sample_rate = sample_rate;
        self.design();
        self.reset();
    }

    fn set_amount(&mut self, amount: Real) {
        self.amount = amount.clamp(0.0, 1.0);
        // Linear in MIDI, and `amount` is already `midi / 127`.
        self.drive = DRIVE_MAX * self.amount;
    }

    fn amount(&self) -> Real {
        self.amount
    }

    fn is_active(&self) -> bool {
        self.amount != 0.0
    }

    fn reset(&mut self) {
        self.state = [HighPassState::default(); crate::biquad::MAX_CHANNELS];
    }

    fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || channels > crate::biquad::MAX_CHANNELS {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = self.tick(channel, *sample);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/spec/10-dsp-effects.md` §6.2 tabulates the design at two rates.
    #[test]
    fn the_high_pass_design_matches_the_reference_table() {
        for (fs, gain, a1, a0) in [
            (44_100.0, 0.839_410, 1.652_862, -0.704_777),
            (48_000.0, 0.851_343, 1.680_463, -0.724_908),
        ] {
            let f = Fidelity::new(fs);
            assert!((f.gain - gain).abs() < 1e-4, "{fs} Hz gain: {}", f.gain);
            assert!((f.a1 - a1).abs() < 1e-4, "{fs} Hz a1: {}", f.a1);
            assert!((f.a0 - a0).abs() < 1e-4, "{fs} Hz a0: {}", f.a0);
        }
    }

    /// §6.4 tabulates drive against the ten slider positions.
    #[test]
    fn the_drive_mapping_matches_the_reference_table() {
        let mut f = Fidelity::new(48_000.0);
        for (midi, expected) in [
            (0_u8, 0.0),
            (13, 0.347_3),
            (51, 1.362_5),
            (64, 1.709_8),
            (127, 3.392_9),
        ] {
            f.set_amount(fxsound_core::scale::midi_to_value(midi));
            assert!(
                (f.drive - expected).abs() < 1e-3,
                "midi {midi}: got {}, expected {expected}",
                f.drive
            );
        }
    }

    #[test]
    fn zero_amount_is_inactive() {
        let mut f = Fidelity::new(48_000.0);
        f.set_amount(0.0);
        assert!(!f.is_active());
        f.set_amount(0.1);
        assert!(f.is_active());
    }

    #[test]
    fn a_full_scale_drive_folds_rather_than_saturating() {
        // The signature of the effect: past pi/2 the sine turns back down. A saturator would be
        // monotonic, so this test fails if someone swaps in tanh.
        let mut f = Fidelity::new(48_000.0);
        f.set_amount(1.0);
        let peak = (std::f32::consts::FRAC_PI_2 / DRIVE_MAX).sin();
        let past_peak = (DRIVE_MAX).sin();
        assert!(
            past_peak < peak,
            "sin({DRIVE_MAX}) = {past_peak} should be below the peak {peak}"
        );
    }

    #[test]
    fn silence_stays_silent() {
        let mut f = Fidelity::new(48_000.0);
        f.set_amount(1.0);
        let mut buffer = vec![0.0; 2048];
        f.process(&mut buffer, 2);
        // SOS_FLOAT_BIAS leaks a denormal-sized constant through, which is the point of it.
        assert!(buffer.iter().all(|s| s.abs() < 1e-20), "silence was not preserved");
    }

    #[test]
    fn low_frequencies_pass_through_largely_untouched() {
        // The exciter only shapes content above ~1.7 kHz, so a 100 Hz tone should come out close
        // to how it went in.
        let mut f = Fidelity::new(48_000.0);
        f.set_amount(1.0);
        let frames = 4800;
        let input: Vec<Real> = (0..frames)
            .flat_map(|n| {
                let s = (n as Real * 100.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                [s, s]
            })
            .collect();
        let mut output = input.clone();
        f.process(&mut output, 2);

        // Compare over the tail, after the filter has settled.
        let tail = frames * 2 / 2;
        let error: Real = input[tail..]
            .iter()
            .zip(&output[tail..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, Real::max);
        assert!(error < 0.05, "100 Hz tone changed by {error}");
    }

    #[test]
    fn high_frequencies_gain_harmonics() {
        // A 4 kHz tone is above the corner, so the waveshaper should add energy.
        let mut f = Fidelity::new(48_000.0);
        f.set_amount(1.0);
        let frames = 4800;
        let input: Vec<Real> = (0..frames)
            .flat_map(|n| {
                let s = (n as Real * 4000.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                [s, s]
            })
            .collect();
        let mut output = input.clone();
        f.process(&mut output, 2);

        let rms = |v: &[Real]| (v.iter().map(|s| s * s).sum::<Real>() / v.len() as Real).sqrt();
        assert!(
            rms(&output) > rms(&input) * 1.05,
            "the exciter did not add energy at 4 kHz"
        );
        assert!(output.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn channels_are_filtered_independently() {
        let mut f = Fidelity::new(48_000.0);
        f.set_amount(0.8);
        // Signal in the left channel only; the right must stay silent.
        let mut buffer: Vec<Real> = (0..1024).flat_map(|n| [(n as Real * 0.3).sin(), 0.0]).collect();
        f.process(&mut buffer, 2);
        assert!(
            buffer.chunks_exact(2).all(|f| f[1].abs() < 1e-20),
            "the right channel picked up the left channel's signal"
        );
    }

    #[test]
    fn a_sample_rate_change_redesigns_the_filter() {
        let mut f = Fidelity::new(44_100.0);
        let before = f.gain;
        f.set_sample_rate(96_000.0);
        assert_ne!(before, f.gain);
        assert_eq!(f.sample_rate, 96_000.0);
    }
}
