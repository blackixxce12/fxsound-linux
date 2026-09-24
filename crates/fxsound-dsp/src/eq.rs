//! The graphic equalizer: band layout, Q derivation and the cascade that runs on the audio thread.
//!
//! Ports `dsp/ptutil/DspUtil/GraphicEq/GraphicEqSet.cpp` and the parts of `dsp/DfxDspEq.cpp` that
//! decide which bands exist and what Q they get.
//!
//! The whole structure is allocated once with room for [`crate::biquad::SOS_MAX_SECTIONS`] bands,
//! so changing the band count, the sample rate or a preset never allocates on the audio thread.

use crate::biquad::{
    BiquadCoeffs, MAX_BOOST_OR_CUT_DB, Real, SOS_MAX_SECTIONS, calc_parametric, magnitude,
};
use crate::smooth::{FadingSection, Ramp, glide_frames};

/// `GraphicEqInit.cpp:49` — the Q multiplier before the user touches the filter-width knob.
pub const DEFAULT_Q_MULTIPLIER: Real = 1.0;
/// The filter-width slider's range (`FxAudioControls.cpp:348`).
pub const MIN_Q_MULTIPLIER: Real = 1.0;
pub const MAX_Q_MULTIPLIER: Real = 3.0;
/// `GraphicEqSet.cpp:555-559` — a band centre is clamped to this before anything else.
pub const MIN_BAND_FREQ_HZ: Real = 10.0;
pub const MAX_BAND_FREQ_HZ: Real = 21_000.0;

/// The original's twenty-band ladder (`GraphicEqSet.cpp:468-478`), which [`band_table`] no longer
/// hands out (audit report R4).
///
/// Kept because curves on it outlive the change: settings saved by an earlier version and
/// Windows twenty-band presets carry these centres, and a curve that brings its own centres keeps
/// them, ripple and all — 3.9 dB of it with every band at +6 dB. Whoever owns such a curve can
/// recognise it by this ladder and move it to `band_table(20)`: band for band, no centre moves by
/// more than 0.17 of an octave.
pub const WINDOWS_TWENTY_BAND_CENTRES_HZ: [Real; 20] = [
    20.0, 31.5, 40.0, 63.0, 80.0, 125.0, 160.0, 250.0, 315.0, 500.0, 630.0, 1000.0, 1250.0, 2000.0,
    2500.0, 4000.0, 5000.0, 8000.0, 10000.0, 16000.0,
];

/// The hard-coded ladders (`GraphicEqSet.cpp:430-492`), with the band edges each one implies.
///
/// Returns `(frequencies, min_band_freq, max_band_freq)`. Any other count falls back to a
/// geometric ladder, as the original does.
///
/// **The twenty-band ladder is not the original's (audit report R4).** Every count shares one Q,
/// derived as if its bands were spread geometrically from the first to the last
/// ([`derive_q`]), and four of the five tables are: ten bands exactly, fifteen and thirty-one to
/// within ISO rounding. The original's twenty are not. They are the octave bands from 31.5 Hz
/// with the third-octave band above each tacked on — 31.5 and 40, 63 and 80, … 8000 and 10000 —
/// so they come in pairs a third of an octave apart with two-thirds of an octave between pairs,
/// and the one Q made for half-octave spacing overlaps each pair and leaves a hole between them:
/// every band at +6 dB came out 10.5 dB on the pairs and 6.6 dB between them, 3.9 dB of ripple
/// from 100 Hz to 10 kHz, where ten bands give 2.3 and thirty-one 2.5. These are the half-octave
/// ladder that Q was derived for, written to six figures as the ten-band table is, with the same
/// ends — so the Q, the band edges the window draws and every other count are untouched — and the
/// ripple is 1.9 dB.
///
/// A Q per band from its neighbours' spacing, which keeps the old centres, was tried first and
/// cannot work: every inner band of the old ladder has one neighbour a third of an octave away
/// and one two-thirds away, so every such rule gives every band the same Q, and a uniform Q only
/// trades ripple for level — 3.5 dB of it for a curve that peaks 2.5 dB higher. Correcting the
/// gains instead, which keeps the centres too, fails for the same reason: every band borders one
/// pair and one gap, so whatever lowers a pair lowers a gap. The best least-squares correction of
/// the twenty gains towards a flat +6 dB found sets them between 3.3 and 6.0 dB and ripples 2.6 dB
/// (4.3 to 7.0 dB), no better than every band at +4 dB; the new ladder at +4 dB ripples 1.2 dB
/// (5.2 to 6.4 dB).
///
/// A curve that already carries its own twenty centres, from a preset or from settings, keeps
/// them: this only decides the ladder a count gets when nothing supplies one. So a twenty-band
/// curve saved before 0.4.0, or read from a Windows twenty-band `.fac`, still sits on the old
/// pairs, with their ripple, until whoever owns it moves it here;
/// [`WINDOWS_TWENTY_BAND_CENTRES_HZ`] is how to recognise one.
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
        20.0, 28.4331, 40.4221, 57.4662, 81.6971, 116.145, 165.118, 234.741, 333.721, 474.436,
        674.485, 958.885, 1363.2, 1938.0, 2755.17, 3916.91, 5568.49, 7916.47, 11254.5, 16000.0,
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

/// The ladder a band count gets when nothing else supplies one: the table for the counts that have
/// one, and otherwise the geometric ladder a fresh equalizer spreads between the ten-band edges —
/// the ladder [`GraphicEq::set_num_bands`] builds from a new equalizer, and the one the window
/// takes for the count.
#[must_use]
pub fn standard_centres(count: usize) -> Vec<Real> {
    if let Some((table, _, _)) = band_table(count) {
        return table.to_vec();
    }
    let (min_hz, max_hz) = band_table(10).map_or((62.5, 16_000.0), |(_, lo, hi)| (lo, hi));
    let mut centres = vec![0.0; count];
    if count > 0 {
        geometric_ladder(count, f64::from(min_hz), f64::from(max_hz), &mut centres);
    }
    centres
}

/// Carry a curve over to a new band count by frequency, so the user's shape stays where it was
/// instead of being wiped flat or slid along the ladder.
///
/// **For a curve on the standard ladder of its count only.** Both ladders are the ones the counts
/// get by default ([`standard_centres`]), and the reading is [`fit_preset_gains`]'s, between
/// them: the gains alone do not say where the curve sits, so this assumes. A curve on centres of
/// its own — a Windows twenty-band preset or a twenty-band curve saved before 0.4.0, still on
/// [`WINDOWS_TWENTY_BAND_CENTRES_HZ`], or any curve with a band dragged in the window — is read at
/// the wrong frequencies here, and a band-count change moves its boosts: a 10 kHz boost on the
/// Windows ladder, read as if it sat at 11.3 kHz, came out on thirty-one bands as 3.98 dB at
/// 10 kHz and 4.21 dB at 12.5 kHz. Carry such a curve with [`remap_curve`], which takes its
/// centres.
///
/// **Changed on purpose (audit report #13).** The original remaps by *position*
/// (`GraphicEqSet.cpp:200-245`, upstream 182a329, which this function used to port line for
/// line): band *i* of the new ladder takes band `i·(old−1)/(new−1)` of the old one, whatever
/// either is tuned to. Across ladders with different edges that moves the curve in frequency — a
/// ten-band preset's +6 dB at 62.5 Hz landed on thirty-one bands as +6 dB at 20 Hz and nothing at
/// 63 Hz, a bass boost slid an octave and a half down to the edge of hearing — and the shrink back
/// kept roughly every third band, so ten bands through thirty-one and back came home up to 2.4 dB
/// away from where they started. Reading the curve by frequency keeps the boost at 63 Hz, and a
/// curve that went up to more bands comes back down exactly whenever the larger ladder is at least
/// as fine as the smaller one all along it — which every pair of the window's counts, 5, 10, 15,
/// 20 and 31, is. A pair that is not comes back read, inside the curve's own range but not
/// restored: fifteen bands have only thirteen in the range fourteen bands cover, so 14 → 15 → 14
/// loses the detail the thirteen cannot hold.
///
/// Where there is nothing to read the answer is still defined:
///
/// - **An equal count** copies, bit for bit — the original returns before remapping
///   (`GraphicEqSet.cpp:131-133`) and so does this.
/// - **No old bands** gives a flat curve, which is what the original's freshly initialised
///   sections hold when its `old_num_bands >= 1` guard skips the remap.
/// - **One old band** is a flat curve at its gain, and every new band takes it.
///
/// A band count reaches this from the command line and D-Bus as well as from the window, so no
/// count and no gain may make it panic.
#[must_use]
pub fn remap_band_gains(old: &[Real], new_count: usize) -> Vec<Real> {
    if old.is_empty() {
        return vec![0.0; new_count];
    }
    if old.len() == new_count {
        return old.to_vec();
    }
    remap_by_frequency(
        &standard_centres(old.len()),
        old,
        &standard_centres(new_count),
    )
}

/// Carry a curve that sits on `old_centres` over to the standard ladder of `new_count` bands
/// ([`standard_centres`]), by frequency: the gains [`remap_band_gains`] gives a curve on the
/// standard ladder, for a curve on any ladder.
///
/// What a band-count change should use whenever the live curve's centres are at hand, since
/// the live curve need not sit on the standard ladder of its count (audit report #13, second
/// review): a Windows twenty-band preset keeps its own centres on purpose (R4), and a band dragged
/// in the window keeps where it was dragged to. Read from its own centres, a Windows twenty-band
/// curve's +6 dB at 10 kHz lands on thirty-one bands as 6.00 dB on the 10 kHz band, where read as
/// if it sat on the standard ladder it gave 3.98 dB there and slid its peak up to 11-12 kHz.
///
/// The reading is [`fit_preset_gains`]'s, rules and all: an equal count copies the gains, band for
/// band, which is also how a curve on the Windows twenty-band ladder moves to [`band_table`]'s —
/// no centre moves by more than 0.17 of an octave. The result sits on the standard ladder of
/// `new_count`, so from there on [`remap_band_gains`] carries it, and a trip up and back down
/// returns to the standard ladder of the old count, not to the old centres. Allocates; call it off
/// the audio thread.
#[must_use]
pub fn remap_curve(old_centres: &[Real], old_gains: &[Real], new_count: usize) -> Vec<Real> {
    fit_preset_gains(old_centres, old_gains, &standard_centres(new_count))
}

