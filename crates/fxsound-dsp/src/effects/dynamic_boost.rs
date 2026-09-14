//! Dynamic Boost — a slow auto-gain feeding a look-ahead brick-wall peak limiter.
//!
//! Ports `dsp/ptechDsp/Maximizer/Maxi32/Maxi32.c:237-482` (`dspsMaximizerProcess32`) with the
//! struct and constants from `dsp/ptutil/include/c_max.h:47-127`. The inner loop is short but
//! strongly order-dependent — the envelope reads the delay line *before* the new sample is
//! written, and the ramp bookkeeping has to happen in exactly the original's order or the
//! look-ahead stops being exact — so it is transcribed statement for statement.
//!
//! Three stages, all driven from one pass over the block:
//!
//! ```text
//!  in[0] ──► level += (in[0]² − level)·filt_gain      0.1 Hz one-pole, τ ≈ 1.59 s, f64
//!              │
//!              ├─► rms = sqrt(level)
//!              │      if gain_boost·rms > 0.32 { gain = max(0.32/rms, 1.06) } else { gain_boost }
//!              ▼
//!  in[ch] ──► delay line, max_delay frames ──► env follower ──► ×(max_output/env) when env > ceiling
//!             write: gain·max_output·in[ch]     linear attack ramp over max_delay frames,
//!             read : the frame from max_delay    exponential release β (τ ≈ 10.18 ms)
//!                    frames ago
//! ```
//!
//! Two properties fall out of that and are what the tests pin down:
//!
//! * **The output can never exceed [`MAX_OUTPUT`].** Both envelope branches raise `env` to at
//!   least `|delayed|` before the output is formed, so `delayed·max_output/env` is bounded by
//!   `max_output`, and the `env <= max_output` branch passes a sample that is already smaller.
//! * **A transient is never clipped on the way in.** The envelope starts ramping the moment the
//!   loud sample is *written*, and reaches it after exactly `max_delay + 1` increments — which is
//!   the same moment that sample reaches the read pointer. That is the entire point of the delay,
//!   and it is why this effect reports latency.
//!
//! # This effect is never bypassed
//!
//! [`Effect::is_active`] returns `true` unconditionally. `dspsPlayProcess32` calls the maximizer
//! outside every bypass test (`dsp/ptechDsp/Play/Play32/Play32.c:867-876`) with the comment
//! *"Note that optimizer is never bypassed, the output gain is set to unity when the process
//! switch on the UI is not selected."* It is the safety net that stops Surround — which gives away
//! nearly 6 dB at maximum — and Bass from clipping the output, so switching it off with the other
//! effects would make the chain *less* safe, not more transparent.
//!
//! Two consequences the port keeps deliberately, because removing either changes what FxSound
//! sounds like:
//!
//! * Whenever the master power is on, everything passes through `×0.966051` = **−0.3 dBFS**
//!   (`Maxi32.c:297`, `Play32.c:411`), even with every slider at zero.
//! * Material loud enough to trip the auto-gain back-off gets **at least ×1.06** (+0.5 dB), even
//!   at slider 0, because of the anti-pumping clamp at `Maxi32.c:288-289`. Quiet material at
//!   slider 0 really is −0.3 dB; loud material at slider 0 is +0.2 dB and limited.
//!
//! # What is not ported
//!
//! * The dither/quantizer block (`Maxi32.c:483-582`) is dead: `quantize_on_flag` sits in a
//!   zero-initialised parameter slot that no `dfxp*` code ever writes (spec §9.7).
//! * `kerWetDry` (`Maxi32.c:591`) is a no-op here — the Play host sets `wet = 1.0, dry = 0.0`
//!   for this block (`Play32.c:407-408`).
//! * The `sp_meters->aux_vals` peak reporting (`Maxi32.c:605-617`) is compiled out in the DFX
//!   build. [`DynamicBoost::envelope`] and [`DynamicBoost::level_rms`] expose the same information
//!   to a Linux meter without a second code path in the loop.

use super::Effect;
use crate::biquad::{MAX_CHANNELS, Real};

/// The permanent output ceiling, `MAXIMIZE_MAX_OUTPUT` (`Maxi32.c:91`, `Play32.c:411`).
///
/// −0.30 dBFS. The PC side never writes this slot — grep `dsp/ptutil/dfxp/` for
/// `MAXIMIZE_MAX_OUTPUT` and there is nothing — so it keeps its initialiser forever.
pub const MAX_OUTPUT: Real = 0.966_051;

/// `MAXI_LOOK_AHEAD_DELAY` (`c_max.h:49`) — 0.75 ms of look-ahead.
pub const LOOK_AHEAD_SECONDS: Real = 0.000_75;

/// Look-ahead capacity, in frames, for the highest rate this port supports (192 kHz).
///
/// The original reserves `MAXI_MAX_DELAY_LEN = 96` (`c_max.h:47`) because it caps its internal
/// rate at 48 kHz and decimates anything higher (spec §2). This port runs at the native rate
/// instead, so the buffer is sized for 192 kHz · 0.75 ms = 144 frames; anything faster is clamped
/// to this length rather than overrunning, which is the failure the original's fixed 96 would
/// have had at 176.4 kHz (spec §9.1, risk R6).
pub const MAX_LOOK_AHEAD_FRAMES: usize = 144;

/// `MAXIMIZE_TARGET_LEVEL_SETTING` (`c_max.h:53`) — the RMS the auto-gain aims at.
///
/// Also written explicitly by `dfxp_CommunicateFixedQnts_Opt()` (`dfxpComm.cpp:1018-1026`) with
/// the same value, so it is 0.32 from both directions.
const TARGET_LEVEL: Real = 0.32;

/// The anti-pumping floor from `Maxi32.c:288-289` ("11/4/04 Modifications to help fix volume
/// pumping"). Once the back-off engages it may not reduce the boost below +0.5 dB.
const MIN_BACKOFF_GAIN: Real = 1.06;

