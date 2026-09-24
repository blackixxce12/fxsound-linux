//! Volume levelling.
//!
//! [`VolumeLeveller`] is a port of `applyVolumeLeveling`
//! (`dsp/ptutil/SOS/SosProcess.cpp:139-492`), with its constant table at
//! `dsp/ptutil/SOS/SosProcess.cpp:38-76`. It is by some distance the most elaborate block in the
//! original tree: a 120 Hz side-chain detector, a three-band tonality estimate, a six-step RMS
//! ring with a gradient predictor and a mis-prediction detector, a 60-second headroom integrator,
//! and a 30-second window of one-second peak buckets driving a retained "quiet gain floor".
//!
//! Two properties of the original are worth stating up front, because they are surprising and both
//! are deliberate here:
//!
//! * **The stage is boost-only in its target.** `desired_gain` is floored at `quiet_gain_floor`
//!   (`SosProcess.cpp:333`), and that floor is itself floored at 1.0 (`:211`, `:458-461`). The only
//!   thing that ever pulls the gain below unity is the peak safety, `ceiling / peak` (`:358-365`).
//!   So a loud programme is *held* at unity rather than turned down towards the RMS target.
//! * **The gain is ramped linearly across each step** from the previous step's value to the new one
//!   (`:375-379`), so the stage is sample-accurate inside a block.
//!
//! # Where the port stops copying
//!
//! Five things the original does were defects, not voicing, and the port fixes them. Each is an
//! item in the 0.4.0 audit of copied Windows defects, and each changes what a listener hears only
//! while levelling is on:
//!
//! * **#1 — the peak safety reads the unfiltered signal.** The original takes the peak that sets
//!   `ceiling / peak` from the 120 Hz high-passed side chain, which sees a 50 Hz tone at 0.385 of
//!   its level. So bass — and any bass an equalizer band lifted — was boosted past full scale and
//!   then hard-clipped: a 50 Hz tone at 0.3 went up by x4.18 and lost 42 % of its samples to the
//!   clip. The side-chain peak still drives the quiet statistics it was designed for; the safety
//!   reads the unfiltered peak of every channel the gain reaches, and the hard clip is kept as a
//!   last guard that only rounding reaches.
//! * **#2 — the state machine steps every 10 ms, not every buffer.** Every smoothing constant in the
//!   table is per step, and the original took one step per buffer, tuned against WASAPI's 10 ms
//!   ones. Handed PipeWire's quantum instead, the attack's time constant was 51 ms at 256 frames
//!   and 405 ms at 2048 rather than 95 ms, and it changed mid-track when a game or a call shrank
//!   the quantum. The stage now keeps its own clock of [`SUB_BLOCK_SECONDS`] steps whatever the
//!   quantum and the rate, carrying a step across calls when the quantum is shorter or not a
//!   multiple of it, so the power ring spans [`POWER_HISTORY_SECONDS`] and every rate is a rate per
//!   10 ms. At 480 frames and 48 kHz each call is exactly one step and the arithmetic is the
//!   original's, sample for sample, wherever #1 and #4 do not apply.
//! * **#3 — the subwoofer is levelled with the rest.** The surround path used to leave the LFE at
//!   x1 while a quiet scene was lifted by 10 to 20 dB, so the bass fell behind by exactly that
//!   much. It is still left out of the statistics.
//! * **#4 — the gain no longer falls in one sample at the start of a step.** The original clamps
//!   *both* ends of its ramp to the peak-safe gain, so a drum hit late in a buffer pulled the gain
//!   down in one sample — 12.9 dB after a quiet intro — up to 10 ms early: a click. The port leaves
//!   the start of the ramp alone, and where the peak safety bites the gain falls over at most
//!   [`PEAK_RAMP_SECONDS`], arriving on the first sample that would otherwise cross the ceiling.
//!   The fade is searched for across the whole call, not step by step, so a transient just past a
//!   step boundary that falls inside a call is faded into from the step before. (The original's
//!   other clamp on the start, to the gain cap, moves it by a fraction of a decibel at most and is
//!   kept.)
//!
//!   **What it cannot do: a transient in the first 2 ms of a call.** The audio before a call has
//!   already been played when the call arrives, so a transient `n` frames into it is faded over
//!   those `n` frames only, and one on a call's first frame still falls the whole way between one
//!   sample and the next — 12.2 dB after that quiet intro, near enough the original's drop, if
//!   only on the hit rather than before it. Doing better
//!   would take 2 ms of look-ahead latency on the whole chain, which the stage does not add. At
//!   48 kHz that is the first 96 frames of every call: about one hit in eleven at a 1024-frame
//!   quantum, one in five at 480, and every hit at 96 frames or less, where the fade is cut short
//!   by the call (at 64 frames, a hit 8 frames into its call still falls 2.8 dB in its last step).
//! * **#5 — the detector filters do not idle in subnormals.** In digital silence the side-chain
//!   high-pass and the tone splitters decay into the subnormal range and stay there, because a
//!   one-pole's step rounds to nothing before the state reaches zero. Their state is flushed to zero
//!   below [`DENORMAL_FLUSH`].
//! * **#11 — switching the stage off lets the gain down over 20 ms.** The original clears the
//!   state machine the moment the amount reaches 0 (`SosSet.cpp:281-340`) and the next buffer goes
//!   through untouched, so a quiet passage lifted by 13 dB fell by 13 dB between one sample and
//!   the next: a click, as a Master Gain step is. The gain being played now glides back to unity
//!   over [`crate::smooth::GLIDE_SECONDS`] first; the state machine is cleared at once, as
//!   before, and the buffers after the glide are untouched, as before. A stage its owner has
//!   stopped running (FxSound or the equalizer switched off: [`VolumeLeveller::sit_out`]) is heard
//!   by nobody, so switched off then it lets go at once, as before, and a glide it had started is
//!   dropped: played when the stage came back, it would lift the level for 20 ms that the listener
//!   last heard unlevelled.
//!
//! The original's RMS normaliser, which sat in front of this stage in `sosProcessBuffer`
//! (`SosProcess.cpp:678-724`), is not ported: it runs only while `setNormalization` has moved its
//! target off 1.0, and nothing in the Windows application ever calls `setNormalization` (audit
//! #37).
//!
//! Real-time safety: every piece of state is a fixed-size inline array, so there is nothing to
//! allocate — not in `new`, not ever. The per-step cost is two passes over the samples, a
//! vectorised peak scan and a handful of `sqrt`/`log10` calls — plus, where a step boundary falls
//! inside a call, a peak scan of the 2 ms after it, and a search over at most one step, only while
//! the peak safety is actually cutting in — and the only loops whose trip count is not the buffer
//! length are bounded by [`PEAK_WINDOW_SIZE`]. No branch in here can panic: every
//! index is proved in range by construction, and every divisor is either a compile-time constant
//! or guarded.
//!
//! The parameter is an abstract 0..=4 "amount", not decibels, despite the original's
//! `setVolumeLeveling(float gain_db)` name (`docs/spec/08-dsp-api.md` §8.4,
//! `GraphicEqSet.cpp:76-89`).

use crate::biquad::{MAX_CHANNELS, Real};
use crate::smooth::{Ramp, glide_frames};

// ---------------------------------------------------------------------------------------------
// The constant table, `SosProcess.cpp:38-76`.
// ---------------------------------------------------------------------------------------------

/// Hard clip ceiling. `SosProcess.cpp:38`.
pub const CEILING: Real = 1.0;
/// Gain-down smoothing per step ([`SUB_BLOCK_SECONDS`], 10 ms). `SosProcess.cpp:39`, where it was
/// per buffer (audit #2).
pub const ATTACK_ALPHA: Real = 0.10;
/// Gain-up smoothing when the gap to the target is small. `SosProcess.cpp:40`.
pub const RELEASE_ALPHA_FAST: Real = 0.05;
/// Gain-up smoothing when the gap to the target is large. `SosProcess.cpp:41`.
pub const RELEASE_ALPHA_SLOW: Real = 0.02;
/// Gap/gain ratio that selects slow over fast release. `SosProcess.cpp:42`.
pub const RELEASE_GAP_THRESHOLD: Real = 0.15;
/// How much of the RMS gradient is extrapolated forward. `SosProcess.cpp:43`.
pub const PREDICTION_STRENGTH: Real = 0.35;
/// The prediction may not stray more than ±15 % from the average. `SosProcess.cpp:44`.
pub const PREDICTION_CLAMP: Real = 0.15;
/// A move this much smaller than predicted counts as a miss. `SosProcess.cpp:45`.
pub const PREDICTION_MISS_RATIO: Real = 0.35;
/// Side-chain detector high-pass. `SosProcess.cpp:46`.
pub const SIDECHAIN_HPF_HZ: Real = 120.0;
/// Tonality band split, low. `SosProcess.cpp:47`.
pub const TONE_LOW_HZ: Real = 180.0;
/// Tonality band split, body. `SosProcess.cpp:48`.
pub const TONE_BODY_HZ: Real = 1200.0;
/// Tonality band split, presence. `SosProcess.cpp:49`.
pub const TONE_PRESENCE_HZ: Real = 4500.0;
/// Largest gain drop permitted inside one step ([`SUB_BLOCK_SECONDS`], 10 ms), `10^(-1 dB / 20)`.
/// `SosProcess.cpp:50`.
///
/// The name keeps the original's `kVolumeLevelingMinRatioPerBuffer`: the original took one step
/// per buffer, the port takes one per 10 ms of audio whatever the buffer (audit #2).
///
/// The original spells it `0.8912509381337456f`; every digit past the ninth is lost to `float`, so
/// this is the shortest literal that names the same `f32`.
pub const MIN_RATIO_PER_BUFFER: Real = 0.891_250_9;
/// dB span that maps the tonality measurement onto -1..=1. `SosProcess.cpp:51`.
pub const TONALITY_DB_RANGE: Real = 7.0;
/// One-pole smoothing of the tonality score, per step ([`SUB_BLOCK_SECONDS`], 10 ms).
/// `SosProcess.cpp:52`, where it was per buffer (audit #2).
pub const TONALITY_SMOOTHING: Real = 0.08;
/// Target RMS is raised by this fraction when the programme is fully muffled. `SosProcess.cpp:53`.
pub const MUFFLED_TARGET_BOOST: Real = 0.12;
/// Target RMS is cut by this fraction when the programme is fully bright. `SosProcess.cpp:54`.
pub const CLEAR_TARGET_REDUCTION: Real = 0.18;
/// Ceiling is trimmed by this fraction when the programme is fully bright. `SosProcess.cpp:55`.
pub const CLEAR_CEILING_REDUCTION: Real = 0.08;
/// Time constant of the headroom integrator, in seconds. `SosProcess.cpp:56`.
pub const HEADROOM_TIME_SECONDS: Real = 60.0;
/// Target RMS boost at full "comfortable headroom". `SosProcess.cpp:57`.
pub const HEADROOM_TARGET_BOOST: Real = 0.08;
/// Target RMS reduction at full "ceiling pressure". `SosProcess.cpp:58`.
pub const HEADROOM_TARGET_REDUCTION: Real = 0.14;
/// Headroom fraction above which the programme counts as comfortable. `SosProcess.cpp:59`.
pub const HEADROOM_COMFORT_THRESHOLD: Real = 0.18;
/// Fraction of the ceiling that counts as "touching it". `SosProcess.cpp:60`.
pub const HEADROOM_NEAR_CEILING_THRESHOLD: Real = 0.985;
/// Fraction of samples near the ceiling that counts as pressure. `SosProcess.cpp:61`.
pub const HEADROOM_HIT_THRESHOLD: Real = 0.002;
/// Post-gain RMS below which the output still counts as very quiet. `SosProcess.cpp:62`.
pub const VERY_QUIET_RMS_THRESHOLD: Real = 0.035;
/// Side-chain peak below which there is nothing worth boosting. `SosProcess.cpp:63`.
pub const QUIET_AUDIBLE_PEAK_THRESHOLD: Real = 0.0035;
/// Side-chain peak at which the quiet boost is at full authority. `SosProcess.cpp:64`.
pub const QUIET_FULL_BOOST_PEAK: Real = 0.02;
/// +20 dB, the hard ceiling on the quiet boost. `SosProcess.cpp:65`.
pub const QUIET_MAX_GAIN: Real = 10.0;
/// Release floor while the quiet boost is engaged. `SosProcess.cpp:66`.
pub const QUIET_RELEASE_ALPHA: Real = 0.18;
/// The output must stay quiet this long before the boost arms. `SosProcess.cpp:67`.
pub const QUIET_ACTIVATION_SECONDS: Real = 10.0;
/// …and then ramps in over this long. `SosProcess.cpp:68`.
pub const QUIET_ACTIVATION_RAMP_SECONDS: Real = 2.0;
/// Post-gain RMS above which the retained quiet floor is released. `SosProcess.cpp:69`.
pub const QUIET_FLOOR_RELEASE_RMS_THRESHOLD: Real = 0.06;
/// Rate at which the retained quiet floor is released. `SosProcess.cpp:70`.
pub const QUIET_FLOOR_RELEASE_ALPHA: Real = 0.02;
/// Faster decay of the retained quiet floor during silence. `SosProcess.cpp:71`.
pub const QUIET_FLOOR_SILENCE_DECAY_ALPHA: Real = 0.08;
/// Width of one peak bucket, in seconds. `SosProcess.cpp:72`.
pub const QUIET_PEAK_BUCKET_SECONDS: Real = 1.0;
/// Fraction of the ceiling the rolling peak is steered towards. `SosProcess.cpp:73`.
pub const QUIET_PEAK_TARGET_RATIO: Real = 0.98;
/// Time constant for raising the quiet floor from the rolling peak. `SosProcess.cpp:74`.
pub const QUIET_PEAK_FLOOR_RAISE_TIME_SECONDS: Real = 6.0;
/// `SOS_VOLUME_LEVELING_HISTORY_SIZE` (`u_sos.h:42`) — steps in the RMS power ring, so it spans
/// [`POWER_HISTORY_SECONDS`] (60 ms). The original held one entry per buffer (audit #2).
pub const HISTORY_SIZE: usize = 6;
/// `SOS_VOLUME_LEVELING_PEAK_WINDOW_SIZE` (`u_sos.h:43`) — one-second peak buckets retained.
pub const PEAK_WINDOW_SIZE: usize = 30;

/// The original's `kPi`, declared as a `float` (`SosProcess.cpp:75`).
///
/// Its literal `3.14159265358979323846f` rounds to exactly [`core::f32::consts::PI`], so naming the
/// library constant is a transcription of the original rather than an approximation of it.
const PI: Real = core::f32::consts::PI;

/// Top of the abstract control range. `kVolumeLevelingMaxControlValue`, `GraphicEqSet.cpp:34`.
pub const MAX_AMOUNT: Real = 4.0;
/// Target RMS at `MAX_AMOUNT`. `kVolumeLevelingMaxTargetRms`, `GraphicEqSet.cpp:35`.
pub const MAX_TARGET_RMS: Real = 0.5;

// Unnamed literals in the original, named here so the port reads as arithmetic rather than magic.
/// `SosProcess.cpp:244` — the RMS target may not imply a gain above `target / 0.125` (+18 dB at
/// the maximum target).
const MAX_GAIN_CAP_DIVISOR: Real = 0.125;
/// `SosProcess.cpp:242` — the tonality trim may never pull the ceiling below this.
const MIN_EFFECTIVE_CEILING: Real = 0.92;
/// `SosProcess.cpp:214-215` — weights on the four tonality bands.
const AIR_WEIGHT: Real = 0.75;
const BODY_WEIGHT: Real = 1.15;
const LOW_WEIGHT: Real = 0.85;
/// `SosProcess.cpp:214-215` — keeps the ratio finite when a band is silent.
const TONALITY_EPSILON: Real = 1e-12;
/// `SosProcess.cpp:225` — how hard ceiling pressure revokes upward tonal authority.
const AUTHORITY_REDUCE_WEIGHT: Real = 1.5;
/// `SosProcess.cpp:225` — how much spare headroom restores it.
const AUTHORITY_BOOST_WEIGHT: Real = 0.25;
/// `SosProcess.cpp:229` — downward authority is never fully revoked.
const AUTHORITY_DOWNWARD_FLOOR: Real = 0.35;
/// `SosProcess.cpp:344` — tonality's pull on the release rate, and its clamp.
const RELEASE_MUFFLED_WEIGHT: Real = 0.20;
const RELEASE_CLEAR_WEIGHT: Real = 0.35;
const RELEASE_TONAL_MIN: Real = 0.55;
const RELEASE_TONAL_MAX: Real = 1.20;
/// `SosProcess.cpp:275` — the prediction-miss detector ignores moves smaller than this.
const MISS_THRESHOLD_RATIO: Real = 0.01;
const MISS_THRESHOLD_FLOOR: Real = 1e-5;
/// `SosProcess.cpp:470` — ceiling-hit ratio at which headroom pressure saturates.
const HEADROOM_HIT_SATURATION: Real = 0.02;
/// `SosProcess.cpp:483` — headroom above the comfort threshold at which the boost saturates.
const HEADROOM_COMFORT_SPAN: Real = 0.25;
/// `SosProcess.cpp:440-443`, `:488` — clamps on the two slow integrator rates.
const SLOW_ALPHA_MIN: Real = 0.0005;
const SLOW_ALPHA_MAX: Real = 0.05;
/// `SosProcess.cpp:458` — a floor this close to unity is snapped to unity.
const QUIET_FLOOR_SNAP: Real = 1.0001;
/// `SosProcess.cpp:146`, `:303`, `:307`, `:339`, `:358`, `:437` — the original's "not zero" epsilon.
const TINY: Real = 1e-6;
/// `SosProcess.cpp:163` — anything at or below this is treated as a missing sample rate.
const MIN_PLAUSIBLE_SAMPLE_RATE: Real = 1000.0;
/// `SosProcess.cpp:163` — …and replaced by this.
const FALLBACK_SAMPLE_RATE: Real = 48_000.0;

