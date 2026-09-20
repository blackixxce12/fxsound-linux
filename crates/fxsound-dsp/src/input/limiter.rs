//! A look-ahead brick-wall peak limiter.
//!
//! The algorithm is the limiter half of Dynamic Boost, ported from
//! `dsp/ptechDsp/Maximizer/Maxi32/Maxi32.c:296-386` — lifted here rather than copied, so there is
//! one implementation and [`crate::effects::DynamicBoost`] drives this one with its own fixed
//! parameters. The property that makes it worth keeping is stated in that module and is what its
//! golden tests pin: the envelope reaches an incoming peak **exactly as that peak leaves the delay
//! line**, so a transient is never clipped on the way in, and the output can never exceed the
//! ceiling.
//!
//! What is parameterised here and fixed there: the ceiling, the look-ahead and the release. A
//! microphone chain wants roughly −1 dBFS and a millisecond; Dynamic Boost wants the original's
//! −0.3 dBFS and 0.75 ms, and must keep wanting exactly that, because every preset ever voiced was
//! voiced through it.
//!
//! Real-time safe: the delay arena is allocated once, for the worst case, and nothing here
//! allocates, locks or branches on anything but its own state.

use crate::biquad::{MAX_CHANNELS, Real};

/// Longest look-ahead the arena can hold: 5 ms at the highest rate this port supports.
///
/// Sized from the maximum rather than from the current rate, so changing either the rate or the
/// look-ahead is a recalculation rather than a reallocation on the audio thread.
pub const MAX_LOOKAHEAD_MS: Real = 5.0;
const MAX_SAMPLE_RATE: Real = 192_000.0;
pub const MAX_LOOKAHEAD_FRAMES: usize = (MAX_SAMPLE_RATE * MAX_LOOKAHEAD_MS / 1000.0) as usize;

/// Keeps the release recursion out of denormals — `MAXI_ENVELOPE_BIAS` (`c_max.h:48`).
const ENVELOPE_BIAS: Real = 1.0e-24;

/// Per-channel limiter state (`c_max.h:101-112`, the `_l`/`_r` pairs).
#[derive(Clone, Copy, Debug, PartialEq)]
struct ChannelState {
    /// Index of the delay-line slot written next.
    write: usize,
    /// The peak envelope.
    env: Real,
    /// Per-sample increment of the attack ramp. Never cleared when a ramp ends — the original
    /// leaves it standing until the next ramp assigns a fresh value.
    delta: Real,
    /// Frames left in the attack ramp.
    ramp_count: usize,
    /// The peak the current ramp is aiming at.
    max_abs: Real,
}

impl ChannelState {
    const SILENT: Self = Self {
        write: 0,
        env: 0.0,
        delta: 0.0,
        ramp_count: 0,
        max_abs: 0.0,
    };
}

pub struct LookaheadLimiter {
    delay: Box<[Real]>,
    state: [ChannelState; MAX_CHANNELS],
    sample_rate: Real,
    /// Look-ahead in frames at the current rate, at least one.
    lookahead: usize,
    lookahead_ms: Real,
    ceiling: Real,
    release_ms: Real,
    release_beta: Real,
}

impl std::fmt::Debug for LookaheadLimiter {
    /// Prints the design, never the delay memory behind it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookaheadLimiter")
            .field("sample_rate", &self.sample_rate)
            .field("lookahead_ms", &self.lookahead_ms)
            .field("lookahead_frames", &self.lookahead)
            .field("ceiling", &self.ceiling)
            .field("release_ms", &self.release_ms)
            .finish()
    }
}

impl LookaheadLimiter {
    /// A limiter sized for the worst case it will ever be asked to handle.
    #[must_use]
    pub fn new(sample_rate: Real, ceiling: Real, lookahead_ms: Real, release_ms: Real) -> Self {
        let mut limiter = Self {
            delay: vec![0.0; MAX_CHANNELS * MAX_LOOKAHEAD_FRAMES].into_boxed_slice(),
            state: [ChannelState::SILENT; MAX_CHANNELS],
            sample_rate: sample_rate.max(1.0),
            lookahead: 1,
            lookahead_ms,
            ceiling: ceiling.max(Real::MIN_POSITIVE),
            release_ms,
            release_beta: 0.0,
        };
        limiter.design();
        limiter
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.design();
        self.reset();
    }

    /// The level the output may never exceed, as a linear amplitude.
    pub fn set_ceiling(&mut self, ceiling: Real) {
        self.ceiling = if ceiling.is_finite() && ceiling > 0.0 {
            ceiling
        } else {
            Real::MIN_POSITIVE
        };
    }

