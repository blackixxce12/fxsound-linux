//! Second-order sections and the parametric peaking design they are built from.
//!
//! A faithful port of `dsp/ptutil/Filt/FiltCalcBiqd.cpp` and the inner loop of
//! `dsp/ptutil/DspUtil/Sos/SosProcess.cpp`. The original's `realtype` and `biqdRealtype` are both
//! `float`, so everything here is `f32` except the handful of places the original routes through
//! `double` (the libm calls and the band-layout maths), which are `f64` for the same reason.
//!
//! The point of matching the original bit-for-bit is that FxSound's presets were authored by ear
//! against *these* coefficients. A textbook RBJ peaking filter would be defensible engineering and
//! would still sound wrong on a preset someone tuned in 2009.

/// The original's `realtype` (`codedefs.h:150`).
pub type Real = f32;

/// The original spells PI as a 24-digit literal (`FiltCalcBiqd.cpp:28`). At `f64` precision that
/// is bit-identical to `std::f64::consts::PI`, so the standard constant is used.
const PI_D: f64 = std::f64::consts::PI;
/// `FiltCalcBiqd.cpp:30` — the original's "smallest positive number" epsilon.
const SPN: f64 = 1.654_36e-24;

/// `u_sos.h:41` — added to every output sample to keep denormals out of the recursion.
pub const SOS_FLOAT_BIAS: Real = 1.0e-30;
/// `sos.h:27`.
pub const SOS_MAX_SECTIONS: usize = 32;
/// Most channels a single section keeps state for.
pub const MAX_CHANNELS: usize = 8;

// Design-time Q limiters, `FiltCalcBiqd.cpp:146-155`.
const Q_UPPER_LIMIT_FREQ: Real = 60.0;
const Q_LOWER_LIMIT_FREQ: Real = 20.0;
const Q_UPPER_LIMIT: Real = 20.0;
const Q_LOWER_LIMIT: Real = 1.0;
const Q_LIMIT_SCALE: Real =
    (Q_UPPER_LIMIT - Q_LOWER_LIMIT) / (Q_UPPER_LIMIT_FREQ - Q_LOWER_LIMIT_FREQ);
const BOOST_WARP_LEVEL: Real = 6.0;
const BOOST_MAX_Q: Real = 20.0;
const BOOST_MIN_Q: Real = 0.2;
const BOOST_SCALE: Real = (BOOST_MAX_Q - BOOST_MIN_Q) / BOOST_WARP_LEVEL;

/// `GraphicEq.h:46` — the DSP layer's own clamp, wider than the GUI's ±12 dB.
pub const MAX_BOOST_OR_CUT_DB: Real = 20.0;

/// Normalised biquad coefficients, `a0` divided out.
///
/// For the parametric design `a1 == b1` always (`FiltCalcBiqd.cpp:217`), which is what lets the
/// hot loop drop one multiply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiquadCoeffs {
    pub b0: Real,
    pub b1: Real,
    pub b2: Real,
    pub a1: Real,
    pub a2: Real,
    /// `false` means "pass through"; the section is skipped entirely rather than run at unity.
    pub on: bool,
}

impl BiquadCoeffs {
    /// A section that passes its input through untouched.
    pub const UNITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
        on: false,
    };
}

impl Default for BiquadCoeffs {
    fn default() -> Self {
        Self::UNITY
    }
}

