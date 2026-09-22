//! The whole signal chain in one object, driven from the audio callback.
//!
//! Mirrors the order in `dfxpProcessReal.cpp` and `Play32.c`, which
//! `docs/spec/10-dsp-effects.md` §11 draws in full:
//!
//! ```text
//! in ─► graphic EQ ─► master gain · balance ─► volume levelling ─► effect chain ─► spectrum tap ─► out
//! ```
//!
//! Everything after construction is allocation-free. [`Engine::process`] is the only method the
//! real-time thread calls per buffer; the others are called from the same thread in response to a
//! parameter snapshot, and none of them allocate either.

use crate::biquad::Real;
use crate::effects::{Chain, MAX_BLOCK_FRAMES};
use crate::eq::GraphicEq;
use crate::leveller::VolumeLeveller;
use crate::spectrum::SpectrumAnalyser;
use fxsound_core::messages::{DspEvent, DspParams, Meters};

/// The complete FxSound processing chain.
#[derive(Debug)]
pub struct Engine {
    eq: GraphicEq,
    leveller: VolumeLeveller,
    chain: Chain,
    spectrum: SpectrumAnalyser,

    sample_rate: Real,
    channels: usize,

    /// `10^(dB/20)`, applied per sample (`GraphicEqSet.cpp:101`).
    master_gain: Real,
    /// Attenuation applied to each channel; balance never boosts (`GraphicEqSet.cpp:38-59`).
    balance_left: Real,
    balance_right: Real,

    /// Cached so a snapshot that did not change a value does not force a redesign.
    applied: DspParams,
    /// Samples processed per channel since the last reset, for the "audio processed" counter.
    processed_samples: u64,
    peak_left: Real,
    peak_right: Real,
    active: bool,
    /// Index of the subwoofer channel in the current layout, when there is one.
    lfe_channel: Option<usize>,
}

impl Engine {
    /// Build an engine sized for the worst case it will be asked to handle.
    ///
    /// `max_block_frames` is capped at [`MAX_BLOCK_FRAMES`]; a larger block is processed correctly
    /// but the spectrum analyser only looks at the first `max_block_frames` of it.
    #[must_use]
    pub fn new(sample_rate: f32, max_block_frames: usize, channels: usize) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let max_block_frames = max_block_frames.clamp(1, MAX_BLOCK_FRAMES);
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(sample_rate);