    /// The level the output may never exceed, in dBFS. `-1.0` is the usual choice for a signal
    /// something else will encode.
    pub fn set_ceiling_db(&mut self, db: Real) {
        let db = if db.is_finite() { db.min(0.0) } else { -1.0 };
        self.set_ceiling(10.0_f32.powf(db / 20.0));
    }

    /// How far ahead the limiter sees, in milliseconds. This **is** the latency it adds.
    pub fn set_lookahead_ms(&mut self, ms: Real) {
        let ms = if ms.is_finite() {
            ms.clamp(0.0, MAX_LOOKAHEAD_MS)
        } else {
            1.0
        };
        if ms == self.lookahead_ms {
            return;
        }
        self.lookahead_ms = ms;
        let beta = self.release_beta;
        self.design();
        self.release_beta = beta;
        self.reset();
    }

    pub fn set_release_ms(&mut self, ms: Real) {
        let ms = if ms.is_finite() { ms.max(0.0) } else { 80.0 };
        if ms == self.release_ms {
            return;
        }
        self.release_ms = ms;
        self.design();
    }

    /// Set the release recursion's coefficient directly.
    ///
    /// For a caller that already has a beta rather than a time: Dynamic Boost derives its own from
    /// a fixed MIDI value through the original's exponential table, in f32, and converting that to
    /// milliseconds and back would move it. Bit-exactness there is the whole point of the port.
    pub fn set_release_beta(&mut self, beta: Real) {
        if beta.is_finite() && (0.0..1.0).contains(&beta) {
            self.release_beta = beta;
        }
    }

    fn design(&mut self) {
        let frames = (self.sample_rate * self.lookahead_ms / 1000.0) as usize;
        self.lookahead = frames.clamp(1, MAX_LOOKAHEAD_FRAMES);
        // One time constant per release: `beta = exp(-1 / (tau * fs))`, so the envelope falls to
        // 1/e of its value in `release_ms`.
        let tau = (self.release_ms / 1000.0).max(1.0e-6);
        self.release_beta = (-1.0 / (tau * self.sample_rate)).exp();
    }

    /// Frames of delay the limiter adds. Publish this, or a recording application is told the
    /// stream is instantaneous when it is not.
    #[must_use]
    pub const fn latency_frames(&self) -> usize {
        self.lookahead
    }

    /// The peak envelope for one channel, which is what a gain-reduction meter shows.
    ///
    /// Reduction in dB is `20·log10(ceiling / envelope)` while the envelope is above the ceiling,
    /// and zero otherwise.
    #[must_use]
    pub fn envelope(&self, channel: usize) -> Real {
        self.state.get(channel).map_or(0.0, |state| state.env)
    }

    pub fn reset(&mut self) {
        self.delay.fill(0.0);
        self.state = [ChannelState::SILENT; MAX_CHANNELS];
    }

    /// One interleaved frame, in place.
    ///
    /// Separate from [`Self::process`] because a caller whose gain changes per frame — Dynamic
    /// Boost's auto-gain does — has to apply that gain between frames.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [Real]) {
        let lookahead = self.lookahead.max(1);
        // "Note that since envelope ramping starts immediately on this sample, divisor of delta
        // calc is delay plus one" (`Maxi32.c:323-326`). Off by one here and the ramp lands early
        // or late, so the gain still steps at the transient instead of arriving already reduced —
        // the ceiling holds either way, the smoothness does not.
        let ramp_divisor = lookahead as Real + 1.0;
        let beta = self.release_beta;
        let ceiling = self.ceiling;

