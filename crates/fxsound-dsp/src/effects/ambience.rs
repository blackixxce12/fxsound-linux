//! Ambience — the Lexicon-224-style figure-of-eight plate reverb.
//!
//! Ports `dsp/ptechDsp/Lex/Lex32/Lex32.c` (`dspsLexReverbProcess32`) with the delay constants and
//! the struct layout from `dsp/ptutil/include/c_lex.h`. It is a textbook Dattorro plate ("Effect
//! Design Part 1", JAES 1997): one input low-pass, four cascaded all-pass diffusers, then a
//! figure-of-eight tank of two decay diffusers, two tapped all-passes, four multi-tap delays and
//! two damping low-passes, read out through a fixed tap matrix.
//!
//! Two things about the shipped build decide how this reads:
//!
//! * **There is no modulation.** FxSound compiles with `PT_DSP_BUILD == PT_DSP_DFX`, so every
//!   `#if (PT_DSP_BUILD == PT_DSP_DSPFX)` block is gone (`Lex32.c:399-460, 547-582`). The LFO
//!   tables are still *built* at init and never read; the two decay diffusers use fixed **integer**
//!   delays with no interpolation. This port implements what ships and does not allocate the
//!   16 386 floats of cosine tables that the original leaves dead in its arena.
//! * **The tank is much shorter than a "reverb" suggests.** In the shipping MUSIC2 mode the decay
//!   coefficient only ever reaches 0.206 (`docs/spec/10-dsp-effects.md` §7.4), so this is a short
//!   ambience wash, not a hall. A loop gain near 1.0 means a mapping bug, not a lush preset.
//!
//! The original packs all thirteen delay lines into one circular "Master" buffer whose single
//! pointer walks forward by each segment's length in turn (`Lex32.c:330-342` and every block
//! after), which makes each segment's write position advance by exactly one sample per frame and
//! makes `next_out` — the value about to be overwritten — the *oldest* sample of the *next*
//! segment. Per-line ring buffers give that for free, so this port uses thirteen rings carved out
//! of one arena: same arithmetic, one allocation, and no pointer games to get wrong.

use super::{Effect, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};
use crate::biquad::Real;
use std::fmt;

// ---------------------------------------------------------------------------------------------
// Fixed delay constants (`c_lex.h:57-99`).
//
// The originals are `ms/1000.0` double literals. The ones the original multiplies by the sample
// rate are cast to `realtype` first (`Lex32.c:174-187`, f32 maths); the ones it multiplies by
// `roomsize * fs` are left as doubles (`Lex32.c:195-236`, f64 maths). The split is reproduced here
// because the products are truncated to integers, and matching the cast is what makes the line
// lengths match the original sample for sample.
// ---------------------------------------------------------------------------------------------

/// `LEX_PREDELAY_LEN` — 100 ms of pre-delay memory, of which the shipped build reads one sample.
const PREDELAY_LEN_S: Real = (100.0_f64 / 1000.0) as Real;
/// `LEX_MAX_MODULATION_DELAY` — headroom the original reserves around the decay diffusers for an
/// LFO that this build never runs.
const MAX_MODULATION_DELAY_S: Real = (2.0_f64 / 1000.0) as Real;

const LAT1_LEN_S: Real = (4.77_f64 / 1000.0) as Real;
const LAT2_LEN_S: Real = (3.595_f64 / 1000.0) as Real;
const LAT3_LEN_S: Real = (12.73_f64 / 1000.0) as Real;
const LAT4_LEN_S: Real = (9.31_f64 / 1000.0) as Real;

const LAT5_LEN_S: f64 = 22.6 / 1000.0;
const D1_TAP1_S: f64 = 10.1 / 1000.0;
const D1_TAP2_S: f64 = 66.9 / 1000.0;
const D1_TAP3_S: f64 = 121.9 / 1000.0;
const D1_TAP4_S: f64 = 149.6 / 1000.0;
/// 6.28 ms. Not τ — the resemblance is a coincidence of the millisecond value, and the literal is
/// kept identical to `c_lex.h:81` so the two files can be read side by side.
#[allow(clippy::approx_constant)]
const LAT6_TAP1_S: f64 = 6.28 / 1000.0;
const LAT6_TAP2_S: f64 = 41.26 / 1000.0;
const LAT6_LEN_S: f64 = 60.5 / 1000.0;
const D2_TAP1_S: f64 = 35.8 / 1000.0;
const D2_TAP2_S: f64 = 89.8 / 1000.0;
const D2_TAP3_S: f64 = 125.0 / 1000.0;
const LAT7_LEN_S: f64 = 30.5 / 1000.0;
const D3_TAP1_S: f64 = 10.1 / 1000.0;
const D3_TAP2_S: f64 = 70.9 / 1000.0;
const D3_TAP3_S: f64 = 99.9 / 1000.0;
const D3_TAP4_S: f64 = 141.7 / 1000.0;
const LAT8_TAP1_S: f64 = 11.25 / 1000.0;
const LAT8_TAP2_S: f64 = 64.3 / 1000.0;
const LAT8_LEN_S: f64 = 89.2 / 1000.0;
const D4_TAP1_S: f64 = 4.065 / 1000.0;
const D4_TAP2_S: f64 = 67.1 / 1000.0;
const D4_TAP3_S: f64 = 106.3 / 1000.0;

// ---------------------------------------------------------------------------------------------
// Fixed coefficients. None of these is reachable from the UI; they are written once at init
// (`Play32.c:288-315`) or once per session (`dfxpComm.cpp:1314-1440`).
// ---------------------------------------------------------------------------------------------

/// Input-diffuser coefficient for AP1/AP2 (`Lex32.c:95`).
const LAT1_COEFF: Real = 0.75;
/// Input-diffuser coefficient for AP3/AP4 (`Lex32.c:96`).
const LAT3_COEFF: Real = 0.625;
/// Decay-diffuser coefficient for AP5/AP7 (`Lex32.c:97`). Used with the opposite sign convention
/// to the input diffusers, which is what makes them *decay* diffusers.
const LAT5_COEFF: Real = 0.70;
/// The pre-delay tap, in samples (`Lex32.c:107`). Yes, one sample: the 100 ms line exists so the
/// Lexicon plug-in could expose a pre-delay knob, and FxSound never does.
const PRE_DELAY: usize = 1;
/// Final output scaling, `0.6 * 0.5` (`Lex32.c:667-668`).
const OUTPUT_SCALE: Real = 0.6 * 0.5;
/// `DSP_DENORM_BIAS` (`boardrv1.h:115`), added to both inputs at `Lex32.c:323-324`.
const DENORM_BIAS: Real = 1.0e-36;

/// `DSP_LEX_ROOM_SIZE_MIN_VALUE` / `MAX` (`c_lex.h:50-51`).
const ROOM_SIZE_MIN: Real = 0.5;
const ROOM_SIZE_MAX: Real = 1.5;
/// `DSP_PLAY_LEX_ROOM_SIZE_MIDI` (`c_play.h:61`) — hard-coded, never reachable from the UI
/// (`dfxpComm.cpp:1326`).
const ROOM_SIZE_MIDI: usize = 64;
/// The room size the UI can never change: **1.0039370**.
const ROOM_SIZE: Real = linear_qnt(ROOM_SIZE_MIN, ROOM_SIZE_MAX, ROOM_SIZE_MIDI);

