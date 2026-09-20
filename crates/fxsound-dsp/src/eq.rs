//! The graphic equalizer: band layout, Q derivation and the cascade that runs on the audio thread.
//!
//! Ports `dsp/ptutil/DspUtil/GraphicEq/GraphicEqSet.cpp` and the parts of `dsp/DfxDspEq.cpp` that
//! decide which bands exist and what Q they get.
//!
//! The whole structure is allocated once with room for [`biquad::SOS_MAX_SECTIONS`] bands, so
//! changing the band count, the sample rate or a preset never allocates on the audio thread.

use crate::biquad::{
    BiquadCoeffs, MAX_BOOST_OR_CUT_DB, Real, SOS_MAX_SECTIONS, Section, calc_parametric, magnitude,
};

/// `GraphicEqInit.cpp:49` — the Q multiplier before the user touches the filter-width knob.
pub const DEFAULT_Q_MULTIPLIER: Real = 1.0;
/// The filter-width slider's range (`FxAudioControls.cpp:348`).
pub const MIN_Q_MULTIPLIER: Real = 1.0;
pub const MAX_Q_MULTIPLIER: Real = 3.0;
/// `GraphicEqSet.cpp:555-559` — a band centre is clamped to this before anything else.
pub const MIN_BAND_FREQ_HZ: Real = 10.0;
pub const MAX_BAND_FREQ_HZ: Real = 21_000.0;

/// The hard-coded ladders (`GraphicEqSet.cpp:430-492`), with the band edges each one implies.
///
/// Returns `(frequencies, min_band_freq, max_band_freq)`. Any other count falls back to a
/// geometric ladder, as the original does.
#[must_use]
pub fn band_table(num_bands: usize) -> Option<(&'static [Real], Real, Real)> {
    const F5: [Real; 5] = [62.5, 250.0, 1000.0, 4000.0, 16000.0];
    const F10: [Real; 10] = [
        62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48, 16000.0,
    ];
    const F15: [Real; 15] = [
        25.0, 40.0, 63.0, 100.0, 160.0, 250.0, 400.0, 630.0, 1000.0, 1600.0, 2500.0, 4000.0,
        6300.0, 10000.0, 16000.0,
    ];
    const F20: [Real; 20] = [
        20.0, 31.5, 40.0, 63.0, 80.0, 125.0, 160.0, 250.0, 315.0, 500.0, 630.0, 1000.0, 1250.0,
        2000.0, 2500.0, 4000.0, 5000.0, 8000.0, 10000.0, 16000.0,
    ];
    const F31: [Real; 31] = [
        20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
        500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0,
        6300.0, 8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
    ];

    match num_bands {
        5 => Some((&F5, 62.5, 16000.0)),
        10 => Some((&F10, 62.5, 16000.0)),
        15 => Some((&F15, 25.0, 16000.0)),
        20 => Some((&F20, 20.0, 16000.0)),
        31 => Some((&F31, 20.0, 20000.0)),
        _ => None,
    }
}

/// The generic geometric ladder used for band counts with no table
/// (`GraphicEqSet.cpp:495-508`). Computed in `f64`, stored as `f32`, like the original.
fn geometric_ladder(num_bands: usize, min_hz: f64, max_hz: f64, out: &mut [Real]) {
    if num_bands == 1 {
        out[0] = min_hz as Real;
        return;
    }
    let ratio = max_hz / min_hz;
    for (i, slot) in out.iter_mut().take(num_bands).enumerate() {
        *slot = (min_hz * ratio.powf(i as f64 / (num_bands as f64 - 1.0))) as Real;
    }
}

/// The constant-Q derivation (`GraphicEqSet.cpp:512-525`).
///
/// `Q = sqrt(r) / (r - 1)` where `r` is the ratio between adjacent band centres, so adjacent bands
/// meet at their geometric midpoints. Then the user's multiplier, then a hard floor of 1.
#[must_use]
pub fn derive_q(min_hz: f64, max_hz: f64, num_bands: usize, q_multiplier: Real) -> Real {
    if num_bands <= 1 {
        return 1.0;
    }
    let r = (max_hz / min_hz).powf(1.0 / (num_bands as f64 - 1.0));
    let mut q = (r.sqrt() / (r - 1.0)) as Real;
    q *= q_multiplier;
    if q < 1.0 {
        q = 1.0;
    }
    q
}