// ---------------------------------------------------------------------------------------------
// Free functions: the parts of the algorithm worth pinning down on their own.
// ---------------------------------------------------------------------------------------------

/// `clampReal` (`SosProcess.cpp:78-81`), which is `fmax(min, fmin(value, max))`.
///
/// Written out rather than expressed as [`f32::clamp`] because the two disagree in exactly the
/// cases that matter on the audio thread: `clamp` panics when `min > max` and propagates NaN,
/// whereas `fmin`/`fmax` discard a NaN operand. This reproduces the C library's behaviour.
#[must_use]
fn clamp_real(value: Real, min_value: Real, max_value: Real) -> Real {
    let upper = if value < max_value { value } else { max_value };
    if min_value < upper { upper } else { min_value }
}

/// `calcOnePoleAlpha` (`SosProcess.cpp:83-88`): the coefficient of a one-pole low-pass,
/// `y += alpha * (x - y)`, at `cutoff_hz`.
///
/// Computed entirely in `f32`, as the original is, so the coefficients match bit for bit.
#[must_use]
pub fn one_pole_alpha(cutoff_hz: Real, sample_rate: Real) -> Real {
    let dt = 1.0 / sample_rate;
    let rc = 1.0 / (2.0 * PI * cutoff_hz);
    dt / (rc + dt)
}

/// The matching one-pole *high*-pass coefficient, `y = alpha * (y_prev + x - x_prev)`
/// (`SosProcess.cpp:95-97`). Note that it is `rc / (rc + dt)`, not the low-pass's `dt / (rc + dt)`.
#[must_use]
pub fn high_pass_alpha(cutoff_hz: Real, sample_rate: Real) -> Real {
    let dt = 1.0 / sample_rate;
    let rc = 1.0 / (2.0 * PI * cutoff_hz);
    rc / (rc + dt)
}

/// The 0..=4 control amount to the linear RMS target the DSP actually uses
/// (`GraphicEqSet.cpp:83-87`). Out-of-range amounts are clamped, as the original clamps them.
#[must_use]
pub fn target_rms_for_amount(amount: Real) -> Real {
    let control = clamp_real(amount, 0.0, MAX_AMOUNT);
    (control / MAX_AMOUNT) * MAX_TARGET_RMS
}

/// `SosProcess.cpp:213-215`: how bright the programme is, in dB, from the four band energies.
///
/// The ratio is formed in `f32` and only the logarithm is taken in `f64`, which is what the
/// original's `(realtype)(10.0f * log10((double)(...)))` does.
#[must_use]
pub fn tonality_db(low: Real, body: Real, presence: Real, air: Real) -> Real {
    let numerator = presence + air * AIR_WEIGHT + TONALITY_EPSILON;
    let denominator = body * BODY_WEIGHT + low * LOW_WEIGHT + TONALITY_EPSILON;
    (10.0 * f64::from(numerator / denominator).log10()) as Real
}

/// `SosProcess.cpp:216`: -1 is fully muffled, +1 fully bright.
#[must_use]
pub fn tonality_score(tonality_db: Real) -> Real {
    clamp_real(tonality_db / TONALITY_DB_RANGE, -1.0, 1.0)
}

/// `SosProcess.cpp:335-346`: the per-step ([`SUB_BLOCK_SECONDS`]) smoothing coefficient for the
/// gain, where the original took one step per buffer (audit #2).
///
/// Moving *down* is always [`ATTACK_ALPHA`]. Moving *up* picks fast or slow by the size of the gap
/// relative to the gain already applied, is then scaled by the tonality (bright programme releases
/// more slowly, muffled more quickly), and is finally floored by the quiet-boost rate.
#[must_use]
fn gain_alpha(
    desired_gain: Real,
    current_gain: Real,
    guarded_muffled_score: Real,
    guarded_clear_score: Real,
    quiet_boost_score: Real,
) -> Real {
    if desired_gain < current_gain {
        return ATTACK_ALPHA;
    }
    let release_gap = desired_gain - current_gain;
    let release_gap_ratio = release_gap / current_gain.max(TINY);
    let mut alpha = if release_gap_ratio > RELEASE_GAP_THRESHOLD {
        RELEASE_ALPHA_SLOW
    } else {
        RELEASE_ALPHA_FAST
    };
    alpha *= clamp_real(
        1.0 + guarded_muffled_score * RELEASE_MUFFLED_WEIGHT
            - guarded_clear_score * RELEASE_CLEAR_WEIGHT,
        RELEASE_TONAL_MIN,
        RELEASE_TONAL_MAX,
    );
    alpha.max(QUIET_RELEASE_ALPHA * quiet_boost_score)
}

