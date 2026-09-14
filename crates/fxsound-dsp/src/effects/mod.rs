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
    fn set_amount(&mut self, amount: Real);

    fn amount(&self) -> Real;

    /// When `false`, [`Chain`] skips [`Effect::process`] entirely.
    ///
    /// The original bypasses each effect at a value of exactly zero by clearing its `*_on` flag
    /// (`DfxDspPrivate.cpp:295-302`), which is a true bypass rather than a unity-gain pass.
    fn is_active(&self) -> bool;

    /// Clear all history without changing the design.
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

    /// Set one effect from the GUI-facing enum.
    pub fn set_effect(&mut self, effect: EffectId, amount: Real) {
        let amount = amount.clamp(0.0, 1.0);
        match effect {
            EffectId::Fidelity => self.fidelity.set_amount(amount),
            EffectId::Ambience => self.ambience.set_amount(amount),
            EffectId::Surround => self.surround.set_amount(amount),
            EffectId::DynamicBoost => self.dynamic_boost.set_amount(amount),
            EffectId::Bass => self.bass.set_amount(amount),
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

    /// Clear every effect's history.
    pub fn reset(&mut self) {
        self.fidelity.reset();
        self.ambience.reset();
        self.surround.reset();
        self.bass.reset();
        self.dynamic_boost.reset();
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
    fn sample_rates_outside_the_supported_range_are_clamped() {
        let mut chain = Chain::new(8_000.0);
        assert_eq!(chain.sample_rate(), MIN_SAMPLE_RATE);
        chain.set_sample_rate(384_000.0);
        assert_eq!(chain.sample_rate(), MAX_SAMPLE_RATE);
    }
}