/// `filtBW2ANGLE` (`FiltCalcBiqd.cpp:55-82`): bandwidth to prototype bandedge.
///
/// One deliberate divergence from the original: `asin` is given a clamped argument. The original
/// passes `d` straight through, and `f32` rounding can push it just past 1.0, which yields a NaN
/// that then poisons every subsequent sample. Clamping changes nothing for in-range inputs.
fn bw_to_angle(a: Real, bandwidth: Real) -> Real {
    let t = (2.0 * PI_D * f64::from(bandwidth)).tan() as Real;
    let a2 = a * a;
    let a4 = a2 * a2;
    let mut d = 2.0 * a2 * t;
    let sn = (1.0 + a4) * t;
    let cs = 1.0 - a4;

    let mag = (f64::from(sn) * f64::from(sn) + f64::from(cs) * f64::from(cs)).sqrt() as Real;
    d /= mag;
    let delta = f64::from(sn).atan2(f64::from(cs)) as Real;
    let asnd = f64::from(d.clamp(-1.0, 1.0)).asin() as Real;

    let mut theta = 0.5 * (PI_D as Real - asnd - delta);
    let alternative = 0.5 * (asnd - delta);
    if alternative > 0.0 && alternative < theta {
        theta = alternative;
    }
    if bandwidth >= 0.5 {
        // The original's own comment calls this a hack (`FiltCalcBiqd.cpp:78-79`).
        theta = 0.005;
    }
    theta / (2.0 * PI_D as Real)
}

/// `filtCalcParametric` (`FiltCalcBiqd.cpp:109-222`).
///
/// `q` is taken by value because the original mutates only its local copy, so the equalizer's
/// nominal Q survives the design-time limiters below.
#[must_use]
pub fn calc_parametric(fs: Real, f0: Real, boost_db: Real, q: Real) -> BiquadCoeffs {
    // An exact zero bypasses the section rather than designing a unity filter.
    if boost_db == 0.0 {
        return BiquadCoeffs::UNITY;
    }

    let mut q = q;

    // Rule A: narrow filters at low frequencies ring, so cap Q as f0 approaches 20 Hz. At exactly
    // 20 Hz this yields Q = 1.0, which is why the golden vectors show 1.0 for the 20 Hz band.
    if f0 < Q_UPPER_LIMIT_FREQ {
        let max_q = (f0 - Q_LOWER_LIMIT_FREQ) * Q_LIMIT_SCALE + Q_LOWER_LIMIT;
        if q > max_q {
            q = max_q;
        }
    }
    // Rule B: a small boost with a high Q is inaudible but costs stability, so warp Q down.
    let abs_boost = boost_db.abs();
    if abs_boost < BOOST_WARP_LEVEL {
        let max_q = abs_boost * BOOST_SCALE + BOOST_MIN_Q;
        if q > max_q {
            q = max_q;
        }
    }

    let w0 = f0 / fs;
    let bandwidth = w0 / q;

    // Bilinear warp: the analogue prototype sits at fs/4.
    let a = (PI_D * (f64::from(w0) - 0.25)).tan() as Real;
    let asq = a * a;

    let big_a = 10_f64.powf(f64::from(boost_db) / 20.0) as Real;
    let f_ref: Real = if boost_db < 6.0 && boost_db > -6.0 {
        f64::from(big_a).sqrt() as Real
    } else if big_a > 1.0 {
        big_a / std::f32::consts::SQRT_2
    } else {
        big_a * std::f32::consts::SQRT_2
    };

    let xfmbw = bw_to_angle(a, bandwidth);
    let c = (1.0 / (2.0 * PI_D * f64::from(xfmbw)).tan()) as Real;

    let f2 = f_ref * f_ref;
    let tmp = big_a * big_a - f2;
    let alphad = if f64::from(tmp).abs() <= SPN {
        c
    } else {
        ((f64::from(c) * f64::from(c)) * (f64::from(f2) - 1.0) / f64::from(tmp)).sqrt() as Real
    };
    let alphan = big_a * alphad;

    let a2plus1 = 1.0 + asq;
    let ma2plus1 = 1.0 - asq;

    let b0_raw = a2plus1 + alphan * ma2plus1;
    let b1_raw = 4.0 * a;
    let b2_raw = a2plus1 - alphan * ma2plus1;
    let a0_raw = a2plus1 + alphad * ma2plus1;
    let a2_raw = a2plus1 - alphad * ma2plus1;

    let recip = 1.0 / a0_raw;
    let b1 = b1_raw * recip;

    BiquadCoeffs {
        b0: b0_raw * recip,
        b1,
        b2: b2_raw * recip,
        a1: b1,
        a2: a2_raw * recip,
        on: true,
    }
}

