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

/// Carry a curve over to a new band count by relative position, so the user's shape survives the
/// change instead of being wiped flat.
///
/// A line-for-line port of the remap in `GraphicEqSetNumBands` (`GraphicEqSet.cpp:200-245`,
/// upstream 182a329): the first band lands on the first band and the last on the last, and in
/// between
///
/// - **one old band** is copied to every new band;
/// - **fewer bands** each take the nearest old band, `1 + (int)((i-1)*(old-1)/(new-1) + 0.5)`
///   evaluated in `double` and truncated, as the C does — a selection, never an average, so a
///   narrow boost keeps its height rather than being smeared into its neighbours;
/// - **more bands** interpolate linearly between the two old bands either side, the index computed
///   in `double` and stored as `float` (`realtype`), the fraction and the blend in `float`, so the
///   results match the original's bits and not merely its values. The last new band falls exactly
///   on the last old one, where there is no upper neighbour; the original's `upper_index` guard
///   copies it, and so does this.
///
/// Remapping is by position, not frequency — which is why it survives a ladder change such as
/// ten bands at 62.5 Hz–16 kHz becoming thirty-one at 20 Hz–20 kHz. It is also not reversible: a
/// shrink keeps only the bands it selects, so 10 → 31 → 10 comes back close to, not equal to, the
/// curve it started from, exactly as it does in the original.
///
/// Where the original has no answer the port picks one and says so:
///
/// - **An equal count** copies. The original returns before remapping (`GraphicEqSet.cpp:131-133`);
///   running the interpolation instead would give the same numbers for every finite gain, and a
///   NaN for an infinite one.
/// - **No old bands** gives a flat curve, which is what the original's freshly initialised sections
///   hold when its `old_num_bands >= 1` guard skips the remap.
/// - **One new band from several** takes the first. The C divides `0.0` by `new - 1 = 0` there and
///   truncates the resulting NaN to an `int`, which is undefined; the first band is the limit the
///   formula tends to, since `i - 1` is zero.
///
/// Every index is additionally clamped to the old curve. The clamps never move an index the
/// formula produces (both formulas stay inside `1..=old` by construction), but the spec's warning
/// that the original is "safe by luck, not construction" (`docs/spec/09-dsp-eq.md` §14) is the
/// reason they are there: a band count reaches this from the command line and D-Bus as well as
/// from the window, and no count may be able to make it panic.
#[must_use]
pub fn remap_band_gains(old: &[Real], new_count: usize) -> Vec<Real> {
    let old_num_bands = old.len();
    if old_num_bands == 0 {
        return vec![0.0; new_count];
    }
    if old_num_bands == new_count {
        return old.to_vec();
    }

    let num_bands = new_count;
    let last = old_num_bands - 1;
    (1..=num_bands)
        .map(|i| {
            if old_num_bands == 1 {
                old[0]
            } else if num_bands == 1 {
                // Undefined in the original (see above); the first band.
                old[0]
            } else if num_bands < old_num_bands {
                // Fewer bands: pick the nearest old band (equidistant selection).
                let source_index = 1
                    + ((i as f64 - 1.0) * (old_num_bands as f64 - 1.0) / (num_bands as f64 - 1.0)
                        + 0.5) as usize;
                old[(source_index - 1).min(last)]
            } else {
                // More bands: linear interpolation between old bands.
                let source_index = (1.0
                    + f64::from((i - 1) as Real) * (old_num_bands as f64 - 1.0)
                        / (num_bands as f64 - 1.0)) as Real;
                let lower_index = (source_index as usize).clamp(1, old_num_bands);
                let upper_index = lower_index + 1;
                let fraction = source_index - lower_index as Real;

                if upper_index <= old_num_bands {
                    old[lower_index - 1] + (old[upper_index - 1] - old[lower_index - 1]) * fraction
                } else {
                    old[lower_index - 1]
                }
            }
        })
        .collect()
}

