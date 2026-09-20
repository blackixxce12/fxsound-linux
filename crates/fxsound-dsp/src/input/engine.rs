//! The microphone chain's engine: the sibling of [`crate::engine::Engine`].
//!
//! Same shape and the same contract — take a parameter snapshot, process a block in place, publish
//! meters — because the audio thread runs whichever of the two the active direction calls for and
//! must not care which. What differs is everything inside: the output engine's equalizer, leveller
//! and five effects, against this one's high-pass, gate, equalizer, de-esser, compressor and
//! limiter.
//!
//! Keeping them apart rather than adding a direction flag to one engine is the whole point: a
//! voice chain and a music chain share the ten-band equalizer and nothing else, and an engine that
//! had to decide per block which half of itself to skip would be one branch away from running the
//! wrong one.

use crate::input::InputChain;
use crate::input::detector::linear_to_db;
use crate::spectrum::SpectrumAnalyser;
use fxsound_core::messages::{DspEvent, InputDspParams, Meters};

/// The complete microphone chain, with its meters.
#[derive(Debug)]
pub struct InputEngine {
    chain: InputChain,
    spectrum: SpectrumAnalyser,

    sample_rate: f32,
    channels: usize,

    /// Cached so a snapshot that did not change a value does not force a redesign — and, more to
    /// the point, does not clear a filter's history and click.
    applied: InputDspParams,
    processed_samples: u64,
    peak_left: f32,
    peak_right: f32,
    active: bool,
}

