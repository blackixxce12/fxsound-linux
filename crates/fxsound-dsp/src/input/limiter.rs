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
//! What is parameterised here and fixed there: the ceiling, the look-ahead, the hold and the
//! release. A microphone chain wants roughly −1 dBFS and a millisecond; Dynamic Boost wants the
//! original's −0.3 dBFS and 0.75 ms, and must keep wanting exactly that, because every preset ever
//! voiced was voiced through it.
//!
//! # Where this departs from `Maxi32.c`
//!
//! Three changes, each one a defect of the original that the 0.4.0 audit of what the port copied
//! found audible, and each one shared by both limiters that use this type:
//!
//! * **One envelope for both sides of a pair** (audit R2). The original runs an envelope per
//!   channel, so a peak on one side — hard-panned material, or Surround at 10 — pulls that side
//!   down by up to 6 dB and leaves the other alone, and the stereo image lurches towards the quiet
//!   side for as long as the limiter works. Here the channels a caller links share one envelope,
//!   which follows the loudest of them, and get the same gain, which is what a limiter behind a
//!   mix is expected to do: the balance the mix had is the balance it keeps. Unless told
//!   otherwise every channel is linked, which is what a stereo or a mono stream wants and what the
//!   microphone chain uses. Dynamic Boost links the speakers either side of the listener and
//!   leaves the centre and the subwoofer an envelope each ([`LookaheadLimiter::set_linked`]): on
//!   5.1 and 7.1 one envelope for all eight channels let a subwoofer boom duck every speaker by
//!   12 dB. Each channel still has its own delay line.
//! * **The attack ramp stops at the peak it is aiming for** (audit #8). The original only ever
//!   steepens a ramp, so a run of rising samples — the front of any low sine — leaves it climbing
//!   at the steepest slope it saw until the countdown ends, past the peak. A 50 Hz sine at twice
//!   the ceiling drove the envelope to 2.26 instead of 2.00: a decibel of reduction for nothing,
//!   and more pumping on bass. Now the envelope is clamped to that peak (or to whatever it already
//!   holds, see below), which never lets anything through: it is still at least every sample that
//!   is leaving.
//! * **The envelope holds before it releases** (audit R1). The original lets go the frame after
//!   a peak has left, at a 10 ms time constant, which is shorter than the gap between two peaks of
//!   a bass note. So under limiting the gain rises and falls again inside every half-cycle, and
//!   that modulation is distortion: a sine 3 dB into Dynamic Boost's limiter came out with 15.6 %
//!   THD+N at 40 Hz and 9.5 % at 80 Hz. Here the envelope may not fall below the loudest sample
//!   that left in the last [`DEFAULT_HOLD_MS`]-or-so — a running peak over the output — and only
//!   then releases, at the same rate as before. A steady tone therefore sees one constant gain,
//!   and a single transient costs its hold time in recovery. The hold follows the program rather
//!   than starting a timer: a decaying bass note is let down as its own crests fall, about a hold
//!   behind them, instead of being released and caught again every half-cycle.
//!
//! With the hold set to zero, the clamp of the second change is the only difference from the
//! original's arithmetic on a mono stream of finite samples. (The other: an infinity is let go of
//! once it has left the hold window, where the original's envelope stayed infinite for good.)
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

/// How long the envelope holds a peak before it starts to release, unless a caller asks for
/// something else.
///
/// Long enough to span the gap between two crests of a bass note: a 25 Hz sine's crests are
/// 20 ms apart, so from there up a limited tone meets one steady gain instead of one that breathes
/// twice a cycle (at 20 Hz, the bottom of hearing, 3 dB into Dynamic Boost's limiter, 0.7 % THD+N
/// is left where the original had 22 %). Not longer, because it is also how much longer a lone
/// transient keeps the level down:
/// 25 ms would have cleaned up 20 Hz as well, for another 4 ms of recovery on every hit.
pub const DEFAULT_HOLD_MS: Real = 20.0;

/// The longest hold [`LookaheadLimiter::set_hold_ms`] accepts.
pub const MAX_HOLD_MS: Real = 50.0;

/// How many pieces the hold window is kept in.
///
/// A running maximum over the last `n` frames either keeps every frame or approximates. This
/// keeps one maximum per `hold / 8` frames, so a frame costs a couple of comparisons and every
/// eighth of the hold eight more, and the window it holds for is between the hold and nine eighths
/// of it.
const HOLD_SEGMENTS: usize = 8;

/// Keeps the release recursion out of denormals — `MAXI_ENVELOPE_BIAS` (`c_max.h:48`).
const ENVELOPE_BIAS: Real = 1.0e-24;

/// The envelope (`c_max.h:101-112`, one of the `_l`/`_r` pairs — one per group of linked
/// channels now, not one per channel).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Envelope {
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

impl Envelope {
    const SILENT: Self = Self {
        env: 0.0,
        delta: 0.0,
        ramp_count: 0,
        max_abs: 0.0,
    };
}

/// The loudest sample to leave the delay line over roughly the last hold time.
///
/// Kept as [`HOLD_SEGMENTS`] closed segments and one open one, each remembering only its own
/// maximum, so the window slides a segment at a time and nothing is ever searched per frame. The
/// closed segments' maxima are kept apart, in a [`HoldRing`] that only a closing segment touches,
/// so what every frame reads and writes is four numbers a loop can hold in registers.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PeakHold {
    /// Frames per segment. Zero means no hold: the window is the current frame alone.
    segment_len: usize,
    /// Frames already in the open segment.
    filled: usize,
    /// The open segment's maximum.
    open: Real,
    /// The maximum of the closed segments, recomputed only when a segment closes.
    closed_max: Real,
}