/// Both one-pole corner frequencies come out of an exponential quantiser spanning 1…20 kHz
/// (`dspfxp_studioverb.h:30-34`, `dfxpQnt.cpp:294-342`).
const FREQ_MIN_KHZ: Real = 1.0;
const FREQ_MAX_KHZ: Real = 20.0;
/// `DSP_PLAY_LEX_ROLLOFF_MIDI` (`c_play.h:62`) — feeds the struct's `bandwidth`, the *input*
/// low-pass. The cross-naming between `LEX_ROLLOFF`/`bandwidth` and `LEX_DAMPING`/`damping` is in
/// the original (`c_lex.h:131-143` against `:207-219`); both ranges are identical so it is
/// harmless, and "fixing" it would only make this file disagree with the C.
const ROLLOFF_MIDI: usize = 89;
/// `DSP_PLAY_LEX_DAMPING_MIDI` (`c_play.h:63`) — feeds the in-tank damping low-pass.
const DAMPING_MIDI: usize = 81;

// ---------------------------------------------------------------------------------------------
// The user-facing mapping (`dfxp_CommunicateAmbience`, `dfxpComm.cpp:571-679`).
// ---------------------------------------------------------------------------------------------

/// `DFXP_MUSIC_MODE2_AMBIENCE_FACTOR` (`dfxpDefs.h:129`). MUSIC2 is the only mode FxSound 13 ships
/// (`DfxDspPreset.cpp:242`), and the public API has no way to leave it, so the warp is
/// unconditional here rather than a mode switch that can only ever hold one value.
const MUSIC2_AMBIENCE_FACTOR: Real = 0.34;
/// `DFXP_MIN_EFFECTIVE_MIDI_AMBIENCE` (`dfxpComm.cpp:50`). At or below this warped value the
/// effect is bypassed outright (`dfxpComm.cpp:1688`) — which is why slider positions 1, 2 and 3 do
/// nothing at all. That is shipped behaviour, reproduced deliberately.
const MIN_EFFECTIVE_MIDI: i32 = 12;
/// `PLY_DECAY_MIN_VALUE` / `PLY_DECAY_MAX_VALUE` (`c_play.h:100-101`), exponential curve
/// (`dfxpQnt.cpp:144-151`).
const DECAY_MIN: Real = 0.095;
const DECAY_MAX: Real = 0.95;
/// `0.21 * PLY_AMBIENCE_BOOST_FACTOR` and `0.69 * PLY_AMBIENCE_BOOST_FACTOR`, the wet/dry pair for
/// warped values above 40 (`dfxpComm.cpp:621-626`; `PLY_AMBIENCE_BOOST_FACTOR = 1.3`, `c_play.h:96`).
const WET_MAX: Real = 0.273;
const DRY_MIN: Real = 0.897;
/// Below 40 the pair is warped so that wet tends to zero at the bypass threshold
/// (`dfxpComm.cpp:628-632`). The span is 40 − 12 = 28 warped steps.
const WARP_SPAN: f64 = 40.0 - 12.0;
const DRY_SPAN: Real = 1.0 - 0.897;

// ---------------------------------------------------------------------------------------------
// The thirteen delay lines, in the order the original's pointer walks them (`Lex32.c:174-236`).
// ---------------------------------------------------------------------------------------------

const PRE: usize = 0;
const LAT1: usize = 1;
const LAT2: usize = 2;
const LAT3: usize = 3;
const LAT4: usize = 4;
const LAT5: usize = 5;
const D1: usize = 6;
const LAT6: usize = 7;
const D2: usize = 8;
const LAT7: usize = 9;
const D3: usize = 10;
const LAT8: usize = 11;
const D4: usize = 12;
const NUM_LINES: usize = 13;

/// `(unsigned long)(fs * (realtype)seconds)` — f32 throughout, truncating (`Lex32.c:174-187`).
///
/// `as usize` on `f32` saturates rather than wrapping, so a negative or non-finite rate yields 0
/// and is then floored by the caller.
fn samples_at_rate(sample_rate: Real, seconds: Real) -> usize {
    let n = sample_rate * seconds;
    if n > 0.0 { n as usize } else { 0 }
}

/// `(unsigned long)(seconds * r_roomsize)` where `r_roomsize = roomsize * fs` is an `f32` and the
/// millisecond constant stays a `double` (`Lex32.c:192-236`).
fn samples_in_room(room_rate: Real, seconds: f64) -> usize {
    let n = seconds * f64::from(room_rate);
    if n > 0.0 { n as usize } else { 0 }
}

/// `QNT_RESPONSE_LINEAR` (`Qntitor.cpp:169-178`): `lo + (hi-lo)/127 * index`, endpoint hard-set.
const fn linear_qnt(lo: Real, hi: Real, index: usize) -> Real {
    if index >= 127 {
        return hi;
    }
    lo + ((hi - lo) / 127.0) * index as Real
}

/// `QNT_RESPONSE_EXP` (`Qntitor.cpp:302-321`): a geometric ladder built by *repeated
/// multiplication*, with both endpoints hard-set.
///
/// The repeated product is not the same as `lo * (hi/lo)^(i/127)` — it accumulates `f32` rounding,
/// and the difference is visible in the sixth digit of every coefficient this drives. Reproducing
/// the accumulation is what makes the damping coefficient come out at the 0.408290 the original
/// hard-codes at `Play32.c:308`.
fn exp_qnt(lo: Real, hi: Real, index: usize) -> Real {
    if index == 0 || lo <= 0.0 {
        return lo;
    }
    if index >= 127 {
        return hi;
    }
    let factor = f64::from(hi / lo).powf(1.0 / 127.0) as Real;
    let mut value = lo;
    for _ in 0..index {
        value *= factor;
    }
    value
}

/// `filtDesignSimple1rstLowPass` (`Fil12But.cpp:37-46`) — the `a0` of `y = (1-a0)x + a0·y₋₁`,
/// placing the −3 dB point exactly at `freq_hz`.
///
/// Above Nyquist the −3 dB match stops meaning anything and the coefficient turns back down; the
/// original has the same behaviour and the pole stays well inside the unit circle, so a 16 kHz
/// stream (where the 8.16 kHz corner sits above Nyquist) is merely dull, not unstable.
fn one_pole_coeff(freq_hz: Real, sample_rate: Real) -> Real {
    // `Qnt2But.cpp:175` multiplies by the sampling *period*, in f32; kept for bit-fidelity.
    let omega = std::f32::consts::TAU * freq_hz * (1.0 / sample_rate);
    let cos_om = f64::from(omega).cos() as Real;
    // (c-1)(c-3) >= 0 for every c in [-1, 1], so the root is always real.
    let root = f64::from(cos_om * cos_om - 4.0 * cos_om + 3.0).sqrt() as Real;
    2.0 - cos_om - root
}

/// One delay line: a window of the shared arena plus the position that is written next.
#[derive(Clone, Copy, Debug, Default)]
struct Line {
    start: usize,
    len: usize,
    write: usize,
}

impl Line {
    /// The sample written `delay` frames ago. `delay == len` is the oldest sample still held, and
    /// is only correct *before* this frame's write — which is the order [`Ambience::tick`] uses.
    #[inline(always)]
    fn read(&self, arena: &[Real], delay: usize) -> Real {
        let offset = if delay <= self.write {
            self.write - delay
        } else {
            self.write + self.len - delay
        };
        arena.get(self.start + offset).copied().unwrap_or(0.0)
    }
}

/// Every rate-dependent length and tap offset, computed the way the original computes them.
///
/// Separated from [`Ambience`] so the worst-case arena size is just `Layout::for_rate(MAX).total()`
/// — no duplicated arithmetic, and no way for the allocation to drift from the addressing.
#[derive(Clone, Copy, Debug)]
struct Layout {
    lengths: [usize; NUM_LINES],
    lat5_delay: usize,
    lat7_delay: usize,
    d1_taps: [usize; 3],
    lat6_taps: [usize; 2],
    d2_taps: [usize; 2],
    d3_taps: [usize; 3],
    lat8_taps: [usize; 2],
    d4_taps: [usize; 2],
}