impl InputEngine {
    /// Build an engine sized for the worst case it will be asked to handle.
    #[must_use]
    pub fn new(sample_rate: f32, max_block_frames: usize, channels: usize) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let max_block_frames = max_block_frames.clamp(1, crate::effects::MAX_BLOCK_FRAMES);
        let mut engine = Self {
            chain: InputChain::new(sample_rate),
            spectrum: SpectrumAnalyser::new(sample_rate, max_block_frames),
            sample_rate,
            channels: channels.clamp(1, crate::biquad::MAX_CHANNELS),
            applied: InputDspParams::default(),
            processed_samples: 0,
            peak_left: 0.0,
            peak_right: 0.0,
            active: false,
        };
        let params = engine.applied;
        engine.apply_unconditionally(&params);
        engine
    }

    pub fn set_format(&mut self, sample_rate: f32, channels: usize) {
        let sample_rate = sample_rate.max(1.0);
        let channels = channels.clamp(1, crate::biquad::MAX_CHANNELS);
        if sample_rate == self.sample_rate && channels == self.channels {
            return;
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.chain.set_sample_rate(sample_rate);
        self.spectrum.set_sample_rate(sample_rate);
        self.reset();
    }

    #[must_use]
    pub const fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Adopt a parameter snapshot, skipping anything that has not changed.
    pub fn apply(&mut self, params: &InputDspParams) {
        if *params == self.applied {
            return;
        }
        self.apply_unconditionally(params);
    }

    fn apply_unconditionally(&mut self, params: &InputDspParams) {
        self.chain.set_power(params.power);
        self.chain
            .set_highpass(params.highpass_hz, usize::from(params.highpass_order));

        self.chain.set_denoise_enabled(params.rnnoise);
        self.chain.set_gate_enabled(params.gate_on);
        let gate = self.chain.gate_mut();
        gate.set_threshold_db(params.gate_threshold_db);
        gate.set_ratio(params.gate_ratio);
        gate.set_range_db(params.gate_range_db);
        gate.set_times(params.gate_attack_ms, params.gate_release_ms);
        gate.set_hold_ms(params.gate_hold_ms);
        gate.set_detection(params.gate_detection);

        let eq = self.chain.eq_mut();
        eq.set_enabled(params.eq_on);
        eq.set_q_multiplier(params.filter_q);
        let (centers, boosts) = params.bands();
        if centers != eq.center_frequencies() || boosts != eq.boosts_db() {
            eq.set_bands(centers, boosts);
        }

        self.chain.set_deesser_enabled(params.deesser_on);
        let deesser = self.chain.deesser_mut();
        deesser.set_frequency(params.deesser_hz);
        deesser.set_threshold_db(params.deesser_threshold_db);

        self.chain.set_compressor_enabled(params.compressor_on);
        let compressor = self.chain.compressor_mut();
        compressor.set_threshold_db(params.compressor_threshold_db);
        compressor.set_ratio(params.compressor_ratio);
        compressor.set_knee_db(params.compressor_knee_db);
        compressor.set_times(params.compressor_attack_ms, params.compressor_release_ms);
        compressor.set_detection(params.compressor_detection);

        self.chain.set_makeup_db(params.makeup_db);
        self.chain.set_ceiling_db(params.ceiling_db);

        self.applied = *params;
    }

    /// Act on a one-shot event. The same three the output engine knows, so the control path does
    /// not have to branch on direction.
    pub fn handle_event(&mut self, event: DspEvent) {
        match event {
            DspEvent::ResetFilterState => self.chain.reset(),
            DspEvent::ResetSpectrum => self.spectrum.reset(),
            DspEvent::ResetProcessedTime => self.processed_samples = 0,
        }
    }

    /// Clear every filter's history.
    pub fn reset(&mut self) {
        self.chain.reset();
        self.spectrum.reset();
        self.peak_left = 0.0;
        self.peak_right = 0.0;
    }

    /// Latency the chain adds, in frames. Only the limiter's look-ahead contributes.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        self.chain.latency_frames()
    }

    /// Whether the de-esser could be built at the current capture rate.
    ///
    /// `false` means it is passing the signal through: a 5500 Hz split needs about 18 kHz of
    /// sample rate, and a Bluetooth headset at 16 kHz does not have it. Worth surfacing rather
    /// than leaving as a preset that quietly does nothing.
    #[must_use]
    pub const fn deesser_running(&self) -> bool {
        self.chain.deesser().is_active()
    }

    /// Whether the denoiser is actually processing.
    ///
    /// `false` while it is switched off, and `false` at any capture rate other than 48 kHz —
    /// RNNoise exists at that rate and nowhere else. Worth surfacing for the same reason as the
    /// de-esser: a preset that asks for a stage the device cannot run should say so rather than
    /// sound different without explanation.
    #[must_use]
    pub fn denoiser_running(&self) -> bool {
        self.chain.denoiser().is_active()
    }

    /// The denoiser's opinion of whether the last frame was voice, `0.0..=1.0`.
    #[must_use]
    pub fn voice_probability(&self) -> f32 {
        self.chain.denoiser().voice_probability()
    }

    /// Process one interleaved block in place.
    ///
    /// Real-time safe: no allocation, no locks, no IO, no panicking paths.
    pub fn process(&mut self, buffer: &mut [f32], channels: usize) {
        let channels = channels.clamp(1, crate::biquad::MAX_CHANNELS);
        if buffer.is_empty() {
            return;
        }

        // The same reasoning as the output engine, and more pressing here: a capture stream is
        // whatever a driver handed the server, and every stage below has infinite memory — a NaN
        // in the crossover's state reproduces itself forever, and one `+inf` through the limiter's
        // delay line silences the microphone for the session.
        for sample in buffer.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }

        self.chain.process(buffer, channels);

        // A block that went in finite can still come out non-finite if a stage's own state has
        // blown up. Hand silence to whoever is listening rather than a NaN, and clear the history
        // so the next block starts clean. Before the spectrum tap, so the meters never show it.
        if buffer.iter().any(|sample| !sample.is_finite()) {
            buffer.fill(0.0);
            self.reset();
        }

        self.spectrum.push(buffer, channels);
        self.measure(buffer, channels);
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
            // A microphone is usually mono, and a meter with one dead side reads as a fault.
            peak_right = peak_left;
        }

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
            // Channel zero: a stereo microphone's two sides are the same voice, and a meter that
            // showed the larger of the two would read as gain reduction the user cannot explain.
            gate_reduction_db: self.chain.gate().reduction_db(0),
            compressor_reduction_db: self.chain.compressor().reduction_db(0),
            deesser_reduction_db: self.chain.deesser().reduction_db(0),
            deesser_running: self.chain.deesser().is_active(),
            denoiser_running: self.chain.denoiser().is_active(),
            voice_probability: self.chain.denoiser().voice_probability(),
        }
    }

    /// The equalizer, for drawing its response curve.
    #[must_use]
    pub const fn equalizer(&self) -> &crate::eq::GraphicEq {
        self.chain.eq()
    }

    /// The chain itself, for a caller that needs to read a stage's state.
    #[must_use]
    pub const fn chain(&self) -> &InputChain {
        &self.chain
    }
}

