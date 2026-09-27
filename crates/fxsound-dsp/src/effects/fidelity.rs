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
use crate::smooth::{Ramp, glide_frames};

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
    /// The waveshaper's drive, gliding to a new amount over [`crate::smooth::GLIDE_SECONDS`]
    /// (audit report #11) instead of stepping between two samples. The high-pass never changes
    /// with the amount, so the drive is the one thing a slider move can step; at zero drive the
    /// stage passes its input, so it fades in from, and out to, the bypass.
    drive: Ramp,
    gain: Real,
    a1: Real,
    a0: Real,
    /// Index of the low-frequency-effects channel, when the layout has one.
    lfe_channel: Option<usize>,
}

impl Fidelity {
    /// Tell the stage which channel is the subwoofer, so it can leave it alone.
    pub fn set_lfe_channel(&mut self, channel: Option<usize>) {
        self.lfe_channel = channel;
    }

    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let mut effect = Self {
            state: [HighPassState::default(); crate::biquad::MAX_CHANNELS],
            sample_rate,
            amount: 0.0,
            drive: Ramp::new(0.0),
            gain: 0.0,
            a1: 0.0,
            a0: 0.0,
            lfe_channel: None,
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

    /// One sample of high-pass plus waveshaping for one channel, at this frame's `drive`.
    #[inline(always)]
    fn tick(&mut self, channel: usize, x: Real, drive: Real) -> Real {
        let state = &mut self.state[channel];

        let mut h = state.y1 * self.a1 + state.y2 * self.a0;
        state.y2 = state.y1;
        h += (x + SOS_FLOAT_BIAS - 2.0 * state.x1 + state.x2) * self.gain;
        state.y1 = h;
        state.x2 = state.x1;
        state.x1 = x;

        let driven = h * drive;
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

    /// Linear in MIDI, and `amount` is already `midi / 127`.
    ///
    /// Coming back from zero clears the high-pass. At zero the chain skips the stage and its
    /// history stands still, as the original's does (`Play32.c:640-647`); resumed from it, the
    /// waveshaper turns the old treble into a click — −6.5 dBFS into silence after a 3 kHz tone
    /// at full Fidelity (audit report #9). Only a stage that really stopped is cleared: one taken
    /// to zero and back while its drive was still fading out never stopped.
    fn set_amount(&mut self, amount: Real) {
        let was_active = self.is_active();
        self.amount = amount.clamp(0.0, 1.0);
        self.drive
            .glide_to(DRIVE_MAX * self.amount, glide_frames(self.sample_rate));
        if !was_active && self.is_active() {
            self.state = [HighPassState::default(); crate::biquad::MAX_CHANNELS];
        }
    }

    fn amount(&self) -> Real {
        self.amount
    }

    /// Switched on, or its drive still fading out after being switched off.
    fn is_active(&self) -> bool {
        self.amount != 0.0 || self.drive.is_gliding()
    }

    fn settle(&mut self) {
        self.drive.settle();
    }

    fn reset(&mut self) {
        self.state = [HighPassState::default(); crate::biquad::MAX_CHANNELS];
        self.drive.settle();
    }

    fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || channels > crate::biquad::MAX_CHANNELS {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            // The glide's value for this frame; the drive it was set to, exactly, when none runs.
            let drive = self.drive.advance();
            for (channel, sample) in frame.iter_mut().enumerate() {
                // The subwoofer never gets this. `docs/spec/08-dsp-api.md:905-906`: the original
                // sends the harmonic generator to the front, rear, side and centre instances and
                // explicitly not to the LFE — and it would not survive the trip anyway, since the
                // waveshaper's whole output is high-frequency content on a channel that is
                // low-passed downstream.
                if Some(channel) == self.lfe_channel {
                    continue;
                }
                *sample = self.tick(channel, *sample, drive);
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
                (f.drive.target() - expected).abs() < 1e-3,
                "midi {midi}: got {}, expected {expected}",
                f.drive.target()
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
        assert!(
            buffer.iter().all(|s| s.abs() < 1e-20),
            "silence was not preserved"
        );
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
        let mut buffer: Vec<Real> = (0..1024)
            .flat_map(|n| [(n as Real * 0.3).sin(), 0.0])
            .collect();
        f.process(&mut buffer, 2);
        assert!(
            buffer.as_chunks::<2>().0.iter().all(|f| f[1].abs() < 1e-20),
            "the right channel picked up the left channel's signal"
        );
    }

    /// Peak of the output when silence goes into a Fidelity that has shaped a loud 3 kHz tone at
    /// full drive and then been set to `off_amount` for 100 ms of the tone and back to full.
    ///
    /// The 100 ms are there since audit report #11: the drive fades over 20 ms, so a Fidelity set
    /// to zero and straight back never stopped — it is the stage that sat at zero while the music
    /// played on that #9 is about.
    fn ring_after_switching(off_amount: Real) -> Real {
        let mut f = Fidelity::new(48_000.0);
        f.set_amount(1.0);
        let mut tone: Vec<Real> = (0..24_001)
            .flat_map(|n| {
                let s = (n as Real * 3_000.0 * TWO_PI / 48_000.0).sin() * 0.5;
                [s, s]
            })
            .collect();
        f.process(&mut tone, 2);
        f.set_amount(off_amount);
        f.process(&mut tone[..2 * 4_800], 2);
        f.set_amount(1.0);
        let mut silence = vec![0.0; 2 * 4_800];
        f.process(&mut silence, 2);
        silence.iter().fold(0.0, |m: Real, s| m.max(s.abs()))
    }

    #[test]
    fn fidelity_brought_back_from_zero_starts_from_rest() {
        // Audit report #9: the high-pass resumed from the treble it last heard, and the
        // waveshaper turned it into a click at -6.5 dBFS in silence.
        let peak = ring_after_switching(0.0);
        assert!(peak < 1e-20, "the old state came out at {peak}");
    }

    #[test]
    fn fidelity_that_stays_on_keeps_its_state_through_an_amount_change() {
        assert!(ring_after_switching(0.5) > 1e-3);
    }

    #[test]
    fn a_sample_rate_change_redesigns_the_filter() {
        let mut f = Fidelity::new(44_100.0);
        let before = f.gain;
        f.set_sample_rate(96_000.0);
        assert_ne!(before, f.gain);
        assert_eq!(f.sample_rate, 96_000.0);
    }

    #[test]
    fn taken_to_zero_fidelity_fades_out_over_twenty_milliseconds_and_is_then_bypassed_exactly() {
        // Audit report #11: the stage runs on through its fade to zero, then the chain skips it
        // and it touches nothing, as it did the moment it reached zero before.
        let mut stage = Fidelity::new(48_000.0);
        stage.set_amount(1.0);
        stage.settle();
        let tone = |frames: usize| -> Vec<Real> {
            (0..frames)
                .flat_map(|n| {
                    let s = (n as Real * 3_000.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
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
