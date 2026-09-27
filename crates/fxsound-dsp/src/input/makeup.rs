//! Makeup gain: after everything that measures, before the limiter.
//!
//! Every threshold in the preset set was voiced against the signal as it arrives, not against one
//! already lifted, which is why this is a stage with a position rather than a multiplier folded
//! into the compressor. It is also the one control in the chain that can manufacture a sample
//! above full scale, which is why the limiter stands behind it and cannot be switched off.
//!
//! A new gain glides there over [`crate::smooth::GLIDE_SECONDS`] once the stage has been heard,
//! as the output chain's master gain does: the window's Makeup Gain slider on a microphone moves
//! it a decibel at a time, and a gain that stepped between two samples clicked at every one.

use crate::biquad::Real;
use crate::input::processor::{AudioProcessor, ProcessContext, StageMeter};
use crate::smooth::{Ramp, glide_frames};
use fxsound_core::messages::InputDspParams;

/// The most makeup a preset may ask for, either way, in dB.
pub const MAX_DB: Real = 24.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Makeup {
    db: Real,
    gain: Ramp,
    sample_rate: Real,
    /// Audio has gone through since the stage was built or cleared; until it has, a new gain
    /// lands at once.
    heard: bool,
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
        Self {
            db: 0.0,
            gain: Ramp::new(1.0),
            sample_rate: 48_000.0,
            heard: false,
        }
    }

    /// Gain in dB, clamped to ±[`MAX_DB`]; a value that is not a number is unity. Glides there
    /// once the stage has been heard.
    pub fn set_db(&mut self, db: Real) {
        let db = if db.is_finite() {
            db.clamp(-MAX_DB, MAX_DB)
        } else {
            0.0
        };
        self.db = db;
        let gain = 10.0_f32.powf(db / 20.0);
        if self.heard {
            self.gain.glide_to(gain, glide_frames(self.sample_rate));
        } else {
            self.gain = Ramp::new(gain);
        }
    }

    #[must_use]
    pub const fn db(&self) -> Real {
        self.db
    }

    /// The gain the stage is at, or gliding to.
    #[must_use]
    pub const fn gain(&self) -> Real {
        self.gain.target()
    }

    /// A whole interleaved block, in place: every channel of a frame alike.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !buffer.is_empty() {
            self.heard = true;
        }
        if !self.gain.is_gliding() {
            let gain = self.gain.value();
            if gain == 1.0 {
                return;
            }
            for sample in buffer.iter_mut() {
                *sample *= gain;
            }
            return;
        }
        for frame in buffer.chunks_mut(channels.max(1)) {
            let gain = self.gain.advance();
            for sample in frame.iter_mut() {
                *sample *= gain;
            }
        }
    }

    fn is_unity(&self) -> bool {
        self.gain.value() == 1.0 && !self.gain.is_gliding()
    }
}

impl AudioProcessor for Makeup {
    fn prepare(&mut self, sample_rate: Real) {
        self.sample_rate = sample_rate;
        self.gain.settle();
    }

    fn apply(&mut self, params: &InputDspParams) {
        self.set_db(params.makeup_db);
    }

    fn reset(&mut self) {
        self.gain.settle();
        self.heard = false;
    }

    fn is_active(&self) -> bool {
        !self.is_unity()
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        Makeup::process(self, buffer, ctx.channels);
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: 0.0,
            running: !self.is_unity(),
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
        makeup.process(&mut block, 1);
        assert_eq!(block, [0.1, -0.2, 0.3]);

        // A fresh stage, so the gain lands at once rather than gliding there.
        let mut makeup = Makeup::new();
        makeup.set_db(12.0);
        assert!(makeup.is_active());
        makeup.process(&mut block, 1);
        for (got, want) in block.iter().zip([0.1, -0.2, 0.3]) {
            assert!((got - want * 3.981).abs() < 1.0e-3, "{got} against {want}");
        }
    }

    #[test]
    fn a_new_gain_glides_there_once_the_stage_has_been_heard() {
        // The window's Makeup Gain slider on a microphone moves a decibel at a time; each move
        // used to be a step in the waveform.
        let mut makeup = Makeup::new();
        makeup.prepare(48_000.0);
        makeup.set_db(0.0);
        let mut heard = vec![0.5; 480];
        makeup.process(&mut heard, 1);
        makeup.set_db(6.0);
        assert!(
            (makeup.gain() - 1.995).abs() < 1.0e-3,
            "it says where it is going"
        );
        let mut block = vec![0.5; 1_000];
        makeup.process(&mut block, 1);
        let step = block
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, Real::max);
        assert!(step < 0.002, "a step of {step}");
        assert!((block[999] - 0.5 * 1.995).abs() < 1.0e-3, "it got there");
        // Every channel of a frame alike.
        makeup.set_db(0.0);
        let mut stereo = vec![0.5; 200];
        makeup.process(&mut stereo, 2);
        assert!(stereo.chunks(2).all(|frame| frame[0] == frame[1]));
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
