//! A light de-reverberation stage: late-reverb suppression in the STFT domain.
//!
//! What a room adds to a voice arrives in two parts. The early reflections, inside the first few
//! tens of milliseconds, are what makes a room sound like a room and are left alone. The late
//! tail — the diffuse decay that follows every syllable — is what makes a voice sound far away,
//! and it is what this stage takes down.
//!
//! The estimator is Lebart's: the late tail at a frame is the reverberant signal's own power
//! spectrum `D` earlier, decayed by the room's time constant,
//!
//! ```text
//! R(k, ℓ) = e^(−2·Δ·ln(1000)/T60) · |Y(k, ℓ−D)|²,   Δ = D in seconds, D = 50 ms
//! ```
//!
//! and the gain is a spectral subtraction against it, `G = max(1 − R/|Y|², floor)`, smoothed over
//! time so that a frame's decision is not a click. `T60` and the floor are the level: a room is
//! not measured, it is assumed at 0.3, 0.5 or 0.8 s, with a floor of −10, −14 or −18 dB. That is
//! what makes the stage *light*: it takes the tail down by up to the floor and no further, and on
//! a dry voice — where the estimator's assumption is wrong and it sees a syllable's own sustain as
//! a tail — it costs at most a decibel or so, which the test over a dry fixture holds it to.
//!
//! **Where it sits, and why.** After the denoiser: RNNoise's mask removes the stationary floor,
//! which this estimator would otherwise read as a tail that never ends and take down toward the
//! floor for the whole session. Before the high-pass and the gate: the gate measures a level, and
//! the tail this removes is exactly what holds a gate open through a pause.
//!
//! **Latency is the frame, 480, not the hop.** The transform's own delay is `N − H = 240`, and
//! that is what a hop-aligned scheme could report: fed a whole hop at a time, it hands back the
//! previous hop in the same call. The blocks here are not hop-aligned and never will be — the
//! audio crate negotiates a power-of-two quantum, 256 to 2048, and none of those is a multiple
//! of 240 — so the bridge that turns arbitrary blocks into hops has to add a hop of its own.
//! That is a bound, not a choice: the first sample of an output hop is complete only once the
//! whole *next* input hop has arrived, so a constant lag under sample-granular blocks cannot be
//! less than `2H − 1 = 479`, and the bridge lands on the hop boundary rather than buy one sample
//! with a second set of counters. What the stage reports is what the signal actually lags by, at
//! every block size. This is the lesson the denoiser taught in 0.3.0, where the library's own
//! frame went unreported and a recording application drifted ten milliseconds out of lip sync;
//! a cross-correlation test pins the figure here, and another holds it at each quantum the
//! stream can deliver.
//!
//! Real-time safe: the FFT is planned and every buffer is sized at construction, and nothing
//! allocates afterwards — held by a counting allocator in the crate's integration tests.

use crate::biquad::{MAX_CHANNELS, Real};
use crate::input::processor::{AudioProcessor, ProcessContext, StageMeter};
use crate::input::sane_rate;
use fxsound_core::DereverbLevel;
use fxsound_core::messages::InputDspParams;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Analysis frame: 10 ms at 48 kHz.
pub const FRAME: usize = 480;
/// Hop: half the frame.
pub const HOP: usize = FRAME / 2;
/// Frames of delay the stage adds while it runs: the transform's `FRAME − HOP` and the hop
/// bridge's `HOP`. Not the hop alone — the module doc says why a bridge over a power-of-two
/// quantum cannot be hop-aligned, and the design's §7 carries this figure.
pub const LATENCY: usize = FRAME;
const BINS: usize = FRAME / 2 + 1;
/// The look-back `D`, in hops: 50 ms at 48 kHz.
const DELAY_HOPS: usize = 10;
/// The history ring holds the look-back plus the current hop.
const HISTORY: usize = DELAY_HOPS + 1;