/// The closed segments of a [`PeakHold`].
#[derive(Clone, Copy, Debug, PartialEq)]
struct HoldRing {
    /// Their maxima, a ring whose next slot to overwrite is `oldest`.
    closed: [Real; HOLD_SEGMENTS],
    oldest: usize,
}

impl HoldRing {
    const EMPTY: Self = Self {
        closed: [0.0; HOLD_SEGMENTS],
        oldest: 0,
    };
}

impl PeakHold {
    const fn new(segment_len: usize) -> Self {
        Self {
            segment_len,
            filled: 0,
            open: 0.0,
            closed_max: 0.0,
        }
    }

    /// Forget every peak, keep the length.
    fn clear(&mut self, ring: &mut HoldRing) {
        *self = Self::new(self.segment_len);
        *ring = HoldRing::EMPTY;
    }

    /// Take one frame's leaving peak and return the loudest the window holds, that one included.
    ///
    /// Comparisons rather than `max`, so a NaN is simply never the larger: it cannot get into the
    /// window, and an infinity falls out of it with the segment it arrived in.
    #[inline]
    fn push(&mut self, ring: &mut HoldRing, value: Real) -> Real {
        if self.segment_len == 0 {
            return value;
        }
        if value > self.open {
            self.open = value;
        }
        let held = if self.closed_max > self.open {
            self.closed_max
        } else {
            self.open
        };
        self.filled += 1;
        if self.filled >= self.segment_len {
            if let Some(slot) = ring.closed.get_mut(ring.oldest) {
                *slot = self.open;
            }
            ring.oldest = (ring.oldest + 1) % HOLD_SEGMENTS;
            self.closed_max = ring.closed.iter().fold(
                0.0,
                |most: Real, &peak| if peak > most { peak } else { most },
            );
            self.open = 0.0;
            self.filled = 0;
        }
        held
    }
}

/// Every channel linked, one bit per channel: the default.
const ALL_LINKED: u32 = (1 << MAX_CHANNELS) - 1;
const _: () = assert!(MAX_CHANNELS < 32, "one bit per channel in a u32");

pub struct LookaheadLimiter {
    delay: Box<[Real]>,
    /// Index of the delay-line slot written next. Every channel is written every frame, so one
    /// index serves every line.
    write: usize,
    /// One envelope and one hold window for the linked channels, kept in the slot of the lowest
    /// of them, and one for each channel of its own, in its own slot. Slot 0 is the only one in
    /// use while every channel is linked.
    envelopes: [Envelope; MAX_CHANNELS],
    holds: [PeakHold; MAX_CHANNELS],
    rings: [HoldRing; MAX_CHANNELS],
    /// One bit per channel that shares the linked envelope.
    linked_mask: u32,
    /// The slot the linked envelope is kept in: the lowest linked channel.
    linked_slot: usize,
    /// How many leading channels are all linked: a frame no wider than this takes the
    /// one-envelope path, which is all a stereo or mono stream ever takes.
    linked: usize,
    sample_rate: Real,
    /// Look-ahead in frames at the current rate, at least one.
    lookahead: usize,
    lookahead_ms: Real,
    ceiling: Real,
    hold_ms: Real,
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
            .field("hold_ms", &self.hold_ms)
            .field("release_ms", &self.release_ms)
            .finish()
    }
}

