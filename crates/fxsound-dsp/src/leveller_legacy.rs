//! The Windows build's Volume Leveling, for «Like FxSound for Windows» = Interface and sound.
//!
//! The arithmetic of `applyVolumeLeveling` (`SosProcess.cpp:139-492`) as the port ran it before the
//! 0.4.0 audit (`0f05ba5`), with audit items #1 to #4 taken back:
//!
//! * **#1** — the peak safety reads the 120 Hz side chain's peak, so bass is lifted past the
//!   ceiling and hard-clipped there;
//! * **#2** — one step of the state machine per call, whatever the quantum;
//! * **#3** — the subwoofer is neither analysed nor levelled;
//! * **#4** — both ends of the step's ramp are clamped to the peak-safe gain, so a transient late
//!   in a call pulls the gain down on the call's first sample.
//!
//! It is not a frozen copy: it runs inside the port's wrappers of today, which are the same at
//! every level. Switching the stage off lets the gain down over 20 ms (#11); a stage its owner
//! left out drops that glide ([`VolumeLeveller::sit_out`]); the detector does no subnormal
//! arithmetic in silence (#5); a non-finite statistic clears the stage. None of them moves a
//! sample of a running stage by more than rounding does, so the steady state is the Windows
//! build's: `scripts/windows-parity-bitexact.sh --compat=windows 3596e99` holds it to the engine
//! before the audit.
//!
//! The statistics and the decision are the port's own functions ([`VolumeLeveller::gather`],
//! [`VolumeLeveller::decide`], [`VolumeLeveller::finish_step`]): the audit did not change them,
//! and a step that is one whole call is exactly the original's buffer. What differs is here: the
//! peak the safety reads, the start of the ramp, and the gain pass. The tests hold the whole of it
//! to `0f05ba5`'s code, taken with `git show`, bit for bit on mono, stereo, 5.1 and 7.1.

use super::{
    DENORMAL_FLUSH, GainRamp, HEADROOM_NEAR_CEILING_THRESHOLD, Layout, StepStats, TINY,
    VolumeLeveller, clamp_real,
};
use crate::biquad::Real;

impl VolumeLeveller {
    /// One call of the Windows build's stage: a whole step, decided on this call's statistics and
    /// ramped across it (`SosProcess.cpp:139-492`).
    pub(super) fn process_windows(
        &mut self,
        buffer: &mut [Real],
        layout: &Layout,
        effective_sample_rate: Real,
    ) {
        let frames = buffer.len() / layout.channels;
        // One step per call (#2 taken back): whatever the port had gathered towards a step of
        // its own clock belongs to no call of this one.
        self.step = StepStats::default();
        let peak = self.gather(buffer, layout);
        self.flush_detector_denormals();
        // `SosProcess.cpp:561-577`, as the port has guarded it since before the audit: a
        // recursion that caught a non-finite value is cleared, and the call passes untouched.
        if !self.step.is_finite() {
            self.reset();
            return;
        }
        self.step.frames = frames;
        self.step.analysed_samples = frames * layout.analysed_channels;

        let decision = self.decide(effective_sample_rate, true);
        let sidechain_peak = self.step.sidechain_peak;

        // Peak safety on both ends of the ramp (`SosProcess.cpp:358-365`, #4 taken back), from the
        // side chain's peak (#1), then the cap on both (`:367-368`). `decide` has done the end.
        let mut gain_start = self.gain;
        if sidechain_peak > TINY {
            let peak_safe_gain = decision.effective_ceiling / sidechain_peak;
            if gain_start > peak_safe_gain {
                gain_start = peak_safe_gain;
            }
        }
        let gain_start = clamp_real(gain_start, 0.0, decision.max_gain_cap);
        let gain_end = decision.gain_end;
        let ceiling = decision.effective_ceiling;
        self.gain = gain_end;
        self.effective_ceiling = ceiling;

        // Levelled silence is not squared into the statistics (#5, which stays): the ramp is a
        // straight line, so its larger end bounds every frame's gain.
        let mut stats = self.step;
        if peak * gain_start.max(gain_end) >= DENORMAL_FLUSH {
            level_frames::<true>(buffer, layout, &mut stats, ceiling, (gain_start, gain_end));
        } else {
            level_frames::<false>(buffer, layout, &mut stats, ceiling, (gain_start, gain_end));
        }
        self.step = stats;

        self.finish_step(&decision);
        self.step = StepStats::default();
        // What is being played at the end of the call, for the let-down if the stage is switched
        // off (#11) and for the port's arithmetic if the level goes back to Off.
        self.ramp = GainRamp::hold(gain_end);
    }
}