        let mut engine = Self {
            eq,
            leveller: VolumeLeveller::new(sample_rate),
            chain: Chain::new(sample_rate),
            spectrum: SpectrumAnalyser::new(sample_rate, max_block_frames),
            sample_rate,
            channels: channels.clamp(1, crate::biquad::MAX_CHANNELS),
            master_gain: 1.0,
            balance_left: 1.0,
            balance_right: 1.0,
            applied: DspParams::default(),
            processed_samples: 0,
            peak_left: 0.0,
            peak_right: 0.0,
            active: false,
            lfe_channel: None,
        };
        let params = DspParams::default();
        engine.apply_unconditionally(&params);
        engine
    }

    #[must_use]
    pub const fn sample_rate(&self) -> Real {
        self.sample_rate
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }

    /// The stream format changed. Redesigns every filter and clears all history.
    pub fn set_format(&mut self, sample_rate: f32, channels: usize) {
        let sample_rate = sample_rate.max(1.0);
        let channels = channels.clamp(1, crate::biquad::MAX_CHANNELS);
        if sample_rate == self.sample_rate && channels == self.channels {
            return;
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.eq.set_sample_rate(sample_rate);
        self.leveller.set_sample_rate(sample_rate);
        self.chain.set_sample_rate(sample_rate);
        self.spectrum.set_sample_rate(sample_rate);
        self.reset();
    }

    /// Name the subwoofer channel, so the stages that must not touch it can skip it.
    ///
    /// Passed as an index rather than derived from the channel count, because a device is free to
    /// order its channels however it likes: the subwoofer sits at 3 in a standard 5.1 or 7.1
    /// layout, and elsewhere in several real ones.
    pub fn set_lfe_channel(&mut self, channel: Option<usize>) {
        self.lfe_channel = channel;
        self.chain.set_lfe_channel(channel);
    }

    /// Name the front pair, so the two stereo-by-nature stages run over the right channels.
    ///
    /// `None` keeps the historical behaviour of using the first two, which is correct for every
    /// layout that starts `FL, FR` — that is, all the standard ones.
    pub fn set_front_pair(&mut self, pair: Option<(usize, usize)>) {
        self.chain.set_front_pair(pair);
    }

    /// Adopt a parameter snapshot, skipping anything that has not changed.
    pub fn apply(&mut self, params: &DspParams) {
        if *params == self.applied {
            return;
        }
        self.apply_unconditionally(params);
    }

    fn apply_unconditionally(&mut self, params: &DspParams) {
        self.chain.apply(params);

        self.eq.set_enabled(params.eq_on);
        self.eq.set_q_multiplier(params.filter_q);
        let (centers, boosts) = params.bands();
        if centers != self.eq.center_frequencies() || boosts != self.eq.boosts_db() {
            self.eq.set_bands(centers, boosts);
        }

        // Despite its name in the original API, this parameter is an abstract 0..=4 amount, not
        // decibels (`docs/spec/08-dsp-api.md` §8.4).
        self.leveller.set_amount(params.volume_leveling_db);

        self.master_gain = db_to_linear(params.master_gain_db);
        let (left, right) = balance_gains(params.balance);
        self.balance_left = left;
        self.balance_right = right;

        self.applied = *params;
    }

    /// Act on a one-shot event.
    pub fn handle_event(&mut self, event: DspEvent) {
        match event {
            DspEvent::ResetFilterState => {
                self.eq.reset();
                self.leveller.reset();
                self.chain.reset();
            }
            DspEvent::ResetSpectrum => self.spectrum.reset(),
            DspEvent::ResetProcessedTime => self.processed_samples = 0,
            // The music chain keeps no capture statistics; the event is the microphone's.
            DspEvent::ResetCaptureStats => {}
        }
    }

    /// Clear every filter's history.
    pub fn reset(&mut self) {
        self.eq.reset();
        self.leveller.reset();
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

    /// Process one interleaved block in place.
    ///
    /// Real-time safe: no allocation, no locks, no IO, no panicking paths.
    pub fn process(&mut self, buffer: &mut [f32], channels: usize) {
        let channels = channels.clamp(1, crate::biquad::MAX_CHANNELS);
        if buffer.is_empty() {
            return;
        }

        // Nothing between the client's bytes and the filters validates a sample, and every stage
        // here has infinite memory: a NaN in a biquad's state reproduces itself forever, the
        // reverb tank latches one into its delay arena, one `+inf` pins the leveller's peak so
        // both ends of its gain ramp become exactly zero, and the same value freezes Dynamic
        // Boost's level estimator — a stage that is never bypassed. None of it recovers on its
        // own; the only escape is a preset change, and the meters hide it because `f32::min`
        // discards NaN. One branch per sample is a rounding error against the block budget.
        for sample in buffer.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }

        if self.applied.power {
            self.eq.process(buffer, channels);
            self.apply_gain_stage(buffer, channels);
            // The subwoofer is excluded from the detector: it carries a deliberately enormous
            // amount of the programme's energy, so letting it into the level analysis pulls the
            // gain down on bass-heavy material for reasons that have nothing to do with how loud
            // the programme actually is.
            self.leveller
                .process_excluding(buffer, channels, self.lfe_channel);
            self.chain.process(buffer, channels);
        } else {
            // Bypassed, the master gain is still applied — it is the one stage that survives a
            // bypass in the original (`SosProcess.cpp:512-514`).
            self.apply_gain_stage(buffer, channels);
        }

        // A block that went in finite can still come out non-finite if a stage's own state has
        // blown up — a coefficient designed from a parameter that reached the engine before it
        // was sanitised, or a divergent filter at an extreme rate. Hand the device silence rather
        // than a NaN and clear the history, so the next block starts clean instead of inheriting
        // the failure for the rest of the session. This runs before the spectrum tap and the
        // meters, so neither the visualizer nor the GUI ever sees the bad block.
        if buffer.iter().any(|sample| !sample.is_finite()) {
            buffer.fill(0.0);
            self.reset();
        }

        self.spectrum.push(buffer, channels);
        self.measure(buffer, channels);
    }

    /// The master gain and the balance attenuation, folded into one pass.
    fn apply_gain_stage(&mut self, buffer: &mut [f32], channels: usize) {
        if self.master_gain == 1.0 && self.balance_left == 1.0 && self.balance_right == 1.0 {
            return;
        }
        if channels >= 2 {
            let left = self.master_gain * self.balance_left;
            let right = self.master_gain * self.balance_right;
            for frame in buffer.chunks_exact_mut(channels) {
                frame[0] *= left;
                frame[1] *= right;
                // Balance is stereo-only in the original; any further channels take the plain
                // master gain (`SosProcess.cpp:583`).
                for sample in &mut frame[2..] {
                    *sample *= self.master_gain;
                }
            }
        } else {
            for sample in buffer.iter_mut() {
                *sample *= self.master_gain;
            }
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
            peak_right = peak_left;
        }

        // Decay the held peaks so the meters fall rather than latching.
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
            // The output chain has no gate, compressor or de-esser; the fields belong to the
            // microphone chain and stay at zero here rather than being left undefined.
            gate_reduction_db: 0.0,
            compressor_reduction_db: 0.0,
            deesser_reduction_db: 0.0,
            deesser_running: false,
            denoiser_running: false,
            voice_probability: 0.0,
            // Likewise the microphone's telemetry and the calibration accumulators.
            input_peak: 0.0,
            input_rms_db: 0.0,
            noise_floor_db: 0.0,
            denoise_reduction_db: 0.0,
            deesser_hz: 0.0,
            dereverb_reduction_db: 0.0,
            latency_frames: 0,
            capture_frames: 0,
            capture_sum_squares: 0.0,
            capture_peak: 0.0,
            capture_clipped: 0,
        }
    }

    /// The equalizer, for drawing its response curve.
    #[must_use]
    pub const fn equalizer(&self) -> &GraphicEq {
        &self.eq
    }
}