impl Layout {
    fn for_rate(sample_rate: Real) -> Self {
        let fs = sample_rate;
        // `Lex32.c:192` — the room size is folded into the rate once, in f32.
        let room_rate = ROOM_SIZE * fs;

        // Headroom for the modulation this build never applies (`Lex32.c:197, 219`). It is part of
        // the line length, so dropping it would shift every later line and change the sound of
        // nothing at all — but it would also stop the memory budget matching the original's.
        let headroom = samples_at_rate(fs, MAX_MODULATION_DELAY_S) + 1;

        let lat5_delay = samples_in_room(room_rate, LAT5_LEN_S).max(1);
        let lat7_delay = samples_in_room(room_rate, LAT7_LEN_S).max(1);

        let mut lengths = [1usize; NUM_LINES];
        lengths[PRE] = samples_at_rate(fs, PREDELAY_LEN_S).max(PRE_DELAY + 1);
        lengths[LAT1] = samples_at_rate(fs, LAT1_LEN_S).max(1);
        lengths[LAT2] = samples_at_rate(fs, LAT2_LEN_S).max(1);
        lengths[LAT3] = samples_at_rate(fs, LAT3_LEN_S).max(1);
        lengths[LAT4] = samples_at_rate(fs, LAT4_LEN_S).max(1);
        lengths[LAT5] = lat5_delay + headroom;
        lengths[D1] = samples_in_room(room_rate, D1_TAP4_S).max(1);
        lengths[LAT6] = samples_in_room(room_rate, LAT6_LEN_S).max(1);
        lengths[D2] = samples_in_room(room_rate, D2_TAP3_S).max(1);
        lengths[LAT7] = lat7_delay + headroom;
        lengths[D3] = samples_in_room(room_rate, D3_TAP4_S).max(1);
        lengths[LAT8] = samples_in_room(room_rate, LAT8_LEN_S).max(1);
        lengths[D4] = samples_in_room(room_rate, D4_TAP3_S).max(1);

        // A tap of 0 would read the slot this frame is about to overwrite, and a tap past the end
        // of its own line would read a neighbour's memory. Neither can happen at any rate in
        // 16k..192k — the ratios are fixed — but the clamp makes that a property of the code
        // rather than of arithmetic the reader has to redo.
        let tap =
            |offset: f64, line: usize| samples_in_room(room_rate, offset).clamp(1, lengths[line]);

        Self {
            lengths,
            lat5_delay: lat5_delay.min(lengths[LAT5]),
            lat7_delay: lat7_delay.min(lengths[LAT7]),
            d1_taps: [tap(D1_TAP1_S, D1), tap(D1_TAP2_S, D1), tap(D1_TAP3_S, D1)],
            lat6_taps: [tap(LAT6_TAP1_S, LAT6), tap(LAT6_TAP2_S, LAT6)],
            d2_taps: [tap(D2_TAP1_S, D2), tap(D2_TAP2_S, D2)],
            d3_taps: [tap(D3_TAP1_S, D3), tap(D3_TAP2_S, D3), tap(D3_TAP3_S, D3)],
            lat8_taps: [tap(LAT8_TAP1_S, LAT8), tap(LAT8_TAP2_S, LAT8)],
            d4_taps: [tap(D4_TAP1_S, D4), tap(D4_TAP2_S, D4)],
        }
    }

    /// `MasterLen` (`Lex32.c:175-236`) — the total delay memory the reverb needs at this rate.
    fn total(&self) -> usize {
        self.lengths.iter().sum()
    }
}

/// The Lexicon-style plate reverb.
///
/// All thirteen ring buffers live in one arena sized in [`Ambience::new`] for
/// [`MAX_SAMPLE_RATE`], so changing the stream format re-slices it but never reallocates.
pub struct Ambience {
    /// One allocation, touched linearly. ~663 kB at 192 kHz; ~152 kB at 44.1 kHz.
    arena: Box<[Real]>,
    lines: [Line; NUM_LINES],

    lat5_delay: usize,
    lat7_delay: usize,
    d1_taps: [usize; 3],
    lat6_taps: [usize; 2],
    d2_taps: [usize; 2],
    d3_taps: [usize; 3],
    lat8_taps: [usize; 2],
    d4_taps: [usize; 2],

    /// Input low-pass (`bandwidth`, fed by `LEX_ROLLOFF`).
    bandwidth: Real,
    one_minus_bandwidth: Real,
    /// In-tank low-pass (`damping`, fed by `LEX_DAMPING`).
    damping: Real,
    one_minus_damping: Real,
    /// The one user-driven tank coefficient.
    decay: Real,
    /// AP6/AP8 coefficient, derived from `decay` (`dfxpComm.cpp:613-617`).
    lat6_coeff: Real,
    wet_gain: Real,
    dry_gain: Real,

    bandwidth_z: Real,
    damp1_z: Real,
    damp2_z: Real,
    /// The tank's cross-feed: D4's longest tap, read one frame later by AP5 (`Lex32.c:422, 665`).
    d4_out: Real,

    sample_rate: Real,
    amount: Real,
    active: bool,
    /// Which channels the single stereo instance runs over; `None` means the first two.
    front_pair: Option<(usize, usize)>,
}

impl Ambience {
    /// Which channels are the front pair. `None` falls back to the first two.
    pub fn set_front_pair(&mut self, pair: Option<(usize, usize)>) {
        self.front_pair = pair;
    }