/// `MAXI_ENVELOPE_BIAS` (`c_max.h:48`) — keeps the release recursion out of f32 denormals.
const ENVELOPE_BIAS: Real = 1.0e-24;

/// `MAXIMIZE_LEVEL_FILT_CUTOFF` (`c_max.h:57`) — the level estimator's corner, in Hz.
const LEVEL_FILT_CUTOFF: f64 = 0.1;

/// The literal `2π` the original's filter design uses (`Maxi32.c:125`).
///
/// Deliberately not `std::f64::consts::TAU`: the original is truncated at seven digits and the
/// resulting `a0` differs in the ninth decimal, which is the sort of thing that makes a
/// bit-comparison against the C code fail for no useful reason. Hence the `allow` — the
/// difference from the real constant is the point.
#[allow(clippy::approx_constant)]
const TWO_PI: f64 = 6.283_185;

/// Below this the level estimate is flushed to zero — see [`DynamicBoost::update_gain`].
const LEVEL_FLOOR: f64 = 1.0e-30;

/// `DFXP_MUSIC_MODE2_DYNAMIC_BOOST_FACTOR` (`dfxpDefs.h:129`), applied at `dfxpComm.cpp:709-717`.
///
/// MUSIC2 is the only mode FxSound 13 ships (`DfxDspPreset.cpp:239-242`: *"As of DFX Version 13,
/// we only allow music mode 2"*), so the warp is folded into the mapping here rather than being a
/// separate host-level stage. SPEECH uses the same 1.8 (`dfxpDefs.h:130`); MUSIC1 would be 1.0 and
/// would make the slider monotonic instead of saturating at 6.
const MUSIC2_BOOST_FACTOR: Real = 1.8;

/// `PLY_OPTIMIZER_BOOST_MAX_SCALE` (`c_play.h:90`), applied at `dfxpComm.cpp:742`.
const BOOST_MAX_SCALE: Real = 0.7;

/// `DSP_PLAY_MAX_RELEASE_TIME_BETA_MIDI` (`c_play.h:66`) — the release time is fixed, not exposed.
const RELEASE_TIME_MIDI: f64 = 85.0;

/// `MAXIMIZE_MIN_TIME_CONST` / `MAXIMIZE_MAX_TIME_CONST` (`c_max.h:38-39`), the ends of the
/// exponential release-time table built at `dfxpQnt.cpp:409-416`.
const MIN_TIME_CONST_MS: f64 = 0.1;
const MAX_TIME_CONST_MS: f64 = 100.0;

/// Used when a caller hands over a sample rate that is not a usable number.
const FALLBACK_SAMPLE_RATE: Real = 48_000.0;

/// Per-channel limiter state (`c_max.h:101-112`, the `_l`/`_r` pairs).
#[derive(Clone, Copy, Debug, PartialEq)]
struct ChannelState {
    /// Index of the delay-line slot written next, `s->ptr_l` as an offset.
    write: usize,
    /// The peak envelope, `s->env_l`.
    env: Real,
    /// Per-sample increment of the attack ramp, `s->delta_l`. Never reset when a ramp ends —
    /// the original leaves it standing until the next ramp assigns a fresh value.
    delta: Real,
    /// The peak the current ramp is climbing towards, `s->max_abs_l`.
    max_abs: Real,
    /// Frames left in the attack ramp, `s->ramp_count_l`.
    ramp_count: usize,
}

impl ChannelState {
    const SILENT: Self = Self {
        write: 0,
        env: 0.0,
        delta: 0.0,
        max_abs: 0.0,
        ramp_count: 0,
    };
}

/// The maximizer: auto-gain plus look-ahead brick-wall peak limiter.
///
/// Allocates one delay buffer in [`DynamicBoost::new`], sized for 192 kHz and eight channels
/// (8 × 144 floats = 4.6 kB), and nothing afterwards. The block size does not enter into it: the
/// effect works a frame at a time in place, so a 16384-frame block costs no more state than a
/// 64-frame one.
#[derive(Clone, Debug)]
pub struct DynamicBoost {
    sample_rate: Real,
    amount: Real,
    /// `s->gain_boost` — the static boost before any back-off.
    gain_boost: Real,
    /// `s->max_delay` — look-ahead length in frames, and therefore the reported latency.
    max_delay: usize,
    /// `s->release_time_beta`.
    release_time_beta: Real,
    /// `s->level`, `s->a0`, `s->filt_gain`. f64 in the original too (`c_max.h:123-125`), with a
    /// comment saying f32 had "a normalization problem" — at `a0 ≈ 0.99999` it does: the f32 gap
    /// at 1.0 is 6e-8, so `1 − a0` loses most of its significant digits.
    level: f64,
    a0: f64,
    filt_gain: f64,
    /// `MAX_CHANNELS` delay lines laid end to end, each [`MAX_LOOK_AHEAD_FRAMES`] long.
    delay: Box<[Real]>,
    state: [ChannelState; MAX_CHANNELS],
}