/// The power spectrum is smoothed over about eight hops — 40 ms — before it is used or
/// remembered. A tail is noise, and a single hop's power in one bin is an exponentially
/// distributed sample of it: against an unsmoothed spectrum the ratio `R/|Y|²` swings from
/// nothing to everything bin by bin, the clamp keeps the swings that say "leave it" and cuts
/// the ones that say "remove it", and the average gain lands well above the floor. Measured on
/// the reverberant fixture: at two hops of smoothing Medium took 4.4 dB off the tail; at eight
/// it takes 11.8, against a floor of 14. The dry voice pays a third of a decibel more for it.
const PSD_COEFF: Real = 0.12;
/// The gain moves down (toward the floor) with this coefficient per hop — about 20 ms — and up
/// with [`GAIN_RELEASE`], about 12 ms: deliberate about deciding something is a tail, quick to
/// let a new syllable through.
const GAIN_ATTACK: Real = 0.25;
const GAIN_RELEASE: Real = 0.4;
/// The reduction meter's time constant, in hops: 100 ms.
const METER_HOPS: Real = 20.0;
/// Keeps the ratio `R/|Y|²` defined in silence.
const EPSILON: Real = 1.0e-12;

/// One channel's transform state.
struct Channel {
    /// The last `FRAME` input samples, in the `[-1, 1]` range.
    window: [Real; FRAME],
    /// The overlap-add buffer: `FRAME` samples, of which the first `HOP` are complete after a hop.
    overlap: [Real; FRAME],
    /// The `HOP` output samples ready for the bridge.
    ready: [Real; HOP],
    /// The dry hop that lines up with `ready`, for the meter.
    dry: [Real; HOP],
    psd: [Real; BINS],
    history: [[Real; BINS]; HISTORY],
    gain: [Real; BINS],
}

impl Channel {
    fn new() -> Self {
        Self {
            window: [0.0; FRAME],
            overlap: [0.0; FRAME],
            ready: [0.0; HOP],
            dry: [0.0; HOP],
            psd: [0.0; BINS],
            history: [[0.0; BINS]; HISTORY],
            gain: [1.0; BINS],
        }
    }

    fn reset(&mut self) {
        self.window.fill(0.0);
        self.overlap.fill(0.0);
        self.ready.fill(0.0);
        self.dry.fill(0.0);
        self.psd.fill(0.0);
        for row in &mut self.history {
            row.fill(0.0);
        }
        self.gain.fill(1.0);
    }
}

pub struct Dereverb {
    forward: Arc<dyn RealToComplex<Real>>,
    inverse: Arc<dyn ComplexToReal<Real>>,
    /// Owned so the FFT never allocates.
    indata: Vec<Real>,
    spectrum: Vec<Complex<Real>>,
    scratch_forward: Vec<Complex<Real>>,
    scratch_inverse: Vec<Complex<Real>>,
    /// Square-root Hann, applied at analysis and again at synthesis: the product is Hann, which
    /// sums to one at half overlap, so with unity gains the round trip is an identity.
    window: [Real; FRAME],
    channels: Vec<Channel>,
    /// Which slot of the history ring the current hop writes.
    head: usize,
    /// Shared across channels, because every channel receives exactly the same number of samples.
    fill: usize,
    read: usize,
    primed: bool,

    sample_rate: Real,
    level: DereverbLevel,
    /// `e^(−2·Δ·ln(1000)/T60)`, the decay over the look-back at the current rate.
    decay: Real,
    floor: Real,
    reduction_db: Real,
    psd_coeff: Real,
    gain_attack: Real,
    gain_release: Real,
}

impl std::fmt::Debug for Dereverb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dereverb")
            .field("sample_rate", &self.sample_rate)
            .field("level", &self.level)
            .field("decay", &self.decay)
            .field("floor", &self.floor)
            .field("reduction_db", &self.reduction_db)
            .finish()
    }
}