/// The gains a preset's curve takes on the user's live band ladder.
///
/// The port of `DfxDspPrivate::getGraphicEqInfoFromVals` (`DfxDspEq.cpp:127-247`, upstream
/// 38e3343, f3d9f23, 12003f0), which is why a user on thirty-one bands who picks a ten-band factory
/// preset stays on thirty-one bands:
///
/// - **Different band counts:** the live ladder is kept and only the gains move. The upstream
///   comment explains why the frequencies are not carried across too: carrying centres across
///   counts "produced incorrect/overlapping ranges". The gains, though, are read *by frequency*
///   from the preset's own centres, where the original reads them by position (**changed on
///   purpose, audit report #13**; see [`remap_band_gains`] for what position did to a curve):
///   - onto **more** bands, each live band takes the preset's curve at its centre, linear in
///     log-frequency between the two preset bands either side, and past the preset's first and
///     last band tapering to 0 dB over one of the preset's own band spacings, which is roughly
///     how far the end band's skirt reaches;
///   - onto **fewer** bands, a preset curve that is itself a reading of some curve on the live
///     ladder — ten bands taken to thirty-one, say — comes back as that curve, exactly; any other
///     is read at the live centres the same way, tapered past the preset's ends too, and a live
///     band at either end of its ladder also takes what lies beyond it: the preset's gains past
///     it, tapered over one of the live ladder's spacings, when one of them goes further from
///     0 dB the same way than the band's own reading, so that a sub-bass boost on thirty-one
///     bands is not lost on ten, whose lowest band is 62.5 Hz.
///     The first is the least-squares fit of the live ladder to the preset's points, and it is
///     taken only where it can be trusted (see `exact_preimage`): the preset's ladder must be at
///     least as fine as the live one wherever they overlap, and the fit must reproduce every
///     preset point. Anything less and the fit invents gain. On a curve with detail finer than the
///     live ladder it rings: three thirty-one-band bands at +12 dB beside three at −12 dB fitted
///     to ten bands at +16.5 dB. Past a preset's ends, or across a gap in its ladder, it
///     extrapolates: a seven-band tilt from 0 to +6 dB over 150 Hz–2 kHz fitted to five bands at
///     −2.0 dB at 62.5 Hz and +7.6 dB at 4 kHz.
///
///   **Changed on purpose (audit report #13, held ends).** Past the preset's ends the curve used
///   to be held flat at its end gain. Every live band out there is a whole peaking section, and
///   they add up: a ten-band +6 dB at 62.5 Hz copied onto the five thirty-one-band bands from
///   20 to 50 Hz came out as a sub-bass shelf peaking at +12.8 dB at 25 Hz, where the preset's own
///   response there is +0.5 dB — the bass boost slid to the edge of hearing again, the thing
///   reading by frequency was for. Across the shipped presets on thirty-one bands, the worst
///   departure from a preset's own response below 45 Hz fell from 25.3 dB to 4.7 dB with the
///   taper. Shrinking the other way lost a boost the smaller ladder does not reach: +9 dB on
///   thirty-one bands' 20-40 Hz came out on ten bands as 0 dB everywhere.
/// - **Equal band counts:** the gains are copied as they are. The original also copies the
///   preset's centre frequencies onto the live ladder then (`DfxDspEq.cpp:229-241`); that half is
///   the caller's, since this returns gains only — pair the result with `preset_centres` when the
///   counts match and with `live_centres` when they do not.
/// - **A preset with no equalizer:** a flat curve at the live count. The original also switches
///   the equalizer on for such a preset (`DfxDspEq.cpp:144-158`); that too is the caller's.
///
/// A preset band is a centre with a gain, so the preset's band count is the shorter of the two
/// slices, the same rule [`GraphicEq::set_bands`] applies to a curve that reaches it. Centres are
/// read as the equalizer installs them — clamped to its 10 Hz–21 kHz window, a NaN taken as the
/// bottom of it — and need not be in order.
#[must_use]
pub fn fit_preset_gains(
    preset_centres: &[Real],
    preset_gains: &[Real],
    live_centres: &[Real],
) -> Vec<Real> {
    let preset_bands = preset_centres.len().min(preset_gains.len());
    if preset_bands == 0 {
        return vec![0.0; live_centres.len()];
    }
    if preset_bands == live_centres.len() {
        return preset_gains[..preset_bands].to_vec();
    }
    remap_by_frequency(
        &preset_centres[..preset_bands],
        &preset_gains[..preset_bands],
        live_centres,
    )
}

/// A centre as the equalizer would install it, on the natural-log axis the curve is read along.
fn log_centre(hz: Real) -> f64 {
    let hz = if hz.is_nan() {
        MIN_BAND_FREQ_HZ
    } else {
        hz.clamp(MIN_BAND_FREQ_HZ, MAX_BAND_FREQ_HZ)
    };
    f64::from(hz).ln()
}

/// `(log-frequency, gain)` points, sorted by frequency. Equal centres keep their order.
fn curve_points(centres: &[Real], gains: &[Real]) -> Vec<(f64, f64)> {
    let mut points: Vec<(f64, f64)> = centres
        .iter()
        .zip(gains)
        .map(|(hz, gain)| (log_centre(*hz), f64::from(*gain)))
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    points
}

/// Where a reading at `x` falls on a sorted ladder: the index at or below it and the index above
/// it with the fraction of the way between them, or a single index when `x` is past an end or
/// exactly on a centre. Equal centres resolve to the last of them, so the division is never by
/// zero.
fn locate(ladder: &[(f64, f64)], x: f64) -> (usize, Option<(usize, f64)>) {
    let last = ladder.len() - 1;
    if x.is_nan() || x <= ladder[0].0 {
        return (0, None);
    }
    if x >= ladder[last].0 {
        return (last, None);
    }
    let upper = ladder.partition_point(|point| point.0 <= x);
    let lower = upper - 1;
    if ladder[lower].0 == x {
        return (lower, None);
    }
    let fraction = (x - ladder[lower].0) / (ladder[upper].0 - ladder[lower].0);
    (lower, Some((upper, fraction)))
}

/// How far past each end of a sorted ladder its end band reaches: the distance, in log-frequency,
/// to the nearest band above the first and below the last. Infinite for a ladder with one
/// distinct centre, which has no spacing to go by.
fn end_spans(ladder: &[(f64, f64)]) -> (f64, f64) {
    let (first, last) = (ladder[0].0, ladder[ladder.len() - 1].0);
    let low = ladder
        .iter()
        .find(|point| point.0 > first)
        .map_or(f64::INFINITY, |point| point.0 - first);
    let high = ladder
        .iter()
        .rev()
        .find(|point| point.0 < last)
        .map_or(f64::INFINITY, |point| last - point.0);
    (low, high)
}

/// What is left of an end band's gain `distance` past it, over a reach of `span`: all of it at
/// the band, none of it a whole span away, a straight line in log-frequency between.
fn taper(distance: f64, span: f64) -> f64 {
    (1.0 - distance / span).clamp(0.0, 1.0)
}

/// A reading of a sorted ladder at `x` as weights on at most two of its bands, the same weights
/// whichever way the ladder is being read: between two bands, the straight line in log-frequency
/// between them; past either end, the end band tapered to 0 dB over its spacing ([`end_spans`]),
/// audit report #13's held ends.
fn reading_weights(ladder: &[(f64, f64)], spans: (f64, f64), x: f64) -> [(usize, f64); 2] {
    let (first, last) = (ladder[0].0, ladder[ladder.len() - 1].0);
    match locate(ladder, x) {
        (index, None) if index == 0 && x < first => [(0, taper(first - x, spans.0)), (0, 0.0)],
        (index, None) if x > last => [(index, taper(x - last, spans.1)), (index, 0.0)],
        (index, None) => [(index, 1.0), (index, 0.0)],
        (lower, Some((upper, fraction))) => [(lower, 1.0 - fraction), (upper, fraction)],
    }
}

/// The curve through `points` at `x`: linear between neighbours, tapered to 0 dB past the ends.
fn read_curve(points: &[(f64, f64)], spans: (f64, f64), x: f64) -> f64 {
    reading_weights(points, spans, x)
        .iter()
        .map(|&(index, weight)| weight * points[index].1)
        .sum()
}

/// What `points` has past the end of a live ladder that ends at `edge`, for the band there to
/// take (see [`fit_preset_gains`]): each point beyond it, tapered over the live ladder's `span`,
/// and of those the one furthest from 0 dB. Zero when nothing lies beyond, or when the live ladder
/// has no spacing to reach by.
fn beyond(points: &[(f64, f64)], edge: f64, span: f64, below: bool) -> f64 {
    if !span.is_finite() {
        return 0.0;
    }
    points
        .iter()
        .filter(|point| {
            if below {
                point.0 < edge
            } else {
                point.0 > edge
            }
        })
        .map(|point| point.1 * taper((point.0 - edge).abs(), span))
        .fold(
            0.0,
            |most: f64, gain| if gain.abs() > most.abs() { gain } else { most },
        )
}

/// The remap both public functions share, for two ladders of different lengths.
fn remap_by_frequency(from_centres: &[Real], gains: &[Real], to_centres: &[Real]) -> Vec<Real> {
    if to_centres.is_empty() || from_centres.is_empty() {
        return vec![0.0; to_centres.len()];
    }
    let points = curve_points(from_centres, gains);
    let spans = end_spans(&points);
    let targets: Vec<f64> = to_centres.iter().map(|hz| log_centre(*hz)).collect();
    let mut read: Vec<f64> = targets
        .iter()
        .map(|x| read_curve(&points, spans, *x))
        .collect();
    if targets.len() < points.len()
        && let Some(preimage) = exact_preimage(&points, &targets, &read)
    {
        // Solved in `f64` from `f32` readings, the preimage lands within a millionth of a decibel
        // of the curve that produced them, not on it: a band that was flat would come back at
        // -2e-8 dB, which the equalizer designs a live section for — it bypasses only an exact
        // zero. A hundred-thousandth of a decibel is far below both that error's size and
        // anything audible, and a gain written with five decimals, as a `.fac` writes one, comes
        // back as written.
        return preimage
            .into_iter()
            .map(|gain| ((gain * 1e5).round() / 1e5 + 0.0) as Real)
            .collect();
    }

    // The live ladder's end bands also stand for what lies past them: a point beyond the lowest
    // band, tapered over that band's reach, which goes further from 0 dB the same way than the
    // band's own reading, is what the band takes. A reading is inside the curve's range and so
    // is a tapered gain, so this invents nothing.
    let mut sorted: Vec<(f64, f64)> = targets.iter().map(|x| (*x, 0.0)).collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let live_spans = end_spans(&sorted);
    let (lowest, highest) = (sorted[0].0, sorted[sorted.len() - 1].0);
    for (x, gain) in targets.iter().zip(&mut read) {
        let past = if *x == lowest {
            beyond(&points, lowest, live_spans.0, true)
        } else if *x == highest {
            beyond(&points, highest, live_spans.1, false)
        } else {
            continue;
        };
        if past.abs() > gain.abs() && past * *gain >= 0.0 {
            *gain = past;
        }
    }
    read.into_iter().map(|gain| (gain + 0.0) as Real).collect()
}

/// The curve on the `targets` ladder whose reading at the old centres is the old curve, when there
/// is exactly one and the old points pin it down.
///
/// Least squares over the reading `A` (`AᵀA·x = Aᵀ·g`), with two rules first that keep it from
/// inventing gain (audit report #13, second review):
///
/// - **A live band past the old curve's first or last point is held**, at what the plain reading
///   gives it: the end gain tapered over the old curve's end spacing. Nothing on that side says
///   what it was, and solving for it extrapolates the slope inside: a seven-band tilt from 0 to
///   +6 dB over 150 Hz–2 kHz, fitted to five bands, came back as −2.03, 1.18, 4.39, 7.61 and
///   6.00 dB, a steeper tilt that bent back at 16 kHz.
/// - **Every other live band must have an old point on it or on each side of it before the next
///   live band.** A band with points on one side only is extrapolated just the same, from inside
///   the curve: a preset with +3 dB at 90 Hz, 0 dB at 62.5 Hz and nothing else below 1 kHz,
///   fitted to five bands, put +11.4 dB at 250 Hz. With a point on or either side of every band
///   left to solve for, no gain is worked out from one side alone, and wherever one comes from a
///   slope there is an equation to spare that checks it. So a curve that is not a reading of one
///   on this ladder fails the check below instead of being matched by a made-up one.
///
/// A curve grown from this ladder on its way back meets both rules for every pair of the window's
/// counts, since each larger ladder reaches as far at both ends and has a band on or between
/// every two neighbours of the smaller. An old point past this ladder's ends is read the way the
/// grow wrote it, as this ladder's end band tapered over its spacing ([`reading_weights`]), so
/// the bands a grow tapered out to 20 Hz check the answer rather than contradict it.
/// The answer is kept only if it reproduces every old point to a thousandth of a decibel;
/// otherwise the curve is read (see [`fit_preset_gains`] for why).
fn exact_preimage(points: &[(f64, f64)], targets: &[f64], read: &[f64]) -> Option<Vec<f64>> {
    const TOLERANCE_DB: f64 = 1e-3;

    let n = targets.len();
    // The live ladder in frequency order, remembering where each band sits in the caller's.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| targets[*a].total_cmp(&targets[*b]));
    let ladder: Vec<(f64, f64)> = order.iter().map(|&band| (targets[band], 0.0)).collect();

    let (first, last) = (points[0].0, points[points.len() - 1].0);
    let held: Vec<bool> = targets.iter().map(|x| *x < first || *x > last).collect();
    for (position, &band) in order.iter().enumerate() {
        if held[band] {
            continue;
        }
        let x = targets[band];
        let below = position
            .checked_sub(1)
            .map_or(f64::NEG_INFINITY, |lower| ladder[lower].0);
        let above = ladder
            .get(position + 1)
            .map_or(f64::INFINITY, |upper| upper.0);
        let from_below = points.iter().any(|point| below < point.0 && point.0 <= x);
        let from_above = points.iter().any(|point| x <= point.0 && point.0 < above);
        if !(from_below && from_above) {
            return None;
        }
    }

    // Each old point as weights on at most two live bands: the reading of the live ladder there.
    let spans = end_spans(&ladder);
    let rows: Vec<[(usize, f64); 2]> = points
        .iter()
        .map(|point| reading_weights(&ladder, spans, point.0).map(|(at, w)| (order[at], w)))
        .collect();

    // A held band is a known, so its row says so and its share of a point moves to the right-hand
    // side of the points it touches.
    let mut normal = vec![vec![0.0_f64; n + 1]; n];
    for (band, row) in normal.iter_mut().enumerate() {
        if held[band] {
            row[band] = 1.0;
            row[n] = read[band];
        }
    }
    for (weights, point) in rows.iter().zip(points) {
        let known: f64 = weights
            .iter()
            .filter(|(band, _)| held[*band])
            .map(|&(band, weight)| weight * read[band])
            .sum();
        for &(i, wi) in weights.iter().filter(|(band, _)| !held[*band]) {
            for &(j, wj) in weights.iter().filter(|(band, _)| !held[*band]) {
                normal[i][j] += wi * wj;
            }
            normal[i][n] += wi * (point.1 - known);
        }
    }
    let solution = solve(normal)?;

    let reproduces = rows.iter().zip(points).all(|(weights, point)| {
        let reading: f64 = weights.iter().map(|&(i, w)| w * solution[i]).sum();
        (reading - point.1).abs() <= TOLERANCE_DB
    });
    reproduces.then_some(solution)
}