impl DynamicBoost {
    /// Build a maximizer for `sample_rate`, with the boost knob at zero.
    ///
    /// This is the only place that allocates.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let mut effect = Self {
            sample_rate: FALLBACK_SAMPLE_RATE,
            amount: 0.0,
            gain_boost: 1.0,
            max_delay: 1,
            release_time_beta: 0.0,
            level: 0.0,
            a0: 0.0,
            filt_gain: 0.0,
            delay: vec![0.0; MAX_CHANNELS * MAX_LOOK_AHEAD_FRAMES].into_boxed_slice(),
            state: [ChannelState::SILENT; MAX_CHANNELS],
        };
        effect.set_sample_rate(sample_rate);
        effect.set_amount(0.0);
        effect
    }

    /// The static boost, linear, before the auto-gain backs it off — `s->gain_boost`.
    ///
    /// 1.0 at slider 0 and 3.8019 (+11.6 dB) from slider 6 upwards; see
    /// [`gain_boost_for_amount`].
    #[must_use]
    pub const fn gain_boost(&self) -> Real {
        self.gain_boost
    }

    /// The current level estimate as an RMS, `sqrt(s->level)`.
    ///
    /// Tracks the left channel only, with a ~1.59 s time constant. Useful for a UI meter that
    /// wants to show why the boost is backing off; it is the same number the loop uses.
    #[must_use]
    pub fn level_rms(&self) -> Real {
        self.level.sqrt() as Real
    }

    /// The peak envelope for one channel, or 0.0 for a channel this instance does not track.
    ///
    /// Gain reduction in dB is `20·log10(MAX_OUTPUT / env)` while `env > MAX_OUTPUT`, and zero
    /// otherwise.
    #[must_use]
    pub fn envelope(&self, channel: usize) -> Real {
        self.state.get(channel).map_or(0.0, |state| state.env)
    }

    /// The one-pole level estimator's feedback coefficient, `s->a0`.
    #[must_use]
    pub const fn level_filter_a0(&self) -> f64 {
        self.a0
    }

    /// The envelope release coefficient, `s->release_time_beta`.
    #[must_use]
    pub const fn release_beta(&self) -> Real {
        self.release_time_beta
    }

    /// Recompute everything that depends on the sample rate (`Maxi32.c:119-137`,
    /// `dfxpComm.cpp:765`, `Qntitor2.cpp:540-546`).
    fn design(&mut self) {
        let fs = f64::from(self.sample_rate);

        // Single-pole lowpass placed by matching the pole to the desired corner. The original's
        // own comment notes 0.01 Hz is the floor for a single-precision design, which is why the
        // whole calculation — and the state it produces — is f64.
        let omega = TWO_PI * LEVEL_FILT_CUTOFF / fs;
        let cos_om = omega.cos();
        let root = (cos_om * cos_om - 4.0 * cos_om + 3.0).sqrt();
        self.a0 = 2.0 - cos_om - root;
        self.filt_gain = 1.0 - self.a0;

        // The release time is a fixed MIDI 85 into an exponential 0.1…100 ms table. The original
        // builds that table by repeated f32 multiplication (`Qntitor.cpp:302-321`); evaluating the
        // closed form in f64 gives 10.182960 ms against the table's 10.182953 ms, which moves beta
        // by less than 1e-8 — far below the f32 it is stored in.
        let factor = (MAX_TIME_CONST_MS / MIN_TIME_CONST_MS).powf(RELEASE_TIME_MIDI / 127.0);
        let time_constant_ms = (MIN_TIME_CONST_MS * factor) as Real;
        // `qntIToRTimeConstantBeta` does this arithmetic in realtype (f32), so so does this.
        let exp_arg = 1.0 / (time_constant_ms * 0.001 * self.sample_rate);
        self.release_time_beta = f64::from(-exp_arg).exp() as Real;

        // `(int)(internal_sampling_freq * MAXI_LOOK_AHEAD_DELAY)` — a truncating cast in f32, so
        // 44 100 gives 33 and 48 000 gives 36. Clamped at both ends: zero would collapse the
        // look-ahead, and the top is the buffer this instance owns.
        let frames = (self.sample_rate * LOOK_AHEAD_SECONDS) as usize;
        self.max_delay = frames.clamp(1, MAX_LOOK_AHEAD_FRAMES);
    }

    /// One sample of the level estimator plus the auto-gain back-off (`Maxi32.c:258-294`).
    ///
    /// `input` is the **left** channel only, which is the original's documented asymmetry: a
    /// hard-left-panned loud track drives the gain for every channel. See the note on
    /// [`Effect::process`].
    #[inline]
    fn update_gain(&mut self, input: Real) -> Real {
        // `float in_sqr = in1 * in1;` — squared in f32 and only then widened, so a −190 dBFS
        // input squares to zero here exactly as it does in the original.
        let in_sqr: Real = input * input;
        self.level = self.level * self.a0 + f64::from(in_sqr) * self.filt_gain;

        // Deviation from the original, for real-time safety: with a truly silent input `level`
        // decays geometrically with nothing to stop it and eventually reaches f64 denormals,
        // where the multiply can cost hundreds of cycles on the audio thread. The envelope has
        // `MAXI_ENVELOPE_BIAS` for exactly this reason; the level estimator was never given one.
        // The floor sits at −300 dBFS, which is 250 dB below anything audible and 150 dB below a
        // 24-bit LSB, so it cannot change the gain the loop computes.
        if self.level < LEVEL_FLOOR {
            self.level = 0.0;
        }

        let rms = self.level.sqrt() as Real;
        if self.gain_boost * rms > TARGET_LEVEL {
            // `rms` is necessarily above `TARGET_LEVEL / gain_boost` to get here, so it is far
            // from zero and this division is safe.
            let backed_off = TARGET_LEVEL / rms;
            if backed_off < MIN_BACKOFF_GAIN {
                MIN_BACKOFF_GAIN
            } else {
                backed_off
            }
        } else {
            self.gain_boost
        }
    }
}

/// Map the user's `0.0..=1.0` knob to the linear `gain_boost` the loop multiplies by.
///
/// The full chain from `dfxp_CommunicateDynamicBoost()` (`dfxpComm.cpp:686-779`):
///
/// ```text
/// midi   = round(amount · 127)                        Qntrtoi.cpp:96-97
/// midi   = min((int)(1.8 · midi), 127)                dfxpComm.cpp:709-721   (MUSIC2)
/// index  = (int)(midi · 0.7)                          dfxpComm.cpp:742
/// dB     = maxi_boost_db(index)                       Qntitor.cpp:458-517
/// gain   = 10^(dB/20)                                 Qntitor.cpp:511-517
/// ```
///
/// The 1.8 warp followed by the MIDI clamp is why **sliders 6 through 10 are identical** — every
/// one of them saturates at index 88, +11.6 dB. That is shipped behaviour, not a porting bug.
#[must_use]
pub fn gain_boost_for_amount(amount: Real) -> Real {
    let midi = fxsound_core::scale::value_to_midi(amount.clamp(0.0, 1.0));

    // Both casts truncate, matching C's `(int)`. `as` saturates in Rust, so neither can trap.
    let warped = ((MUSIC2_BOOST_FACTOR * Real::from(midi)) as i32).min(127);
    let index = ((warped as Real * BOOST_MAX_SCALE) as i32).clamp(0, 127) as usize;

    let db = maxi_boost_db(index);
    // `pow(10.0, (double)table[i] / 20.0)` then stored back into a realtype.
    10.0_f64.powf(f64::from(db) / 20.0) as Real
}