/// The gains a preset's curve takes on the user's live band ladder.
///
/// The port of `DfxDspPrivate::getGraphicEqInfoFromVals` (`DfxDspEq.cpp:127-247`, upstream
/// 38e3343, f3d9f23, 12003f0), which is why a user on thirty-one bands who picks a ten-band factory
/// preset stays on thirty-one bands:
///
/// - **Different band counts:** the live ladder is kept and only the gains move, remapped by
///   position. The upstream comment explains why the frequencies are not interpolated too:
///   carrying centres across counts "produced incorrect/overlapping ranges". The remap is the
///   one [`remap_band_gains`] ports; `DfxDspEq.cpp` carries its own copy of it, identical for every
///   count except the two edge cases where it is undefined — a one-band live ladder, and an index
///   landing outside the curve, where it leaves the output uninitialised — and the spec asks for
///   one routine with the `GraphicEqSet` behaviour (`docs/spec/09-dsp-eq.md`, "Open questions").
/// - **Equal band counts:** the gains are copied as they are. The original also copies the
///   preset's centre frequencies onto the live ladder then (`DfxDspEq.cpp:229-241`); that half is
///   the caller's, since this returns gains only — pair the result with `preset_centres` when the
///   counts match and with `live_centres` when they do not.
/// - **A preset with no equalizer:** a flat curve at the live count. The original also switches
///   the equalizer on for such a preset (`DfxDspEq.cpp:144-158`); that too is the caller's.
///
/// A preset band is a centre with a gain, so the preset's band count is the shorter of the two
/// slices, the same rule [`GraphicEq::set_bands`] applies to a curve that reaches it. Nothing else
/// about the preset's centres enters the result: the fit is by position, not by frequency.
#[must_use]
pub fn fit_preset_gains(
    preset_centres: &[Real],
    preset_gains: &[Real],
    live_centres: &[Real],
) -> Vec<Real> {
    let preset_bands = preset_centres.len().min(preset_gains.len());
    remap_band_gains(&preset_gains[..preset_bands], live_centres.len())
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
    ///
    /// Flat on purpose, although the original's `GraphicEqSetNumBands` has remapped the old curve
    /// since 182a329: the one caller on the audio thread is [`GraphicEq::set_bands`], which
    /// installs the caller's gains straight afterwards, and the curve a user sees survive a band
    /// count change is remapped where the settings live, with [`remap_band_gains`].
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

    // --- Band-count remapping (U1) and preset fitting (U2) ------------------------------------
    //
    // The `*_BITS` expectations below are the original's own output: the loops of
    // `GraphicEqSet.cpp:200-245` and `DfxDspEq.cpp:182-227`, compiled verbatim with gcc on x86-64
    // (`realtype` is `float`, `codedefs.h:150`) and printed with `%.9g`, which round-trips an
    // `f32`. The two upstream copies agree on every case here. The hand-worked entries next to
    // them are the arithmetic a reader can check on paper; the bit patterns are what proves the
    // port evaluates it in the same precisions, in the same order.

    /// A ten-band curve with both signs, a zero, the full ±12 dB and fractional gains.
    const TEN: [Real; 10] = [6.0, 4.5, -3.0, 0.0, 2.25, -12.0, 12.0, 1.5, -0.75, 3.0];
    const FIVE: [Real; 5] = [-6.0, 3.0, 0.0, 9.0, -1.5];

    /// Thirty-one bands whose gain is their own 1-based index, so a selection reads as the list of
    /// bands it selected.
    fn thirty_one_numbered() -> Vec<Real> {
        (1..=31).map(|band| band as Real).collect()
    }

    fn assert_close(got: Real, want: Real, what: &str) {
        assert!(
            (got - want).abs() < 1e-5,
            "{what}: got {got}, expected {want}"
        );
    }

    #[test]
    fn ten_bands_grow_to_thirty_one_by_interpolating_the_way_the_original_does() {
        let remapped = remap_band_gains(&TEN, 31);
        assert_eq!(remapped.len(), 31);

        // Band i reads old position 1 + (i-1)*9/30 = 1 + 0.3*(i-1).
        assert_close(remapped[0], 6.0, "band 1 sits on old band 1");
        assert_close(remapped[1], 5.55, "band 2: 6 + (4.5-6)*0.3");
        assert_close(remapped[4], 3.0, "band 5: 4.5 + (-3-4.5)*0.2");
        assert_close(remapped[10], 0.0, "band 11 sits on old band 4");
        assert_close(remapped[15], -4.875, "band 16: 2.25 + (-12-2.25)*0.5");
        assert_close(remapped[20], 12.0, "band 21 sits on old band 7");
        assert_close(
            remapped[30],
            3.0,
            "band 31 is the upper-index guard's copy of old band 10",
        );

        const TEN_TO_THIRTY_ONE_BITS: [Real; 31] = [
            6.0,
            5.55,
            5.1,
            4.65,
            2.9999995,
            0.75,
            -1.4999995,
            -2.7000003,
            -1.7999997,
            -0.89999986,
            0.0,
            0.6750004,
            1.3499998,
            2.025,
            -0.5999973,
            -4.875,
            -9.1500025,
            -9.600002,
            -2.3999977,
            4.7999954,
            12.0,
            8.849998,
            5.700001,
            2.5499992,
            1.0500004,
            0.375,
            -0.30000043,
            -0.37499857,
            0.74999857,
            1.8749993,
            3.0,
        ];
        assert_eq!(remapped, TEN_TO_THIRTY_ONE_BITS);
    }

    #[test]
    fn thirty_one_bands_shrink_to_ten_by_picking_the_nearest_band() {
        // 1 + (int)((i-1)*30/9 + 0.5): 0.5, 3.83, 7.17, 10.5, 13.83, 17.17, 20.5, 23.83, 27.17,
        // 30.5, truncated — note 10.5 and 20.5 truncate down, they do not round half up.
        assert_eq!(
            remap_band_gains(&thirty_one_numbered(), 10),
            [1.0, 4.0, 8.0, 11.0, 14.0, 18.0, 21.0, 24.0, 28.0, 31.0]
        );
    }

    #[test]
    fn thirty_one_bands_shrink_to_five_by_picking_the_nearest_band() {
        // 1 + (int)((i-1)*30/4 + 0.5): 0.5, 8.0, 15.5, 23.0, 30.5.
        assert_eq!(
            remap_band_gains(&thirty_one_numbered(), 5),
            [1.0, 9.0, 16.0, 24.0, 31.0]
        );
    }

    #[test]
    fn a_shrink_selects_a_band_rather_than_averaging_its_neighbours() {
        // Ten bands to five: bands 1, 3, 6, 8 and 10 (0.5, 2.75, 5.0, 7.25, 9.5 truncated). Old
        // band 6's full -12 dB cut and old band 7's +12 dB boost sit side by side, and the one
        // selected keeps its whole height instead of the two cancelling out.
        assert_eq!(remap_band_gains(&TEN, 5), [6.0, -3.0, -12.0, 1.5, 3.0]);
    }

    #[test]
    fn five_bands_grow_to_twenty_by_interpolating_the_way_the_original_does() {
        let remapped = remap_band_gains(&FIVE, 20);

        // Band i reads old position 1 + (i-1)*4/19.
        assert_close(remapped[0], -6.0, "band 1 sits on old band 1");
        assert_close(
            remapped[1],
            -6.0 + 9.0 * 4.0 / 19.0,
            "band 2: 4/19 of the way to 3 dB",
        );
        assert_close(
            remapped[5],
            3.0 + (0.0 - 3.0) * 1.0 / 19.0,
            "band 6: old position 2+1/19",
        );
        assert_close(
            remapped[19],
            -1.5,
            "band 20 is the guard's copy of old band 5",
        );

        const FIVE_TO_TWENTY_BITS: [Real; 20] = [
            -6.0, -4.1052628, -2.210527, -0.3157897, 1.5789475, 2.8421052, 2.2105265, 1.5789471,
            0.9473684, 0.3157897, 0.9473691, 2.8421052, 4.736841, 6.6315794, 8.526316, 7.342107,
            5.1315784, 2.921051, 0.7105274, -1.5,
        ];
        assert_eq!(remapped, FIVE_TO_TWENTY_BITS);
    }

    #[test]
    fn a_single_band_is_copied_to_every_new_band() {
        assert_eq!(remap_band_gains(&[4.5], 10), [4.5; 10]);
        assert_eq!(remap_band_gains(&[-7.25], 31), [-7.25; 31]);
    }

    #[test]
    fn the_band_counts_the_window_offers_remap_bit_for_bit_with_the_original() {
        // 10 -> 15 and 10 -> 20 exercise fractions that 10 -> 31 does not (9/14 and 9/19).
        const TEN_TO_FIFTEEN_BITS: [Real; 15] = [
            6.0,
            5.035714,
            2.357142,
            -2.4642859,
            -1.2857144,
            0.48214316,
            1.9285716,
            -4.875,
            -8.57143,
            6.8571396,
            7.500002,
            1.3392863,
            -0.10714316,
            0.5892842,
            3.0,
        ];
        const TEN_TO_TWENTY_BITS: [Real; 20] = [
            6.0,
            5.2894735,
            4.5789475,
            1.3421049,
            -2.2105255,
            -1.8947368,
            -0.47368455,
            0.7105268,
            1.7763155,
            -1.4999993,
            -8.250001,
            -6.947365,
            4.421047,
            10.342107,
            5.3684216,
            1.2631588,
            0.1973691,
            -0.55263233,
            1.2236838,
            3.0,
        ];
        assert_eq!(remap_band_gains(&TEN, 15), TEN_TO_FIFTEEN_BITS);
        assert_eq!(remap_band_gains(&TEN, 20), TEN_TO_TWENTY_BITS);
    }

    #[test]
    fn a_round_trip_through_thirty_one_bands_comes_back_close_but_not_identical() {
        // The shrink back selects bands 1, 4, 8, 11, ... of the thirty-one, which sit at old
        // positions 1.0, 1.9, 3.1, 4.0, ... — the ends and every third band come home exactly,
        // the rest come home interpolated. The original does the same.
        let there = remap_band_gains(&TEN, 31);
        let back = remap_band_gains(&there, 10);
        const ROUND_TRIP_BITS: [Real; 10] = [
            6.0,
            4.65,
            -2.7000003,
            0.0,
            2.025,
            -9.600002,
            12.0,
            2.5499992,
            -0.37499857,
            3.0,
        ];
        assert_eq!(back, ROUND_TRIP_BITS);
        assert_eq!(back[0], TEN[0]);
        assert_eq!(back[3], TEN[3]);
        assert_eq!(back[6], TEN[6]);
        assert_eq!(back[9], TEN[9]);
        assert_ne!(back, TEN);
    }

    #[test]
    fn an_equal_band_count_is_copied_bit_for_bit() {
        // Including the values an interpolation at fraction zero would not preserve: an infinity
        // turns into NaN through `(inf - inf) * 0`, and a negative zero into a positive one.
        let odd = [f32::INFINITY, -0.0, 1e-30, -12.0, 3.0];
        let copied = remap_band_gains(&odd, 5);
        for (got, want) in copied.iter().zip(odd) {
            assert_eq!(got.to_bits(), want.to_bits());
        }
        assert_eq!(remap_band_gains(&TEN, 10), TEN);
    }

    #[test]
    fn an_empty_curve_remaps_to_a_flat_one() {
        assert_eq!(remap_band_gains(&[], 10), [0.0; 10]);
        assert!(remap_band_gains(&[], 0).is_empty());
    }

    #[test]
    fn a_band_count_of_zero_gives_an_empty_curve() {
        assert!(remap_band_gains(&TEN, 0).is_empty());
    }

    #[test]
    fn shrinking_to_a_single_band_keeps_the_first() {
        // Undefined in the original, which truncates 0.0 / 0.0 to an int.
        assert_eq!(remap_band_gains(&TEN, 1), [6.0]);
        assert_eq!(remap_band_gains(&thirty_one_numbered(), 1), [1.0]);
    }

    #[test]
    fn every_pair_of_band_counts_keeps_the_ends_and_invents_no_gain() {
        // Every count the engine can hold, both ways, as a proof that no pair of counts reaches
        // an index outside the old curve: an out-of-range index would panic here.
        for old_count in 1..=SOS_MAX_SECTIONS {
            let old: Vec<Real> = (0..old_count)
                .map(|band| ((band * 7 % 11) as Real - 5.0) * 2.0)
                .collect();
            let (low, high) = old
                .iter()
                .fold((Real::INFINITY, Real::NEG_INFINITY), |(lo, hi), g| {
                    (lo.min(*g), hi.max(*g))
                });
            for new_count in 0..=SOS_MAX_SECTIONS {
                let remapped = remap_band_gains(&old, new_count);
                assert_eq!(remapped.len(), new_count, "{old_count} -> {new_count}");
                if new_count == 0 {
                    continue;
                }
                assert_eq!(
                    remapped[0], old[0],
                    "{old_count} -> {new_count}: first band"
                );
                if new_count > 1 {
                    assert_eq!(
                        remapped[new_count - 1],
                        old[old_count - 1],
                        "{old_count} -> {new_count}: last band"
                    );
                }
                for (band, gain) in remapped.iter().enumerate() {
                    assert!(
                        (low..=high).contains(gain),
                        "{old_count} -> {new_count}: band {band} is {gain}, outside {low}..={high}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_flat_curve_stays_flat_and_a_constant_one_stays_constant_at_any_count() {
        for old_count in 1..=SOS_MAX_SECTIONS {
            for new_count in 1..=SOS_MAX_SECTIONS {
                assert!(
                    remap_band_gains(&vec![0.0; old_count], new_count)
                        .iter()
                        .all(|g| *g == 0.0),
                    "{old_count} -> {new_count}"
                );
                assert!(
                    remap_band_gains(&vec![3.5; old_count], new_count)
                        .iter()
                        .all(|g| *g == 3.5),
                    "{old_count} -> {new_count}"
                );
            }
        }
    }

    #[test]
    fn a_ten_band_preset_lands_on_a_thirty_one_band_ladder_by_position() {
        let (live, _, _) = band_table(31).expect("the 31-band table");
        let (centres, _, _) = band_table(10).expect("the 10-band table");
        let fitted = fit_preset_gains(centres, &TEN, live);
        assert_eq!(fitted.len(), 31);
        // `DfxDspEq.cpp` gives the same bits as `GraphicEqSet.cpp` here.
        assert_eq!(fitted, remap_band_gains(&TEN, 31));
        assert_eq!(fitted[20], 12.0);
    }

    #[test]
    fn a_thirty_one_band_preset_on_a_ten_band_ladder_selects_the_nearest_bands() {
        let (live, _, _) = band_table(10).expect("the 10-band table");
        let (centres, _, _) = band_table(31).expect("the 31-band table");
        assert_eq!(
            fit_preset_gains(centres, &thirty_one_numbered(), live),
            [1.0, 4.0, 8.0, 11.0, 14.0, 18.0, 21.0, 24.0, 28.0, 31.0]
        );
    }

    #[test]
    fn a_preset_with_the_live_band_count_keeps_its_gains_whatever_its_ladder() {
        // The shipped presets carry their own ten centres (115 Hz where the table says 115.734);
        // with equal counts the gains go across untouched and the centres are the caller's to copy.
        let preset_centres = [
            62.5, 115.0, 250.0, 450.0, 630.0, 1250.0, 2700.0, 5300.0, 7500.0, 13000.0,
        ];
        let (live, _, _) = band_table(10).expect("the 10-band table");
        assert_eq!(fit_preset_gains(&preset_centres, &TEN, live), TEN);
    }

    #[test]
    fn a_preset_without_an_equalizer_fits_as_a_flat_curve() {
        let (live, _, _) = band_table(20).expect("the 20-band table");
        assert_eq!(fit_preset_gains(&[], &[], live), [0.0; 20]);
    }

    #[test]
    fn a_preset_is_fitted_by_position_so_its_centres_do_not_move_a_gain() {
        // Upstream deliberately stopped carrying frequencies across band counts; the same five
        // gains on two very different ladders must land identically.
        let (live, _, _) = band_table(15).expect("the 15-band table");
        let low_ladder = [30.0, 60.0, 120.0, 240.0, 480.0];
        let wide_ladder = [62.5, 250.0, 1000.0, 4000.0, 16000.0];
        assert_eq!(
            fit_preset_gains(&low_ladder, &FIVE, live),
            fit_preset_gains(&wide_ladder, &FIVE, live)
        );
        assert_eq!(
            fit_preset_gains(&wide_ladder, &FIVE, live),
            remap_band_gains(&FIVE, 15)
        );
    }

    #[test]
    fn a_preset_band_needs_both_a_centre_and_a_gain() {
        let (live, _, _) = band_table(10).expect("the 10-band table");
        let (ten_centres, _, _) = band_table(10).expect("the 10-band table");

        // An eleventh gain with no centre is not a band: the preset still has ten, and copies.
        let mut eleven_gains = TEN.to_vec();
        eleven_gains.push(9.0);
        assert_eq!(fit_preset_gains(ten_centres, &eleven_gains, live), TEN);

        // Nine centres make a nine-band preset, remapped onto the ten live bands.
        assert_eq!(
            fit_preset_gains(&ten_centres[..9], &TEN, live),
            remap_band_gains(&TEN[..9], 10)
        );
    }

    #[test]
    fn a_one_band_preset_fills_the_whole_live_ladder() {
        let (live, _, _) = band_table(31).expect("the 31-band table");
        assert_eq!(fit_preset_gains(&[1000.0], &[-4.0], live), [-4.0; 31]);
    }

    #[test]
    fn a_preset_fitted_to_a_single_live_band_takes_its_first_band() {
        assert_eq!(
            fit_preset_gains(&[62.5, 1000.0], &[2.0, 8.0], &[1000.0]),
            [2.0]
        );
        assert!(fit_preset_gains(&[62.5], &[2.0], &[]).is_empty());
    }
}
