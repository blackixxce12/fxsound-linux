//! The five effects and the chain that runs them.
//!
//! The public `Effect` enum in `fxsound-core` is ordered the way the GUI lays the sliders out.
//! The *processing* order is different and is fixed by `dspsPlayProcess32()`
//! (`dsp/ptechDsp/Play/Play32/Play32.c:640-878`):
//!
//! ```text
//! Fidelity -> Ambience -> Surround -> Bass -> Dynamic Boost
//! ```
//!
//! Getting that order wrong changes the sound: Dynamic Boost is last precisely because it has to
//! catch the headroom that Surround and Bass give away, and Ambience feeds the widener rather than
//! the other way round.
//!
//! Two effects the original still compiles are permanently disabled in the shipping build and are
//! not ported: vocal reduction (`Play32.c:442-637`, always off) and the eight-tap headphone delay
//! (`Play32.c:763-860`, always off).

pub mod ambience;
pub mod bass;
pub mod dynamic_boost;
pub mod fidelity;
pub mod surround;

use crate::biquad::Real;
use fxsound_core::{Effect as EffectId, messages::DspParams};

pub use ambience::Ambience;
pub use bass::Bass;
pub use dynamic_boost::DynamicBoost;
pub use fidelity::Fidelity;

/// Two distinct samples of one frame, by index.
///
/// Returns `None` when either index is out of range or they are the same channel, so a caller
/// with a nonsensical layout simply does nothing rather than panicking on the audio thread.
pub(crate) fn pair_mut(
    frame: &mut [Real],
    left: usize,
    right: usize,
) -> Option<(&mut Real, &mut Real)> {
    if left == right || left >= frame.len() || right >= frame.len() {
        return None;
    }
    let (low, high) = if left < right {
        (left, right)
    } else {
        (right, left)
    };
    let (head, tail) = frame.split_at_mut(high);
    let first = head.get_mut(low)?;
    let second = tail.first_mut()?;
    Some(if left < right {
        (first, second)
    } else {
        (second, first)
    })
}
pub use surround::Surround;

/// The largest block a single `process` call may carry (`DAW_MAX_BUFFER_SIZE`, `u_dfxp.h:46`).
pub const MAX_BLOCK_FRAMES: usize = 16_384;
/// `DAW_MAX_SAMPLING_FREQ` (`u_dfxp.h:45`).
pub const MAX_SAMPLE_RATE: Real = 192_000.0;
/// `DAW_MIN_SAMPLING_FREQ` (`u_dfxp.h:44`).
pub const MIN_SAMPLE_RATE: Real = 16_000.0;

/// One audio effect.
///
/// Implementors own every buffer they need and never allocate once constructed, because
/// [`Effect::process`] runs on the PipeWire real-time thread.
pub trait Effect {
    /// The stream format changed. Redesign anything rate-dependent and clear history.
    fn set_sample_rate(&mut self, sample_rate: Real);

    /// The user's knob, `0.0..=1.0`.
    ///
    /// The original carries this value as a 0..10 slider in the GUI and as MIDI 0..127 in presets
    /// and in the engine; both normalise to this range, and the conversion happens once, at the
    /// edges of the system.
    ///
    /// The effect glides to the new amount over [`crate::smooth::GLIDE_SECONDS`] rather than
    /// jumping to it (audit report #11), and one taken to zero stays active until it has faded out.
    /// [`Effect::settle`] lands it at once.
    fn set_amount(&mut self, amount: Real);

    /// Finish every glide at once, as if it had already run: for a stage nobody has heard yet,
    /// where there is nothing to glide from. [`Chain`] calls it until audio has gone through.
    fn settle(&mut self);

    fn amount(&self) -> Real;

    /// When `false`, [`Chain`] skips [`Effect::process`] entirely.
    ///
    /// The original bypasses each effect at a value of exactly zero by clearing its `*_on` flag
    /// (`DfxDspPrivate.cpp:295-302`), which is a true bypass rather than a unity-gain pass. Here
    /// that bypass waits for the fade to zero to finish.
    fn is_active(&self) -> bool;

