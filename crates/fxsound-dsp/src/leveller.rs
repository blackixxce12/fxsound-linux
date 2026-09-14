//! Volume levelling, plus the normalisation stage that sits immediately before it.
//!
//! [`VolumeLeveller`] is a literal port of `applyVolumeLeveling`
//! (`dsp/ptutil/SOS/SosProcess.cpp:139-492`), with its constant table at
//! `dsp/ptutil/SOS/SosProcess.cpp:38-76`. It is by some distance the most elaborate block in the
//! original tree: a 120 Hz side-chain detector, a three-band tonality estimate, a six-buffer RMS
//! ring with a gradient predictor and a mis-prediction detector, a 60-second headroom integrator,
//! and a 30-second window of one-second peak buckets driving a retained "quiet gain floor".
//!
//! Two properties of the original are worth stating up front, because they are surprising and both
//! are deliberate here:
//!
//! * **The stage is boost-only in its target.** `desired_gain` is floored at `quiet_gain_floor`
//!   (`SosProcess.cpp:333`), and that floor is itself floored at 1.0 (`:211`, `:458-461`). The only
//!   thing that ever pulls the gain below unity is the peak-safety clamp `ceiling / peak`
//!   (`:358-365`). So a loud programme is *held* at unity and clipped at the ceiling rather than
//!   turned down towards the RMS target.
//! * **The gain is ramped linearly across the buffer** from the previous buffer's value to the new
//!   one (`:375-379`), so the stage is sample-accurate inside a block and its behaviour depends on
//!   the block size the host hands it.
//!
//! Real-time safety: every piece of state is a fixed-size inline array, so there is nothing to
//! allocate — not in `new`, not ever. The per-buffer cost is two passes over the samples plus a
//! handful of `sqrt`/`log10` calls, and the only loops whose trip count is not the buffer length
//! are bounded by [`PEAK_WINDOW_SIZE`]. No branch in here can panic: every index is proved in
//! range by construction, and every divisor is either a compile-time constant or guarded.
//!
//! The parameter is an abstract 0..=4 "amount", not decibels, despite the original's
//! `setVolumeLeveling(float gain_db)` name (`docs/spec/08-dsp-api.md` §8.4,
//! `GraphicEqSet.cpp:76-89`).

use crate::biquad::{MAX_CHANNELS, Real};

// ---------------------------------------------------------------------------------------------
// The constant table, `SosProcess.cpp:38-76`.
// ---------------------------------------------------------------------------------------------

/// Hard clip ceiling. `SosProcess.cpp:38`.
pub const CEILING: Real = 1.0;
/// Gain-down smoothing per buffer. `SosProcess.cpp:39`.
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
/// Largest gain drop permitted inside one buffer, `10^(-1 dB / 20)`. `SosProcess.cpp:50`.
///
/// The original spells it `0.8912509381337456f`; every digit past the ninth is lost to `float`, so
/// this is the shortest literal that names the same `f32`.
pub const MIN_RATIO_PER_BUFFER: Real = 0.891_250_9;
/// dB span that maps the tonality measurement onto -1..=1. `SosProcess.cpp:51`.
pub const TONALITY_DB_RANGE: Real = 7.0;
/// One-pole smoothing of the tonality score, per buffer. `SosProcess.cpp:52`.
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
/// `SOS_VOLUME_LEVELING_HISTORY_SIZE` (`u_sos.h:42`) — buffers in the RMS power ring.
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

