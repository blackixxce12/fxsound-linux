//! The microphone chain: a list of stages, built from a spec, in the spec's order.
//!
//! The default — [`ChainSpec::voice`], the chain 0.3.0 shipped — runs
//!
//! ```text
//! mic ─► denoise ─► dereverb ─► high-pass ─► gate ─► EQ ─► de-esser ─► compressor ─► makeup ─► limiter ─► out
//! ```
//!
//! The order is not a preference. Each position earns itself:
//!
//! - **Denoising first of all.** Five of the nine community chains surveyed put it first and none
//!   put it after the limiter, and the reason is downstream: everything below measures a level,
//!   and a denoised level is a different number. Every gate threshold in the preset draft was
//!   chosen against an un-denoised floor and has to be re-voiced now that this stage exists.
//! - **De-reverb second**, and only after the denoiser: its estimator takes a decaying tail for
//!   the room's, and a stationary floor is a tail that never decays — the network's mask has to
//!   have removed it first, or the whole session is taken down toward the floor. Before the
//!   high-pass and the gate, because the tail it removes is exactly what holds a gate open
//!   through a pause. Inert unless a preset or the settings ask for it.
//! - **High-pass next**, because desk rumble and a plosive are ten to twenty decibels above the
//!   voice below 150 Hz. Leave them in and they hold the gate open through every pause and drive
//!   the compressor on sounds nobody can hear.
//! - **Gate before the EQ**, so that what the gate measures is the microphone rather than the
//!   preset's own presence lift — otherwise changing a band would move the threshold.
//! - **De-esser before the compressor**, which is the order the design was written down in. A
//!   compressor in front would ride the sibilant and duck the word behind it, which is the exact
//!   complaint that makes a de-esser necessary in the first place. (The streaming and broadcast
//!   specs argue the other way, for their own material; see [`ChainSpec`].)
//! - **Makeup after everything that measures**, because every threshold in the preset set was
//!   voiced against the signal as it arrives, not against one already lifted.
//! - **Limiter last, always running.** It is the only stage that cannot be switched off: makeup
//!   gain is the one control here that can produce a sample above full scale, and something has to
//!   be standing behind it. A spec cannot leave it out or move it.
//!
//! The other specs are the same stages in another order, or without one of them, and each says
//! why on [`ChainSpec`]. What a preset's parameters do to each stage is [`InputChain::apply`],
//! in one place, so that a field added to the snapshot reaches the audio thread and the test
//! bench alike.
//!
//! Real-time safe: every stage is fixed-size, and the chain itself only sequences them. Building
//! a chain from a spec allocates and belongs on the main loop; the [`crate::input::InputEngine`]
//! documents when that happens.

use crate::biquad::Real;
use crate::eq::GraphicEq;
use crate::input::denoise::Denoiser;
use crate::input::dereverb::Dereverb;
use crate::input::highpass::HighPass;
use crate::input::limiter::LookaheadLimiter;
use crate::input::makeup::Makeup;
use crate::input::processor::{
    AudioProcessor, ChainSpec, MAX_STAGES, ProcessContext, Stage, StageAccess, StageKind,
    StageMeter,
};
use crate::input::{Compressor, DeEsser, Gate, sane_rate};
use fxsound_core::messages::InputDspParams;

/// The default ceiling for a microphone stream, in dBFS.
///
/// −3 rather than −1: what leaves here is re-encoded downstream, Opus for a voice call and AAC for
/// the streaming platforms, and a lossy encoder overshoots the sample peak it was handed.
const DEFAULT_CEILING_DB: Real = -3.0;

pub struct InputChain {
    /// In the spec's order, the limiter last; the slots past the spec's length are empty.
    stages: [Option<Stage>; MAX_STAGES],
    spec: ChainSpec,
    sample_rate: Real,
    power: bool,
}

impl std::fmt::Debug for InputChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kinds: Vec<StageKind> = self.spec.kinds().collect();
        f.debug_struct("InputChain")
            .field("sample_rate", &self.sample_rate)
            .field("power", &self.power)
            .field("spec", &kinds)
            .field("stages", &self.stages)
            .finish()
    }
}