/// The half-step geometric edges the GUI uses as each band's frequency slider range
/// (`GraphicEqGet.cpp:105-168`), including the asymmetric `+1` / `+10` nudge on the low edge.
#[must_use]
pub fn band_frequency_range(
    band: usize,
    num_bands: usize,
    min_hz: Real,
    max_hz: Real,
) -> (Real, Real) {
    if num_bands <= 1 {
        return (min_hz, max_hz);
    }
    let ratio = f64::from(max_hz) / f64::from(min_hz);
    let denominator = (num_bands * 2 - 2) as f64;
    let one_based = band + 1;

    let low = if band == 0 {
        min_hz
    } else {
        let exponent = ((one_based as f64 - 1.0) * 2.0 - 1.0) / denominator;
        let edge = (f64::from(min_hz) * ratio.powf(exponent)).round() as Real;
        if edge < 1000.0 {
            edge + 1.0
        } else {
            edge + 10.0
        }
    };

    let high = if one_based == num_bands {
        max_hz
    } else {
        let exponent = (one_based as f64 * 2.0 - 1.0) / denominator;
        (f64::from(min_hz) * ratio.powf(exponent)).round() as Real
    };

    (low, high)
}

/// The graphic equalizer: a cascade of peaking sections, one per band.
#[derive(Clone, Debug)]
pub struct GraphicEq {
    sections: [Section; SOS_MAX_SECTIONS],
    center_hz: [Real; SOS_MAX_SECTIONS],
    /// The boost last *installed* into each section. The original compares against this with exact
    /// float equality to skip redundant redesigns, and so does this.
    installed_boost_db: [Real; SOS_MAX_SECTIONS],
    /// What the user asked for, which survives a band being bypassed at Nyquist.
    requested_boost_db: [Real; SOS_MAX_SECTIONS],
    num_bands: usize,
    min_band_hz: Real,
    max_band_hz: Real,
    q_multiplier: Real,
    q: Real,
    sample_rate: Real,
    enabled: bool,
}

impl GraphicEq {
    /// Build an equalizer with the default ten-band layout at 48 kHz.
    #[must_use]
    pub fn new() -> Self {
        let mut eq = Self {
            sections: [Section::new(); SOS_MAX_SECTIONS],
            center_hz: [0.0; SOS_MAX_SECTIONS],
            installed_boost_db: [0.0; SOS_MAX_SECTIONS],
            requested_boost_db: [0.0; SOS_MAX_SECTIONS],
            num_bands: 10,
            min_band_hz: 62.5,
            max_band_hz: 16000.0,
            q_multiplier: DEFAULT_Q_MULTIPLIER,
            q: 1.0,
            sample_rate: 48_000.0,
            enabled: true,
        };
        eq.set_num_bands(10);
        eq
    }

    #[must_use]
    pub const fn num_bands(&self) -> usize {
        self.num_bands
    }

    #[must_use]
    pub const fn sample_rate(&self) -> Real {
        self.sample_rate
    }

    #[must_use]
    pub const fn q(&self) -> Real {
        self.q
    }

