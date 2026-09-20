//! The microphone chain, in order.
//!
//! ```text
//! mic ─► high-pass ─► gate ─► 10-band EQ ─► de-esser ─► compressor ─► makeup ─► limiter ─► out
//! ```
//!
//! The order is not a preference. Each position earns itself:
//!
//! - **High-pass first**, because desk rumble and a plosive are ten to twenty decibels above the
//!   voice below 150 Hz. Leave them in and they hold the gate open through every pause and drive
//!   the compressor on sounds nobody can hear.
//! - **Gate before the EQ**, so that what the gate measures is the microphone rather than the
//!   preset's own presence lift — otherwise changing a band would move the threshold.
//! - **De-esser before the compressor**, which is the order the design was written down in. A
//!   compressor in front would ride the sibilant and duck the word behind it, which is the exact
//!   complaint that makes a de-esser necessary in the first place.
//! - **Makeup after everything that measures**, because every threshold in the preset set was
//!   voiced against the signal as it arrives, not against one already lifted.
//! - **Limiter last, always running.** It is the only stage that cannot be switched off: makeup
//!   gain is the one control here that can produce a sample above full scale, and something has to
//!   be standing behind it.
//!
//! Denoising, when it arrives, goes in *front* of the high-pass — five of the nine community
//! chains surveyed put it first and none put it after the limiter — which is why the gate
//! thresholds in the preset table will have to be re-voiced when it does.
//!
//! Real-time safe: every stage is fixed-size, and the chain itself only sequences them.

use crate::biquad::{MAX_CHANNELS, Real, Section, calc_butterworth_highpass};
use crate::eq::GraphicEq;
use crate::input::limiter::LookaheadLimiter;
use crate::input::{Compressor, DeEsser, Gate, prewarped, sane_rate};

/// The default ceiling for a microphone stream, in dBFS.
///
/// −3 rather than −1: what leaves here is re-encoded downstream, Opus for a voice call and AAC for
/// the streaming platforms, and a lossy encoder overshoots the sample peak it was handed.
const DEFAULT_CEILING_DB: Real = -3.0;
/// A millisecond of look-ahead. Enough for the limiter to arrive before a transient does, short
/// enough that nobody is talking over themselves.
const LOOKAHEAD_MS: Real = 1.0;
const LIMITER_RELEASE_MS: Real = 80.0;

/// Highest high-pass order the chain can build: two cascaded second-order sections.
const MAX_SECTIONS: usize = 2;

pub struct InputChain {
    highpass: [Section; MAX_SECTIONS],
    highpass_hz: Real,
    /// `0`, `2` or `4`. Zero is off; the preset set uses both of the others.
    highpass_order: usize,
    /// How many sections the rate could actually carry — a corner too close to Nyquist is not a
    /// filter, and the chain says so rather than building something else.
    highpass_sections: usize,

    gate: Gate,
    eq: GraphicEq,
    deesser: DeEsser,
    compressor: Compressor,
    limiter: LookaheadLimiter,

    makeup: Real,
    makeup_db: Real,

    gate_on: bool,
    deesser_on: bool,
    compressor_on: bool,

    sample_rate: Real,
    power: bool,
}

impl std::fmt::Debug for InputChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputChain")
            .field("sample_rate", &self.sample_rate)
            .field("power", &self.power)
            .field("highpass_hz", &self.highpass_hz)
            .field("highpass_order", &self.highpass_order)
            .field("highpass_sections", &self.highpass_sections)
            .field("gate_on", &self.gate_on)
            .field("deesser_on", &self.deesser_on)
            .field("compressor_on", &self.compressor_on)
            .field("makeup_db", &self.makeup_db)
            .finish()
    }
}

impl InputChain {
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let sample_rate = sane_rate(sample_rate);
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(sample_rate);
        let mut limiter = LookaheadLimiter::new(sample_rate, 1.0, LOOKAHEAD_MS, LIMITER_RELEASE_MS);
        limiter.set_ceiling_db(DEFAULT_CEILING_DB);