impl LookaheadLimiter {
    /// A limiter sized for the worst case it will ever be asked to handle, holding for
    /// [`DEFAULT_HOLD_MS`].
    #[must_use]
    pub fn new(sample_rate: Real, ceiling: Real, lookahead_ms: Real, release_ms: Real) -> Self {
        let mut limiter = Self {
            delay: vec![0.0; MAX_CHANNELS * MAX_LOOKAHEAD_FRAMES].into_boxed_slice(),
            write: 0,
            envelopes: [Envelope::SILENT; MAX_CHANNELS],
            holds: [PeakHold::new(0); MAX_CHANNELS],
            rings: [HoldRing::EMPTY; MAX_CHANNELS],
            linked_mask: ALL_LINKED,
            linked_slot: 0,
            linked: MAX_CHANNELS,
            sample_rate: sample_rate.max(1.0),
            lookahead: 1,
            lookahead_ms,
            ceiling: ceiling.max(Real::MIN_POSITIVE),
            hold_ms: DEFAULT_HOLD_MS,
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

    /// How long a peak keeps the gain down after it has left, before the release starts, in
    /// milliseconds; `0.0` releases at once, as the original did. Clamped to [`MAX_HOLD_MS`].
    ///
    /// The peaks already held are forgotten, so the envelope starts releasing from where it is:
    /// a change of design, not of history worth keeping.
    pub fn set_hold_ms(&mut self, ms: Real) {
        let ms = if ms.is_finite() {
            ms.clamp(0.0, MAX_HOLD_MS)
        } else {
            DEFAULT_HOLD_MS
        };
        if ms == self.hold_ms {
            return;
        }
        self.hold_ms = ms;
        let beta = self.release_beta;
        self.design();
        self.release_beta = beta;
        // `design` only rebuilds the window when its segment length changes; a hold that moves
        // by less than a segment keeps the length, so forget the peaks here, as the doc promises.
        for (hold, ring) in self.holds.iter_mut().zip(&mut self.rings) {
            hold.clear(ring);
        }
    }

    /// The hold, in milliseconds, as set.
    #[must_use]
    pub const fn hold_ms(&self) -> Real {
        self.hold_ms
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
        // The hold in frames, split into its segments; rounded up, so the window is never shorter
        // than asked for.
        let hold_frames = (self.sample_rate * self.hold_ms / 1000.0).round() as usize;
        let segment_len = hold_frames.div_ceil(HOLD_SEGMENTS);
        if segment_len != self.holds[0].segment_len {
            self.holds = [PeakHold::new(segment_len); MAX_CHANNELS];
            self.rings = [HoldRing::EMPTY; MAX_CHANNELS];
        }
    }

    /// Link every channel to one envelope: the loudest of them sets the gain for all. The default,
    /// and what a stereo or mono stream wants.
    pub fn link_all(&mut self) {
        self.set_linked(&[true; MAX_CHANNELS]);
    }

    /// Say which channels share an envelope: every channel `c` with `linked[c]` set is limited
    /// together with the others set, on the loudest of them, and every other channel on an
    /// envelope of its own. A channel past the end of `linked` is on its own, so `&[true, true]`
    /// links the first two channels and leaves the rest alone.
    ///
    /// Dynamic Boost links the speakers either side of the listener and gives the centre and the
    /// subwoofer one each (audit R2). One linked set is all that asks for, and it keeps the
    /// bookkeeping to a bit per channel: the first cut took any grouping and walked it group by
    /// group, and on 5.1 cost half as much again as the original's envelope per channel.
    /// Allocation-free; a change starts every envelope from the one reducing the most, so no
    /// channel's gain jumps up at the change and the limiter lets go at its own release from
    /// there.
    pub fn set_linked(&mut self, linked: &[bool]) {
        let mask = linked
            .iter()
            .take(MAX_CHANNELS)
            .enumerate()
            .filter(|(_, linked)| **linked)
            .fold(0, |mask, (channel, _)| mask | 1 << channel);
        if mask == self.linked_mask {
            return;
        }
        self.linked_mask = mask;
        // With nothing linked the slot is never used: every channel reads its own.
        self.linked_slot = (mask.trailing_zeros() as usize).min(MAX_CHANNELS - 1);
        self.linked = (mask.trailing_ones() as usize).min(MAX_CHANNELS);
        let loudest = (0..MAX_CHANNELS).fold(0, |most, slot| {
            if self.envelopes[slot].env > self.envelopes[most].env {
                slot
            } else {
                most
            }
        });
        let (envelope, hold, ring) = (
            self.envelopes[loudest],
            self.holds[loudest],
            self.rings[loudest],
        );
        self.envelopes = [envelope; MAX_CHANNELS];
        self.holds = [hold; MAX_CHANNELS];
        self.rings = [ring; MAX_CHANNELS];
    }

    /// The slot of the envelope `channel` is limited by.
    const fn slot(&self, channel: usize) -> usize {
        if self.linked_mask & (1 << channel) != 0 {
            self.linked_slot
        } else {
            channel
        }
    }

    /// Frames of delay the limiter adds. Publish this, or a recording application is told the
    /// stream is instantaneous when it is not.
    #[must_use]
    pub const fn latency_frames(&self) -> usize {
        self.lookahead
    }

    /// The peak envelope `channel` is limited by — its group's, shared by every channel linked to
    /// it — which is what a gain-reduction meter shows. Zero for a channel the limiter does not
    /// reach.
    ///
    /// Reduction in dB is `20·log10(ceiling / envelope)` while the envelope is above the ceiling,
    /// and zero otherwise.
    #[must_use]
    pub fn envelope(&self, channel: usize) -> Real {
        if channel < MAX_CHANNELS {
            self.envelopes
                .get(self.slot(channel))
                .map_or(0.0, |envelope| envelope.env)
        } else {
            0.0
        }
    }

    /// What a gain-reduction meter shows for one channel: `20·log10(envelope / ceiling)` while
    /// the envelope is above the ceiling, as a positive number, and zero otherwise.
    #[must_use]
    pub fn reduction_db(&self, channel: usize) -> Real {
        let envelope = self.envelope(channel);
        if envelope > self.ceiling && self.ceiling > 0.0 {
            20.0 * (envelope / self.ceiling).log10()
        } else {
            0.0
        }
    }

    pub fn reset(&mut self) {
        self.delay.fill(0.0);
        self.write = 0;
        self.envelopes = [Envelope::SILENT; MAX_CHANNELS];
        for (hold, ring) in self.holds.iter_mut().zip(&mut self.rings) {
            hold.clear(ring);
        }
    }

    /// One interleaved frame, in place, exactly as [`Self::process`] would limit it.
    ///
    /// For a caller that has to act between frames. [`Self::process`] is the cheaper on a stream
    /// whose channels are not all linked, since it runs each envelope over the whole block.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [Real]) {
        if frame.len().min(MAX_CHANNELS) > self.linked {
            self.process_grouped(frame, frame.len());
            return;
        }
        let lookahead = self.lookahead.max(1);
        let write = self.write;

        // Look-ahead delay: read the frame written `lookahead` frames ago, then overwrite that slot
        // with the incoming one (`Maxi32.c:296-301`), and note the loudest sample of each over
        // every channel — the one envelope follows those. One fixed-length line per channel;
        // zipping against `frame` stops at whichever runs out first, which is how channels beyond
        // `MAX_CHANNELS` end up untouched, and out of the envelope, without an index or a branch.
        // The delayed sample is parked in the frame until the gain is known.
        let mut new_abs: Real = 0.0;
        let mut abs_out: Real = 0.0;
        let (lines, _) = self.delay.as_chunks_mut::<MAX_LOOKAHEAD_FRAMES>();
        for (line, sample) in lines.iter_mut().zip(frame.iter_mut()) {
            let Some(slot) = line.get_mut(write) else {
                continue;
            };
            let delayed = *slot;
            *slot = *sample;
            // Comparisons, not `max`: cheaper, and a NaN is never the larger either way.
            let arriving = sample.abs();
            if arriving > new_abs {
                new_abs = arriving;
            }
            let leaving = delayed.abs();
            if leaving > abs_out {
                abs_out = leaving;
            }
            *sample = delayed;
        }
        self.write = if write + 1 >= lookahead { 0 } else { write + 1 };

        let env = Self::update_envelope(
            &mut self.envelopes[0],
            &mut self.holds[0],
            &mut self.rings[0],
            self.release_beta,
            new_abs,
            abs_out,
            lookahead,
        );

        // `env >= |delayed|` for every channel after the update, so this is a true brick wall
        // (`Maxi32.c:366-386`). `env > ceiling > 0` guards the division.
        let ceiling = self.ceiling;
        if env > ceiling {
            for sample in frame.iter_mut().take(MAX_CHANNELS) {
                *sample = *sample * ceiling / env;
            }
        }
    }

    /// [`Self::process`] for a stream whose channels are not all linked: the same delay, and the
    /// same envelope arithmetic once for the linked channels, on the loudest of them, and once for
    /// each channel of its own, on its own samples.
    ///
    /// The envelopes never meet, so each is run over the whole block before the next, with its
    /// state in registers: a channel of its own in a loop of its own, the linked ones a frame at a
    /// time. The output is the one frame-by-frame processing gives, to the bit. Dynamic Boost
    /// alone at slider 10, on white noise at ±0.3 in 480-frame blocks at 48 kHz (best of seven
    /// release runs on a Ryzen 7 6800H), costs 20.0 ns a frame on 5.1 with its sides named and
    /// 22.4 on 7.1, where the original's envelope per channel cost 20.8 and 30.3 and one envelope
    /// for every channel 15.4 and 17.7. With only the front pair known, 5.1 costs 23.7 against the
    /// original's 20.6: four channels of their own, each paying for the hold and the clamp the
    /// original's envelope did without. The first cut ran any grouping a frame at a time, group by
    /// group, and cost 33.6 on 5.1 and 39.4 on 7.1.
    fn process_grouped(&mut self, buffer: &mut [Real], channels: usize) {
        let lookahead = self.lookahead.max(1);
        let ceiling = self.ceiling;
        let release_beta = self.release_beta;
        let present = channels.min(MAX_CHANNELS);
        let all = (1_u32 << present) - 1;
        let linked = self.linked_mask & all;
        let linked_slot = self.linked_slot;
        let start = self.write;
        // Where the write index ends up, stepped as every loop below steps it.
        let mut end = start;

        let Self {
            delay,
            envelopes,
            holds,
            rings,
            ..
        } = self;
        let (lines, _) = delay.as_chunks_mut::<MAX_LOOKAHEAD_FRAMES>();

        for (channel, (((line, state), hold), ring)) in lines
            .iter_mut()
            .zip(envelopes.iter_mut())
            .zip(holds.iter_mut())
            .zip(rings.iter_mut())
            .enumerate()
            .take(present)
        {
            if linked & (1 << channel) != 0 {
                continue;
            }
            let (mut env_state, mut hold_state) = (*state, *hold);
            let mut write = start;
            for frame in buffer.chunks_exact_mut(channels) {
                let step = if write + 1 >= lookahead { 0 } else { write + 1 };
                let (Some(sample), Some(slot)) = (frame.get_mut(channel), line.get_mut(write))
                else {
                    write = step;
                    continue;
                };
                write = step;
                let delayed = *slot;
                *slot = *sample;
                let env = Self::update_envelope(
                    &mut env_state,
                    &mut hold_state,
                    ring,
                    release_beta,
                    sample.abs(),
                    delayed.abs(),
                    lookahead,
                );
                // `env >= |delayed|`, as on the linked path.
                *sample = if env > ceiling {
                    delayed * ceiling / env
                } else {
                    delayed
                };
            }
            (*state, *hold) = (env_state, hold_state);
            end = write;
        }

        if let (true, Some(state), Some(hold), Some(ring)) = (
            linked != 0,
            envelopes.get_mut(linked_slot),
            holds.get_mut(linked_slot),
            rings.get_mut(linked_slot),
        ) {
            let (mut env_state, mut hold_state) = (*state, *hold);
            let mut write = start;
            for frame in buffer.chunks_exact_mut(channels) {
                // Comparisons, not `max`: a NaN is never the larger.
                let mut new_abs: Real = 0.0;
                let mut abs_out: Real = 0.0;
                for (channel, (line, sample)) in lines.iter_mut().zip(frame.iter_mut()).enumerate()
                {
                    if linked & (1 << channel) == 0 {
                        continue;
                    }
                    let Some(slot) = line.get_mut(write) else {
                        continue;
                    };
                    let delayed = *slot;
                    *slot = *sample;
                    let arriving = sample.abs();
                    if arriving > new_abs {
                        new_abs = arriving;
                    }
                    let leaving = delayed.abs();
                    if leaving > abs_out {
                        abs_out = leaving;
                    }
                    *sample = delayed;
                }
                write = if write + 1 >= lookahead { 0 } else { write + 1 };
                let env = Self::update_envelope(
                    &mut env_state,
                    &mut hold_state,
                    ring,
                    release_beta,
                    new_abs,
                    abs_out,
                    lookahead,
                );
                if env > ceiling {
                    for (channel, sample) in frame.iter_mut().enumerate().take(MAX_CHANNELS) {
                        if linked & (1 << channel) != 0 {
                            *sample = *sample * ceiling / env;
                        }
                    }
                }
            }
            (*state, *hold) = (env_state, hold_state);
            end = write;
        }
        self.write = end;
    }

    /// The envelope `state`, with its hold window `hold`, one frame on: `new_abs` is the loudest
    /// sample of its channels just written, `abs_out` the loudest one just read (`Maxi32.c:304-362`,
    /// with the three departures above).
    #[inline(always)]
    fn update_envelope(
        state: &mut Envelope,
        hold: &mut PeakHold,
        ring: &mut HoldRing,
        release_beta: Real,
        new_abs: Real,
        abs_out: Real,
        lookahead: usize,
    ) -> Real {
        // "Note that since envelope ramping starts immediately on this sample, divisor of delta
        // calc is delay plus one" (`Maxi32.c:323-326`). Off by one here and the ramp lands early
        // or late, so the gain still steps at the transient instead of arriving already reduced —
        // the ceiling holds either way, the smoothness does not.
        let ramp_divisor = lookahead as Real + 1.0;
        // The loudest sample to leave in the hold window, this frame's included, so never below
        // `abs_out`: every "at least what is leaving" below can use it instead.
        let held = hold.push(ring, abs_out);

        if state.ramp_count != 0 {
            // Attack ramp in progress (`Maxi32.c:304-336`).
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
            // Audit #8: a slope kept steep from an earlier retarget must not carry the envelope
            // past the peak it was aiming at. Clamped to that peak, or to what the hold already
            // holds if that is louder — both at least `abs_out`, so the wall still stands.
            let target = if held > state.max_abs {
                held
            } else {
                state.max_abs
            };
            if state.env > target {
                state.env = target;
            }
        } else {
            // Release (`Maxi32.c:338-362`), but never below the hold: the envelope decays only
            // once the loudest recent peak has aged out of the window, and then towards the next
            // loudest rather than past it. With no hold `held` is `abs_out`, which is the
            // original's "raise the envelope to what is leaving" exactly. The bias keeps the
            // recursion off denormals.
            let decayed = state.env * release_beta + ENVELOPE_BIAS;
            // An infinity decays to itself, so one non-finite sample would otherwise hold the gain
            // at zero for good. It is let go here, once it has also left the hold window.
            let decayed = if decayed.is_finite() { decayed } else { 0.0 };
            state.env = if held > decayed { held } else { decayed };
            if new_abs > state.env {
                // Start a ramp that lands on `new_abs` exactly as it leaves the delay.
                state.max_abs = new_abs;
                state.delta = (new_abs - state.env) / ramp_divisor;
                state.env += state.delta;
                state.ramp_count = lookahead;
            }
        }
        state.env
    }

    /// A whole interleaved block, in place.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || buffer.is_empty() {
            return;
        }
        if channels.min(MAX_CHANNELS) > self.linked {
            self.process_grouped(buffer, channels);
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            self.process_frame(frame);
        }
    }
}