/// dB to a linear amplitude factor.
#[inline]
#[must_use]
pub fn db_to_linear(db: Real) -> Real {
    if db == 0.0 {
        1.0
    } else {
        10_f32.powf(db / 20.0)
    }
}

/// Balance in dB to a pair of per-channel attenuations.
///
/// Positive pans right by attenuating the left channel and vice versa; neither side is ever
/// boosted (`GraphicEqSet.cpp:38-59`).
#[inline]
#[must_use]
pub fn balance_gains(balance_db: Real) -> (Real, Real) {
    if balance_db > 0.0 {
        (db_to_linear(-balance_db), 1.0)
    } else if balance_db < 0.0 {
        (1.0, db_to_linear(balance_db))
    } else {
        (1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leveller::CEILING;
    use fxsound_core::Effect as EffectId;

    fn tone(frames: usize, channels: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let s = (n as f32 * 0.05).sin() * amplitude;
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    #[test]
    fn a_default_engine_is_close_to_transparent_once_its_latency_is_accounted_for() {
        let mut engine = Engine::new(48_000.0, 4096, 2);
        let latency = engine.latency_frames();
        // Dynamic Boost always runs and it has a look-ahead delay, so the output is shifted by
        // that much and scaled by its permanent -0.3 dBFS ceiling. Nothing else should move.
        assert!(latency > 0, "the limiter should report its look-ahead");

        let input = tone(4096, 2, 0.25);
        let mut buffer = input.clone();
        engine.process(&mut buffer, 2);

        let ceiling = 0.966_051_f32;
        for frame in latency..(4096 - latency) {
            for channel in 0..2 {
                let got = buffer[frame * 2 + channel];
                let want = input[(frame - latency) * 2 + channel] * ceiling;
                assert!(
                    (got - want).abs() < 0.01,
                    "frame {frame} channel {channel}: got {got}, expected about {want}"
                );
            }
        }
    }

    #[test]
    fn master_gain_is_applied_in_decibels() {
        let mut engine = Engine::new(48_000.0, 256, 2);
        // Power off isolates the gain stage from the effects.
        let params = DspParams {
            power: false,
            master_gain_db: -6.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = vec![1.0_f32; 8];
        engine.process(&mut buffer, 2);
        let expected = 10_f32.powf(-6.0 / 20.0);
        assert!((buffer[0] - expected).abs() < 1e-6, "got {}", buffer[0]);
    }

    #[test]
    fn master_gain_survives_a_bypass() {
        // The original keeps the master gain live even when the engine is bypassed.
        let mut engine = Engine::new(48_000.0, 256, 2);
        let params = DspParams {
            power: false,
            master_gain_db: 6.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = vec![0.1_f32; 8];
        engine.process(&mut buffer, 2);
        assert!(
            buffer[0] > 0.15,
            "the bypass swallowed the master gain: {}",
            buffer[0]
        );
    }

    #[test]
    fn balance_attenuates_one_side_and_never_boosts_the_other() {
        let (left, right) = balance_gains(6.0);
        assert!(left < 1.0 && right == 1.0, "positive balance pans right");
        let (left, right) = balance_gains(-6.0);
        assert!(left == 1.0 && right < 1.0, "negative balance pans left");
        assert_eq!(balance_gains(0.0), (1.0, 1.0));
    }

    #[test]
    fn balance_reaches_the_audio_path() {
        let mut engine = Engine::new(48_000.0, 256, 2);
        let params = DspParams {
            power: false,
            balance: 20.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = vec![1.0_f32, 1.0, 1.0, 1.0];
        engine.process(&mut buffer, 2);
        assert!(buffer[0] < buffer[1], "left should be attenuated");
        assert!((buffer[1] - 1.0).abs() < 1e-6, "right should be untouched");
    }

    #[test]
    fn an_unchanged_snapshot_is_not_reapplied() {
        let mut engine = Engine::new(48_000.0, 256, 2);
        let params = DspParams::default();
        engine.apply(&params);
        let before = engine.applied;
        engine.apply(&params);
        assert_eq!(before, engine.applied);
    }

    #[test]
    fn the_equalizer_curve_follows_the_snapshot() {
        let mut engine = Engine::new(48_000.0, 256, 2);
        let mut params = DspParams::default();
        params.band_boost_db[2] = 9.0;
        engine.apply(&params);
        let center = engine.equalizer().center_frequencies()[2];
        assert!(engine.equalizer().response_db(center) > 5.0);
    }

    #[test]
    fn the_processed_sample_counter_tracks_frames_and_resets() {
        let mut engine = Engine::new(48_000.0, 512, 2);
        let mut buffer = tone(480, 2, 0.1);
        engine.process(&mut buffer, 2);
        assert_eq!(engine.meters().processed_samples, 480);
        engine.process(&mut buffer, 2);
        assert_eq!(engine.meters().processed_samples, 960);
        engine.handle_event(DspEvent::ResetProcessedTime);
        assert_eq!(engine.meters().processed_samples, 0);
    }

    #[test]
    fn meters_report_silence_as_inactive() {
        let mut engine = Engine::new(48_000.0, 512, 2);
        let mut buffer = vec![0.0_f32; 960];
        engine.process(&mut buffer, 2);
        assert!(!engine.meters().active);

        let mut buffer = tone(480, 2, 0.5);
        engine.process(&mut buffer, 2);
        assert!(engine.meters().active);
        assert!(engine.meters().peak_left > 0.1);
    }

    #[test]
    fn a_format_change_is_safe_mid_stream() {
        let mut engine = Engine::new(48_000.0, 1024, 2);
        let mut params = DspParams::default();
        for id in EffectId::ALL {
            params.set_effect(id, 0.7);
        }
        engine.apply(&params);

        let mut buffer = tone(512, 2, 0.4);
        engine.process(&mut buffer, 2);
        engine.set_format(96_000.0, 2);
        assert_eq!(engine.sample_rate(), 96_000.0);
        let mut buffer = tone(512, 2, 0.4);
        engine.process(&mut buffer, 2);
        assert!(buffer.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn mono_streams_are_handled() {
        let mut engine = Engine::new(48_000.0, 512, 1);
        let params = DspParams {
            power: false,
            master_gain_db: -6.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut buffer = vec![1.0_f32; 64];
        engine.process(&mut buffer, 1);
        let expected = 10_f32.powf(-6.0 / 20.0);
        assert!(buffer.iter().all(|s| (s - expected).abs() < 1e-6));
    }

    /// 10 ms at 48 kHz. The block size is part of the levelling stage's behaviour, not an
    /// implementation detail: the gain is ramped across whatever block it is handed
    /// (`SosProcess.cpp:375-379`), and the detector's time constants are counted in blocks.
    const LEVELLER_BLOCK: usize = 480;

    /// A phase-continuous 300 Hz stereo tone.
    ///
    /// Continuity matters for this stage in a way it does not for the rest of the chain: the
    /// levelling detector is a 120 Hz high-pass (`SosProcess.cpp:174-177`), so a phase jump at
    /// every block boundary would reach it as a transient the programme does not actually contain.
    fn continuous_tone(start_frame: usize, frames: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let n = (start_frame + i) as f64;
                let phase = std::f64::consts::TAU * 300.0 * n / 48_000.0;
                let value = (f64::from(amplitude) * phase.sin()) as f32;
                std::iter::repeat_n(value, 2)
            })
            .collect()
    }

    /// Runs a steady tone through a whole engine at the given levelling amount and returns the
    /// output peak over the last ten blocks — that is, once the levelling gain has settled.
    fn settled_output_peak(amount: f32, amplitude: f32, blocks: usize) -> f32 {
        let params = DspParams {
            volume_leveling_db: amount,
            ..DspParams::default()
        };
        let mut engine = Engine::new(48_000.0, LEVELLER_BLOCK, 2);
        engine.apply(&params);

        let mut peak = 0.0_f32;
        for block in 0..blocks {
            let mut buffer = continuous_tone(block * LEVELLER_BLOCK, LEVELLER_BLOCK, amplitude);
            engine.process(&mut buffer, 2);
            if block + 10 >= blocks {
                peak = buffer.iter().fold(peak, |m, s| m.max(s.abs()));
            }
        }
        peak
    }

    #[test]
    fn the_levelling_stage_lifts_a_quiet_passage_and_holds_a_loud_one_down() {
        // The stage sits between the gain stage and the effect chain, so the honest way to see
        // what it did to the finished output is to run the identical chain twice and change
        // nothing but the amount.
        let quiet_off = settled_output_peak(0.0, 0.02, 400);
        let quiet_on = settled_output_peak(4.0, 0.02, 400);
        assert!(
            quiet_on > quiet_off * 2.5,
            "a -34 dBFS passage was not lifted: {quiet_off} -> {quiet_on}"
        );

        // A loud passage is *held*, not turned down towards the RMS target: `desired_gain` is
        // floored at the quiet gain floor, which is itself floored at unity
        // (`SosProcess.cpp:333`, `:458-461`), so the only thing that can pull the levelling gain
        // below 1.0 is the peak clamp at `:358-364`.
        let loud_off = settled_output_peak(0.0, 0.9, 400);
        let loud_on = settled_output_peak(4.0, 0.9, 400);
        assert!(
            loud_on <= loud_off * 1.02,
            "a -1 dBFS passage was boosted rather than held: {loud_off} -> {loud_on}"
        );
        assert!(loud_on <= CEILING, "the output broke full scale: {loud_on}");

        // Which is the point of the stage: the two passages come out far closer together than they
        // went in.
        let spread_off = loud_off / quiet_off;
        let spread_on = loud_on / quiet_on;
        assert!(
            spread_on < spread_off / 2.0,
            "levelling did not close the gap: {spread_off}:1 became {spread_on}:1"
        );
    }

    #[test]
    fn a_zero_amount_leveller_is_absent_from_the_path_bit_for_bit() {
        // Amount 0 is the shipping default (`fxsound/Source/GUI/FxController.h:48`), and
        // `SosProcess.cpp:146-150` returns before touching a sample. `Engine::process` calls the
        // stage unconditionally, so what has to be proved here is that having it in the chain and
        // not having it at all produce the same bits — not merely the same audio. Anything that
        // rode in from the previous buffer (a stale ramp, a retained gain, a clamp at the ceiling)
        // would show up as a mismatch somewhere in the block.
        let mut params = DspParams {
            master_gain_db: -3.0,
            volume_leveling_db: 0.0,
            ..DspParams::default()
        };
        params.band_boost_db[3] = 6.0;

        let mut engine = Engine::new(48_000.0, 2048, 2);
        engine.apply(&params);
        let input = tone(2048, 2, 0.2);
        let mut through_engine = input.clone();
        engine.process(&mut through_engine, 2);

        // The same chain assembled by hand, with no leveller in it at all. Both snapshots are
        // applied in turn because `Engine::new` adopts the default one before `apply` adopts ours.
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        let mut chain = Chain::new(48_000.0);
        for snapshot in [&DspParams::default(), &params] {
            chain.apply(snapshot);
            eq.set_enabled(snapshot.eq_on);
            eq.set_q_multiplier(snapshot.filter_q);
            let (centers, boosts) = snapshot.bands();
            if centers != eq.center_frequencies() || boosts != eq.boosts_db() {
                eq.set_bands(centers, boosts);
            }
        }

        let mut reference = input;
        eq.process(&mut reference, 2);
        let (left, right) = balance_gains(params.balance);
        let left = db_to_linear(params.master_gain_db) * left;
        let right = db_to_linear(params.master_gain_db) * right;
        for frame in reference.as_chunks_mut::<2>().0 {
            frame[0] *= left;
            frame[1] *= right;
        }
        chain.process(&mut reference, 2);

        for (index, (got, want)) in through_engine.iter().zip(reference.iter()).enumerate() {
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "sample {index}: engine gave {got}, a leveller-free chain gave {want}"
            );
        }
    }

    #[test]
    fn everything_at_maximum_stays_finite_and_bounded() {
        let mut engine = Engine::new(48_000.0, 4096, 2);
        let mut params = DspParams::default();
        for id in EffectId::ALL {
            params.set_effect(id, 1.0);
        }
        for band in 0..10 {
            params.band_boost_db[band] = 12.0;
        }
        params.master_gain_db = 20.0;
        engine.apply(&params);

        let mut buffer = tone(4096, 2, 0.9);
        engine.process(&mut buffer, 2);
        assert!(buffer.iter().all(|s| s.is_finite()));
    }

    /// The three stages that latch a bad sample do it in three different ways — the biquads and
    /// the reverb tank go non-finite, the leveller goes silent, Dynamic Boost's gain sticks — so
    /// each one is measured on the quantity that actually moves.
    fn poison(buffer: &mut [f32], value: f32) {
        buffer[0] = value;
    }

    fn peak_of(buffer: &[f32]) -> f32 {
        buffer.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))
    }

    /// Pins the premise of the app-level fix: a power cycle does not clear everything, so
    /// something above the engine has to send `ResetFilterState`.
    ///
    /// `Chain::set_power` resets the five effects, so the reverb tail really does go — but the
    /// equalizer and the volume leveller are outside the chain and keep their state, and the
    /// leveller's is the one a listener notices, because its gain is built from seconds of
    /// history. If anyone ever makes `apply` reset on a false→true transition, this fails and the
    /// two fixes get reconciled in one place instead of quietly both existing.
    /// Peak of one channel of an interleaved buffer.
    fn channel_peak(buffer: &[f32], channels: usize, channel: usize) -> f32 {
        buffer
            .iter()
            .skip(channel)
            .step_by(channels)
            .fold(0.0_f32, |acc, s| acc.max(s.abs()))
    }

    /// A buffer that is silent except for one full-scale sample in one channel.
    fn impulse(frames: usize, channels: usize, channel: usize) -> Vec<f32> {
        let mut buffer = vec![0.0_f32; frames * channels];
        buffer[64 * channels + channel] = 1.0;
        buffer
    }

    /// Wrong routing is worse than a wrong equalizer curve: the listener hears the left channel on
    /// the right, or the centre from the subwoofer, and nothing in the interface suggests why.
    /// Nothing asserted this before, so every divergence was invisible to the test suite.
    #[test]
    fn an_impulse_stays_in_the_channel_it_was_put_in() {
        for channels in 1..=crate::biquad::MAX_CHANNELS {
            let mut engine = Engine::new(48_000.0, 4096, channels);
            // Every effect amount at zero. Surround and Ambience deliberately cross-mix the front
            // pair and get their own test below; what is left — the equalizer, the gain stage, the
            // leveller, and Dynamic Boost, which is never bypassed — must be strictly per-channel,
            // at any channel count.
            let mut params = DspParams {
                volume_leveling_db: 2.0,
                ..DspParams::default()
            };
            for band in 0..10 {
                params.band_boost_db[band] = if band % 2 == 0 { 6.0 } else { -6.0 };
            }
            engine.apply(&params);

            for source in 0..channels {
                engine.reset();
                let mut buffer = impulse(2048, channels, source);
                engine.process(&mut buffer, channels);

                assert!(
                    channel_peak(&buffer, channels, source) > 0.01,
                    "{channels}ch: the impulse vanished from channel {source}"
                );
                for other in (0..channels).filter(|c| *c != source) {
                    let leak = channel_peak(&buffer, channels, other);
                    assert!(
                        leak < 1e-6,
                        "{channels}ch: an impulse in channel {source} leaked {leak} into {other}"
                    );
                }
            }
        }
    }

    /// The two effects that are inherently stereo run one instance over the front pair, which is
    /// what the original does. That is a deliberate divergence from per-channel processing, so it
    /// is pinned rather than left to be rediscovered — and it is exactly why a device that reports
    /// its channels in a non-standard order is a routing hazard: "the front pair" is currently
    /// "channels 0 and 1", not "whichever channels are FL and FR".
    #[test]
    fn the_two_effects_that_mix_channels_only_touch_the_first_two() {
        for channels in 2..=crate::biquad::MAX_CHANNELS {
            for (effect, name) in [
                (EffectId::Surround, "Surround"),
                (EffectId::Ambience, "Ambience"),
            ] {
                let mut engine = Engine::new(48_000.0, 4096, channels);
                let mut params = DspParams::default();
                params.set_effect(effect, 1.0);
                engine.apply(&params);

                // An impulse in the left of the pair must reach the right of the pair.
                engine.reset();
                let mut buffer = impulse(2048, channels, 0);
                engine.process(&mut buffer, channels);
                assert!(
                    channel_peak(&buffer, channels, 1) > 1e-4,
                    "{channels}ch: {name} did not reach the other half of the front pair"
                );
                for rear in 2..channels {
                    assert!(
                        channel_peak(&buffer, channels, rear) < 1e-6,
                        "{channels}ch: {name} spilled the front pair into channel {rear}"
                    );
                }

                // And a rear channel must be left alone in both directions.
                engine.reset();
                let mut buffer = impulse(2048, channels, channels - 1);
                engine.process(&mut buffer, channels);
                if channels > 2 {
                    assert!(
                        channel_peak(&buffer, channels, 0) < 1e-6
                            && channel_peak(&buffer, channels, 1) < 1e-6,
                        "{channels}ch: {name} pulled a rear channel into the front pair"
                    );
                }
            }
        }
    }

    /// A 5.1 layout in the order every consumer device uses: FL FR FC LFE RL RR.
    const LFE: usize = 3;

    /// The failure this prevents is the one a listener notices instantly and cannot attribute to
    /// FxSound: a device that reports `FL, FC, FR, LFE, SL, SR` — front right at index 2, not 1 —
    /// had its stereo widener applied across front-left and *centre*, pulling dialogue out of the
    /// centre channel and into a phantom image.
    #[test]
    fn the_stereo_stages_follow_the_layout_rather_than_the_first_two_channels() {
        let channels = 6;
        const FL: usize = 0;
        const FC: usize = 1;
        const FR: usize = 2;

        for (effect, name) in [
            (EffectId::Surround, "Surround"),
            (EffectId::Ambience, "Ambience"),
        ] {
            let mut engine = Engine::new(48_000.0, 4096, channels);
            engine.set_front_pair(Some((FL, FR)));
            let mut params = DspParams::default();
            params.set_effect(effect, 1.0);
            engine.apply(&params);

            let mut buffer = impulse(2048, channels, FL);
            engine.process(&mut buffer, channels);

            assert!(
                channel_peak(&buffer, channels, FR) > 1e-4,
                "{name} did not reach front right at index {FR}"
            );
            assert!(
                channel_peak(&buffer, channels, FC) < 1e-6,
                "{name} reached the centre channel, which is the dialogue"
            );
        }
    }

    #[test]
    fn the_harmonic_generator_is_kept_out_of_the_subwoofer() {
        // `docs/spec/08-dsp-api.md:905-906` — the original sends Fidelity to the front, rear, side
        // and centre instances and explicitly not to the LFE. Putting sine-fold harmonics on the
        // one channel that exists to carry only the bottom two octaves is the clearest possible
        // case of the wrong signal in the wrong place.
        let channels = 6;
        let mut params = DspParams::default();
        params.set_effect(EffectId::Fidelity, 1.0);

        let mut without = Engine::new(48_000.0, 4096, channels);
        without.apply(&params);
        let mut with = Engine::new(48_000.0, 4096, channels);
        with.set_lfe_channel(Some(LFE));
        with.apply(&params);

        let mut a = tone(2048, channels, 0.5);
        let mut b = a.clone();
        let reference = a.clone();
        without.process(&mut a, channels);
        with.process(&mut b, channels);

        let latency = with.latency_frames();
        let sample = |buf: &[f32], frame: usize, ch: usize| buf[frame * channels + ch];
        let mut touched = 0.0_f32;
        let mut untouched = 0.0_f32;
        for frame in latency..2000 {
            let dry = sample(&reference, frame - latency, LFE);
            untouched = untouched.max((sample(&b, frame, LFE) - dry * 0.966_051).abs());
            touched = touched.max((sample(&a, frame, LFE) - dry * 0.966_051).abs());
        }
        assert!(
            touched > 1e-3,
            "the fixture is wrong: Fidelity did not change the subwoofer even without the exclusion"
        );
        assert!(
            untouched < 1e-4,
            "Fidelity still reached the subwoofer: {untouched} away from the dry signal"
        );

        // And the channels it is supposed to reach must still be reached.
        let mut front = 0.0_f32;
        for frame in latency..2000 {
            let dry = sample(&reference, frame - latency, 0);
            front = front.max((sample(&b, frame, 0) - dry * 0.966_051).abs());
        }
        assert!(
            front > 1e-3,
            "Fidelity stopped reaching the front channels too"
        );
    }

    #[test]
    fn the_subwoofer_does_not_drag_the_leveller_down_with_it() {
        // The LFE channel carries a deliberately enormous share of a film's energy. Letting it
        // into the level detector pulls the gain down on everything else for a reason that has
        // nothing to do with how loud the programme is.
        let channels = 6;
        let params = DspParams {
            volume_leveling_db: 4.0,
            ..DspParams::default()
        };

        let mut quiet_sub = Engine::new(48_000.0, 4096, channels);
        quiet_sub.set_lfe_channel(Some(LFE));
        quiet_sub.apply(&params);
        let mut loud_sub = Engine::new(48_000.0, 4096, channels);
        loud_sub.set_lfe_channel(Some(LFE));
        loud_sub.apply(&params);

        let mut last_quiet = Vec::new();
        let mut last_loud = Vec::new();
        for _ in 0..30 {
            let mut a = tone(2048, channels, 0.1);
            let mut b = a.clone();
            for frame in b.chunks_exact_mut(channels) {
                frame[LFE] *= 9.0;
            }
            quiet_sub.process(&mut a, channels);
            loud_sub.process(&mut b, channels);
            last_quiet = a;
            last_loud = b;
        }

        let front_quiet = channel_peak(&last_quiet, channels, 0);
        let front_loud = channel_peak(&last_loud, channels, 0);
        assert!(
            (front_quiet - front_loud).abs() < front_quiet * 0.02,
            "a loud subwoofer moved the front channels: {front_loud} against {front_quiet}"
        );
    }

    #[test]
    fn a_power_cycle_alone_does_not_clear_the_levellers_gain() {
        let params = DspParams {
            volume_leveling_db: 4.0,
            ..DspParams::default()
        };

        // Drive the leveller with a loud passage until its gain has settled downwards.
        let mut used = Engine::new(48_000.0, 4096, 2);
        used.apply(&params);
        for _ in 0..40 {
            let mut loud = tone(4096, 2, 0.9);
            used.process(&mut loud, 2);
        }

        let off = DspParams {
            power: false,
            ..params
        };
        used.apply(&off);
        used.apply(&params);

        let mut fresh = Engine::new(48_000.0, 4096, 2);
        fresh.apply(&params);

        let mut after_cycle = tone(4096, 2, 0.1);
        let mut from_fresh = tone(4096, 2, 0.1);
        used.process(&mut after_cycle, 2);
        fresh.process(&mut from_fresh, 2);

        assert!(
            (peak_of(&after_cycle) - peak_of(&from_fresh)).abs() > 1e-4,
            "the power cycle already produced a fresh leveller, so the app need not send \
             ResetFilterState"
        );

        used.handle_event(DspEvent::ResetFilterState);
        let mut after_reset = tone(4096, 2, 0.1);
        used.process(&mut after_reset, 2);
        assert!(
            (peak_of(&after_reset) - peak_of(&from_fresh)).abs() < 1e-4,
            "ResetFilterState did not bring the leveller back to its initial gain"
        );
    }

    #[test]
    fn one_bad_sample_does_not_poison_the_filters_for_the_rest_of_the_session() {
        // With Ambience up, a single non-finite sample used to reach the 663 kB delay arena and
        // every later block came out non-finite on both channels, for good.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut engine = Engine::new(48_000.0, 4096, 2);
            let mut params = DspParams::default();
            params.set_effect(EffectId::Ambience, 1.0);
            for band in 0..10 {
                params.band_boost_db[band] = 6.0;
            }
            engine.apply(&params);

            let mut first = tone(512, 2, 0.25);
            poison(&mut first, bad);
            engine.process(&mut first, 2);
            assert!(
                first.iter().all(|s| s.is_finite()),
                "{bad:?} reached the output"
            );

            for _ in 0..200 {
                let mut block = tone(512, 2, 0.25);
                engine.process(&mut block, 2);
                assert!(
                    block.iter().all(|s| s.is_finite()),
                    "{bad:?} was still poisoning the chain 200 blocks later"
                );
            }
        }
    }

    #[test]
    fn an_infinity_does_not_silence_the_volume_leveller_for_good() {
        // `+inf` pinned the side-chain peak, and the peak-safety division then made both ends of
        // the gain ramp exactly zero — finite, so nothing downstream could notice, and silent
        // until the next preset change.
        let mut engine = Engine::new(48_000.0, 4096, 2);
        let params = DspParams {
            volume_leveling_db: 4.0,
            ..DspParams::default()
        };
        engine.apply(&params);

        let mut warm = tone(4096, 2, 0.25);
        engine.process(&mut warm, 2);
        let reference = peak_of(&warm);
        assert!(reference > 0.01, "the reference block should not be silent");

        let mut bad = tone(4096, 2, 0.25);
        poison(&mut bad, f32::INFINITY);
        engine.process(&mut bad, 2);

        let mut recovered = 0.0_f32;
        for _ in 0..40 {
            let mut block = tone(4096, 2, 0.25);
            engine.process(&mut block, 2);
            recovered = peak_of(&block);
        }
        assert!(
            recovered > reference * 0.5,
            "the leveller stayed silent: {recovered} against a reference of {reference}"
        );
    }

    #[test]
    fn an_infinity_does_not_stick_dynamic_boosts_auto_gain() {
        // Dynamic Boost is never bypassed, so a stuck level estimator changed the output level
        // for every user with the default preset. The symptom was a permanent +1.06 factor.
        let mut clean = Engine::new(48_000.0, 4096, 2);
        let mut dirty = Engine::new(48_000.0, 4096, 2);

        for _ in 0..20 {
            let mut block = tone(4096, 2, 0.2);
            clean.process(&mut block, 2);
        }
        let mut bad = tone(4096, 2, 0.2);
        poison(&mut bad, f32::INFINITY);
        dirty.process(&mut bad, 2);
        for _ in 0..19 {
            let mut block = tone(4096, 2, 0.2);
            dirty.process(&mut block, 2);
        }

        let mut a = tone(4096, 2, 0.2);
        let mut b = tone(4096, 2, 0.2);
        clean.process(&mut a, 2);
        dirty.process(&mut b, 2);
        let (want, got) = (peak_of(&a), peak_of(&b));
        assert!(
            (got - want).abs() < want * 0.01,
            "the auto-gain did not come back: {got} against {want}"
        );
    }
}