/// `SosProcess.cpp:335-346`: the per-buffer smoothing coefficient for the gain.
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

    /// `volume_leveling_gain`: where the previous buffer's ramp ended.
    gain: Real,
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
        }
    }

    /// The original takes the rate as an argument to every buffer; here it is set once.
    ///
    /// The detector coefficients are not recomputed until the next [`process`](Self::process),
    /// and then only if the rate actually changed (`SosProcess.cpp:90-103`).
    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        self.sample_rate = sample_rate;
    }

    /// The abstract 0..=4 control amount; 0 disables the stage and resets its state machine.
    ///
    /// `GraphicEqSetVolumeLeveling` (`GraphicEqSet.cpp:76-89`) clamps and scales, and
    /// `sosSetVolumeLeveling` (`SosSet.cpp:281-340`) clears the whole state machine when the
    /// resulting target is zero.
    pub fn set_amount(&mut self, amount: Real) {
        self.amount = clamp_real(amount, 0.0, MAX_AMOUNT);
        self.target_rms = target_rms_for_amount(self.amount);
        if self.target_rms <= 0.0 {
            self.reset();
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

    /// The gain the last buffer's ramp ended on. Useful for metering; not part of the original API.
    #[must_use]
    pub const fn gain(&self) -> Real {
        self.gain
    }

    /// Clear the state machine without changing the amount (`SosSet.cpp:292-320`).
    ///
    /// The cached detector coefficients are cleared too, so the next buffer redesigns them.
    pub fn reset(&mut self) {
        self.gain = 1.0;
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
    }

    /// Level one interleaved buffer in place.
    ///
    /// RT-safe: no allocation, no locks, no panicking path. At amount 0 the buffer is not touched
    /// at all — the stage is an exact bypass, not a unity multiply (`SosProcess.cpp:146-150`).
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        self.process_excluding(buffer, channels, None);
    }

    /// As [`process`](Self::process), but leaving one channel out of both the detector and the
    /// gain.
    ///
    /// The surround path passes channel 3 — the LFE — so that the sub is neither analysed nor
    /// levelled (`SosProcess.cpp:908`). The stereo path passes nothing (`:725`).
    pub fn process_excluding(
        &mut self,
        buffer: &mut [Real],
        channels: usize,
        excluded_channel: Option<usize>,
    ) {
        if channels == 0 {
            self.gain = 1.0;
            return;
        }
        let frames = buffer.len() / channels;
        if self.target_rms <= 0.0 || frames == 0 {
            self.gain = 1.0;
            return;
        }

        // The original indexes fixed `[8]` arrays with the raw channel number, so a ninth channel
        // would be a buffer overrun. Here the detector simply ignores anything past
        // `MAX_CHANNELS`; the gain is still applied to those channels so they stay in step with
        // the rest of the mix. For every format FxSound actually handles (mono, stereo, 5.1, 7.1)
        // this is bit-identical to the original.
        let detector_channels = channels.min(MAX_CHANNELS);
        let analysed_channels = (0..detector_channels)
            .filter(|c| Some(*c) != excluded_channel)
            .count();
        if analysed_channels == 0 {
            // `SosProcess.cpp:153-154` returns here *without* resetting the gain.
            return;
        }
        let analysed_samples = (frames * analysed_channels) as Real;

        let effective_sample_rate = if self.sample_rate > MIN_PLAUSIBLE_SAMPLE_RATE {
            self.sample_rate
        } else {
            FALLBACK_SAMPLE_RATE
        };
        self.update_coefficients(effective_sample_rate);

        // -- Pass 1: side-chain detector and tonality split (`SosProcess.cpp:166-203`). ----------
        let mut sum_squares: Real = 0.0;
        let mut peak: Real = 0.0;
        let mut low_energy: Real = 0.0;
        let mut body_energy: Real = 0.0;
        let mut presence_energy: Real = 0.0;
        let mut air_energy: Real = 0.0;

        for frame in buffer.chunks_exact(channels) {
            for (channel, &value) in frame.iter().enumerate().take(detector_channels) {
                if Some(channel) == excluded_channel {
                    continue;
                }

                let sc_prev_in = self.sc_prev_in[channel];
                let sc_prev_out = self.sc_prev_out[channel];
                let sc_value = self.sc_hpf_alpha * (sc_prev_out + value - sc_prev_in);
                self.sc_prev_in[channel] = value;
                self.sc_prev_out[channel] = sc_value;

                sum_squares += sc_value * sc_value;

                let abs_value = sc_value.abs();
                if abs_value > peak {
                    peak = abs_value;
                }

                let tone_state = &mut self.tone_lp_state[channel];
                tone_state[0] += self.tone_low_alpha * (value - tone_state[0]);
                tone_state[1] += self.tone_body_alpha * (value - tone_state[1]);
                tone_state[2] += self.tone_presence_alpha * (value - tone_state[2]);

                let low_band = tone_state[0];
                let body_band = tone_state[1] - tone_state[0];
                let presence_band = tone_state[2] - tone_state[1];
                let air_band = value - tone_state[2];

                low_energy += low_band * low_band;
                body_energy += body_band * body_band;
                presence_energy += presence_band * presence_band;
                air_energy += air_band * air_band;
            }
        }

        // -- Targets and authorities (`SosProcess.cpp:205-244`). --------------------------------
        let current_power = sum_squares / analysed_samples;
        let current_rms = current_power.sqrt();
        let mut gain_start = self.gain;
        let mut gain_end = gain_start;
        let headroom_reduce_score = self.headroom_score.max(0.0);
        let headroom_boost_score = (-self.headroom_score).max(0.0);
        let mut quiet_gain_floor = self.quiet_gain_floor.max(1.0);
        // Read before the state machine advances it at `:410`; `:212` and `:310` see the same
        // value in the original for the same reason.
        let quiet_duration_before = self.quiet_duration_seconds;

        let target_tonality_score = tonality_score(tonality_db(
            low_energy,
            body_energy,
            presence_energy,
            air_energy,
        ));
        self.tonality_score = self.tonality_score * (1.0 - TONALITY_SMOOTHING)
            + target_tonality_score * TONALITY_SMOOTHING;

        let clear_score = self.tonality_score.max(0.0);
        let muffled_score = (-self.tonality_score).max(0.0);
        let buffer_duration_seconds = frames as Real / effective_sample_rate;
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

        // -- The six-buffer power ring (`SosProcess.cpp:246-265`). ------------------------------
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
            // an attenuation. Only the peak clamp below can pull the gain under unity.
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

        // Avoid abrupt "crushed" sound: at most 1 dB of drop per buffer (`SosProcess.cpp:350-356`).
        if gain_end < gain_start {
            let min_allowed_gain_end = gain_start * MIN_RATIO_PER_BUFFER;
            if gain_end < min_allowed_gain_end {
                gain_end = min_allowed_gain_end;
            }
        }

        // Peak safety, applied to both ends of the ramp (`SosProcess.cpp:358-365`).
        if peak > TINY {
            let peak_safe_gain = effective_ceiling / peak;
            if gain_end > peak_safe_gain {
                gain_end = peak_safe_gain;
            }
            if gain_start > peak_safe_gain {
                gain_start = peak_safe_gain;
            }
        }

        gain_start = clamp_real(gain_start, 0.0, max_gain_cap);
        gain_end = clamp_real(gain_end, 0.0, max_gain_cap);
        self.gain = gain_end;

        // -- Pass 2: the ramp, and the post-gain statistics (`SosProcess.cpp:371-400`). ----------
        let mut post_gain_sum_squares: Real = 0.0;
        let mut post_gain_peak_abs: Real = 0.0;
        let mut ceiling_hit_count: usize = 0;
        let near_ceiling = effective_ceiling * HEADROOM_NEAR_CEILING_THRESHOLD;
        let frames_real = frames as Real;

        for (sample, frame) in buffer.chunks_exact_mut(channels).enumerate() {
            let t = sample as Real / frames_real;
            let gain = gain_start + t * (gain_end - gain_start);

            for (channel, value) in frame.iter_mut().enumerate() {
                if Some(channel) == excluded_channel {
                    continue;
                }

                *value *= gain;

                if channel < detector_channels {
                    let post_gain_abs = value.abs();
                    post_gain_sum_squares += *value * *value;
                    if post_gain_abs > post_gain_peak_abs {
                        post_gain_peak_abs = post_gain_abs;
                    }
                    if post_gain_abs >= near_ceiling {
                        ceiling_hit_count += 1;
                    }
                }

                if *value > effective_ceiling {
                    *value = effective_ceiling;
                } else if *value < -effective_ceiling {
                    *value = -effective_ceiling;
                }
            }
        }

        // -- The quiet state machine (`SosProcess.cpp:402-462`). ---------------------------------
        let post_gain_rms = (post_gain_sum_squares / analysed_samples).sqrt();
        self.update_quiet_peak_window(post_gain_peak_abs, buffer_duration_seconds);
        let rolling_peak_max = self.quiet_peak_window_max();

        let post_gain_still_quiet =
            peak > QUIET_AUDIBLE_PEAK_THRESHOLD && post_gain_rms < VERY_QUIET_RMS_THRESHOLD;
        if post_gain_still_quiet {
            self.quiet_duration_seconds += buffer_duration_seconds;
        } else {
            self.quiet_duration_seconds = 0.0;
        }

        // The boost "had authority" if it was armed and actually pushed past the nominal cap.
        let quiet_boost_had_authority =
            quiet_duration_before >= QUIET_ACTIVATION_SECONDS && gain_end > nominal_gain_cap;
        if quiet_boost_had_authority && gain_end > quiet_gain_floor {
            quiet_gain_floor = gain_end;
        }

        let quiet_peak_window_ready = self.quiet_peak_history_count == PEAK_WINDOW_SIZE;
        let quiet_floor_is_active =
            quiet_duration_before >= QUIET_ACTIVATION_SECONDS || quiet_gain_floor > 1.0;
        let quiet_peak_target = effective_ceiling * QUIET_PEAK_TARGET_RATIO;
        let sustained_headroom_available = rolling_peak_max > QUIET_AUDIBLE_PEAK_THRESHOLD
            && rolling_peak_max < quiet_peak_target
            && headroom_reduce_score < 0.25;
        if quiet_peak_window_ready && quiet_floor_is_active && sustained_headroom_available {
            let desired_quiet_floor = clamp_real(
                quiet_gain_floor * (quiet_peak_target / rolling_peak_max.max(TINY)),
                quiet_gain_floor,
                QUIET_MAX_GAIN,
            );
            let quiet_floor_raise_alpha = clamp_real(
                buffer_duration_seconds / QUIET_PEAK_FLOOR_RAISE_TIME_SECONDS,
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
        let ceiling_hit_ratio = ceiling_hit_count as Real / analysed_samples;
        let target_headroom_score =
            headroom_target_score(ceiling_hit_ratio, post_gain_peak_abs, effective_ceiling);
        let headroom_alpha = clamp_real(
            buffer_duration_seconds / HEADROOM_TIME_SECONDS,
            SLOW_ALPHA_MIN,
            SLOW_ALPHA_MAX,
        );
        self.headroom_score = self.headroom_score * (1.0 - headroom_alpha)
            + target_headroom_score * headroom_alpha;
    }

    /// `updateVolumeLevelingCoefficients` (`SosProcess.cpp:90-103`): redesign the four detector
    /// one-poles, but only when the sample rate has actually moved.
    fn update_coefficients(&mut self, sample_rate: Real) {
        if self.alpha_sample_rate == sample_rate {
            return;
        }
        self.sc_hpf_alpha = high_pass_alpha(SIDECHAIN_HPF_HZ, sample_rate);
        self.tone_low_alpha = one_pole_alpha(TONE_LOW_HZ, sample_rate);
        self.tone_body_alpha = one_pole_alpha(TONE_BODY_HZ, sample_rate);
        self.tone_presence_alpha = one_pole_alpha(TONE_PRESENCE_HZ, sample_rate);
        self.alpha_sample_rate = sample_rate;
    }

    /// `updateQuietPeakWindow` (`SosProcess.cpp:105-126`): fold this buffer's post-gain peak into
    /// the current one-second bucket, retiring whole buckets into the ring as they fill.
    ///
    /// The `while` is bounded: `buffer_duration_seconds` is at most `frames / 1000` by the sample
    /// rate guard, so even a pathological block retires a finite number of buckets.
    fn update_quiet_peak_window(&mut self, post_gain_peak_abs: Real, buffer_duration_seconds: Real) {
        self.quiet_peak_bucket_max = self.quiet_peak_bucket_max.max(post_gain_peak_abs);
        self.quiet_peak_bucket_seconds += buffer_duration_seconds;

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

// ---------------------------------------------------------------------------------------------
// Normaliser
// ---------------------------------------------------------------------------------------------

/// Slow attack, instant release. `SosProcess.cpp:699-703`.
const NORMALISATION_ATTACK_BASE: Real = 0.0005;
/// …scaled by how far the target is from where the gain already sits.
const NORMALISATION_ATTACK_SLOPE: Real = 0.001;
/// `SosProcess.cpp:711` — the smoothing coefficient is capped regardless of branch.
const NORMALISATION_MAX_ALPHA: Real = 0.5;
/// `SosProcess.cpp:687-688` — the stage attenuates only, and never by more than 40 dB.
const NORMALISATION_MIN_GAIN: Real = 0.01;
const NORMALISATION_MAX_GAIN: Real = 1.0;

/// The RMS normalisation stage, `SosProcess.cpp:678-724` (`docs/spec/08-dsp-api.md` §8.3).
///
/// Attenuate-only automatic gain with a ~20-second attack and an instantaneous release, applied
/// flat across the buffer with no ramp. It is **stereo-only** in the original
/// (`SosProcess.cpp:679`) and is left here as-is: it sits between master gain/balance and the
/// leveller.
///
/// **No GUI control reaches it.** `setNormalization` is never called from `fxsound/Source/GUI/`,
/// so in the shipping application this stage is permanently at its `gain_db == 0` default, which
/// is also its disabled state. It is ported for completeness of the DSP surface, not because the
/// app drives it.
#[derive(Clone, Debug)]
pub struct Normaliser {
    gain_db: Real,
    /// `target_rms`; exactly 1.0 means the stage is off (`Sos.cpp:65`).
    target_rms: Real,
    /// `normalization_gain` (`Sos.cpp:66`).
    gain: Real,
}

impl Default for Normaliser {
    fn default() -> Self {
        Self::new()
    }
}

impl Normaliser {
    /// The power-on state: 0 dB, which is also "disabled" (`Sos.cpp:65-66`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            gain_db: 0.0,
            target_rms: 1.0,
            gain: 1.0,
        }
    }

    /// `GraphicEqSetNormalization` (`GraphicEqSet.cpp:62-74`): the dB value becomes a linear
    /// **target RMS**, not a gain.
    pub fn set_gain_db(&mut self, gain_db: Real) {
        self.gain_db = gain_db;
        self.target_rms = (10.0 as Real).powf(gain_db / 20.0);
    }

    /// The dB value last handed to [`set_gain_db`](Self::set_gain_db).
    #[must_use]
    pub const fn gain_db(&self) -> Real {
        self.gain_db
    }

    /// The stage is disabled at exactly `target_rms == 1.0`, i.e. at 0 dB (`SosProcess.cpp:679`).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.target_rms != 1.0
    }

    /// The smoothed gain currently applied.
    #[must_use]
    pub const fn gain(&self) -> Real {
        self.gain
    }

    /// Return the smoothed gain to unity, leaving the target alone.
    pub fn reset(&mut self) {
        self.gain = 1.0;
    }

    /// Normalise one interleaved buffer in place. Stereo only; any other channel count is a
    /// bit-exact bypass, as it is in the original.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels != 2 || !self.is_enabled() {
            return;
        }
        let frames = buffer.len() / 2;
        if frames == 0 {
            return;
        }

        // The original accumulates this inside the EQ's own sample loop, over the post-master-gain,
        // post-balance output samples (`SosProcess.cpp:633-636`) — the same samples we are handed.
        let mut sum_squares: Real = 0.0;
        for frame in buffer.as_chunks::<2>().0 {
            sum_squares += frame[0] * frame[0] + frame[1] * frame[1];
        }

        let mut current_rms = (sum_squares / (frames * 2) as Real).sqrt();
        if current_rms < TINY {
            current_rms = TINY;
        }

        let target_gain = clamp_real(
            self.target_rms / current_rms,
            NORMALISATION_MIN_GAIN,
            NORMALISATION_MAX_GAIN,
        );
        let gain_diff = (target_gain - self.gain).abs();
        let smoothing_factor = if target_gain > self.gain {
            NORMALISATION_ATTACK_BASE + gain_diff * NORMALISATION_ATTACK_SLOPE
        } else {
            1.0
        }
        .min(NORMALISATION_MAX_ALPHA);

        self.gain += (target_gain - self.gain) * smoothing_factor;

        let gain = self.gain;
        for sample in buffer.iter_mut() {
            *sample *= gain;
        }
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
        assert_eq!(gain_alpha(2.2, 2.0, 1.0, 0.0, 0.0), RELEASE_ALPHA_FAST * 1.20);
        assert_eq!(gain_alpha(2.4, 2.0, 1.0, 0.0, 0.0), RELEASE_ALPHA_SLOW * 1.20);
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
        // the very next buffer. gain_start is clamped to the peak-safe gain (SosProcess.cpp:363)
        // and anything left over is hard-clipped (:393-396).
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
        // The level matters. The side-chain high-pass turns a DC block into a decaying transient
        // whose peak is essentially the DC level itself, and `:358-364` clamps *both* ends of the
        // ramp to `ceiling / peak`. At 0.05 that clamp sits at ×20, far above the ×4.5 the stage
        // is running at, so it cannot interfere and `gain_start` really is the retained gain.
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

    #[test]
    fn an_excluded_channel_is_neither_analysed_nor_levelled() {
        // The 5.1 path leaves the LFE out of both passes (SosProcess.cpp:908 passes channel 3).
        //
        // The programme has to be a real tone, not a constant: the detector is fed through a
        // 120 Hz high-pass (`:174-177`), which sees DC as a single decaying transient and then
        // nothing at all, so a DC "quiet programme" would leave `current_rms` below the 1e-6 guard
        // at `:307` and freeze the gain where it stood. The LFE carries a constant precisely so
        // that any leakage into it would be unmistakable.
        const LFE_MARKER: Real = 0.4;
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);
        let channels = 6;
        let mut buffer = vec![0.0 as Real; BLOCK * channels];
        let mut first_frame = 0usize;

        for _ in 0..400 {
            for (i, frame) in buffer.chunks_exact_mut(channels).enumerate() {
                let n = (first_frame + i) as f64;
                let phase = core::f64::consts::TAU * 300.0 * n / f64::from(FS);
                frame.fill((0.05 * phase.sin()) as Real);
                frame[3] = LFE_MARKER;
            }
            leveller.process_excluding(&mut buffer, channels, Some(3));
            first_frame += BLOCK;
        }

        assert!(
            leveller.gain() > 1.5,
            "the other channels were not levelled: {}",
            leveller.gain()
        );
        for frame in buffer.chunks_exact(channels) {
            assert_eq!(
                frame[3].to_bits(),
                LFE_MARKER.to_bits(),
                "the LFE must pass through untouched"
            );
        }
        // The front channels carry the same tone scaled by the ramped gain, so their peak has to
        // have grown by very nearly that gain.
        let out_peak = buffer
            .chunks_exact(channels)
            .fold(0.0 as Real, |m, frame| m.max(frame[0].abs()));
        assert!(
            out_peak > 0.05 * 1.5,
            "the front channels must be levelled, peak was {out_peak}"
        );
        // …and the LFE's detector slot must never have been written.
        assert_eq!(leveller.sc_prev_in[3], 0.0);
        assert_eq!(leveller.tone_lp_state[3], [0.0; 3]);
    }

    #[test]
    fn degenerate_buffers_are_handled_without_panicking() {
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_amount(MAX_AMOUNT);

        // Zero channels, and a buffer shorter than one frame, both reset the gain (:146-150).
        let mut empty: Vec<Real> = Vec::new();
        leveller.process(&mut empty, 0);
        assert_eq!(leveller.gain(), 1.0);
        leveller.process(&mut empty, 2);
        assert_eq!(leveller.gain(), 1.0);

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

    #[test]
    fn the_normaliser_is_disabled_at_zero_db_and_only_ever_attenuates() {
        // docs/spec/08-dsp-api.md §8.3 — target_rms == 1.0 disables the stage (Sos.cpp:46), which
        // is where the shipping GUI leaves it forever.
        let mut normaliser = Normaliser::new();
        assert!(!normaliser.is_enabled());
        assert_eq!(normaliser.gain_db(), 0.0);

        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        fill_sine(&mut buffer, 1000.0, 0.9, 0);
        let original = buffer.clone();
        normaliser.process(&mut buffer, 2);
        for (got, want) in buffer.iter().zip(original.iter()) {
            assert_eq!(got.to_bits(), want.to_bits());
        }

        // -12 dB is a target RMS of 0.2512.
        normaliser.set_gain_db(-12.0);
        assert!(normaliser.is_enabled());
        assert!((normaliser.target_rms - 0.251_19).abs() < 1e-4);

        // A signal already below the target must not be boosted: the gain is clamped to 1.0
        // (SosProcess.cpp:687-690), so the first buffer leaves a quiet input alone.
        fill_sine(&mut buffer, 1000.0, 0.05, 0);
        let quiet = buffer.clone();
        normaliser.process(&mut buffer, 2);
        for (got, want) in buffer.iter().zip(quiet.iter()) {
            assert_eq!(got.to_bits(), want.to_bits());
        }
        assert_eq!(normaliser.gain(), 1.0);
    }

    #[test]
    fn the_normaliser_halves_its_gap_on_release_and_attacks_over_about_twenty_seconds() {
        // SosProcess.cpp:699-711. The downward branch writes `smoothing_factor = 1` and calls
        // itself a fast release, but the `fmin(smoothing_factor, 0.5f)` at `:711` applies to
        // *both* branches, so the release is really a halving of the remaining gap per buffer —
        // fast, but not instantaneous. The upward branch is 0.0005 + diff*0.001, which that cap
        // never comes near. The spec's pseudocode (§8.3) has the same ordering; only its prose
        // rounds the release off to "instantaneous".
        let mut normaliser = Normaliser::new();
        normaliser.set_gain_db(-20.0); // target RMS 0.1

        let mut buffer = vec![0.0 as Real; BLOCK * 2];
        fill_sine(&mut buffer, 1000.0, 0.8, 0);
        normaliser.process(&mut buffer, 2);
        // Input RMS is 0.8/sqrt(2) = 0.5657, so the target gain is 0.1/0.5657 = 0.17678 and the
        // first buffer covers exactly half the distance to it from unity.
        let target_gain = 0.1 / (0.8 / (2.0 as Real).sqrt());
        let half_way = 1.0 + (target_gain - 1.0) * NORMALISATION_MAX_ALPHA;
        assert!(
            (normaliser.gain() - half_way).abs() < 1e-4,
            "the release moved to {}, expected the half-way point {half_way}",
            normaliser.gain()
        );

        // Twenty more buffers leave 2^-20 of the gap, i.e. the target to within a part in 10^6.
        for _ in 0..20 {
            fill_sine(&mut buffer, 1000.0, 0.8, 0);
            normaliser.process(&mut buffer, 2);
        }
        assert!(
            (normaliser.gain() - target_gain).abs() < 1e-4,
            "the release did not settle on {target_gain}: {}",
            normaliser.gain()
        );

        // Now go quiet. The gain must crawl back up: one buffer moves it by about
        // (1.0 - 0.1768) * (0.0005 + 0.8232*0.001) = 0.0011.
        let before = normaliser.gain();
        fill_sine(&mut buffer, 1000.0, 0.02, 0);
        normaliser.process(&mut buffer, 2);
        let step = normaliser.gain() - before;
        let expected = (1.0 - before)
            * (NORMALISATION_ATTACK_BASE + (1.0 - before) * NORMALISATION_ATTACK_SLOPE);
        assert!(step > 0.0, "the attack must be upward");
        assert!(
            (step - expected).abs() < 1e-6,
            "attack step {step}, expected {expected}"
        );

        // ~20 s at 10 ms buffers to get most of the way back (spec §8.3).
        for _ in 0..2000 {
            fill_sine(&mut buffer, 1000.0, 0.02, 0);
            normaliser.process(&mut buffer, 2);
        }
        assert!(
            normaliser.gain() > 0.8,
            "after 20 s the gain should be near unity, got {}",
            normaliser.gain()
        );
    }

    #[test]
    fn the_normaliser_is_stereo_only() {
        // SosProcess.cpp:679 guards on i_num_channels == 2; mono and surround are bypassed.
        let mut normaliser = Normaliser::new();
        normaliser.set_gain_db(-20.0);
        let mut mono = vec![0.8 as Real; BLOCK];
        let original = mono.clone();
        normaliser.process(&mut mono, 1);
        assert_eq!(mono, original);
        assert_eq!(normaliser.gain(), 1.0);
    }

    #[test]
    fn the_normaliser_never_attenuates_by_more_than_forty_decibels() {
        // The 0.01 floor at SosProcess.cpp:688 caps the stage at -40 dB however loud the input is.
        // -60 dB asks for a target RMS of 0.001, i.e. a gain of 0.001 against full-scale DC; the
        // clamp holds the *target* at 0.01 and the release then halves the gap towards it every
        // buffer (`:703`, `:711`), so the floor is approached from above and never crossed.
        let mut normaliser = Normaliser::new();
        normaliser.set_gain_db(-60.0);
        let mut buffer = vec![1.0 as Real; BLOCK * 2];

        for _ in 0..40 {
            buffer.fill(1.0);
            normaliser.process(&mut buffer, 2);
            assert!(
                normaliser.gain() >= NORMALISATION_MIN_GAIN,
                "the gain undershot the -40 dB floor: {}",
                normaliser.gain()
            );
        }
        assert!(
            (normaliser.gain() - NORMALISATION_MIN_GAIN).abs() < 1e-7,
            "the gain settled at {}, expected the -40 dB floor",
            normaliser.gain()
        );
        assert!(
            (buffer[0] - NORMALISATION_MIN_GAIN).abs() < 1e-7,
            "the output settled at {}, expected the -40 dB floor",
            buffer[0]
        );
    }
}