/// Pass 2 of the Windows build (`SosProcess.cpp:371-400`): the ramp from `gain_start` to
/// `gain_end` across the call, the post-gain statistics of the analysed channels, and the hard
/// clip at the ceiling. The subwoofer is skipped altogether, gain and clip (#3 taken back).
///
/// `SQUARES` false leaves the post-gain sum of squares alone, for levelled silence (#5): a loop of
/// its own rather than a test inside it, because a compiler may square first and test after.
#[inline(always)]
fn level_frames<const SQUARES: bool>(
    buffer: &mut [Real],
    layout: &Layout,
    stats: &mut StepStats,
    effective_ceiling: Real,
    (gain_start, gain_end): (Real, Real),
) {
    let frames = (buffer.len() / layout.channels) as Real;
    let near_ceiling = effective_ceiling * HEADROOM_NEAR_CEILING_THRESHOLD;
    for (sample, frame) in buffer.chunks_exact_mut(layout.channels).enumerate() {
        let t = sample as Real / frames;
        let gain = gain_start + t * (gain_end - gain_start);
        for (channel, value) in frame.iter_mut().enumerate() {
            if Some(channel) == layout.lfe_channel {
                continue;
            }
            *value *= gain;
            if channel < layout.detector_channels {
                let post_gain_abs = value.abs();
                if SQUARES {
                    stats.post_gain_sum_squares += *value * *value;
                }
                if post_gain_abs > stats.post_gain_peak_abs {
                    stats.post_gain_peak_abs = post_gain_abs;
                }
                if post_gain_abs >= near_ceiling {
                    stats.ceiling_hit_count += 1;
                }
            }
            if *value > effective_ceiling {
                *value = effective_ceiling;
            } else if *value < -effective_ceiling {
                *value = -effective_ceiling;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CEILING, MAX_AMOUNT};
    use super::*;
    use fxsound_core::DspCompat;

    const FS: Real = 48_000.0;
    const BLOCK: usize = 480;

    /// A leveller at full amount playing `compat`.
    fn leveller(compat: DspCompat) -> VolumeLeveller {
        let mut leveller = VolumeLeveller::new(FS);
        leveller.set_compat(compat);
        leveller.set_amount(MAX_AMOUNT);
        leveller
    }

    /// `frames` frames of `channels` channels from frame `start` of `programme`, which gives each
    /// channel's sample for a frame.
    fn block(
        start: usize,
        frames: usize,
        channels: usize,
        programme: impl Fn(usize, usize) -> Real,
    ) -> Vec<Real> {
        (start..start + frames)
            .flat_map(|n| (0..channels).map(move |c| (n, c)))
            .map(|(n, c)| programme(n, c))
            .collect()
    }

    fn sine(hz: f64, amplitude: f64, n: usize) -> Real {
        (amplitude * (core::f64::consts::TAU * hz * n as f64 / f64::from(FS)).sin()) as Real
    }

    #[test]
    fn at_interface_and_sound_a_50_hz_tone_is_lifted_past_the_ceiling_and_clipped_as_on_windows() {
        // The twin of `a_50_hz_tone_is_held_under_the_ceiling_by_its_unfiltered_peak_rather_than_
        // clipped` (audit #1 taken back): the peak safety reads the 120 Hz side chain, which hears
        // a 50 Hz tone at 0.385 of its level, so a tone at 0.3 is lifted to x4.18 and the hard
        // clip flattens 41.8 % of its samples at full scale.
        const AMP: f64 = 0.3;
        let mut leveller = leveller(DspCompat::Windows);
        let (mut clipped, mut samples) = (0_usize, 0_usize);
        for index in 0..600 {
            let mut buffer = block(index * BLOCK, BLOCK, 2, |n, _| sine(50.0, AMP, n));
            leveller.process(&mut buffer, 2);
            assert!(buffer.iter().all(|sample| sample.abs() <= CEILING));
            if index >= 500 {
                clipped += buffer.iter().filter(|s| s.abs() == CEILING).count();
                samples += buffer.len();
            }
        }
        assert!(
            (4.1..4.3).contains(&leveller.gain()),
            "the tone rode x{}, was x4.18",
            leveller.gain()
        );
        let share = clipped as f64 / samples as f64;
        assert!(
            (0.40..0.44).contains(&share),
            "{share} of the samples clipped"
        );
    }

    /// How many times the gain the stage decided on changed over `frames` frames of a quiet tone
    /// being lifted, handed over in calls of `quantum` frames.
    fn decisions(compat: DspCompat, quantum: usize, frames: usize) -> usize {
        let mut leveller = leveller(compat);
        let mut changes = 0;
        let mut last = leveller.gain();
        let mut start = 0;
        while start < frames {
            let mut buffer = block(start, quantum, 2, |n, _| sine(300.0, 0.05, n));
            leveller.process(&mut buffer, 2);
            if leveller.gain() != last {
                changes += 1;
                last = leveller.gain();
            }
            start += quantum;
        }
        changes
    }

    #[test]
    fn at_interface_and_sound_the_state_machine_steps_once_a_call_as_on_windows() {
        // Audit #2 taken back: one step per call, so the speed of every smoothing constant follows
        // the quantum again. Over a second of a quiet tone being lifted the stage decides a
        // hundred times on the port's clock whatever the quantum (counted once a call, so on
        // quanta no longer than its step), and as often as it is called on the Windows build's.
        for quantum in [96, 240, 480] {
            assert_eq!(
                decisions(DspCompat::Linux, quantum, 48_000),
                100,
                "{quantum}"
            );
        }
        for quantum in [96, 240, 480, 1_200] {
            assert_eq!(
                decisions(DspCompat::Windows, quantum, 48_000),
                48_000 / quantum,
                "{quantum}"
            );
        }
    }

    #[test]
    fn at_interface_and_sound_the_subwoofer_is_left_at_unity_as_on_windows() {
        // The twin of `the_subwoofer_is_levelled_with_the_rest_but_left_out_of_the_statistics`
        // (audit #3 taken back): a quiet 5.1 scene is lifted by 13 dB and the subwoofer is not,
        // not a bit of it.
        const CHANNELS: usize = 6;
        const LFE: usize = 3;
        let mut leveller = leveller(DspCompat::Windows);
        let scene = |n: usize, channel: usize| {
            if channel == LFE {
                sine(50.0, 0.1, n)
            } else {
                sine(300.0, 0.05, n)
            }
        };
        let mut lifted = 0.0;
        for index in 0..400 {
            let dry = block(index * BLOCK, BLOCK, CHANNELS, scene);
            let mut wet = dry.clone();
            leveller.process_with_lfe(&mut wet, CHANNELS, Some(LFE));
            for (dry, wet) in dry
                .as_chunks::<CHANNELS>()
                .0
                .iter()
                .zip(wet.as_chunks::<CHANNELS>().0)
            {
                assert_eq!(dry[LFE].to_bits(), wet[LFE].to_bits(), "block {index}");
                if dry[0].abs() > 0.04 {
                    lifted = wet[0] / dry[0];
                }
            }
        }
        assert!(lifted > 4.0, "the fronts rode x{lifted}");
    }

    #[test]
    fn at_interface_and_sound_a_late_transient_pulls_the_gain_down_at_the_top_of_the_call_as_on_windows()
     {
        // The twin of `a_transient_late_in_a_step_pulls_the_gain_down_in_the_two_milliseconds_
        // before_it` (audit #4 taken back): a quiet passage lifted to about x4.5, then a burst at
        // ±0.9 from frame 400 of a 480-frame call. Both ends of the call's ramp are clamped to the
        // peak-safe gain, so the gain falls more than 12 dB between the last sample of the call
        // before and the first of this one, 8.3 ms before the hit. A DC pilot on a third channel
        // reads the gain on every sample.
        const PILOT: Real = 0.01;
        const HIT_BLOCK: usize = 418;
        let mut leveller = leveller(DspCompat::Windows);
        let programme = |n: usize, channel: usize| {
            if channel == 2 {
                PILOT
            } else if n >= HIT_BLOCK * BLOCK + 400 {
                if (n / 12).is_multiple_of(2) {
                    0.9
                } else {
                    -0.9
                }
            } else {
                sine(300.0, 0.05, n)
            }
        };
        let mut before = 0.0;
        for index in 0..=HIT_BLOCK {
            let mut buffer = block(index * BLOCK, BLOCK, 3, programme);
            leveller.process(&mut buffer, 3);
            let gains: Vec<Real> = buffer
                .as_chunks::<3>()
                .0
                .iter()
                .map(|f| f[2] / PILOT)
                .collect();
            if index < HIT_BLOCK {
                before = gains[BLOCK - 1];
            } else {
                assert!(before > 4.0, "the fixture must start boosted: x{before}");
                let fall = 20.0 * (before / gains[0]).log10();
                assert!(
                    fall > 12.0,
                    "the gain fell {fall} dB at the top of the call"
                );
            }
        }
    }

    #[test]
    fn the_arithmetic_changes_over_from_the_gain_being_played() {
        // A switch of level in the middle of a lifted passage carries on from the gain the
        // listener hears, either way: neither arithmetic starts again from unity.
        for (from, to) in [
            (DspCompat::Linux, DspCompat::Windows),
            (DspCompat::Windows, DspCompat::Linux),
        ] {
            let mut leveller = leveller(from);
            let mut last = Vec::new();
            for index in 0..400 {
                last = block(index * BLOCK, BLOCK, 2, |n, _| sine(300.0, 0.05, n));
                leveller.process(&mut last, 2);
            }
            let playing = leveller.gain();
            assert!(playing > 3.0, "{from:?}: the fixture must lift, x{playing}");
            leveller.set_compat(to);
            assert_eq!(leveller.compat(), to);
            let mut next = block(400 * BLOCK, BLOCK, 2, |n, _| sine(300.0, 0.05, n));
            leveller.process(&mut next, 2);
            // One cycle of the tone at each side of the switch: 160 frames.
            let peak = |samples: &[Real]| samples.iter().fold(0.0 as Real, |m, s| m.max(s.abs()));
            let before = peak(&last[last.len() - 320..]);
            let after = peak(&next[..320]);
            let step = 20.0 * (after / before).log10();
            assert!(
                step.abs() < 0.2,
                "{from:?} to {to:?}: {step} dB at the switch"
            );
        }
    }

    #[test]
    fn at_interface_and_sound_the_stage_is_the_original_bit_for_bit_on_every_layout() {
        // The Windows path is built from today's statistics and decision, not copied; this holds
        // it to `applyVolumeLeveling` as the port ran it before the audit (`git show
        // 0f05ba5:crates/fxsound-dsp/src/leveller.rs`, [`original::OriginalLeveller`]) on mono,
        // stereo, 5.1 and 7.1 with the subwoofer on channel 3, handed over in ragged calls. The
        // programme is twelve seconds of a quiet passage, long enough for the quiet boost to take
        // over, then a burst with bass the side chain barely hears, then the quiet again; the
        // subwoofer carries a loud 45 Hz line throughout, which would pull the gain down if it
        // were analysed and would be levelled if it were not left alone.
        const QUIET_UNTIL: usize = 12 * 48_000;
        const BURST_UNTIL: usize = 14 * 48_000;
        const END: usize = 16 * 48_000;
        const QUANTA: [usize; 9] = [480, 97, 1_024, 256, 1, 333, 2_048, 441, 7];
        let noise = |n: usize, channel: usize| {
            // A fixed hash of the frame and the channel: white noise in -1..1, the same each run.
            let mut x = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (channel as u64 + 1);
            x ^= x >> 29;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 32;
            ((x >> 40) as f64 / f64::from(1_u32 << 23) - 1.0) as Real
        };
        for (channels, lfe) in [(1, None), (2, None), (6, Some(3)), (8, Some(3))] {
            let programme = |n: usize, channel: usize| -> Real {
                if Some(channel) == lfe {
                    return sine(45.0, 0.6, n);
                }
                let voice = 220.0 + 110.0 * channel as f64;
                if (QUIET_UNTIL..BURST_UNTIL).contains(&n) {
                    sine(50.0, 0.6, n) + sine(voice, 0.2, n) + 0.1 * noise(n, channel)
                } else {
                    sine(voice, 0.015, n) + 0.004 * noise(n, channel)
                }
            };
            for amount in [MAX_AMOUNT, 1.5] {
                let mut port = VolumeLeveller::new(FS);
                port.set_compat(DspCompat::Windows);
                port.set_amount(amount);
                let mut windows = original::OriginalLeveller::new(FS);
                windows.set_amount(amount);
                let (mut start, mut call) = (0, 0);
                let (mut lifted, mut held): (Real, Real) = (1.0, Real::MAX);
                while start < END {
                    let frames = QUANTA[call % QUANTA.len()].min(END - start);
                    let mut ours = block(start, frames, channels, programme);
                    let mut theirs = ours.clone();
                    port.process_with_lfe(&mut ours, channels, lfe);
                    windows.process_excluding(&mut theirs, channels, lfe);
                    for (index, (a, b)) in ours.iter().zip(&theirs).enumerate() {
                        assert_eq!(
                            a.to_bits(),
                            b.to_bits(),
                            "{channels} channels at {amount}: frame {}, channel {}: {a} against \
                             {b}",
                            start + index / channels,
                            index % channels
                        );
                    }
                    assert_eq!(port.gain().to_bits(), windows.gain().to_bits());
                    if start < QUIET_UNTIL {
                        lifted = lifted.max(port.gain());
                    } else if start < BURST_UNTIL {
                        held = held.min(port.gain());
                    }
                    start += frames;
                    call += 1;
                }
                assert!(
                    lifted > 1.5,
                    "{channels} at {amount}: the quiet is lifted, x{lifted}"
                );
                assert!(
                    held < 0.7 * lifted,
                    "{channels} at {amount}: the burst pulls the gain down, x{held}"
                );
            }
        }
    }

    /// `applyVolumeLeveling` (`SosProcess.cpp:139-492`) as the port ran it before the 0.4.0 audit,
    /// taken line for line from `git show 0f05ba5:crates/fxsound-dsp/src/leveller.rs` (its
    /// comments left out): the reference the Windows path is held to, as
    /// [`crate::input::limiter::OriginalLimiter`] is Dynamic Boost's.
    mod original {
        use crate::biquad::{MAX_CHANNELS, Real};

        const CEILING: Real = 1.0;
        const ATTACK_ALPHA: Real = 0.10;
        const RELEASE_ALPHA_FAST: Real = 0.05;
        const RELEASE_ALPHA_SLOW: Real = 0.02;
        const RELEASE_GAP_THRESHOLD: Real = 0.15;
        const PREDICTION_STRENGTH: Real = 0.35;
        const PREDICTION_CLAMP: Real = 0.15;
        const PREDICTION_MISS_RATIO: Real = 0.35;
        const SIDECHAIN_HPF_HZ: Real = 120.0;
        const TONE_LOW_HZ: Real = 180.0;
        const TONE_BODY_HZ: Real = 1200.0;
        const TONE_PRESENCE_HZ: Real = 4500.0;
        const MIN_RATIO_PER_BUFFER: Real = 0.891_250_9;
        const TONALITY_DB_RANGE: Real = 7.0;
        const TONALITY_SMOOTHING: Real = 0.08;
        const MUFFLED_TARGET_BOOST: Real = 0.12;
        const CLEAR_TARGET_REDUCTION: Real = 0.18;
        const CLEAR_CEILING_REDUCTION: Real = 0.08;
        const HEADROOM_TIME_SECONDS: Real = 60.0;
        const HEADROOM_TARGET_BOOST: Real = 0.08;
        const HEADROOM_TARGET_REDUCTION: Real = 0.14;
        const HEADROOM_COMFORT_THRESHOLD: Real = 0.18;
        const HEADROOM_NEAR_CEILING_THRESHOLD: Real = 0.985;
        const HEADROOM_HIT_THRESHOLD: Real = 0.002;
        const VERY_QUIET_RMS_THRESHOLD: Real = 0.035;
        const QUIET_AUDIBLE_PEAK_THRESHOLD: Real = 0.0035;
        const QUIET_FULL_BOOST_PEAK: Real = 0.02;
        const QUIET_MAX_GAIN: Real = 10.0;
        const QUIET_RELEASE_ALPHA: Real = 0.18;
        const QUIET_ACTIVATION_SECONDS: Real = 10.0;
        const QUIET_ACTIVATION_RAMP_SECONDS: Real = 2.0;
        const QUIET_FLOOR_RELEASE_RMS_THRESHOLD: Real = 0.06;
        const QUIET_FLOOR_RELEASE_ALPHA: Real = 0.02;
        const QUIET_FLOOR_SILENCE_DECAY_ALPHA: Real = 0.08;
        const QUIET_PEAK_BUCKET_SECONDS: Real = 1.0;
        const QUIET_PEAK_TARGET_RATIO: Real = 0.98;
        const QUIET_PEAK_FLOOR_RAISE_TIME_SECONDS: Real = 6.0;
        const HISTORY_SIZE: usize = 6;
        const PEAK_WINDOW_SIZE: usize = 30;
        const PI: Real = core::f32::consts::PI;
        const MAX_AMOUNT: Real = 4.0;
        const MAX_TARGET_RMS: Real = 0.5;
        const MAX_GAIN_CAP_DIVISOR: Real = 0.125;
        const MIN_EFFECTIVE_CEILING: Real = 0.92;
        const AIR_WEIGHT: Real = 0.75;
        const BODY_WEIGHT: Real = 1.15;
        const LOW_WEIGHT: Real = 0.85;
        const TONALITY_EPSILON: Real = 1e-12;
        const AUTHORITY_REDUCE_WEIGHT: Real = 1.5;
        const AUTHORITY_BOOST_WEIGHT: Real = 0.25;
        const AUTHORITY_DOWNWARD_FLOOR: Real = 0.35;
        const RELEASE_MUFFLED_WEIGHT: Real = 0.20;
        const RELEASE_CLEAR_WEIGHT: Real = 0.35;
        const RELEASE_TONAL_MIN: Real = 0.55;
        const RELEASE_TONAL_MAX: Real = 1.20;
        const MISS_THRESHOLD_RATIO: Real = 0.01;
        const MISS_THRESHOLD_FLOOR: Real = 1e-5;
        const HEADROOM_HIT_SATURATION: Real = 0.02;
        const HEADROOM_COMFORT_SPAN: Real = 0.25;
        const SLOW_ALPHA_MIN: Real = 0.0005;
        const SLOW_ALPHA_MAX: Real = 0.05;
        const QUIET_FLOOR_SNAP: Real = 1.0001;
        const TINY: Real = 1e-6;
        const MIN_PLAUSIBLE_SAMPLE_RATE: Real = 1000.0;
        const FALLBACK_SAMPLE_RATE: Real = 48_000.0;

        fn clamp_real(value: Real, min_value: Real, max_value: Real) -> Real {
            let upper = if value < max_value { value } else { max_value };
            if min_value < upper { upper } else { min_value }
        }

        fn one_pole_alpha(cutoff_hz: Real, sample_rate: Real) -> Real {
            let dt = 1.0 / sample_rate;
            let rc = 1.0 / (2.0 * PI * cutoff_hz);
            dt / (rc + dt)
        }

        fn high_pass_alpha(cutoff_hz: Real, sample_rate: Real) -> Real {
            let dt = 1.0 / sample_rate;
            let rc = 1.0 / (2.0 * PI * cutoff_hz);
            rc / (rc + dt)
        }

        fn target_rms_for_amount(amount: Real) -> Real {
            let control = clamp_real(amount, 0.0, MAX_AMOUNT);
            (control / MAX_AMOUNT) * MAX_TARGET_RMS
        }

        fn tonality_db(low: Real, body: Real, presence: Real, air: Real) -> Real {
            let numerator = presence + air * AIR_WEIGHT + TONALITY_EPSILON;
            let denominator = body * BODY_WEIGHT + low * LOW_WEIGHT + TONALITY_EPSILON;
            (10.0 * f64::from(numerator / denominator).log10()) as Real
        }

        fn tonality_score(tonality_db: Real) -> Real {
            clamp_real(tonality_db / TONALITY_DB_RANGE, -1.0, 1.0)
        }

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

        pub(super) struct OriginalLeveller {
            target_rms: Real,
            sample_rate: Real,

            gain: Real,
            power_history: [Real; HISTORY_SIZE],
            power_sum: Real,
            power_index: usize,
            power_count: usize,
            previous_average_rms: Real,
            previous_predicted_rms: Real,

            alpha_sample_rate: Real,
            sc_hpf_alpha: Real,
            tone_low_alpha: Real,
            tone_body_alpha: Real,
            tone_presence_alpha: Real,

            sc_prev_in: [Real; MAX_CHANNELS],
            sc_prev_out: [Real; MAX_CHANNELS],
            tone_lp_state: [[Real; 3]; MAX_CHANNELS],

            tonality_score: Real,
            headroom_score: Real,
            quiet_duration_seconds: Real,
            quiet_gain_floor: Real,
            quiet_peak_history: [Real; PEAK_WINDOW_SIZE],
            quiet_peak_bucket_max: Real,
            quiet_peak_bucket_seconds: Real,
            quiet_peak_history_index: usize,
            quiet_peak_history_count: usize,
        }

        impl OriginalLeveller {
            pub(super) fn new(sample_rate: Real) -> Self {
                Self {
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

            pub(super) fn set_amount(&mut self, amount: Real) {
                let amount = clamp_real(amount, 0.0, MAX_AMOUNT);
                self.target_rms = target_rms_for_amount(amount);
                if self.target_rms <= 0.0 {
                    self.reset();
                }
            }

            pub(super) const fn gain(&self) -> Real {
                self.gain
            }

            fn reset(&mut self) {
                *self = Self {
                    target_rms: self.target_rms,
                    ..Self::new(self.sample_rate)
                };
            }

            pub(super) fn process_excluding(
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

                let detector_channels = channels.min(MAX_CHANNELS);
                let analysed_channels = (0..detector_channels)
                    .filter(|c| Some(*c) != excluded_channel)
                    .count();
                if analysed_channels == 0 {
                    return;
                }
                let analysed_samples = (frames * analysed_channels) as Real;

                let effective_sample_rate = if self.sample_rate > MIN_PLAUSIBLE_SAMPLE_RATE {
                    self.sample_rate
                } else {
                    FALLBACK_SAMPLE_RATE
                };
                self.update_coefficients(effective_sample_rate);

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

                if !(sum_squares.is_finite()
                    && peak.is_finite()
                    && low_energy.is_finite()
                    && body_energy.is_finite()
                    && presence_energy.is_finite()
                    && air_energy.is_finite())
                {
                    self.reset();
                    return;
                }

                let current_power = sum_squares / analysed_samples;
                let current_rms = current_power.sqrt();
                let mut gain_start = self.gain;
                let mut gain_end = gain_start;
                let headroom_reduce_score = self.headroom_score.max(0.0);
                let headroom_boost_score = (-self.headroom_score).max(0.0);
                let mut quiet_gain_floor = self.quiet_gain_floor.max(1.0);
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
                    AUTHORITY_DOWNWARD_FLOOR
                        + tonal_upward_authority * (1.0 - AUTHORITY_DOWNWARD_FLOOR),
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
                        let overshot =
                            actual_delta.abs() < (predicted_delta.abs() * PREDICTION_MISS_RATIO);
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

                if current_rms > TINY {
                    let quiet_activation_score = clamp_real(
                        (quiet_duration_before - QUIET_ACTIVATION_SECONDS)
                            / QUIET_ACTIVATION_RAMP_SECONDS,
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
                    let quiet_boost_score =
                        quiet_rms_score * audible_peak_score * quiet_activation_score;
                    if quiet_boost_score > 0.0 {
                        let quiet_gain_cap = max_gain_cap.max(QUIET_MAX_GAIN);
                        max_gain_cap += (quiet_gain_cap - max_gain_cap) * quiet_boost_score;
                    }

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

                if gain_end < gain_start {
                    let min_allowed_gain_end = gain_start * MIN_RATIO_PER_BUFFER;
                    if gain_end < min_allowed_gain_end {
                        gain_end = min_allowed_gain_end;
                    }
                }

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

                let quiet_boost_had_authority = quiet_duration_before >= QUIET_ACTIVATION_SECONDS
                    && gain_end > nominal_gain_cap;
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
                if quiet_peak_window_ready && quiet_floor_is_active && sustained_headroom_available
                {
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

            fn update_quiet_peak_window(
                &mut self,
                post_gain_peak_abs: Real,
                buffer_duration_seconds: Real,
            ) {
                self.quiet_peak_bucket_max = self.quiet_peak_bucket_max.max(post_gain_peak_abs);
                self.quiet_peak_bucket_seconds += buffer_duration_seconds;

                while self.quiet_peak_bucket_seconds >= QUIET_PEAK_BUCKET_SECONDS {
                    self.quiet_peak_history[self.quiet_peak_history_index] =
                        self.quiet_peak_bucket_max;
                    self.quiet_peak_history_index =
                        (self.quiet_peak_history_index + 1) % PEAK_WINDOW_SIZE;
                    if self.quiet_peak_history_count < PEAK_WINDOW_SIZE {
                        self.quiet_peak_history_count += 1;
                    }
                    self.quiet_peak_bucket_seconds -= QUIET_PEAK_BUCKET_SECONDS;
                    self.quiet_peak_bucket_max = 0.0;
                }
            }

            fn quiet_peak_window_max(&self) -> Real {
                let mut rolling_peak_max = self.quiet_peak_bucket_max;
                for bucket in &self.quiet_peak_history[..self.quiet_peak_history_count] {
                    rolling_peak_max = rolling_peak_max.max(*bucket);
                }
                rolling_peak_max
            }
        }
    }
}