    /// Build the reverb, sized for the worst case the original supports.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let arena_len = Layout::for_rate(MAX_SAMPLE_RATE).total();
        let mut effect = Self {
            arena: vec![0.0; arena_len].into_boxed_slice(),
            lines: [Line::default(); NUM_LINES],
            lat5_delay: 1,
            lat7_delay: 1,
            d1_taps: [1; 3],
            lat6_taps: [1; 2],
            d2_taps: [1; 2],
            d3_taps: [1; 3],
            lat8_taps: [1; 2],
            d4_taps: [1; 2],
            bandwidth: 0.0,
            one_minus_bandwidth: 1.0,
            damping: 0.0,
            one_minus_damping: 1.0,
            decay: 0.0,
            lat6_coeff: 0.25,
            wet_gain: 0.0,
            dry_gain: 1.0,
            bandwidth_z: 0.0,
            damp1_z: 0.0,
            damp2_z: 0.0,
            d4_out: 0.0,
            sample_rate: clamp_rate(sample_rate),
            amount: 0.0,
            active: false,
            front_pair: None,
        };
        effect.design();
        effect.set_amount(0.0);
        effect
    }

    /// Re-slice the arena and redesign the two one-pole filters for the current rate.
    ///
    /// Every length is `floor(constant · rate)`, so the layout at [`MAX_SAMPLE_RATE`] — the one the
    /// arena was sized from — is the largest that can be asked for. The rate is clamped before it
    /// gets here, which is what makes that an invariant rather than a hope.
    fn design(&mut self) {
        let layout = Layout::for_rate(self.sample_rate);
        debug_assert!(layout.total() <= self.arena.len());

        let mut start = 0;
        for (line, len) in self.lines.iter_mut().zip(layout.lengths) {
            *line = Line {
                start,
                len,
                write: 0,
            };
            start += len;
        }

        self.lat5_delay = layout.lat5_delay;
        self.lat7_delay = layout.lat7_delay;
        self.d1_taps = layout.d1_taps;
        self.lat6_taps = layout.lat6_taps;
        self.d2_taps = layout.d2_taps;
        self.d3_taps = layout.d3_taps;
        self.lat8_taps = layout.lat8_taps;
        self.d4_taps = layout.d4_taps;

        self.bandwidth = one_pole_coeff(
            exp_qnt(FREQ_MIN_KHZ, FREQ_MAX_KHZ, ROLLOFF_MIDI) * 1000.0,
            self.sample_rate,
        );
        self.one_minus_bandwidth = 1.0 - self.bandwidth;
        self.damping = one_pole_coeff(
            exp_qnt(FREQ_MIN_KHZ, FREQ_MAX_KHZ, DAMPING_MIDI) * 1000.0,
            self.sample_rate,
        );
        self.one_minus_damping = 1.0 - self.damping;
    }

    #[inline(always)]
    fn read(&self, line: usize, delay: usize) -> Real {
        self.lines[line].read(&self.arena, delay)
    }

    /// The oldest sample the line still holds — the original's `next_out` (`Lex32.c:340, 361, …`).
    #[inline(always)]
    fn read_oldest(&self, line: usize) -> Real {
        self.read(line, self.lines[line].len)
    }

    #[inline(always)]
    fn write_and_advance(&mut self, line: usize, value: Real) {
        let (index, len) = {
            let line = self.lines[line];
            (line.start + line.write, line.len)
        };
        if let Some(slot) = self.arena.get_mut(index) {
            *slot = value;
        }
        let line = &mut self.lines[line];
        line.write += 1;
        if line.write >= len {
            line.write = 0;
        }
    }

    /// Lattice all-pass (`Lex32.c:354-364`): `y = D_out + k·(x − k·D_out)`.
    #[inline(always)]
    fn allpass(&mut self, line: usize, x: Real, k: Real) -> Real {
        let d_out = self.read_oldest(line);
        let d_in = x - k * d_out;
        self.write_and_advance(line, d_in);
        d_out + k * d_in
    }

    /// The same lattice with two extra output taps (`Lex32.c:500-519`), used by AP6 and AP8.
    ///
    /// The original reads the taps *after* writing; both offsets are strictly inside the line, so
    /// reading them first touches the same memory and keeps every read in one place.
    #[inline(always)]
    fn allpass_tapped(
        &mut self,
        line: usize,
        x: Real,
        k: Real,
        taps: [usize; 2],
    ) -> (Real, Real, Real) {
        let d_out = self.read_oldest(line);
        let tap1 = self.read(line, taps[0]);
        let tap2 = self.read(line, taps[1]);
        let d_in = x - k * d_out;
        self.write_and_advance(line, d_in);
        (d_out + k * d_in, tap1, tap2)
    }

    /// Decay diffuser (`Lex32.c:462-465`) — the all-pass with its signs flipped, and with a fixed
    /// **integer** delay because the shipped build compiles the interpolator out.
    #[inline(always)]
    fn decay_diffuser(&mut self, line: usize, x: Real, delay: usize) -> Real {
        let d_out = self.read(line, delay);
        let d_in = x + LAT5_COEFF * d_out;
        self.write_and_advance(line, d_in);
        d_out - LAT5_COEFF * d_in
    }

    /// Four-tap delay (`Lex32.c:469-489`). The fourth tap is the line's full length.
    #[inline(always)]
    fn delay4(&mut self, line: usize, x: Real, taps: [usize; 3]) -> (Real, Real, Real, Real) {
        let tap4 = self.read_oldest(line);
        let tap1 = self.read(line, taps[0]);
        let tap2 = self.read(line, taps[1]);
        let tap3 = self.read(line, taps[2]);
        self.write_and_advance(line, x);
        (tap1, tap2, tap3, tap4)
    }

    /// Three-tap delay (`Lex32.c:524-540`). The third tap is the line's full length.
    #[inline(always)]
    fn delay3(&mut self, line: usize, x: Real, taps: [usize; 2]) -> (Real, Real, Real) {
        let tap3 = self.read_oldest(line);
        let tap1 = self.read(line, taps[0]);
        let tap2 = self.read(line, taps[1]);
        self.write_and_advance(line, x);
        (tap1, tap2, tap3)
    }

    /// One frame of the tank. Inputs must already carry [`DENORM_BIAS`]; returns the *wet* pair,
    /// before the wet/dry mix.
    #[inline]
    fn tick(&mut self, in1: Real, in2: Real) -> (Real, Real) {
        // Pre-delay: the two inputs are summed to mono here and never separated again. Everything
        // that follows is one mono tank; the stereo image comes entirely from the output taps.
        let pre_out = self.read(PRE, PRE_DELAY);
        self.write_and_advance(PRE, in1 + in2);

        // Input bandwidth low-pass (`Lex32.c:345-346`).
        let mut tmp_a = pre_out * self.one_minus_bandwidth + self.bandwidth * self.bandwidth_z;
        self.bandwidth_z = tmp_a;

        // Four cascaded input diffusers.
        let mut tmp_b = self.allpass(LAT1, tmp_a, LAT1_COEFF);
        tmp_a = self.allpass(LAT2, tmp_b, LAT1_COEFF);
        tmp_b = self.allpass(LAT3, tmp_a, LAT3_COEFF);
        let input_diffuser_out = self.allpass(LAT4, tmp_b, LAT3_COEFF);

        // ---- Tank branch A. Fed by branch B's tail through `d4_out`. ----
        tmp_b = input_diffuser_out + self.decay * self.d4_out;
        tmp_a = self.decay_diffuser(LAT5, tmp_b, self.lat5_delay);

        let taps = self.d1_taps;
        let (tap1, tap2, tap3, tap4) = self.delay4(D1, tmp_a, taps);
        let mut out1 = -tap2;
        let mut out2 = tap1 + tap3;

        tmp_a = tap4 * self.one_minus_damping + self.damping * self.damp1_z;
        self.damp1_z = tmp_a;
        tmp_a *= self.decay;

        let taps = self.lat6_taps;
        let (y, tap1, tap2) = self.allpass_tapped(LAT6, tmp_a, self.lat6_coeff, taps);
        out1 -= tap1;
        out2 -= tap2;

        let taps = self.d2_taps;
        let (tap1, tap2, tap3) = self.delay3(D2, y, taps);
        out1 -= tap1;
        out2 += tap2;

        // ---- Tank branch B. Fed by branch A's tail through D2's longest tap. ----
        tmp_b = input_diffuser_out + self.decay * tap3;
        tmp_a = self.decay_diffuser(LAT7, tmp_b, self.lat7_delay);

        let taps = self.d3_taps;
        let (tap1, tap2, tap3, tap4) = self.delay4(D3, tmp_a, taps);
        out1 += tap1 + tap3;
        out2 -= tap2;

        tmp_a = tap4 * self.one_minus_damping + self.damping * self.damp2_z;
        self.damp2_z = tmp_a;
        tmp_a *= self.decay;

        let taps = self.lat8_taps;
        let (y, tap1, tap2) = self.allpass_tapped(LAT8, tmp_a, self.lat6_coeff, taps);
        out1 -= tap2;
        out2 -= tap1;

        let taps = self.d4_taps;
        let (tap1, tap2, tap3) = self.delay3(D4, y, taps);
        out1 += tap2;
        out2 -= tap1;
        self.d4_out = tap3;

        (out1 * OUTPUT_SCALE, out2 * OUTPUT_SCALE)
    }
}

/// `DAW_MIN_SAMPLING_FREQ`…`DAW_MAX_SAMPLING_FREQ` (`u_dfxp.h:44-45`), with non-finite input
/// pinned to the bottom of the range so the layout can never be asked for a nonsense length.
fn clamp_rate(sample_rate: Real) -> Real {
    if sample_rate.is_finite() {
        sample_rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE)
    } else {
        MIN_SAMPLE_RATE
    }
}

