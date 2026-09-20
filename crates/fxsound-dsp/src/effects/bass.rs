//! Bass — a single parametric peaking biquad at 90 Hz.
//!
//! There is no separate DSP module in the original: Bass is implemented inline in the Play host
//! (`dsp/ptechDsp/Play/Play32/Play32.c:692-759`) as one transposed-direct-form-II section
//! specialised for `b1 == a1`, which is exactly the specialisation
//! [`crate::biquad::Section::tick`] already implements.
//!
//! The filter is designed by the same `filtCalcParametric` the graphic equalizer uses, so there is
//! nothing new here beyond the fixed centre frequency, the fixed Q and the dB mapping.

use super::Effect;
use crate::biquad::{BiquadCoeffs, Real, Section, calc_parametric};

/// `DSP_PLY_BASSBOOST_CENTER_FREQ` (`c_play.h:124-127`).
pub const CENTER_HZ: Real = 90.0;
/// `DSP_PLY_BASSBOOST_Q`.
pub const Q: Real = 2.5;
/// `DSP_PLY_BASSBOOST_MAX_VALUE` — the boost at a full slider, in dB.
pub const MAX_BOOST_DB: Real = 15.0;

/// The bass boost.
#[derive(Clone, Debug)]
pub struct Bass {
    section: Section,
    sample_rate: Real,
    amount: Real,
    boost_db: Real,
}

impl Bass {
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        Self {
            section: Section::new(),
            sample_rate,
            amount: 0.0,
            boost_db: 0.0,
        }
    }

    /// The boost currently applied, in dB.
    #[must_use]
    pub const fn boost_db(&self) -> Real {
        self.boost_db
    }

    /// The designed coefficients, exposed for tests and for drawing a response curve.
    #[must_use]
    pub const fn coeffs(&self) -> BiquadCoeffs {
        self.section.coeffs
    }

    fn design(&mut self) {
        self.section.coeffs = calc_parametric(self.sample_rate, CENTER_HZ, self.boost_db, Q);
    }
}

impl Effect for Bass {
    fn set_sample_rate(&mut self, sample_rate: Real) {
        if sample_rate == self.sample_rate || sample_rate <= 0.0 {
            return;
        }
        self.sample_rate = sample_rate;
        self.design();
        self.reset();
    }

    fn set_amount(&mut self, amount: Real) {
        self.amount = amount.clamp(0.0, 1.0);
        // Linear in MIDI from 0 to 15 dB (`QntitoBoostCut.cpp:75, 89`).
        self.boost_db = MAX_BOOST_DB * self.amount;
        self.design();
    }

    fn amount(&self) -> Real {
        self.amount
    }

    fn is_active(&self) -> bool {
        self.amount != 0.0 && self.section.coeffs.on
    }

    fn reset(&mut self) {
        self.section.reset();
    }

    fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || channels > crate::biquad::MAX_CHANNELS || !self.section.coeffs.on {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = self.section.tick(channel, *sample);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biquad::magnitude;

    /// `docs/spec/10-dsp-effects.md` §10.2.
    #[test]
    fn the_boost_mapping_matches_the_reference_table() {
        let mut b = Bass::new(48_000.0);
        for (midi, expected_db) in [
            (0_u8, 0.000),
            (13, 1.535),
            (51, 6.024),
            (68, 8.031),
            (102, 12.047),
            (127, 15.000),
        ] {
            b.set_amount(fxsound_core::scale::midi_to_value(midi));
            assert!(
                (b.boost_db() - expected_db).abs() < 1e-2,
                "midi {midi}: got {}, expected {expected_db}",
                b.boost_db()
            );
        }
    }

    #[test]
    fn a_full_boost_lifts_90hz_by_15db() {
        let mut b = Bass::new(48_000.0);
        b.set_amount(1.0);
        let db = 20.0 * magnitude(&b.coeffs(), CENTER_HZ / 48_000.0).log10();
        assert!((db - 15.0).abs() < 0.3, "measured {db} dB at 90 Hz");
    }

    #[test]
    fn the_boost_is_local_to_the_low_end() {
        let mut b = Bass::new(48_000.0);
        b.set_amount(1.0);
        // Q 2.5 at 90 Hz: by 1 kHz the lift should be all but gone.
        let db = 20.0 * magnitude(&b.coeffs(), 1000.0 / 48_000.0).log10();
        assert!(db.abs() < 1.0, "1 kHz moved by {db} dB");
    }

    #[test]
    fn zero_amount_bypasses_the_section() {
        let mut b = Bass::new(48_000.0);
        b.set_amount(0.0);
        assert!(!b.is_active());
        assert_eq!(b.coeffs(), BiquadCoeffs::UNITY);

        let original: Vec<Real> = (0..512).map(|n| (n as Real * 0.02).sin()).collect();
        let mut buffer = original.clone();
        b.process(&mut buffer, 2);
        assert_eq!(buffer, original);
    }

    #[test]
    fn a_low_tone_gets_louder_and_a_high_one_does_not() {
        let rms = |v: &[Real]| (v.iter().map(|s| s * s).sum::<Real>() / v.len() as Real).sqrt();
        let tone = |hz: Real, frames: usize| -> Vec<Real> {
            (0..frames)
                .flat_map(|n| {
                    let s = (n as Real * hz * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
                    [s, s]
                })
                .collect()
        };

        let mut b = Bass::new(48_000.0);
        b.set_amount(1.0);

        let low = tone(90.0, 24_000);
        let mut processed_low = low.clone();
        b.process(&mut processed_low, 2);
        // Skip the transient while the filter settles.
        let settled = processed_low.len() / 2;
        assert!(
            rms(&processed_low[settled..]) > rms(&low[settled..]) * 3.0,
            "90 Hz was not boosted"
        );

        b.reset();
        let high = tone(4000.0, 24_000);
        let mut processed_high = high.clone();
        b.process(&mut processed_high, 2);
        let settled = processed_high.len() / 2;
        let ratio = rms(&processed_high[settled..]) / rms(&high[settled..]);
        assert!(
            (ratio - 1.0).abs() < 0.1,
            "4 kHz changed by a factor of {ratio}"
        );
    }

    #[test]
    fn the_design_uses_the_shared_parametric_routine() {
        // Guards against someone substituting an RBJ cookbook peaking filter, which is not
        // bit-identical and differs audibly at Q 2.5 / 90 Hz / 15 dB.
        let mut b = Bass::new(44_100.0);
        b.set_amount(1.0);
        let expected = calc_parametric(44_100.0, CENTER_HZ, MAX_BOOST_DB, Q);
        assert_eq!(b.coeffs(), expected);
        assert_eq!(
            b.coeffs().a1,
            b.coeffs().b1,
            "the TDF-II specialisation needs a1 == b1"
        );
    }

    #[test]
    fn a_sample_rate_change_redesigns_the_filter() {
        let mut b = Bass::new(44_100.0);
        b.set_amount(1.0);
        let before = b.coeffs();
        b.set_sample_rate(96_000.0);
        assert_ne!(before, b.coeffs());
        let db = 20.0 * magnitude(&b.coeffs(), CENTER_HZ / 96_000.0).log10();
        assert!(
            (db - 15.0).abs() < 0.3,
            "measured {db} dB after the rate change"
        );
    }

    #[test]
    fn channels_are_filtered_independently() {
        let mut b = Bass::new(48_000.0);
        b.set_amount(1.0);
        let mut buffer: Vec<Real> = (0..1024)
            .flat_map(|n| [(n as Real * 0.01).sin(), 0.0])
            .collect();
        b.process(&mut buffer, 2);
        assert!(
            buffer.as_chunks::<2>().0.iter().all(|f| f[1].abs() < 1e-20),
            "the right channel picked up the left channel's signal"
        );
    }
}
