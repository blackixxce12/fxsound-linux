//! Makeup gain: after everything that measures, before the limiter.
//!
//! Every threshold in the preset set was voiced against the signal as it arrives, not against one
//! already lifted, which is why this is a stage with a position rather than a multiplier folded
//! into the compressor. It is also the one control in the chain that can manufacture a sample
//! above full scale, which is why the limiter stands behind it and cannot be switched off.

use crate::biquad::Real;
use crate::input::processor::{AudioProcessor, ProcessContext, StageMeter};
use fxsound_core::messages::InputDspParams;

/// The most makeup a preset may ask for, either way, in dB.
pub const MAX_DB: Real = 24.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Makeup {
    db: Real,
    gain: Real,
}

impl Default for Makeup {
    fn default() -> Self {
        Self::new()
    }
}

impl Makeup {
    /// Unity.
    #[must_use]
    pub const fn new() -> Self {
        Self { db: 0.0, gain: 1.0 }
    }

    /// Gain in dB, clamped to ±[`MAX_DB`]; a value that is not a number is unity.
    pub fn set_db(&mut self, db: Real) {
        let db = if db.is_finite() {
            db.clamp(-MAX_DB, MAX_DB)
        } else {
            0.0
        };
        self.db = db;
        self.gain = 10.0_f32.powf(db / 20.0);
    }

    #[must_use]
    pub const fn db(&self) -> Real {
        self.db
    }

    #[must_use]
    pub const fn gain(&self) -> Real {
        self.gain
    }

    /// A whole block, in place. Every channel alike, so the channel count does not matter.
    pub fn process(&self, buffer: &mut [Real]) {
        if self.gain == 1.0 {
            return;
        }
        for sample in buffer.iter_mut() {
            *sample *= self.gain;
        }
    }
}

impl AudioProcessor for Makeup {
    fn prepare(&mut self, _sample_rate: Real) {}

    fn apply(&mut self, params: &InputDspParams) {
        self.set_db(params.makeup_db);
    }

    fn reset(&mut self) {}

    fn is_active(&self) -> bool {
        self.gain != 1.0
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, buffer: &mut [Real], _ctx: &ProcessContext) {
        Makeup::process(self, buffer);
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: 0.0,
            running: self.gain != 1.0,
            aux: self.db,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_decibels_is_a_wire_and_twelve_is_four_times() {
        let mut makeup = Makeup::new();
        assert!(!makeup.is_active());
        let mut block = [0.1, -0.2, 0.3];
        makeup.process(&mut block);
        assert_eq!(block, [0.1, -0.2, 0.3]);

        makeup.set_db(12.0);
        assert!(makeup.is_active());
        makeup.process(&mut block);
        for (got, want) in block.iter().zip([0.1, -0.2, 0.3]) {
            assert!((got - want * 3.981).abs() < 1.0e-3, "{got} against {want}");
        }
    }

    #[test]
    fn it_is_clamped_and_a_nan_is_unity() {
        let mut makeup = Makeup::new();
        makeup.set_db(500.0);
        assert_eq!(makeup.db(), MAX_DB);
        makeup.set_db(-500.0);
        assert_eq!(makeup.db(), -MAX_DB);
        makeup.set_db(Real::NAN);
        assert_eq!(makeup.db(), 0.0);
        assert_eq!(makeup.gain(), 1.0);
    }
}