impl Dereverb {
    /// Plans the transform and allocates every buffer. Do this on the main loop.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let mut planner = RealFftPlanner::<Real>::new();
        let forward = planner.plan_fft_forward(FRAME);
        let inverse = planner.plan_fft_inverse(FRAME);
        let indata = forward.make_input_vec();
        let spectrum = forward.make_output_vec();
        let scratch_forward = forward.make_scratch_vec();
        let scratch_inverse = inverse.make_scratch_vec();

        let mut window = [0.0; FRAME];
        for (i, w) in window.iter_mut().enumerate() {
            let phase = std::f64::consts::TAU * i as f64 / FRAME as f64;
            *w = (0.5 - 0.5 * phase.cos()).sqrt() as Real;
        }

        let mut stage = Self {
            forward,
            inverse,
            indata,
            spectrum,
            scratch_forward,
            scratch_inverse,
            window,
            channels: (0..MAX_CHANNELS).map(|_| Channel::new()).collect(),
            head: 0,
            fill: 0,
            read: HOP,
            primed: false,
            sample_rate: sane_rate(sample_rate),
            level: DereverbLevel::Off,
            decay: 0.0,
            floor: 1.0,
            reduction_db: 0.0,
            psd_coeff: PSD_COEFF,
            gain_attack: GAIN_ATTACK,
            gain_release: GAIN_RELEASE,
        };
        stage.design();
        stage
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sane_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.design();
        self.reset();
    }

    /// The assumed room. A change of level while running is a change of two numbers, not a
    /// restart.
    pub fn set_level(&mut self, level: DereverbLevel) {
        if level == self.level {
            return;
        }
        self.level = level;
        self.design();
    }

    #[must_use]
    pub const fn level(&self) -> DereverbLevel {
        self.level
    }

    /// The assumed `T60` in seconds and the gain floor in dB for a level. Design choices, not
    /// measurements: a light touch for a room that is only a little live, up to a floor that
    /// still leaves the voice recognisably in a room.
    #[must_use]
    pub const fn room(level: DereverbLevel) -> (Real, Real) {
        match level {
            DereverbLevel::Off => (0.0, 0.0),
            DereverbLevel::Light => (0.3, -10.0),
            DereverbLevel::Medium => (0.5, -14.0),
            DereverbLevel::Strong => (0.8, -18.0),
        }
    }

    fn design(&mut self) {
        let (t60, floor_db) = Self::room(self.level);
        if t60 <= 0.0 {
            self.decay = 0.0;
            self.floor = 1.0;
            return;
        }
        let delta = (DELAY_HOPS * HOP) as Real / self.sample_rate;
        self.decay = (-2.0 * delta * 1000.0_f32.ln() / t60).exp();
        self.floor = 10.0_f32.powf(floor_db / 20.0);
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.level != DereverbLevel::Off
    }

    #[must_use]
    pub fn latency_frames(&self) -> usize {
        if self.is_active() { LATENCY } else { 0 }
    }

    /// What a meter shows: decibels the stage is taking away, smoothed. Zero when inactive.
    #[must_use]
    pub const fn reduction_db(&self) -> Real {
        self.reduction_db
    }

    /// Forget everything, in place.
    pub fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.reset();
        }
        self.head = 0;
        self.fill = 0;
        self.read = HOP;
        self.reduction_db = 0.0;
        self.primed = self.is_active();
    }

    /// A whole interleaved block, in place. Channels past [`MAX_CHANNELS`] make the stage stand
    /// aside completely, for the reason the denoiser gives: its difference is time, and delaying
    /// eight channels but not the ninth would tear the frame apart.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.is_active() || channels == 0 || channels > MAX_CHANNELS || buffer.is_empty() {
            self.primed = false;
            return;
        }
        if !self.primed {
            self.reset();
        }

        for frame in buffer.chunks_exact_mut(channels) {
            let reading = self.read < HOP;
            for (channel, sample) in frame.iter_mut().enumerate() {
                let Some(state) = self.channels.get_mut(channel) else {
                    continue;
                };
                let incoming = if sample.is_finite() { *sample } else { 0.0 };
                *sample = if reading { state.ready[self.read] } else { 0.0 };
                // The window slides by a hop: the new hop lands in the second half.
                state.window[HOP + self.fill] = incoming;
            }
            if reading {
                self.read += 1;
            }
            self.fill += 1;
            if self.fill == HOP {
                self.hop(channels);
                self.fill = 0;
                self.read = 0;
            }
        }
    }

    /// One hop on every channel: transform the window, estimate the tail, apply the gain,
    /// overlap-add, and shift.
    fn hop(&mut self, channels: usize) {
        let previous = (self.head + HISTORY - DELAY_HOPS) % HISTORY;
        let (mut in_energy, mut out_energy) = (0.0_f64, 0.0_f64);

        for channel in 0..channels {
            let Some(state) = self.channels.get_mut(channel) else {
                continue;
            };

            for ((dst, &x), &w) in self.indata.iter_mut().zip(&state.window).zip(&self.window) {
                *dst = x * w;
            }
            if self
                .forward
                .process_with_scratch(
                    &mut self.indata,
                    &mut self.spectrum,
                    &mut self.scratch_forward,
                )
                .is_err()
            {
                continue;
            }

            for (k, bin) in self.spectrum.iter_mut().enumerate() {
                let power = bin.norm_sqr();
                let psd = &mut state.psd[k];
                *psd += self.psd_coeff * (power - *psd);
                if !psd.is_finite() {
                    *psd = 0.0;
                }
                let tail = self.decay * state.history[previous][k];
                state.history[self.head][k] = *psd;

                let wanted = (1.0 - tail / (*psd + EPSILON)).clamp(self.floor, 1.0);
                let gain = &mut state.gain[k];
                let coeff = if wanted < *gain {
                    self.gain_attack
                } else {
                    self.gain_release
                };
                *gain += coeff * (wanted - *gain);
                if !gain.is_finite() {
                    *gain = 1.0;
                }
                *bin *= *gain;
            }

            if self
                .inverse
                .process_with_scratch(
                    &mut self.spectrum,
                    &mut self.indata,
                    &mut self.scratch_inverse,
                )
                .is_err()
            {
                continue;
            }

            // realfft's inverse is unnormalised: divide by the frame. Window again, overlap-add,
            // and the first hop of the sum is complete.
            let norm = 1.0 / FRAME as Real;
            for ((acc, &y), &w) in state.overlap.iter_mut().zip(&self.indata).zip(&self.window) {
                *acc += y * norm * w;
            }
            let (done, rest) = state.overlap.split_at(HOP);
            state.ready.copy_from_slice(done);
            // The dry samples that line up with `ready` are the first half of the window that
            // was just transformed.
            state.dry.copy_from_slice(&state.window[..HOP]);
            let mut shifted = [0.0; FRAME];
            shifted[..HOP].copy_from_slice(rest);
            state.overlap = shifted;
            state.window.copy_within(HOP.., 0);

            for sample in state.ready.iter_mut() {
                if !sample.is_finite() {
                    *sample = 0.0;
                }
            }
            if channel == 0 {
                for (&wet, &dry) in state.ready.iter().zip(&state.dry) {
                    in_energy += f64::from(dry * dry);
                    out_energy += f64::from(wet * wet);
                }
            }
        }

        self.head = (self.head + 1) % HISTORY;
        // Ten times the log of an energy ratio is twenty times the log of the RMS ratio — the
        // figure the strip shows. The first draft halved it, and every test on the meter was a
        // lower bound; one now holds it to the ratio measured from outside.
        let target = if in_energy > 1.0e-10 && out_energy > 0.0 {
            (10.0 * (in_energy / out_energy).log10()).max(0.0) as Real
        } else {
            0.0
        };
        self.reduction_db += (target - self.reduction_db) / METER_HOPS;
        if !self.reduction_db.is_finite() {
            self.reduction_db = 0.0;
        }
    }
}