impl InputChain {
    /// The voice chain, sized for the worst case. Allocates; build it on the main loop.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        Self::from_spec(ChainSpec::voice(), sample_rate)
    }

    /// Any chain the spec describes. Allocates every stage the spec names; build it on the main
    /// loop, never on the audio thread.
    #[must_use]
    pub fn from_spec(spec: ChainSpec, sample_rate: Real) -> Self {
        let sample_rate = sane_rate(sample_rate);
        let mut stages: [Option<Stage>; MAX_STAGES] = std::array::from_fn(|_| None);
        for (slot, kind) in stages.iter_mut().zip(spec.kinds()) {
            *slot = Some(Stage::build(kind, sample_rate));
        }
        let mut chain = Self {
            stages,
            spec,
            sample_rate,
            power: true,
        };
        chain.set_ceiling_db(DEFAULT_CEILING_DB);
        chain
    }

    /// The ordering this chain was built from.
    #[must_use]
    pub const fn spec(&self) -> ChainSpec {
        self.spec
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

    /// Redesign every stage for a rate. Each stage resets itself as part of that, so there is no
    /// reset of the chain on top — 0.3.0 reset the denoiser twice here, once inside its own rate
    /// change and once for the chain's, and the second was eight network states rebuilt for
    /// nothing.
    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sane_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        for stage in self.stages.iter_mut().flatten() {
            stage.prepare(sample_rate);
        }
    }

    /// The stage of a type, if the spec holds one: `chain.stage::<Gate>()`.
    #[must_use]
    pub fn stage<T: StageAccess>(&self) -> Option<&T> {
        self.stages.iter().flatten().find_map(T::from_stage)
    }

    pub fn stage_mut<T: StageAccess>(&mut self) -> Option<&mut T> {
        self.stages.iter_mut().flatten().find_map(T::from_stage_mut)
    }

    /// Every stage, in order.
    pub fn stages(&self) -> impl Iterator<Item = &Stage> {
        self.stages.iter().flatten()
    }

    /// One stage's meter, or an empty one for a stage the spec does not hold.
    #[must_use]
    pub fn meter(&self, kind: StageKind) -> StageMeter {
        self.stages
            .iter()
            .flatten()
            .find(|stage| stage.kind() == kind)
            .map(AudioProcessor::meter)
            .unwrap_or_default()
    }

    /// Everything the snapshot says, to every stage — and the bypass. The one place a parameter
    /// becomes a stage setting, for the audio thread and the test bench alike.
    pub fn apply(&mut self, params: &InputDspParams) {
        self.set_power(params.power);
        for stage in self.stages.iter_mut().flatten() {
            stage.apply(params);
        }
    }

    /// The high-pass corner and its order — `0` for off, `2` or `4`. See [`HighPass::set`] for
    /// what order 4 is and is not.
    pub fn set_highpass(&mut self, hz: Real, order: usize) {
        if let Some(highpass) = self.stage_mut::<HighPass>() {
            highpass.set(hz, order);
        }
    }

    /// How many second-order sections the high-pass is actually running: `0` when it is switched
    /// off, when the sample rate cannot carry the corner asked for, or when the spec has no
    /// high-pass.
    #[must_use]
    pub fn highpass_sections(&self) -> usize {
        self.stage::<HighPass>().map_or(0, HighPass::sections)
    }

    /// Gain applied after everything that measures and before the limiter, in dB.
    pub fn set_makeup_db(&mut self, db: Real) {
        if let Some(makeup) = self.stage_mut::<Makeup>() {
            makeup.set_db(db);
        }
    }

    #[must_use]
    pub fn makeup_db(&self) -> Real {
        self.stage::<Makeup>().map_or(0.0, Makeup::db)
    }

    /// The level the chain's output may never exceed, in dBFS.
    pub fn set_ceiling_db(&mut self, db: Real) {
        if let Some(limiter) = self.stage_mut::<LookaheadLimiter>() {
            limiter.set_ceiling_db(db);
        }
    }

    /// Switch the denoiser on. Whether it then runs also depends on its level and the capture
    /// rate: RNNoise exists at 48 kHz and nowhere else. [`InputChain::denoiser`] reports which.
    pub fn set_denoise_enabled(&mut self, on: bool) {
        if let Some(denoiser) = self.stage_mut::<Denoiser>() {
            denoiser.set_enabled(on);
        }
    }

    pub fn set_gate_enabled(&mut self, on: bool) {
        if let Some(gate) = self.stage_mut::<Gate>() {
            gate.set_enabled(on);
        }
    }

    pub fn set_deesser_enabled(&mut self, on: bool) {
        if let Some(deesser) = self.stage_mut::<DeEsser>() {
            deesser.set_enabled(on);
        }
    }

    pub fn set_compressor_enabled(&mut self, on: bool) {
        if let Some(compressor) = self.stage_mut::<Compressor>() {
            compressor.set_enabled(on);
        }
    }

    /// Each stage owns its own parameters; the chain owns the order, the format and the bypass.
    /// `None` when the spec does not hold the stage.
    pub fn gate_mut(&mut self) -> Option<&mut Gate> {
        self.stage_mut::<Gate>()
    }

    pub fn eq_mut(&mut self) -> Option<&mut GraphicEq> {
        self.stage_mut::<GraphicEq>()
    }

    pub fn deesser_mut(&mut self) -> Option<&mut DeEsser> {
        self.stage_mut::<DeEsser>()
    }

    pub fn compressor_mut(&mut self) -> Option<&mut Compressor> {
        self.stage_mut::<Compressor>()
    }

    pub fn denoiser_mut(&mut self) -> Option<&mut Denoiser> {
        self.stage_mut::<Denoiser>()
    }

    pub fn dereverb_mut(&mut self) -> Option<&mut Dereverb> {
        self.stage_mut::<Dereverb>()
    }

    #[must_use]
    pub fn gate(&self) -> Option<&Gate> {
        self.stage::<Gate>()
    }

    #[must_use]
    pub fn eq(&self) -> Option<&GraphicEq> {
        self.stage::<GraphicEq>()
    }

    #[must_use]
    pub fn deesser(&self) -> Option<&DeEsser> {
        self.stage::<DeEsser>()
    }

    #[must_use]
    pub fn compressor(&self) -> Option<&Compressor> {
        self.stage::<Compressor>()
    }

    #[must_use]
    pub fn limiter(&self) -> Option<&LookaheadLimiter> {
        self.stage::<LookaheadLimiter>()
    }

    #[must_use]
    pub fn denoiser(&self) -> Option<&Denoiser> {
        self.stage::<Denoiser>()
    }

    #[must_use]
    pub fn dereverb(&self) -> Option<&Dereverb> {
        self.stage::<Dereverb>()
    }

    /// Clear every stage's history, in place.
    pub fn reset(&mut self) {
        for stage in self.stages.iter_mut().flatten() {
            stage.reset();
        }
    }

    /// Frames of delay the chain adds: the sum over its stages of what each reports while it is
    /// active — the limiter's look-ahead always, RNNoise's two frames and the de-reverb's one
    /// when they run. Everything else here is a filter or a gain.
    ///
    /// Reported whether or not the chain is powered, as the output chain does: a latency that
    /// changed when someone pressed a button would mean renegotiating the stream to save a
    /// millisecond, and the audio crate republishes only on a change. The cost is that a bypassed
    /// chain with the denoiser on is reported twenty milliseconds late — a lip-sync error for
    /// the length of the bypass, and a deliberate one: renegotiation is the worse of the two.
    /// **Switching a stage with latency on does change the figure**, so a caller that publishes
    /// it has to notice.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        self.stages
            .iter()
            .flatten()
            .map(AudioProcessor::latency_frames)
            .sum()
    }

    /// Run the chain over one interleaved block, in place.
    ///
    /// The denoiser's voice probability, once it has run, rides along in the context for the
    /// stages after it; a chain without a denoiser hands them zero.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.power || channels == 0 || buffer.is_empty() {
            return;
        }
        let mut ctx = ProcessContext {
            sample_rate: self.sample_rate,
            channels,
            voice_probability: 0.0,
        };
        for stage in self.stages.iter_mut().flatten() {
            stage.process(buffer, &ctx);
            if let Stage::Denoise(denoiser) = stage {
                ctx.voice_probability = denoiser.voice_probability();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biquad::MAX_CHANNELS;
    use crate::input::detector::{db_to_linear, linear_to_db};
    use fxsound_core::{DenoiseLevel, DereverbLevel};

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
        chain.eq_mut().expect("eq").set_enabled(false);
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
        with_filter
            .gate_mut()
            .expect("gate")
            .set_threshold_db(-45.0);
        with_filter.gate_mut().expect("gate").set_range_db(-14.0);
        with_filter.gate_mut().expect("gate").set_hold_ms(0.0);
        with_filter.set_deesser_enabled(false);
        with_filter.set_compressor_enabled(false);
        let mut block = rumble.clone();
        with_filter.process(&mut block, 1);

        let mut without = InputChain::new(FS);
        without.set_highpass(80.0, 0);
        without.set_gate_enabled(true);
        without.gate_mut().expect("gate").set_threshold_db(-45.0);
        without.gate_mut().expect("gate").set_range_db(-14.0);
        without.gate_mut().expect("gate").set_hold_ms(0.0);
        without.set_deesser_enabled(false);
        without.set_compressor_enabled(false);
        let mut block = rumble;
        without.process(&mut block, 1);

        assert!(
            without.gate().expect("gate").gain(0) > 0.99,
            "premise: unfiltered rumble should hold the gate open, it is at {}",
            without.gate().expect("gate").gain(0)
        );
        assert!(
            with_filter.gate().expect("gate").reduction_db(0) > 13.0,
            "the rumble reached the gate: only {} dB of reduction",
            with_filter.gate().expect("gate").reduction_db(0)
        );
    }

    #[test]
    fn the_gate_measures_the_microphone_and_not_the_presets_own_eq() {
        // Why the gate sits in front of the equalizer. A preset that lifts presence by two decibels
        // must not thereby move its own gate threshold by two decibels.
        let quiet = tone(2_519.0, db_to_linear(-46.0), 48_000);

        let mut flat = bare(FS);
        flat.set_gate_enabled(true);
        flat.gate_mut().expect("gate").set_hold_ms(0.0);
        let mut block = quiet.clone();
        flat.process(&mut block, 1);

        let mut lifted = bare(FS);
        lifted.set_gate_enabled(true);
        lifted.gate_mut().expect("gate").set_hold_ms(0.0);
        let eq = lifted.eq_mut().expect("eq");
        eq.set_enabled(true);
        for band in 0..10 {
            eq.set_band_boost(band, 0.0);
        }
        eq.set_band_boost(6, 6.0);
        let mut block = quiet;
        lifted.process(&mut block, 1);

        let gap = (flat.gate().expect("gate").reduction_db(0)
            - lifted.gate().expect("gate").reduction_db(0))
        .abs();
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
            chain
                .compressor_mut()
                .expect("compressor")
                .set_threshold_db(-30.0);
            chain.compressor_mut().expect("compressor").set_ratio(4.0);
            chain.set_makeup_db(makeup_db);
            let mut block = voice.clone();
            chain.process(&mut block, 1);
            reductions.push(chain.compressor().expect("compressor").reduction_db(0));
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
    fn the_fourth_order_high_pass_is_six_decibels_down_at_its_corner() {
        // Pinned because it is a trap for whoever writes a preset. "Order 4" reads as a
        // fourth-order Butterworth, which is 3 dB down at the corner; this is two identical
        // second-order sections, which is a Linkwitz-Riley and is 6 dB down. Two voice presets
        // were drafted against the textbook figures before anyone measured the real ones.
        let measure = |hz: Real, order: usize, at: Real| {
            let mut chain = bare(FS);
            chain.set_highpass(hz, order);
            chain.set_makeup_db(0.0);
            let mut block = tone(at, 0.1, 48_000);
            chain.process(&mut block, 1);
            let peak = block[24_000..].iter().fold(0.0_f32, |a, b| a.max(b.abs()));
            20.0 * (peak / 0.1).log10()
        };

        for (at, fourth, second) in [
            (120.0, -6.0, -3.0),
            (115.7, -6.7, -3.3),
            (100.0, -9.8, -4.9),
            (85.0, -13.9, -7.0),
        ] {
            let got = measure(120.0, 4, at);
            assert!(
                (got - fourth).abs() < 0.2,
                "120 Hz order 4 at {at} Hz: {got:.2} dB, expected {fourth}"
            );
            let got = measure(120.0, 2, at);
            assert!(
                (got - second).abs() < 0.2,
                "120 Hz order 2 at {at} Hz: {got:.2} dB, expected {second}"
            );
        }
    }

    #[test]
    fn a_rate_that_cannot_carry_the_corners_reports_it_rather_than_moving_them() {
        // The narrowband case, end to end. At 16 kHz a 5500 Hz de-esser cannot be built; the
        // high-pass at 80 Hz still can, and says so.
        let mut chain = InputChain::new(16_000.0);
        chain.set_highpass(80.0, 4);
        assert_eq!(chain.highpass_sections(), 2);
        assert!(!chain.deesser().expect("de-esser").is_active());

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
        assert!(chain.gate().expect("gate").gain(0) > 0.0);

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
    fn a_rate_change_resets_the_denoiser_once() {
        // Each stage resets itself as part of its own redesign, and the chain must not reset it
        // again on top: 0.3.0 did, and the second was eight network states rebuilt for nothing.
        // The output cannot show it — silence after one reset is silence after two — so the
        // stage counts.
        let mut chain = InputChain::new(FS);
        chain.set_denoise_enabled(true);
        let mut block = tone(300.0, 0.5, 4_800);
        chain.process(&mut block, 1);
        let before = chain.denoiser().expect("denoiser").resets;

        chain.set_sample_rate(44_100.0);
        let after = chain.denoiser().expect("denoiser").resets;
        assert_eq!(
            after - before,
            1,
            "the chain reset the denoiser on top of its own rate change"
        );

        chain.set_sample_rate(44_100.0);
        assert_eq!(
            chain.denoiser().expect("denoiser").resets,
            after,
            "the same rate again is not a change"
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

    #[test]
    fn every_spec_builds_at_every_rate_the_port_supports_and_ends_in_a_limiter() {
        for name in ChainSpec::NAMES {
            for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
                let spec = ChainSpec::by_name(name).expect(name);
                let mut chain = InputChain::from_spec(spec, rate);
                assert_eq!(chain.spec(), spec);
                let kinds: Vec<StageKind> = chain.stages().map(Stage::kind).collect();
                assert_eq!(kinds, spec.kinds().collect::<Vec<_>>(), "{name} at {rate}");
                assert!(chain.limiter().is_some(), "{name} at {rate}");
                let mut block = tone(300.0, 0.2, (rate / 10.0) as usize);
                chain.process(&mut block, 1);
                assert!(block.iter().all(|x| x.is_finite()));
            }
        }
    }

    #[test]
    fn a_spec_without_a_limiter_still_gets_one() {
        let chain = InputChain::from_spec(ChainSpec::custom(&[StageKind::Makeup]), FS);
        let kinds: Vec<StageKind> = chain.stages().map(Stage::kind).collect();
        assert_eq!(kinds, vec![StageKind::Makeup, StageKind::Limiter]);
        assert_eq!(
            chain.latency_frames(),
            48,
            "and it is the limiter that delays"
        );
        assert!(
            chain.gate().is_none(),
            "what the spec does not name is not there"
        );
        assert_eq!(chain.highpass_sections(), 0);
        assert_eq!(chain.makeup_db(), 0.0);
    }

    #[test]
    fn a_spec_without_a_gate_leaves_the_floor_alone() {
        let quiet = tone(300.0, db_to_linear(-60.0), 48_000);
        let mut voice = InputChain::new(FS);
        voice.set_compressor_enabled(false);
        voice.set_deesser_enabled(false);
        voice.set_makeup_db(0.0);
        let mut block = quiet.clone();
        voice.process(&mut block, 1);
        let gated = block[24_000..].iter().fold(0.0_f32, |a, b| a.max(b.abs()));

        let mut podcast = InputChain::from_spec(ChainSpec::podcast(), FS);
        podcast.set_compressor_enabled(false);
        podcast.set_deesser_enabled(false);
        podcast.set_makeup_db(0.0);
        assert!(podcast.gate().is_none());
        let mut block = quiet;
        podcast.process(&mut block, 1);
        let open = block[24_000..].iter().fold(0.0_f32, |a, b| a.max(b.abs()));
        assert!(
            linear_to_db(open) - linear_to_db(gated) > 10.0,
            "the podcast chain gated the floor: {gated} against {open}"
        );
    }

    #[test]
    fn apply_reaches_every_stage() {
        let mut chain = InputChain::new(FS);
        let mut params = InputDspParams {
            power: true,
            highpass_hz: 120.0,
            highpass_order: 4,
            rnnoise: true,
            denoise_level: DenoiseLevel::Strong,
            denoise_control: DenoiseLevel::Strong.control(),
            dereverb: DereverbLevel::Light,
            gate_on: false,
            eq_on: false,
            deesser_on: false,
            deesser_hz: 6_000.0,
            compressor_on: true,
            compressor_ratio: 5.0,
            makeup_db: 9.0,
            vad_gate: true,
            ..InputDspParams::default()
        };
        params.sanitise();
        chain.apply(&params);
        assert_eq!(chain.highpass_sections(), 2);
        assert!(chain.denoiser().expect("denoiser").is_active());
        assert_eq!(
            chain.denoiser().expect("denoiser").level(),
            DenoiseLevel::Strong
        );
        assert_eq!(
            chain.dereverb().expect("dereverb").level(),
            DereverbLevel::Light
        );
        assert!(!chain.gate().expect("gate").is_enabled());
        assert!(chain.gate().expect("gate").vad_gate());
        assert!(!chain.eq().expect("eq").is_enabled());
        assert!(!chain.deesser().expect("de-esser").is_active());
        assert_eq!(chain.deesser().expect("de-esser").frequency(), 6_000.0);
        assert!(chain.compressor().expect("compressor").is_enabled());
        assert_eq!(chain.makeup_db(), 9.0);
        assert_eq!(chain.latency_frames(), 48 + 960 + 480);

        params.power = false;
        chain.apply(&params);
        assert!(!chain.power());
    }

    #[test]
    fn the_denoisers_word_holds_the_gate_open_through_a_pause() {
        // The side-chain, end to end: the gate reads the voice probability the denoiser wrote
        // into the context. The network's opinion is forced so the test measures the wiring.
        let held_gain = |vad_gate: bool| {
            let mut chain = bare(FS);
            chain.set_gate_enabled(true);
            {
                let gate = chain.gate_mut().expect("gate");
                gate.set_threshold_db(-45.0);
                gate.set_range_db(-14.0);
                gate.set_times(1.0, 20.0);
                gate.set_hold_ms(50.0);
                gate.set_vad_gate(vad_gate);
            }
            chain.set_denoise_enabled(true);
            {
                let denoiser = chain.denoiser_mut().expect("denoiser");
                denoiser.set_level(DenoiseLevel::Light);
                denoiser.set_control(DenoiseLevel::Light.control());
                denoiser.vad_override = Some(0.95);
            }
            let mut word = tone(300.0, db_to_linear(-20.0), 24_000);
            chain.process(&mut word, 1);
            let mut pause = tone(300.0, db_to_linear(-60.0), 48_000);
            chain.process(&mut pause, 1);
            chain.gate().expect("gate").gain(0)
        };
        assert!(
            held_gain(true) > 0.99,
            "the gate closed with the network saying voice: {}",
            held_gain(true)
        );
        assert!(
            held_gain(false) < db_to_linear(-13.0),
            "without the side-chain the pause should close it: {}",
            held_gain(false)
        );
    }

    #[test]
    fn the_de_reverb_sits_between_the_denoiser_and_the_high_pass() {
        let kinds: Vec<StageKind> = InputChain::new(FS).stages().map(Stage::kind).collect();
        let at = |kind| kinds.iter().position(|k| *k == kind).expect("present");
        assert!(at(StageKind::Denoise) < at(StageKind::Dereverb));
        assert!(at(StageKind::Dereverb) < at(StageKind::HighPass));
        assert_eq!(kinds.last(), Some(&StageKind::Limiter));
    }

    #[test]
    fn a_stage_the_spec_does_not_hold_reads_an_empty_meter() {
        let chain = InputChain::from_spec(ChainSpec::podcast(), FS);
        assert_eq!(chain.meter(StageKind::Gate), StageMeter::default());
        assert!(chain.meter(StageKind::Limiter).running);
    }
}