impl Effect for Ambience {
    fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = clamp_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.design();
        self.reset();
    }

    /// `dfxp_CommunicateAmbience` (`dfxpComm.cpp:571-679`) end to end.
    ///
    /// Four stages: normalised → MIDI, MIDI → MUSIC2 warp, warp → bypass test, warp → coefficients.
    /// Nothing here allocates and the longest loop is the 43-step quantiser ladder, so it is safe
    /// to call from the audio thread when a parameter message arrives mid-block.
    fn set_amount(&mut self, amount: Real) {
        self.amount = amount.clamp(0.0, 1.0);

        let midi = fxsound_core::scale::value_to_midi(self.amount);
        // `(int)(midi * 0.34)` — truncating, so the warped value only ever reaches 43.
        let warped = (Real::from(midi) * MUSIC2_AMBIENCE_FACTOR) as i32;
        self.active = warped > MIN_EFFECTIVE_MIDI;

        let decay = exp_qnt(DECAY_MIN, DECAY_MAX, warped.clamp(0, 127) as usize);
        // `pow(decay, roomsize)` (`dfxpComm.cpp:611`): a bigger room takes longer to decay, so the
        // coefficient is compensated rather than the delay lengths.
        self.decay = f64::from(decay).powf(f64::from(ROOM_SIZE)) as Real;
        self.lat6_coeff = (self.decay + 0.15).clamp(0.25, 0.5);

        // Below the bypass threshold this warp runs off the end of its own range: at warped 0 it
        // yields wet −0.117 and dry 1.044. The original computes and transmits those values too
        // (`dfxpComm.cpp:628-632` has no floor) and they are harmless only because the effect is
        // switched off there. Anyone who "simplifies" `is_active` away gets phase-inverted wet and
        // 0.4 dB of dry boost for free.
        if warped > 40 {
            self.wet_gain = WET_MAX;
            self.dry_gain = DRY_MIN;
        } else {
            self.wet_gain =
                (f64::from(warped - 12) * (1.0 / WARP_SPAN) * f64::from(WET_MAX)) as Real;
            self.dry_gain =
                DRY_MIN + (f64::from(40 - warped) * (1.0 / WARP_SPAN)) as Real * DRY_SPAN;
        }
    }

    fn amount(&self) -> Real {
        self.amount
    }

    fn is_active(&self) -> bool {
        self.active
    }

    /// Clears the tank.
    ///
    /// This is a `memset` of the whole arena — bounded, allocation-free and lock-free, but ~663 kB,
    /// so it belongs at a stream restart rather than in a per-block path. The whole arena rather
    /// than just the part the current rate uses, so that dropping to a lower rate can never leave
    /// a previous format's audio sitting in memory a later, higher rate would read back.
    fn reset(&mut self) {
        self.arena.fill(0.0);
        for line in &mut self.lines {
            line.write = 0;
        }
        self.bandwidth_z = 0.0;
        self.damp1_z = 0.0;
        self.damp2_z = 0.0;
        self.d4_out = 0.0;
    }

    /// Interleaved, in place.
    ///
    /// Channels beyond the first two are left untouched: the original runs a *separate* reverb
    /// instance per output pair (`dfxpComm.cpp:88-111`) and forces the subwoofer's off entirely,
    /// so mixing surround channels into one tank would be a new effect, not this one.
    fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.active || channels == 0 {
            return;
        }

        if channels == 1 {
            // `Lex32.c:670-675`: the mono path zeroes the second input, halves both wet outputs
            // and sums them into the single output sample (`dutio.h:414-426`).
            for sample in buffer.iter_mut() {
                let dry = *sample + DENORM_BIAS;
                let (wet1, wet2) = self.tick(dry, dry);
                let out1 = wet1 * 0.5 * self.wet_gain + self.dry_gain * dry;
                let out2 = wet2 * 0.5 * self.wet_gain;
                *sample = out1 + out2;
            }
            return;
        }

        let (li, ri) = self.front_pair.unwrap_or((0, 1));
        for frame in buffer.chunks_exact_mut(channels) {
            let Some((left, right)) = super::pair_mut(frame, li, ri) else {
                continue;
            };
            let in1 = *left + DENORM_BIAS;
            let in2 = *right + DENORM_BIAS;
            let (wet1, wet2) = self.tick(in1, in2);
            // `kerWetDry` (`kerdelay.h:205-210`): the master gain is already folded into the pair.
            *left = wet1 * self.wet_gain + self.dry_gain * in1;
            *right = wet2 * self.wet_gain + self.dry_gain * in2;
        }
    }
}

/// Prints the design, never the 40 000-plus floats of delay memory behind it.
impl fmt::Debug for Ambience {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ambience")
            .field("amount", &self.amount)
            .field("sample_rate", &self.sample_rate)
            .field("active", &self.active)
            .field("decay", &self.decay)
            .field("lat6_coeff", &self.lat6_coeff)
            .field("wet_gain", &self.wet_gain)
            .field("dry_gain", &self.dry_gain)
            .field("arena_floats", &self.arena.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::scale::slider_to_value;

    /// Feed one stereo impulse and return `frames` frames of interleaved output.
    fn impulse_response(amount: Real, sample_rate: Real, frames: usize) -> Vec<Real> {
        let mut reverb = Ambience::new(sample_rate);
        reverb.set_amount(amount);
        let mut buffer = vec![0.0; frames * 2];
        buffer[0] = 1.0;
        buffer[1] = 1.0;
        reverb.process(&mut buffer, 2);
        buffer
    }

    /// Block RMS of the left channel, in blocks of `block` frames.
    fn envelope(ir: &[Real], block: usize) -> Vec<Real> {
        ir.chunks(block * 2)
            .map(|chunk| {
                let sum: f64 = chunk
                    .iter()
                    .step_by(2)
                    .map(|s| f64::from(*s) * f64::from(*s))
                    .sum();
                (sum / (chunk.len() as f64 / 2.0)).sqrt() as Real
            })
            .collect()
    }

    /// Time in seconds at which the wet tail has fallen 60 dB below its peak.
    ///
    /// Block 0 is skipped: it holds the dry impulse, which is not part of the tail.
    fn decay_time(ir: &[Real], sample_rate: Real, block: usize) -> Real {
        let env = envelope(ir, block);
        let tail = &env[1..];
        let peak = tail.iter().copied().fold(0.0, Real::max);
        assert!(peak > 0.0, "the reverb produced no tail at all");
        let threshold = peak * 1.0e-3;
        let last = tail
            .iter()
            .rposition(|rms| *rms > threshold)
            .expect("the peak block is always above the threshold");
        (last + 1) as Real * block as Real / sample_rate
    }

    #[test]
    fn the_delay_line_lengths_match_the_memory_table_in_the_spec() {
        // docs/spec/10-dsp-effects.md §7.5, which is `MasterLen` at `Lex32.c:174-239`.
        let at_44 = Layout::for_rate(44_100.0);
        assert_eq!(
            at_44.lengths,
            [
                4410, 210, 158, 561, 410, 1089, 6623, 2678, 5534, 1439, 6273, 3949, 4706
            ]
        );
        assert_eq!(at_44.total(), 38_040);

        let at_48 = Layout::for_rate(48_000.0);
        assert_eq!(
            at_48.lengths,
            [
                4800, 228, 172, 611, 446, 1186, 7209, 2915, 6023, 1566, 6828, 4298, 5122
            ]
        );
        assert_eq!(at_48.total(), 41_404);

        // The un-modulated decay diffusers read a truncated integer delay, not the padded length.
        assert_eq!(at_44.lat5_delay, 1000);
        assert_eq!(at_44.lat7_delay, 1350);
        assert_eq!(at_48.lat5_delay, 1089);
        assert_eq!(at_48.lat7_delay, 1469);
    }

    #[test]
    fn the_tap_offsets_match_the_constant_table() {
        // §7.2 scaled by roomsize · fs at 44.1 kHz, truncated.
        let layout = Layout::for_rate(44_100.0);
        assert_eq!(layout.d1_taps, [447, 2961, 5396]);
        assert_eq!(layout.lat6_taps, [278, 1826]);
        assert_eq!(layout.d2_taps, [1584, 3975]);
        assert_eq!(layout.d3_taps, [447, 3138, 4422]);
        assert_eq!(layout.lat8_taps, [498, 2846]);
        assert_eq!(layout.d4_taps, [179, 2970]);
        // Every tap has to stay inside its own line, or it would read a neighbour's samples.
        for (taps, line) in [
            (&layout.d1_taps[..], D1),
            (&layout.lat6_taps[..], LAT6),
            (&layout.d2_taps[..], D2),
            (&layout.d3_taps[..], D3),
            (&layout.lat8_taps[..], LAT8),
            (&layout.d4_taps[..], D4),
        ] {
            assert!(taps.iter().all(|t| *t >= 1 && *t <= layout.lengths[line]));
        }
    }

    #[test]
    fn the_arena_is_sized_for_the_highest_rate_and_never_grows() {
        let reverb = Ambience::new(44_100.0);
        let worst_case = Layout::for_rate(MAX_SAMPLE_RATE).total();
        assert_eq!(reverb.arena.len(), worst_case);
        // ~663 kB of f32. The spec's budget (§12) is "allocate ~160 KB for the reverb" at 44.1 kHz;
        // this is that figure scaled to 192 kHz, still one allocation.
        assert!(worst_case > Layout::for_rate(48_000.0).total());
        for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            assert!(Layout::for_rate(rate).total() <= worst_case, "{rate} Hz");
        }
    }