/// One cascaded second-order section with per-channel state.
///
/// Mirrors `sosSectionType` (`u_sos.h:47-62`) minus two fields the original never reads.
#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub coeffs: BiquadCoeffs,
    s1: [Real; MAX_CHANNELS],
    s2: [Real; MAX_CHANNELS],
}

impl Section {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            coeffs: BiquadCoeffs::UNITY,
            s1: [0.0; MAX_CHANNELS],
            s2: [0.0; MAX_CHANNELS],
        }
    }

    /// The original's hot loop (`SosProcess.cpp:576-578`), which folds `a1 == b1` into one
    /// multiply. Only correct for the parametric design; use [`Section::tick_general`] for a
    /// filter whose `a1` and `b1` differ.
    #[inline(always)]
    pub fn tick(&mut self, channel: usize, x: Real) -> Real {
        let y = self.s1[channel] + self.coeffs.b0 * x + SOS_FLOAT_BIAS;
        self.s1[channel] = (x - y) * self.coeffs.b1 + self.s2[channel];
        self.s2[channel] = self.coeffs.b2 * x - self.coeffs.a2 * y;
        y
    }

    /// Direct Form II transposed, no assumption about `a1`.
    #[inline(always)]
    pub fn tick_general(&mut self, channel: usize, x: Real) -> Real {
        let y = self.s1[channel] + self.coeffs.b0 * x + SOS_FLOAT_BIAS;
        self.s1[channel] = self.coeffs.b1 * x - self.coeffs.a1 * y + self.s2[channel];
        self.s2[channel] = self.coeffs.b2 * x - self.coeffs.a2 * y;
        y
    }

    /// Clear the filter history. Call when the format changes or a preset rewrites the band layout.
    #[inline]
    pub fn reset(&mut self) {
        self.s1 = [0.0; MAX_CHANNELS];
        self.s2 = [0.0; MAX_CHANNELS];
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.coeffs.on
    }
}

impl Default for Section {
    fn default() -> Self {
        Self::new()
    }
}

/// `filtPolyCalc2ndOrderResponse` (`Filtpoly.cpp:30-40`). `f` is normalised, cycles per sample.
#[inline]
fn poly2_magnitude(c0: Real, c1: Real, c2: Real, f: Real) -> Real {
    // `mth.h:19-20` defines its own TWO_PI/FOUR_PI. They round to the same `f32` as the standard
    // constants, so the standard ones are used rather than re-deriving the originals' digits.
    const TWO_PI: Real = std::f32::consts::TAU;
    const FOUR_PI: Real = 2.0 * std::f32::consts::TAU;
    let w = TWO_PI * f;
    let w2 = FOUR_PI * f;
    let re = c0 * w2.cos() + c1 * w.cos() + c2;
    let im = c0 * w2.sin() + c1 * w.sin();
    (re * re + im * im).sqrt()
}

/// Magnitude of one biquad at a normalised frequency
/// (`filtPolyCalcBiquadResponseFiltStruct`, `Filtpoly.cpp:83-92`).
#[must_use]
pub fn magnitude(coeffs: &BiquadCoeffs, f_norm: Real) -> Real {
    poly2_magnitude(coeffs.b0, coeffs.b1, coeffs.b2, f_norm)
        / poly2_magnitude(1.0, coeffs.a1, coeffs.a2, f_norm)
}