/// `SosProcess.cpp:464-486`: the instantaneous headroom verdict for one buffer.
///
/// Positive means the output is pressing against the ceiling, negative that it has room to spare;
/// the three branches are mutually exclusive and checked in the original's order.
#[must_use]
fn headroom_target_score(
    ceiling_hit_ratio: Real,
    post_gain_peak_abs: Real,
    effective_ceiling: Real,
) -> Real {
    let headroom_ratio = clamp_real(
        (effective_ceiling - post_gain_peak_abs) / effective_ceiling,
        0.0,
        1.0,
    );

    if ceiling_hit_ratio > HEADROOM_HIT_THRESHOLD {
        clamp_real(ceiling_hit_ratio / HEADROOM_HIT_SATURATION, 0.0, 1.0)
    } else if post_gain_peak_abs >= (effective_ceiling * HEADROOM_NEAR_CEILING_THRESHOLD) {
        clamp_real(
            (post_gain_peak_abs / effective_ceiling - HEADROOM_NEAR_CEILING_THRESHOLD)
                / (1.0 - HEADROOM_NEAR_CEILING_THRESHOLD),
            0.0,
            1.0,
        )
    } else if headroom_ratio > HEADROOM_COMFORT_THRESHOLD {
        -clamp_real(
            (headroom_ratio - HEADROOM_COMFORT_THRESHOLD) / HEADROOM_COMFORT_SPAN,
            0.0,
            1.0,
        )
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------------------------
// Where the port departs from the original: the step clock, the peak ramp, the flush.
// ---------------------------------------------------------------------------------------------

/// Length of one step of the state machine, in seconds (audit #2).
///
/// Every smoothing constant in the table above is a coefficient *per step*, and the original took
/// one step per buffer — on Windows, WASAPI's 10 ms shared-mode period, the clock the table was
/// tuned against. PipeWire hands the chain whatever quantum the graph runs at, and changes it when
/// a game or a call joins, so the stage keeps its own clock instead: one step every 10 ms of audio,
/// whatever the quantum and the rate. At 48 kHz that is 480 frames.
pub const SUB_BLOCK_SECONDS: Real = 0.010;

/// What the six-step RMS ring spans: [`HISTORY_SIZE`] steps of [`SUB_BLOCK_SECONDS`], 60 ms.
pub const POWER_HISTORY_SECONDS: Real = 0.060;

/// The longest the peak safety takes to bring the gain down (audit #4): long enough that a 12 dB
/// cut is a fade rather than a click, short enough that it comes just before the transient that
/// needs it rather than a whole step early.
///
/// The fade needs the audio it runs over in hand, and the stage adds no latency, so a transient
/// less than this far into a call is faded over only the frames of the call before it.
pub const PEAK_RAMP_SECONDS: Real = 0.002;

/// Detector filter state below this is flushed to zero after every stretch of audio (audit #5):
/// some 8.5 × 10^17 times the smallest normal `f32`, so it catches every state on its way down
/// long before it could settle in the subnormal range, and 400 dB below full scale, so nothing
/// the detector could measure is lost.
pub const DENORMAL_FLUSH: Real = 1e-20;

/// `seconds` of audio at `sample_rate`, in whole frames. Worked in `f64` so that 10 ms at 48 kHz is
/// exactly 480 and not 479.99998 rounded; saturating, so a non-finite rate cannot wrap.
#[must_use]
fn frames_in(sample_rate: Real, seconds: Real) -> usize {
    (f64::from(sample_rate) * f64::from(seconds)).round() as usize
}

/// Which channels of an interleaved frame do what.
#[derive(Clone, Copy, Debug)]
struct Layout {
    channels: usize,
    /// Channels the detector has state for; the rest are levelled but not analysed.
    detector_channels: usize,
    /// Channels that feed the statistics: the detector's, less the subwoofer.
    analysed_channels: usize,
    lfe_channel: Option<usize>,
}

impl Layout {
    /// Whether `channel` feeds the statistics. Every channel is levelled either way.
    fn is_analysed(&self, channel: usize) -> bool {
        channel < self.detector_channels && Some(channel) != self.lfe_channel
    }
}

/// What the current step has gathered so far.
///
/// A step spans several calls when the quantum is shorter than a step or not a multiple of one, so
/// the sums live on the leveller rather than on the stack. They are accumulated sample by sample in
/// the order the original accumulates a buffer, so a step's statistics come out bit for bit the
/// same however the host happened to slice it.
#[derive(Clone, Copy, Debug, Default)]
struct StepStats {
    /// Frames gathered towards the step.
    frames: usize,
    /// Samples that fed the statistics: frames times the analysed channels.
    analysed_samples: usize,
    sum_squares: Real,
    /// The 120 Hz side chain's peak, which is what the quiet statistics were designed to read.
    sidechain_peak: Real,
    /// The unfiltered peak of every channel the gain reaches, which is what the peak safety must
    /// read (audit #1).
    full_band_peak: Real,
    low_energy: Real,
    body_energy: Real,
    presence_energy: Real,
    air_energy: Real,
    post_gain_sum_squares: Real,
    post_gain_peak_abs: Real,
    ceiling_hit_count: usize,
}

impl StepStats {
    fn is_finite(&self) -> bool {
        self.sum_squares.is_finite()
            && self.sidechain_peak.is_finite()
            && self.full_band_peak.is_finite()
            && self.low_energy.is_finite()
            && self.body_energy.is_finite()
            && self.presence_energy.is_finite()
            && self.air_energy.is_finite()
    }
}

/// The gain trajectory: a straight line from `from` to `to` over `length` frames, of which
/// `elapsed` have been played, and `to` held after that.
#[derive(Clone, Copy, Debug)]
struct GainRamp {
    from: Real,
    to: Real,
    length: usize,
    elapsed: usize,
}

impl GainRamp {
    const fn hold(gain: Real) -> Self {
        Self {
            from: gain,
            to: gain,
            length: 0,
            elapsed: 0,
        }
    }

    /// The gain `offset` frames after the next frame to be played.
    fn at(&self, offset: usize) -> Real {
        let position = self.elapsed.saturating_add(offset);
        if position >= self.length {
            self.to
        } else {
            // `SosProcess.cpp:377-378` term for term, so a step that arrives in one piece is
            // ramped exactly as the original ramps its buffer.
            let t = position as Real / self.length as Real;
            self.from + t * (self.to - self.from)
        }
    }

    fn advance(&mut self, frames: usize) {
        self.elapsed = self.elapsed.saturating_add(frames).min(self.length);
    }
}

/// Where the peak safety has to pull a stretch of audio down (audit #4).
///
/// The original clamps *both* ends of its ramp to `ceiling / peak` (`SosProcess.cpp:358-365`), so a
/// transient anywhere in a buffer pulls the gain down on the buffer's first sample. Here the ramp
/// is left alone up to `start`, falls in a straight line to `safe_gain` at `crossing` — the first
/// frame the unguarded ramp would have carried past the ceiling — and is held at or below
/// `safe_gain` from there on.
///
/// `crossing` may lie past the end of the stretch being levelled: that is a transient early in the
/// next step of the same call, whose fade has to begin in this one. Indices count from the
/// stretch's first frame either way.
#[derive(Clone, Copy, Debug)]
struct PeakGuard {
    start: usize,
    crossing: usize,
    start_gain: Real,
    safe_gain: Real,
}

impl PeakGuard {
    /// The most gain frame `index` may take.
    fn limit(&self, index: usize) -> Real {
        if index >= self.crossing {
            self.safe_gain
        } else if index >= self.start {
            let t = (index - self.start) as Real / (self.crossing - self.start) as Real;
            self.start_gain + t * (self.safe_gain - self.start_gain)
        } else {
            Real::INFINITY
        }
    }
}

/// The largest magnitude in `samples`, ignoring NaN.
///
/// A loop of its own rather than a line in the detector's, and eight independent maxima rather
/// than one, because then it vectorises: the recursions in the detector cannot, and a running
/// maximum threaded through them costs a dependent compare on every sample.
#[must_use]
fn peak_magnitude(samples: &[Real]) -> Real {
    const LANES: usize = 8;
    let (chunks, tail) = samples.as_chunks::<LANES>();
    let mut lanes = [0.0 as Real; LANES];
    for chunk in chunks {
        for (lane, value) in lanes.iter_mut().zip(chunk) {
            let magnitude = value.abs();
            if magnitude > *lane {
                *lane = magnitude;
            }
        }
    }
    let mut peak: Real = 0.0;
    for magnitude in lanes
        .into_iter()
        .chain(tail.iter().map(|value| value.abs()))
    {
        if magnitude > peak {
            peak = magnitude;
        }
    }
    peak
}

/// The first frame of `segment` that `gain` — the gain each frame would be played at — would carry
/// past `ceiling` on any channel.
fn first_crossing(
    segment: &[Real],
    channels: usize,
    gain: impl Fn(usize) -> Real,
    ceiling: Real,
) -> Option<usize> {
    segment
        .chunks_exact(channels)
        .enumerate()
        .find_map(|(index, frame)| {
            let gain = gain(index);
            frame
                .iter()
                .any(|value| (value * gain).abs() > ceiling)
                .then_some(index)
        })
}

/// What a finished step decided, with the values the bookkeeping after the gain pass reads.
#[derive(Clone, Copy, Debug)]
struct Decision {
    /// The gain the step ends on: `volume_leveling_gain`, peak-safe and capped.
    gain_end: Real,
    /// The same before the peak safety had its say: what the smoothing alone asked for, capped.
    smoothed_gain_end: Real,
    /// Whether it was the peak safety that set `gain_end`.
    peak_limited: bool,
    /// The step's `max_gain_cap`, quiet boost included.
    max_gain_cap: Real,
    effective_ceiling: Real,
    nominal_gain_cap: Real,
    quiet_gain_floor: Real,
    quiet_duration_before: Real,
    headroom_reduce_score: Real,
    step_seconds: Real,
}

// ---------------------------------------------------------------------------------------------
// VolumeLeveller
// ---------------------------------------------------------------------------------------------

/// The automatic gain stage, `applyVolumeLeveling` (`SosProcess.cpp:139-492`).
///
/// One instance carries the whole state machine for up to [`MAX_CHANNELS`] channels; the original
/// keeps the same fixed `[8]` arrays on its `sosHdlType` (`u_sos.h:93-95`).
#[derive(Clone, Debug)]
pub struct VolumeLeveller {
    /// The clamped 0..=4 control value, as `GraphicEqSet.cpp:85` caches it.
    amount: Real,
    /// `volume_leveling_target_rms` — 0 means the stage is off.
    target_rms: Real,
    sample_rate: Real,

    /// `volume_leveling_gain`: the gain the last step decided on.
    gain: Real,
    /// The gain actually being played, which follows `gain` a step behind and dips under it
    /// wherever the peak safety cut in.
    ramp: GainRamp,
    /// The ceiling the last step decided on, for audio that arrives before the next decision.
    effective_ceiling: Real,
    /// [`SUB_BLOCK_SECONDS`] at the current rate, in frames.
    step_frames: usize,
    /// [`PEAK_RAMP_SECONDS`] at the current rate, in frames.
    peak_ramp_frames: usize,
    step: StepStats,

    power_history: [Real; HISTORY_SIZE],
    power_sum: Real,
    power_index: usize,
    power_count: usize,
    previous_average_rms: Real,
    previous_predicted_rms: Real,

    /// The sample rate the cached detector coefficients were derived for (`:99-101`).
    alpha_sample_rate: Real,
    sc_hpf_alpha: Real,
    tone_low_alpha: Real,
    tone_body_alpha: Real,
    tone_presence_alpha: Real,

    sc_prev_in: [Real; MAX_CHANNELS],
    sc_prev_out: [Real; MAX_CHANNELS],
    /// Low / body / presence low-pass states, per channel (`u_sos.h:95`).
    tone_lp_state: [[Real; 3]; MAX_CHANNELS],

    /// -1 muffled … +1 clear.
    tonality_score: Real,
    /// +1 ceiling pressure … -1 comfortable headroom.
    headroom_score: Real,
    quiet_duration_seconds: Real,
    quiet_gain_floor: Real,
    quiet_peak_history: [Real; PEAK_WINDOW_SIZE],
    quiet_peak_bucket_max: Real,
    quiet_peak_bucket_seconds: Real,
    quiet_peak_history_index: usize,
    quiet_peak_history_count: usize,

    /// The gain being played when the stage was switched off, gliding back to unity (audit #11).
    /// Unity, and still, whenever the stage is on or has finished letting go.
    release: Ramp,
    /// The ceiling the stage was holding when it was switched off, which the glide keeps to.
    release_ceiling: Real,
    /// The stage has been run since it was built or its owner last left it out
    /// ([`VolumeLeveller::sit_out`]): only then is the gain it holds the one being heard, and
    /// only then does switching it off glide.
    heard: bool,
}

impl VolumeLeveller {
    /// A leveller in its power-on state: amount 0, gain 1.0 (`Sos.cpp:67-86`).
    ///
    /// Nothing is heap-allocated here or later — every buffer the algorithm needs is an inline
    /// array whose size is a compile-time constant, independent of block size and sample rate.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        Self {
            amount: 0.0,
            target_rms: 0.0,
            sample_rate,

            gain: 1.0,
            ramp: GainRamp::hold(1.0),
            effective_ceiling: CEILING,
            step_frames: 1,
            peak_ramp_frames: 0,
            step: StepStats::default(),

            power_history: [0.0; HISTORY_SIZE],
            power_sum: 0.0,
            power_index: 0,
            power_count: 0,
            previous_average_rms: 0.0,
            previous_predicted_rms: 0.0,

            alpha_sample_rate: 0.0,
            sc_hpf_alpha: 0.0,
            tone_low_alpha: 0.0,
            tone_body_alpha: 0.0,
            tone_presence_alpha: 0.0,

            sc_prev_in: [0.0; MAX_CHANNELS],
            sc_prev_out: [0.0; MAX_CHANNELS],
            tone_lp_state: [[0.0; 3]; MAX_CHANNELS],

            tonality_score: 0.0,
            headroom_score: 0.0,
            quiet_duration_seconds: 0.0,
            quiet_gain_floor: 1.0,
            quiet_peak_history: [0.0; PEAK_WINDOW_SIZE],
            quiet_peak_bucket_max: 0.0,
            quiet_peak_bucket_seconds: 0.0,
            quiet_peak_history_index: 0,
            quiet_peak_history_count: 0,

            release: Ramp::new(1.0),
            release_ceiling: CEILING,
            heard: false,
        }
    }

    /// The original takes the rate as an argument to every buffer; here it is set once.
    ///
    /// The detector coefficients and the step length are not recomputed until the next
    /// [`process`](Self::process), and then only if the rate actually changed
    /// (`SosProcess.cpp:90-103`).
    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        self.sample_rate = sample_rate;
    }

    /// The abstract 0..=4 control amount; 0 disables the stage and resets its state machine.
    ///
    /// `GraphicEqSetVolumeLeveling` (`GraphicEqSet.cpp:76-89`) clamps and scales, and
    /// `sosSetVolumeLeveling` (`SosSet.cpp:281-340`) clears the whole state machine when the
    /// resulting target is zero.
    ///
    /// The gain being played is not dropped with it (audit #11): it glides back to unity over
    /// [`crate::smooth::GLIDE_SECONDS`], and only then is the stage the exact bypass it is at 0.
    /// Switched back on before that, the stage starts from where the glide had got to. A stage
    /// that is not being heard ([`VolumeLeveller::sit_out`]) has no gain being played to let
    /// down, and is cleared at once, as the original clears it.
    pub fn set_amount(&mut self, amount: Real) {
        let was_enabled = self.is_enabled();
        self.amount = clamp_real(amount, 0.0, MAX_AMOUNT);
        self.target_rms = target_rms_for_amount(self.amount);
        if self.target_rms <= 0.0 {
            if was_enabled && !self.heard {
                self.reset();
            } else if was_enabled {
                let playing = self.ramp.at(0);
                let ceiling = self.effective_ceiling;
                self.reset();
                self.release_ceiling = ceiling;
                self.release = Ramp::new(playing);
                self.release.glide_to(1.0, glide_frames(self.sample_rate));
            }
        } else if self.release.is_gliding() {
            let playing = self.release.value();
            self.release = Ramp::new(1.0);
            self.gain = playing;
            self.ramp = GainRamp::hold(playing);
        }
    }

    /// The clamped 0..=4 control amount currently in force.
    #[must_use]
    pub const fn amount(&self) -> Real {
        self.amount
    }

    /// Whether the stage does anything. False at amount 0, which is the shipping default
    /// (`fxsound/Source/GUI/FxController.h:48`).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.target_rms > 0.0
    }

    /// The gain the last step decided on. Useful for metering; not part of the original API.
    #[must_use]
    pub const fn gain(&self) -> Real {
        self.gain
    }

    /// Tell the stage its owner ran a block without it: FxSound or the equalizer's block is
    /// switched off, and the stage's state stands still until it is run again, as the original's
    /// does (`dfxpProcessReal.cpp:143-157`).
    ///
    /// Only [`VolumeLeveller::process`] plays the glide that lets the gain down after the stage is
    /// switched off, so a glide started while the stage is left out would wait for it to come
    /// back and then lift 20 ms of audio the listener last heard unlevelled: at Volume Leveling 4
    /// on a −34 dBFS tone, set to 0 with FxSound off, switching back on played 0.087 for a
    /// steady 0.019, a 13 dB burst. So a stage left out drops any glide it had, and until it is run
    /// again, switching it off clears it at once, as before audit #11.
    pub fn sit_out(&mut self) {
        self.release = Ramp::new(1.0);
        self.heard = false;
    }

    /// Clear the state machine without changing the amount (`SosSet.cpp:292-320`).
    ///
    /// The cached detector coefficients are cleared too, so the next buffer redesigns them, and a
    /// step half gathered is dropped.
    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.ramp = GainRamp::hold(1.0);
        self.effective_ceiling = CEILING;
        self.step = StepStats::default();

        self.power_history = [0.0; HISTORY_SIZE];
        self.power_sum = 0.0;
        self.power_index = 0;
        self.power_count = 0;
        self.previous_average_rms = 0.0;
        self.previous_predicted_rms = 0.0;

        self.alpha_sample_rate = 0.0;
        self.sc_hpf_alpha = 0.0;
        self.tone_low_alpha = 0.0;
        self.tone_body_alpha = 0.0;
        self.tone_presence_alpha = 0.0;

        self.sc_prev_in = [0.0; MAX_CHANNELS];
        self.sc_prev_out = [0.0; MAX_CHANNELS];
        self.tone_lp_state = [[0.0; 3]; MAX_CHANNELS];

        self.tonality_score = 0.0;
        self.headroom_score = 0.0;
        self.quiet_duration_seconds = 0.0;
        self.quiet_gain_floor = 1.0;
        self.quiet_peak_history = [0.0; PEAK_WINDOW_SIZE];
        self.quiet_peak_bucket_max = 0.0;
        self.quiet_peak_bucket_seconds = 0.0;
        self.quiet_peak_history_index = 0;
        self.quiet_peak_history_count = 0;

        self.release = Ramp::new(1.0);
    }

    /// Level one interleaved buffer in place.
    ///
    /// RT-safe: no allocation, no locks, no panicking path. At amount 0 the buffer is not touched
    /// at all — the stage is an exact bypass, not a unity multiply (`SosProcess.cpp:146-150`).
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        self.process_with_lfe(buffer, channels, None);
    }

    /// As [`process`](Self::process), naming the subwoofer channel so that it is left out of the
    /// statistics.
    ///
    /// The surround path names channel 3 in the original (`SosProcess.cpp:908`): the LFE carries a
    /// deliberately enormous share of a film's energy, and letting it into the level analysis would
    /// pull the gain down on bass-heavy material for reasons that have nothing to do with how loud
    /// the programme is. The original also left it out of the *gain*, so a quiet scene lifted by
    /// 10 to 20 dB lost its subwoofer by exactly that much (audit #3). Here the LFE is levelled
    /// with every other channel and counts towards the peak safety, because the ceiling applies to
    /// it like any other; it is only kept out of the RMS, tonality and post-gain statistics.
    ///
    /// The buffer is worked through in steps of [`SUB_BLOCK_SECONDS`] (audit #2). A step that the
    /// buffer ends in the middle of is carried to the next call: the audio in it is levelled at the
    /// gain already in force — pulled down by the peak safety wherever it has to be — and the
    /// decision is taken once the step is whole, on exactly the statistics an undivided step would
    /// have given. It is taken before that last part of the step is levelled, so the ramp to the
    /// new gain starts on the first frame of the part of the call that completes the step — the
    /// step's own first frame when the step lies inside one call, otherwise up to one quantum
    /// before the step completes (at a 256-frame quantum and 48 kHz, frame 256 of the 480) — and
    /// runs a full step from there, into the next call if need be.
    pub fn process_with_lfe(
        &mut self,
        buffer: &mut [Real],
        channels: usize,
        lfe_channel: Option<usize>,
    ) {
        if channels > 0 && !buffer.is_empty() {
            self.heard = true;
        }
        if self.target_rms <= 0.0 || channels == 0 {
            if self.release.is_gliding() && channels > 0 {
                self.let_go(buffer, channels);
            }
            return;
        }
        let frames = buffer.len() / channels;
        if frames == 0 {
            // The original resets the gain to unity on an empty buffer (`:146-150`). With a step
            // that can span calls an empty one carries no audio at all, so it changes nothing.
            return;
        }

        // The original indexes fixed `[8]` arrays with the raw channel number, so a ninth channel
        // would be a buffer overrun. Here the detector simply ignores anything past
        // `MAX_CHANNELS`; the gain is still applied to those channels so they stay in step with
        // the rest of the mix. For every format FxSound actually handles (mono, stereo, 5.1, 7.1)
        // this is bit-identical to the original.
        let detector_channels = channels.min(MAX_CHANNELS);
        let analysed_channels = (0..detector_channels)
            .filter(|c| Some(*c) != lfe_channel)
            .count();
        if analysed_channels == 0 {
            // `SosProcess.cpp:153-154` returns here *without* resetting the gain.
            return;
        }
        let layout = Layout {
            channels,
            detector_channels,
            analysed_channels,
            lfe_channel,
        };

        let effective_sample_rate =
            if self.sample_rate > MIN_PLAUSIBLE_SAMPLE_RATE && self.sample_rate.is_finite() {
                self.sample_rate
            } else {
                FALLBACK_SAMPLE_RATE
            };
        self.update_coefficients(effective_sample_rate);

        // A ragged tail — a buffer that is not a whole number of frames — is left untouched.
        let Some(mut rest) = buffer.get_mut(..frames * channels) else {
            return;
        };
        while !rest.is_empty() {
            let frames_left = rest.len() / channels;
            let length = self
                .step_frames
                .saturating_sub(self.step.frames)
                .max(1)
                .min(frames_left);
            let Some((segment, tail)) = rest.split_at_mut_checked(length * channels) else {
                return;
            };
            // The rest of the call is still the input, untouched: the peak safety reads the first
            // 2 ms of it, so that a transient just past a step boundary is faded into from this
            // side of it (audit #4).
            if !self.process_segment(segment, tail, &layout, effective_sample_rate) {
                return;
            }
            rest = tail;
        }
    }

    /// The glide back to unity after the stage is switched off, on every channel the gain reached,
    /// under the ceiling the stage was holding.
    fn let_go(&mut self, buffer: &mut [Real], channels: usize) {
        let ceiling = self.release_ceiling;
        for frame in buffer.chunks_exact_mut(channels) {
            let gain = self.release.advance();
            for value in frame.iter_mut() {
                // Compared rather than `clamp`ed, which panics on a NaN bound.
                *value *= gain;
                if *value > ceiling {
                    *value = ceiling;
                } else if *value < -ceiling {
                    *value = -ceiling;
                }
            }
        }
    }

    /// Level one stretch of audio that lies inside a single step, taking the step's decision if
    /// the stretch completes it. `ahead` is the rest of the call, not yet levelled. Returns false
    /// if the stage had to reset itself.
    fn process_segment(
        &mut self,
        segment: &mut [Real],
        ahead: &[Real],
        layout: &Layout,
        effective_sample_rate: Real,
    ) -> bool {
        let frames = segment.len() / layout.channels;
        let segment_peak = self.gather(segment, layout);
        self.flush_detector_denormals();

        // The side-chain high-pass and the tone filters are recursions with no escape: once a
        // non-finite value reaches `sc_prev_out` or `tone_lp_state` it stays there, the peak is
        // `+inf` on every later step, and the peak-safety division then drives the gain to exactly
        // zero — permanent digital silence, for the rest of the session, that the engine's own
        // output check cannot catch because zero is perfectly finite. `Engine::process` sanitises
        // the audio this stage is handed, so what is guarded here is the stage's own arithmetic.
        // Re-arming costs the rest of the buffer, which passes through untouched.
        if !self.step.is_finite() {
            self.reset();
            return false;
        }
        self.step.frames += frames;
        self.step.analysed_samples += frames * layout.analysed_channels;

        let decision = if self.step.frames >= self.step_frames {
            Some(self.decide(effective_sample_rate))
        } else {
            None
        };
        if let Some(decision) = &decision {
            // The ramp starts from wherever the gain being played has got to — at 480 frames and
            // 48 kHz, the previous step's decision, as in the original — and is not clamped to the
            // peak-safe gain at the step's first sample (audit #4). Where it is the peak safety
            // that brings the gain down, the ramp does not start falling at the top of the step
            // either: it keeps to what the smoothing asked for, never rising, and [`PeakGuard`]
            // takes it down to the peak-safe gain in the last [`PEAK_RAMP_SECONDS`] before the
            // first sample that needs it — from the step before, when that sample is early in
            // this step and the step before is in the same call. A sustained note already held at
            // its peak-safe gain starts the step there, so it stays flat, as it does in the
            // original.
            //
            // The original's other clamp on the start, to `max_gain_cap` (`:367`), is kept. The
            // cap drifts by a fraction of a decibel a step as the tonality and headroom trims move,
            // and a gain riding it — the usual state on quiet material — is where most listening
            // happens; keeping the clamp keeps that state the original's sample for sample.
            let from = clamp_real(self.ramp.at(0), 0.0, decision.max_gain_cap);
            let to = if decision.peak_limited && from > decision.gain_end {
                decision.smoothed_gain_end.min(from)
            } else {
                decision.gain_end
            };
            self.ramp = GainRamp {
                from,
                to,
                length: self.step_frames,
                elapsed: 0,
            };
            self.gain = decision.gain_end;
            self.effective_ceiling = decision.effective_ceiling;
        }

        self.apply_gain(segment, ahead, layout, segment_peak);

        if let Some(decision) = decision {
            self.finish_step(&decision);
            self.step = StepStats::default();
        }
        true
    }

    /// Pass 1: the side-chain detector and the tonality split (`SosProcess.cpp:166-203`), folded
    /// into the current step. Returns the stretch's own unfiltered peak across every channel.
    fn gather(&mut self, segment: &[Real], layout: &Layout) -> Real {
        // The peak safety reads what the gain will actually be applied to: every channel,
        // unfiltered (audit #1).
        let segment_peak = peak_magnitude(segment);

        let mut stats = self.step;
        for frame in segment.chunks_exact(layout.channels) {
            for (channel, &value) in frame.iter().enumerate().take(layout.detector_channels) {
                if Some(channel) == layout.lfe_channel {
                    continue;
                }

                let sc_prev_in = self.sc_prev_in[channel];
                let sc_prev_out = self.sc_prev_out[channel];
                let sc_value = self.sc_hpf_alpha * (sc_prev_out + value - sc_prev_in);
                self.sc_prev_in[channel] = value;
                self.sc_prev_out[channel] = sc_value;

                stats.sum_squares += sc_value * sc_value;

                let abs_value = sc_value.abs();
                if abs_value > stats.sidechain_peak {
                    stats.sidechain_peak = abs_value;
                }

                let tone_state = &mut self.tone_lp_state[channel];
                tone_state[0] += self.tone_low_alpha * (value - tone_state[0]);
                tone_state[1] += self.tone_body_alpha * (value - tone_state[1]);
                tone_state[2] += self.tone_presence_alpha * (value - tone_state[2]);

                let low_band = tone_state[0];
                let body_band = tone_state[1] - tone_state[0];
                let presence_band = tone_state[2] - tone_state[1];
                let air_band = value - tone_state[2];

                stats.low_energy += low_band * low_band;
                stats.body_energy += body_band * body_band;
                stats.presence_energy += presence_band * presence_band;
                stats.air_energy += air_band * air_band;
            }
        }

        // `max` discards a NaN, so a NaN sample on a channel the statistics skip would slip past
        // the finiteness check; an infinite one is caught here like any other.
        if segment_peak > stats.full_band_peak {
            stats.full_band_peak = segment_peak;
        }
        self.step = stats;
        segment_peak
    }

    /// Flush the detector's filter state to zero once it has decayed below [`DENORMAL_FLUSH`]
    /// (audit #5).
    ///
    /// In digital silence a one-pole's state decays towards zero but never arrives: once it is
    /// subnormal, `alpha * state` rounds to nothing and the state stays where it is, and every
    /// sample after that does its arithmetic on a subnormal — tens of times slower on many x86
    /// parts, on the real-time thread, for as long as the silence lasts. Checked after each
    /// stretch rather than each sample: a fast splitter can still pass through the subnormals in
    /// the stretch where the silence begins, but it is zero from the next one on, and a flush per
    /// stretch costs forty compares rather than forty per sample.
    fn flush_detector_denormals(&mut self) {
        for state in self
            .sc_prev_in
            .iter_mut()
            .chain(self.sc_prev_out.iter_mut())
            .chain(self.tone_lp_state.iter_mut().flatten())
        {
            if state.abs() < DENORMAL_FLUSH {
                *state = 0.0;
            }
        }
    }

    /// The gain decision for a whole step (`SosProcess.cpp:205-369`).
    fn decide(&mut self, effective_sample_rate: Real) -> Decision {
        let stats = self.step;
        let analysed_samples = stats.analysed_samples as Real;
        let peak = stats.sidechain_peak;

        // -- Targets and authorities (`SosProcess.cpp:205-244`). --------------------------------
        let current_power = stats.sum_squares / analysed_samples;
        let current_rms = current_power.sqrt();
        let gain_start = self.gain;
        let mut gain_end = gain_start;
        let headroom_reduce_score = self.headroom_score.max(0.0);
        let headroom_boost_score = (-self.headroom_score).max(0.0);
        let quiet_gain_floor = self.quiet_gain_floor.max(1.0);
        // Read before the state machine advances it at `:410`; `:212` and `:310` see the same
        // value in the original for the same reason.
        let quiet_duration_before = self.quiet_duration_seconds;

        let target_tonality_score = tonality_score(tonality_db(
            stats.low_energy,
            stats.body_energy,
            stats.presence_energy,
            stats.air_energy,
        ));
        self.tonality_score = self.tonality_score * (1.0 - TONALITY_SMOOTHING)
            + target_tonality_score * TONALITY_SMOOTHING;

        let clear_score = self.tonality_score.max(0.0);
        let muffled_score = (-self.tonality_score).max(0.0);
        let step_seconds = stats.frames as Real / effective_sample_rate;
        let tonal_upward_authority = clamp_real(
            1.0 - headroom_reduce_score * AUTHORITY_REDUCE_WEIGHT
                + headroom_boost_score * AUTHORITY_BOOST_WEIGHT,
            0.0,
            1.0,
        );
        let tonal_downward_authority = clamp_real(
            AUTHORITY_DOWNWARD_FLOOR + tonal_upward_authority * (1.0 - AUTHORITY_DOWNWARD_FLOOR),
            AUTHORITY_DOWNWARD_FLOOR,
            1.0,
        );
        let guarded_muffled_score = muffled_score * tonal_upward_authority;
        let guarded_clear_score = clear_score * tonal_downward_authority;
        let effective_target_rms = self.target_rms
            * (1.0 + guarded_muffled_score * MUFFLED_TARGET_BOOST
                - guarded_clear_score * CLEAR_TARGET_REDUCTION
                + headroom_boost_score * HEADROOM_TARGET_BOOST
                - headroom_reduce_score * HEADROOM_TARGET_REDUCTION);
        let effective_ceiling = clamp_real(
            CEILING * (1.0 - guarded_clear_score * CLEAR_CEILING_REDUCTION),
            MIN_EFFECTIVE_CEILING,
            CEILING,
        );
        let nominal_gain_cap = effective_target_rms / MAX_GAIN_CAP_DIVISOR;
        let mut max_gain_cap = nominal_gain_cap.max(quiet_gain_floor);

        // -- The six-step power ring (`SosProcess.cpp:246-265`). --------------------------------
        if self.power_count == HISTORY_SIZE {
            self.power_sum -= self.power_history[self.power_index];
        } else {
            self.power_count += 1;
        }
        self.power_history[self.power_index] = current_power;
        self.power_sum += current_power;
        self.power_index = (self.power_index + 1) % HISTORY_SIZE;

        let averaged_rms = if self.power_count > 0 {
            (self.power_sum / self.power_count as Real).sqrt()
        } else {
            current_rms
        };

        // -- Gradient prediction and its miss detector (`SosProcess.cpp:267-305`). --------------
        let mut predicted_rms = averaged_rms;
        let mut prediction_missed = false;
        if self.previous_average_rms > TINY {
            let previous_average_rms = self.previous_average_rms;
            let predicted_delta = self.previous_predicted_rms - previous_average_rms;
            let actual_delta = averaged_rms - previous_average_rms;
            let miss_threshold =
                (previous_average_rms * MISS_THRESHOLD_RATIO).max(MISS_THRESHOLD_FLOOR);

            if predicted_delta.abs() > miss_threshold {
                let wrong_direction = (predicted_delta * actual_delta) < 0.0;
                let overshot = actual_delta.abs() < (predicted_delta.abs() * PREDICTION_MISS_RATIO);
                if wrong_direction || overshot {
                    prediction_missed = true;
                }
            }
        }

        if self.previous_average_rms > TINY {
            let gradient = averaged_rms - self.previous_average_rms;
            predicted_rms += gradient * PREDICTION_STRENGTH;
            predicted_rms = clamp_real(
                predicted_rms,
                averaged_rms * (1.0 - PREDICTION_CLAMP),
                averaged_rms * (1.0 + PREDICTION_CLAMP),
            );
        }

        if prediction_missed {
            predicted_rms = averaged_rms;
        }

        predicted_rms = predicted_rms.max(TINY);
        self.previous_average_rms = averaged_rms;
        self.previous_predicted_rms = predicted_rms;

        // -- The gain decision (`SosProcess.cpp:307-348`). ---------------------------------------
        if current_rms > TINY {
            let quiet_activation_score = clamp_real(
                (quiet_duration_before - QUIET_ACTIVATION_SECONDS) / QUIET_ACTIVATION_RAMP_SECONDS,
                0.0,
                1.0,
            );
            let quiet_rms_score = clamp_real(
                (VERY_QUIET_RMS_THRESHOLD - predicted_rms) / VERY_QUIET_RMS_THRESHOLD,
                0.0,
                1.0,
            );
            let audible_peak_score = clamp_real(
                (peak - QUIET_AUDIBLE_PEAK_THRESHOLD)
                    / (QUIET_FULL_BOOST_PEAK - QUIET_AUDIBLE_PEAK_THRESHOLD),
                0.0,
                1.0,
            );
            let quiet_boost_score = quiet_rms_score * audible_peak_score * quiet_activation_score;
            if quiet_boost_score > 0.0 {
                let quiet_gain_cap = max_gain_cap.max(QUIET_MAX_GAIN);
                max_gain_cap += (quiet_gain_cap - max_gain_cap) * quiet_boost_score;
            }

            // Note the floor: `quiet_gain_floor` is never below 1.0, so the *target* gain is never
            // an attenuation. Only the peak safety below can pull the gain under unity.
            let mut desired_gain = effective_target_rms / predicted_rms;
            desired_gain = desired_gain.min(max_gain_cap);
            desired_gain = desired_gain.max(quiet_gain_floor);

            let alpha = gain_alpha(
                desired_gain,
                self.gain,
                guarded_muffled_score,
                guarded_clear_score,
                quiet_boost_score,
            );
            gain_end = self.gain * (1.0 - alpha) + desired_gain * alpha;
        }

        // Avoid abrupt "crushed" sound: at most 1 dB of drop per step (`SosProcess.cpp:350-356`).
        if gain_end < gain_start {
            let min_allowed_gain_end = gain_start * MIN_RATIO_PER_BUFFER;
            if gain_end < min_allowed_gain_end {
                gain_end = min_allowed_gain_end;
            }
        }

        // Peak safety (`SosProcess.cpp:358-365`), on the unfiltered peak (audit #1) and on the
        // end of the ramp only: the start is the gain already being played, and where the step's
        // audio needs it lower sooner, [`PeakGuard`] takes it down just before it does (audit #4).
        let smoothed_gain_end = clamp_real(gain_end, 0.0, max_gain_cap);
        let mut peak_limited = false;
        if stats.full_band_peak > TINY {
            let peak_safe_gain = effective_ceiling / stats.full_band_peak;
            if gain_end > peak_safe_gain {
                gain_end = peak_safe_gain;
                peak_limited = true;
            }
        }
        gain_end = clamp_real(gain_end, 0.0, max_gain_cap);

        Decision {
            gain_end,
            smoothed_gain_end,
            peak_limited,
            max_gain_cap,
            effective_ceiling,
            nominal_gain_cap,
            quiet_gain_floor,
            quiet_duration_before,
            headroom_reduce_score,
            step_seconds,
        }
    }

    /// Pass 2: the ramp, and the post-gain statistics (`SosProcess.cpp:371-400`).
    ///
    /// Every channel is levelled, the subwoofer included (audit #3); only the analysed ones are
    /// measured. The hard clip at the ceiling is kept, but as a last guard: with the peak safety
    /// reading the unfiltered signal and pulling the gain down before the first sample that needs
    /// it, all that is left for the clip is the last bit of rounding in `ceiling / peak * peak`.
    ///
    /// `ahead` is the rest of the call, not yet levelled: a transient in its first
    /// [`PEAK_RAMP_SECONDS`] has its fade begin here (audit #4, [`Self::guard_ahead`]).
    fn apply_gain(
        &mut self,
        segment: &mut [Real],
        ahead: &[Real],
        layout: &Layout,
        segment_peak: Real,
    ) {
        let channels = layout.channels;
        let frames = segment.len() / channels;
        let ceiling = self.effective_ceiling;
        let near_ceiling = ceiling * HEADROOM_NEAR_CEILING_THRESHOLD;
        let ramp = self.ramp;

        // The ramp is a straight line, so if neither end of this stretch of it carries the peak
        // past the ceiling no frame does, and the search below — the only per-sample work the
        // guard costs — runs only while the peak safety is actually cutting in.
        let mut guard = None;
        if segment_peak > TINY && frames > 0 {
            let safe_gain = ceiling / segment_peak;
            if ramp.at(0).max(ramp.at(frames - 1)) > safe_gain
                && let Some(crossing) =
                    first_crossing(segment, channels, |index| ramp.at(index), ceiling)
            {
                let start = crossing.saturating_sub(self.peak_ramp_frames);
                guard = Some(PeakGuard {
                    start,
                    crossing,
                    start_gain: ramp.at(start),
                    safe_gain,
                });
            }
        }
        let early = self.guard_ahead(ahead, channels, frames, &ramp, guard.as_ref());
        let guarded = guard.is_some() || early.is_some();
        let limit = |index: usize| {
            let own = guard.map_or(Real::INFINITY, |guard| guard.limit(index));
            let next = early.map_or(Real::INFINITY, |early| early.limit(index));
            own.min(next)
        };

        let mut stats = self.step;
        let mut level = |frame: &mut [Real], gain: Real| {
            for (channel, value) in frame.iter_mut().enumerate() {
                *value *= gain;

                if layout.is_analysed(channel) {
                    let post_gain_abs = value.abs();
                    stats.post_gain_sum_squares += *value * *value;
                    if post_gain_abs > stats.post_gain_peak_abs {
                        stats.post_gain_peak_abs = post_gain_abs;
                    }
                    if post_gain_abs >= near_ceiling {
                        stats.ceiling_hit_count += 1;
                    }
                }

                if *value > ceiling {
                    *value = ceiling;
                } else if *value < -ceiling {
                    *value = -ceiling;
                }
            }
        };
        let frames_iter = segment.chunks_exact_mut(channels).enumerate();
        if guarded {
            frames_iter.for_each(|(index, frame)| {
                level(frame, ramp.at(index).min(limit(index)));
            });
        } else {
            frames_iter.for_each(|(index, frame)| level(frame, ramp.at(index)));
        }
        self.step = stats;

        if guarded {
            // Carry on from where the guards left the gain, not from where the ramp would have
            // been: the next stretch must not jump back up above a peak-safe gain this step has
            // already settled on, nor undo a fade it has begun. What is left of the ramp still
            // runs, but never above either guard's gain. A ramp that has run its course plays its
            // `to`, so when this one has, what carries on is a hold at wherever the fade has got
            // to.
            let mut played = ramp;
            played.advance(frames);
            let hold = |guard: Option<PeakGuard>| guard.map_or(Real::INFINITY, |g| g.safe_gain);
            let from = ramp.at(frames).min(limit(frames));
            let length = played.length - played.elapsed;
            self.ramp = if length == 0 {
                GainRamp::hold(from)
            } else {
                GainRamp {
                    from,
                    to: ramp.to.min(hold(guard)).min(hold(early)),
                    length,
                    elapsed: 0,
                }
            };
        } else {
            self.ramp.advance(frames);
        }
    }

    /// The fade this stretch has to begin for a transient early in the next stretch of the same
    /// call (audit #4).
    ///
    /// Only a stretch that completes its step has audio after it in the call, so what follows is
    /// the next step, or as much of it as the call holds. Left to itself, that stretch could fade
    /// into a transient only from its own first frame, so one within [`PEAK_RAMP_SECONDS`] of the
    /// step boundary would fall most of the way in a sample or two, although the audio before the
    /// boundary is in hand. So the first [`PEAK_RAMP_SECONDS`] of it are searched here, at the most
    /// gain they can be played at:
    ///
    /// * if the next stretch completes a step, that step's ramp starts from whatever gain this
    ///   stretch hands over and ends at or below its own peak-safe gain, which no sample of the
    ///   step can carry past the ceiling, so it is the gain handed over that can; the ceiling is
    ///   the lowest the next decision can set ([`Self::next_ceiling_floor`]);
    /// * if not, the next stretch plays on along this step's ramp, under this step's ceiling.
    ///
    /// The fade aims at the gain the next stretch's own guard will hold it to — the ceiling over
    /// the next stretch's whole peak — so that guard, starting from where this one hands over, runs
    /// on down the same line rather than steepening it. A transient in the first
    /// [`PEAK_RAMP_SECONDS`] of a *call* gets no such lead: the audio before it has already gone.
    fn guard_ahead(
        &self,
        ahead: &[Real],
        channels: usize,
        frames: usize,
        ramp: &GainRamp,
        guard: Option<&PeakGuard>,
    ) -> Option<PeakGuard> {
        let ahead_frames = ahead.len() / channels;
        let next_frames = ahead_frames.min(self.step_frames);
        let window_frames = next_frames.min(self.peak_ramp_frames);
        if window_frames == 0 {
            return None;
        }
        let window = ahead.get(..window_frames * channels)?;
        let next = ahead.get(..next_frames * channels)?;

        let new_step = ahead_frames >= self.step_frames;
        let ceiling = if new_step {
            self.next_ceiling_floor()
        } else {
            self.effective_ceiling
        };
        let hold = guard.map_or(Real::INFINITY, |guard| guard.safe_gain);
        let gain = |offset: usize| {
            let played = if new_step {
                ramp.at(frames)
            } else {
                ramp.at(frames + offset)
            };
            played.min(hold)
        };

        // The gain is a straight line across the window, or a constant, so if neither end of it
        // carries the window's peak past the ceiling no frame does: the search below runs only
        // where the peak safety is about to cut in.
        if gain(0).max(gain(window_frames - 1)) * peak_magnitude(window) <= ceiling {
            return None;
        }
        let offset = first_crossing(window, channels, gain, ceiling)?;
        let crossing = frames + offset;
        let start = crossing.saturating_sub(self.peak_ramp_frames);
        Some(PeakGuard {
            start,
            crossing,
            start_gain: ramp
                .at(start)
                .min(guard.map_or(Real::INFINITY, |guard| guard.limit(start))),
            // Not a division by zero: the crossing sample itself is in `next`.
            safe_gain: ceiling / peak_magnitude(next),
        })
    }

    /// The lowest effective ceiling the next step's decision can set.
    ///
    /// The ceiling comes down only as the programme turns clear, and the tonality it follows moves
    /// [`TONALITY_SMOOTHING`] of the way to its target in a step, so one step can take it no
    /// further towards fully clear than that; the headroom authority that scales it lets at most
    /// all of it through. On muffled material that is the full ceiling. On bright material it is a
    /// few hundredths of a decibel under the ceiling in force while the authority is full, and at
    /// most half a decibel when headroom pressure is holding the authority down, which is all a
    /// fade aimed at it can overshoot by — and the next step's ramp gives that back.
    fn next_ceiling_floor(&self) -> Real {
        let clear = self.tonality_score + TONALITY_SMOOTHING * (1.0 - self.tonality_score);
        clamp_real(
            CEILING * (1.0 - clamp_real(clear, 0.0, 1.0) * CLEAR_CEILING_REDUCTION),
            MIN_EFFECTIVE_CEILING,
            CEILING,
        )
    }

    /// The quiet state machine and the headroom integrator, once a step's audio has been levelled
    /// (`SosProcess.cpp:402-491`).
    fn finish_step(&mut self, decision: &Decision) {
        let stats = self.step;
        let analysed_samples = stats.analysed_samples as Real;
        let peak = stats.sidechain_peak;
        let gain_end = decision.gain_end;
        let effective_ceiling = decision.effective_ceiling;
        let quiet_duration_before = decision.quiet_duration_before;
        let step_seconds = decision.step_seconds;
        let mut quiet_gain_floor = decision.quiet_gain_floor;

        // -- The quiet state machine (`SosProcess.cpp:402-462`). ---------------------------------
        let post_gain_rms = (stats.post_gain_sum_squares / analysed_samples).sqrt();
        self.update_quiet_peak_window(stats.post_gain_peak_abs, step_seconds);
        let rolling_peak_max = self.quiet_peak_window_max();

        let post_gain_still_quiet =
            peak > QUIET_AUDIBLE_PEAK_THRESHOLD && post_gain_rms < VERY_QUIET_RMS_THRESHOLD;
        if post_gain_still_quiet {
            self.quiet_duration_seconds += step_seconds;
        } else {
            self.quiet_duration_seconds = 0.0;
        }

        // The boost "had authority" if it was armed and actually pushed past the nominal cap.
        let quiet_boost_had_authority = quiet_duration_before >= QUIET_ACTIVATION_SECONDS
            && gain_end > decision.nominal_gain_cap;
        if quiet_boost_had_authority && gain_end > quiet_gain_floor {
            quiet_gain_floor = gain_end;
        }

        let quiet_peak_window_ready = self.quiet_peak_history_count == PEAK_WINDOW_SIZE;
        let quiet_floor_is_active =
            quiet_duration_before >= QUIET_ACTIVATION_SECONDS || quiet_gain_floor > 1.0;
        let quiet_peak_target = effective_ceiling * QUIET_PEAK_TARGET_RATIO;
        let sustained_headroom_available = rolling_peak_max > QUIET_AUDIBLE_PEAK_THRESHOLD
            && rolling_peak_max < quiet_peak_target
            && decision.headroom_reduce_score < 0.25;
        if quiet_peak_window_ready && quiet_floor_is_active && sustained_headroom_available {
            let desired_quiet_floor = clamp_real(
                quiet_gain_floor * (quiet_peak_target / rolling_peak_max.max(TINY)),
                quiet_gain_floor,
                QUIET_MAX_GAIN,
            );
            let quiet_floor_raise_alpha = clamp_real(
                step_seconds / QUIET_PEAK_FLOOR_RAISE_TIME_SECONDS,
                SLOW_ALPHA_MIN,
                SLOW_ALPHA_MAX,
            );
            quiet_gain_floor = quiet_gain_floor * (1.0 - quiet_floor_raise_alpha)
                + desired_quiet_floor * quiet_floor_raise_alpha;
        }

        if peak <= QUIET_AUDIBLE_PEAK_THRESHOLD {
            quiet_gain_floor += (1.0 - quiet_gain_floor) * QUIET_FLOOR_SILENCE_DECAY_ALPHA;
        } else if post_gain_rms > QUIET_FLOOR_RELEASE_RMS_THRESHOLD {
            quiet_gain_floor += (1.0 - quiet_gain_floor) * QUIET_FLOOR_RELEASE_ALPHA;
        }

        if quiet_gain_floor < QUIET_FLOOR_SNAP {
            quiet_gain_floor = 1.0;
        }
        self.quiet_gain_floor = quiet_gain_floor;

        // -- The 60-second headroom integrator (`SosProcess.cpp:464-491`). ------------------------
        let ceiling_hit_ratio = stats.ceiling_hit_count as Real / analysed_samples;
        let target_headroom_score = headroom_target_score(
            ceiling_hit_ratio,
            stats.post_gain_peak_abs,
            effective_ceiling,
        );
        let headroom_alpha = clamp_real(
            step_seconds / HEADROOM_TIME_SECONDS,
            SLOW_ALPHA_MIN,
            SLOW_ALPHA_MAX,
        );
        self.headroom_score =
            self.headroom_score * (1.0 - headroom_alpha) + target_headroom_score * headroom_alpha;
    }

    /// `updateVolumeLevelingCoefficients` (`SosProcess.cpp:90-103`): redesign the four detector
    /// one-poles, but only when the sample rate has actually moved — and with them the step and
    /// the peak ramp, which are lengths of time.
    fn update_coefficients(&mut self, sample_rate: Real) {
        if self.alpha_sample_rate == sample_rate {
            return;
        }
        self.sc_hpf_alpha = high_pass_alpha(SIDECHAIN_HPF_HZ, sample_rate);
        self.tone_low_alpha = one_pole_alpha(TONE_LOW_HZ, sample_rate);
        self.tone_body_alpha = one_pole_alpha(TONE_BODY_HZ, sample_rate);
        self.tone_presence_alpha = one_pole_alpha(TONE_PRESENCE_HZ, sample_rate);
        self.step_frames = frames_in(sample_rate, SUB_BLOCK_SECONDS).max(1);
        self.peak_ramp_frames = frames_in(sample_rate, PEAK_RAMP_SECONDS);
        // A step half gathered at the old rate cannot be finished at the new one.
        self.step = StepStats::default();
        self.alpha_sample_rate = sample_rate;
    }

    /// `updateQuietPeakWindow` (`SosProcess.cpp:105-126`): fold this step's post-gain peak into
    /// the current one-second bucket, retiring whole buckets into the ring as they fill.
    ///
    /// The `while` is bounded: a step is [`SUB_BLOCK_SECONDS`] long to within a frame, so it
    /// retires at most one bucket.
    fn update_quiet_peak_window(&mut self, post_gain_peak_abs: Real, step_seconds: Real) {
        self.quiet_peak_bucket_max = self.quiet_peak_bucket_max.max(post_gain_peak_abs);
        self.quiet_peak_bucket_seconds += step_seconds;

        while self.quiet_peak_bucket_seconds >= QUIET_PEAK_BUCKET_SECONDS {
            self.quiet_peak_history[self.quiet_peak_history_index] = self.quiet_peak_bucket_max;
            self.quiet_peak_history_index = (self.quiet_peak_history_index + 1) % PEAK_WINDOW_SIZE;
            if self.quiet_peak_history_count < PEAK_WINDOW_SIZE {
                self.quiet_peak_history_count += 1;
            }
            self.quiet_peak_bucket_seconds -= QUIET_PEAK_BUCKET_SECONDS;
            self.quiet_peak_bucket_max = 0.0;
        }
    }

    /// `getQuietPeakWindowMax` (`SosProcess.cpp:128-137`): the largest peak in the retired buckets
    /// *and* the partial one still filling.
    fn quiet_peak_window_max(&self) -> Real {
        let mut rolling_peak_max = self.quiet_peak_bucket_max;
        for bucket in &self.quiet_peak_history[..self.quiet_peak_history_count] {
            rolling_peak_max = rolling_peak_max.max(*bucket);
        }
        rolling_peak_max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;
    /// 10 ms at 48 kHz, the block size the original was tuned against.
    const BLOCK: usize = 480;

    /// Fills `buffer` with a continuous-phase stereo sine, both channels identical.
    fn fill_sine(buffer: &mut [Real], freq: Real, amp: Real, start_frame: usize) {
        for (i, frame) in buffer.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let n = (start_frame + i) as f64;
            let phase = core::f64::consts::TAU * f64::from(freq) * n / f64::from(FS);
            let value = (f64::from(amp) * phase.sin()) as Real;
            frame[0] = value;
            frame[1] = value;
        }
    }

    /// Runs `buffers` blocks of a steady sine through `leveller`, returning the gain each block
    /// ended on.
    fn run_sine(
        leveller: &mut VolumeLeveller,
        buffers: usize,
        freq: Real,
        amp: Real,
        frame: &mut usize,
    ) -> Vec<Real> {
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        let mut gains = Vec::with_capacity(buffers);
        for _ in 0..buffers {
            fill_sine(&mut buffer, freq, amp, *frame);
            leveller.process(&mut buffer, 2);
            *frame += BLOCK;
            gains.push(leveller.gain());
        }
        gains
    }

    /// The ratio of successive gain increments. For a first-order smoother `g += alpha*(d - g)`
    /// with a steady `d`, this is exactly `1 - alpha` and does not depend on `d` — which is what
    /// lets the trajectory be measured without knowing the target.
    fn convergence_ratio(gains: &[Real]) -> Real {
        assert!(gains.len() >= 3);
        let first = gains[1] - gains[0];
        let second = gains[2] - gains[1];
        second / first
    }

    /// A leveller at full amount, warmed up on a steady tone until every integrator has settled.
    fn warmed_up(freq: Real, amp: Real, buffers: usize) -> (VolumeLeveller, usize) {
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut frame = 0;
        run_sine(&mut leveller, buffers, freq, amp, &mut frame);
        (leveller, frame)
    }

    #[test]
    fn the_constant_table_matches_the_spec() {
        // docs/spec/08-dsp-api.md §8.4, transcribed from SosProcess.cpp:38-76. A slip in any one of
        // these is invisible in a listening test but changes the whole trajectory.
        assert_eq!(CEILING, 1.0);
        assert_eq!(ATTACK_ALPHA, 0.10);
        assert_eq!(RELEASE_ALPHA_FAST, 0.05);
        assert_eq!(RELEASE_ALPHA_SLOW, 0.02);
        assert_eq!(RELEASE_GAP_THRESHOLD, 0.15);
        assert_eq!(PREDICTION_STRENGTH, 0.35);
        assert_eq!(PREDICTION_CLAMP, 0.15);
        assert_eq!(PREDICTION_MISS_RATIO, 0.35);
        assert_eq!(SIDECHAIN_HPF_HZ, 120.0);
        assert_eq!(TONE_LOW_HZ, 180.0);
        assert_eq!(TONE_BODY_HZ, 1200.0);
        assert_eq!(TONE_PRESENCE_HZ, 4500.0);
        assert_eq!(MIN_RATIO_PER_BUFFER, 0.891_250_9);
        assert_eq!(TONALITY_DB_RANGE, 7.0);
        assert_eq!(TONALITY_SMOOTHING, 0.08);
        assert_eq!(MUFFLED_TARGET_BOOST, 0.12);
        assert_eq!(CLEAR_TARGET_REDUCTION, 0.18);
        assert_eq!(CLEAR_CEILING_REDUCTION, 0.08);
        assert_eq!(HEADROOM_TIME_SECONDS, 60.0);
        assert_eq!(HEADROOM_TARGET_BOOST, 0.08);
        assert_eq!(HEADROOM_TARGET_REDUCTION, 0.14);
        assert_eq!(HEADROOM_COMFORT_THRESHOLD, 0.18);
        assert_eq!(HEADROOM_NEAR_CEILING_THRESHOLD, 0.985);
        assert_eq!(HEADROOM_HIT_THRESHOLD, 0.002);
        assert_eq!(VERY_QUIET_RMS_THRESHOLD, 0.035);
        assert_eq!(QUIET_AUDIBLE_PEAK_THRESHOLD, 0.0035);
        assert_eq!(QUIET_FULL_BOOST_PEAK, 0.02);
        assert_eq!(QUIET_MAX_GAIN, 10.0);
        assert_eq!(QUIET_RELEASE_ALPHA, 0.18);
        assert_eq!(QUIET_ACTIVATION_SECONDS, 10.0);
        assert_eq!(QUIET_ACTIVATION_RAMP_SECONDS, 2.0);
        assert_eq!(QUIET_FLOOR_RELEASE_RMS_THRESHOLD, 0.06);
        assert_eq!(QUIET_FLOOR_RELEASE_ALPHA, 0.02);
        assert_eq!(QUIET_FLOOR_SILENCE_DECAY_ALPHA, 0.08);
        assert_eq!(QUIET_PEAK_BUCKET_SECONDS, 1.0);
        assert_eq!(QUIET_PEAK_TARGET_RATIO, 0.98);
        assert_eq!(QUIET_PEAK_FLOOR_RAISE_TIME_SECONDS, 6.0);
        assert_eq!(HISTORY_SIZE, 6);
        assert_eq!(PEAK_WINDOW_SIZE, 30);

        // `MIN_RATIO_PER_BUFFER` is documented as 10^(-1 dB / 20); prove the transcription.
        let one_db_down = (10.0 as Real).powf(-1.0 / 20.0);
        assert!((MIN_RATIO_PER_BUFFER - one_db_down).abs() < 1e-7);
    }

    #[test]
    fn the_amount_is_an_abstract_zero_to_four_control_not_decibels() {
        // GraphicEqSet.cpp:83-87 — four steps of 0.5 across a 0..0.5 linear RMS target.
        for (amount, expected) in [
            (0.0, 0.0),
            (0.5, 0.0625),
            (1.0, 0.125),
            (2.0, 0.25),
            (3.0, 0.375),
            (4.0, 0.5),
        ] {
            assert_eq!(target_rms_for_amount(amount), expected, "amount {amount}");
        }
        // Out of range is clamped, not rejected (GraphicEqSet.cpp:83).
        assert_eq!(target_rms_for_amount(-3.0), 0.0);
        assert_eq!(target_rms_for_amount(9.0), MAX_TARGET_RMS);

        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(7.5);
        assert_eq!(leveller.amount(), MAX_AMOUNT);
        leveller.set_amount(-1.0);
        assert_eq!(leveller.amount(), 0.0);
        assert!(!leveller.is_enabled());
        leveller.set_amount(2.5);
        assert_eq!(leveller.amount(), 2.5);
        assert!(leveller.is_enabled());
    }

    #[test]
    fn the_detector_one_poles_match_their_analytic_coefficients() {
        // SosProcess.cpp:83-88 and :95-97. The low-pass is dt/(rc+dt); the high-pass is rc/(rc+dt),
        // and the two must sum to one at any rate.
        for rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let lp = one_pole_alpha(SIDECHAIN_HPF_HZ, rate);
            let hp = high_pass_alpha(SIDECHAIN_HPF_HZ, rate);
            assert!((lp + hp - 1.0).abs() < 1e-6, "{rate} Hz: {lp} + {hp}");
        }

        // A one-pole low-pass has a -3 dB point at its cutoff; check the 1200 Hz tone splitter
        // against the closed form for `y += a*(x - y)` driven by a sine.
        let alpha = one_pole_alpha(TONE_BODY_HZ, FS);
        let expected = {
            let dt = 1.0 / FS;
            let rc = 1.0 / (2.0 * PI * TONE_BODY_HZ);
            dt / (rc + dt)
        };
        assert_eq!(alpha, expected);
        assert!((0.135..0.14).contains(&alpha), "alpha was {alpha}");

        // The 120 Hz side-chain high-pass is very close to unity at 48 kHz, which is why the
        // detector sees essentially the full signal above a couple of hundred hertz.
        assert!((high_pass_alpha(SIDECHAIN_HPF_HZ, FS) - 0.984_53).abs() < 1e-4);
    }

    #[test]
    fn the_coefficients_are_only_redesigned_when_the_sample_rate_moves() {
        // SosProcess.cpp:90-93 short-circuits on an unchanged rate; the cache must therefore be
        // primed by the first buffer and updated by a rate change.
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        leveller.process(&mut buffer, 2);
        assert_eq!(leveller.alpha_sample_rate, FS);
        assert_eq!(leveller.sc_hpf_alpha, high_pass_alpha(SIDECHAIN_HPF_HZ, FS));

        leveller.set_sample_rate(192_000.0);
        leveller.process(&mut buffer, 2);
        assert_eq!(leveller.alpha_sample_rate, 192_000.0);
        assert_eq!(
            leveller.tone_presence_alpha,
            one_pole_alpha(TONE_PRESENCE_HZ, 192_000.0)
        );

        // An implausible rate falls back to 48 kHz rather than producing infinite coefficients
        // (SosProcess.cpp:163).
        leveller.set_sample_rate(0.0);
        leveller.process(&mut buffer, 2);
        assert_eq!(leveller.alpha_sample_rate, FALLBACK_SAMPLE_RATE);
    }

    #[test]
    fn amount_zero_leaves_every_sample_bit_identical() {
        // SosProcess.cpp:146-150 returns before touching the buffer: the bypass is not a unity
        // multiply, so even denormals and signed zeroes survive unchanged.
        let mut leveller = VolumeLeveller::new(FS);
        assert!(!leveller.is_enabled());

        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        fill_sine(&mut buffer, 1000.0, 0.01, 0);
        buffer[0] = -0.0;
        buffer[1] = Real::MIN_POSITIVE / 4.0;
        buffer[2] = 7.5;
        let original = buffer.clone();

        for _ in 0..16 {
            leveller.process(&mut buffer, 2);
        }
        for (got, want) in buffer.iter().zip(original.iter()) {
            assert_eq!(got.to_bits(), want.to_bits());
        }
        assert_eq!(leveller.gain(), 1.0);
    }

    #[test]
    fn setting_the_amount_to_zero_resets_the_whole_state_machine() {
        // SosSet.cpp:292-320 clears every field when the target reaches zero.
        let (mut leveller, _) = warmed_up(300.0, 0.08, 400);
        assert!(leveller.gain() > 1.5);
        assert_ne!(leveller.tonality_score, 0.0);

        leveller.set_amount(0.0);
        let fresh = VolumeLeveller::new(FS);
        assert_eq!(leveller.gain(), fresh.gain());
        assert_eq!(leveller.tonality_score, fresh.tonality_score);
        assert_eq!(leveller.headroom_score, fresh.headroom_score);
        assert_eq!(leveller.quiet_gain_floor, fresh.quiet_gain_floor);
        assert_eq!(leveller.power_count, fresh.power_count);
        assert_eq!(leveller.power_sum, fresh.power_sum);
        assert_eq!(leveller.previous_average_rms, fresh.previous_average_rms);
        assert_eq!(leveller.alpha_sample_rate, fresh.alpha_sample_rate);
        assert_eq!(leveller.sc_prev_out, fresh.sc_prev_out);
        assert_eq!(leveller.quiet_peak_history_count, 0);
    }

    #[test]
    fn reset_clears_the_state_machine_but_keeps_the_amount() {
        let (mut leveller, _) = warmed_up(300.0, 0.08, 200);
        leveller.reset();
        assert_eq!(leveller.amount(), MAX_AMOUNT);
        assert!(leveller.is_enabled());
        assert_eq!(leveller.gain(), 1.0);
        assert_eq!(leveller.quiet_gain_floor, 1.0);
    }

    #[test]
    fn the_alpha_selector_uses_the_documented_attack_and_release_constants() {
        // SosProcess.cpp:335-346. With a neutral tonality the three branches are the bare
        // constants from the spec table.
        assert_eq!(gain_alpha(1.0, 2.0, 0.0, 0.0, 0.0), ATTACK_ALPHA);
        // Gap ratio 0.10 <= 0.15 selects the fast release…
        assert_eq!(gain_alpha(2.2, 2.0, 0.0, 0.0, 0.0), RELEASE_ALPHA_FAST);
        // …and 0.20 > 0.15 the slow one.
        assert_eq!(gain_alpha(2.4, 2.0, 0.0, 0.0, 0.0), RELEASE_ALPHA_SLOW);
        // Exactly at the threshold the comparison is strict, so fast wins.
        assert_eq!(gain_alpha(2.3, 2.0, 0.0, 0.0, 0.0), RELEASE_ALPHA_FAST);
        // Equal gains take the release branch (`desired >= gain`), not the attack branch.
        assert_eq!(gain_alpha(2.0, 2.0, 0.0, 0.0, 0.0), RELEASE_ALPHA_FAST);

        // Tonality scales the release only, by clamp(1 + 0.20*muffled - 0.35*clear, 0.55, 1.20).
        assert_eq!(
            gain_alpha(2.2, 2.0, 1.0, 0.0, 0.0),
            RELEASE_ALPHA_FAST * 1.20
        );
        assert_eq!(
            gain_alpha(2.4, 2.0, 1.0, 0.0, 0.0),
            RELEASE_ALPHA_SLOW * 1.20
        );
        assert_eq!(
            gain_alpha(2.2, 2.0, 0.0, 1.0, 0.0),
            RELEASE_ALPHA_FAST * 0.65
        );
        // …and never below the 0.55 clamp, however bright the programme is scored.
        assert_eq!(
            gain_alpha(2.2, 2.0, 0.0, 2.0, 0.0),
            RELEASE_ALPHA_FAST * RELEASE_TONAL_MIN
        );
        // The attack is immune to all of it.
        assert_eq!(gain_alpha(1.0, 2.0, 1.0, 0.0, 0.0), ATTACK_ALPHA);

        // An armed quiet boost puts a floor under the release rate (`:345`).
        assert_eq!(
            gain_alpha(2.4, 2.0, 0.0, 0.0, 1.0),
            QUIET_RELEASE_ALPHA,
            "the quiet release floor must beat the slow release"
        );
    }

    #[test]
    fn the_attack_follows_its_documented_one_tenth_per_buffer_time_constant() {
        // The gain falls towards the target as g += 0.10*(target - g) (SosProcess.cpp:335, :347),
        // so successive increments shrink by exactly 1 - ATTACK_ALPHA every buffer.
        let (mut leveller, mut frame) = warmed_up(300.0, 0.1, 300);
        let boosted = leveller.gain();
        assert!(boosted > 3.0, "warm-up did not boost: {boosted}");

        // Step the programme up. The six-buffer power ring needs that many blocks to forget the
        // old level, so the trajectory is only first-order from then on.
        run_sine(&mut leveller, 8, 300.0, 0.354, &mut frame);
        let gains = run_sine(&mut leveller, 4, 300.0, 0.354, &mut frame);
        assert!(gains[1] < gains[0], "the gain must be falling: {gains:?}");

        let ratio = convergence_ratio(&gains);
        assert!(
            (ratio - (1.0 - ATTACK_ALPHA)).abs() < 2e-3,
            "attack converged at {ratio}, expected {}",
            1.0 - ATTACK_ALPHA
        );
    }

    #[test]
    fn the_fast_release_follows_its_documented_time_constant() {
        // A small upward gap (ratio <= RELEASE_GAP_THRESHOLD) takes RELEASE_ALPHA_FAST, scaled by
        // the tonality factor, which a 300 Hz tone drives to its 1.20 maximum (fully muffled).
        let (mut leveller, mut frame) = warmed_up(300.0, 0.4, 400);
        // The score is a one-pole approach to the clamped target (`SosProcess.cpp:217-219`), so it
        // converges on -1 without ever landing on it: 400 buffers at alpha 0.08 leave about 4e-7.
        assert!(
            (leveller.tonality_score + 1.0).abs() < 1e-5,
            "the tone must score muffled, got {}",
            leveller.tonality_score
        );

        run_sine(&mut leveller, 8, 300.0, 0.37, &mut frame);
        let gains = run_sine(&mut leveller, 4, 300.0, 0.37, &mut frame);
        assert!(gains[1] > gains[0], "the gain must be rising: {gains:?}");

        let expected_alpha = RELEASE_ALPHA_FAST * RELEASE_TONAL_MAX;
        let ratio = convergence_ratio(&gains);
        assert!(
            (ratio - (1.0 - expected_alpha)).abs() < 2e-3,
            "fast release converged at {ratio}, expected {}",
            1.0 - expected_alpha
        );
    }

    #[test]
    fn the_slow_release_follows_its_documented_time_constant() {
        // Halving the programme level opens a gap far wider than RELEASE_GAP_THRESHOLD, which
        // selects RELEASE_ALPHA_SLOW — again at the 1.20 muffled factor.
        let (mut leveller, mut frame) = warmed_up(300.0, 0.4, 400);

        run_sine(&mut leveller, 8, 300.0, 0.2, &mut frame);
        let gains = run_sine(&mut leveller, 4, 300.0, 0.2, &mut frame);
        assert!(gains[1] > gains[0], "the gain must be rising: {gains:?}");

        let expected_alpha = RELEASE_ALPHA_SLOW * RELEASE_TONAL_MAX;
        let ratio = convergence_ratio(&gains);
        assert!(
            (ratio - (1.0 - expected_alpha)).abs() < 2e-3,
            "slow release converged at {ratio}, expected {}",
            1.0 - expected_alpha
        );
    }

    #[test]
    fn a_quiet_programme_is_brought_up_and_a_loud_one_is_held_down() {
        // Quiet: the gain climbs towards max_gain_cap = effective_target / 0.125.
        let (quiet, _) = warmed_up(300.0, 0.05, 500);
        assert!(
            quiet.gain() > 4.0,
            "a -26 dBFS tone should be boosted hard, got {}",
            quiet.gain()
        );

        // Loud: `desired_gain` is floored at the quiet floor, which is floored at unity
        // (SosProcess.cpp:211, :333), so a loud programme is held at 1.0 rather than turned down
        // towards the RMS target. Only the peak clamp can go below unity.
        let (loud, _) = warmed_up(300.0, 0.9, 500);
        assert!(
            loud.gain() <= 1.0 + 1e-6,
            "a -1 dBFS tone must not be boosted, got {}",
            loud.gain()
        );
        assert!(loud.gain() > 0.9, "…nor crushed, got {}", loud.gain());

        // And the boost really does reach the output, not just the state.
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        let mut frame = 0;
        let mut out_peak: Real = 0.0;
        for _ in 0..500 {
            fill_sine(&mut buffer, 300.0, 0.05, frame);
            leveller.process(&mut buffer, 2);
            frame += BLOCK;
            out_peak = buffer.iter().fold(out_peak, |m, s| m.max(s.abs()));
        }
        assert!(
            out_peak > 0.2,
            "a 0.05 peak input should leave well above 0.2 at the output, got {out_peak}"
        );
    }

    #[test]
    fn the_ceiling_is_never_exceeded_even_when_a_boosted_gain_meets_a_full_scale_transient() {
        // The worst case for the stage: boost hard on a whisper, then hit it with full scale in
        // the very next buffer. The peak guard brings the gain down before the first sample that
        // would cross the ceiling, and the hard clip (:393-396) is still there for the rounding.
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        let mut frame = 0;

        for _ in 0..400 {
            fill_sine(&mut buffer, 300.0, 0.02, frame);
            leveller.process(&mut buffer, 2);
            frame += BLOCK;
        }
        assert!(leveller.gain() > 2.0, "expected a boosted starting gain");

        let mut worst: Real = 0.0;
        for block in 0..200 {
            // Alternate whisper and full scale so the ramp is never settled.
            let amp = if block % 2 == 0 { 1.0 } else { 0.02 };
            fill_sine(&mut buffer, 300.0, amp, frame);
            leveller.process(&mut buffer, 2);
            frame += BLOCK;
            worst = buffer.iter().fold(worst, |m, s| m.max(s.abs()));
        }
        assert!(
            worst <= CEILING,
            "the output reached {worst}, above the {CEILING} ceiling"
        );
    }

    #[test]
    fn a_full_scale_square_wave_stays_finite_and_inside_the_ceiling() {
        // Square waves are the pathological input for the side-chain high-pass: every edge is a
        // step of 2.0 through a filter with a 0.984 feedback coefficient.
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        let mut frame = 0usize;

        for _ in 0..2000 {
            for (i, chunk) in buffer.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                // 100 Hz square: 240 samples high, 240 low, at 48 kHz.
                let value = if ((frame + i) / 240).is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                chunk[0] = value;
                chunk[1] = value;
            }
            leveller.process(&mut buffer, 2);
            frame += BLOCK;

            for sample in &buffer {
                assert!(sample.is_finite(), "non-finite output sample {sample}");
                assert!(sample.abs() <= CEILING, "sample {sample} broke the ceiling");
            }
        }
        assert!(leveller.gain().is_finite() && leveller.gain() > 0.0);
        assert!(leveller.tonality_score.is_finite());
        assert!(leveller.headroom_score.is_finite());
        assert!(leveller.quiet_gain_floor.is_finite());
        assert!(leveller.previous_predicted_rms.is_finite());
    }

    #[test]
    fn silence_stays_silent_and_leaves_every_integrator_finite() {
        // Digital black exercises every division-by-nearly-zero in the algorithm at once:
        // current_rms is 0, the tonality ratio is 1e-12/1e-12, and predicted_rms is floored at
        // 1e-6 (SosProcess.cpp:303).
        let (mut leveller, _) = warmed_up(300.0, 0.05, 400);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];

        for _ in 0..3000 {
            buffer.fill(0.0);
            leveller.process(&mut buffer, 2);
            for sample in &buffer {
                assert_eq!(*sample, 0.0);
            }
        }
        assert!(leveller.gain().is_finite());
        assert!(leveller.tonality_score.is_finite());
        assert!(leveller.headroom_score.is_finite());
        assert!(leveller.previous_average_rms.is_finite());
        assert!(leveller.previous_predicted_rms.is_finite());
        // Silence is below the audible-peak threshold, so the retained quiet floor decays back to
        // exactly unity (SosProcess.cpp:449-461).
        assert_eq!(leveller.quiet_gain_floor, 1.0);
        assert_eq!(leveller.quiet_duration_seconds, 0.0);
    }

    #[test]
    fn a_sustained_whisper_arms_the_quiet_boost_past_the_nominal_gain_cap() {
        // SosProcess.cpp:309-329: the boost needs QUIET_ACTIVATION_SECONDS of continuously quiet
        // *output*, plus a peak above QUIET_AUDIBLE_PEAK_THRESHOLD, before it lifts max_gain_cap
        // towards QUIET_MAX_GAIN — and the gain it reaches is then retained as a floor (:417-423).
        let nominal_cap = MAX_TARGET_RMS / MAX_GAIN_CAP_DIVISOR;
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut frame = 0;

        // Under 10 s the boost is not armed, so the only things that can lift the cap above
        // `target / 0.125` are the two target trims at `SosProcess.cpp:234-241`: a 300 Hz tone
        // scores fully muffled (+12 %) and nine seconds of untouched headroom drives the headroom
        // integrator far enough negative to add most of a further +8 %. Nothing may reach the
        // ×10 the armed boost would allow.
        run_sine(&mut leveller, 900, 300.0, 0.008, &mut frame);
        let before_arming = leveller.gain();
        let unarmed_cap = nominal_cap * (1.0 + MUFFLED_TARGET_BOOST + HEADROOM_TARGET_BOOST);
        assert!(
            before_arming < unarmed_cap,
            "the quiet boost armed early: {before_arming} is past the unarmed cap {unarmed_cap}"
        );
        assert!(leveller.quiet_duration_seconds >= 8.0);

        // Well past the 10 s threshold plus its 2 s ramp, the cap and the gain both move up.
        run_sine(&mut leveller, 1600, 300.0, 0.008, &mut frame);
        let after_arming = leveller.gain();
        assert!(
            after_arming > before_arming + 0.2,
            "the quiet boost never engaged: {before_arming} -> {after_arming}"
        );
        assert!(
            after_arming < QUIET_MAX_GAIN,
            "the quiet boost blew past its +20 dB limit: {after_arming}"
        );
        assert!(
            leveller.quiet_gain_floor > 1.0,
            "the boosted gain should have been retained as a floor"
        );
    }

    #[test]
    fn the_gain_is_ramped_across_the_buffer_rather_than_stepped() {
        // SosProcess.cpp:375-379: frame n rides `gain_start + (n/frames) * (gain_end - gain_start)`,
        // so sample 0 gets the *previous* buffer's gain and the last sample very nearly the new
        // one. A DC buffer makes the ramp directly readable off the output.
        //
        // The level matters. The peak safety (`:358-364`) caps the ramp at `ceiling / peak`, and
        // at 0.05 that cap sits at ×20, far above the ×4.5 the stage is running at, so it cannot
        // interfere and the ramp really runs from the retained gain to the new one.
        const DC: Real = 0.05;
        let (mut leveller, _) = warmed_up(300.0, 0.05, 400);
        let start_gain = leveller.gain();
        assert!(
            start_gain < CEILING / DC,
            "the peak clamp would trim gain_start: {start_gain}"
        );

        let mut buffer = vec![DC; BLOCK * 2];
        leveller.process(&mut buffer, 2);
        let end_gain = leveller.gain();
        assert!(
            (end_gain - start_gain).abs() > 1e-4,
            "this test needs the gain to move: {start_gain} -> {end_gain}"
        );

        assert!(
            (buffer[0] / DC - start_gain).abs() < 1e-5,
            "sample 0 rode {}, expected the retained gain {start_gain}",
            buffer[0] / DC
        );
        let last = buffer[buffer.len() - 1] / DC;
        let expected_last =
            start_gain + ((BLOCK - 1) as Real / BLOCK as Real) * (end_gain - start_gain);
        assert!(
            (last - expected_last).abs() < 1e-5,
            "last sample rode {last}, expected {expected_last}"
        );

        // …and every frame in between is on the same straight line, which is what distinguishes a
        // ramp from a step at the block boundary.
        for (index, frame) in buffer.as_chunks::<2>().0.iter().enumerate() {
            let want = start_gain + (index as Real / BLOCK as Real) * (end_gain - start_gain);
            let got = frame[0] / DC;
            assert!(
                (got - want).abs() < 1e-5,
                "frame {index} rode {got}, expected {want}"
            );
        }
    }

    /// A 5.1 frame: a 300 Hz tone at `front` on every channel but the subwoofer, which carries a
    /// 50 Hz tone at `sub` of its own.
    fn surround_block(start_frame: usize, frames: usize, front: f64, sub: f64) -> Vec<Real> {
        const CHANNELS: usize = 6;
        let mut buffer = vec![0.0 as Real; frames * CHANNELS];
        for (i, frame) in buffer.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let t = (start_frame + i) as f64 / f64::from(FS);
            frame.fill((front * (core::f64::consts::TAU * 300.0 * t).sin()) as Real);
            frame[3] = (sub * (core::f64::consts::TAU * 50.0 * t).sin()) as Real;
        }
        buffer
    }

    #[test]
    fn the_subwoofer_is_levelled_with_the_rest_but_left_out_of_the_statistics() {
        // Changed on purpose: audit report #3. This test used to pin the original, which left
        // channel 3 out of the gain as well as the detector (`SosProcess.cpp:382-383`, `:908`):
        // on this fixture the fronts came out 13.1 dB up and the subwoofer at x1, so a quiet
        // scene lost its bass by 13.1 dB. The LFE is now levelled with the rest and kept out of the
        // statistics only.
        //
        // The subwoofer carries a 50 Hz tone twice as loud as the fronts, so any leakage into the
        // statistics would move the gain; and the second leveller hears the same fronts over a
        // silent subwoofer, so the two must decide identically.
        const CHANNELS: usize = 6;
        const LFE: usize = 3;
        let mut with_sub = VolumeLeveller::new(FS);
        with_sub.set_amount(MAX_AMOUNT);
        let mut silent_sub = with_sub.clone();

        let mut first_frame = 0;
        let (mut input, mut levelled, mut reference) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..400 {
            input = surround_block(first_frame, BLOCK, 0.05, 0.1);
            levelled = input.clone();
            with_sub.process_with_lfe(&mut levelled, CHANNELS, Some(LFE));

            reference = input.clone();
            for frame in reference.as_chunks_mut::<CHANNELS>().0 {
                frame[LFE] = 0.0;
            }
            silent_sub.process_with_lfe(&mut reference, CHANNELS, Some(LFE));
            first_frame += BLOCK;
        }

        assert!(
            with_sub.gain() > 3.2,
            "the fixture must lift the scene by more than 10 dB, got x{}",
            with_sub.gain()
        );
        // Levelled: every frame's subwoofer rode the gain its front left did.
        let mut compared = 0;
        for (dry, wet) in input
            .as_chunks::<CHANNELS>()
            .0
            .iter()
            .zip(levelled.as_chunks::<CHANNELS>().0)
        {
            if dry[0].abs() > 1e-2 && dry[LFE].abs() > 1e-2 {
                let front_gain = wet[0] / dry[0];
                let sub_gain = wet[LFE] / dry[LFE];
                assert!(
                    (front_gain - sub_gain).abs() < front_gain * 1e-5,
                    "the subwoofer rode x{sub_gain} while the fronts rode x{front_gain}"
                );
                compared += 1;
            }
        }
        assert!(compared > BLOCK / 2, "only {compared} frames were compared");

        // Not analysed: what the subwoofer carries changes nothing anywhere else.
        assert_eq!(with_sub.gain().to_bits(), silent_sub.gain().to_bits());
        for (wet, want) in levelled
            .as_chunks::<CHANNELS>()
            .0
            .iter()
            .zip(reference.as_chunks::<CHANNELS>().0)
        {
            for channel in (0..CHANNELS).filter(|c| *c != LFE) {
                assert_eq!(wet[channel].to_bits(), want[channel].to_bits());
            }
        }
        // …and its detector slot was never written.
        assert_eq!(with_sub.sc_prev_in[LFE], 0.0);
        assert_eq!(with_sub.tone_lp_state[LFE], [0.0; 3]);
    }

    #[test]
    fn degenerate_buffers_are_handled_without_panicking() {
        // Changed on purpose: audit report #2. Zero channels, and a buffer shorter than one frame,
        // change nothing. The original resets the gain to unity on both (`SosProcess.cpp:146-150`);
        // with a step that spans calls an empty call carries no audio, and resetting on it would
        // drop a boosted programme back to x1 in one sample — here x4.54, 13.1 dB — in the middle of
        // a step. So the leveller is boosted first, and left part way through a step, or a gain of
        // unity would pass under the old behaviour and the new alike.
        let (mut leveller, frame) = warmed_up(300.0, 0.05, 400);
        let mut half_step = vec![0.0 as Real; 256 * 2];
        fill_sine(&mut half_step, 300.0, 0.05, frame);
        leveller.process(&mut half_step, 2);
        let boosted = leveller.gain();
        assert!(boosted > 2.0, "the warm-up did not boost: {boosted}");
        let untouched = leveller.clone();

        let mut empty: Vec<Real> = Vec::new();
        leveller.process(&mut empty, 2);
        assert_eq!(leveller.gain().to_bits(), boosted.to_bits());
        leveller.process(&mut empty, 0);
        assert_eq!(leveller.gain().to_bits(), boosted.to_bits());
        let mut less_than_a_frame = [0.5 as Real];
        leveller.process(&mut less_than_a_frame, 2);
        assert_eq!(leveller.gain().to_bits(), boosted.to_bits());
        assert_eq!(less_than_a_frame, [0.5], "a lone sample must be untouched");

        // …and not a sample after them differs from a leveller that never saw them: the rest of the
        // step, the ramp and the next steps all go on exactly as they would have.
        let mut after = vec![0.0 as Real; 1024 * 2];
        fill_sine(&mut after, 300.0, 0.05, frame + 256);
        let mut reference = after.clone();
        leveller.process(&mut after, 2);
        let mut untouched = untouched;
        untouched.process(&mut reference, 2);
        assert!(
            after
                .iter()
                .zip(&reference)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        assert_eq!(leveller.gain().to_bits(), untouched.gain().to_bits());

        // A ragged tail is ignored rather than read past.
        let mut ragged = vec![0.5 as Real; BLOCK * 2 + 1];
        leveller.process(&mut ragged, 2);
        assert_eq!(ragged[BLOCK * 2], 0.5, "the odd sample must be untouched");

        // More channels than the detector has state for: still levelled, never indexed past 8.
        let channels = MAX_CHANNELS + 2;
        let mut wide = vec![0.05 as Real; 64 * channels];
        for _ in 0..200 {
            wide.fill(0.05);
            leveller.process(&mut wide, channels);
        }
        assert!(wide.iter().all(|s| s.is_finite() && *s > 0.05));
    }

    // -- The audit's fixes (#3 is above, beside the surround fixture) ----------------------------

    /// Levels a tone on channels 0 and 1 with a DC pilot on channel 2, handing the leveller the
    /// frames in calls of `quanta` (cycled), and returns the gain every frame rode.
    ///
    /// The pilot is named as the subwoofer, so it is levelled like the rest but stays out of the
    /// statistics: the leveller decides exactly as it would on the stereo tone alone, and the gain
    /// can be read off the pilot on every sample rather than only where the tone is far from zero.
    fn pilot_trajectory(
        rate: Real,
        quanta: &[usize],
        seconds: f64,
        programme: impl Fn(usize) -> Real,
    ) -> Vec<f64> {
        const PILOT: Real = 0.01;
        let total = (seconds * f64::from(rate)) as usize;
        let mut leveller = VolumeLeveller::new(rate);
        leveller.set_amount(MAX_AMOUNT);
        let mut gains = Vec::with_capacity(total);
        let mut frame = 0;
        for &quantum in quanta.iter().cycle() {
            if frame >= total {
                break;
            }
            let frames = quantum.min(total - frame);
            let mut buffer: Vec<Real> = (frame..frame + frames)
                .flat_map(|n| {
                    let value = programme(n);
                    [value, value, PILOT]
                })
                .collect();
            leveller.process_with_lfe(&mut buffer, 3, Some(2));
            gains.extend(
                buffer
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|out| f64::from(out[2]) / f64::from(PILOT)),
            );
            frame += frames;
        }
        gains
    }

    /// Milliseconds after `at` until the gain has covered 63 % of its way from where it stood to
    /// where it ends.
    fn time_to_63_percent(gains: &[f64], at: usize, rate: Real) -> f64 {
        let (start, end) = (gains[at - 1], gains[gains.len() - 1]);
        let mark = start + 0.632 * (end - start);
        let frames = gains[at..]
            .iter()
            .position(|gain| {
                if end < start {
                    *gain <= mark
                } else {
                    *gain >= mark
                }
            })
            .expect("the gain never got there");
        frames as f64 * 1000.0 / f64::from(rate)
    }

    #[test]
    fn the_attack_and_the_release_take_the_same_time_whatever_the_quantum_and_the_rate() {
        // Audit report #2. The original takes one step per buffer, and every smoothing constant is
        // per step, so on PipeWire the speed followed the quantum. On this fixture — a 300 Hz tone
        // stepping from 0.1 to 0.2 (attack) or back (release) after 4 s — the original's attack
        // at 48 kHz reached 63 % in 67 ms at a 128-frame quantum, 88 ms at 256, 147 ms at 480 and
        // 298 ms at 1024, its release in 301, 174, 233 and 404 ms, and at 2048 frames it had not
        // finished the warm-up when the step came. The stage now steps every 10 ms of audio, so
        // every quantum, and one that changes mid-track, lands within one step of the 10 ms one:
        // an attack of 146 to 157 ms and a release of 227 to 241 ms at 44.1, 48 and 96 kHz alike.
        const STEP_AT: f64 = 4.0;
        let changing = [256, 1024, 333, 2048, 97, 480];
        for (rate, quanta) in [
            (44_100.0, vec![441, 256, 1024]),
            (48_000.0, vec![480, 128, 256, 1024, 2048]),
            (96_000.0, vec![960, 256, 2048]),
        ] {
            let at = (STEP_AT * f64::from(rate)) as usize;
            let tone = |before: f64, after: f64| {
                move |n: usize| {
                    let amplitude = if n < at { before } else { after };
                    let t = n as f64 / f64::from(rate);
                    (amplitude * (core::f64::consts::TAU * 300.0 * t).sin()) as Real
                }
            };
            let times = |quanta: &[usize]| {
                let attack = pilot_trajectory(rate, quanta, STEP_AT + 3.0, tone(0.1, 0.2));
                let release = pilot_trajectory(rate, quanta, STEP_AT + 3.0, tone(0.2, 0.1));
                (
                    time_to_63_percent(&attack, at, rate),
                    time_to_63_percent(&release, at, rate),
                )
            };

            let (attack_10ms, release_10ms) = times(&quanta[..1]);
            assert!(
                (120.0..180.0).contains(&attack_10ms) && (200.0..280.0).contains(&release_10ms),
                "{rate} Hz: the 10 ms quantum itself moved: {attack_10ms} / {release_10ms} ms"
            );
            let one_step = 10.0 + 1000.0 / f64::from(rate);
            for quantum in quanta[1..]
                .iter()
                .map(core::slice::from_ref)
                .chain([&changing[..]])
            {
                let (attack, release) = times(quantum);
                assert!(
                    (attack - attack_10ms).abs() <= one_step,
                    "{rate} Hz, {quantum:?}: the attack took {attack} ms against {attack_10ms}"
                );
                assert!(
                    (release - release_10ms).abs() <= one_step,
                    "{rate} Hz, {quantum:?}: the release took {release} ms against {release_10ms}"
                );
            }
        }
    }

    #[test]
    fn at_480_frames_and_48_khz_the_stage_is_the_original_sample_for_sample_where_no_fix_applies() {
        // Audit report #2: stepping on a clock of its own changes nothing at the quantum the
        // original was tuned against. Captured from the implementation before the audit's fixes
        // (0f05ba5) on a fixture that never meets the peak safety, a subwoofer or digital silence
        // — five passages of a stereo tone, quiet, loud and in between, 600 steps in all: the gain
        // every 50 steps, and the RMS and the peak of the output over those 50 steps. The
        // rebuilt stage matched every output sample bit for bit when this was captured.
        const GOLDEN: [(Real, f64, Real); 12] = [
            (3.102858, 0.07000566031803987, 0.15511192),
            (4.016516, 0.1094909735607437, 0.20081055),
            (4.2207603, 0.12694253682417966, 0.21103692),
            (4.234997, 0.5164626934948632, 0.84699917),
            (4.235148, 0.5169814967383515, 0.84707993),
            (4.4715276, 0.053288892941713045, 0.089418374),
            (4.5121975, 0.05491462269676526, 0.09024065),
            (3.5750713, 0.23282220465166986, 0.44525418),
            (3.5624273, 0.21751615697551285, 0.35737154),
            (3.5669358, 0.21754425130690602, 0.356693),
            (4.4215035, 0.3716828553330951, 0.66313964),
            (4.5377216, 0.4122744829047998, 0.68065256),
        ];
        let passages = [
            (150, 1000.0, 0.05),
            (100, 1000.0, 0.2),
            (100, 440.0, 0.02),
            (150, 2500.0, 0.1),
            (100, 300.0, 0.15),
        ];
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut windows = Vec::new();
        let (mut frame, mut step) = (0, 0);
        let (mut sum, mut peak) = (0.0_f64, 0.0 as Real);
        for (steps, hz, amplitude) in passages {
            for _ in 0..steps {
                let mut buffer: Vec<Real> = (frame..frame + BLOCK)
                    .flat_map(|n| {
                        let t = core::f64::consts::TAU * hz * n as f64 / f64::from(FS);
                        let left = (amplitude * t.sin()) as Real;
                        [left, (0.7 * f64::from(left)) as Real]
                    })
                    .collect();
                leveller.process(&mut buffer, 2);
                frame += BLOCK;
                for sample in &buffer {
                    sum += f64::from(*sample).powi(2);
                    peak = peak.max(sample.abs());
                }
                step += 1;
                if step % 50 == 0 {
                    windows.push((leveller.gain(), (sum / (50.0 * 960.0)).sqrt(), peak));
                    (sum, peak) = (0.0, 0.0);
                }
            }
        }

        let close = |got: f64, want: f64| (got - want).abs() <= want.abs() * 1e-6;
        for (window, (got, want)) in windows.iter().zip(&GOLDEN).enumerate() {
            assert!(
                close(f64::from(got.0), f64::from(want.0))
                    && close(got.1, want.1)
                    && close(f64::from(got.2), f64::from(want.2)),
                "steps {}..{}: got {got:?}, the original gave {want:?}",
                window * 50,
                window * 50 + 50
            );
        }
        assert_eq!(windows.len(), GOLDEN.len());
    }

    #[test]
    fn the_state_machine_steps_once_every_ten_milliseconds_of_audio_whatever_the_call_size() {
        // Audit report #2. Handed 100 frames at a time, the original stepped on every call —
        // 48 decisions in these 4800 frames. The stage now carries a step across calls and
        // decides once per 480 frames, on the call that completes each step.
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut buffer = vec![0.0 as Real; 100 * 2];
        let mut frame = 0;
        let mut previous = leveller.gain();
        let mut decided_at = Vec::new();
        for _ in 0..48 {
            fill_sine(&mut buffer, 300.0, 0.05, frame);
            leveller.process(&mut buffer, 2);
            frame += 100;
            if leveller.gain() != previous {
                decided_at.push(frame);
                previous = leveller.gain();
            }
        }
        let expected: Vec<usize> = (1_usize..=10)
            .map(|k| (k * 480).div_ceil(100) * 100)
            .collect();
        assert_eq!(decided_at, expected);
    }

    #[test]
    fn a_step_gathers_the_same_statistics_however_the_host_slices_it() {
        // Audit report #2. The detector is accumulated sample by sample into the step, in the
        // original's order, so what a step measures — and with it the power ring and the tonality
        // — is bit for bit the same whether it arrived whole or in pieces.
        let run = |quanta: &[usize]| {
            let mut leveller = VolumeLeveller::new(FS);
            leveller.set_amount(MAX_AMOUNT);
            let mut frame = 0;
            for &quantum in quanta.iter().cycle() {
                if frame >= BLOCK * 300 {
                    break;
                }
                let frames = quantum.min(BLOCK * 300 - frame);
                let mut buffer = vec![0.0 as Real; frames * 2];
                fill_sine(&mut buffer, 700.0, 0.08, frame);
                leveller.process(&mut buffer, 2);
                frame += frames;
            }
            leveller
        };
        let whole = run(&[BLOCK]);
        for quanta in [&[128][..], &[1024], &[1, 333, 2048, 97]] {
            let sliced = run(quanta);
            assert_eq!(sliced.power_count, whole.power_count, "{quanta:?}");
            for (got, want) in sliced.power_history.iter().zip(&whole.power_history) {
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "{quanta:?}: the power ring moved"
                );
            }
            assert_eq!(
                sliced.tonality_score.to_bits(),
                whole.tonality_score.to_bits(),
                "{quanta:?}: the tonality moved"
            );
            assert_eq!(sliced.sc_prev_out, whole.sc_prev_out, "{quanta:?}");
        }
    }

    #[test]
    fn a_50_hz_tone_is_held_under_the_ceiling_by_its_unfiltered_peak_rather_than_clipped() {
        // Audit report #1. The original took the peak for `ceiling / peak` from the 120 Hz
        // high-passed side chain, which sees a 50 Hz tone at about 0.385 of its level. A tone at
        // 0.3 was lifted to x4.18 and the hard clip flattened 41.8 % of its samples at full
        // scale, leaving a residual only 20.6 dB under the tone. The peak safety now reads the
        // unfiltered peak: the gain stops at 1 / 0.3 = x3.33, and what comes out is the tone,
        // scaled, with the residual some 150 dB down.
        const AMP: Real = 0.3;
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        let (mut residual, mut signal) = (0.0_f64, 0.0_f64);
        for block in 0..600 {
            fill_sine(&mut buffer, 50.0, AMP, block * BLOCK);
            let dry = buffer.clone();
            leveller.process(&mut buffer, 2);
            assert!(buffer.iter().all(|sample| sample.abs() <= CEILING));
            if block < 500 {
                continue;
            }
            // The gain this block rode, by least squares, and what is left once it is taken out.
            let cross: f64 = buffer
                .iter()
                .zip(&dry)
                .map(|(wet, dry)| f64::from(*wet) * f64::from(*dry))
                .sum();
            let power: f64 = dry.iter().map(|dry| f64::from(*dry).powi(2)).sum();
            let gain = cross / power;
            residual += buffer
                .iter()
                .zip(&dry)
                .map(|(wet, dry)| (f64::from(*wet) - gain * f64::from(*dry)).powi(2))
                .sum::<f64>();
            signal += gain * gain * power;
        }
        let residual_db = 10.0 * (residual / signal).log10();
        assert!(
            residual_db < -100.0,
            "the tone came out distorted: residual {residual_db} dB"
        );
        assert!(
            leveller.gain() <= CEILING / AMP * (1.0 + 1e-6),
            "the gain went past the peak-safe x{}: x{}",
            CEILING / AMP,
            leveller.gain()
        );
        assert!(
            leveller.gain() > CEILING / AMP * 0.98,
            "the tone should still be lifted to the ceiling, got x{}",
            leveller.gain()
        );
    }

    /// The audit #4 fixture: a quiet 300 Hz passage lifted to about x4.5, then a burst at ±0.9 —
    /// a 2 kHz square — from frame `hit`, handed over in calls of `quantum` frames. Returns the
    /// gain every frame rode.
    fn burst_trajectory(quantum: usize, hit: usize) -> Vec<f64> {
        let programme = move |n: usize| {
            if n >= hit {
                if (n / 12).is_multiple_of(2) {
                    0.9
                } else {
                    -0.9
                }
            } else {
                let t = n as f64 / f64::from(FS);
                (0.05 * (core::f64::consts::TAU * 300.0 * t).sin()) as Real
            }
        };
        pilot_trajectory(FS, &[quantum], (hit + 80) as f64 / f64::from(FS), programme)
    }

    /// `a` over `b`, in decibels.
    fn db(a: f64, b: f64) -> f64 {
        20.0 * (a / b).log10()
    }

    /// The furthest the gain falls between one frame and the next, in decibels, from `from` on.
    fn largest_drop_db(gains: &[f64], from: usize) -> f64 {
        gains[from..]
            .windows(2)
            .map(|pair| db(pair[0], pair[1]))
            .fold(0.0, f64::max)
    }

    /// The largest step, in decibels, of a straight fade from `from` down to `to` over `frames`
    /// frames — its last, where the gain is lowest — or the whole fall at once over none.
    fn fade_step_db(from: f64, to: f64, frames: usize) -> f64 {
        db(to + (from - to) / frames.max(1) as f64, to)
    }

    /// [`PEAK_RAMP_SECONDS`] at the fixtures' rate: 96 frames.
    const LEAD: usize = 96;

    #[test]
    fn a_transient_late_in_a_step_pulls_the_gain_down_in_the_two_milliseconds_before_it() {
        // Audit report #4. A quiet 300 Hz passage lifted to about x4.5, then a burst at ±0.9 from
        // frame 400 of a 480-frame step. The original clamped both ends of its ramp to the
        // peak-safe gain, so the gain fell 12.9 dB between one sample and the next, 8.3 ms before
        // the hit: a click. Now the step starts where the last one ended — less 0.1 dB of the
        // original's other clamp, to the gain cap, which the burst's brightness moves — keeps to
        // the smoothing until 2 ms before the hit, and falls from there in steps of at most
        // 0.27 dB, arriving at the peak-safe x1.11 on the hit itself.
        const HIT: usize = 418 * BLOCK + 400;
        let gains = burst_trajectory(BLOCK, HIT);
        let step_start = 418 * BLOCK;

        assert!(
            gains[step_start - 1] > 4.0,
            "the fixture must start boosted"
        );
        assert!(
            db(gains[step_start], gains[step_start - 1]).abs() < 0.2,
            "the gain jumped at the top of the step: x{} -> x{}",
            gains[step_start - 1],
            gains[step_start]
        );
        let lead = HIT - (PEAK_RAMP_SECONDS * FS).round() as usize;
        assert!(
            db(gains[lead - 1], gains[step_start]) > -1.0,
            "the gain was already down to x{} {} frames before the hit",
            gains[lead - 1],
            HIT + 1 - lead
        );
        let largest_drop = largest_drop_db(&gains, step_start - 1);
        assert!(
            largest_drop < 0.5,
            "the gain fell {largest_drop} dB in one sample"
        );
        assert!(
            gains[HIT] <= f64::from(CEILING / 0.9) * (1.0 + 1e-6),
            "the hit rode x{}, past the peak-safe gain",
            gains[HIT]
        );
    }

    #[test]
    fn a_transient_just_past_a_step_boundary_inside_a_call_is_faded_into_from_the_step_before() {
        // Audit report #4, where a step boundary falls inside a call: the audio before it is in
        // hand, so the fade into a transient just past it starts in the step before. Levelled a
        // step at a time, the stage could fade only from the new step's first frame, so at a
        // 1024-frame quantum, with the boundary 960 frames into the call, a hit on the step's
        // first or second frame fell 12.22 dB between one sample and the next — the original,
        // clamping the whole buffer, fell 13.3 dB on the call's first frame, 20 ms before the hit
        // — and one 10 frames in fell 2.34 dB in its last step. Now every one of them falls over
        // the full 2 ms, in steps of at most 0.27 dB, and the gain 2 ms before the hit is still
        // where the smoothing had it. Both kinds of boundary are here: one with the rest of its
        // step in the next call (960 frames into a 1024-frame call), which plays on along the
        // step's ramp, and one with the whole step after it in the call (416 frames into a 1024-
        // or a 2048-frame one), which starts a ramp of its own from the gain the fade hands it.
        let whole_fade = fade_step_db(4.55, f64::from(CEILING / 0.9), LEAD);
        for (quantum, step, offset) in [
            (1024, 418, 0),
            (1024, 418, 1),
            (1024, 418, 10),
            (1024, 418, 50),
            (1024, 419, 0),
            (1024, 419, 10),
            (2048, 419, 0),
        ] {
            let step_start = step * BLOCK;
            let hit = step_start + offset;
            assert!(
                step_start % quantum >= LEAD,
                "the boundary must lie 2 ms or more into its call"
            );
            let gains = burst_trajectory(quantum, hit);
            let boosted = gains[hit - 200];
            assert!(boosted > 4.0, "the fixture must start boosted");
            assert!(
                db(gains[hit - LEAD - 1], boosted) > -0.2,
                "{quantum}/{offset}: the gain was down to x{} 2 ms before the hit",
                gains[hit - LEAD - 1]
            );
            let largest_drop = largest_drop_db(&gains, hit - 300);
            assert!(
                largest_drop <= whole_fade + 0.01,
                "{quantum}/{offset}: the gain fell {largest_drop} dB in one sample"
            );
            assert!(
                gains[hit] <= f64::from(CEILING / 0.9) * (1.0 + 1e-6),
                "{quantum}/{offset}: the hit rode x{}, past the peak-safe gain",
                gains[hit]
            );
        }
        assert!(whole_fade < 0.28, "{whole_fade}");
    }

    #[test]
    fn a_transient_early_in_a_call_is_faded_over_only_the_frames_of_the_call_before_it() {
        // Audit report #4, and its limit. The audio before a call has already been played when the
        // call arrives, so a transient n frames into a call can be faded into over those n frames
        // and no more; fading into one on a call's first frame would take 2 ms of look-ahead
        // latency on the whole chain. So at the 480-frame quantum a hit on the first or second
        // frame of a step — and of the call — still falls the whole 12.22 dB between one sample
        // and the next, one 10 frames in falls 2.34 dB in its last step and one 50 frames in
        // 0.52 dB; and at a 64-frame quantum, shorter than the fade, every hit is cut short: 2.83
        // dB 8 frames into its call, 1.53 dB 16 frames in, 0.80 dB 32 frames in. What the stage
        // does guarantee is measured here, wherever the step boundary falls: the gain falls no
        // faster than a straight fade over the frames of the call before the hit, or over 2 ms
        // where the call holds more. Step 418 starts on a call's first frame at both quanta;
        // step 419 starts 32 frames into a 64-frame call, where a hit on the step's first frame
        // used to fall 12.22 dB at once and now falls over the 32 frames before it, 0.80 dB in
        // its last step, and one 10 frames in over 42 frames rather than 10.
        for (quantum, step, offset) in [
            (BLOCK, 418, 0),
            (BLOCK, 418, 1),
            (BLOCK, 418, 10),
            (BLOCK, 418, 50),
            (BLOCK, 418, 95),
            (64, 418, 0),
            (64, 418, 10),
            (64, 418, 200),
            (64, 418, 400),
            (64, 418, 96),
            (64, 418, 63),
            (64, 419, 0),
            (64, 419, 1),
            (64, 419, 10),
            (256, 418, 95),
            (256, 418, 400),
            (333, 418, 200),
        ] {
            let hit = step * BLOCK + offset;
            let gains = burst_trajectory(quantum, hit);
            let safe = gains[hit];
            assert!(
                (safe / f64::from(CEILING / 0.9) - 1.0).abs() < 1e-6,
                "{quantum}/{step}+{offset}: the hit rode x{safe}, not the peak-safe gain"
            );
            let before = gains[hit - 200..hit].iter().copied().fold(0.0, f64::max);
            assert!(before > 4.0, "the fixture must start boosted");
            let lead = (hit % quantum).min(LEAD);
            let bound = fade_step_db(before, safe, lead);
            let largest_drop = largest_drop_db(&gains, hit - 300);
            assert!(
                largest_drop <= bound + 0.01,
                "{quantum}/{step}+{offset}: the gain fell {largest_drop} dB in one sample, {} \
                 frames into its call, against {bound} dB for a fade over {lead}",
                hit % quantum
            );
        }
    }

    #[test]
    fn the_next_steps_ceiling_never_comes_down_further_than_the_look_ahead_assumes() {
        // Audit report #4. A transient early in the next step of a call is looked for, and faded
        // into, against the lowest ceiling the next decision can set, since that decision needs
        // the whole next step. On programme that swings between dull and bright, as far as the
        // tonality can go, every decision has to land at or above the floor taken before it, and
        // on the bright side the floor has to sit under the ceiling in force.
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        let mut below = 0;
        for block in 0..1500 {
            let hz = if (block / 100) % 2 == 0 {
                150.0
            } else {
                9000.0
            };
            fill_sine(&mut buffer, hz, 0.2, block * BLOCK);
            let floor = leveller.next_ceiling_floor();
            if floor < leveller.effective_ceiling {
                below += 1;
            }
            leveller.process(&mut buffer, 2);
            assert!(
                leveller.effective_ceiling >= floor,
                "block {block}: the ceiling came down to {}, under the floor {floor}",
                leveller.effective_ceiling
            );
        }
        assert!(
            below > 100,
            "the fixture must reach the bright side: {below}"
        );
    }

    #[test]
    fn the_detector_filters_come_to_rest_at_zero_in_digital_silence_not_in_the_subnormals() {
        // Audit report #5. In the original, 0.1 s of digital silence after a tone left 6 of the 40
        // detector states subnormal, and 8 from 0.5 s on, for as long as the silence lasted: a
        // one-pole's step rounds to nothing before the state reaches zero. They are flushed to
        // zero once below 1e-20, so none is ever subnormal when a buffer is done.
        let (mut leveller, _) = warmed_up(300.0, 0.05, 400);
        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        for block in 0..200 {
            buffer.fill(0.0);
            leveller.process(&mut buffer, 2);
            let states = leveller
                .sc_prev_in
                .iter()
                .chain(&leveller.sc_prev_out)
                .chain(leveller.tone_lp_state.iter().flatten());
            for state in states {
                assert!(
                    !state.is_subnormal(),
                    "block {block}: a state idled at {state:e}"
                );
            }
        }
        assert_eq!(leveller.sc_prev_out, [0.0; MAX_CHANNELS]);
        assert_eq!(leveller.tone_lp_state, [[0.0; 3]; MAX_CHANNELS]);
    }

    #[test]
    fn the_ceiling_holds_on_every_sample_whatever_the_quantum() {
        // A whisper that has been lifted hard, then full scale, then the whisper again, with a
        // 40 Hz bass line under both and calls of every awkward size, including single frames:
        // the peak safety has to hold on every channel of every frame, with the hard clip as
        // nothing more than a guard.
        //
        // The clip makes `|out| <= CEILING` true whatever the peak safety does, so that alone
        // proves nothing about it. A DC pilot on the subwoofer is levelled with the rest but kept
        // out of the statistics, so the gain every frame rode can be read straight off it, and
        // `input * gain` is what the frame would have played had there been no clip. Before
        // audits #1 and #4 the clip caught 62k-77k samples of this fixture; now none may go past
        // the ceiling by more than the rounding in `ceiling / peak * peak` (none does: 65 frames
        // are set right at it, and not one of them over). With both guards switched off, frame
        // 24 013 — the first loud one after the lifted whisper — would play at x1.064.
        const PILOT: Real = 0.001;
        const CHANNELS: usize = 3;
        // Two roundings on the way in (the pilot and its product) and one on the way out.
        const ROUNDING: Real = 4.0 * Real::EPSILON;
        let quanta = [1, 7, 64, 333, 480, 1024, 2048, 97];
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut frame = 0;
        let mut peak_limited_frames = 0;
        for &quantum in quanta.iter().cycle().take(400) {
            let input: Vec<Real> = (frame..frame + quantum)
                .flat_map(|n| {
                    let t = n as f64 / f64::from(FS);
                    let level = if (n / 24_000) % 3 == 1 { 0.6 } else { 0.01 };
                    let value = level * (core::f64::consts::TAU * 300.0 * t).sin()
                        + level * 0.6 * (core::f64::consts::TAU * 40.0 * t).sin();
                    [value as Real, value as Real, PILOT]
                })
                .collect();
            let mut buffer = input.clone();
            leveller.process_with_lfe(&mut buffer, CHANNELS, Some(2));
            assert_eq!(
                leveller.effective_ceiling, CEILING,
                "the fixture must stay muffled, so the ceiling in force is the full one"
            );
            let (frames_in, _) = input.as_chunks::<CHANNELS>();
            let (frames_out, _) = buffer.as_chunks::<CHANNELS>();
            for (k, (before, after)) in frames_in.iter().zip(frames_out).enumerate() {
                let gain = after[2] / PILOT;
                let mut peak_limited = false;
                for channel in 0..2 {
                    let played = after[channel];
                    assert!(played.is_finite() && played.abs() <= CEILING, "{played}");
                    let unclipped = before[channel] * gain;
                    assert!(
                        unclipped.abs() <= CEILING * (1.0 + ROUNDING),
                        "frame {}: the gain {gain} carries {} to {unclipped}, the clip caught it",
                        frame + k,
                        before[channel]
                    );
                    peak_limited |= unclipped.abs() >= CEILING * (1.0 - ROUNDING);
                }
                peak_limited_frames += usize::from(peak_limited);
            }
            frame += quantum;
        }
        assert!(
            frame > 48_000 * 3,
            "the fixture must reach the loud passage"
        );
        assert!(
            peak_limited_frames > 0,
            "the fixture must drive the peak safety to the ceiling"
        );
    }

    #[test]
    fn a_step_split_across_calls_starts_its_ramp_on_the_first_frame_of_the_call_that_completes_it()
    {
        // Audit report #2, and what `process_with_lfe` promises about it: the decision is taken
        // before the part of the call that completes the step is levelled, so at a 256-frame
        // quantum the new ramp starts on frame 256 of the 480-frame step — the first frame of the
        // second call, 224 frames before the step completes — and not where the step completes.
        // A DC pilot named as the subwoofer is levelled but stays out of the statistics, so the
        // gain every frame rode can be read straight off it.
        const PILOT: Real = 0.01;
        const CHANNELS: usize = 3;
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let mut frame = 0;
        let mut call = |leveller: &mut VolumeLeveller, frames: usize| {
            let mut buffer: Vec<Real> = (frame..frame + frames)
                .flat_map(|n| {
                    let t = core::f64::consts::TAU * 300.0 * n as f64 / f64::from(FS);
                    let value = (0.05 * t.sin()) as Real;
                    [value, value, PILOT]
                })
                .collect();
            leveller.process_with_lfe(&mut buffer, CHANNELS, Some(2));
            frame += frames;
            buffer
                .as_chunks::<CHANNELS>()
                .0
                .iter()
                .map(|f| f[2])
                .collect::<Vec<_>>()
        };

        // A steady 0.05 tone in whole steps, stopped while the gain is still on its way up so that
        // the next ramp moves.
        for _ in 0..60 {
            call(&mut leveller, BLOCK);
        }
        let rising = leveller.gain();

        // The first part of the step is played at the gain already in force: nothing is decided.
        let first = call(&mut leveller, 256);
        assert!(
            first
                .iter()
                .all(|p| p.to_bits() == (PILOT * rising).to_bits())
        );
        assert_eq!(leveller.step.frames, 256);

        // The second call completes the step after 224 frames, and the ramp starts on its first.
        let second = call(&mut leveller, 256);
        let ramp = leveller.ramp;
        assert_eq!((ramp.length, ramp.elapsed), (BLOCK, 256));
        assert_eq!(leveller.step.frames, 32, "the next step has begun");
        assert_eq!(ramp.from.to_bits(), rising.to_bits());
        for (k, pilot) in second.iter().enumerate() {
            let t = k as Real / BLOCK as Real;
            let gain = ramp.from + t * (ramp.to - ramp.from);
            assert_eq!(pilot.to_bits(), (PILOT * gain).to_bits(), "frame {k}");
        }
        // Read "where the step completes", frames 0 to 223 would all hold x3.5949; they rise to
        // x3.6048 by frame 223 instead.
        assert!(
            second[223] > second[0],
            "the ramp must already be moving before the step completes"
        );
    }

    #[test]
    fn the_time_constants_are_in_milliseconds() {
        // Audit report #2: the step, and so the six-step power ring, is a length of time.
        assert_eq!(frames_in(48_000.0, SUB_BLOCK_SECONDS), 480);
        assert_eq!(frames_in(44_100.0, SUB_BLOCK_SECONDS), 441);
        assert_eq!(frames_in(96_000.0, SUB_BLOCK_SECONDS), 960);
        assert_eq!(frames_in(48_000.0, PEAK_RAMP_SECONDS), 96);
        assert!((HISTORY_SIZE as Real * SUB_BLOCK_SECONDS - POWER_HISTORY_SECONDS).abs() < 1e-6);
    }

    #[test]
    fn switched_back_on_mid_release_the_stage_carries_on_from_where_the_release_had_got_to() {
        // Audit #11: switched off, the gain being played glides back to unity over 20 ms; brought
        // back before that, the stage starts from wherever the glide had got to rather than from
        // unity, which would be the step down the glide exists to avoid.
        let mut leveller = VolumeLeveller::new(48_000.0);
        leveller.set_amount(4.0);
        let quiet = |frames: usize| -> Vec<Real> {
            (0..frames)
                .flat_map(|n| {
                    let s = (n as Real * 300.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.02;
                    [s, s]
                })
                .collect()
        };
        for _ in 0..200 {
            let mut block = quiet(480);
            leveller.process(&mut block, 2);
        }
        let lifted = leveller.ramp.at(0);
        assert!(lifted > 3.0, "the fixture was not lifted: {lifted}");

        leveller.set_amount(0.0);
        let mut releasing = quiet(480);
        leveller.process(&mut releasing, 2);
        let halfway = leveller.release.value();
        assert!(halfway > 1.0 && halfway < lifted, "{halfway}");

        leveller.set_amount(4.0);
        assert!(!leveller.release.is_gliding());
        assert_eq!(leveller.ramp.at(0).to_bits(), halfway.to_bits());
        let input = quiet(480);
        let mut resumed = input.clone();
        leveller.process(&mut resumed, 2);
        // Frame 20, a quarter of the way up the tone's first cycle, is still played at the
        // release's gain, not at unity and not back at the full lift.
        let gain = resumed[40] / input[40];
        assert!(
            (gain - halfway).abs() < 0.05 * halfway,
            "the stage started from {gain}, not {halfway}"
        );
    }

    #[test]
    fn a_stage_its_owner_leaves_out_is_switched_off_at_once_and_drops_its_let_down() {
        // Audit #11: the let-down that switching the stage off starts is played by `process`
        // alone, so a stage its owner is not running (FxSound or the equalizer off) must not keep
        // one for when it comes back, and switched off while it is left out it is cleared at
        // once, as the original clears it: from then on it is the exact bypass.
        let quiet = |frames: usize| -> Vec<Real> {
            (0..frames)
                .flat_map(|n| {
                    let s = (n as Real * 300.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.02;
                    [s, s]
                })
                .collect()
        };
        let lifted = || {
            let mut leveller = VolumeLeveller::new(48_000.0);
            leveller.set_amount(4.0);
            for _ in 0..200 {
                leveller.process(&mut quiet(480), 2);
            }
            assert!(leveller.ramp.at(0) > 3.0, "the fixture was not lifted");
            leveller
        };

        // Left out, then switched off.
        let mut leveller = lifted();
        leveller.sit_out();
        leveller.set_amount(0.0);
        assert!(!leveller.release.is_gliding());
        let input = quiet(480);
        let mut block = input.clone();
        leveller.process(&mut block, 2);
        assert_eq!(
            block, input,
            "a stage switched off while left out touched the audio"
        );

        // Switched off, then left out before the let-down could play.
        let mut leveller = lifted();
        leveller.set_amount(0.0);
        assert!(leveller.release.is_gliding());
        leveller.sit_out();
        assert!(!leveller.release.is_gliding());
        let mut block = input.clone();
        leveller.process(&mut block, 2);
        assert_eq!(
            block, input,
            "the let-down waited for the stage to come back"
        );
    }
}