    #[must_use]
    pub const fn q_multiplier(&self) -> Real {
        self.q_multiplier
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// The centre frequencies of the live bands.
    #[must_use]
    pub fn center_frequencies(&self) -> &[Real] {
        &self.center_hz[..self.num_bands]
    }

    /// The boost the user asked for, per live band.
    #[must_use]
    pub fn boosts_db(&self) -> &[Real] {
        &self.requested_boost_db[..self.num_bands]
    }

    #[must_use]
    pub fn band_range(&self, band: usize) -> (Real, Real) {
        band_frequency_range(band, self.num_bands, self.min_band_hz, self.max_band_hz)
    }

    /// Rebuild the band ladder for a new band count, resetting every gain to flat.
    ///
    /// Clamped to `1..=SOS_MAX_SECTIONS`, as `GraphicEqNew` does.
    pub fn set_num_bands(&mut self, num_bands: usize) {
        let num_bands = num_bands.clamp(1, SOS_MAX_SECTIONS);
        self.num_bands = num_bands;

        if let Some((table, min_hz, max_hz)) = band_table(num_bands) {
            self.min_band_hz = min_hz;
            self.max_band_hz = max_hz;
            self.center_hz[..num_bands].copy_from_slice(table);
        } else {
            // Keep whatever edges are current and spread the bands geometrically between them.
            let (min_hz, max_hz) = (f64::from(self.min_band_hz), f64::from(self.max_band_hz));
            geometric_ladder(num_bands, min_hz, max_hz, &mut self.center_hz);
        }

        self.recompute_q();
        for band in 0..num_bands {
            self.requested_boost_db[band] = 0.0;
            self.installed_boost_db[band] = 0.0;
            self.sections[band].coeffs = BiquadCoeffs::UNITY;
            self.sections[band].reset();
        }
    }

    /// Set the filter-width multiplier.
    ///
    /// The original resets the whole frequency table here, discarding preset-supplied band
    /// frequencies and forcing the sample rate back to 44100 until the next buffer corrects it
    /// (`GraphicEqSet.cpp:115` into `GraphicEqInitSections.cpp:43-52`). The spec flags that as a
    /// bug; this port keeps the layout and only redesigns the coefficients.
    pub fn set_q_multiplier(&mut self, multiplier: Real) {
        let multiplier = multiplier.clamp(MIN_Q_MULTIPLIER, MAX_Q_MULTIPLIER);
        if multiplier == self.q_multiplier {
            return;
        }
        self.q_multiplier = multiplier;
        self.recompute_q();
        self.redesign_all();
    }

    /// Move one band's centre frequency, clamped the way `GraphicEqSetBandFreq` clamps it.
    pub fn set_band_frequency(&mut self, band: usize, freq_hz: Real) {
        if band >= self.num_bands {
            return;
        }
        let freq = freq_hz.clamp(MIN_BAND_FREQ_HZ, MAX_BAND_FREQ_HZ);
        if freq == self.center_hz[band] {
            return;
        }
        self.center_hz[band] = freq;
        // Force a redesign: the boost has not changed, so the equality guard would skip it.
        self.installed_boost_db[band] = Real::NAN;
        let boost = self.requested_boost_db[band];
        self.set_band_boost(band, boost);
    }

    /// Set one band's boost or cut in dB (`GraphicEqSetBandBoostCut`, `GraphicEqSet.cpp:258-313`).
    pub fn set_band_boost(&mut self, band: usize, boost_db: Real) {
        if band >= self.num_bands {
            return;
        }
        self.requested_boost_db[band] = boost_db;

        let f0 = self.center_hz[band];
        // Bypass on an exact zero, or when the band sits at or above Nyquist. Note the comparison
        // is `2*f0 >= fs`, not `f0 >= fs/2` — at 44.1 kHz a 20 kHz band survives, at 40 kHz it
        // does not.
        if boost_db == 0.0 || f0 * 2.0 >= self.sample_rate {
            self.sections[band].coeffs = BiquadCoeffs::UNITY;
            self.installed_boost_db[band] = 0.0;
            return;
        }

        let clamped = boost_db.clamp(-MAX_BOOST_OR_CUT_DB, MAX_BOOST_OR_CUT_DB);
        if clamped == self.installed_boost_db[band] {
            return;
        }
        self.sections[band].coeffs = calc_parametric(self.sample_rate, f0, clamped, self.q);
        self.installed_boost_db[band] = clamped;
    }

    /// Apply a whole curve at once — the preset-load path.
    pub fn set_bands(&mut self, centers_hz: &[Real], boosts_db: &[Real]) {
        let count = centers_hz.len().min(boosts_db.len());
        if count != self.num_bands {
            self.set_num_bands(count);
        }
        let live = self.num_bands;
        for (slot, center) in self.center_hz[..live].iter_mut().zip(centers_hz) {
            *slot = center.clamp(MIN_BAND_FREQ_HZ, MAX_BAND_FREQ_HZ);
        }
        // NaN defeats the exact-equality guard in `set_band_boost`, forcing a redesign even when
        // the gain has not changed — the same trick the original plays at `GraphicEqSet.cpp:343`.
        self.installed_boost_db[..live].fill(Real::NAN);
        for (band, boost) in boosts_db.iter().take(live).enumerate() {
            self.set_band_boost(band, *boost);
        }
    }

    /// Tell the equalizer the stream format changed. Redesigns every band and clears the history.
    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        if sample_rate == self.sample_rate || sample_rate <= 0.0 {
            return;
        }
        self.sample_rate = sample_rate;
        self.redesign_all();
        self.reset();
    }

    /// Clear the filter history without touching the design.
    pub fn reset(&mut self) {
        for section in &mut self.sections[..self.num_bands] {
            section.reset();
        }
    }