/// Magnitude of a whole cascade in dB — what the GUI draws as the response curve.
#[must_use]
pub fn cascade_db(sections: &[Section], f_hz: Real, fs: Real) -> Real {
    let f = f_hz / fs;
    let mut mag = 1.0;
    for section in sections {
        if section.coeffs.on {
            mag *= magnitude(&section.coeffs, f);
        }
    }
    20.0 * mag.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden vectors in `docs/spec/09-dsp-eq.md` §13, produced by compiling the original C
    /// with glibc libm on x86-64.
    ///
    /// The tolerance is relative to the *coefficient scale* (O(1)) rather than to each value, so
    /// it bottoms out at 1e-6 absolute. `b0` and `b2` are differences of two nearly equal terms,
    /// and when that difference lands near zero — 20 kHz at +12 dB gives b2 = -0.1236 out of terms
    /// of magnitude ~1.6 — `f32` cancellation leaves only a handful of significant digits. Judging
    /// such a value against its own magnitude would be measuring rounding noise, not correctness.
    /// 1e-6 on a coefficient of this size is below 1e-5 dB of response error.
    fn assert_close(actual: Real, expected: Real, what: &str) {
        let tolerance = 1e-6 * expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= tolerance,
            "{what}: got {actual:.9}, expected {expected:.9}"
        );
    }

    fn check(fs: Real, f0: Real, boost: Real, q: Real, expect: [Real; 4]) {
        let c = calc_parametric(fs, f0, boost, q);
        assert!(c.on, "{f0} Hz @ {boost} dB should design a live section");
        assert_eq!(c.a1, c.b1, "the parametric design requires a1 == b1");
        assert_close(c.b0, expect[0], &format!("b0 at {f0} Hz {boost} dB"));
        assert_close(c.b1, expect[1], &format!("b1 at {f0} Hz {boost} dB"));
        assert_close(c.b2, expect[2], &format!("b2 at {f0} Hz {boost} dB"));
        assert_close(c.a2, expect[3], &format!("a2 at {f0} Hz {boost} dB"));
    }

    #[test]
    fn golden_vectors_48k_31band() {
        let fs = 48_000.0;
        let q = 4.333_365_4;
        check(fs, 20.0, 3.0, q, [1.000_453_8, -1.997_792_7, 0.997_345_75, 0.997_799_63]);
        check(fs, 20.0, -3.0, q, [0.999_546_35, -1.996_886_4, 0.997_347_0, 0.996_893_35]);
        check(fs, 20.0, 6.0, q, [1.000_918_2, -1.998_148_0, 0.997_236_73, 0.998_154_88]);
        check(fs, 20.0, 12.0, q, [1.003_643_3, -1.997_548_9, 0.993_912_52, 0.997_555_8]);
        check(fs, 20.0, -12.0, q, [0.996_369_96, -1.990_297_7, 0.993_934_63, 0.990_304_6]);
        check(fs, 100.0, 3.0, q, [1.000_523_6, -1.997_290_5, 0.996_937_99, 0.997_461_6]);
        check(fs, 100.0, -12.0, q, [0.995_814_7, -1.988_650_9, 0.993_006_7, 0.988_821_4]);
        check(fs, 1000.0, 3.0, q, [1.005_177_3, -1.958_005_3, 0.969_723_6, 0.974_900_7]);
        check(fs, 1000.0, 12.0, q, [1.041_505_7, -1.955_281_7, 0.930_648_1, 0.972_153_7]);
        check(fs, 1000.0, -12.0, q, [0.960_148_4, -1.877_360_7, 0.933_411_9, 0.893_560_3]);
        check(fs, 10_000.0, 3.0, q, [1.046_831_3, -0.458_875_9, 0.726_129_0, 0.772_960_2]);
        check(fs, 10_000.0, 12.0, q, [1.371_291_6, -0.453_166_46, 0.379_608_9, 0.750_900_5]);
        check(fs, 10_000.0, -12.0, q, [0.729_239_4, -0.330_466_84, 0.547_586_26, 0.276_825_76]);
        check(fs, 20_000.0, 3.0, q, [1.085_694_2, 1.372_261_4, 0.498_856_9, 0.584_551_04]);
        check(fs, 20_000.0, 12.0, q, [1.672_465_0, 1.341_337_7, -0.123_621_62, 0.548_843_44]);
        check(fs, 20_000.0, -12.0, q, [0.597_919_9, 0.802_012_44, 0.328_164_37, -0.073_915_82]);
    }

    #[test]
    fn golden_vectors_44k1_10band() {
        let fs = 44_100.0;
        let q = 1.597_641_2;
        let boost = 6.0;
        check(fs, 62.5, boost, q, [1.001_952_8, -1.995_996_7, 0.994_123_16, 0.996_075_93]);
        check(fs, 115.734, boost, q, [1.003_609_9, -1.992_474_6, 0.989_135_56, 0.992_745_6]);
        check(fs, 214.311, boost, q, [1.006_664_4, -1.985_681_8, 0.979_943_45, 0.986_607_8]);
        check(fs, 396.85, boost, q, [1.012_271_6, -1.972_183_2, 0.963_068_2, 0.975_339_8]);
        check(fs, 734.867, boost, q, [1.022_493_5, -1.944_094_2, 0.932_305_5, 0.954_799_0]);
        check(fs, 1360.79, boost, q, [1.040_899_8, -1.881_879_5, 0.876_911_6, 0.917_811_3]);
        check(fs, 2519.84, boost, q, [1.073_377_7, -1.734_432_8, 0.779_168_0, 0.852_545_8]);
        check(fs, 4666.12, boost, q, [1.129_007_3, -1.370_035_4, 0.611_749_5, 0.740_756_9]);
        check(fs, 8640.48, boost, q, [1.221_388_7, -0.518_224_48, 0.333_726_47, 0.555_115_1]);
        check(fs, 16_000.0, boost, q, [1.377_299_2, 0.808_339_18, -0.135_489_66, 0.241_809_56]);
    }

    #[test]
    fn the_low_frequency_q_limiter_fires_at_20hz() {
        // Rule A caps Q to exactly 1.0 at 20 Hz regardless of what was asked for, so the design
        // must be identical for any nominal Q at that frequency.
        let wide = calc_parametric(48_000.0, 20.0, 6.0, 1.0);
        let narrow = calc_parametric(48_000.0, 20.0, 6.0, 4.333_365_4);
        assert_eq!(wide, narrow);
    }

    #[test]
    fn zero_boost_bypasses_the_section() {
        let c = calc_parametric(48_000.0, 1000.0, 0.0, 2.0);
        assert_eq!(c, BiquadCoeffs::UNITY);
        assert!(!c.on);
    }

    #[test]
    fn a_boosted_band_lifts_its_own_frequency_by_the_requested_amount() {
        // The design is a peaking filter, so its gain at f0 should be the requested boost.
        let fs = 48_000.0;
        for (f0, boost) in [(100.0, 6.0), (1000.0, 12.0), (1000.0, -12.0), (8000.0, 3.0)] {
            let c = calc_parametric(fs, f0, boost, 1.597_641_2);
            let db = 20.0 * magnitude(&c, f0 / fs).log10();
            assert!(
                (db - boost).abs() < 0.25,
                "{f0} Hz asked for {boost} dB, measured {db:.3} dB"
            );
        }
    }

    #[test]
    fn a_bypassed_section_is_flat() {
        let s = Section::new();
        assert_eq!(cascade_db(&[s], 1000.0, 48_000.0), 0.0);
    }

    #[test]
    fn the_inner_loop_is_stable_and_settles() {
        // Run a boosted band over a long impulse and make sure it decays rather than blowing up.
        let mut s = Section::new();
        s.coeffs = calc_parametric(48_000.0, 1000.0, 12.0, 1.6);
        let mut peak_late = 0.0_f32;
        for n in 0..48_000 {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let y = s.tick(0, x);
            assert!(y.is_finite(), "sample {n} was not finite");
            if n > 24_000 {
                peak_late = peak_late.max(y.abs());
            }
        }
        assert!(peak_late < 1e-3, "impulse response had not decayed: {peak_late}");
    }

    #[test]
    fn channels_keep_independent_state() {
        let mut s = Section::new();
        s.coeffs = calc_parametric(48_000.0, 1000.0, 6.0, 1.6);
        let left = s.tick(0, 1.0);
        let right = s.tick(1, 1.0);
        assert_eq!(left, right, "a fresh channel must behave like a fresh filter");
        let left_second = s.tick(0, 0.0);
        let right_second = s.tick(1, 0.0);
        assert_eq!(left_second, right_second);
    }
}