/// Decibels of gain reduction, as a positive number, from a linear gain. Re-exported for a caller
/// building its own meter from [`InputEngine::chain`].
#[must_use]
pub fn reduction_db(gain: f32) -> f32 {
    -linear_to_db(gain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::Detection;

    const FS: f32 = 48_000.0;

    fn voice(level_db: f32, frames: usize) -> Vec<f32> {
        let amplitude = 10.0_f32.powf(level_db / 20.0);
        (0..frames)
            .map(|n| (n as f32 * std::f32::consts::TAU * 220.0 / FS).sin() * amplitude)
            .collect()
    }

    #[test]
    fn the_snapshot_reaches_every_stage() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut params = InputDspParams {
            gate_on: true,
            gate_threshold_db: -20.0,
            gate_ratio: 2.0,
            gate_range_db: -12.0,
            gate_hold_ms: 0.0,
            compressor_on: true,
            compressor_threshold_db: -50.0,
            compressor_ratio: 4.0,
            deesser_on: true,
            makeup_db: 0.0,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);

        // Quiet enough to be under the gate's threshold and loud enough to be over the
        // compressor's, so both stages must be working for this to hold.
        let mut block = voice(-30.0, 48_000);
        engine.process(&mut block, 1);
        let meters = engine.meters();
        assert!(
            meters.gate_reduction_db > 5.0,
            "the gate never saw its threshold: {}",
            meters.gate_reduction_db
        );
        assert!(
            meters.compressor_reduction_db > 1.0,
            "the compressor never saw its threshold: {}",
            meters.compressor_reduction_db
        );
    }

    #[test]
    fn moving_one_control_does_not_rebuild_the_high_pass() {
        // A parameter snapshot arrives whenever *anything* moves. Rebuilding a filter clears its
        // history, which is a click on a live microphone — so a corner that did not change must
        // not be redesigned because a threshold did.
        //
        // Two engines rather than one, fed the same continuing signal: the first draft of this
        // test pushed the same 480-sample tone through one engine twice and compared the results,
        // which measured the phase discontinuity it had just created in its own input.
        let settings = {
            let mut params = InputDspParams {
                highpass_hz: 80.0,
                highpass_order: 4,
                gate_on: false,
                deesser_on: false,
                compressor_on: false,
                eq_on: false,
                makeup_db: 0.0,
                ..InputDspParams::default()
            };
            params.sanitise();
            params
        };
        let mut untouched = InputEngine::new(FS, 1_024, 1);
        let mut moved = InputEngine::new(FS, 1_024, 1);
        untouched.apply(&settings);
        moved.apply(&settings);

        // Below the corner, where the filter is working hardest and its history matters most.
        let warm: Vec<f32> = (0..4_800)
            .map(|n| (n as f32 * std::f32::consts::TAU * 40.0 / FS).sin() * 0.5)
            .collect();
        untouched.process(&mut warm.clone(), 1);
        moved.process(&mut warm.clone(), 1);

        // Move something the high-pass has nothing to do with.
        let mut elsewhere = settings;
        elsewhere.gate_threshold_db = -33.0;
        moved.apply(&elsewhere);

        let next: Vec<f32> = (4_800..5_280)
            .map(|n| (n as f32 * std::f32::consts::TAU * 40.0 / FS).sin() * 0.5)
            .collect();
        let (mut a, mut b) = (next.clone(), next);
        untouched.process(&mut a, 1);
        moved.process(&mut b, 1);

        let jump = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            jump < 1.0e-6,
            "the high-pass was rebuilt and the signal jumped by {jump}"
        );
    }

    #[test]
    fn a_non_finite_sample_never_leaves_the_engine() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut block = vec![f32::INFINITY; 480];
        engine.process(&mut block, 1);
        assert!(block.iter().all(|x| x.is_finite()));

        let mut block = voice(-20.0, 48_000);
        engine.process(&mut block, 1);
        assert!(
            block.iter().all(|x| x.is_finite()),
            "the engine did not recover"
        );
        assert!(
            block.iter().any(|x| x.abs() > 1.0e-3),
            "the engine recovered into silence"
        );
    }

    #[test]
    fn one_bad_sample_costs_one_sample_and_not_the_block() {
        // There are two guards, and the mutation run showed why both are needed: the output check
        // alone already keeps the engine from *shipping* a NaN, so removing the input check left
        // every test green. What it changes is the cost. Sanitised on the way in, a single bad
        // sample is one silent sample. Left alone, it poisons a stage, the output check fires, and
        // the whole block becomes silence plus a full reset — a dropout instead of a click.
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut params = InputDspParams {
            gate_on: false,
            deesser_on: false,
            compressor_on: false,
            eq_on: false,
            makeup_db: 0.0,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);

        // Past the limiter's look-ahead, so most of this block is audio that reached the output.
        let mut block = voice(-12.0, 4_800);
        block[10] = f32::INFINITY;
        engine.process(&mut block, 1);

        assert!(block.iter().all(|x| x.is_finite()));
        let peak = block.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
        assert!(
            peak > 0.1,
            "one bad sample cost the whole block: peak {peak}"
        );
    }

    #[test]
    fn the_detector_mode_is_carried_by_the_snapshot() {
        // The field exists so that a preset means one thing. If it did not reach the stage, these
        // two would land in the same place.
        let mut reductions = Vec::new();
        for mode in [Detection::Peak, Detection::Rms] {
            let mut engine = InputEngine::new(FS, 1_024, 1);
            let mut params = InputDspParams {
                gate_on: false,
                deesser_on: false,
                eq_on: false,
                makeup_db: 0.0,
                compressor_on: true,
                compressor_threshold_db: -20.0,
                compressor_ratio: 4.0,
                compressor_knee_db: 0.0,
                compressor_attack_ms: 0.0,
                compressor_release_ms: 0.0,
                compressor_detection: mode,
                ..InputDspParams::default()
            };
            params.sanitise();
            engine.apply(&params);

            let mut block: Vec<f32> = (0..48_000)
                .map(|n| if n % 240 == 0 { 0.9 } else { 0.01 })
                .collect();
            engine.process(&mut block, 1);
            let mut spike = [0.9_f32];
            engine.process(&mut spike, 1);
            reductions.push(engine.meters().compressor_reduction_db);
        }
        let gap = reductions[0] - reductions[1];
        assert!(gap.abs() > 3.0, "the mode did not reach the stage: {gap}");
    }

    #[test]
    fn switching_the_denoiser_on_changes_the_latency_and_says_so() {
        // Ten milliseconds is far too much to add silently: a recording application told the old
        // figure drifts out of lip sync by exactly that much, and nobody can diagnose it from the
        // outside. The engine's number has to move, which is what lets the supervisor republish.
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let quiet = engine.latency_frames();
        assert_eq!(quiet, 48, "the limiter's millisecond, and nothing else");

        let mut params = InputDspParams {
            rnnoise: true,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);
        assert!(engine.denoiser_running());
        assert_eq!(
            engine.latency_frames(),
            quiet + 480,
            "RNNoise's whole frame, on top of the limiter's look-ahead"
        );

        // And at a rate RNNoise cannot work at, the stage stands aside and takes its latency with
        // it — the chain is still correct, it is just not denoised.
        engine.set_format(44_100.0, 1);
        assert!(!engine.denoiser_running());
        assert_eq!(engine.latency_frames(), 44, "1 ms at 44.1 kHz, no denoiser");
    }

    #[test]
    fn the_denoiser_runs_before_everything_that_measures_a_level() {
        // Position is the whole argument for putting it first: the gate, the compressor and the
        // de-esser all measure a level, and a denoised level is a different number. With a room
        // floor on the input, the gate sees less of it — which is why every gate threshold in the
        // preset draft has to be re-voiced now that this stage exists.
        let floor: Vec<f32> = {
            let mut state = 0x2545_f491_4f6c_dd1d_u64;
            (0..48_000)
                .map(|n| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let hiss = ((state >> 40) as f32 / 8_388_608.0 - 1.0) * 0.02;
                    hiss + (n as f32 * std::f32::consts::TAU * 50.0 / FS).sin() * 0.05
                })
                .collect()
        };

        let reduction = |rnnoise: bool| {
            let mut engine = InputEngine::new(FS, 1_024, 1);
            let mut params = InputDspParams {
                rnnoise,
                gate_on: true,
                gate_threshold_db: -30.0,
                gate_ratio: 2.0,
                gate_range_db: -20.0,
                gate_hold_ms: 0.0,
                compressor_on: false,
                deesser_on: false,
                eq_on: false,
                highpass_order: 0,
                makeup_db: 0.0,
                ..InputDspParams::default()
            };
            params.sanitise();
            engine.apply(&params);
            let mut block = floor.clone();
            engine.process(&mut block, 1);
            engine.meters().gate_reduction_db
        };

        let plain = reduction(false);
        let denoised = reduction(true);
        assert!(
            denoised > plain + 3.0,
            "the gate saw the same floor either way: {plain} against {denoised}"
        );
    }

    #[test]
    fn a_rate_that_cannot_carry_the_de_esser_says_so() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        assert!(engine.deesser_running());
        engine.set_format(16_000.0, 1);
        assert!(
            !engine.deesser_running(),
            "a 5500 Hz split cannot be built at 16 kHz"
        );
    }

    #[test]
    fn the_latency_is_the_limiters_look_ahead_and_survives_a_format_change() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        assert_eq!(engine.latency_frames(), 48, "1 ms at 48 kHz");
        engine.set_format(96_000.0, 2);
        assert_eq!(engine.latency_frames(), 96);
    }

    #[test]
    fn a_bypassed_chain_passes_the_capture_through() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut params = InputDspParams {
            power: false,
            makeup_db: 12.0,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);
        let input = voice(-12.0, 4_800);
        let mut block = input.clone();
        engine.process(&mut block, 1);
        assert_eq!(block, input);
    }
}
