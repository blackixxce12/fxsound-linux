//! The microphone chain's engine: the sibling of [`crate::engine::Engine`].
//!
//! Same shape and the same contract — take a parameter snapshot, process a block in place, publish
//! meters — because the audio thread runs each of the two lanes' engines the same way and must
//! not care which. What differs is everything inside: the output engine's equalizer, leveller
//! and five effects, against this one's chain of stages built from a [`ChainSpec`].
//!
//! Keeping them apart rather than adding a direction flag to one engine is the whole point: a
//! voice chain and a music chain share the ten-band equalizer and nothing else, and an engine that
//! had to decide per block which half of itself to skip would be one branch away from running the
//! wrong one.
//!
//! **The pre-chain tap.** Before the chain runs, the engine measures the microphone as it
//! arrives: a peak that holds and decays, a ten-millisecond RMS, a running noise floor, and four
//! cumulative counters the calibration wizard differences between phases. The tap sits before
//! the chain because the wizard wants the microphone, not the preset.

use crate::input::InputChain;
use crate::input::detector::linear_to_db;
use crate::input::processor::{ChainSpec, StageKind};
use crate::spectrum::SpectrumAnalyser;
use fxsound_core::messages::{DspEvent, InputDspParams, Meters};

/// The noise floor rises at this rate when the signal stays above it, in dB per second. Slow,
/// so that speech never lifts it; a microphone whose floor genuinely rose is caught up with in
/// a few seconds.
const FLOOR_RISE_DB_PER_SECOND: f32 = 0.5;
/// The floor cannot fall below this: a muted microphone reports a floor, not `-inf`.
const FLOOR_MIN_DB: f32 = -100.0;
/// How fast the held input peak decays, per block.
const PEAK_DECAY: f32 = 0.85;
/// A sample at or above this counts as clipped: full scale less a hair, because a driver that
/// clips hands over `0.99997` as often as `1.0`.
const CLIP_LEVEL: f32 = 0.999;

/// The complete microphone chain, with its meters.
#[derive(Debug)]
pub struct InputEngine {
    chain: InputChain,
    spectrum: SpectrumAnalyser,

    sample_rate: f32,
    channels: usize,
    /// What the audio crate learned about the source's own rate; kept so a rebuilt chain is
    /// told again.
    source_rate: Option<f32>,

    /// Cached so a snapshot that did not change a value does not force a redesign — and, more to
    /// the point, does not clear a filter's history and click.
    applied: InputDspParams,
    processed_samples: u64,
    peak_left: f32,
    peak_right: f32,
    active: bool,

    // ---- the pre-chain tap ----
    input_peak: f32,
    input_rms_db: f32,
    noise_floor_db: f32,
    /// The ten-millisecond block the floor and the RMS are measured over.
    floor_block_frames: usize,
    floor_block_sum: f64,
    floor_block_count: usize,
    capture_frames: u64,
    capture_sum_squares: f64,
    capture_peak: f32,
    capture_clipped: u64,
}

impl InputEngine {
    /// Build the voice chain, sized for the worst case it will be asked to handle.
    #[must_use]
    pub fn new(sample_rate: f32, max_block_frames: usize, channels: usize) -> Self {
        Self::new_with_spec(sample_rate, max_block_frames, channels, ChainSpec::voice())
    }