/// The `QNT_RESPONSE_MAXI_BOOST_DSP` table in closed form (`Qntitor.cpp:458-517`).
///
/// The original fills a 128-entry array with three `while` loops; the piecewise expression below
/// reproduces it entry for entry, including the flat top where index 126 is overwritten with
/// index 127's hard-set 30 dB.
#[must_use]
fn maxi_boost_db(index: usize) -> Real {
    if index >= 126 {
        30.0
    } else if index >= 90 {
        // 0.5 dB per step from 12.0 dB.
        0.5 * (index - 90) as Real + 12.0
    } else if index >= 60 {
        // 0.2 dB per step from 6.0 dB.
        0.2 * (index - 60) as Real + 6.0
    } else {
        // 0.1 dB per step from 0.0 dB.
        0.1 * index as Real
    }
}

impl Effect for DynamicBoost {
    fn set_sample_rate(&mut self, sample_rate: Real) {
        self.sample_rate = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            FALLBACK_SAMPLE_RATE
        };
        self.design();
        // The delay line now means something different, and the envelope's ramp was measured in
        // the old rate's frames.
        self.reset();
    }

    fn set_amount(&mut self, amount: Real) {
        self.amount = amount.clamp(0.0, 1.0);
        self.gain_boost = gain_boost_for_amount(self.amount);
    }

    fn amount(&self) -> Real {
        self.amount
    }

    /// Always `true`.
    ///
    /// The maximizer is the one effect the original never bypasses (`Play32.c:867-876`); at a
    /// zero knob it still applies the −0.3 dB ceiling and still catches whatever Surround and Bass
    /// pushed past full scale. See the module documentation.
    fn is_active(&self) -> bool {
        true
    }

    fn reset(&mut self) {
        self.delay.fill(0.0);
        self.state = [ChannelState::SILENT; MAX_CHANNELS];
        self.level = 0.0;
    }

    /// `Maxi32.c:237-482`, one frame at a time, in place.
    ///
    /// Channels are independent apart from the auto-gain, which the original derives from the left
    /// channel alone (`Maxi32.c:258-259`) and applies to both. This port keeps that: channel 0
    /// drives the estimator, every channel gets its own delay line and envelope. On a stream with
    /// more than [`MAX_CHANNELS`] channels the extra channels are passed through untouched rather
    /// than panicking — the original never sees one, since its host splits multichannel streams
    /// into per-pair instances (spec §2).
    fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || buffer.is_empty() {
            return;
        }

        let max_delay = self.max_delay.max(1);
        // "Note that since envelope ramping starts immediately on this sample, divisor of delta
        // calc is delay plus one" (`Maxi32.c:323-326`). Off by one here and the ramp lands early
        // or late, so the gain still steps at the transient instead of arriving already reduced —
        // the ceiling holds either way, the smoothness does not.
        let ramp_divisor = max_delay as Real + 1.0;
        let beta = self.release_time_beta;

        for frame in buffer.chunks_exact_mut(channels) {
            let input = frame.first().copied().unwrap_or(0.0);
            let boost = self.update_gain(input) * MAX_OUTPUT;

            // One fixed-length delay line per channel. Zipping against `frame` stops at whichever
            // runs out first, which is how channels beyond MAX_CHANNELS end up untouched without
            // an index or a branch.
            let (lines, _) = self.delay.as_chunks_mut::<MAX_LOOK_AHEAD_FRAMES>();
            for ((state, line), sample) in self
                .state
                .iter_mut()
                .zip(lines.iter_mut())
                .zip(frame.iter_mut())
            {
                // Look-ahead delay: read the frame written `max_delay` frames ago, then overwrite
                // that slot with the boosted input (`Maxi32.c:296-301`).
                let Some(slot) = line.get_mut(state.write) else {
                    continue;
                };
                let delayed = *slot;
                let boosted = boost * *sample;
                *slot = boosted;
                let new_abs = boosted.abs();
                state.write += 1;
                if state.write >= max_delay {
                    state.write = 0;
                }

                if state.ramp_count != 0 {
                    // Attack ramp in progress (`Maxi32.c:304-336`).
                    let abs_out = delayed.abs();
                    if abs_out > state.env {
                        state.env = abs_out;
                    }
                    if new_abs > state.max_abs {
                        // A louder peak arrived mid-ramp: retarget and restart the countdown, but
                        // only steepen the slope, never flatten it — flattening would let the
                        // previous peak through unlimited.
                        state.max_abs = new_abs;
                        state.ramp_count = max_delay;
                        let tmp_delta = (new_abs - state.env) / ramp_divisor;
                        if tmp_delta > state.delta {
                            state.delta = tmp_delta;
                        }
                    } else {
                        state.ramp_count -= 1;
                    }
                    state.env += state.delta;
                } else {
                    // Release (`Maxi32.c:338-362`). The bias keeps the recursion off denormals.
                    state.env = state.env * beta + ENVELOPE_BIAS;
                    let abs_out = delayed.abs();
                    if abs_out > state.env {
                        state.env = abs_out;
                    }
                    if new_abs > state.env {
                        // Start a ramp that lands on `new_abs` exactly as it leaves the delay.
                        state.max_abs = new_abs;
                        state.delta = (new_abs - state.env) / ramp_divisor;
                        state.env += state.delta;
                        state.ramp_count = max_delay;
                    }
                }

                // `env >= |delayed|` holds in both branches above, so this is a true brick wall
                // (`Maxi32.c:366-386`). `env > MAX_OUTPUT > 0` guards the division.
                *sample = if state.env > MAX_OUTPUT {
                    delayed * MAX_OUTPUT / state.env
                } else {
                    delayed
                };
            }
        }
    }

    /// The look-ahead delay, `s->max_delay`: 33 frames at 44.1 kHz, 36 at 48 kHz (0.75 ms).
    ///
    /// This is the chain's only latency, and it is present whenever the power is on.
    fn latency_frames(&self) -> usize {
        self.max_delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    /// A constant-amplitude sine, interleaved identically across `channels`.
    fn sine(frames: usize, channels: usize, amplitude: Real, hz: Real) -> Vec<Real> {
        let w = std::f32::consts::TAU * hz / FS;
        (0..frames)
            .flat_map(|n| {
                let s = amplitude * (w * n as Real).sin();
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    fn peak(buffer: &[Real]) -> Real {
        buffer.iter().fold(0.0, |m: Real, s| m.max(s.abs()))
    }

    fn rms(buffer: &[Real]) -> Real {
        if buffer.is_empty() {
            return 0.0;
        }
        (buffer.iter().map(|s| s * s).sum::<Real>() / buffer.len() as Real).sqrt()
    }

    #[test]
    fn the_gain_boost_mapping_matches_the_shipping_slider_table() {
        // docs/spec/10-dsp-effects.md §9.3, the MUSIC2 column.
        for (slider, expected) in [
            (0, 1.0000),
            (1, 1.2023),
            (2, 1.4289),
            (3, 1.7179),
            (4, 2.1380),
            (5, 3.1623),
            (6, 3.8019),
            (7, 3.8019),
            (8, 3.8019),
            (9, 3.8019),
            (10, 3.8019),
        ] {
            let amount = fxsound_core::scale::slider_to_value(slider as Real);
            let got = gain_boost_for_amount(amount);
            assert!(
                (got - expected).abs() < 5e-4,
                "slider {slider}: got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn sliders_six_through_ten_are_identical_because_the_music_warp_saturates() {
        // Shipped quirk, spec §15.5 — worth a test so nobody "fixes" the clamp order later.
        let six = gain_boost_for_amount(fxsound_core::scale::slider_to_value(6.0));
        for slider in 7..=10 {
            let got = gain_boost_for_amount(fxsound_core::scale::slider_to_value(slider as Real));
            assert_eq!(got, six, "slider {slider}");
        }
        assert!((20.0 * six.log10() - 11.6).abs() < 1e-3, "{six}");
    }

    #[test]
    fn the_boost_table_reproduces_the_three_piecewise_segments() {
        assert!((maxi_boost_db(0) - 0.0).abs() < 1e-6);
        assert!((maxi_boost_db(59) - 5.9).abs() < 1e-5);
        assert!((maxi_boost_db(60) - 6.0).abs() < 1e-6);
        assert!((maxi_boost_db(89) - 11.8).abs() < 1e-5);
        assert!((maxi_boost_db(90) - 12.0).abs() < 1e-6);
        assert!((maxi_boost_db(125) - 29.5).abs() < 1e-5);
        // 126 is overwritten with 127's hard-set endpoint.
        assert!((maxi_boost_db(126) - 30.0).abs() < 1e-6);
        assert!((maxi_boost_db(127) - 30.0).abs() < 1e-6);
    }

    #[test]
    fn the_ceiling_is_a_permanent_minus_0_3_db() {
        let db = 20.0 * MAX_OUTPUT.log10();
        assert!((db + 0.3).abs() < 5e-4, "ceiling is {db} dB");
    }

    #[test]
    fn the_level_estimator_coefficients_match_the_reference_design() {
        // spec §9.6.
        for (fs, a0, filt_gain) in [
            (44_100.0, 0.999_985_752_5_f64, 1.424_75e-5_f64),
            (48_000.0, 0.999_986_910_1_f64, 1.309_0e-5_f64),
        ] {
            let boost = DynamicBoost::new(fs);
            assert!(
                (boost.level_filter_a0() - a0).abs() < 1e-9,
                "fs {fs}: a0 {}",
                boost.level_filter_a0()
            );
            assert!(
                (boost.filt_gain - filt_gain).abs() < 1e-9,
                "fs {fs}: filt_gain {}",
                boost.filt_gain
            );
            // τ = 1/((1 − a0)·fs) ≈ 1.5916 s at every rate, which is the point of the design.
            let tau = 1.0 / ((1.0 - boost.level_filter_a0()) * f64::from(fs));
            assert!((tau - 1.591_6).abs() < 1e-3, "fs {fs}: tau {tau}");
        }
    }

    #[test]
    fn the_release_coefficient_matches_the_reference_values() {
        // spec §9.5; 44.1 kHz also matches the `Play32.c:413` literal 0.997776.
        for (fs, beta) in [(44_100.0_f32, 0.997_775_65_f64), (48_000.0, 0.997_956_19)] {
            let boost = DynamicBoost::new(fs);
            assert!(
                (f64::from(boost.release_beta()) - beta).abs() < 1e-6,
                "fs {fs}: beta {}",
                boost.release_beta()
            );
        }
    }

    #[test]
    fn the_look_ahead_length_matches_the_reference_table() {
        // spec §9.1: 33 frames at 44.1 kHz, 36 at 48 kHz, and clamped to the buffer above that.
        assert_eq!(DynamicBoost::new(44_100.0).latency_frames(), 33);
        assert_eq!(DynamicBoost::new(48_000.0).latency_frames(), 36);
        assert_eq!(DynamicBoost::new(96_000.0).latency_frames(), 72);
        assert_eq!(
            DynamicBoost::new(192_000.0).latency_frames(),
            MAX_LOOK_AHEAD_FRAMES
        );
        // Nonsense rates must not produce a zero-length or oversized delay line.
        assert_eq!(DynamicBoost::new(Real::NAN).latency_frames(), 36);
        assert_eq!(DynamicBoost::new(-1.0).latency_frames(), 36);
        assert_eq!(DynamicBoost::new(1.0).latency_frames(), 1);
    }

    #[test]
    fn the_limiter_is_never_bypassed_even_at_zero() {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(0.0);
        assert!(boost.is_active());
        assert_eq!(boost.gain_boost(), 1.0);
        boost.set_amount(1.0);
        assert!(boost.is_active());
    }

    #[test]
    fn a_signal_below_the_threshold_passes_through_with_only_the_ceiling_applied() {
        // Amplitude 0.4 sine: RMS 0.283 never reaches the 0.32 back-off threshold at unity boost,
        // and the boosted peak 0.386 never reaches the 0.966 ceiling. So the only thing that may
        // happen to it is the permanent −0.3 dB and the look-ahead delay.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(0.0);

        let frames = 4_800;
        let input = sine(frames, 2, 0.4, 1_000.0);
        let mut buffer = input.clone();
        boost.process(&mut buffer, 2);

        let delay = boost.latency_frames();
        for n in 0..frames - delay {
            for ch in 0..2 {
                let got = buffer[(n + delay) * 2 + ch];
                let want = input[n * 2 + ch] * MAX_OUTPUT;
                assert!(
                    (got - want).abs() < 1e-6,
                    "frame {n} ch {ch}: got {got}, expected {want}"
                );
            }
        }
        // Nothing engaged: the whole point of this test.
        assert!(boost.level_rms() < TARGET_LEVEL, "{}", boost.level_rms());
    }

    #[test]
    fn the_first_frames_are_the_silent_look_ahead_delay() {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(0.5);
        let mut buffer = sine(512, 2, 0.5, 1_000.0);
        boost.process(&mut buffer, 2);
        let delay = boost.latency_frames();
        assert!(
            buffer[..delay * 2].iter().all(|s| *s == 0.0),
            "the delay line should still be flushing zeros"
        );
        assert!(buffer[delay * 2..].iter().any(|s| *s != 0.0));
    }

    #[test]
    fn no_output_sample_exceeds_the_ceiling_on_a_signal_that_would_clip() {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0); // ×3.8019, so a full-scale sine asks for +11.6 dB above full scale.
        let mut buffer = sine(FS as usize, 2, 1.0, 440.0);
        boost.process(&mut buffer, 2);

        let p = peak(&buffer);
        assert!(p <= MAX_OUTPUT + 1e-6, "peak {p} is above the ceiling");
        // And it does reach the ceiling — an over-cautious limiter would pass this test too.
        assert!(p > MAX_OUTPUT - 1e-3, "peak {p} never reached the ceiling");
    }

    #[test]
    fn the_limiter_never_overshoots_on_a_step_transient() {
        // Silence, then an instant jump to near full scale, asking for 0.9·3.8019·0.966051 = 3.305
        // — 10.7 dB above the ceiling.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);

        let frames = 4_800;
        let mut buffer = vec![0.0; frames * 2];
        for frame in buffer.as_chunks_mut::<2>().0.iter_mut().skip(1_000) {
            frame[0] = 0.9;
            frame[1] = -0.9;
        }
        boost.process(&mut buffer, 2);

        let p = peak(&buffer);
        assert!(p <= MAX_OUTPUT + 1e-6, "transient overshot to {p}");

        // Every frame of the step, including the very first one out of the delay line.
        let step_start = (1_000 + boost.latency_frames()) * 2;
        assert!(
            buffer[step_start..]
                .iter()
                .all(|s| s.abs() <= MAX_OUTPUT + 1e-6),
            "a frame inside the step exceeded the ceiling"
        );
        // After the envelope settles the wall is flat at exactly the ceiling.
        let settled = &buffer[buffer.len() - 200..];
        for s in settled {
            assert!(
                (s.abs() - MAX_OUTPUT).abs() < 1e-4,
                "settled output {s} is not sitting on the ceiling"
            );
        }
    }

    #[test]
    fn the_gain_is_already_down_before_the_transient_reaches_the_output() {
        // This is what the look-ahead actually buys, and it is not the same claim as "the output
        // never exceeds the ceiling": `Maxi32.c:346-348` raises the envelope to |delayed| inside
        // the same frame, so the wall would hold even with no delay at all — it would just hold it
        // by snapping the gain down in one sample, which is audible as a click. The delay exists
        // so the envelope climbs to the peak *linearly over max_delay frames before the peak
        // arrives*. Feeding the envelope from the delayed signal instead of the newly written one
        // leaves this test failing while every ceiling test still passes.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);
        let delay = boost.latency_frames();

        let step = 1_000;
        let frames = step + delay + 64;
        let mut buffer = vec![0.0; frames * 2];
        for frame in buffer.as_chunks_mut::<2>().0.iter_mut().skip(step) {
            frame[0] = 0.9;
            frame[1] = -0.9;
        }

        // One frame at a time so the envelope can be sampled as it moves.
        let mut envelope = Vec::with_capacity(frames);
        for frame in buffer.chunks_mut(2) {
            boost.process(frame, 2);
            envelope.push(boost.envelope(0));
        }

        // The level estimator has seen nothing but silence, so the step is written at full boost.
        let written = 0.9 * boost.gain_boost() * MAX_OUTPUT;
        assert!(written > 3.0, "the step should be asking for {written}");

        // Nothing before the step: the envelope cannot know about a sample that has not arrived.
        // It rests on MAXI_ENVELOPE_BIAS/(1 − beta) = 4.3e-22 rather than zero, which is the
        // denormal floor the bias exists to hold.
        assert!(envelope[step - 1] < 1e-20, "{}", envelope[step - 1]);

        // The ramp runs for the whole look-ahead, one equal step per frame (`Maxi32.c:328, 335`).
        let delta = written / (delay as Real + 1.0);
        for (k, env) in envelope.iter().skip(step).take(delay).enumerate() {
            let want = (k + 1) as Real * delta;
            assert!(
                (env - want).abs() < 1e-3,
                "frame {}: envelope {env} is not on the linear ramp ({want})",
                step + k
            );
        }

        // By the frame before the peak emerges the envelope is already within 3% of it — so the
        // gain that meets the transient is the ramped-down one, not a one-sample collapse.
        let arrival = step + delay;
        assert!(
            envelope[arrival - 1] > 0.95 * written,
            "envelope was only {} of {written} when the transient arrived",
            envelope[arrival - 1]
        );
        assert!(buffer[arrival * 2].abs() <= MAX_OUTPUT + 1e-6);
    }

    #[test]
    fn the_release_time_constant_matches_the_spec_within_a_few_percent() {
        // Push the envelope well above the ceiling with a short loud burst, let the ramp finish,
        // then watch it decay with a silent input. env(n) = env(0)·beta^n, so the measured time
        // constant is −1/(fs·ln beta) and must land on 0.1·1000^(85/127) = 10.18296 ms.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(0.0);

        // Values above ±1.0 are legitimate here: Surround hands this stage nearly 6 dB of
        // overshoot at maximum.
        let mut burst = vec![4.0; 64 * 2];
        boost.process(&mut burst, 2);
        let mut flush = vec![0.0; 256 * 2];
        boost.process(&mut flush, 2);

        let start = boost.envelope(0);
        assert!(start > MAX_OUTPUT, "envelope {start} never rose");

        let n = 2_000;
        let mut decay = vec![0.0; n * 2];
        boost.process(&mut decay, 2);
        let end = boost.envelope(0);

        let measured_beta = (end / start).powf(1.0 / n as Real);
        let tau_ms = -1.0 / (FS * measured_beta.ln()) * 1_000.0;
        assert!(
            (tau_ms - 10.182_96).abs() / 10.182_96 < 0.02,
            "release time constant measured {tau_ms} ms, expected 10.18296 ms"
        );
    }

    #[test]
    fn a_quiet_signal_is_lifted_by_the_full_static_boost() {
        // RMS 0.08 at ×3.8019 gives 0.304, just under the 0.32 target, so the auto-gain never
        // backs off and the peak (0.415 after boost) never reaches the ceiling: the output is the
        // input times the full static boost times the permanent ceiling.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);

        let frames = (FS * 5.0) as usize;
        let mut buffer = sine(frames, 2, 0.08 * std::f32::consts::SQRT_2, 220.0);
        boost.process(&mut buffer, 2);

        let tail = &buffer[(frames - FS as usize) * 2..];
        let got = rms(tail);
        let want = 0.08 * boost.gain_boost() * MAX_OUTPUT;
        assert!(
            (got - want).abs() / want < 0.01,
            "output RMS {got}, expected {want}"
        );
        assert!(peak(tail) <= MAX_OUTPUT + 1e-6);
        // Confirm the interpretation: the estimator settled below the back-off threshold.
        assert!(boost.gain_boost() * boost.level_rms() < TARGET_LEVEL);
    }

    #[test]
    fn the_auto_gain_backs_off_towards_the_target_over_the_documented_time_constant() {
        // A steady ±0.2 input at full boost asks for 0.76 RMS, well over the 0.32 target, so the
        // estimator has to pull the boost down from 3.8019 to 0.32/0.2 = 1.6. The estimator is a
        // one-pole with τ = 1.5916 s, so level(τ) must be 63.2% of its final value.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);

        let tau_frames = (FS * 1.591_6) as usize;
        let mut block = vec![0.2; tau_frames * 2];
        boost.process(&mut block, 2);

        let level_at_tau = f64::from(boost.level_rms()).powi(2);
        let final_level = 0.2_f64 * 0.2;
        let ratio = level_at_tau / final_level;
        assert!(
            (ratio - 0.632_1).abs() < 0.01,
            "level reached {:.4} of its final value at one time constant",
            ratio
        );

        // Five more time constants and the estimator is converged; the boost must then be exactly
        // the back-off that puts the input RMS on the target.
        let mut rest = vec![0.2; tau_frames * 5 * 2];
        boost.process(&mut rest, 2);
        let effective = TARGET_LEVEL / boost.level_rms();
        assert!(
            (effective - 1.6).abs() / 1.6 < 0.01,
            "settled boost {effective}, expected 1.6"
        );
        assert!(boost.level_rms() > 0.19 && boost.level_rms() < 0.21);
    }

    #[test]
    fn the_back_off_never_goes_below_the_anti_pumping_floor() {
        // At slider 0 a loud input would mathematically want 0.32/0.7 = 0.457, i.e. attenuation.
        // `Maxi32.c:288-289` refuses to go under 1.06, so loud material at slider 0 is boosted.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(0.0);
        let mut block = vec![0.7; (FS * 10.0) as usize * 2];
        boost.process(&mut block, 2);

        let rms_now = boost.level_rms();
        assert!(boost.gain_boost() * rms_now > TARGET_LEVEL, "back-off idle");
        assert!(
            TARGET_LEVEL / rms_now < MIN_BACKOFF_GAIN,
            "this input should be asking for attenuation"
        );
        // The applied gain is the floor, so the output sits at 0.7·1.06·0.966051 = 0.717 …
        // and then the peak limiter, which sees 0.717 < 0.966, leaves it alone.
        let tail = &block[block.len() - 200..];
        let want = 0.7 * MIN_BACKOFF_GAIN * MAX_OUTPUT;
        for s in tail {
            assert!((s - want).abs() < 1e-4, "got {s}, expected {want}");
        }
    }

    #[test]
    fn the_level_estimate_follows_the_left_channel_only() {
        // The documented asymmetry (`Maxi32.c:258-259`). Silence on the left, full scale on the
        // right: the estimator must stay at zero, so the right channel keeps the full static
        // boost and is held down by the peak limiter alone.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);

        let frames = (FS * 2.0) as usize;
        let mut buffer = vec![0.0; frames * 2];
        for (n, frame) in buffer.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            frame[0] = 0.0;
            frame[1] = 0.5 * (0.05 * n as Real).sin();
        }
        boost.process(&mut buffer, 2);

        assert_eq!(boost.level_rms(), 0.0, "a silent left channel must read zero");
        let right: Vec<Real> = buffer.as_chunks::<2>().0.iter().map(|f| f[1]).collect();
        assert!(peak(&right) <= MAX_OUTPUT + 1e-6);
        // 0.5·3.8019·0.966051 = 1.836 asked for, so the limiter must be doing real work.
        assert!(boost.envelope(1) > MAX_OUTPUT, "{}", boost.envelope(1));
        assert!(boost.envelope(0) < 1e-20, "{}", boost.envelope(0));
    }

    #[test]
    fn a_full_scale_square_wave_stays_finite_and_bounded() {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);

        let frames = (FS * 2.0) as usize;
        let mut buffer = vec![0.0; frames * 2];
        for (n, frame) in buffer.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            // 100 Hz square: a worst case of instant full-scale transients in both directions.
            let s = if (n / 240) % 2 == 0 { 1.0 } else { -1.0 };
            frame[0] = s;
            frame[1] = -s;
        }
        boost.process(&mut buffer, 2);

        assert!(buffer.iter().all(|s| s.is_finite()), "a sample went non-finite");
        assert!(peak(&buffer) <= MAX_OUTPUT + 1e-6);
        assert!(boost.envelope(0).is_finite() && boost.level_rms().is_finite());
    }

    #[test]
    fn resetting_returns_the_effect_to_its_initial_response() {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(0.4);

        let input = sine(1_024, 2, 0.3, 700.0);
        let mut first = input.clone();
        boost.process(&mut first, 2);

        // Beat it up, then reset.
        let mut loud = vec![3.0; 4_096 * 2];
        boost.process(&mut loud, 2);
        boost.reset();

        let mut second = input.clone();
        boost.process(&mut second, 2);
        assert_eq!(first, second, "reset did not clear every piece of history");
        // The design survived the reset.
        assert!((boost.gain_boost() - gain_boost_for_amount(0.4)).abs() < 1e-6);
    }

    #[test]
    fn changing_the_sample_rate_redesigns_and_clears() {
        let mut boost = DynamicBoost::new(44_100.0);
        boost.set_amount(1.0);
        let mut buffer = sine(512, 2, 0.8, 1_000.0);
        boost.process(&mut buffer, 2);
        assert_eq!(boost.latency_frames(), 33);

        boost.set_sample_rate(96_000.0);
        assert_eq!(boost.latency_frames(), 72);
        assert_eq!(boost.envelope(0), 0.0);
        assert_eq!(boost.level_rms(), 0.0);
        assert!((boost.release_beta() - 0.998_977).abs() < 1e-4);
        // The knob is a design parameter, not history.
        assert!((boost.gain_boost() - 3.8019).abs() < 1e-3);
    }

    #[test]
    fn mono_and_multichannel_streams_are_handled_without_panicking() {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);

        for channels in [1_usize, 2, 6, 8] {
            let mut buffer = sine(512, channels, 0.9, 440.0);
            boost.reset();
            boost.process(&mut buffer, channels);
            assert!(peak(&buffer) <= MAX_OUTPUT + 1e-6, "{channels} channels");
            assert!(buffer.iter().all(|s| s.is_finite()), "{channels} channels");
        }

        // Degenerate calls must be no-ops rather than panics.
        let mut empty: Vec<Real> = Vec::new();
        boost.process(&mut empty, 2);
        let mut buffer = vec![0.5; 8];
        boost.process(&mut buffer, 0);
        assert!(buffer.iter().all(|s| *s == 0.5));
    }

    #[test]
    fn channels_beyond_the_supported_count_pass_through_untouched() {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);
        let channels = MAX_CHANNELS + 2;
        let mut buffer = sine(64, channels, 0.9, 440.0);
        let original = buffer.clone();
        boost.process(&mut buffer, channels);

        for frame in 0..64 {
            for ch in MAX_CHANNELS..channels {
                let i = frame * channels + ch;
                assert_eq!(buffer[i], original[i], "frame {frame} ch {ch}");
            }
        }
    }

    #[test]
    fn a_block_longer_than_the_host_maximum_is_still_processed_whole() {
        // 16384 frames is DAW_MAX_BUFFER_SIZE; nothing here is block-sized, but prove it.
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(1.0);
        let mut buffer = sine(20_000, 2, 1.0, 440.0);
        boost.process(&mut buffer, 2);
        assert!(peak(&buffer) <= MAX_OUTPUT + 1e-6);
        assert!(buffer[19_000 * 2..].iter().any(|s| s.abs() > 0.1));
    }

    #[test]
    fn processing_one_frame_at_a_time_gives_the_same_result_as_one_block() {
        // The effect is a pure per-frame recursion, so block boundaries must not be audible.
        let input = sine(2_048, 2, 0.9, 330.0);

        let mut blocked = DynamicBoost::new(FS);
        blocked.set_amount(0.7);
        let mut whole = input.clone();
        blocked.process(&mut whole, 2);

        let mut framed = DynamicBoost::new(FS);
        framed.set_amount(0.7);
        let mut piecewise = input.clone();
        for frame in piecewise.chunks_mut(2) {
            framed.process(frame, 2);
        }

        assert_eq!(whole, piecewise);
    }
}