    /// Clear all history without changing the design. A glide under way is history too: the
    /// effect lands on its amount.
    fn reset(&mut self);

    /// Interleaved, in place, nominal range ±1.0.
    fn process(&mut self, buffer: &mut [Real], channels: usize);

    /// Added latency in frames.
    fn latency_frames(&self) -> usize {
        0
    }
}

/// The five effects wired up in processing order.
#[derive(Debug)]
pub struct Chain {
    fidelity: Fidelity,
    ambience: Ambience,
    surround: Surround,
    bass: Bass,
    dynamic_boost: DynamicBoost,
    sample_rate: Real,
    power: bool,
    /// Audio has gone through since the chain was built or last cleared. Until it has, a new
    /// amount lands at once (see [`crate::smooth`]). The chain keeps this rather than each effect,
    /// because an effect switched off is heard too, as the signal it lets through, and only the
    /// chain sees that.
    heard: bool,
}

impl Chain {
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let sample_rate = sample_rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
        Self {
            fidelity: Fidelity::new(sample_rate),
            ambience: Ambience::new(sample_rate),
            surround: Surround::new(sample_rate),
            bass: Bass::new(sample_rate),
            dynamic_boost: DynamicBoost::new(sample_rate),
            sample_rate,
            power: true,
            heard: false,
        }
    }

    /// Master bypass. When off the chain does not touch the buffer at all.
    pub fn set_power(&mut self, on: bool) {
        if self.power != on {
            self.power = on;
            // Coming back from a bypass with stale reverb tails and limiter state would click.
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

    /// Tell the stages that care which channels are the front pair: the two stereo-by-nature ones,
    /// and Dynamic Boost, whose level estimator listens to the pair and whose limiter turns both
    /// sides of it down together (audit #7 and R2).
    ///
    /// `None` means the layout does not name one, in which case they fall back to the first two
    /// channels, which is what they always did.
    pub fn set_front_pair(&mut self, pair: Option<(usize, usize)>) {
        self.surround.set_front_pair(pair);
        self.ambience.set_front_pair(pair);
        self.dynamic_boost.set_front_pair(pair);
    }

    /// Tell the stages that must not touch the subwoofer which channel it is.
    ///
    /// Only Fidelity acts on this today; Ambience and Surround already confine themselves to the
    /// front pair, and Bass and Dynamic Boost are meant to reach every channel. Dynamic Boost
    /// limits the subwoofer on an envelope of its own, which it learns from the channel sides
    /// ([`Chain::set_channel_sides`]).
    pub fn set_lfe_channel(&mut self, channel: Option<usize>) {
        self.fidelity.set_lfe_channel(channel);
    }

    /// Tell Dynamic Boost which side of the room each channel stands on, so that its limiter
    /// turns every speaker either side of the listener down together and the centre and the
    /// subwoofer each on their own (audit R2, [`DynamicBoost::set_channel_sides`]). The engine
    /// passes the sides it balances by.
    pub fn set_channel_sides(&mut self, sides: &[crate::engine::ChannelSide]) {
        self.dynamic_boost.set_channel_sides(Some(sides));
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sample_rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.fidelity.set_sample_rate(sample_rate);
        self.ambience.set_sample_rate(sample_rate);
        self.surround.set_sample_rate(sample_rate);
        self.bass.set_sample_rate(sample_rate);
        self.dynamic_boost.set_sample_rate(sample_rate);
    }

    /// Set one effect from the GUI-facing enum: gliding to the amount once the chain has been
    /// heard, at once before.
    pub fn set_effect(&mut self, effect: EffectId, amount: Real) {
        let amount = amount.clamp(0.0, 1.0);
        let stage: &mut dyn Effect = match effect {
            EffectId::Fidelity => &mut self.fidelity,
            EffectId::Ambience => &mut self.ambience,
            EffectId::Surround => &mut self.surround,
            EffectId::DynamicBoost => &mut self.dynamic_boost,
            EffectId::Bass => &mut self.bass,
        };
        stage.set_amount(amount);
        if !self.heard {
            stage.settle();
        }
    }

    #[must_use]
    pub fn effect(&self, effect: EffectId) -> Real {
        match effect {
            EffectId::Fidelity => self.fidelity.amount(),
            EffectId::Ambience => self.ambience.amount(),
            EffectId::Surround => self.surround.amount(),
            EffectId::DynamicBoost => self.dynamic_boost.amount(),
            EffectId::Bass => self.bass.amount(),
        }
    }

    /// Apply a whole parameter snapshot.
    pub fn apply(&mut self, params: &DspParams) {
        self.set_power(params.power);
        for id in EffectId::ALL {
            self.set_effect(id, params.effect(id));
        }
    }

    /// Clear every effect's history. Until audio goes through again, a new amount lands at once.
    pub fn reset(&mut self) {
        self.fidelity.reset();
        self.ambience.reset();
        self.surround.reset();
        self.bass.reset();
        self.dynamic_boost.reset();
        self.heard = false;
    }

    /// Total added latency. Only the limiter's look-ahead contributes.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        self.fidelity.latency_frames()
            + self.ambience.latency_frames()
            + self.surround.latency_frames()
            + self.bass.latency_frames()
            + self.dynamic_boost.latency_frames()
    }

    /// Run the chain over one interleaved block, in place.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.power {
            // A true bypass: the original leaves the buffer untouched (`Play32.c:436-440`).
            return;
        }
        if !buffer.is_empty() {
            self.heard = true;
        }
        if self.fidelity.is_active() {
            self.fidelity.process(buffer, channels);
        }
        if self.ambience.is_active() {
            self.ambience.process(buffer, channels);
        }
        if self.surround.is_active() {
            self.surround.process(buffer, channels);
        }
        if self.bass.is_active() {
            self.bass.process(buffer, channels);
        }
        // Dynamic Boost always runs: it is the safety net for everything above it.
        self.dynamic_boost.process(buffer, channels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, channels: usize, amplitude: Real) -> Vec<Real> {
        (0..frames)
            .flat_map(|n| {
                let s = (n as Real * 0.05).sin() * amplitude;
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    #[test]
    fn power_off_is_an_exact_bypass() {
        let mut chain = Chain::new(48_000.0);
        for id in EffectId::ALL {
            chain.set_effect(id, 1.0);
        }
        chain.set_power(false);

        let original = tone(1024, 2, 0.5);
        let mut buffer = original.clone();
        chain.process(&mut buffer, 2);
        assert_eq!(buffer, original);
    }

    #[test]
    fn the_processing_order_is_not_the_enum_order() {
        // A guard against someone "tidying" the chain into enum order later.
        assert_eq!(EffectId::ALL[2], EffectId::Surround);
        assert_eq!(EffectId::ALL[3], EffectId::DynamicBoost);
        assert_eq!(EffectId::ALL[4], EffectId::Bass);
        // Bass runs before Dynamic Boost even though its enum index is higher.
    }

    #[test]
    fn every_effect_round_trips_through_the_chain_accessors() {
        let mut chain = Chain::new(48_000.0);
        for (i, id) in EffectId::ALL.into_iter().enumerate() {
            let value = (i as Real + 1.0) / 10.0;
            chain.set_effect(id, value);
            assert!((chain.effect(id) - value).abs() < 1e-6, "{id:?}");
        }
    }

    #[test]
    fn a_snapshot_drives_every_effect() {
        let mut chain = Chain::new(48_000.0);
        let mut params = DspParams::default();
        params.set_effect(EffectId::Bass, 0.8);
        params.set_effect(EffectId::Fidelity, 0.4);
        chain.apply(&params);
        assert!((chain.effect(EffectId::Bass) - 0.8).abs() < 1e-6);
        assert!((chain.effect(EffectId::Fidelity) - 0.4).abs() < 1e-6);
        assert!(chain.power());
    }

    #[test]
    fn the_chain_stays_finite_with_everything_at_maximum() {
        let mut chain = Chain::new(48_000.0);
        for id in EffectId::ALL {
            chain.set_effect(id, 1.0);
        }
        let mut buffer = tone(48_000, 2, 0.9);
        chain.process(&mut buffer, 2);
        assert!(buffer.iter().all(|s| s.is_finite()));
        // Dynamic Boost is the last stage, so nothing may leave above its ceiling.
        assert!(
            buffer.iter().all(|s| s.abs() <= 1.0),
            "the limiter let a sample through above full scale"
        );
    }

    #[test]
    fn changing_the_sample_rate_is_safe_mid_stream() {
        let mut chain = Chain::new(48_000.0);
        for id in EffectId::ALL {
            chain.set_effect(id, 0.6);
        }
        let mut buffer = tone(512, 2, 0.5);
        chain.process(&mut buffer, 2);
        chain.set_sample_rate(96_000.0);
        chain.process(&mut buffer, 2);
        assert!(buffer.iter().all(|s| s.is_finite()));
        assert_eq!(chain.sample_rate(), 96_000.0);
    }

    #[test]
    fn an_effect_switched_off_and_on_by_snapshot_starts_from_rest() {
        // Audit report #9, through the path the engine takes: a snapshot that zeroes an effect and
        // one that brings it back. Bass, Fidelity and Ambience keep history; each must start
        // clean, so silence in is silence out behind Dynamic Boost, which is never bypassed.
        for effect in [EffectId::Bass, EffectId::Fidelity, EffectId::Ambience] {
            let mut chain = Chain::new(48_000.0);
            let mut on = DspParams::default();
            on.set_effect(effect, 1.0);
            let off = DspParams::default();
            chain.apply(&on);
            let mut loud = tone(48_000, 2, 0.5);
            chain.process(&mut loud, 2);

            chain.apply(&off);
            let mut quiet = vec![0.0; 2 * 4_800];
            chain.process(&mut quiet, 2);
            chain.apply(&on);
            let mut silence = vec![0.0; 2 * 9_600];
            chain.process(&mut silence, 2);
            let peak = silence.iter().fold(0.0, |m: Real, s| m.max(s.abs()));
            assert!(peak < 1e-6, "{effect:?} came back with old audio at {peak}");
        }
    }

    #[test]
    fn sample_rates_outside_the_supported_range_are_clamped() {
        let mut chain = Chain::new(8_000.0);
        assert_eq!(chain.sample_rate(), MIN_SAMPLE_RATE);
        chain.set_sample_rate(384_000.0);
        assert_eq!(chain.sample_rate(), MAX_SAMPLE_RATE);
    }

    #[test]
    fn dynamic_boost_hears_the_front_pair_the_chain_is_told_about() {
        // Audit #7 through the path the engine takes: a device ordered [FC, FL, FR], a mix loud
        // only on the right, slider 10. Told the pair, Dynamic Boost backs off as it does on plain
        // stereo and the right peaks near 0.62; hearing the centre and the left instead, it gave
        // the full +11.6 dB and the right sat on the ceiling, 5.6 dB into the limiter.
        let run = |pair: Option<(usize, usize)>| {
            let mut chain = Chain::new(48_000.0);
            chain.set_effect(EffectId::DynamicBoost, 1.0);
            chain.set_front_pair(pair);
            let frames = 480_000;
            let mut buffer = vec![0.0; frames * 3];
            for (n, frame) in buffer.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let t = n as Real / 48_000.0;
                frame[0] = 0.05 * (std::f32::consts::TAU * 220.0 * t).sin();
                frame[1] = 0.05 * (std::f32::consts::TAU * 440.0 * t).sin();
                frame[2] = 0.5 * (std::f32::consts::TAU * 660.0 * t).sin();
            }
            chain.process(&mut buffer, 3);
            buffer[(frames - 48_000) * 3..]
                .iter()
                .skip(2)
                .step_by(3)
                .fold(0.0, |m: Real, s| m.max(s.abs()))
        };
        let told = run(Some((1, 2)));
        let not_told = run(None);
        assert!(told < 0.63, "the right peaks at {told}");
        assert!(
            not_told > 0.95,
            "the fixture should show the defect: {not_told}"
        );
    }
}