        // One fixed-length delay line per channel. Zipping against `frame` stops at whichever runs
        // out first, which is how channels beyond `MAX_CHANNELS` end up untouched without an index
        // or a branch.
        let (lines, _) = self.delay.as_chunks_mut::<MAX_LOOKAHEAD_FRAMES>();
        for ((state, line), sample) in self
            .state
            .iter_mut()
            .zip(lines.iter_mut())
            .zip(frame.iter_mut())
        {
            // Look-ahead delay: read the frame written `lookahead` frames ago, then overwrite that
            // slot with the incoming one (`Maxi32.c:296-301`).
            let Some(slot) = line.get_mut(state.write) else {
                continue;
            };
            let delayed = *slot;
            let incoming = *sample;
            *slot = incoming;
            let new_abs = incoming.abs();
            state.write += 1;
            if state.write >= lookahead {
                state.write = 0;
            }

            if state.ramp_count != 0 {
                // Attack ramp in progress (`Maxi32.c:304-336`).
                let abs_out = delayed.abs();
                if abs_out > state.env {
                    state.env = abs_out;
                }
                if new_abs > state.max_abs {
                    // A louder peak arrived mid-ramp: retarget and restart the countdown, but only
                    // steepen the slope, never flatten it — flattening would let the previous peak
                    // through unlimited.
                    state.max_abs = new_abs;
                    state.ramp_count = lookahead;
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
                    state.ramp_count = lookahead;
                }
            }

            // `env >= |delayed|` holds in both branches above, so this is a true brick wall
            // (`Maxi32.c:366-386`). `env > ceiling > 0` guards the division.
            *sample = if state.env > ceiling {
                delayed * ceiling / state.env
            } else {
                delayed
            };
        }
    }

    /// A whole interleaved block, in place.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || buffer.is_empty() {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            self.process_frame(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    fn limiter(ceiling_db: Real) -> LookaheadLimiter {
        let mut l = LookaheadLimiter::new(FS, 1.0, 1.0, 80.0);
        l.set_ceiling_db(ceiling_db);
        l
    }

    #[test]
    fn nothing_leaves_above_the_ceiling() {
        for ceiling_db in [-0.3, -1.0, -3.0, -6.0] {
            let mut l = limiter(ceiling_db);
            let ceiling = 10.0_f32.powf(ceiling_db / 20.0);
            let mut peak: Real = 0.0;
            // A signal that is nothing but transients: the hardest case for a look-ahead design,
            // because every sample retargets the ramp.
            for n in 0..48_000_usize {
                let value = if n % 97 == 0 { 4.0 } else { 0.05 };
                let mut frame = [value, -value];
                l.process_frame(&mut frame);
                peak = peak.max(frame[0].abs()).max(frame[1].abs());
            }
            assert!(
                peak <= ceiling * 1.0001,
                "{ceiling_db} dBFS: something got out at {peak}, ceiling is {ceiling}"
            );
        }
    }

    #[test]
    fn quiet_audio_passes_through_untouched_after_the_delay() {
        let mut l = limiter(-1.0);
        let input: Vec<Real> = (0..4_000).map(|n| (n as Real * 0.05).sin() * 0.1).collect();
        let mut out = Vec::with_capacity(input.len());
        for &x in &input {
            let mut frame = [x];
            l.process_frame(&mut frame);
            out.push(frame[0]);
        }
        let delay = l.latency_frames();
        for n in delay..input.len() {
            let want = input[n - delay];
            assert!(
                (out[n] - want).abs() < 1e-6,
                "frame {n}: {} against {want}",
                out[n]
            );
        }
    }

    #[test]
    fn the_latency_is_the_look_ahead_and_is_reported() {
        let mut l = LookaheadLimiter::new(FS, 1.0, 1.0, 80.0);
        assert_eq!(l.latency_frames(), 48, "1 ms at 48 kHz");
        l.set_lookahead_ms(0.75);
        assert_eq!(l.latency_frames(), 36, "0.75 ms, what Dynamic Boost uses");
        l.set_sample_rate(96_000.0);
        assert_eq!(l.latency_frames(), 72);
        // Asking for more than the arena holds is clamped, never a reallocation.
        l.set_lookahead_ms(1_000.0);
        assert!(l.latency_frames() <= MAX_LOOKAHEAD_FRAMES);
    }

    #[test]
    fn a_transient_is_already_reduced_when_it_arrives() {
        // The property the whole look-ahead exists for: the sample *before* a loud transient is
        // already attenuated, so the transient itself is never clipped flat.
        let mut l = limiter(-1.0);
        let mut out = Vec::new();
        for n in 0..2_000_usize {
            let x: Real = if n == 1_000 { 6.0 } else { 0.2 };
            let mut frame = [x];
            l.process_frame(&mut frame);
            out.push(frame[0]);
        }
        let delay = l.latency_frames();
        let at = 1_000 + delay;
        assert!(out[at].abs() <= 10.0_f32.powf(-1.0 / 20.0) * 1.0001);
        // Gain reduction started before the peak reached the output.
        assert!(
            out[at - 1].abs() < 0.2,
            "the run-up was not attenuated: {}",
            out[at - 1]
        );
    }

    #[test]
    fn a_non_finite_sample_cannot_latch_the_envelope() {
        // The engine sanitises its input block, so this is defence in depth for a stage that a
        // voice chain will drive with a makeup gain in front of it.
        let mut l = limiter(-1.0);
        let mut frame = [Real::INFINITY];
        l.process_frame(&mut frame);
        for _ in 0..4_000 {
            let mut frame = [0.1_f32];
            l.process_frame(&mut frame);
        }
        let mut frame = [0.1_f32];
        l.process_frame(&mut frame);
        assert!(frame[0].is_finite(), "the limiter is stuck: {}", frame[0]);
    }
}