    /// Build the chain a spec describes. Allocates; this is the main loop's call.
    #[must_use]
    pub fn new_with_spec(
        sample_rate: f32,
        max_block_frames: usize,
        channels: usize,
        spec: ChainSpec,
    ) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let max_block_frames = max_block_frames.clamp(1, crate::effects::MAX_BLOCK_FRAMES);
        let mut engine = Self {
            chain: InputChain::from_spec(spec, sample_rate),
            spectrum: SpectrumAnalyser::new(sample_rate, max_block_frames),
            sample_rate,
            channels: channels.clamp(1, crate::biquad::MAX_CHANNELS),
            source_rate: None,
            applied: InputDspParams::default(),
            processed_samples: 0,
            peak_left: 0.0,
            peak_right: 0.0,
            active: false,
            input_peak: 0.0,
            input_rms_db: FLOOR_MIN_DB,
            noise_floor_db: 0.0,
            floor_block_frames: floor_block_frames(sample_rate),
            floor_block_sum: 0.0,
            floor_block_count: 0,
            capture_frames: 0,
            capture_sum_squares: 0.0,
            capture_peak: 0.0,
            capture_clipped: 0,
        };
        let params = engine.applied;
        engine.chain.apply(&params);
        engine
    }

    /// Replace the chain with one built from another spec, keeping every parameter and the
    /// source rate.
    ///
    /// **Main loop only.** This allocates the new stages and drops the old ones — eight network
    /// states among them — which is exactly what the audio thread must never do. The audio crate
    /// calls it from its supervisor when a preset names a different chain, and hands the engine
    /// to the process callback afterwards.
    pub fn set_spec(&mut self, spec: ChainSpec) {
        if spec == self.chain.spec() {
            return;
        }
        self.chain = InputChain::from_spec(spec, self.sample_rate);
        let params = self.applied;
        self.chain.apply(&params);
        if let Some(deesser) = self.chain.deesser_mut() {
            deesser.set_source_rate(self.source_rate);
        }
        self.spectrum.reset();
        self.peak_left = 0.0;
        self.peak_right = 0.0;
    }

    #[must_use]
    pub const fn spec(&self) -> ChainSpec {
        self.chain.spec()
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
        self.floor_block_frames = floor_block_frames(sample_rate);
        self.floor_block_sum = 0.0;
        self.floor_block_count = 0;
        self.reset();
    }

    #[must_use]
    pub const fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// The channel count the chain was last told to run at.
    ///
    /// With `sample_rate`, what a replacement engine has to be told before it can take over
    /// from this one: the audio crate builds the replacement on its main loop from the format it
    /// last negotiated, and the outgoing engine is the authority on what that format is now.
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }

    /// What the audio crate knows about the source's own rate — `audio.rate` when the device
    /// publishes it, 16 kHz for a Bluetooth headset profile, `None` when the stream rate is all
    /// there is to know. The adaptive de-esser places its corner from it.
    pub fn set_source_rate(&mut self, rate: Option<f32>) {
        self.source_rate = rate;
        if let Some(deesser) = self.chain.deesser_mut() {
            deesser.set_source_rate(rate);
        }
    }

    #[must_use]
    pub const fn source_rate(&self) -> Option<f32> {
        self.source_rate
    }

    /// Adopt a parameter snapshot, skipping anything that has not changed.
    pub fn apply(&mut self, params: &InputDspParams) {
        if *params == self.applied {
            return;
        }
        self.chain.apply(params);
        self.applied = *params;
    }

    /// Act on a one-shot event. The same events the output engine knows, so the control path
    /// does not have to branch on direction.
    pub fn handle_event(&mut self, event: DspEvent) {
        match event {
            DspEvent::ResetFilterState => self.chain.reset(),
            DspEvent::ResetSpectrum => self.spectrum.reset(),
            DspEvent::ResetProcessedTime => self.processed_samples = 0,
            DspEvent::ResetCaptureStats => self.reset_capture_stats(),
        }
    }

    /// Zero the four cumulative counters. The wizard sends this on entering each phase; the
    /// floor and the peak are left alone, because they describe the microphone and not a phase.
    pub fn reset_capture_stats(&mut self) {
        self.capture_frames = 0;
        self.capture_sum_squares = 0.0;
        self.capture_peak = 0.0;
        self.capture_clipped = 0;
    }

    /// Clear every filter's history.
    pub fn reset(&mut self) {
        self.chain.reset();
        self.spectrum.reset();
        self.peak_left = 0.0;
        self.peak_right = 0.0;
    }

    /// Latency the chain adds, in frames: the limiter's look-ahead, plus what the denoiser and
    /// the de-reverb add while they run.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        self.chain.latency_frames()
    }

    /// Whether the de-esser is running: switched on, and buildable at the current capture rate.
    ///
    /// `false` with it switched on means it is passing the signal through: a 5500 Hz split needs
    /// about 18 kHz of sample rate, and a Bluetooth headset at 16 kHz does not have it. Worth
    /// surfacing rather than leaving as a preset that quietly does nothing.
    #[must_use]
    pub fn deesser_running(&self) -> bool {
        self.chain.meter(StageKind::DeEsser).running
    }

    /// Whether the denoiser is actually processing.
    ///
    /// `false` while it is switched off or at `Off`, and `false` at any capture rate other than
    /// 48 kHz — RNNoise exists at that rate and nowhere else. Worth surfacing for the same reason
    /// as the de-esser: a preset that asks for a stage the device cannot run should say so rather
    /// than sound different without explanation.
    #[must_use]
    pub fn denoiser_running(&self) -> bool {
        self.chain.meter(StageKind::Denoise).running
    }

    /// The denoiser's opinion of whether the last frame was voice, `0.0..=1.0`.
    #[must_use]
    pub fn voice_probability(&self) -> f32 {
        self.chain.meter(StageKind::Denoise).aux
    }

    /// The running noise floor of the microphone, dBFS.
    #[must_use]
    pub const fn noise_floor_db(&self) -> f32 {
        self.noise_floor_db
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

        self.tap(buffer, channels);
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

    /// The pre-chain statistics: what the microphone is doing before the preset touches it.
    fn tap(&mut self, buffer: &[f32], channels: usize) {
        let mut peak = 0.0_f32;
        let mut clipped = 0_u64;
        let mut frames = 0_u64;
        for frame in buffer.chunks_exact(channels) {
            let mut sum_sq = 0.0_f32;
            for &x in frame {
                let magnitude = x.abs();
                sum_sq += x * x;
                peak = peak.max(magnitude);
                if magnitude >= CLIP_LEVEL {
                    clipped += 1;
                }
            }
            // The mean square over the channels, so the counters read as a mono signal's whatever
            // the channel count: `sqrt(Δsum / Δframes)` is the RMS.
            let mean_sq = f64::from(sum_sq) / channels as f64;
            self.capture_sum_squares += mean_sq;
            frames += 1;

            self.floor_block_sum += mean_sq;
            self.floor_block_count += 1;
            if self.floor_block_count >= self.floor_block_frames {
                self.close_floor_block();
            }
        }
        self.capture_frames = self.capture_frames.saturating_add(frames);
        self.capture_clipped = self.capture_clipped.saturating_add(clipped);
        self.capture_peak = self.capture_peak.max(peak);
        self.input_peak = peak.max(self.input_peak * PEAK_DECAY);
        if !self.capture_sum_squares.is_finite() {
            self.capture_sum_squares = 0.0;
        }
    }

    /// One ten-millisecond block is in: its RMS is the short-window level, and the floor
    /// follows it down at once or creeps up toward it.
    fn close_floor_block(&mut self) {
        let mean_sq = self.floor_block_sum / self.floor_block_count.max(1) as f64;
        self.floor_block_sum = 0.0;
        self.floor_block_count = 0;
        let rms_db = if mean_sq > 0.0 && mean_sq.is_finite() {
            (10.0 * mean_sq.log10()) as f32
        } else {
            FLOOR_MIN_DB
        }
        .clamp(FLOOR_MIN_DB, 0.0);
        self.input_rms_db = rms_db;

        if rms_db < self.noise_floor_db {
            self.noise_floor_db = rms_db;
        } else {
            let seconds = self.floor_block_frames as f32 / self.sample_rate;
            self.noise_floor_db =
                (self.noise_floor_db + FLOOR_RISE_DB_PER_SECOND * seconds).min(rms_db);
        }
        if !self.noise_floor_db.is_finite() {
            self.noise_floor_db = FLOOR_MIN_DB;
        }
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

        self.peak_left = peak_left.max(self.peak_left * PEAK_DECAY);
        self.peak_right = peak_right.max(self.peak_right * PEAK_DECAY);
        self.active = peak_left > 1e-6 || peak_right > 1e-6;
    }

    /// What the GUI should draw. Cheap enough to call every buffer.
    #[must_use]
    pub fn meters(&self) -> Meters {
        let denoise = self.chain.meter(StageKind::Denoise);
        let dereverb = self.chain.meter(StageKind::Dereverb);
        let deesser = self.chain.meter(StageKind::DeEsser);
        Meters {
            spectrum: self.spectrum.bands(),
            peak_left: self.peak_left.min(1.0),
            peak_right: self.peak_right.min(1.0),
            processed_samples: self.processed_samples,
            sample_rate: self.sample_rate as u32,
            active: self.active,
            // Channel zero: a stereo microphone's two sides are the same voice, and a meter that
            // showed the larger of the two would read as gain reduction the user cannot explain.
            gate_reduction_db: self.chain.meter(StageKind::Gate).reduction_db,
            compressor_reduction_db: self.chain.meter(StageKind::Compressor).reduction_db,
            deesser_reduction_db: deesser.reduction_db,
            deesser_running: deesser.running,
            denoiser_running: denoise.running,
            voice_probability: denoise.aux,
            input_peak: self.input_peak.min(1.0),
            input_rms_db: self.input_rms_db,
            noise_floor_db: self.noise_floor_db,
            denoise_reduction_db: denoise.reduction_db,
            deesser_hz: deesser.aux,
            dereverb_reduction_db: dereverb.reduction_db,
            latency_frames: u32::try_from(self.chain.latency_frames()).unwrap_or(u32::MAX),
            capture_frames: self.capture_frames,
            capture_sum_squares: self.capture_sum_squares,
            capture_peak: self.capture_peak,
            capture_clipped: self.capture_clipped,
        }
    }

    /// The equalizer, for drawing its response curve. `None` for a chain without one.
    #[must_use]
    pub fn equalizer(&self) -> Option<&crate::eq::GraphicEq> {
        self.chain.eq()
    }

    /// The chain itself, for a caller that needs to read a stage's state.
    #[must_use]
    pub const fn chain(&self) -> &InputChain {
        &self.chain
    }
}

