//! The high-pass: a rumble filter, second or fourth order.
//!
//! Desk rumble and a plosive are ten to twenty decibels above the voice below 150 Hz. Left in,
//! they hold the gate open through every pause and drive the compressor on sounds nobody can
//! hear, which is why this sits ahead of both. In 0.3.0 it lived inline in the chain; it is a
//! stage of its own now so that a chain spec can place it.
//!
//! Real-time safe: two sections, fixed, designed only when the corner or the order changes.

use crate::biquad::{MAX_CHANNELS, Real, Section, calc_butterworth_highpass};
use crate::input::processor::{AudioProcessor, ProcessContext, StageMeter};
use crate::input::{prewarped, sane_rate};
use fxsound_core::messages::InputDspParams;

/// Highest order the stage can build: two cascaded second-order sections.
const MAX_SECTIONS: usize = 2;

pub struct HighPass {
    sections: [Section; MAX_SECTIONS],
    hz: Real,
    /// `0`, `2` or `4`. Zero is off; the preset set uses both of the others.
    order: usize,
    /// How many sections the rate could actually carry — a corner too close to Nyquist is not a
    /// filter, and the stage says so rather than building something else.
    live: usize,
    sample_rate: Real,
}

impl std::fmt::Debug for HighPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HighPass")
            .field("sample_rate", &self.sample_rate)
            .field("hz", &self.hz)
            .field("order", &self.order)
            .field("sections", &self.live)
            .finish()
    }
}

impl HighPass {
    /// The default is Clean Voice's: 80 Hz, second order.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let mut stage = Self {
            sections: [Section::new(); MAX_SECTIONS],
            hz: 80.0,
            order: 2,
            live: 0,
            sample_rate: sane_rate(sample_rate),
        };
        stage.design();
        stage
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sane_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.design();
        self.reset();
    }

    /// The corner and its order — `0` for off, `2` or `4`. Anything else rounds down to one of
    /// those, because there are only two sections to build it from.
    ///
    /// **Order 4 is Linkwitz–Riley, not Butterworth, and it is 6 dB down at the corner rather than
    /// 3.** It is two *identical* second-order Butterworth sections in series; a true fourth-order
    /// Butterworth would need two sections at different Qs, and this crate's design function only
    /// makes the one Q. That is a fine filter — the steeper approach is what a rumble filter wants
    /// — but the number matters to whoever writes a preset: measured at 48 kHz, a 120 Hz
    /// fourth-order corner is −6.0 dB at 120 Hz, −6.7 at 115.7, −9.8 at 100 and −13.9 at 85, where
    /// the second-order one is −3.0, −3.3, −4.9 and −7.0. A preset voiced against the textbook
    /// Butterworth figures is voiced against a filter this chain does not build.
    pub fn set(&mut self, hz: Real, order: usize) {
        let hz = if hz.is_finite() && hz > 0.0 { hz } else { 80.0 };
        let order = match order {
            0 => 0,
            1..=3 => 2,
            _ => 4,
        };
        // Redesigning clears the filter's history, which is a click. A parameter snapshot arrives
        // whenever *any* control moves, so a corner that did not change must not be rebuilt
        // because the gate threshold did.
        if hz == self.hz && order == self.order {
            return;
        }
        self.hz = hz;
        self.order = order;
        self.design();
        for section in &mut self.sections {
            section.reset();
        }
    }

    #[must_use]
    pub const fn hz(&self) -> Real {
        self.hz
    }

    #[must_use]
    pub const fn order(&self) -> usize {
        self.order
    }

    /// How many second-order sections are actually running: `0` when the stage is switched off
    /// *or* when the sample rate cannot carry the corner asked for.
    #[must_use]
    pub const fn sections(&self) -> usize {
        self.live
    }

    fn design(&mut self) {
        self.live = 0;
        if self.order == 0 {
            return;
        }
        let Some(request) = prewarped(self.sample_rate, self.hz) else {
            return;
        };
        let coeffs = calc_butterworth_highpass(self.sample_rate, request);
        for section in &mut self.sections {
            section.coeffs = coeffs;
        }
        self.live = self.order / 2;
    }

    pub fn reset(&mut self) {
        for section in &mut self.sections {
            section.reset();
        }
    }

    /// A whole interleaved block, in place.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if self.live == 0 || channels == 0 {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            for (channel, sample) in frame.iter_mut().enumerate().take(MAX_CHANNELS) {
                for section in self.sections.iter_mut().take(self.live) {
                    *sample = section.tick_general(channel, *sample);
                }
            }
        }
    }
}