    fn recompute_q(&mut self) {
        self.q = derive_q(
            f64::from(self.min_band_hz),
            f64::from(self.max_band_hz),
            self.num_bands,
            self.q_multiplier,
        );
    }

    /// Re-run the design for every band, the way `GraphicEqReCalcAllBandCoeffs` does.
    fn redesign_all(&mut self) {
        for band in 0..self.num_bands {
            self.installed_boost_db[band] = Real::NAN;
            let boost = self.requested_boost_db[band];
            self.set_band_boost(band, boost);
        }
    }

    /// Process one interleaved buffer in place.
    ///
    /// Allocation-free and branch-light: bypassed sections are skipped entirely, which is also
    /// what the original does — and why a section that gets switched off keeps stale state until
    /// it is reset.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.enabled || channels == 0 || channels > crate::biquad::MAX_CHANNELS {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            for section in &mut self.sections[..self.num_bands] {
                if !section.coeffs.on {
                    continue;
                }
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = section.tick(channel, *sample);
                }
            }
        }
    }

    /// The cascade's magnitude response in dB — what the GUI draws behind the band handles.
    #[must_use]
    pub fn response_db(&self, freq_hz: Real) -> Real {
        if !self.enabled {
            return 0.0;
        }
        let f = freq_hz / self.sample_rate;
        let mut mag = 1.0;
        for section in &self.sections[..self.num_bands] {
            if section.coeffs.on {
                mag *= magnitude(&section.coeffs, f);
            }
        }
        20.0 * mag.log10()
    }
}

impl Default for GraphicEq {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q_matches_the_reference_table() {
        // docs/spec/09-dsp-eq.md §4.1
        let cases = [
            (5, 62.5, 16000.0, 1.0_f32), // clamped up from 0.6667
            (10, 62.5, 16000.0, 1.597_641_2),
            (15, 25.0, 16000.0, 2.147_578_5),
            (20, 20.0, 16000.0, 2.827_742_6),
            (31, 20.0, 20000.0, 4.333_365_5),
        ];
        for (n, min_hz, max_hz, expected) in cases {
            let q = derive_q(min_hz, max_hz, n, 1.0);
            assert!(
                (q - expected).abs() < 1e-5,
                "{n} bands: got {q}, expected {expected}"
            );
        }
    }

    #[test]
    fn the_five_band_case_is_the_one_that_hits_the_floor() {
        // raw Q is 0.667 there, everything else is already above 1.
        assert_eq!(derive_q(62.5, 16000.0, 5, 1.0), 1.0);
        assert!(derive_q(62.5, 16000.0, 10, 1.0) > 1.0);
    }

    #[test]
    fn a_q_multiplier_of_three_scales_the_31_band_q() {
        let q = derive_q(20.0, 20000.0, 31, 3.0);
        assert!((q - 13.000_096).abs() < 1e-4, "got {q}");
    }

    #[test]
    fn every_table_has_the_band_count_it_claims() {
        for n in [5, 10, 15, 20, 31] {
            let (table, min_hz, max_hz) = band_table(n).expect("table exists");
            assert_eq!(table.len(), n);
            assert_eq!(table[0], min_hz);
            assert_eq!(table[n - 1], max_hz);
            assert!(
                table.windows(2).all(|w| w[0] < w[1]),
                "{n}-band table is not ascending"
            );
        }
        assert!(band_table(7).is_none());
    }

    #[test]
    fn an_untabulated_band_count_falls_back_to_a_geometric_ladder() {
        let mut eq = GraphicEq::new();
        eq.set_num_bands(7);
        assert_eq!(eq.num_bands(), 7);
        let f = eq.center_frequencies();
        assert_eq!(f.len(), 7);
        assert!(f.windows(2).all(|w| w[0] < w[1]));
        // A geometric ladder has a constant ratio between neighbours.
        let first = f[1] / f[0];
        let last = f[6] / f[5];
        assert!((first - last).abs() < 1e-3, "{first} vs {last}");
    }

    #[test]
    fn a_flat_equalizer_is_transparent() {
        let mut eq = GraphicEq::new();
        let original: Vec<Real> = (0..512).map(|n| (n as Real * 0.01).sin()).collect();
        let mut buffer = original.clone();
        eq.process(&mut buffer, 2);
        assert_eq!(buffer, original, "a flat EQ must not touch the signal");
    }