impl AudioProcessor for Dereverb {
    fn prepare(&mut self, sample_rate: Real) {
        self.set_sample_rate(sample_rate);
    }

    fn apply(&mut self, params: &InputDspParams) {
        self.set_level(params.dereverb);
    }

    fn reset(&mut self) {
        Dereverb::reset(self);
    }

    fn is_active(&self) -> bool {
        Dereverb::is_active(self)
    }

    fn latency_frames(&self) -> usize {
        Dereverb::latency_frames(self)
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        Dereverb::process(self, buffer, ctx.channels);
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: self.reduction_db,
            running: self.is_active(),
            aux: 0.0,
        }
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;
    pub use crate::input::detector::db_to_linear;

    pub const FS: Real = 48_000.0;

    pub fn noise(frames: usize, amplitude: Real) -> Vec<Real> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..frames)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as Real / 8_388_608.0 - 1.0) * amplitude
            })
            .collect()
    }

    /// A stand-in for a voice: harmonics under a syllabic envelope.
    pub fn speech_like(frames: usize, amplitude: Real) -> Vec<Real> {
        (0..frames)
            .map(|n| {
                let t = n as Real / FS;
                let f = 130.0;
                let body = (t * std::f32::consts::TAU * f).sin() * 0.6
                    + (t * std::f32::consts::TAU * f * 2.0).sin() * 0.3
                    + (t * std::f32::consts::TAU * f * 5.0).sin() * 0.2
                    + (t * std::f32::consts::TAU * f * 11.0).sin() * 0.1;
                let syllable = (t * 4.0).fract();
                let envelope = if syllable < 0.55 {
                    (syllable / 0.55 * std::f32::consts::PI).sin().powf(0.6)
                } else {
                    0.02
                };
                body * envelope * amplitude / 1.2
            })
            .collect()
    }

    pub fn rms(samples: &[Real]) -> Real {
        (samples.iter().map(|x| x * x).sum::<Real>() / samples.len().max(1) as Real).sqrt()
    }

    /// `dry` convolved with an exponentially decaying noise tail of the given `T60`, by
    /// overlap-add FFT convolution: the fixture the estimator is built for.
    pub fn reverberate(dry: &[Real], t60: Real) -> Vec<Real> {
        let taps = (t60 * FS) as usize;
        let tail: Vec<Real> = noise(taps, 1.0)
            .iter()
            .enumerate()
            .map(|(n, x)| x * (-(n as Real) * 1000.0_f32.ln() / (t60 * FS)).exp() * 0.05)
            .collect();
        let mut impulse = vec![0.0; taps];
        impulse[0] = 1.0;
        for (i, t) in tail.iter().enumerate().skip(480) {
            impulse[i] += t;
        }

        let block = 16_384;
        let size = (block + taps).next_power_of_two();
        let mut planner = RealFftPlanner::<Real>::new();
        let forward = planner.plan_fft_forward(size);
        let inverse = planner.plan_fft_inverse(size);
        let mut h_in = vec![0.0; size];
        h_in[..taps].copy_from_slice(&impulse);
        let mut h = forward.make_output_vec();
        forward.process(&mut h_in, &mut h).expect("h");

        let mut out = vec![0.0; dry.len() + taps];
        for (b, chunk) in dry.chunks(block).enumerate() {
            let mut x = vec![0.0; size];
            x[..chunk.len()].copy_from_slice(chunk);
            let mut spec = forward.make_output_vec();
            forward.process(&mut x, &mut spec).expect("x");
            for (s, hh) in spec.iter_mut().zip(&h) {
                *s *= hh;
            }
            let mut y = vec![0.0; size];
            inverse.process(&mut spec, &mut y).expect("y");
            let start = b * block;
            for (i, v) in y.iter().enumerate() {
                if start + i < out.len() {
                    out[start + i] += v / size as Real;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;

    fn stage(level: DereverbLevel) -> Dereverb {
        let mut d = Dereverb::new(FS);
        d.set_level(level);
        d
    }

    /// The lag at which the output best matches the input, by cross-correlation over the
    /// middle of the signal and every lag up to two frames — no prior on where it lands.
    fn measured_lag(output: &[Real], input: &[Real]) -> usize {
        let (mut best_lag, mut best) = (0, Real::MIN);
        let (start, window) = (FRAME * 12, FRAME * 16);
        for lag in 0..=FRAME * 2 {
            let score: Real = (start..start + window)
                .map(|n| output[n] * input[n - lag])
                .sum();
            if score > best {
                best = score;
                best_lag = lag;
            }
        }
        best_lag
    }

    #[test]
    fn switched_off_it_is_not_in_the_signal_path_at_all() {
        let mut d = Dereverb::new(FS);
        assert!(!d.is_active());
        assert_eq!(d.latency_frames(), 0);
        let input = noise(4_800, 0.1);
        let mut block = input.clone();
        d.process(&mut block, 1);
        assert_eq!(block, input);
    }

    #[test]
    fn a_dry_voice_passes_within_a_decibel_and_a_half() {
        // The estimator's assumption is wrong on a dry signal — a syllable's own sustain looks
        // like a tail fifty milliseconds later — and the price of that has to be small, or the
        // stage is a compressor that thinks it is a de-reverb.
        let dry = speech_like(FS as usize * 2, db_to_linear(-12.0));
        for level in [DereverbLevel::Light, DereverbLevel::Medium] {
            let mut d = stage(level);
            let mut block = dry.clone();
            d.process(&mut block, 1);
            let before = rms(&dry[FS as usize / 2..dry.len() - LATENCY]);
            let after = rms(&block[FS as usize / 2 + LATENCY..]);
            let change = 20.0 * (after / before).log10();
            assert!(
                change.abs() < 1.5,
                "{level:?} moved a dry voice by {change:.2} dB"
            );
        }
    }

    #[test]
    fn the_tail_after_a_voice_stops_is_taken_down() {
        // A dry burst through a 0.6 s tail. Two hundred to five hundred milliseconds after the
        // burst ends there is nothing but tail, and Medium — which assumes half a second — has
        // to take more than six decibels of it away.
        let burst = FS as usize;
        let mut dry = speech_like(burst, db_to_linear(-12.0));
        dry.extend(std::iter::repeat_n(0.0, FS as usize));
        let wet = reverberate(&dry, 0.6);

        let from = burst + (0.2 * FS) as usize;
        let to = burst + (0.5 * FS) as usize;
        let mut d = stage(DereverbLevel::Medium);
        let mut block = wet.clone();
        // Up to the end of the window, so the meter is read while the tail is still there
        // rather than after a second of silence has let it fall back to nothing.
        d.process(&mut block[..to + LATENCY], 1);
        assert!(
            d.reduction_db() > 3.0,
            "the meter did not see it: {}",
            d.reduction_db()
        );
        d.process(&mut block[to + LATENCY..], 1);

        let before = rms(&wet[from..to]);
        let after = rms(&block[from + LATENCY..to + LATENCY]);
        assert!(before > 1.0e-4, "premise: the tail is audible, {before}");
        let reduction = 20.0 * (before / after.max(1.0e-9)).log10();
        assert!(
            reduction > 6.0,
            "the tail was only taken down by {reduction:.2} dB"
        );
    }

    #[test]
    fn stronger_levels_take_more_of_the_tail() {
        let burst = FS as usize;
        let mut dry = speech_like(burst, db_to_linear(-12.0));
        dry.extend(std::iter::repeat_n(0.0, FS as usize));
        let wet = reverberate(&dry, 0.6);
        let from = burst + (0.2 * FS) as usize;
        let to = burst + (0.5 * FS) as usize;
        let mut reductions = Vec::new();
        for level in [
            DereverbLevel::Light,
            DereverbLevel::Medium,
            DereverbLevel::Strong,
        ] {
            let mut d = stage(level);
            let mut block = wet.clone();
            d.process(&mut block, 1);
            let before = rms(&wet[from..to]);
            let after = rms(&block[from + LATENCY..to + LATENCY]);
            reductions.push(20.0 * (before / after.max(1.0e-9)).log10());
        }
        assert!(
            reductions[0] < reductions[1] && reductions[1] < reductions[2],
            "not monotone: {reductions:?}"
        );
    }

    #[test]
    fn the_meter_reads_the_decibels_actually_taken_away() {
        // As the denoiser's test of the same name says: the meter is `20·log10(rms_in /
        // rms_out)`, its first draft read half of that, and every other assertion on it is a
        // lower bound. On stationary noise the estimator's tail is the noise's own spectrum
        // fifty milliseconds ago, the gain settles, and the meter has to agree with the ratio
        // measured from outside — the input aligned by the stage's own latency.
        let mut d = stage(DereverbLevel::Strong);
        let input = noise(FRAME * 200, 0.1);
        let mut block = input.clone();
        d.process(&mut block, 1);
        let (from, to) = (FRAME * 100, FRAME * 200);
        let before = rms(&input[from - LATENCY..to - LATENCY]);
        let after = rms(&block[from..to]);
        let outside = 20.0 * (before / after.max(1.0e-9)).log10();
        assert!(
            outside > 1.0,
            "premise: a tail estimate on noise takes something away, not {outside} dB"
        );
        assert!(
            (d.reduction_db() - outside).abs() < 1.0,
            "the meter reads {} dB against {outside} dB measured from outside",
            d.reduction_db()
        );
    }

    #[test]
    fn the_delay_is_the_frame_and_it_is_reported() {
        // Cross-correlation against white noise, which a de-reverb at its lightest barely
        // touches. 240 is the transform's own delay and what a hop-aligned scheme could report;
        // the bridge adds a hop, and the stage reports what the signal lags by — the figure the
        // design's §7 carries.
        let mut d = stage(DereverbLevel::Light);
        let input = noise(FRAME * 40, 0.1);
        let mut block = input.clone();
        d.process(&mut block, 1);
        let lag = measured_lag(&block, &input);
        assert_eq!(lag, LATENCY, "the signal lags by {lag}");
        assert_eq!(d.latency_frames(), LATENCY);
    }

    #[test]
    fn the_lag_is_the_frame_at_every_block_size_the_stream_can_deliver() {
        // The bridge exists because the blocks are not hop-aligned: the quantum is a power of
        // two from 256 to 2048, and none of those is a multiple of 240. What it buys is that the
        // output does not depend on how the input was cut — the hops fall at the same samples
        // whatever the block size, so the result is the same to the bit — and so the reported
        // lag is one figure, not one per quantum. A hop-aligned bridge, which could report 240,
        // would run out of output on the first 256-frame block and could hold neither.
        let input = noise(FRAME * 40, 0.1);
        let mut whole = input.clone();
        stage(DereverbLevel::Light).process(&mut whole, 1);
        assert_eq!(measured_lag(&whole, &input), LATENCY);

        for block_size in [1, 7, HOP, 256, 512, 1024, 2048] {
            let mut d = stage(DereverbLevel::Light);
            let mut cut = input.clone();
            for block in cut.chunks_mut(block_size) {
                d.process(block, 1);
            }
            assert_eq!(
                cut, whole,
                "a block size of {block_size} changed the output"
            );
            let lag = measured_lag(&cut, &input);
            assert_eq!(
                lag, LATENCY,
                "at a block size of {block_size} the signal lags by {lag}"
            );
            assert_eq!(d.latency_frames(), LATENCY);
        }
    }

    #[test]
    fn with_a_zero_decay_the_transform_pair_is_an_identity() {
        // The STFT round trip on its own: with nothing to subtract, `Light` at a rate where the
        // decay term is negligible is a wire after the delay. Done by driving the design to a
        // negligible decay rather than by a switch, so that the whole hop path runs.
        let mut d = Dereverb::new(FS);
        d.set_level(DereverbLevel::Light);
        d.decay = 0.0;
        let input: Vec<Real> = (0..FRAME * 20)
            .map(|n| (n as Real * std::f32::consts::TAU * 440.0 / FS).sin() * 0.5)
            .collect();
        let mut block = input.clone();
        d.process(&mut block, 1);
        for n in LATENCY + FRAME * 2..block.len() {
            let want = input[n - LATENCY];
            assert!(
                (block[n] - want).abs() < 1.0e-4,
                "sample {n}: {} against {want}",
                block[n]
            );
        }
    }

    #[test]
    fn channels_are_processed_apart_and_stay_in_step() {
        let mut d = stage(DereverbLevel::Medium);
        let mono = noise(FRAME * 10, 0.2);
        let mut block = Vec::with_capacity(mono.len() * 2);
        for &x in &mono {
            block.push(x);
            block.push(0.0);
        }
        d.process(&mut block, 2);
        let right: Vec<Real> = block.iter().skip(1).step_by(2).copied().collect();
        assert!(
            rms(&right[LATENCY..]) < 1.0e-6,
            "the silent channel picked something up"
        );
        let left: Vec<Real> = block.iter().step_by(2).copied().collect();
        assert!(left[..HOP].iter().all(|x| *x == 0.0));
        assert!(
            rms(&left[LATENCY + FRAME..]) > 0.05,
            "the loud channel came out silent"
        );
    }

    #[test]
    fn more_channels_than_it_supports_make_it_stand_aside() {
        let mut d = stage(DereverbLevel::Medium);
        let input: Vec<Real> = (0..(MAX_CHANNELS + 1) * FRAME * 2)
            .map(|n| (n % 97) as Real / 100.0)
            .collect();
        let mut block = input.clone();
        d.process(&mut block, MAX_CHANNELS + 1);
        assert_eq!(block, input);
    }

    #[test]
    fn a_non_finite_sample_does_not_come_out_the_other_side() {
        let mut d = stage(DereverbLevel::Medium);
        let mut block = vec![Real::INFINITY; FRAME * 3];
        d.process(&mut block, 1);
        assert!(block.iter().all(|x| x.is_finite()));
        d.reset();
        let mut block = noise(FRAME * 4, 0.1);
        d.process(&mut block, 1);
        assert!(block.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn a_level_change_while_running_does_not_restart_the_bridge() {
        let mut d = stage(DereverbLevel::Light);
        let mut block = noise(FRAME * 10, 0.1);
        d.process(&mut block, 1);
        d.set_level(DereverbLevel::Strong);
        let mut block = noise(FRAME * 2, 0.1);
        d.process(&mut block, 1);
        assert!(
            block[..HOP].iter().any(|x| x.abs() > 1.0e-6),
            "the bridge was restarted on a level change"
        );
    }

    #[test]
    fn the_room_table_is_what_the_design_says() {
        assert_eq!(Dereverb::room(DereverbLevel::Light), (0.3, -10.0));
        assert_eq!(Dereverb::room(DereverbLevel::Medium), (0.5, -14.0));
        assert_eq!(Dereverb::room(DereverbLevel::Strong), (0.8, -18.0));
        let d = stage(DereverbLevel::Medium);
        let want = (-2.0 * 0.05 * 1000.0_f32.ln() / 0.5).exp();
        assert!(
            (d.decay - want).abs() < 1.0e-6,
            "{} against {want}",
            d.decay
        );
    }
}