    #[test]
    fn the_one_pole_coefficients_match_the_values_the_original_hard_codes() {
        // §7.3, and the literals at `Play32.c:308-311`.
        for (rate, bandwidth, damping) in [
            (44_100.0, 0.350_110, 0.408_290),
            (48_000.0, 0.375_810, 0.435_258),
        ] {
            let reverb = Ambience::new(rate);
            assert!(
                (reverb.bandwidth - bandwidth).abs() < 1e-5,
                "{rate} Hz bandwidth: {}",
                reverb.bandwidth
            );
            assert!(
                (reverb.damping - damping).abs() < 1e-5,
                "{rate} Hz damping: {}",
                reverb.damping
            );
            assert!((reverb.one_minus_bandwidth - (1.0 - bandwidth)).abs() < 1e-5);
            assert!((reverb.one_minus_damping - (1.0 - damping)).abs() < 1e-5);
        }
    }

    #[test]
    fn the_room_size_is_the_one_the_quantiser_gives_at_midi_64() {
        assert!((ROOM_SIZE - 1.003_937).abs() < 1e-6, "{ROOM_SIZE}");
        // And the linear quantiser it comes from hard-sets its top end.
        assert_eq!(linear_qnt(0.5, 1.5, 127), 1.5);
        assert_eq!(linear_qnt(0.5, 1.5, 0), 0.5);
    }

    #[test]
    fn the_user_mapping_matches_the_reference_table() {
        // docs/spec/10-dsp-effects.md §7.4, MUSIC2 (the only shipping mode).
        let cases = [
            (4, 0.128_258, 0.278_258, 0.048_750, 0.981_607),
            (5, 0.137_945, 0.287_945, 0.087_750, 0.966_893),
            (6, 0.148_363, 0.298_363, 0.126_750, 0.952_179),
            (7, 0.162_499, 0.312_499, 0.175_500, 0.933_786),
            (8, 0.174_771, 0.324_771, 0.214_500, 0.919_071),
            (9, 0.187_971, 0.337_971, 0.253_500, 0.904_357),
            (10, 0.205_881, 0.355_881, 0.273_000, 0.897_000),
        ];
        let mut reverb = Ambience::new(48_000.0);
        for (slider, decay, lat6, wet, dry) in cases {
            reverb.set_amount(slider_to_value(slider as Real));
            assert!(reverb.is_active(), "slider {slider} should be active");
            assert!(
                (reverb.decay - decay).abs() < 1e-5,
                "slider {slider} decay: {}",
                reverb.decay
            );
            assert!(
                (reverb.lat6_coeff - lat6).abs() < 1e-5,
                "slider {slider} lat6: {}",
                reverb.lat6_coeff
            );
            assert!(
                (reverb.wet_gain - wet).abs() < 1e-6,
                "slider {slider} wet: {}",
                reverb.wet_gain
            );
            assert!(
                (reverb.dry_gain - dry).abs() < 1e-6,
                "slider {slider} dry: {}",
                reverb.dry_gain
            );
        }
    }

    #[test]
    fn the_wet_dry_pair_is_continuous_across_the_warp_boundary() {
        // The two branches of `dfxpComm.cpp:621-632` meet at warped 40: wet 0.273, dry 0.897.
        assert!((WET_MAX - (0.21 * 1.3)).abs() < 1e-6);
        assert!((DRY_MIN - (0.69 * 1.3)).abs() < 1e-6);
        let boundary_wet = (f64::from(40 - 12) * (1.0 / WARP_SPAN) * f64::from(WET_MAX)) as Real;
        assert!((boundary_wet - WET_MAX).abs() < 1e-6);
    }

    #[test]
    fn the_bottom_three_slider_positions_are_bypassed() {
        // §7.4: the MUSIC2 warp pushes sliders 1..3 to a warped value of 12 or less, and the
        // effect is switched off there. Shipped behaviour, and a known user complaint.
        let mut reverb = Ambience::new(48_000.0);
        for slider in 0..=3 {
            reverb.set_amount(slider_to_value(slider as Real));
            assert!(!reverb.is_active(), "slider {slider} should be bypassed");
        }
        for slider in 4..=10 {
            reverb.set_amount(slider_to_value(slider as Real));
            assert!(reverb.is_active(), "slider {slider} should be active");
        }
    }

    #[test]
    fn amount_zero_is_an_exact_bypass() {
        let mut reverb = Ambience::new(48_000.0);
        reverb.set_amount(0.0);
        let original: Vec<Real> = (0..2048).map(|n| (n as Real * 0.037).sin() * 0.7).collect();
        let mut buffer = original.clone();
        reverb.process(&mut buffer, 2);
        assert_eq!(
            buffer, original,
            "a bypassed reverb must not touch a sample"
        );
    }