/// Gaussian elimination with partial pivoting on an augmented `n × (n+1)` matrix.
fn solve(mut matrix: Vec<Vec<f64>>) -> Option<Vec<f64>> {
    let n = matrix.len();
    for column in 0..n {
        let pivot = (column..n).max_by(|a, b| {
            matrix[*a][column]
                .abs()
                .total_cmp(&matrix[*b][column].abs())
        })?;
        let magnitude = matrix[pivot][column].abs();
        if magnitude.is_nan() || magnitude == 0.0 {
            return None;
        }
        matrix.swap(column, pivot);
        let (done, rest) = matrix.split_at_mut(column + 1);
        let pivot_row = &done[column];
        for row in rest {
            let factor = row[column] / pivot_row[column];
            if factor != 0.0 {
                for (value, above) in row.iter_mut().zip(pivot_row).skip(column) {
                    *value -= factor * above;
                }
            }
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let tail: f64 = matrix[row][row + 1..n]
            .iter()
            .zip(&x[row + 1..])
            .map(|(a, b)| a * b)
            .sum();
        x[row] = (matrix[row][n] - tail) / matrix[row][row];
    }
    x.iter().all(|value| value.is_finite()).then_some(x)
}

/// The graphic equalizer: a cascade of peaking sections, one per band.
///
/// A band that is moved crossfades from its old design to its new one over
/// [`crate::smooth::GLIDE_SECONDS`] rather than switching between two samples (audit report #11;
/// [`crate::smooth`] has why a crossfade), and a band that goes to or comes back from exactly
/// 0 dB fades out to, or in from, the bypass. A new band count crossfades the whole curve: the
/// old ladder plays on beside the new one for the same 20 ms, and the output moves from one to the
/// other. Until the equalizer has processed audio since it was built or cleared, or since its
/// owner last left it out ([`GraphicEq::sit_out`]), a change lands at once: there is nothing heard
/// to fade from.
#[derive(Clone, Debug)]
pub struct GraphicEq {
    sections: [FadingSection; SOS_MAX_SECTIONS],
    /// One bit per section that is crossfading, so a block with none runs the plain cascade. A
    /// section past the live bands can have one: see [`GraphicEq::span`].
    fading: u32,
    /// Audio has gone through since the equalizer was built, last cleared or last left out.
    heard: bool,
    /// The ladder a new band count replaced, playing on while the new one fades in: its first
    /// `outgoing_bands` sections run, and none once the crossfade is over.
    outgoing: [FadingSection; SOS_MAX_SECTIONS],
    outgoing_bands: usize,
    /// The new ladder's share of the output while the old one fades out, 0 to 1; still at 1
    /// whenever no ladder is being replaced.
    ladder_fade: Ramp,
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
            sections: [FadingSection::new(); SOS_MAX_SECTIONS],
            fading: 0,
            heard: false,
            outgoing: [FadingSection::new(); SOS_MAX_SECTIONS],
            outgoing_bands: 0,
            ladder_fade: Ramp::new(1.0),
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

    /// Switch the whole equalizer in or out.
    ///
    /// Switched out, it is skipped, so every section keeps what it held when the switch went off,
    /// perhaps minutes of music ago; the original resumes from that (`dfxpProcessReal.cpp:143-157`
    /// skips the block, nothing clears it). With 62.5 Hz at +3 dB under loud bass, switching the
    /// equalizer off and back on rang the old state out into silence at −16.8 dBFS, the thump
    /// [`GraphicEq::set_band_boost`] already stops for one band coming back from 0 dB (audit
    /// report #10). So on the way back in every section starts from rest. Only the edge does it:
    /// the engine passes the switch on every parameter change, and a running equalizer told to
    /// stay on keeps its state.
    pub fn set_enabled(&mut self, on: bool) {
        if on && !self.enabled {
            self.reset();
        }
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
    ///
    /// Once the equalizer has been heard, the old curve fades out to the flat one over
    /// [`crate::smooth::GLIDE_SECONDS`] rather than vanishing between two samples.
    pub fn set_num_bands(&mut self, num_bands: usize) {
        self.relayout(num_bands);
        for band in 0..self.num_bands {
            self.install(band, BiquadCoeffs::UNITY);
        }
    }

    /// Lay out a new band count, every band flat, and hand the sections over to it.
    ///
    /// The original, and this port before audit #11, cleared every section here, so a preset
    /// change from a ten-band curve to a 31-band one, or the user changing the band count, dropped
    /// the old curve in one sample and brought the new one in from rest in the next: ten bands
    /// with 62.5 Hz at +6 dB changed to 31 with 63 Hz at +6 dB under a 50 Hz tone at 0.3 moved the
    /// waveform by 0.143 between two samples, and the click reached −18.4 dBFS above 300 Hz, the
    /// same click a changed curve at a fixed band count made before its bands crossfaded.
    ///
    /// The band counts do not line up section for section, so the sections cannot crossfade one
    /// by one as a moved band does; the whole ladder does instead. The old sections move to a
    /// second bank that plays on, as they were, while the new ladder starts from rest — as a band
    /// that comes back from the bypass does, and as the original starts every section here — and
    /// [`GraphicEq::process`] crossfades from the old cascade's output to the new one's over
    /// [`crate::smooth::GLIDE_SECONDS`]. The second bank runs only for those 20 ms. The same
    /// change now moves the waveform by no more than the tone does on its own, 0.0030, and above
    /// 300 Hz peaks at −67.8 dBFS.
    ///
    /// A band count asked for while that crossfade runs cannot take the old bank, which is still
    /// fading out, so the new ladder's sections move to the newest one section by section instead,
    /// each crossfading from its design to the one it has in the newest ladder, and those past the
    /// newest's end fading out to the bypass: a curve that morphs for 20 ms instead of one that
    /// crossfades, but never a step. A ladder nobody has heard yet, because no audio has gone
    /// through since it went in, is simply replaced.
    fn relayout(&mut self, num_bands: usize) {
        let num_bands = num_bands.clamp(1, SOS_MAX_SECTIONS);
        let old_span = self.span();
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
        self.requested_boost_db.fill(0.0);
        self.installed_boost_db.fill(0.0);

        if !self.heard || self.new_ladder_unheard() {
            // Nothing to fade from, or only a ladder nobody has heard: start from rest, at once.
            self.sections = [FadingSection::new(); SOS_MAX_SECTIONS];
            self.fading = 0;
        } else if !self.ladder_fade.is_gliding() {
            self.outgoing = self.sections;
            self.outgoing_bands = old_span;
            self.sections = [FadingSection::new(); SOS_MAX_SECTIONS];
            self.fading = 0;
            self.ladder_fade = Ramp::new(0.0);
            self.ladder_fade
                .glide_to(1.0, glide_frames(self.sample_rate));
        } else {
            for band in num_bands..SOS_MAX_SECTIONS {
                self.install(band, BiquadCoeffs::UNITY);
            }
        }
    }

    /// The new ladder has not played a frame since a band count change put it in: its designs
    /// land at once, from rest, while the old ladder carries the sound.
    fn new_ladder_unheard(&self) -> bool {
        self.ladder_fade.is_gliding() && self.ladder_fade.value() == 0.0
    }

    /// How many sections [`GraphicEq::process`] runs: the live bands, and any past them still
    /// fading out after a band count changed in the middle of a crossfade.
    fn span(&self) -> usize {
        let highest_fading = (u32::BITS - self.fading.leading_zeros()) as usize;
        self.num_bands.max(highest_fading)
    }

    /// End a ladder crossfade at once: the old ladder stops playing.
    fn drop_outgoing(&mut self) {
        self.outgoing_bands = 0;
        self.ladder_fade = Ramp::new(1.0);
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
            self.install(band, BiquadCoeffs::UNITY);
            self.installed_boost_db[band] = 0.0;
            return;
        }

        let clamped = boost_db.clamp(-MAX_BOOST_OR_CUT_DB, MAX_BOOST_OR_CUT_DB);
        if clamped == self.installed_boost_db[band] {
            return;
        }
        self.install(band, calc_parametric(self.sample_rate, f0, clamped, self.q));
        self.installed_boost_db[band] = clamped;
    }

    /// Hand a band its new design: crossfaded once the equalizer has been heard, at once before.
    ///
    /// A bypassed section is skipped by `process`, so its history is whatever it held the moment it
    /// went to exactly 0 dB — perhaps minutes of music ago. The original resumes from it
    /// (`GraphicEqSet.cpp:288-295`, `SosProcess.cpp:567-571`), and a band that comes back from
    /// 0 dB on a preset change or Restore Defaults rang that old state out as a low thump: 62.5 Hz
    /// taken +3 → 0 → −1 dB after loud bass rang at −17.5 dBFS into silence (audit report #10).
    /// [`FadingSection`] brings a section back from the bypass from rest, and fades it in. A
    /// running section carries its history into the new design, so moving a live band does not
    /// click either.
    fn install(&mut self, band: usize, design: BiquadCoeffs) {
        let at_once = !self.heard || self.new_ladder_unheard();
        let section = &mut self.sections[band];
        section.set_design(design, glide_frames(self.sample_rate));
        if at_once {
            section.settle();
        }
        let bit = 1_u32 << band;
        if section.is_fading() {
            self.fading |= bit;
        } else {
            self.fading &= !bit;
        }
    }

    /// Apply a whole curve at once — the preset-load path.
    ///
    /// A curve with a new band count replaces the ladder, the whole old curve crossfading into
    /// the new one over [`crate::smooth::GLIDE_SECONDS`] once the equalizer has been heard; one
    /// with the same count moves each band that changed, as [`GraphicEq::set_band_boost`] does.
    pub fn set_bands(&mut self, centers_hz: &[Real], boosts_db: &[Real]) {
        let count = centers_hz.len().min(boosts_db.len());
        if count != self.num_bands {
            // Not `set_num_bands`: flattening first would send every band through the bypass on
            // its way to the new curve.
            self.relayout(count);
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
    ///
    /// A crossfade under way is history too: every band lands on its newest design, a ladder
    /// being replaced stops playing, and until audio goes through again a change lands at once.
    pub fn reset(&mut self) {
        for section in &mut self.sections {
            section.reset();
        }
        self.fading = 0;
        self.drop_outgoing();
        self.heard = false;
    }

    /// Tell the equalizer its owner ran a block without it: FxSound is switched off, and the
    /// sections stand still until it comes back, as the original's do
    /// (`dfxpProcessReal.cpp:143-157`).
    ///
    /// Only [`GraphicEq::process`] plays a crossfade, so one that started, or was waiting, while
    /// the equalizer was left out would play when it came back: 20 ms of a curve the listener
    /// last heard before switching off. With 31.25 Hz at +12 dB under a tone there at 0.05, the
    /// band set to 0 dB with FxSound off, switching back on played 0.134 for a steady 0.048. So
    /// every band lands on its newest design, a ladder being replaced stops playing, and until
    /// audio goes through again a change lands at once, as it does before the first block. The
    /// filters keep their history, as they do across the switch.
    pub fn sit_out(&mut self) {
        if !self.heard {
            // Never heard, cleared or already left out: every change since landed at once.
            return;
        }
        for section in &mut self.sections {
            section.settle();
        }
        self.fading = 0;
        self.drop_outgoing();
        self.heard = false;
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
    /// what the original does — and why a section that gets switched off keeps stale state, which
    /// [`FadingSection`] leaves behind when the section comes back, and [`GraphicEq::set_enabled`]
    /// clears when the whole equalizer does.
    ///
    /// While no band is crossfading this is the cascade the equalizer always ran, sample for
    /// sample; a block with a fade in it takes the slower path for all of it, and costs a second
    /// section per fading band, and the whole old ladder while a new band count fades in.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.enabled || channels == 0 || channels > crate::biquad::MAX_CHANNELS {
            return;
        }
        if !buffer.is_empty() {
            self.heard = true;
        }
        if self.fading == 0 && self.outgoing_bands == 0 {
            for frame in buffer.chunks_exact_mut(channels) {
                for section in &mut self.sections[..self.num_bands] {
                    let section = section.steady();
                    if !section.coeffs.on {
                        continue;
                    }
                    for (channel, sample) in frame.iter_mut().enumerate() {
                        *sample = section.tick(channel, *sample);
                    }
                }
            }
            return;
        }
        let span = self.span();
        for frame in buffer.chunks_exact_mut(channels) {
            if self.outgoing_bands == 0 {
                for section in &mut self.sections[..span] {
                    section.process_frame(frame);
                }
                continue;
            }
            let mut old = [0.0; crate::biquad::MAX_CHANNELS];
            let old = &mut old[..channels];
            old.copy_from_slice(frame);
            for section in &mut self.outgoing[..self.outgoing_bands] {
                section.process_frame(old);
            }
            for section in &mut self.sections[..span] {
                section.process_frame(frame);
            }
            // Linear, for the reason `FadingSection::process_frame` gives: the two ladders hear
            // the same input and answer it much alike. The last frame of the fade is the new
            // ladder's own output, and from the next the old one is not run.
            let share = self.ladder_fade.advance();
            if self.ladder_fade.is_gliding() {
                for (sample, old) in frame.iter_mut().zip(old.iter()) {
                    *sample = old + share * (*sample - old);
                }
            } else {
                self.outgoing_bands = 0;
            }
        }
        self.fading = 0;
        for (band, section) in self.sections[..span].iter().enumerate() {
            if section.is_fading() {
                self.fading |= 1 << band;
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
            // What the user set, not where a crossfade has got to.
            let design = section.design();
            if design.on {
                mag *= magnitude(&design, f);
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

    /// Peak of the output when silence follows a band that played loud bass at +3 dB, was set to
    /// exactly 0 dB for a while and then to `last_db`.
    fn ring_after_passing_through_zero(last_db: Real, via_zero: bool) -> Real {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        eq.set_band_boost(0, 3.0);
        let mut bass: Vec<Real> = (0..48_000)
            .flat_map(|n| {
                let s = (n as Real * 62.5 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                [s, s]
            })
            .collect();
        eq.process(&mut bass, 2);
        if via_zero {
            eq.set_band_boost(0, 0.0);
            let mut quiet = vec![0.0; 2 * 4_800];
            eq.process(&mut quiet, 2);
        }
        eq.set_band_boost(0, last_db);
        let mut silence = vec![0.0; 2 * 4_800];
        eq.process(&mut silence, 2);
        silence.iter().fold(0.0, |m: Real, s| m.max(s.abs()))
    }

    #[test]
    fn a_band_back_from_exactly_zero_db_starts_from_rest() {
        // Audit report #10: 62.5 Hz at +3 dB under loud bass, then 0 dB, then -1 dB — a preset
        // change or Restore Defaults. The section is skipped at 0 dB and kept the state it had,
        // and rang it out as a thump at -17.5 dBFS into silence. Now nothing but the denormal
        // bias comes out.
        let peak = ring_after_passing_through_zero(-1.0, true);
        assert!(peak < 1e-20, "the old state rang out at {peak}");
    }

    #[test]
    fn a_band_that_never_stopped_keeps_its_state_through_a_new_gain() {
        // Only a section coming back from bypass is cleared. One that is redesigned while it runs
        // — a band dragged from +3 to -1 dB — carries on, as it always did.
        assert!(ring_after_passing_through_zero(-1.0, false) > 1e-3);
    }

    /// Peak of the output when silence follows loud bass through 62.5 Hz at +3 dB, after the whole
    /// equalizer was told `on` a first and then a second time — off and back on, or on twice.
    fn ring_after_switching(first: bool, second: bool) -> Real {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        eq.set_band_boost(0, 3.0);
        let mut bass: Vec<Real> = (0..48_000)
            .flat_map(|n| {
                let s = (n as Real * 62.5 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                [s, s]
            })
            .collect();
        eq.process(&mut bass, 2);
        eq.set_enabled(first);
        let mut skipped = vec![0.5; 2 * 4_800];
        eq.process(&mut skipped, 2);
        eq.set_enabled(second);
        let mut silence = vec![0.0; 2 * 4_800];
        eq.process(&mut silence, 2);
        silence.iter().fold(0.0, |m: Real, s| m.max(s.abs()))
    }

    #[test]
    fn switching_the_equalizer_back_on_starts_every_band_from_rest() {
        // The same thump as audit report #10, for the whole equalizer: switched off, it is
        // skipped, and it came back with the state it had, ringing out at -16.8 dBFS into
        // silence. Now nothing but the denormal bias comes out.
        let peak = ring_after_switching(false, true);
        assert!(peak < 1e-20, "the old state rang out at {peak}");
    }

    #[test]
    fn an_equalizer_told_to_stay_on_keeps_its_state() {
        // The engine passes the switch on with every parameter change, a slider drag included;
        // only the off-to-on edge may clear anything, or every drag would click.
        let peak = ring_after_switching(true, true);
        assert!(peak > 1e-3, "a running equalizer lost its state: {peak}");
    }

    /// A `.fac` whose only equalizer band is +6 dB at 15 Hz — below the 20 Hz any table starts
    /// at, but a legal centre for a preset.
    const FAC_WITH_A_15_HZ_BAND: &str = "CLASS1 : Effect Type\n9: Version\nSub sonic\n\
        0: Double Params Flag\n1: Total number of elements\n0: Main 0\n0: Main 1\n0: Main 2\n\
        0: Main 3\n0: Main 4\n0: Main 5\n0: Element Number\n   0: Param 0\n   0: Param 1\n\
           0: Param 2\n   0: Param 3\n   0: Param 4\n   0: Param 5\n   0: Param 6\n\
        7: Number of Application Dependent Integers\n0: Number of Application Dependent Reals\n\
        0: Number of Application Dependent Strings\n1: Integer[0]\n1: Integer[1]\n\
        1: Integer[2]\n1: Integer[3]\n1: Integer[4]\n0: Integer[5]\n2: Integer[6]\n\
        10: Number of EQ Bands\n1: On/Off Flag\n\
        Band 1\n   15: CF\n   6: Boost/Cut\nBand 2\n   115.734: CF\n   0: Boost/Cut\n\
        Band 3\n   214.311: CF\n   0: Boost/Cut\nBand 4\n   396.85: CF\n   0: Boost/Cut\n\
        Band 5\n   734.867: CF\n   0: Boost/Cut\nBand 6\n   1360.79: CF\n   0: Boost/Cut\n\
        Band 7\n   2519.84: CF\n   0: Boost/Cut\nBand 8\n   4666.12: CF\n   0: Boost/Cut\n\
        Band 9\n   8640.48: CF\n   0: Boost/Cut\nBand 10\n   16000: CF\n   0: Boost/Cut\n";

    #[test]
    fn a_band_below_20_hz_stays_in_the_sub_bass() {
        // Audit report #12: the low-frequency Q cap went negative under 17.9 Hz, and a band at
        // 15 Hz became a flat +6 dB across the whole spectrum — 1 kHz and 10 kHz included.
        let preset = fxsound_preset::parse(FAC_WITH_A_15_HZ_BAND.as_bytes()).expect("parses");
        assert_eq!(preset.eq_bands[0].center_hz, 15.0);
        let centres: Vec<Real> = preset.eq_bands.iter().map(|b| b.center_hz).collect();
        let boosts: Vec<Real> = preset.eq_bands.iter().map(|b| b.boost_db).collect();

        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        eq.set_bands(&centres, &boosts);

        assert!(
            (eq.response_db(15.0) - 6.0).abs() < 0.25,
            "{}",
            eq.response_db(15.0)
        );
        for hz in [200.0, 1_000.0, 10_000.0] {
            let db = eq.response_db(hz);
            assert!(db.abs() < 0.1, "{hz} Hz moved by {db} dB");
        }

        // And it is a stable filter: an impulse dies away.
        let mut impulse = vec![0.0; 96_000];
        impulse[0] = 1.0;
        eq.process(&mut impulse, 1);
        assert!(impulse.iter().all(|s| s.is_finite()));
        assert!(impulse[48_000..].iter().all(|s| s.abs() < 1e-4));
    }

    #[test]
    fn twenty_bands_at_six_db_are_as_even_as_ten_or_thirty_one() {
        // Audit report R4. Every band at +6 dB, measured 100 Hz to 10 kHz at 48 kHz, away from the
        // ends where the 20 Hz Q cap and Nyquist bend every ladder. The paired twenty-band ladder
        // gave 10.5 dB on its pairs and 6.6 dB between them — 3.9 dB of ripple, where ten bands
        // give 2.3 and thirty-one 2.5; the half-octave one gives 1.9.
        fn ripple(count: usize) -> Real {
            let mut eq = GraphicEq::new();
            eq.set_sample_rate(48_000.0);
            eq.set_num_bands(count);
            for band in 0..count {
                eq.set_band_boost(band, 6.0);
            }
            let (mut low, mut high) = (Real::INFINITY, Real::NEG_INFINITY);
            for step in 0..=2_000 {
                let db = eq.response_db(100.0 * 100.0_f32.powf(step as Real / 2_000.0));
                low = low.min(db);
                high = high.max(db);
            }
            high - low
        }
        let (ten, twenty, thirty_one) = (ripple(10), ripple(20), ripple(31));
        assert!(twenty < 2.0, "twenty bands ripple by {twenty} dB");
        assert!(
            twenty < ten && twenty < thirty_one,
            "{ten} / {twenty} / {thirty_one}"
        );
        // The ten- and thirty-one-band ladders are not touched.
        assert!((ten - 2.33).abs() < 0.01 && (thirty_one - 2.47).abs() < 0.01);
    }

    #[test]
    fn the_twenty_band_ladder_is_the_half_octave_ladder_its_q_was_derived_for() {
        let (table, min_hz, max_hz) = band_table(20).expect("the 20-band table");
        assert_eq!(
            (min_hz, max_hz),
            (20.0, 16_000.0),
            "the edges, and so the Q, are kept"
        );
        let mut geometric = [0.0; 20];
        geometric_ladder(20, 20.0, 16_000.0, &mut geometric);
        for (band, (got, want)) in table.iter().zip(geometric).enumerate() {
            assert!(
                (got / want - 1.0).abs() < 1e-5,
                "band {band}: {got} against {want}"
            );
        }
        // Each centre sits inside the frequency range the window gives its band.
        for (band, centre) in table.iter().enumerate() {
            let (low, high) = band_frequency_range(band, 20, min_hz, max_hz);
            assert!(low <= *centre && *centre <= high, "band {band}");
        }
        assert!((derive_q(20.0, 16_000.0, 20, 1.0) - 2.827_742_6).abs() < 1e-5);
    }

    #[test]
    fn a_curve_on_the_windows_twenty_band_centres_keeps_them_and_their_ripple() {
        // Audit report R4, the part a new ladder cannot reach. A curve that brings its own
        // centres keeps them, so every band at +6 dB on the Windows centres still ripples by
        // 3.92 dB from 100 Hz to 10 kHz and 5.24 dB from 40 Hz to 12 kHz, where the same curve on
        // `band_table(20)` gives 1.85 and 2.99. Settings and presets on the old centres have to
        // be moved by whoever owns them, which is what the constant is for.
        fn ripple(centres: &[Real], low_hz: Real, high_hz: Real) -> Real {
            let mut eq = GraphicEq::new();
            eq.set_sample_rate(48_000.0);
            eq.set_bands(centres, &[6.0; 20]);
            assert_eq!(
                eq.center_frequencies(),
                centres,
                "the curve kept its centres"
            );
            let (mut low, mut high) = (Real::INFINITY, Real::NEG_INFINITY);
            for step in 0..=2_000 {
                let db = eq.response_db(low_hz * (high_hz / low_hz).powf(step as Real / 2_000.0));
                low = low.min(db);
                high = high.max(db);
            }
            high - low
        }
        let old = &WINDOWS_TWENTY_BAND_CENTRES_HZ[..];
        let (new, _, _) = band_table(20).expect("the 20-band table");
        let measured = [
            ripple(old, 100.0, 10_000.0),
            ripple(new, 100.0, 10_000.0),
            ripple(old, 40.0, 12_000.0),
            ripple(new, 40.0, 12_000.0),
        ];
        for (got, want) in measured.iter().zip([3.92, 1.85, 5.24, 2.99]) {
            assert!((got - want).abs() < 0.01, "ripple {measured:?} dB");
        }
        // Same ends, so the Q and the band edges did not move with the centres.
        assert_eq!((old[0], old[19]), (new[0], new[19]));
    }

    // --- Band-count remapping (U1) and preset fitting (U2) ------------------------------------
    //
    // Changed on purpose: audit report #13. These tests used to hold the original's remap by
    // position bit for bit (`GraphicEqSet.cpp:200-245`, `DfxDspEq.cpp:182-227`, compiled with gcc
    // and printed with `%.9g`). That remap slid a ten-band bass boost from 62.5 Hz to 20 Hz on
    // thirty-one bands and brought ten bands home from thirty-one up to 2.4 dB away from where
    // they started, so it is gone, and so are its bit patterns. What replaces them is checked
    // against the arithmetic it is defined by — linear in log-frequency, the ends held — computed
    // here in `f64` from the ladders themselves, so a reader can redo any of it on paper.

    /// A ten-band curve with both signs, a zero, the full ±12 dB and fractional gains.
    const TEN: [Real; 10] = [6.0, 4.5, -3.0, 0.0, 2.25, -12.0, 12.0, 1.5, -0.75, 3.0];
    const FIVE: [Real; 5] = [-6.0, 3.0, 0.0, 9.0, -1.5];

    /// Thirty-one bands whose gain is their own 1-based index.
    fn thirty_one_numbered() -> Vec<Real> {
        (1..=31).map(|band| band as Real).collect()
    }

    fn ladder(count: usize) -> Vec<Real> {
        standard_centres(count)
    }

    fn assert_close(got: Real, want: Real, what: &str) {
        assert!(
            (got - want).abs() < 1e-4,
            "{what}: got {got}, expected {want}"
        );
    }

    fn assert_all_close(got: &[Real], want: &[Real], tolerance: Real, what: &str) {
        assert_eq!(got.len(), want.len(), "{what}: lengths differ");
        for (band, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(
                (g - w).abs() <= tolerance,
                "{what}: band {band} is {g}, expected {w}"
            );
        }
    }

    /// The curve through `(centres, gains)` read at `hz` — linear in log-frequency, and past the
    /// ends the end gain tapered to 0 dB over the end spacing — worked independently of the code
    /// under test, for distinct centres in ascending order.
    ///
    /// Changed on purpose: audit report #13 (held ends). Past the ends the curve was held at its
    /// end gain, which piled a sub-bass shelf the preset never had onto every band out there.
    fn reading(centres: &[Real], gains: &[Real], hz: Real) -> Real {
        let x = f64::from(hz).ln();
        let xs: Vec<f64> = centres.iter().map(|c| f64::from(*c).ln()).collect();
        let last = xs.len() - 1;
        if last == 0 {
            return gains[0];
        }
        if x <= xs[0] {
            let left = 1.0 - (xs[0] - x) / (xs[1] - xs[0]);
            return (f64::from(gains[0]) * left.max(0.0)) as Real;
        }
        if x >= xs[last] {
            let left = 1.0 - (x - xs[last]) / (xs[last] - xs[last - 1]);
            return (f64::from(gains[last]) * left.max(0.0)) as Real;
        }
        let upper = xs.iter().position(|c| *c > x).expect("inside the ladder");
        let t = (x - xs[upper - 1]) / (xs[upper] - xs[upper - 1]);
        (f64::from(gains[upper - 1]) + (f64::from(gains[upper]) - f64::from(gains[upper - 1])) * t)
            as Real
    }

    #[test]
    fn a_ten_band_bass_boost_stays_at_its_frequency_on_thirty_one_bands() {
        // Audit report #13's case: +6 dB at 62.5 Hz on a flat ten-band curve. By position it
        // landed on thirty-one bands as +6 dB at 20 Hz and 0 dB at 63 Hz.
        let mut bass = [0.0; 10];
        bass[0] = 6.0;
        let remapped = remap_band_gains(&bass, 31);
        let (thirty_one, _, _) = band_table(31).expect("the 31-band table");
        assert_eq!(thirty_one[5], 63.0);

        // 63 Hz sits just above 62.5 Hz, a hundredth of the way to 115.734 Hz.
        let at_63 = 6.0 * (1.0 - (63.0_f64 / 62.5).ln() / (115.734_f64 / 62.5).ln());
        assert_close(remapped[5], at_63 as Real, "63 Hz");
        assert!(remapped[5] > 5.9, "63 Hz lost the boost: {}", remapped[5]);
        assert_close(
            remapped[6],
            3.596_018,
            "80 Hz, part of the way down the slope",
        );
        assert_close(remapped[7], 1.422_993, "100 Hz");
        // Changed on purpose: audit report #13 (held ends). Below the ten-band ladder the lowest
        // band's gain was held on all five bands from 20 to 50 Hz, which added up to a sub-bass
        // shelf peaking at +12.8 dB at 25 Hz. It now tapers to 0 dB over the ladder's first
        // spacing, 62.5 to 115.7 Hz, which is roughly how far the band's own skirt reaches.
        let spacing = (115.734_f64 / 62.5).ln();
        for band in 0..5 {
            let below = (62.5 / f64::from(thirty_one[band])).ln();
            let tapered = 6.0 * (1.0 - below / spacing).max(0.0);
            assert_close(
                remapped[band],
                tapered as Real,
                &format!("{} Hz", thirty_one[band]),
            );
        }
        assert_eq!(&remapped[..3], [0.0; 3], "20 to 31.5 Hz are past the reach");
        assert!((1.6..1.7).contains(&remapped[3]), "40 Hz: {}", remapped[3]);
        assert!((3.8..3.9).contains(&remapped[4]), "50 Hz: {}", remapped[4]);
        // From 125 Hz up, both neighbours are flat.
        assert!(remapped[8..].iter().all(|g| *g == 0.0), "{remapped:?}");
    }

    /// An equalizer holding `gains` on `centres` at 48 kHz, for its response.
    fn equalizer(centres: &[Real], gains: &[Real]) -> GraphicEq {
        let mut eq = GraphicEq::new();
        eq.set_sample_rate(48_000.0);
        eq.set_bands(centres, gains);
        eq
    }

    #[test]
    fn a_ten_band_bass_boost_on_thirty_one_bands_sounds_within_2_db_of_itself_below_45_hz() {
        // Audit report #13 (held ends), by the response rather than the gains, which is what the
        // gain tests could not see. +6 dB at 62.5 Hz on ten bands is +0.5 dB at 25 Hz and +1.9 dB
        // at 40 Hz as ten bands play it. Held on thirty-one bands' five bands from 20 to 50 Hz,
        // it played +12.7 dB at 25 Hz and +11.3 dB at 40 Hz, and the curve peaked at +12.8 dB at
        // 25.5 Hz. Tapered, it plays 0.2 and 2.9 dB there.
        let mut bass = [0.0; 10];
        bass[0] = 6.0;
        let own = equalizer(&ladder(10), &bass);
        let there = equalizer(&ladder(31), &remap_band_gains(&bass, 31));
        for step in 0..=40 {
            let hz = 20.0 * (45.0_f32 / 20.0).powf(step as Real / 40.0);
            let (want, got) = (own.response_db(hz), there.response_db(hz));
            assert!(
                (got - want).abs() < 2.0,
                "{hz} Hz: {got} dB on thirty-one bands, {want} dB on ten"
            );
        }
        // And the boost is still where it was: the loudest point of the curve is near 63 Hz, not
        // down at the edge of hearing.
        let loudest = (0..=200)
            .map(|step| 20.0 * 1000.0_f32.powf(step as Real / 200.0))
            .max_by(|a, b| there.response_db(*a).total_cmp(&there.response_db(*b)))
            .expect("points");
        assert!(
            (55.0..75.0).contains(&loudest),
            "the curve peaks at {loudest} Hz"
        );
    }

    #[test]
    fn a_windows_twenty_band_curves_ten_kilohertz_boost_stays_at_ten_kilohertz_on_thirty_one_bands()
    {
        // Audit report #13, second review. A Windows twenty-band `.fac` keeps its own centres (R4)
        // — 8, 10 and 16 kHz at the top — and a band-count change read it as if it sat on the
        // standard ladder, where its 10 kHz band is 11.3 kHz: on thirty-one bands, 0.18, 3.98 and
        // 4.21 dB on the 8, 10 and 12.5 kHz bands, 5.2 dB at 10 kHz and the peak slid to 11-12 kHz.
        // Read from its own centres it is 6.00 dB on the 10 kHz band.
        let mut treble = [0.0; 20];
        treble[18] = 6.0;
        let by_centres = remap_curve(&WINDOWS_TWENTY_BAND_CENTRES_HZ, &treble, 31);
        assert_eq!(ladder(31)[27], 10_000.0);
        assert_eq!(&by_centres[..27], [0.0; 27]);
        assert_eq!(by_centres[27], 6.0, "the 10 kHz band");
        assert_close(
            by_centres[28],
            reading(&WINDOWS_TWENTY_BAND_CENTRES_HZ, &treble, 12_500.0),
            "12.5 kHz, on the way down to 16 kHz",
        );
        let standard = remap_band_gains(&treble, 31);
        assert!((3.9..4.0).contains(&standard[27]), "{standard:?}");
        let own = equalizer(&WINDOWS_TWENTY_BAND_CENTRES_HZ, &treble);
        let there = equalizer(&ladder(31), &by_centres);
        assert!(
            (there.response_db(10_000.0) - own.response_db(10_000.0)).abs() < 1.0,
            "{} dB at 10 kHz, {} on the Windows ladder",
            there.response_db(10_000.0),
            own.response_db(10_000.0)
        );
        assert!(there.response_db(10_000.0) > there.response_db(12_000.0));

        // The same at the bottom: +6 dB on the 31.5 Hz band stays on the 31.5 Hz band.
        let mut sub = [0.0; 20];
        sub[1] = 6.0;
        let sub = remap_curve(&WINDOWS_TWENTY_BAND_CENTRES_HZ, &sub, 31);
        assert_eq!((sub[2], sub[3]), (6.0, 0.0), "31.5 and 40 Hz: {sub:?}");

        // And a band dragged in the window: ten bands with the first moved up to 85 Hz and at
        // +6 dB. Read as if it sat at 62.5 Hz, the curve peaked there, 0.44 of an octave low.
        let mut dragged_centres = ladder(10);
        dragged_centres[0] = 85.0;
        let mut dragged = [0.0; 10];
        dragged[0] = 6.0;
        let peak_of = |eq: &GraphicEq| {
            (0..=200)
                .map(|step| 20.0 * 1000.0_f32.powf(step as Real / 200.0))
                .max_by(|a, b| eq.response_db(*a).total_cmp(&eq.response_db(*b)))
                .expect("points")
        };
        let read_as_standard = equalizer(&ladder(31), &remap_band_gains(&dragged, 31));
        let read_from_centres =
            equalizer(&ladder(31), &remap_curve(&dragged_centres, &dragged, 31));
        assert!(peak_of(&read_as_standard) < 70.0);
        assert!(
            (75.0..95.0).contains(&peak_of(&read_from_centres)),
            "{} Hz",
            peak_of(&read_from_centres)
        );
    }

    #[test]
    fn a_sub_bass_boost_on_thirty_one_bands_is_not_lost_on_ten() {
        // Audit report #13 (held ends), the shrink. +9 dB on thirty-one bands' 20 to 40 Hz lies
        // wholly below ten bands' lowest centre, 62.5 Hz, and the reading at the ten centres found
        // 0 dB everywhere: the boost vanished. The lowest band now also takes what lies within its
        // reach below it, the 40 Hz band's +9 dB tapered over its spacing: +2.5 dB, which ten
        // bands play as +2.5 dB at 62.5 Hz — the thirty-one-band curve's own is +4.1 there — and
        // +0.4 dB at 31.5 Hz, where no ten-band curve reaches the +22.4 dB of its own.
        let mut sub = [0.0; 31];
        sub[..4].fill(9.0);
        let ten = remap_band_gains(&sub, 10);
        let expected = 9.0 * (1.0 - (62.5_f64 / 40.0).ln() / (115.734_f64 / 62.5).ln());
        assert_close(ten[0], expected as Real, "62.5 Hz");
        assert!(ten[0] > 2.4, "{ten:?}");
        assert_eq!(&ten[1..], [0.0; 9]);
        let there = equalizer(&ladder(10), &ten);
        assert!(there.response_db(62.5) > 2.4 && there.response_db(31.5) > 0.3);

        // A cut goes the same way, and a band whose own reading goes further, or the other way,
        // keeps it.
        let cut: Vec<Real> = sub.iter().map(|gain| -gain).collect();
        assert_close(remap_band_gains(&cut, 10)[0], -expected as Real, "a cut");
        let mut against = sub;
        against[5] = -3.0;
        against[4] = -3.0;
        let kept = remap_band_gains(&against, 10);
        assert!(kept[0] < 0.0, "the band's own reading is a cut: {kept:?}");
    }

    #[test]
    fn growing_reads_the_curve_at_each_new_centre_linear_in_log_frequency() {
        for (old, count) in [
            (&TEN[..], 31),
            (&TEN[..], 15),
            (&TEN[..], 20),
            (&FIVE[..], 20),
        ] {
            let from = ladder(old.len());
            let to = ladder(count);
            let remapped = remap_band_gains(old, count);
            let expected: Vec<Real> = to.iter().map(|hz| reading(&from, old, *hz)).collect();
            assert_all_close(
                &remapped,
                &expected,
                1e-5,
                &format!("{} -> {count}", old.len()),
            );
        }
    }

    #[test]
    fn ten_bands_through_thirty_one_and_back_come_home_as_they_left() {
        // By position they came back up to 2.4 dB away: [6, 4.65, -2.7, 0, 2.025, -9.6, 12, 2.55,
        // -0.375, 3]. By frequency the shrink finds the curve the grow read from.
        let there = remap_band_gains(&TEN, 31);
        let back = remap_band_gains(&there, 10);
        assert_eq!(back, TEN, "through {there:?}");

        let mut bass = [0.0; 10];
        bass[0] = 6.0;
        assert_eq!(remap_band_gains(&remap_band_gains(&bass, 31), 10), bass);
    }

    #[test]
    fn every_trip_to_more_bands_and_back_comes_home() {
        // Every pair of the window's counts, and one pair of geometric ladders. Each larger ladder
        // here reaches as far at both ends and has a band on or between every two neighbouring
        // bands of the smaller one, which is what makes the trip exact; 14 -> 15 -> 14 is not such a pair and has its own test.
        for (small, large) in [
            (5, 10),
            (5, 15),
            (5, 20),
            (5, 31),
            (10, 15),
            (10, 20),
            (10, 31),
            (15, 20),
            (15, 31),
            (20, 31),
            (7, 12),
        ] {
            let curve: Vec<Real> = (0..small)
                .map(|band| ((band * 5 % 7) as Real - 3.0) * 2.5)
                .collect();
            let back = remap_band_gains(&remap_band_gains(&curve, large), small);
            assert_all_close(
                &back,
                &curve,
                1e-5,
                &format!("{small} -> {large} -> {small}"),
            );
        }
    }

    #[test]
    fn a_shrink_of_a_curve_no_smaller_ladder_made_is_read_at_the_new_centres() {
        let numbered = thirty_one_numbered();
        let from = ladder(31);
        for count in [10, 5, 20, 15] {
            let expected: Vec<Real> = ladder(count)
                .iter()
                .map(|hz| reading(&from, &numbered, *hz))
                .collect();
            assert_all_close(
                &remap_band_gains(&numbered, count),
                &expected,
                1e-5,
                &format!("31 -> {count}"),
            );
        }
    }

    #[test]
    fn a_shrink_never_rings_past_the_curve_it_was_given() {
        // Three bands at +12 dB beside three at -12 dB: finer detail than ten bands can draw. A
        // least-squares fit of the ten-band ladder to it rings to +16.5 dB and -8.5 dB where the
        // curve is flat; the reading does not.
        let mut detail = [0.0; 31];
        detail[10..13].fill(12.0);
        detail[13..16].fill(-12.0);
        let ten = remap_band_gains(&detail, 10);
        assert!(
            ten.iter().all(|g| (-12.0..=12.0).contains(g)),
            "the shrink invented gain: {ten:?}"
        );
        assert!(ten.iter().any(|g| *g > 1.0) && ten.iter().any(|g| *g < -1.0));
    }

    /// `true` when `gains` never go down from one band to the next.
    fn rises(gains: &[Real]) -> bool {
        gains.windows(2).all(|pair| pair[0] <= pair[1])
    }

    #[test]
    fn a_tilt_with_more_bands_than_the_live_ladder_is_read_along_itself_and_stays_in_its_range() {
        // Audit report #13, second review. A seven-band tilt from 0 to +6 dB over 150 Hz-2 kHz
        // fitted to five bands came back as -2.03, 1.18, 4.39, 7.61 and 6.00 dB: the fit
        // extrapolated past the preset's ends, inventing 2 dB of cut at 62.5 Hz and 1.6 dB of
        // boost at 4 kHz, and bent back down at 16 kHz. Past its ends the tilt is read, not
        // solved for.
        //
        // Changed on purpose: audit report #13 (held ends). The reading past the ends held the
        // end gain, so 4 and 16 kHz both took +6 dB, which the tilt's own response only has at
        // 2 kHz; this was `..._stays_monotonic_and_within_its_own_range`. They now take the taper
        // from 2 kHz over the tilt's own spacing, which has run out an octave up.
        let seven: Vec<Real> = (0..7)
            .map(|band| (150.0 * (2000.0_f64 / 150.0).powf(f64::from(band) / 6.0)) as Real)
            .collect();
        let rising: Vec<Real> = (0..7).map(|band| band as Real).collect();
        let five = fit_preset_gains(&seven, &rising, &ladder(5));
        assert!(five.iter().all(|g| (0.0..=6.0).contains(g)), "{five:?}");
        assert_eq!((five[0], five[3], five[4]), (0.0, 0.0, 0.0), "{five:?}");
        assert!(rises(&five[..3]), "{five:?}");
        for (gain, hz) in five.iter().zip(ladder(5)) {
            assert_close(*gain, reading(&seven, &rising, hz), "read along the tilt");
        }

        // Twelve bands from 0 to +6 dB over 200 Hz-8 kHz on ten: the fit dipped to -0.89 dB at
        // 116 Hz and rose past +6 dB at 8.6 kHz. 8.6 kHz is a quarter of a spacing past 8 kHz.
        let twelve: Vec<Real> = (0..12)
            .map(|band| (200.0 * 40.0_f64.powf(f64::from(band) / 11.0)) as Real)
            .collect();
        let rising: Vec<Real> = (0..12)
            .map(|band| (6.0 * f64::from(band) / 11.0) as Real)
            .collect();
        let ten = fit_preset_gains(&twelve, &rising, &ladder(10));
        assert!(ten.iter().all(|g| (0.0..=6.0).contains(g)), "{ten:?}");
        assert!(rises(&ten[..8]), "{ten:?}");
        assert_eq!((ten[0], ten[1], ten[9]), (0.0, 0.0, 0.0));
        for (gain, hz) in ten.iter().zip(ladder(10)) {
            assert_close(*gain, reading(&twelve, &rising, hz), "read along the tilt");
        }
        assert!((4.5..4.7).contains(&ten[8]), "{ten:?}");
    }

    #[test]
    fn a_live_band_with_preset_bands_on_one_side_only_is_read_not_extrapolated() {
        // Audit report #13, second review. 0 dB at 62.5 Hz, +3 dB at 90 Hz, then nothing but
        // 0 dB from 1 kHz up: on five bands the fit stretched the 90 Hz slope out to 250 Hz, where
        // nothing says what the curve does, and put +11.4 dB there. Now 250 Hz is read, 1.73 dB
        // on the way down from 90 Hz to 1 kHz.
        let centres = [62.5, 90.0, 1000.0, 2000.0, 4000.0, 8000.0, 16_000.0];
        let gains = [0.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let five = fit_preset_gains(&centres, &gains, &ladder(5));
        assert_close(five[1], reading(&centres, &gains, 250.0), "250 Hz");
        assert!((1.72..1.73).contains(&five[1]), "{five:?}");
        assert_eq!([five[0], five[2], five[3], five[4]], [0.0; 4]);
    }

    /// A pseudo-random stream, so the sweep below is the same on every run.
    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1_u64 << 53) as f64
        }

        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + (high - low) * self.next()
        }
    }

    #[test]
    fn a_preset_fitted_to_fewer_live_bands_never_leaves_its_own_gain_range() {
        // Audit report #13, second review. Random gains and straight tilts over random stretches
        // of the spectrum, neither of them a reading of a curve on the live ladder, fitted to the
        // window's ladders: the fit used to take 1643 of these 4000 outside the range they
        // started in, where the live ladder reached past the preset's ends or across a gap in its
        // ladder, the worst by 1780 dB. Now every one comes back inside it.
        let mut random = Xorshift(0x9e37_79b9_7f4a_7c15);
        for case in 0..4_000 {
            let live = ladder([5, 10, 15, 20, 31][case % 5]);
            let count = live.len() + 1 + (random.next() * 12.0) as usize;
            let low = random.between(15.0_f64.ln(), 21_000.0_f64.ln());
            let high = random.between(low, 21_000.0_f64.ln());
            let mut centres: Vec<Real> = (0..count)
                .map(|_| random.between(low, high).exp() as Real)
                .collect();
            centres.sort_by(Real::total_cmp);
            let (from, to) = (random.between(-12.0, 12.0), random.between(-12.0, 12.0));
            let gains: Vec<Real> = if case % 2 == 0 {
                (0..count)
                    .map(|_| random.between(-12.0, 12.0) as Real)
                    .collect()
            } else {
                let span = (f64::from(centres[count - 1]) / f64::from(centres[0]))
                    .ln()
                    .max(1e-9);
                centres
                    .iter()
                    .map(|hz| {
                        let along = (f64::from(*hz) / f64::from(centres[0])).ln() / span;
                        (from + (to - from) * along) as Real
                    })
                    .collect()
            };
            let (lowest, highest) = gains
                .iter()
                .fold((Real::INFINITY, Real::NEG_INFINITY), |(lo, hi), g| {
                    (lo.min(*g), hi.max(*g))
                });
            // Changed on purpose: audit report #13 (held ends). Past the preset's ends its curve
            // tapers to 0 dB rather than holding its end gain, so 0 dB is inside the range too:
            // no gain, not an invented one.
            let (lowest, highest) = (lowest.min(0.0), highest.max(0.0));
            for (band, gain) in fit_preset_gains(&centres, &gains, &live).iter().enumerate() {
                assert!(
                    (lowest - 1e-4..=highest + 1e-4).contains(gain),
                    "case {case}: band {band} is {gain}, outside {lowest}..={highest}"
                );
            }
        }
    }

    #[test]
    fn a_trip_the_larger_ladder_cannot_hold_comes_back_read_and_inside_the_curve() {
        // Audit report #13, second review. Fifteen bands have only thirteen in the range fourteen
        // bands cover, so a fourteen-band curve cannot survive the trip. The fit used to try on
        // the way back and could land outside the curve's range: in this sweep over every pair of
        // counts the engine holds, five pairs did, by up to 0.82 dB. Now a trip that cannot be
        // exact comes back as a plain reading of the larger curve — lossy, since the loss happened
        // on the way up, but inside the range.
        let mut random = Xorshift(0x2545_f491_4f6c_dd1d);
        for small in 2..=SOS_MAX_SECTIONS {
            for large in small + 1..=SOS_MAX_SECTIONS {
                let curve: Vec<Real> = (0..small)
                    .map(|_| random.between(-12.0, 12.0) as Real)
                    .collect();
                let (lowest, highest) = curve
                    .iter()
                    .fold((Real::INFINITY, Real::NEG_INFINITY), |(lo, hi), g| {
                        (lo.min(*g), hi.max(*g))
                    });
                let back = remap_band_gains(&remap_band_gains(&curve, large), small);
                for (band, gain) in back.iter().enumerate() {
                    assert!(
                        (lowest - 1e-4..=highest + 1e-4).contains(gain),
                        "{small} -> {large} -> {small}: band {band} is {gain}, outside \
                         {lowest}..={highest}"
                    );
                }
            }
        }

        let fourteen: Vec<Real> = (0..14)
            .map(|band| ((band * 5 % 7) as Real - 3.0) * 2.5)
            .collect();
        let there = remap_band_gains(&fourteen, 15);
        let back = remap_band_gains(&there, 14);
        let expected: Vec<Real> = ladder(14)
            .iter()
            .map(|hz| reading(&ladder(15), &there, *hz))
            .collect();
        assert_all_close(&back, &expected, 1e-5, "14 -> 15 -> 14 is read");
        assert_ne!(back, fourteen);
    }

    #[test]
    fn a_single_band_is_copied_to_every_new_band() {
        assert_eq!(remap_band_gains(&[4.5], 10), [4.5; 10]);
        assert_eq!(remap_band_gains(&[-7.25], 31), [-7.25; 31]);
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
    fn shrinking_to_a_single_band_reads_the_curve_at_its_centre() {
        // Changed on purpose: audit report #13 (by position, the first band was kept). A lone
        // band sits at 62.5 Hz, the ten-band ladder's first centre, so a ten-band curve still
        // gives its first gain; thirty-one bands give their curve at 62.5 Hz, between the 50 Hz
        // and 63 Hz bands.
        assert_eq!(remap_band_gains(&TEN, 1), [6.0]);
        let one = remap_band_gains(&thirty_one_numbered(), 1);
        assert_close(
            one[0],
            reading(&ladder(31), &thirty_one_numbered(), 62.5),
            "31 -> 1",
        );
        assert!((5.0..6.0).contains(&one[0]), "{one:?}");
    }

    #[test]
    fn every_pair_of_band_counts_tapers_past_the_ends_and_invents_no_gain() {
        // Changed on purpose: audit report #13. By position, the first band always landed on the
        // first and the last on the last, whatever their frequencies; by frequency the ends that
        // count are the ends of the old curve's *range*. Every count the engine can hold, both
        // ways, as a proof that no pair of counts makes the remap panic.
        //
        // Changed on purpose: audit report #13 (held ends). A band past those ends held the end
        // gain, and this was `every_pair_of_band_counts_holds_the_ends_and_invents_no_gain`. It
        // now takes the end gain tapered to 0 dB over the old ladder's end spacing, so 0 dB is
        // inside the range too.
        for old_count in 1..=SOS_MAX_SECTIONS {
            let old: Vec<Real> = (0..old_count)
                .map(|band| ((band * 7 % 11) as Real - 5.0) * 2.0)
                .collect();
            let (low, high) = old.iter().fold((0.0 as Real, 0.0 as Real), |(lo, hi), g| {
                (lo.min(*g), hi.max(*g))
            });
            let from = ladder(old_count);
            for new_count in 0..=SOS_MAX_SECTIONS {
                let remapped = remap_band_gains(&old, new_count);
                assert_eq!(remapped.len(), new_count, "{old_count} -> {new_count}");
                for (band, (gain, hz)) in remapped.iter().zip(ladder(new_count)).enumerate() {
                    assert!(
                        (low..=high).contains(gain),
                        "{old_count} -> {new_count}: band {band} is {gain}, outside {low}..={high}"
                    );
                    let past_an_end = hz <= from[0] || hz >= from[old_count - 1];
                    if old_count != new_count && past_an_end {
                        assert!(
                            (*gain - reading(&from, &old, hz)).abs() < 1e-4,
                            "{old_count} -> {new_count}: {hz} Hz is {gain}, the taper {}",
                            reading(&from, &old, hz)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_flat_curve_stays_flat_and_a_constant_one_stays_constant_over_its_own_range() {
        // Changed on purpose: audit report #13 (held ends). A constant curve stayed constant on
        // every band of any count, including bands past the old ladder's ends, where it was held
        // (this was `..._stays_constant_at_any_count`). Past the ends it now tapers like any
        // other curve: ten bands at +3.5 dB are a curve that falls away below 62.5 Hz, and
        // thirty-one bands down to 20 Hz draw it that way.
        for old_count in 1..=SOS_MAX_SECTIONS {
            let from = ladder(old_count);
            for new_count in 1..=SOS_MAX_SECTIONS {
                assert!(
                    remap_band_gains(&vec![0.0; old_count], new_count)
                        .iter()
                        .all(|g| g.to_bits() == 0.0_f32.to_bits()),
                    "{old_count} -> {new_count}"
                );
                let constant = remap_band_gains(&vec![3.5; old_count], new_count);
                for (gain, hz) in constant.iter().zip(ladder(new_count)) {
                    let inside = hz >= from[0] && hz <= from[old_count - 1];
                    if inside || old_count == 1 || old_count == new_count {
                        assert_eq!(*gain, 3.5, "{old_count} -> {new_count}: {hz} Hz");
                    } else {
                        let tapered = reading(&from, &vec![3.5; old_count], hz);
                        assert!(
                            (*gain - tapered).abs() < 1e-4 && (0.0..3.5).contains(gain),
                            "{old_count} -> {new_count}: {hz} Hz is {gain}, tapered {tapered}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_standard_ladders_are_the_tables_and_else_the_ten_band_edges_geometrically() {
        for count in [5, 10, 15, 20, 31] {
            let (table, _, _) = band_table(count).expect("a table");
            assert_eq!(standard_centres(count), table);
        }
        for count in [1, 2, 7, 12, 32] {
            let mut eq = GraphicEq::new();
            eq.set_num_bands(count);
            assert_eq!(
                standard_centres(count),
                eq.center_frequencies(),
                "{count} bands"
            );
        }
        assert!(standard_centres(0).is_empty());
    }

    #[test]
    fn a_ten_band_preset_lands_on_a_thirty_one_band_ladder_by_frequency() {
        // Changed on purpose: audit report #13. The 2 kHz band — index 20 — used to take the
        // ten-band curve's seventh band, +12 dB at 2520 Hz; it now takes the curve at 2 kHz, on
        // the slope up from -12 dB at 1361 Hz.
        let (live, _, _) = band_table(31).expect("the 31-band table");
        let (centres, _, _) = band_table(10).expect("the 10-band table");
        let fitted = fit_preset_gains(centres, &TEN, live);
        assert_eq!(fitted.len(), 31);
        assert_eq!(fitted, remap_band_gains(&TEN, 31));
        assert_close(fitted[20], reading(centres, &TEN, 2000.0), "2 kHz");
        assert!((2.9..3.1).contains(&fitted[20]), "{}", fitted[20]);
    }

    #[test]
    fn a_thirty_one_band_preset_on_a_ten_band_ladder_is_read_at_the_live_centres() {
        let (live, _, _) = band_table(10).expect("the 10-band table");
        let (centres, _, _) = band_table(31).expect("the 31-band table");
        let expected: Vec<Real> = live
            .iter()
            .map(|hz| reading(centres, &thirty_one_numbered(), *hz))
            .collect();
        assert_all_close(
            &fit_preset_gains(centres, &thirty_one_numbered(), live),
            &expected,
            1e-5,
            "31 on 10",
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
    fn a_presets_own_centres_decide_where_each_gain_lands() {
        // Changed on purpose: audit report #13. By position, the same five gains landed
        // identically on any ladder; by frequency a boost at 240 Hz stays at 240 Hz. On the
        // fifteen-band ladder, the low preset's 9 dB sits on the 250 Hz band's doorstep and
        // everything above its last band at 480 Hz holds -1.5 dB; the wide preset's 9 dB is at
        // 4 kHz.
        let (live, _, _) = band_table(15).expect("the 15-band table");
        let low_ladder = [30.0, 60.0, 120.0, 240.0, 480.0];
        let wide_ladder = [62.5, 250.0, 1000.0, 4000.0, 16000.0];
        let low = fit_preset_gains(&low_ladder, &FIVE, live);
        let wide = fit_preset_gains(&wide_ladder, &FIVE, live);
        assert_ne!(low, wide);
        for (band, hz) in live.iter().enumerate() {
            assert_close(low[band], reading(&low_ladder, &FIVE, *hz), "low ladder");
            assert_close(wide[band], reading(&wide_ladder, &FIVE, *hz), "wide ladder");
        }
        // Changed on purpose: audit report #13 (held ends). Past the low ladder's ends its end
        // gains were held — -6 dB at 25 Hz, -1.5 dB on every band from 630 Hz up — and now taper
        // over its octave spacing: a sixth of an octave below 30 Hz keeps most of the -6 dB, and
        // from an octave above 480 Hz the -1.5 dB has run out.
        assert!(
            (-4.5..-4.3).contains(&low[0]),
            "25 Hz, below the low ladder: {}",
            low[0]
        );
        assert!((-1.0..-0.8).contains(&low[7]), "630 Hz: {}", low[7]);
        assert!(low[8..].iter().all(|g| *g == 0.0), "from 1 kHz up: {low:?}");
        assert_eq!(wide[11], 9.0, "4 kHz, on the wide ladder's fourth band");
    }

    #[test]
    fn a_preset_band_needs_both_a_centre_and_a_gain() {
        let (live, _, _) = band_table(10).expect("the 10-band table");
        let (ten_centres, _, _) = band_table(10).expect("the 10-band table");

        // An eleventh gain with no centre is not a band: the preset still has ten, and copies.
        let mut eleven_gains = TEN.to_vec();
        eleven_gains.push(9.0);
        assert_eq!(fit_preset_gains(ten_centres, &eleven_gains, live), TEN);

        // Nine centres make a nine-band preset. Its bands sit exactly on the first nine live
        // ones, and the tenth, at 16 kHz, is past its last. Changed on purpose: audit report #13
        // (held ends): it held the last gain, and now takes it tapered over the nine bands' last
        // spacing, which 16 kHz is one whole spacing past — nothing is left.
        let mut expected = TEN;
        expected[9] = 0.0;
        assert_all_close(
            &fit_preset_gains(&ten_centres[..9], &TEN, live),
            &expected,
            1e-5,
            "nine bands on ten",
        );
    }

    #[test]
    fn a_one_band_preset_fills_the_whole_live_ladder() {
        let (live, _, _) = band_table(31).expect("the 31-band table");
        assert_eq!(fit_preset_gains(&[1000.0], &[-4.0], live), [-4.0; 31]);
    }

    #[test]
    fn a_preset_fitted_to_a_single_live_band_takes_the_curve_at_that_band() {
        // Changed on purpose: audit report #13 (by position, the first band). The live band is at
        // 1 kHz, exactly on the preset's second band.
        assert_eq!(
            fit_preset_gains(&[62.5, 1000.0], &[2.0, 8.0], &[1000.0]),
            [8.0]
        );
        assert!(fit_preset_gains(&[62.5], &[2.0], &[]).is_empty());
    }

    #[test]
    fn a_preset_whose_centres_are_out_of_order_or_out_of_range_is_read_as_it_would_install() {
        // A hand-edited `.fac` can say anything. Centres are clamped to the equalizer's 10 Hz to
        // 21 kHz window and a NaN is taken as its bottom, as installing them would; order does
        // not matter; nothing panics and nothing comes out non-finite.
        let (live, _, _) = band_table(10).expect("the 10-band table");
        let shuffled = fit_preset_gains(&[1000.0, 62.5, 4000.0], &[3.0, -3.0, 6.0], live);
        let sorted = fit_preset_gains(&[62.5, 1000.0, 4000.0], &[-3.0, 3.0, 6.0], live);
        assert_eq!(shuffled, sorted);

        let wild = fit_preset_gains(
            &[Real::NAN, -5.0, 1e9, Real::INFINITY, 500.0],
            &[1.0, 2.0, 3.0, 4.0, 5.0],
            live,
        );
        assert_eq!(wild.len(), 10);
        assert!(wild.iter().all(|g| g.is_finite()), "{wild:?}");
    }

    // --- Crossfaded redesigns (audit report #11) -----------------------------------------------

    fn low_tone(frames: usize, hz: Real, amplitude: Real) -> Vec<Real> {
        low_tone_from(0, frames, hz, amplitude)
    }

    /// [`low_tone`] from frame `start` on, so that two calls join without a jump.
    fn low_tone_from(start: usize, frames: usize, hz: Real, amplitude: Real) -> Vec<Real> {
        (start..start + frames)
            .flat_map(|n| {
                let s = (n as Real * hz * std::f32::consts::TAU / 48_000.0).sin() * amplitude;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_band_taken_to_zero_fades_out_and_is_then_an_exact_bypass() {
        // The section runs on through its 20 ms fade to the bypass, and from there the equalizer
        // is as transparent as one that was never touched.
        let mut eq = GraphicEq::new();
        eq.set_band_boost(0, 6.0);
        let mut bass = low_tone(4_800, 62.5, 0.3);
        eq.process(&mut bass, 2);
        eq.set_band_boost(0, 0.0);
        let mut fading = low_tone(960, 62.5, 0.3);
        eq.process(&mut fading, 2);
        assert_eq!(eq.fading, 0, "the fade did not finish in 20 ms");

        let input = low_tone(4_800, 62.5, 0.3);
        let mut after = input.clone();
        eq.process(&mut after, 2);
        assert_eq!(after, input, "a flat equalizer touched the signal");
    }

    #[test]
    fn a_dragged_band_settles_on_the_output_of_the_curve_it_ends_on() {
        // After a drag the section runs exactly the design the drag ended on, so what it plays
        // comes to equal what an equalizer set to that curve from the start plays: the crossfade
        // leaves nothing behind. "Equal" to within the filter's own rounding: two transposed
        // direct forms at 62.5 Hz that heard different pasts wander a rounding error apart, about
        // 6e-5 RMS on this 0.85 tone, for seconds, faded or not — two equalizers never touched
        // but started 100 frames apart do the same — until their states meet (here after 16 s,
        // from when on they agree bit for bit). In the second after the drag they are still up
        // to 4.3e-2 apart, the last crossfade's and the band's own decay.
        let mut dragged = GraphicEq::new();
        let mut fixed = GraphicEq::new();
        fixed.set_band_boost(0, 9.0);
        let mut position = 0;
        let next = |eq: &mut GraphicEq, frames: usize, start: usize| {
            let mut block: Vec<Real> = (start..start + frames)
                .flat_map(|n| {
                    let s = (n as Real * 62.5 * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
                    [s, s]
                })
                .collect();
            eq.process(&mut block, 2);
            block
        };
        for step in 0..=9 {
            if step > 0 {
                dragged.set_band_boost(0, step as Real);
            }
            next(&mut dragged, 800, position);
            next(&mut fixed, 800, position);
            position += 800;
        }
        // The first second after the drag lets the last crossfade and the decay behind it run.
        next(&mut dragged, 48_000, position);
        next(&mut fixed, 48_000, position);
        position += 48_000;
        let a = next(&mut dragged, 48_000, position);
        let b = next(&mut fixed, 48_000, position);
        let differences: Vec<f64> = a.iter().zip(&b).map(|(x, y)| f64::from(x - y)).collect();
        let largest = differences.iter().fold(0.0_f64, |acc, d| acc.max(d.abs()));
        let rms =
            (differences.iter().map(|d| d * d).sum::<f64>() / differences.len() as f64).sqrt();
        assert!(
            rms < 1e-4 && largest < 5e-4,
            "{rms} RMS, {largest} at most, away from the curve it ended on"
        );
    }

    #[test]
    fn an_equalizer_redesigned_before_every_block_stays_bounded() {
        // Thirty-one bands at the narrowest width, every band given a new gain at random between
        // −20 and +20 dB before every 64-frame block for ten seconds, under noise at 0.1: faster
        // than any crossfade finishes, so every band always has one running and one waiting.
        // Every design is stable and a crossfade only ever mixes two of them, so nothing can
        // grow: the loudest sample is 1.6, where the same curves switched in at once, from rest,
        // reach 1.9.
        let mut eq = GraphicEq::new();
        eq.set_num_bands(31);
        eq.set_q_multiplier(MAX_Q_MULTIPLIER);
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as Real / 16_777_216.0
        };
        let mut warm = vec![0.0; 2 * 64];
        eq.process(&mut warm, 2);
        let mut loudest = 0.0_f32;
        for _ in 0..(10 * 48_000 / 64) {
            for band in 0..31 {
                eq.set_band_boost(band, -20.0 + 40.0 * random());
            }
            let mut block: Vec<Real> = (0..2 * 64).map(|_| 0.1 * (2.0 * random() - 1.0)).collect();
            eq.process(&mut block, 2);
            assert!(block.iter().all(|s| s.is_finite()));
            loudest = block.iter().fold(loudest, |acc, s| acc.max(s.abs()));
        }
        assert!(loudest < 4.0, "the cascade grew to {loudest}");
    }

    // --- A new band count (audit report #11) ---------------------------------------------------

    /// The largest change between two neighbouring samples of the left channel.
    fn largest_left_step(interleaved: &[Real]) -> Real {
        let left: Vec<Real> = interleaved.iter().step_by(2).copied().collect();
        left.windows(2)
            .fold(0.0, |acc, pair| acc.max((pair[1] - pair[0]).abs()))
    }

    #[test]
    fn a_new_band_count_plays_the_old_curve_out_and_is_then_the_new_curve_exactly() {
        // Ten bands with 62.5 Hz at +6 dB, then that curve remapped to 31 bands under a 50 Hz
        // tone at 0.3. The new ladder starts from rest at the change and runs on its own; the old
        // one only adds to the output while it fades. So from the last frame of the 20 ms
        // crossfade on, the equalizer hands back exactly what one given the new curve at the
        // moment of the change does, bit for bit, and it runs the plain cascade again. On the way
        // nothing moves further between two samples than the louder curve's tone does.
        let (ten, _, _) = band_table(10).expect("ten bands");
        let (thirty_one, _, _) = band_table(31).expect("31 bands");
        let mut ten_gains = [0.0; 10];
        ten_gains[0] = 6.0;
        let gains = remap_band_gains(&ten_gains, 31);

        let mut eq = GraphicEq::new();
        eq.set_bands(ten, &ten_gains);
        let mut before = low_tone_from(0, 4_800, 50.0, 0.3);
        eq.process(&mut before, 2);
        eq.set_bands(thirty_one, &gains);
        assert_eq!(eq.outgoing_bands, 10, "the old ladder is not playing out");

        let mut fresh = GraphicEq::new();
        fresh.set_bands(thirty_one, &gains);
        let input = low_tone_from(4_800, 4_800, 50.0, 0.3);
        let mut crossfaded = input.clone();
        eq.process(&mut crossfaded, 2);
        let mut reference = input;
        fresh.process(&mut reference, 2);

        assert_eq!(eq.outgoing_bands, 0, "the crossfade did not end in 20 ms");
        assert_eq!(eq.fading, 0);
        assert_eq!(
            &crossfaded[2 * 959..],
            &reference[2 * 959..],
            "after the crossfade the equalizer is not the new curve"
        );
        let mut joined = before;
        joined.extend_from_slice(&crossfaded);
        let steepest = largest_left_step(&joined[2 * 480..]);
        let louder = reference.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
        let own = louder * std::f32::consts::TAU * 50.0 / 48_000.0;
        assert!(
            steepest < own * 1.1,
            "a step of {steepest} where the tone moves {own}"
        );
    }

    #[test]
    fn a_band_count_asked_for_mid_crossfade_moves_the_new_ladder_there_section_by_section() {
        // Ten bands to 31, and to ten again 10 ms later with the curve moved, while the old ten
        // still fade out: the newest ladder cannot take the old bank, so the 31 new sections move
        // to it one by one, the 21 past the tenth fading out to the bypass. Nothing steps on the
        // way, and once every fade is over the equalizer runs ten bands on the plain cascade, with
        // the newest curve.
        let (ten, _, _) = band_table(10).expect("ten bands");
        let (thirty_one, _, _) = band_table(31).expect("31 bands");
        let mut first = [3.0; 10];
        first[0] = 6.0;
        let mut last = first;
        last[1] = -6.0;

        let mut eq = GraphicEq::new();
        eq.set_bands(ten, &first);
        let mut rendered = low_tone_from(0, 4_800, 50.0, 0.3);
        eq.process(&mut rendered, 2);
        eq.set_bands(thirty_one, &remap_band_gains(&first, 31));
        let mut block = low_tone_from(4_800, 480, 50.0, 0.3);
        eq.process(&mut block, 2);
        rendered.extend_from_slice(&block);
        eq.set_bands(ten, &last);
        assert_eq!(
            eq.span(),
            31,
            "the sections past the tenth are not fading out"
        );

        let mut block = low_tone_from(5_280, 4_800, 50.0, 0.3);
        eq.process(&mut block, 2);
        rendered.extend_from_slice(&block);
        assert_eq!((eq.fading, eq.outgoing_bands, eq.span()), (0, 0, 10));

        let mut fresh = GraphicEq::new();
        fresh.set_bands(ten, &last);
        for hz in [31.0, 62.5, 115.0, 1_000.0, 10_000.0] {
            assert_eq!(eq.response_db(hz), fresh.response_db(hz), "{hz} Hz");
        }
        let steepest = largest_left_step(&rendered[2 * 480..]);
        assert!(steepest < 0.01, "a step of {steepest}");
    }

    #[test]
    fn an_equalizer_left_out_lands_its_crossfades_and_takes_the_next_change_at_once() {
        // `sit_out`: a block went by without the equalizer. Whatever crossfade was running or
        // waiting lands, a ladder being replaced stops playing, and until audio goes through
        // again a change lands at once, so the curve that plays when the equalizer comes back is
        // the one that was set.
        let mut eq = GraphicEq::new();
        eq.set_band_boost(0, 12.0);
        eq.process(&mut low_tone(4_800, 62.5, 0.3), 2);
        eq.set_band_boost(0, 0.0);
        eq.set_band_boost(1, 6.0);
        assert_ne!(eq.fading, 0, "the fixture is not fading");
        eq.sit_out();
        assert_eq!(eq.fading, 0);
        assert!(!eq.sections[0].is_active());
        assert!(eq.sections[1].is_active() && !eq.sections[1].is_fading());
        eq.set_band_boost(2, -6.0);
        assert_eq!(eq.fading, 0, "a change while left out did not land at once");

        eq.process(&mut low_tone(4_800, 62.5, 0.3), 2);
        let (thirty_one, _, _) = band_table(31).expect("31 bands");
        eq.set_bands(thirty_one, &[3.0; 31]);
        assert_ne!(
            eq.outgoing_bands, 0,
            "the fixture is not replacing its ladder"
        );
        eq.sit_out();
        assert_eq!(eq.outgoing_bands, 0);
        assert!(!eq.ladder_fade.is_gliding());
    }
}