impl AudioProcessor for HighPass {
    fn prepare(&mut self, sample_rate: Real) {
        self.set_sample_rate(sample_rate);
    }

    fn apply(&mut self, params: &InputDspParams) {
        self.set(params.highpass_hz, usize::from(params.highpass_order));
    }

    fn reset(&mut self) {
        HighPass::reset(self);
    }

    fn is_active(&self) -> bool {
        self.live != 0
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        HighPass::process(self, buffer, ctx.channels);
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: 0.0,
            running: self.live != 0,
            aux: self.hz,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    fn tone(hz: Real, amplitude: Real, frames: usize) -> Vec<Real> {
        (0..frames)
            .map(|n| (n as Real * std::f32::consts::TAU * hz / FS).sin() * amplitude)
            .collect()
    }

    fn level_after(stage: &mut HighPass, hz: Real) -> Real {
        let mut block = tone(hz, 0.1, 48_000);
        stage.process(&mut block, 1);
        let peak = block[24_000..].iter().fold(0.0_f32, |a, b| a.max(b.abs()));
        20.0 * (peak / 0.1).log10()
    }

    #[test]
    fn order_zero_is_off_and_says_so() {
        let mut stage = HighPass::new(FS);
        stage.set(80.0, 0);
        assert_eq!(stage.sections(), 0);
        assert!(!stage.is_active());
        let input = tone(30.0, 0.5, 4_800);
        let mut block = input.clone();
        stage.process(&mut block, 1);
        assert_eq!(block, input);
    }

    #[test]
    fn the_fourth_order_corner_is_six_decibels_down_and_the_second_is_three() {
        let mut fourth = HighPass::new(FS);
        fourth.set(120.0, 4);
        assert_eq!(fourth.sections(), 2);
        let got = level_after(&mut fourth, 120.0);
        assert!(
            (got + 6.0).abs() < 0.2,
            "order 4 at its corner: {got:.2} dB"
        );

        let mut second = HighPass::new(FS);
        second.set(120.0, 2);
        assert_eq!(second.sections(), 1);
        let got = level_after(&mut second, 120.0);
        assert!(
            (got + 3.0).abs() < 0.2,
            "order 2 at its corner: {got:.2} dB"
        );
    }

    #[test]
    fn a_corner_the_rate_cannot_carry_takes_the_stage_out_rather_than_moving_it() {
        let mut stage = HighPass::new(16_000.0);
        stage.set(80.0, 4);
        assert_eq!(stage.sections(), 2);
        stage.set(7_000.0, 2);
        assert_eq!(stage.sections(), 0);
        assert!(!stage.is_active());
    }

    #[test]
    fn an_unchanged_corner_is_not_redesigned() {
        // The history is the click. Two stages fed the same continuing signal, one of which is
        // asked for the corner it already has, must stay sample-identical.
        let mut untouched = HighPass::new(FS);
        let mut asked_again = HighPass::new(FS);
        for stage in [&mut untouched, &mut asked_again] {
            stage.set(80.0, 4);
            let mut warm = tone(40.0, 0.5, 4_800);
            stage.process(&mut warm, 1);
        }
        asked_again.set(80.0, 4);
        let next: Vec<Real> = (4_800..5_280)
            .map(|n| (n as Real * std::f32::consts::TAU * 40.0 / FS).sin() * 0.5)
            .collect();
        let (mut a, mut b) = (next.clone(), next);
        untouched.process(&mut a, 1);
        asked_again.process(&mut b, 1);
        assert_eq!(a, b);
    }

    #[test]
    fn every_channel_is_filtered_with_its_own_history() {
        let mut stage = HighPass::new(FS);
        stage.set(200.0, 4);
        let mut block = vec![0.0; 9_600 * 2];
        for (n, sample) in block.iter_mut().enumerate() {
            let frame = (n / 2) as Real;
            *sample = if n % 2 == 0 {
                (frame * std::f32::consts::TAU * 30.0 / FS).sin() * 0.5
            } else {
                (frame * std::f32::consts::TAU * 3_000.0 / FS).sin() * 0.5
            };
        }
        stage.process(&mut block, 2);
        let left = block[9_600..]
            .iter()
            .step_by(2)
            .fold(0.0_f32, |a, b| a.max(b.abs()));
        let right = block[9_600..]
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0.0_f32, |a, b| a.max(b.abs()));
        assert!(left < 0.02, "the rumble on the left survived: {left}");
        assert!(right > 0.45, "the voice on the right was filtered: {right}");
    }
}