        let mut chain = Self {
            highpass: [Section::new(); MAX_SECTIONS],
            highpass_hz: 80.0,
            highpass_order: 2,
            highpass_sections: 0,
            gate: Gate::new(sample_rate),
            eq,
            deesser: DeEsser::new(sample_rate),
            compressor: Compressor::new(sample_rate),
            limiter,
            makeup: 1.0,
            makeup_db: 0.0,
            gate_on: true,
            deesser_on: true,
            compressor_on: true,
            sample_rate,
            power: true,
        };
        chain.design_highpass();
        chain
    }

    /// Master bypass. When off the chain does not touch the buffer at all, as the output chain's
    /// does — including the limiter, because a bypass that still processes is not one.
    pub fn set_power(&mut self, on: bool) {
        if self.power != on {
            self.power = on;
            // Coming back with a half-closed gate or a delay line full of the last sentence would
            // be audible on the first word.
            self.reset();
        }
    }

    #[must_use]
    pub const fn power(&self) -> bool {
        self.power
    }

    #[must_use]
    pub const fn sample_rate(&self) -> Real {
        self.sample_rate
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sane_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.gate.set_sample_rate(sample_rate);
        self.eq.set_sample_rate(sample_rate);
        self.deesser.set_sample_rate(sample_rate);
        self.compressor.set_sample_rate(sample_rate);
        self.limiter.set_sample_rate(sample_rate);
        self.design_highpass();
        self.reset();
    }

    /// The high-pass corner and its order — `0` for off, `2` or `4`. Anything else rounds down to
    /// one of those, because there are only two sections to build it from.
    pub fn set_highpass(&mut self, hz: Real, order: usize) {
        self.highpass_hz = if hz.is_finite() && hz > 0.0 { hz } else { 80.0 };
        self.highpass_order = match order {
            0 => 0,
            1..=3 => 2,
            _ => 4,
        };
        self.design_highpass();
        for section in &mut self.highpass {
            section.reset();
        }
    }

    /// How many second-order sections the high-pass is actually running: `0` when it is switched
    /// off *or* when the sample rate cannot carry the corner asked for.
    #[must_use]
    pub const fn highpass_sections(&self) -> usize {
        self.highpass_sections
    }

    /// Gain applied after everything that measures and before the limiter, in dB.
    pub fn set_makeup_db(&mut self, db: Real) {
        let db = if db.is_finite() {
            db.clamp(-24.0, 24.0)
        } else {
            0.0
        };
        self.makeup_db = db;
        self.makeup = 10.0_f32.powf(db / 20.0);
    }

    #[must_use]
    pub const fn makeup_db(&self) -> Real {
        self.makeup_db
    }

    /// The level the chain's output may never exceed, in dBFS.
    pub fn set_ceiling_db(&mut self, db: Real) {
        self.limiter.set_ceiling_db(db);
    }

    pub fn set_gate_enabled(&mut self, on: bool) {
        if self.gate_on != on {
            self.gate_on = on;
            self.gate.reset();
        }
    }

    pub fn set_deesser_enabled(&mut self, on: bool) {
        if self.deesser_on != on {
            self.deesser_on = on;
            self.deesser.reset();
        }
    }

    pub fn set_compressor_enabled(&mut self, on: bool) {
        if self.compressor_on != on {
            self.compressor_on = on;
            self.compressor.reset();
        }
    }

    /// Each stage owns its own parameters; the chain owns the order, the format and the bypass.
    pub fn gate_mut(&mut self) -> &mut Gate {
        &mut self.gate
    }

    pub fn eq_mut(&mut self) -> &mut GraphicEq {
        &mut self.eq
    }

    pub fn deesser_mut(&mut self) -> &mut DeEsser {
        &mut self.deesser
    }

    pub fn compressor_mut(&mut self) -> &mut Compressor {
        &mut self.compressor
    }

    #[must_use]
    pub const fn gate(&self) -> &Gate {
        &self.gate
    }

    #[must_use]
    pub const fn eq(&self) -> &GraphicEq {
        &self.eq
    }

    #[must_use]
    pub const fn deesser(&self) -> &DeEsser {
        &self.deesser
    }

    #[must_use]
    pub const fn compressor(&self) -> &Compressor {
        &self.compressor
    }

    #[must_use]
    pub const fn limiter(&self) -> &LookaheadLimiter {
        &self.limiter
    }

    fn design_highpass(&mut self) {
        self.highpass_sections = 0;
        if self.highpass_order == 0 {
            return;
        }
        let Some(request) = prewarped(self.sample_rate, self.highpass_hz) else {
            return;
        };
        let coeffs = calc_butterworth_highpass(self.sample_rate, request);
        for section in &mut self.highpass {
            section.coeffs = coeffs;
        }
        self.highpass_sections = self.highpass_order / 2;
    }

    pub fn reset(&mut self) {
        for section in &mut self.highpass {
            section.reset();
        }
        self.gate.reset();
        self.eq.reset();
        self.deesser.reset();
        self.compressor.reset();
        self.limiter.reset();
    }

    /// Frames of delay the chain adds. Only the limiter's look-ahead contributes; everything else
    /// here is a filter or a gain.
    ///
    /// Reported whether or not the chain is powered, as the output chain does: a latency that
    /// changed when someone pressed a button would mean renegotiating the stream to save a
    /// millisecond.
    #[must_use]
    pub const fn latency_frames(&self) -> usize {
        self.limiter.latency_frames()
    }

    /// Run the chain over one interleaved block, in place.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.power || channels == 0 || buffer.is_empty() {
            return;
        }

        if self.highpass_sections != 0 {
            for frame in buffer.chunks_exact_mut(channels) {
                for (channel, sample) in frame.iter_mut().enumerate().take(MAX_CHANNELS) {
                    for section in self.highpass.iter_mut().take(self.highpass_sections) {
                        *sample = section.tick_general(channel, *sample);
                    }
                }
            }
        }

        if self.gate_on {
            self.gate.process(buffer, channels);
        }
        if self.eq.is_enabled() {
            self.eq.process(buffer, channels);
        }
        if self.deesser_on {
            self.deesser.process(buffer, channels);
        }
        if self.compressor_on {
            self.compressor.process(buffer, channels);
        }
        if self.makeup != 1.0 {
            for sample in buffer.iter_mut() {
                *sample *= self.makeup;
            }
        }
        // Always. Everything above it can be switched off; the thing standing behind the makeup
        // gain cannot.
        self.limiter.process(buffer, channels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::detector::{db_to_linear, linear_to_db};

    const FS: Real = 48_000.0;

    fn silent(chain: &mut InputChain, seconds: Real) {
        let mut block = vec![0.0; (FS * seconds) as usize];
        chain.process(&mut block, 1);
    }

    fn tone(hz: Real, amplitude: Real, frames: usize) -> Vec<Real> {
        (0..frames)
            .map(|n| (n as Real * std::f32::consts::TAU * hz / FS).sin() * amplitude)
            .collect()
    }

    /// Everything switched off, so a test can add one stage back and see only that stage.
    fn bare(sample_rate: Real) -> InputChain {
        let mut chain = InputChain::new(sample_rate);
        chain.set_highpass(80.0, 0);
        chain.set_gate_enabled(false);
        chain.eq_mut().set_enabled(false);
        chain.set_deesser_enabled(false);
        chain.set_compressor_enabled(false);
        chain
    }

    #[test]
    fn with_every_stage_off_only_the_limiters_delay_remains() {
        let mut chain = bare(FS);
        let input = tone(300.0, db_to_linear(-20.0), 8_000);
        let mut out = input.clone();
        chain.process(&mut out, 1);
        let delay = chain.latency_frames();
        assert_eq!(delay, 48, "1 ms at 48 kHz");
        for n in delay..input.len() {
            assert!(
                (out[n] - input[n - delay]).abs() < 1.0e-6,
                "frame {n}: {} against {}",
                out[n],
                input[n - delay]
            );
        }
    }

    #[test]
    fn a_powered_down_chain_does_not_touch_the_buffer() {
        let mut chain = InputChain::new(FS);
        chain.set_makeup_db(12.0);
        chain.set_power(false);
        let input = tone(300.0, 0.5, 4_000);
        let mut out = input.clone();
        chain.process(&mut out, 1);
        assert_eq!(out, input);
    }

    #[test]
    fn nothing_leaves_above_the_ceiling_however_much_makeup_is_asked_for() {
        // The reason the limiter cannot be switched off. Makeup is the one control here that can
        // manufacture a sample above full scale, and a voice stream that clips is a voice stream
        // that clips for everyone listening.
        for makeup_db in [0.0, 6.0, 12.0, 24.0] {
            let mut chain = bare(FS);
            chain.set_makeup_db(makeup_db);
            let mut block = tone(220.0, db_to_linear(-6.0), 48_000);
            chain.process(&mut block, 1);
            let peak = block.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
            assert!(
                peak <= db_to_linear(-3.0) * 1.0001,
                "{makeup_db} dB of makeup let {peak} out"
            );
        }
    }

    #[test]
    fn the_high_pass_runs_before_the_gate_so_rumble_cannot_hold_it_open() {
        // Order, measured rather than asserted. A 25 Hz rumble at −25 dB is twenty decibels above
        // the gate's threshold; if the gate saw it, it would hold the gate wide open through every
        // silence in the recording. (At 40 Hz the same 80 Hz filter leaves it only 25 dB down,
        // which is enough to start the gate closing but not enough to reach its range — a useful
        // reminder that a high-pass corner is a slope, not a wall.)
        let rumble = tone(25.0, db_to_linear(-25.0), 48_000);

        let mut with_filter = InputChain::new(FS);
        with_filter.set_highpass(80.0, 4);
        with_filter.set_gate_enabled(true);
        with_filter.gate_mut().set_threshold_db(-45.0);
        with_filter.gate_mut().set_range_db(-14.0);
        with_filter.gate_mut().set_hold_ms(0.0);
        with_filter.set_deesser_enabled(false);
        with_filter.set_compressor_enabled(false);
        let mut block = rumble.clone();
        with_filter.process(&mut block, 1);

        let mut without = InputChain::new(FS);
        without.set_highpass(80.0, 0);
        without.set_gate_enabled(true);
        without.gate_mut().set_threshold_db(-45.0);
        without.gate_mut().set_range_db(-14.0);
        without.gate_mut().set_hold_ms(0.0);
        without.set_deesser_enabled(false);
        without.set_compressor_enabled(false);
        let mut block = rumble;
        without.process(&mut block, 1);

        assert!(
            without.gate().gain(0) > 0.99,
            "premise: unfiltered rumble should hold the gate open, it is at {}",
            without.gate().gain(0)
        );
        assert!(
            with_filter.gate().reduction_db(0) > 13.0,
            "the rumble reached the gate: only {} dB of reduction",
            with_filter.gate().reduction_db(0)
        );
    }

    #[test]
    fn the_gate_measures_the_microphone_and_not_the_presets_own_eq() {
        // Why the gate sits in front of the equalizer. A preset that lifts presence by two decibels
        // must not thereby move its own gate threshold by two decibels.
        let quiet = tone(2_519.0, db_to_linear(-46.0), 48_000);

        let mut flat = bare(FS);
        flat.set_gate_enabled(true);
        flat.gate_mut().set_hold_ms(0.0);
        let mut block = quiet.clone();
        flat.process(&mut block, 1);

        let mut lifted = bare(FS);
        lifted.set_gate_enabled(true);
        lifted.gate_mut().set_hold_ms(0.0);
        lifted.eq_mut().set_enabled(true);
        for band in 0..10 {
            lifted.eq_mut().set_band_boost(band, 0.0);
        }
        lifted.eq_mut().set_band_boost(6, 6.0);
        let mut block = quiet;
        lifted.process(&mut block, 1);

        let gap = (flat.gate().reduction_db(0) - lifted.gate().reduction_db(0)).abs();
        assert!(
            gap < 0.1,
            "six decibels of presence moved the gate by {gap} dB, so the EQ is in front of it"
        );
    }

    #[test]
    fn the_makeup_gain_is_behind_every_stage_that_measures() {
        // Twelve decibels of makeup must not move the compressor's own reduction: every threshold
        // in the preset set was voiced against the signal as it arrives.
        let voice = tone(300.0, db_to_linear(-24.0), 48_000);
        let (mut reductions, mut peaks) = (Vec::new(), Vec::new());
        for makeup_db in [0.0, 12.0] {
            let mut chain = bare(FS);
            chain.set_compressor_enabled(true);
            chain.compressor_mut().set_threshold_db(-30.0);
            chain.compressor_mut().set_ratio(4.0);
            chain.set_makeup_db(makeup_db);
            let mut block = voice.clone();
            chain.process(&mut block, 1);
            reductions.push(chain.compressor().reduction_db(0));
            peaks.push(block[24_000..].iter().fold(0.0_f32, |a, b| a.max(b.abs())));
        }
        assert!(
            reductions[0] > 1.0,
            "premise: the compressor should be working"
        );
        assert!(
            (reductions[0] - reductions[1]).abs() < 0.01,
            "makeup moved the compressor: {reductions:?}"
        );
        // And the other half of the claim: behind the compressor, not absent. Without this the
        // assertion above passes just as happily when the makeup gain does nothing at all — which
        // is exactly what the mutation run caught.
        let lift = linear_to_db(peaks[1]) - linear_to_db(peaks[0]);
        assert!(
            (lift - 12.0).abs() < 0.1,
            "twelve decibels of makeup produced {lift} dB"
        );
    }

    #[test]
    fn a_rate_that_cannot_carry_the_corners_reports_it_rather_than_moving_them() {
        // The narrowband case, end to end. At 16 kHz a 5500 Hz de-esser cannot be built; the
        // high-pass at 80 Hz still can, and says so.
        let mut chain = InputChain::new(16_000.0);
        chain.set_highpass(80.0, 4);
        assert_eq!(chain.highpass_sections(), 2);
        assert!(!chain.deesser().is_active());

        // And a corner that is nonsense at any rate takes the high-pass out rather than building
        // something else.
        chain.set_highpass(7_000.0, 2);
        assert_eq!(chain.highpass_sections(), 0);
    }

    #[test]
    fn a_format_change_clears_what_the_last_format_left_behind() {
        let mut chain = InputChain::new(FS);
        let mut block = tone(300.0, 0.5, 4_800);
        chain.process(&mut block, 1);
        assert!(chain.gate().gain(0) > 0.0);

        chain.set_sample_rate(44_100.0);
        assert_eq!(chain.sample_rate(), 44_100.0);
        // A silent block after the change must come out silent: nothing is left in a delay line.
        let mut block = vec![0.0; 4_800];
        chain.process(&mut block, 1);
        assert!(
            block.iter().all(|x| x.abs() < 1.0e-6),
            "something survived the format change"
        );
    }

    #[test]
    fn coming_back_from_a_bypass_does_not_play_the_last_sentence() {
        let mut chain = InputChain::new(FS);
        let mut block = tone(300.0, 0.8, 4_800);
        chain.process(&mut block, 1);

        chain.set_power(false);
        chain.set_power(true);
        let mut block = vec![0.0; 480];
        chain.process(&mut block, 1);
        assert!(
            block.iter().all(|x| x.abs() < 1.0e-6),
            "the delay line still had the last block in it"
        );
        silent(&mut chain, 0.1);
    }

    #[test]
    fn the_whole_chain_runs_on_every_channel_it_is_given() {
        for channels in 1..=MAX_CHANNELS {
            let mut chain = InputChain::new(FS);
            chain.set_makeup_db(6.0);
            let mut block = vec![0.0; 4_800 * channels];
            for (n, sample) in block.iter_mut().enumerate() {
                let frame = n / channels;
                *sample = (frame as Real * 0.05).sin() * db_to_linear(-12.0);
            }
            chain.process(&mut block, channels);
            for channel in 0..channels {
                let peak = block
                    .iter()
                    .skip(channel)
                    .step_by(channels)
                    .fold(0.0_f32, |a, b| a.max(b.abs()));
                assert!(
                    peak > db_to_linear(-20.0),
                    "channel {channel} of {channels} came out at {} dB",
                    linear_to_db(peak)
                );
            }
        }
    }
}