/// THD+N of a steady sine, for the tests of both limiters: the residual after a least-squares fit
/// of the fundamental and DC, against the fundamental. `x` must span a whole number of periods.
#[cfg(test)]
pub(crate) fn thd_n(x: &[Real], hz: f64, sample_rate: Real) -> f64 {
    let w = std::f64::consts::TAU * hz / f64::from(sample_rate);
    let basis = |n: usize| {
        let (s, c) = (w * n as f64).sin_cos();
        [s, c, 1.0]
    };
    let mut gram = [[0.0_f64; 3]; 3];
    let mut rhs = [0.0_f64; 3];
    for (n, &v) in x.iter().enumerate() {
        let b = basis(n);
        for (row, &bi) in gram.iter_mut().zip(&b) {
            for (cell, &bj) in row.iter_mut().zip(&b) {
                *cell += bi * bj;
            }
        }
        for (r, &bi) in rhs.iter_mut().zip(&b) {
            *r += bi * f64::from(v);
        }
    }
    // Cramer's rule on the 3×3 normal equations.
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let whole = det(gram);
    let mut fit = [0.0_f64; 3];
    for (k, coefficient) in fit.iter_mut().enumerate() {
        let mut m = gram;
        for (row, &r) in m.iter_mut().zip(&rhs) {
            row[k] = r;
        }
        *coefficient = det(m) / whole;
    }
    let residual: f64 = x
        .iter()
        .enumerate()
        .map(|(n, &v)| {
            let b = basis(n);
            let e = f64::from(v) - (fit[0] * b[0] + fit[1] * b[1] + fit[2] * b[2]);
            e * e
        })
        .sum();
    (residual / x.len() as f64).sqrt() / ((fit[0] * fit[0] + fit[1] * fit[1]) / 2.0).sqrt()
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
    fn the_meter_reads_how_far_over_the_ceiling_the_envelope_is() {
        let mut l = limiter(-6.0);
        assert_eq!(l.reduction_db(0), 0.0);
        for _ in 0..200 {
            let mut frame = [1.0_f32];
            l.process_frame(&mut frame);
        }
        // Full scale against a −6 dB ceiling is six decibels of reduction.
        assert!(
            (l.reduction_db(0) - 6.0).abs() < 0.1,
            "{}",
            l.reduction_db(0)
        );
        assert_eq!(
            l.reduction_db(MAX_CHANNELS + 1),
            0.0,
            "an unknown channel reads zero"
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
        // And not stuck at silence either: an infinity decays to itself, so without the guard in
        // the release the gain stayed at zero for good. It lets go once the infinity has left
        // the hold window.
        assert!(
            (frame[0] - 0.1).abs() < 1e-6,
            "the gain never came back: {}",
            frame[0]
        );
    }

    /// Dynamic Boost's limiter as it was before 0.4.0's hold: 0.75 ms, the original's beta.
    fn original_timing(hold_ms: Real) -> LookaheadLimiter {
        let mut l = LookaheadLimiter::new(FS, 1.0, 0.75, 10.0);
        l.set_release_beta(0.997_956_2);
        l.set_hold_ms(hold_ms);
        l
    }

    #[test]
    fn a_low_sine_cannot_drive_the_envelope_past_its_own_peak() {
        // Audit #8, the report's scenario: a 50 Hz sine at twice the ceiling. Every rising sample
        // retargets the ramp and only ever steepens it, so the original's envelope kept climbing
        // after the crest and reached 2.26 — 1.06 dB of reduction nobody asked for, every
        // half-cycle. Clamped, it stops at 2.00. With no hold, so this is the clamp alone.
        for hold_ms in [0.0, DEFAULT_HOLD_MS] {
            let mut l = original_timing(hold_ms);
            let w = std::f32::consts::TAU * 50.0 / FS;
            let mut highest: Real = 0.0;
            let mut loudest_out: Real = 0.0;
            for n in 0..96_000_usize {
                let mut frame = [2.0 * (w * n as Real).sin()];
                l.process_frame(&mut frame);
                if n >= 48_000 {
                    highest = highest.max(l.envelope(0));
                    loudest_out = loudest_out.max(frame[0].abs());
                }
            }
            assert!(
                highest <= 2.0 * 1.000_001,
                "hold {hold_ms}: the envelope reached {highest}, was 2.26"
            );
            // The same wall, now met exactly rather than half a decibel short of it.
            assert!(
                loudest_out <= 1.000_001 && loudest_out > 0.9999,
                "hold {hold_ms}: {loudest_out}"
            );
        }
    }

    #[test]
    fn every_channel_is_limited_by_the_same_gain() {
        // Audit R2 at the limiter itself: one envelope, from the loudest channel, applied to all
        // of them, so the ratio between channels leaves as it arrived. A channel past
        // `MAX_CHANNELS` is neither limited nor listened to.
        let mut l = LookaheadLimiter::new(FS, 1.0, 1.0, 80.0);
        let w = std::f32::consts::TAU * 440.0 / FS;
        let delay = l.latency_frames();
        let mut history: Vec<[Real; MAX_CHANNELS + 1]> = Vec::new();
        let mut quietest_gain: Real = 1.0;
        for n in 0..24_000_usize {
            let s = (w * n as Real).sin();
            let mut frame = [0.0; MAX_CHANNELS + 1];
            for (k, sample) in frame.iter_mut().enumerate().take(MAX_CHANNELS) {
                *sample = s * (0.2 + 0.2 * k as Real);
            }
            frame[MAX_CHANNELS] = 50.0 * s;
            history.push(frame);
            l.process_frame(&mut frame);
            if n < delay {
                continue;
            }
            let arrived = history[n - delay];
            if arrived[0].abs() < 0.05 {
                continue;
            }
            let gain = frame[0] / arrived[0];
            quietest_gain = quietest_gain.min(gain);
            for k in 1..MAX_CHANNELS {
                let other = frame[k] / arrived[k];
                assert!(
                    (other - gain).abs() <= gain * 1e-5,
                    "frame {n}: channel {k} got {other}, channel 0 got {gain}"
                );
            }
            // Passed straight through, undelayed, as it always was.
            assert_eq!(frame[MAX_CHANNELS], history[n][MAX_CHANNELS], "frame {n}");
        }
        // The loudest channel is 1.6 against a ceiling of 1.0, so everything came down 4.1 dB.
        let reduction_db = -20.0 * quietest_gain.log10();
        assert!(
            (reduction_db - 4.08).abs() < 0.05,
            "the channels came down {reduction_db} dB"
        );
    }

    #[test]
    fn a_decaying_note_is_let_down_behind_its_own_peaks_rather_than_between_them() {
        // Audit R1, what "the hold follows the program" means. A 40 Hz note decaying from 12 dB
        // over the ceiling. Released at once, the original's 10 ms envelope let go between every
        // two crests, 12.5 ms apart, and caught the next one: a 6 dB swing of the gain every
        // half-cycle, which is the distortion. Held, the envelope sits on the loudest crest of the
        // last 20 ms and comes down with the note, a crest's worth of decay at a time: 0.36 dB per
        // 12.5 ms, which is exactly what a 300 ms decay loses in 12.5 ms.
        let deepest_fall = |hold_ms: Real| {
            let mut l = original_timing(hold_ms);
            let w = std::f32::consts::TAU * 40.0 / FS;
            let mut envelope = Vec::new();
            for n in 0..24_000_usize {
                let t = n as Real / FS;
                let mut frame = [4.0 * (-t / 0.3).exp() * (w * n as Real).sin()];
                l.process_frame(&mut frame);
                envelope.push(l.envelope(0));
            }
            // From 50 ms, once the note is established, to 350 ms, while it is still over the
            // ceiling: the largest fall of the envelope within any 12.5 ms.
            let span = (FS * 0.0125) as usize;
            let mut deepest: Real = 0.0;
            for n in (FS * 0.05) as usize..(FS * 0.35) as usize {
                let start = envelope[n];
                let lowest = envelope[n..n + span].iter().fold(start, |m, e| m.min(*e));
                deepest = deepest.max(20.0 * (start / lowest).log10());
            }
            let came_down = envelope[(FS * 0.35) as usize] / envelope[(FS * 0.05) as usize];
            (deepest, came_down)
        };
        let (released_at_once, _) = deepest_fall(0.0);
        let (held, came_down) = deepest_fall(DEFAULT_HOLD_MS);
        assert!(
            released_at_once > 5.0,
            "the original's release should swing the gain: {released_at_once} dB"
        );
        assert!(
            held < 0.4,
            "held, the envelope still fell {held} dB inside one half-cycle"
        );
        // And it did follow the note down: 300 ms of a 300 ms decay is 8.7 dB.
        assert!(
            (20.0 * came_down.log10() + 8.7).abs() < 1.0,
            "the envelope came down {} dB with the note",
            20.0 * came_down.log10()
        );
    }

    #[test]
    fn the_hold_is_clamped_and_zero_releases_the_frame_after_the_peak_leaves() {
        let mut l = LookaheadLimiter::new(FS, 1.0, 1.0, 10.0);
        assert!((l.hold_ms() - DEFAULT_HOLD_MS).abs() < 1e-6);
        l.set_hold_ms(1_000.0);
        assert!((l.hold_ms() - MAX_HOLD_MS).abs() < 1e-6);
        l.set_hold_ms(-5.0);
        assert_eq!(l.hold_ms(), 0.0);
        l.set_hold_ms(Real::NAN);
        assert!((l.hold_ms() - DEFAULT_HOLD_MS).abs() < 1e-6);

        // Hold zero is the original's timing: one loud sample, and the envelope is falling the
        // frame after it has left the delay line.
        for (hold_ms, held_frames) in [(0.0, 0), (10.0, 480)] {
            let mut l = LookaheadLimiter::new(FS, 1.0, 1.0, 10.0);
            l.set_hold_ms(hold_ms);
            let delay = l.latency_frames();
            let mut envelope = Vec::new();
            for n in 0..4_000_usize {
                let mut frame = [if n == 100 { 4.0 } else { 0.0 }];
                l.process_frame(&mut frame);
                envelope.push(l.envelope(0));
            }
            let left = 100 + delay;
            assert!((envelope[left] - 4.0).abs() < 1e-5, "{}", envelope[left]);
            let falling_from = (left..4_000)
                .find(|&n| envelope[n] < envelope[left])
                .expect("it never released");
            assert!(
                falling_from - left > held_frames && falling_from - left <= held_frames * 9 / 8 + 1,
                "hold {hold_ms}: released {} frames after the peak left",
                falling_from - left
            );
        }
    }

    #[test]
    fn a_change_of_hold_too_small_to_move_the_window_still_forgets_the_held_peaks() {
        // 20 ms and 20.01 ms are both 120 frames a segment at 48 kHz, so `design` keeps the
        // window; the peak held before the change must not survive it.
        let mut l = LookaheadLimiter::new(FS, 1.0, 1.0, 10.0);
        let delay = l.latency_frames();
        for n in 0..=100 + delay {
            let mut frame = [if n == 100 { 4.0 } else { 0.0 }];
            l.process_frame(&mut frame);
        }
        let at_peak = l.envelope(0);
        assert!((at_peak - 4.0).abs() < 1e-5, "{at_peak}");
        l.set_hold_ms(DEFAULT_HOLD_MS + 0.01);
        let mut frame = [0.0];
        l.process_frame(&mut frame);
        assert!(
            l.envelope(0) < at_peak,
            "the envelope stayed at {} after the hold changed",
            l.envelope(0)
        );
    }

    #[test]
    fn only_the_channels_linked_together_share_a_gain() {
        // Audit R2 as Dynamic Boost uses it on surround before it knows the sides: the front pair
        // linked, every other channel on an envelope of its own. A burst on channel 2 is limited
        // on channel 2 alone; one on channel 1 turns channels 0 and 1 down by the same ratio and
        // nothing else. Nothing leaves above the ceiling on any channel, and linking everything
        // again gives back the one envelope.
        let mut l = LookaheadLimiter::new(FS, 1.0, 1.0, 80.0);
        l.set_linked(&[true, true]);
        let delay = l.latency_frames();
        let w = std::f32::consts::TAU * 440.0 / FS;
        let burst_on_2 = 4_800..9_600;
        let burst_on_1 = 19_200..24_000;
        let mut arrived: Vec<[Real; 4]> = Vec::new();
        let mut deepest_on_0: Real = 1.0;
        for n in 0..36_000_usize {
            let s = (w * n as Real).sin();
            let mut frame = [0.5 * s; 4];
            if burst_on_2.contains(&n) {
                frame[2] = 3.0 * s;
            }
            if burst_on_1.contains(&n) {
                frame[1] = 2.0 * s;
            }
            arrived.push(frame);
            l.process_frame(&mut frame);
            assert!(
                frame.iter().all(|x| x.abs() <= 1.000_001),
                "frame {n}: {frame:?}"
            );
            let Some(input) = n.checked_sub(delay).map(|at| arrived[at]) else {
                continue;
            };
            if input[0].abs() < 0.1 {
                continue;
            }
            let gains: Vec<Real> = frame.iter().zip(input).map(|(o, i)| o / i).collect();
            // Channel 3 is never linked to anything loud: untouched throughout.
            assert!((gains[3] - 1.0).abs() < 1e-5, "frame {n}: {gains:?}");
            // The pair always leaves with one gain.
            assert!((gains[0] - gains[1]).abs() < 1e-5, "frame {n}: {gains:?}");
            if burst_on_2.contains(&(n - delay)) {
                assert!((gains[0] - 1.0).abs() < 1e-5, "frame {n}: {gains:?}");
            }
            deepest_on_0 = deepest_on_0.min(gains[0]);
        }
        assert!(
            deepest_on_0 < 0.55,
            "channel 0 should have come down with channel 1: {deepest_on_0}"
        );
        assert_eq!(l.envelope(0), l.envelope(1), "the pair reads one envelope");

        l.link_all();
        for channel in 1..MAX_CHANNELS {
            assert_eq!(l.envelope(channel), l.envelope(0), "channel {channel}");
        }
    }

    #[test]
    fn a_block_limits_every_linking_to_the_bit_as_frames_one_at_a_time_do() {
        // Audit R2's cost. With the channels not all linked, `process` runs each envelope over the
        // whole block in turn rather than every envelope a frame at a time, which took Dynamic
        // Boost on 5.1 from 33.6 ns a frame back to 20.0, where the original's envelope per
        // channel had it at 20.8. Nothing may change for it: every sample and every envelope is
        // what `process_frame` gives, frame after frame, for the linkings Dynamic Boost asks for,
        // for none at all, and for a stream wider than the arena, across blocks of any length and
        // a change of rate.
        let linkings: [(usize, &[bool]); 5] = [
            (6, &[true, true, false, false, true, true]),
            (6, &[true, true]),
            (8, &[true, true, false, false, true, true, true, true]),
            (10, &[false, true, true]),
            (3, &[]),
        ];
        for (channels, linked) in linkings {
            let mut by_block = LookaheadLimiter::new(FS, 1.0, 1.0, 80.0);
            let mut by_frame = LookaheadLimiter::new(FS, 1.0, 1.0, 80.0);
            by_block.set_linked(linked);
            by_frame.set_linked(linked);
            let mut seed = 0x2545_f491_u32;
            let mut noise = move || {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as Real / 16_777_216.0 - 0.5
            };
            let mut n = 0_usize;
            for (block, frames) in [97_usize, 480, 1, 33, 480, 480, 200]
                .into_iter()
                .enumerate()
            {
                if block == 4 {
                    by_block.set_sample_rate(44_100.0);
                    by_frame.set_sample_rate(44_100.0);
                }
                let mut input = vec![0.0; frames * channels];
                for frame in input.chunks_exact_mut(channels) {
                    for (channel, sample) in frame.iter_mut().enumerate() {
                        // Each channel over the ceiling at times of its own, and one burst of
                        // 4x on channel 3 alone.
                        let hz = 60.0 + 70.0 * channel as Real;
                        let tone = (std::f32::consts::TAU * hz * n as Real / FS).sin();
                        let burst = if channel == 3 && (700..900).contains(&n) {
                            4.0
                        } else {
                            1.0
                        };
                        *sample = burst * (1.3 * tone + noise());
                    }
                    n += 1;
                }
                let mut blocked = input.clone();
                by_block.process(&mut blocked, channels);
                let mut framed = input;
                for frame in framed.chunks_exact_mut(channels) {
                    by_frame.process_frame(frame);
                }
                for (at, (a, b)) in blocked.iter().zip(&framed).enumerate() {
                    assert_eq!(
                        a.to_bits(),
                        b.to_bits(),
                        "{channels} channels linked {linked:?}: block {block}, sample {at}"
                    );
                }
                for channel in 0..=MAX_CHANNELS {
                    assert_eq!(
                        by_block.envelope(channel).to_bits(),
                        by_frame.envelope(channel).to_bits(),
                        "{channels} channels linked {linked:?}: block {block}, channel {channel}"
                    );
                }
            }
            assert!(
                (0..channels.min(MAX_CHANNELS)).any(|channel| by_block.envelope(channel) > 1.0),
                "the fixture should keep the limiter working"
            );
        }
    }
}