    #[test]
    fn boosting_a_band_raises_its_response_and_leaves_the_others_alone() {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        let centers: Vec<Real> = eq.center_frequencies().to_vec();

        eq.set_band_boost(4, 6.0);
        let boosted = eq.response_db(centers[4]);
        assert!((boosted - 6.0).abs() < 0.5, "measured {boosted} dB");

        // Two bands away the response should be back near flat.
        let far = eq.response_db(centers[0]);
        assert!(far.abs() < 0.5, "band 0 moved to {far} dB");
    }

    #[test]
    fn a_cut_is_symmetric_with_a_boost() {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        let f = eq.center_frequencies()[5];

        eq.set_band_boost(5, 9.0);
        let up = eq.response_db(f);
        eq.set_band_boost(5, -9.0);
        let down = eq.response_db(f);
        assert!((up + down).abs() < 0.3, "{up} dB vs {down} dB");
    }

    #[test]
    fn a_band_at_or_above_nyquist_is_bypassed() {
        let mut eq = GraphicEq::new();
        eq.set_num_bands(31); // has a 20 kHz band
        eq.set_sample_rate(40_000.0); // 20000*2 >= 40000 -> bypassed
        eq.set_band_boost(30, 12.0);
        assert!(eq.response_db(19_000.0).abs() < 0.01);

        // At 48 kHz the same band designs normally.
        eq.set_sample_rate(48_000.0);
        eq.set_band_boost(30, 12.0);
        assert!(eq.response_db(20_000.0) > 5.0);
    }

    #[test]
    fn disabling_the_equalizer_bypasses_it_without_losing_the_curve() {
        let mut eq = GraphicEq::new();
        eq.set_band_boost(3, 10.0);
        eq.set_enabled(false);

        let original: Vec<Real> = (0..256).map(|n| (n as Real * 0.05).sin()).collect();
        let mut buffer = original.clone();
        eq.process(&mut buffer, 2);
        assert_eq!(buffer, original);
        assert_eq!(eq.response_db(eq.center_frequencies()[3]), 0.0);

        eq.set_enabled(true);
        assert!(eq.response_db(eq.center_frequencies()[3]) > 5.0);
        assert_eq!(eq.boosts_db()[3], 10.0);
    }

    #[test]
    fn changing_the_sample_rate_redesigns_every_band() {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(44_100.0);
        eq.set_band_boost(6, 6.0);
        let before = eq.response_db(eq.center_frequencies()[6]);

        eq.set_sample_rate(96_000.0);
        let after = eq.response_db(eq.center_frequencies()[6]);
        // The boost at the band centre must survive the rate change.
        assert!((before - after).abs() < 0.3, "{before} dB vs {after} dB");
    }

    #[test]
    fn applying_a_preset_curve_installs_both_frequencies_and_gains() {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        // A preset-shaped curve: ten bands, a fractional gain, both signs. Written out
        // here rather than lifted from a shipped `.fac`, which is free to be retuned.
        let centers = [
            62.5, 115.0, 250.0, 450.0, 630.0, 1250.0, 2700.0, 5300.0, 7500.0, 13000.0,
        ];
        let boosts = [4.72441, 0.0, 1.0, 2.0, 0.0, -1.0, 0.0, -1.0, -2.0, 0.0];
        eq.set_bands(&centers, &boosts);

        assert_eq!(eq.center_frequencies(), centers);
        assert_eq!(eq.boosts_db(), boosts);
        assert!(eq.response_db(62.5) > 3.0);
        assert!(eq.response_db(7500.0) < -1.0);
    }

    #[test]
    fn processing_stays_finite_under_a_full_boost_curve() {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        for band in 0..eq.num_bands() {
            eq.set_band_boost(band, 12.0);
        }
        let mut buffer: Vec<Real> = (0..4800)
            .flat_map(|n| {
                let s = (n as Real * 0.1).sin() * 0.9;
                [s, -s]
            })
            .collect();
        eq.process(&mut buffer, 2);
        assert!(buffer.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn band_ranges_tile_the_spectrum_without_overlapping() {
        let eq = GraphicEq::new();
        let n = eq.num_bands();
        for band in 0..n {
            let (low, high) = eq.band_range(band);
            let center = eq.center_frequencies()[band];
            assert!(
                low <= center && center <= high,
                "band {band}: {low}..{high} excludes {center}"
            );
            if band + 1 < n {
                let (next_low, _) = eq.band_range(band + 1);
                assert!(next_low > high, "band {band} overlaps its successor");
            }
        }
    }
}