    #[test]
    fn silence_in_gives_silence_out() {
        let mut reverb = Ambience::new(48_000.0);
        reverb.set_amount(1.0);
        let mut buffer = vec![0.0; 8192];
        reverb.process(&mut buffer, 2);
        // Only the 1e-36 denormal bias can leak through, scaled by the wet and dry gains.
        assert!(
            buffer.iter().all(|s| s.abs() < 1e-30),
            "silence was not preserved"
        );
        assert!(buffer.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn an_impulse_produces_a_tail_that_arrives_after_the_diffusers_and_then_decays() {
        let rate = 48_000.0;
        let ir = impulse_response(1.0, rate, rate as usize);
        assert!(ir.iter().all(|s| s.is_finite()));

        // The dry impulse comes straight out, scaled by the dry gain.
        assert!((ir[0] - DRY_MIN).abs() < 1e-3, "dry path: {}", ir[0]);

        let block = 512;
        let env = envelope(&ir, block);
        assert!(env[1] > 0.0, "nothing came out of the tank");
        // A plate *builds up* before it decays: the taps and the two cross-fed branches take time
        // to fill. A reverb that peaks in its first block has lost its diffusion.
        let peak_index = env[1..]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i + 1)
            .expect("the envelope is never empty");
        assert!(
            (2..=20).contains(&peak_index),
            "the tail peaked in block {peak_index} ({} ms)",
            peak_index as Real * block as Real / rate * 1000.0
        );
        // A monotone-in-the-large decay: every 100 ms window is quieter than the one before it.
        let windows: Vec<Real> = env
            .chunks(9)
            .skip(1)
            .map(|c| c.iter().copied().fold(0.0, Real::max))
            .collect();
        assert!(
            windows.windows(2).all(|w| w[1] < w[0]),
            "the tail did not decay monotonically: {windows:?}"
        );
    }

    #[test]
    fn the_decay_time_lengthens_as_the_amount_rises() {
        let rate = 48_000.0;
        let frames = rate as usize * 2;
        let block = 256;
        // The tank is deliberately short: `decay` only reaches 0.206 and the signal is multiplied
        // by it *twice* per circuit — once into the decay diffuser and once after the damping
        // filter (`Lex32.c:422, 497`) — so the loop gain is about 0.04 and this is an ambience
        // wash, not a hall. These times follow from that loop gain, so losing one of the two
        // multiplies, or mis-mapping `decay`, moves them straight out of the band.
        let expected = [(0.4, 0.795), (0.7, 0.848), (1.0, 0.928)];
        let times: Vec<Real> = expected
            .into_iter()
            .map(|(amount, _)| decay_time(&impulse_response(amount, rate, frames), rate, block))
            .collect();
        assert!(
            times.windows(2).all(|w| w[1] > w[0]),
            "decay times should grow with the amount: {times:?}"
        );
        for ((amount, want), got) in expected.into_iter().zip(&times) {
            assert!(
                (got - want).abs() < want * 0.08,
                "amount {amount}: 60 dB decay in {got}s, expected about {want}s"
            );
        }
    }

    #[test]
    fn the_tail_stays_bounded_and_finite_over_ten_seconds_at_full_amount() {
        let rate = 48_000.0;
        let mut reverb = Ambience::new(rate);
        reverb.set_amount(1.0);

        // One impulse, then ten seconds of silence, processed in realistic blocks.
        let block_frames = 1024;
        let mut peak_after_a_second = 0.0_f32;
        let mut buffer = vec![0.0; block_frames * 2];
        buffer[0] = 1.0;
        buffer[1] = 1.0;
        let blocks = (10.0 * rate) as usize / block_frames;
        for block in 0..blocks {
            reverb.process(&mut buffer, 2);
            assert!(
                buffer.iter().all(|s| s.is_finite()),
                "block {block} produced a non-finite sample"
            );
            assert!(
                buffer.iter().all(|s| s.abs() <= 1.5),
                "block {block} exceeded the input's magnitude"
            );
            if block as Real * block_frames as Real / rate > 1.0 {
                peak_after_a_second =
                    peak_after_a_second.max(buffer.iter().fold(0.0, |m, s| m.max(s.abs())));
            }
            buffer.fill(0.0);
        }
        // After a second the tank must be effectively empty — 60 dB below the impulse and more.
        assert!(
            peak_after_a_second < 1.0e-3,
            "the tank was still ringing at {peak_after_a_second}"
        );
    }

    #[test]
    fn a_sustained_full_scale_input_does_not_run_away() {
        let rate = 48_000.0;
        let mut reverb = Ambience::new(rate);
        reverb.set_amount(1.0);
        let mut buffer: Vec<Real> = (0..4096)
            .flat_map(|n| {
                let s = (n as Real * 440.0 * std::f32::consts::TAU / rate).sin();
                [s, s]
            })
            .collect();
        for _ in 0..40 {
            reverb.process(&mut buffer, 2);
            let peak = buffer.iter().fold(0.0, |m: Real, s| m.max(s.abs()));
            assert!(peak.is_finite() && peak < 2.0, "peak ran away to {peak}");
            for (n, sample) in buffer.iter_mut().enumerate() {
                *sample = ((n / 2) as Real * 440.0 * std::f32::consts::TAU / rate).sin();
            }
        }
    }

    #[test]
    fn changing_the_sample_rate_redesigns_and_clears_without_producing_nans() {
        let mut reverb = Ambience::new(44_100.0);
        reverb.set_amount(1.0);
        let mut buffer = vec![0.0; 4096];
        buffer[0] = 1.0;
        buffer[1] = 1.0;
        reverb.process(&mut buffer, 2);

        for rate in [48_000.0, 96_000.0, 192_000.0, 16_000.0, 44_100.0] {
            reverb.set_sample_rate(rate);
            assert_eq!(reverb.sample_rate, rate);
            // The tank must be empty after a format change, not holding the old rate's tail.
            let dirty = reverb.arena.iter().filter(|s| **s != 0.0).count();
            assert!(
                dirty == 0,
                "rate {rate}: {dirty} non-zero samples left in the arena"
            );
            let mut buffer = vec![0.0; 8192];
            buffer[0] = 0.9;
            buffer[1] = -0.9;
            reverb.process(&mut buffer, 2);
            assert!(
                buffer.iter().all(|s| s.is_finite()),
                "{rate} Hz produced a non-finite sample"
            );
            assert!(buffer.iter().all(|s| s.abs() < 1.5), "{rate} Hz clipped");
        }
    }

    #[test]
    fn an_absurd_sample_rate_is_clamped_rather_than_trusted() {
        let mut reverb = Ambience::new(48_000.0);
        reverb.set_sample_rate(Real::NAN);
        assert_eq!(reverb.sample_rate, MIN_SAMPLE_RATE);
        reverb.set_sample_rate(10_000_000.0);
        assert_eq!(reverb.sample_rate, MAX_SAMPLE_RATE);
        reverb.set_sample_rate(-1.0);
        assert_eq!(reverb.sample_rate, MIN_SAMPLE_RATE);
        // And the layout still fits the arena it was built with.
        assert!(Layout::for_rate(reverb.sample_rate).total() <= reverb.arena.len());
    }

    #[test]
    fn the_mono_path_sums_the_stereo_pair() {
        // `Lex32.c:670-675` halves both wet outputs and the host sums them, so a mono stream
        // should land within rounding of (L + R) of the stereo one fed the same signal.
        let rate = 48_000.0;
        let frames = 4096;
        let mut mono_reverb = Ambience::new(rate);
        mono_reverb.set_amount(1.0);
        let mut mono = vec![0.0; frames];
        mono[0] = 1.0;
        mono_reverb.process(&mut mono, 1);

        let stereo = impulse_response(1.0, rate, frames);
        let halved: Vec<Real> = stereo
            .as_chunks::<2>()
            .0
            .iter()
            .map(|f| (f[0] + f[1]) * 0.5)
            .collect();

        // Both paths drive the tank with in1 + in2 = 2.0, so the wet pair is identical; the mono
        // path then halves it and keeps one dry term instead of two. The result is exactly the
        // stereo pair folded down to mono.
        assert_eq!(mono.len(), halved.len());
        for (n, (m, s)) in mono.iter().zip(&halved).enumerate() {
            assert!(
                (m - s).abs() < 1e-6,
                "frame {n}: mono {m} against folded stereo {s}"
            );
        }
        assert!(
            mono.iter().any(|s| s.abs() > 1e-6),
            "the mono path was silent"
        );
    }

    #[test]
    fn extra_channels_are_left_alone() {
        let mut reverb = Ambience::new(48_000.0);
        reverb.set_amount(1.0);
        let mut buffer: Vec<Real> = (0..1024)
            .flat_map(|n| [0.5, -0.5, 0.25, n as Real])
            .collect();
        let before: Vec<Real> = buffer.iter().skip(2).step_by(4).copied().collect();
        reverb.process(&mut buffer, 4);
        let after: Vec<Real> = buffer.iter().skip(2).step_by(4).copied().collect();
        assert_eq!(before, after, "the third channel was modified");
    }

    #[test]
    fn reset_clears_the_tail_without_changing_the_design() {
        let mut reverb = Ambience::new(48_000.0);
        reverb.set_amount(1.0);
        let decay = reverb.decay;
        let mut buffer = vec![0.0; 4096];
        buffer[0] = 1.0;
        reverb.process(&mut buffer, 2);
        assert!(reverb.arena.iter().any(|s| *s != 0.0));

        reverb.reset();
        assert!(reverb.arena.iter().all(|s| *s == 0.0));
        assert_eq!(reverb.decay, decay);
        assert_eq!(reverb.d4_out, 0.0);

        let mut buffer = vec![0.0; 4096];
        reverb.process(&mut buffer, 2);
        assert!(
            buffer.iter().all(|s| s.abs() < 1e-30),
            "a tail survived reset"
        );
    }

    #[test]
    fn the_exponential_quantiser_hard_sets_both_endpoints() {
        // `Qntitor.cpp:311-312`: accumulated products never land exactly on the limits, so the
        // original overwrites them.
        assert_eq!(exp_qnt(0.095, 0.95, 0), 0.095);
        assert_eq!(exp_qnt(0.095, 0.95, 127), 0.95);
        // And the ladder is monotonic in between.
        let mut previous = exp_qnt(0.095, 0.95, 0);
        for index in 1..=127 {
            let value = exp_qnt(0.095, 0.95, index);
            assert!(value > previous, "index {index}: {value} <= {previous}");
            previous = value;
        }
    }

    #[test]
    fn the_quantiser_frequencies_are_the_ones_the_spec_tabulates() {
        // §7.3: MIDI 89 -> 8.161 kHz for the input low-pass, MIDI 81 -> 6.758 kHz for damping.
        let rolloff = exp_qnt(FREQ_MIN_KHZ, FREQ_MAX_KHZ, ROLLOFF_MIDI) * 1000.0;
        let damping = exp_qnt(FREQ_MIN_KHZ, FREQ_MAX_KHZ, DAMPING_MIDI) * 1000.0;
        assert!((rolloff - 8161.0).abs() < 1.0, "{rolloff}");
        assert!((damping - 6757.5).abs() < 1.0, "{damping}");
    }

    #[test]
    fn the_two_output_taps_decorrelate_the_tail() {
        // The output matrix (`Lex32.c:490-664`) is deliberately asymmetric — different taps, and
        // different signs — so a mono input still comes out as a stereo field. Identical channels
        // would mean the matrix was copy-pasted.
        let ir = impulse_response(1.0, 48_000.0, 24_000);
        let (mut same, mut differ) = (0usize, 0usize);
        for frame in ir.as_chunks::<2>().0 {
            if frame[0].abs() < 1e-9 && frame[1].abs() < 1e-9 {
                continue;
            }
            if (frame[0] - frame[1]).abs() < 1e-9 {
                same += 1;
            } else {
                differ += 1;
            }
        }
        assert!(
            differ > same * 10,
            "the two channels tracked each other: {same} equal against {differ} different"
        );
    }

    #[test]
    fn the_first_wet_output_arrives_at_the_first_output_tap() {
        // The structural check: the earliest path to an output is pre-delay (1 sample) plus the
        // zero-delay branch of all five lattices, then D1's first tap at 10.1 ms
        // (`c_lex.h:77`, `Lex32.c:491`). Anything else means a line was mis-ordered.
        let rate = 48_000.0;
        let ir = impulse_response(1.0, rate, 4096);
        let first = ir
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, frame)| frame[0].abs() > 1e-6)
            .map(|(n, _)| n)
            .expect("no wet output at all");
        let expected = (0.010_1 * f64::from(ROOM_SIZE) * f64::from(rate)) as usize + PRE_DELAY;
        assert!(
            first.abs_diff(expected) <= 1,
            "first wet sample at {first}, expected about {expected}"
        );
    }

    #[test]
    fn the_first_reflection_has_the_gain_the_whole_input_chain_implies() {
        // The one measurement that pins the entire pre-tank path in a single number. The earliest
        // way in to an output tap is: pre-delay (1 sample) -> the input low-pass -> the *zero
        // delay* branch of the four input diffusers (a lattice passes `k·x` through instantly)
        // -> the zero-delay branch of AP5/AP7, which is `-k` because their signs are flipped
        // -> D1/D3 tap 1 at 10.1 ms -> output scale -> wet gain.
        let rate = 48_000.0;
        let layout = Layout::for_rate(rate);
        assert_eq!(
            layout.d1_taps[0], layout.d3_taps[0],
            "both branches tap 10.1 ms"
        );
        let arrival = PRE_DELAY + layout.d1_taps[0];

        let bandwidth = one_pole_coeff(
            exp_qnt(FREQ_MIN_KHZ, FREQ_MAX_KHZ, ROLLOFF_MIDI) * 1000.0,
            rate,
        );
        // Both channels of the impulse are summed into the tank, hence the 2.0.
        let expected = 2.0
            * (1.0 - bandwidth)
            * (LAT1_COEFF * LAT1_COEFF * LAT3_COEFF * LAT3_COEFF)
            * (-LAT5_COEFF)
            * OUTPUT_SCALE
            * WET_MAX;

        let ir = impulse_response(1.0, rate, 4096);
        let frames = ir.as_chunks::<2>().0;
        assert!(
            (frames[arrival][0] - expected).abs() < 1e-6,
            "first reflection {} against the chain gain {expected}",
            frames[arrival][0]
        );
        assert!(
            (frames[arrival][0] - -0.015_725_7).abs() < 1e-6,
            "first reflection drifted: {}",
            frames[arrival][0]
        );
        // Both branches reach their first tap through identical gains, so it lands centred.
        assert_eq!(frames[arrival][0], frames[arrival][1]);
    }

    #[test]
    fn the_input_diffusers_echo_at_their_own_lengths() {
        // A lattice all-pass with coefficient k answers an impulse with k, then (1−k²) one delay
        // later, then −k(1−k²), … Measured against the first reflection, the echo at AP1's and
        // AP2's length must therefore sit at (1−k²)/k, and the second-order one at −(1−k²).
        // Wrong diffuser lengths, a wrong coefficient or a mis-wired cascade all move these.
        let rate = 48_000.0;
        let layout = Layout::for_rate(rate);
        let arrival = PRE_DELAY + layout.d1_taps[0];
        let ir = impulse_response(1.0, rate, 4096);
        let frames = ir.as_chunks::<2>().0;
        let first = frames[arrival][0];

        let k = LAT1_COEFF;
        let echo = (1.0 - k * k) / k;
        for length in [layout.lengths[LAT1], layout.lengths[LAT2]] {
            let ratio = frames[arrival + length][0] / first;
            assert!(
                (ratio - echo).abs() < 1e-4,
                "echo at {length} samples: ratio {ratio}, expected {echo}"
            );
        }
        let second_order = frames[arrival + 2 * layout.lengths[LAT2]][0] / first;
        assert!(
            (second_order + (1.0 - k * k)).abs() < 1e-4,
            "the second-order echo did not invert: {second_order}"
        );
    }

    #[test]
    fn debug_does_not_dump_the_delay_memory() {
        let reverb = Ambience::new(48_000.0);
        let text = format!("{reverb:?}");
        assert!(text.len() < 400, "Debug printed {} bytes", text.len());
        assert!(text.contains("arena_floats"));
    }
}