/// Ten milliseconds at a rate, at least one frame.
fn floor_block_frames(sample_rate: f32) -> usize {
    ((sample_rate / 100.0) as usize).max(1)
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
    use fxsound_core::{DeEsserMode, Detection};

    const FS: f32 = 48_000.0;

    fn voice(level_db: f32, frames: usize) -> Vec<f32> {
        let amplitude = 10.0_f32.powf(level_db / 20.0);
        (0..frames)
            .map(|n| (n as f32 * std::f32::consts::TAU * 220.0 / FS).sin() * amplitude)
            .collect()
    }

    /// Deterministic noise at an RMS level in dBFS.
    fn noise(level_db: f32, frames: usize) -> Vec<f32> {
        let amplitude = 10.0_f32.powf(level_db / 20.0) * 3.0_f32.sqrt();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..frames)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as f32 / 8_388_608.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn run(engine: &mut InputEngine, signal: &[f32], channels: usize) {
        let mut signal = signal.to_vec();
        for block in signal.chunks_mut(1_024 * channels) {
            engine.process(block, channels);
        }
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
        // Twenty milliseconds is far too much to add silently: a recording application told the
        // old figure drifts out of lip sync by exactly that much, and nobody can diagnose it from
        // the outside. The engine's number has to move, which is what lets the supervisor
        // republish — and it has to be the true figure, which 0.3.0's 480 was not.
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let quiet = engine.latency_frames();
        assert_eq!(quiet, 48, "the limiter's millisecond, and nothing else");
        assert_eq!(engine.meters().latency_frames, 48);

        let mut params = InputDspParams {
            rnnoise: true,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);
        assert!(engine.denoiser_running());
        assert_eq!(
            engine.latency_frames(),
            quiet + 960,
            "RNNoise's bridge and its own frame, on top of the limiter's look-ahead"
        );
        assert_eq!(engine.meters().latency_frames, 48 + 960);

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
        assert_eq!(engine.meters().deesser_hz, 5_500.0);
        engine.set_format(16_000.0, 1);
        assert!(
            !engine.deesser_running(),
            "a 5500 Hz split cannot be built at 16 kHz"
        );
        assert_eq!(engine.meters().deesser_hz, 0.0);

        // The adaptive mode builds one at 4 kHz, and the meter says where.
        let mut params = InputDspParams {
            deesser_mode: DeEsserMode::Adaptive,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);
        assert!(engine.deesser_running());
        assert_eq!(engine.meters().deesser_hz, 4_000.0);
    }

    #[test]
    fn the_source_rate_reaches_the_de_esser_and_survives_a_new_spec() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut params = InputDspParams {
            deesser_mode: DeEsserMode::Adaptive,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);
        engine.set_source_rate(Some(16_000.0));
        assert_eq!(engine.meters().deesser_hz, 4_000.0);

        engine.set_spec(ChainSpec::streaming());
        assert_eq!(engine.spec(), ChainSpec::streaming());
        assert_eq!(engine.source_rate(), Some(16_000.0));
        assert_eq!(
            engine.meters().deesser_hz,
            4_000.0,
            "the rebuilt chain was not told the source rate"
        );
        assert_eq!(
            engine.chain().deesser().expect("de-esser").mode(),
            DeEsserMode::Adaptive,
            "nor the parameters"
        );

        engine.set_source_rate(None);
        assert_eq!(engine.meters().deesser_hz, 5_500.0);
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

    #[test]
    fn the_capture_counters_measure_the_microphone_before_the_preset() {
        // The wizard's arithmetic: three seconds of a −48 dBFS floor, then five of a −18 dBFS
        // talker. The floor tracker reads the first; differencing the cumulative counters over
        // the second reads the talker — through a chain that is gating, compressing and lifting
        // the signal, none of which the tap may see.
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut params = InputDspParams {
            makeup_db: 12.0,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);

        engine.handle_event(DspEvent::ResetCaptureStats);
        run(&mut engine, &noise(-48.0, FS as usize * 3), 1);
        let after_floor = engine.meters();
        assert!(
            (after_floor.noise_floor_db + 48.0).abs() < 1.0,
            "the floor reads {} dB",
            after_floor.noise_floor_db
        );
        let floor_rms = 10.0
            * (after_floor.capture_sum_squares / after_floor.capture_frames as f64).log10() as f32;
        assert!(
            (floor_rms + 48.0).abs() < 0.5,
            "the first window's RMS reads {floor_rms} dB"
        );
        assert_eq!(after_floor.capture_frames, FS as u64 * 3);
        assert_eq!(after_floor.capture_clipped, 0);

        engine.handle_event(DspEvent::ResetCaptureStats);
        let tone = voice(-18.0 + 3.0103, FS as usize * 5); // a sine's RMS is 3 dB under its peak
        run(&mut engine, &tone, 1);
        let after_talker = engine.meters();
        let talker_rms = 10.0
            * (after_talker.capture_sum_squares / after_talker.capture_frames as f64).log10()
                as f32;
        assert!(
            (talker_rms + 18.0).abs() < 0.5,
            "the second window's RMS reads {talker_rms} dB"
        );
        assert_eq!(after_talker.capture_frames, FS as u64 * 5);
        assert!(
            (after_talker.capture_peak - 10.0_f32.powf((-18.0 + 3.0103) / 20.0)).abs() < 0.01,
            "the peak reads {}",
            after_talker.capture_peak
        );
        assert!(
            after_talker.noise_floor_db > -48.0 && after_talker.noise_floor_db < -44.0,
            "five seconds of talker should lift the floor by about two and a half decibels, it \
             reads {}",
            after_talker.noise_floor_db
        );
        assert!(
            (after_talker.input_rms_db + 18.0).abs() < 1.0,
            "the short-window RMS reads {}",
            after_talker.input_rms_db
        );
        assert!(after_talker.input_peak > 0.9 * after_talker.capture_peak);
    }

    #[test]
    fn a_clipped_block_counts_its_clipped_samples_and_a_reset_zeroes_everything() {
        let mut engine = InputEngine::new(FS, 1_024, 2);
        let mut block = vec![0.1; 2_000];
        block[10] = 1.0;
        block[11] = -1.0;
        block[500] = 0.999;
        block[501] = 0.998;
        engine.process(&mut block, 2);
        let meters = engine.meters();
        assert_eq!(
            meters.capture_clipped, 3,
            "two full-scale samples and one at 0.999"
        );
        assert_eq!(meters.capture_frames, 1_000);
        assert_eq!(meters.capture_peak, 1.0);
        assert!(meters.capture_sum_squares > 0.0);

        engine.handle_event(DspEvent::ResetCaptureStats);
        let meters = engine.meters();
        assert_eq!(meters.capture_clipped, 0);
        assert_eq!(meters.capture_frames, 0);
        assert_eq!(meters.capture_sum_squares, 0.0);
        assert_eq!(meters.capture_peak, 0.0);
    }

    #[test]
    fn a_muted_microphone_reports_a_floor_and_not_infinity() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        run(&mut engine, &vec![0.0; FS as usize], 1);
        let meters = engine.meters();
        assert_eq!(meters.noise_floor_db, FLOOR_MIN_DB);
        assert_eq!(meters.input_rms_db, FLOOR_MIN_DB);
        assert_eq!(meters.input_peak, 0.0);
    }

    #[test]
    fn the_meters_carry_the_stages_own_readings() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut params = InputDspParams {
            rnnoise: true,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);
        let floor: Vec<f32> = noise(-40.0, FS as usize)
            .iter()
            .enumerate()
            .map(|(n, x)| x + (n as f32 * std::f32::consts::TAU * 50.0 / FS).sin() * 0.03)
            .collect();
        run(&mut engine, &floor, 1);
        let meters = engine.meters();
        assert!(meters.denoiser_running);
        assert!(
            meters.denoise_reduction_db > 3.0,
            "the denoiser's meter reads {}",
            meters.denoise_reduction_db
        );
        assert_eq!(meters.dereverb_reduction_db, 0.0, "the de-reverb is off");
        assert_eq!(meters.latency_frames, 48 + 960);
    }

    #[test]
    fn a_new_spec_keeps_the_parameters_and_is_the_new_order() {
        let mut engine = InputEngine::new(FS, 1_024, 1);
        let mut params = InputDspParams {
            makeup_db: 9.0,
            gate_threshold_db: -30.0,
            ..InputDspParams::default()
        };
        params.sanitise();
        engine.apply(&params);
        engine.set_spec(ChainSpec::podcast());
        assert_eq!(engine.spec(), ChainSpec::podcast());
        assert_eq!(engine.chain().makeup_db(), 9.0);
        assert!(engine.chain().gate().is_none());
        // The same spec again is a no-op, not a rebuild.
        engine.set_spec(ChainSpec::podcast());
        assert_eq!(engine.spec(), ChainSpec::podcast());
    }
}
