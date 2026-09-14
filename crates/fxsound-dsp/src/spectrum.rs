//! The ten-band spectrum analyser that feeds the visualizer.
//!
//! The tap sits at the very end of the chain, on the post-processing front pair
//! (`dsp/ptutil/dfxp/dfxpProcessReal.cpp:474-511` — "Analyse the output buffer's spectrum", with
//! `spectrum_process_num_channels = 2` unless the stream is mono). Everything downstream of the
//! tap is display: `spectrumGetBandValues` is a plain array copy
//! (`dsp/ptutil/DspUtil/spectrum/spectrumGet.cpp:30-54`) and the GUI does no smoothing of its own.
//!
//! # What is ported verbatim
//!
//! * The ten band centres and the eleven −3 dB crossover points
//!   (`dsp/ptutil/DspUtil/spectrum/spectrumReset.cpp:101-115`).
//! * The per-band sensitivity warps `SPECTRUM_BAND_n_WARP` and the overall
//!   `sensitivity = SPECTRUM_DEFAULT_SENSITIVITY * SPECTRUM_SENSITIVITY_FACTOR = 4.5`
//!   (`dsp/ptutil/include/spectrum.h:28-37,50-52`, `dsp/ptutil/DspUtil/spectrum/u_spectrum.h:26`,
//!   `spectrumSet.cpp:75`).
//! * The one-pole *mean-square* smoother and its coefficient
//!   `alpha = exp(-time_constant / samp_freq)` with `SPECTRUM_DEFAULT_TIME_CONSTANT = 10.0`
//!   (`spectrumSet.cpp:47`, `spectrum.h:47`) — a 0.1 s time constant on the squared value,
//!   independent of sample rate, and therefore 0.2 s on the displayed level.
//! * `level = sqrt(clamp(squared_filtered, .., 1.0))` and the `[0.0, 1.0]` output range
//!   (`spectrumProcess.cpp:173-191`, `spectrum.h:39-40`).
//! * Zero response at DC and at Nyquist. The original gets those from the `1 - z^-2` numerator
//!   shared by all ten resonators (`spectrumProcess.cpp:73-82`); here bin 0 and bin N/2 are
//!   simply never summed into a band, and a DC blocker keeps a constant offset from leaking into
//!   bin 1 through the analysis window.
//!
//! # What is deliberately different
//!
//! The original runs a bank of ten 2-pole resonant bandpasses whose coefficients are hard-coded
//! for 44.1 kHz and **never re-derived** for another rate (`spectrumReset.cpp:118-165`), so on a
//! 48 kHz machine every band centre sits ~8.8 % high. `docs/spec/04-equalizer-visualizer.md`
//! (Open questions, item 2) recommends fixing that in the port rather than reproducing it, and
//! that is what this module does: band energy comes from a windowed real FFT and the band edges
//! are mapped to bins from the *actual* sample rate, so the meter is correct at every rate.
//!
//! Consequences of that swap, all of them intentional:
//!
//! * The original's internal rate reduction (stride 2 above 48 kHz, 4 at 192 kHz,
//!   `spectrumReset.cpp:52-76`) exists only to keep the fixed-coefficient filters near their
//!   design rate. With rate-derived bin edges there is nothing to correct, so there is no
//!   decimation here — and no aliasing from decimating without a low-pass.
//! * The smoother is stepped once per hop instead of once per sample, with
//!   `alpha_hop = exp(-time_constant * HOP_SIZE / fs) = alpha_sample^HOP_SIZE`. The decay rate is
//!   therefore identical; the attack is marginally softer because the value being smoothed is
//!   already a window mean square rather than one instantaneous sample.
//! * For *broadband* input the original reads about `sqrt(pi/2)` ≈ 1.25x higher than this module,
//!   because a first-order Butterworth bandpass has a noise bandwidth `pi/2` times its −3 dB
//!   width. For *tonal* input — what actually drives a music visualizer — the two agree.
//! * The outer bands are open-ended: band 1 reaches down to the first non-DC bin and band 10 up
//!   to the last bin below Nyquist. The original's first-order sections do not stop at their
//!   −3 dB points either, they roll off at 6 dB/octave, so truncating at 42.17 Hz / 13.3 kHz
//!   would make the port *less* faithful, not more.
//! * `fast_sqrt` (`spectrumProcess.cpp:177-191`, a float bit-hack with up to 6 % error) is
//!   replaced by a real `sqrt`, as the spec recommends.
//!
//! # Real-time contract
//!
//! Every buffer — the ring, the window, the FFT input/output/scratch — is allocated in
//! [`SpectrumAnalyser::new`]. `RealFftPlanner` is touched only there. [`push`](SpectrumAnalyser::push)
//! allocates nothing, locks nothing, and cannot panic: all slice access goes through `get`,
//! `chunks_exact` or a power-of-two mask. Worst-case work per call is fixed at construction from
//! `max_block`.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use fxsound_core::{NUM_SPECTRUM_BARS, SpectrumFrame};

use crate::biquad::{Real, SOS_FLOAT_BIAS};

/// `SPECTRUM_MAX_NUM_BANDS` / `DFXP_SPECTRUM_NUM_BANDS` (`dsp/ptutil/include/spectrum.h:25`,
/// `dsp/ptutil/include/dfxpDefs.h:153`). The visualizer draws ten bars per band, hence
/// [`fxsound_core::NUM_SPECTRUM_BARS`] being the same number.
pub const NUM_BANDS: usize = NUM_SPECTRUM_BARS;

/// The ten band centres in Hz (`dsp/ptutil/DspUtil/spectrum/spectrumReset.cpp:107`).
///
/// Four per decade — each centre is `10^0.25` ≈ 1.7783 above the previous one, spanning 56.23 Hz
/// to 10 kHz. (The comment above that table calls this "√10-per-half-decade", which is the same
/// spacing said less clearly.)
///
/// Kept for the integrator's band labels; the analyser itself works from [`BAND_EDGES_HZ`].
pub const BAND_CENTRES_HZ: [Real; NUM_BANDS] = [
    56.23, 100.0, 177.83, 316.228, 562.34, 1000.0, 1778.28, 3162.28, 5623.4, 10000.0,
];

/// The eleven −3 dB crossover points in Hz (`spectrumReset.cpp:110`).
///
/// Band `k` owns every FFT bin whose centre falls in `[BAND_EDGES_HZ[k], BAND_EDGES_HZ[k + 1])`,
/// except that the first and last bands are open-ended — see the module docs.
pub const BAND_EDGES_HZ: [Real; NUM_BANDS + 1] = [
    42.17, 74.99, 133.35, 237.14, 421.70, 749.89, 1333.52, 2371.37, 4216.97, 7498.94, 13335.21,
];

/// `SPECTRUM_BAND_1_WARP` … `SPECTRUM_BAND_10_WARP` (`dsp/ptutil/include/spectrum.h:28-37`).
///
/// A per-band trim so that the ten bars look balanced on real music; it is not a correction for
/// anything physical.
pub const BAND_WARP: [Real; NUM_BANDS] = [0.6, 0.6, 1.0, 1.0, 1.3, 1.3, 1.3, 1.3, 1.5, 1.5];

/// `SPECTRUM_DEFAULT_SENSITIVITY` (`dsp/ptutil/include/spectrum.h:52`).
pub const DEFAULT_SENSITIVITY: Real = 1.0;
/// `SPECTRUM_SENSITIVITY_FACTOR` (`dsp/ptutil/DspUtil/spectrum/u_spectrum.h:26`).
pub const SENSITIVITY_FACTOR: Real = 4.5;
/// `SPECTRUM_DEFAULT_TIME_CONSTANT` (`dsp/ptutil/include/spectrum.h:47`), in nepers per second of
/// the *squared* band value — so the mean square decays with τ = 0.1 s and the displayed level
/// with τ = 0.2 s.
pub const TIME_CONSTANT: Real = 10.0;
/// `SPECTRUM_MAX_OUTPUT_VALUE` (`dsp/ptutil/include/spectrum.h:40`).
pub const MAX_OUTPUT_VALUE: Real = 1.0;

/// Analysis window length.
///
/// 4096 points is 85 ms at 48 kHz, which is just under five cycles of the 56.23 Hz band-1 centre —
/// the shortest window that can resolve the bottom band at all — and gives 11.7 Hz bins, so the
/// narrowest band (42.17–74.99 Hz) still gets three of them. Halving it would make band 1 a
/// single-bin guess; doubling it would make the meter visibly lag the music.
pub const FFT_SIZE: usize = 4096;

/// Samples between analyses.
///
/// 1024 is 21.3 ms at 48 kHz, comfortably faster than both the original's 40 ms band-value store
/// (`DFXP_SPECTRUM_REFRESH_RATE_MSECS`, `dsp/ptutil/include/dfxpDefs.h:168`) and the GUI's 30 Hz
/// repaint, so [`bands`](SpectrumAnalyser::bands) never hands the visualizer a stale frame.
pub const HOP_SIZE: usize = 1024;

const RING_MASK: usize = FFT_SIZE - 1;
const _: () = assert!(FFT_SIZE.is_power_of_two(), "the ring index uses a mask");
const _: () = assert!(HOP_SIZE > 0 && HOP_SIZE <= FFT_SIZE);

/// Rates the analyser will accept. Outside this the caller gets the clamp, not a panic; the
/// original returns `NOT_OKAY` and leaves the meter frozen (`spectrumReset.cpp:55`).
const MIN_SAMPLE_RATE: Real = 8_000.0;
/// `SPECTRUM_MAXIMUM_SAMP_FREQ` is 192 kHz (`u_spectrum.h:30`), but that ceiling only existed to
/// bound the decimation ratio, which this port does not use.
const MAX_SAMPLE_RATE: Real = 768_000.0;

/// Corner of the DC blocker, low enough to leave band 1 (from 42.17 Hz) untouched.
const DC_BLOCK_HZ: Real = 5.0;

/// Below this the smoothed mean square is snapped to zero.
///
/// The original never needs this: its `1.0e-5` bias inside the resonator
/// (`spectrumProcess.cpp:162`) keeps every state well clear of the denormal range — at the cost of
/// a permanent ~5e-4 noise floor on band 1. An FFT has no recursion to protect, so a bias here
/// would be a visible floor and nothing else; a flush costs one compare per band per hop and lets
/// silence reach a true zero, which is what the GUI's `0.0 -> 0.01` substitution expects
/// (`fxsound/Source/GUI/FxVisualizer.cpp:205`).
const DENORMAL_FLOOR: Real = 1.0e-20;

/// One band's bin range, fixed gain, and the two pieces of per-band state.
#[derive(Clone, Copy, Debug)]
struct Band {
    /// First FFT bin in the band.
    lo_bin: usize,
    /// One past the last FFT bin in the band.
    hi_bin: usize,
    /// `(sensitivity * warp)^2`, squared because it multiplies a mean square rather than an
    /// amplitude — the original applies `sensitivity * warp / (num_channels * design_gain)` to the
    /// filter output and *then* squares it (`spectrumReset.cpp:120`, `spectrumProcess.cpp:165-167`).
    gain: Real,
    /// `squared_filtered` (`u_spectrum.h:48`).
    smoothed_ms: Real,
    /// `level` (`u_spectrum.h:47`), already clamped and square-rooted.
    level: Real,
}

/// A ten-band spectrum analyser: post-processing audio in, ten smoothed `0.0..=1.0` magnitudes out.
///
/// Feed it from the audio callback with [`push`](Self::push) and read it from the GUI thread with
/// [`bands`](Self::bands).
pub struct SpectrumAnalyser {
    fft: Arc<dyn RealToComplex<Real>>,
    /// Windowed FFT input. `process_with_scratch` treats it as scratch, so it is refilled every hop.
    indata: Vec<Real>,
    /// `FFT_SIZE / 2 + 1` complex bins.
    spectrum: Vec<Complex<Real>>,
    /// Owned so the FFT never allocates (`RealToComplex::process` would).
    scratch: Vec<Complex<Real>>,
    /// Periodic Hann window.
    window: Vec<Real>,
    /// `1 / (FFT_SIZE * sum(window^2))`, doubled for one-sided bins. Turns `sum |X_k|^2` over a
    /// band straight into that band's mean square, by Parseval.
    power_norm: Real,

    /// Mono ring of the last `FFT_SIZE` samples; `write` indexes the oldest of them.
    ring: Vec<Real>,
    write: usize,
    since_hop: usize,
    /// Worst-case analyses per `push`, budgeted from `max_block` at construction.
    max_hops_per_push: usize,

    bands: [Band; NUM_BANDS],
    sample_rate: Real,
    alpha: Real,
    one_minus_alpha: Real,

    /// DC blocker state: `y[n] = x[n] - x[n-1] + pole * y[n-1]`.
    dc_pole: Real,
    dc_x1: Real,
    dc_y1: Real,
}

impl SpectrumAnalyser {
    /// Plans the FFT and allocates every buffer the analyser will ever use.
    ///
    /// `max_block` is the largest number of frames a single [`push`](Self::push) may carry. It is
    /// the analyser's work budget: a push that honours it runs at most `max_block / HOP_SIZE + 1`
    /// analyses, and one that exceeds it is still handled — the ring stays correct — but the
    /// surplus hops are skipped rather than allowed to overrun the audio deadline.
    ///
    /// This allocates. Call it from the setup path, never from the callback.
    #[must_use]
    pub fn new(sample_rate: Real, max_block: usize) -> Self {
        let mut planner = RealFftPlanner::<Real>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);

        let indata = fft.make_input_vec();
        let spectrum = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();

        // Periodic Hann: -31 dB first sidelobe rolling off at 18 dB/octave, so a tone in one band
        // leaks a vanishing amount into the next one half a decade away.
        let mut window = vec![0.0 as Real; FFT_SIZE];
        let mut sum_sq = 0.0f64;
        for (i, w) in window.iter_mut().enumerate() {
            let phase = core::f64::consts::TAU * i as f64 / FFT_SIZE as f64;
            let v = 0.5 - 0.5 * phase.cos();
            *w = v as Real;
            sum_sq += v * v;
        }
        // Factor 2 because bin 0 and bin N/2 are never summed into a band, so every bin that is
        // used stands for both halves of the two-sided spectrum.
        let power_norm = (2.0 / (FFT_SIZE as f64 * sum_sq)) as Real;

        let sensitivity = DEFAULT_SENSITIVITY * SENSITIVITY_FACTOR;
        let mut bands = [Band {
            lo_bin: 0,
            hi_bin: 0,
            gain: 0.0,
            smoothed_ms: 0.0,
            level: 0.0,
        }; NUM_BANDS];
        for (band, warp) in bands.iter_mut().zip(BAND_WARP.iter()) {
            let g = sensitivity * *warp;
            band.gain = g * g;
        }

        let mut me = Self {
            fft,
            indata,
            spectrum,
            scratch,
            window,
            power_norm,
            ring: vec![0.0 as Real; FFT_SIZE],
            write: 0,
            since_hop: 0,
            max_hops_per_push: (max_block / HOP_SIZE).saturating_add(1),
            bands,
            sample_rate: 0.0,
            alpha: 0.0,
            one_minus_alpha: 1.0,
            dc_pole: 0.0,
            dc_x1: 0.0,
            dc_y1: 0.0,
        };
        me.configure(sample_rate);
        me
    }

    /// Re-derives the bin layout and the smoothing coefficient for a new rate, and clears the
    /// analyser.
    ///
    /// A no-op when the rate has not actually changed, matching the guard the original puts in
    /// front of `spectrumReset` (`dsp/ptutil/DspUtil/spectrum/spectrumProcess.cpp:51-58`). Cheap
    /// enough to be safe from the audio thread, but it discards the current display state, so the
    /// natural place to call it is a format change.
    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let wanted = sanitise_rate(sample_rate);
        if wanted == self.sample_rate {
            return;
        }
        self.configure(wanted);
        self.reset();
    }

    /// Feeds post-processing audio, interleaved, `channels` samples per frame.
    ///
    /// Only the front pair is analysed — the original does the same, deliberately, and says so at
    /// `dsp/ptutil/dfxp/dfxpProcessReal.cpp:478-480`. A mono stream is analysed as-is. The two
    /// channels are averaged: the original sums them and divides the band gain by `num_channels`
    /// (`spectrumProcess.cpp:66-69`, `spectrumReset.cpp:120`), which is the same thing.
    ///
    /// Real-time safe: no allocation, no locks, no panics. A block shorter than [`HOP_SIZE`] is
    /// accumulated until there is enough to analyse; a block longer than [`FFT_SIZE`] simply runs
    /// several analyses.
    pub fn push(&mut self, buffer: &[Real], channels: usize) {
        if channels == 0 {
            return;
        }
        // "front two channels, before reordering" — everything above index 1 is rear/side/centre.
        let used = if channels >= 2 { 2 } else { 1 };
        let inv_used = 1.0 / used as Real;
        let mut hops_left = self.max_hops_per_push;

        for frame in buffer.chunks_exact(channels) {
            let mut sum = 0.0 as Real;
            for s in frame.iter().take(used) {
                sum += *s;
            }
            let sum = sum * inv_used;
            // A driver hiccup must not be able to poison the ring for the next FFT_SIZE samples.
            let x = if sum.is_finite() { sum } else { 0.0 };

            // Stands in for the original's `1 - z^-2` zero at DC (`spectrumProcess.cpp:73-82`).
            // Without it a constant offset spreads into bin 1 through the window and pegs band 1.
            // SOS_FLOAT_BIAS keeps the pole's state out of the denormal range during silence, the
            // same job the `1.0e-5` bias does in the original's resonators.
            let y = x - self.dc_x1 + self.dc_pole * self.dc_y1 + SOS_FLOAT_BIAS;
            self.dc_x1 = x;
            self.dc_y1 = y;

            if let Some(slot) = self.ring.get_mut(self.write) {
                *slot = y;
            }
            self.write = (self.write + 1) & RING_MASK;

            self.since_hop += 1;
            if self.since_hop >= HOP_SIZE {
                self.since_hop = 0;
                if hops_left > 0 {
                    hops_left -= 1;
                    self.analyse();
                }
            }
        }
    }

    /// The ten smoothed band magnitudes, each `0.0..=1.0`, ready for the GUI.
    ///
    /// Index `k` is the band centred on `BAND_CENTRES_HZ[k]`. Linear amplitude, not dB
    /// (`docs/spec/08-dsp-api.md` §10).
    #[must_use]
    pub fn bands(&self) -> SpectrumFrame {
        let mut out = [0.0 as Real; NUM_SPECTRUM_BARS];
        for (dst, band) in out.iter_mut().zip(self.bands.iter()) {
            *dst = band.level;
        }
        out
    }

    /// Clears the history and drops every band to zero.
    ///
    /// The equivalent of `spectrum_ResetFilter` over all ten bands plus the input history
    /// (`dsp/ptutil/DspUtil/spectrum/spectrumProcess.cpp:195-202`, `spectrumReset.cpp:88-99`).
    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write = 0;
        self.since_hop = 0;
        self.dc_x1 = 0.0;
        self.dc_y1 = 0.0;
        for band in self.bands.iter_mut() {
            band.smoothed_ms = 0.0;
            band.level = 0.0;
        }
    }

    /// Everything that depends on the sample rate: bin ranges, smoothing, DC corner.
    fn configure(&mut self, sample_rate: Real) {
        let fs = sanitise_rate(sample_rate);
        self.sample_rate = fs;

        // alpha = exp(-time_constant / samp_freq) per sample (`spectrumSet.cpp:47`), raised to the
        // hop length so the decay rate is the rate-independent 0.1 s the original documents.
        let alpha = (-f64::from(TIME_CONSTANT) * HOP_SIZE as f64 / f64::from(fs)).exp();
        self.alpha = alpha as Real;
        self.one_minus_alpha = (1.0 - alpha) as Real;

        self.dc_pole =
            (-core::f64::consts::TAU * f64::from(DC_BLOCK_HZ) / f64::from(fs)).exp() as Real;

        let bin_hz = f64::from(fs) / FFT_SIZE as f64;
        let nyquist_bin = FFT_SIZE / 2;
        for (k, band) in self.bands.iter_mut().enumerate() {
            // Bin k belongs to the band whose half-open edge interval contains its centre, so the
            // ten ranges tile the spectrum with no gap and no overlap.
            let lo = bin_index_at_or_above(BAND_EDGES_HZ[k], bin_hz);
            let hi = bin_index_at_or_above(BAND_EDGES_HZ[k + 1], bin_hz);

            // Open-ended ends (see module docs), and never bin 0 or bin N/2 — those are where the
            // original's shared `1 - z^-2` numerator has its zeros.
            let lo = if k == 0 { 1 } else { lo.max(1) };
            let hi = if k == NUM_BANDS - 1 { nyquist_bin } else { hi };

            let lo = lo.min(nyquist_bin);
            let hi = hi.clamp(lo, nyquist_bin);
            band.lo_bin = lo;
            band.hi_bin = hi;
        }
    }

    /// One analysis: window the ring, transform, fold the bins into ten levels.
    fn analyse(&mut self) {
        {
            let Self {
                ring,
                window,
                indata,
                write,
                ..
            } = self;
            // `write` is the oldest sample, since the ring is exactly FFT_SIZE long and always full.
            let oldest = *write;
            for (i, (dst, w)) in indata.iter_mut().zip(window.iter()).enumerate() {
                let idx = (oldest + i) & RING_MASK;
                *dst = ring.get(idx).copied().unwrap_or(0.0) * *w;
            }
        }

        {
            let Self {
                fft,
                indata,
                spectrum,
                scratch,
                ..
            } = self;
            // Lengths come from `make_*_vec`, so this cannot fail; bail rather than unwrap anyway.
            if fft
                .process_with_scratch(indata, spectrum, scratch)
                .is_err()
            {
                return;
            }
        }

        let Self {
            bands,
            spectrum,
            alpha,
            one_minus_alpha,
            power_norm,
            ..
        } = self;
        let (alpha, one_minus_alpha, power_norm) = (*alpha, *one_minus_alpha, *power_norm);

        for band in bands.iter_mut() {
            // f64 because a full band can be a thousand bins and the sum spans a wide dynamic range.
            let mut energy = 0.0f64;
            if let Some(bins) = spectrum.get(band.lo_bin..band.hi_bin) {
                for bin in bins {
                    energy += f64::from(bin.norm_sqr());
                }
            }
            let ms = (energy * f64::from(power_norm)) as Real * band.gain;

            // `squared_filtered = one_minus_alpha * tmp + alpha * squared_filtered`
            // (`spectrumProcess.cpp:171`).
            let mut smoothed = one_minus_alpha * ms + alpha * band.smoothed_ms;
            if !smoothed.is_finite() || smoothed < DENORMAL_FLOOR {
                smoothed = 0.0;
            }
            band.smoothed_ms = smoothed;

            // `spectrumProcess.cpp:173-174`: clamp the square, then root it.
            band.level = if smoothed > MAX_OUTPUT_VALUE {
                MAX_OUTPUT_VALUE
            } else {
                smoothed.sqrt()
            };
        }
    }
}

impl core::fmt::Debug for SpectrumAnalyser {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpectrumAnalyser")
            .field("sample_rate", &self.sample_rate)
            .field("fft_size", &FFT_SIZE)
            .field("hop_size", &HOP_SIZE)
            .field("levels", &self.bands())
            .finish()
    }
}

/// First bin whose centre frequency is at or above `hz`.
fn bin_index_at_or_above(hz: Real, bin_hz: f64) -> usize {
    let exact = f64::from(hz) / bin_hz;
    // `ceil` on a negative or non-finite value would be nonsense; neither can occur with the
    // constant edge table, but the clamp costs nothing and keeps the `as usize` cast total.
    let ceiled = exact.ceil();
    if ceiled.is_finite() && ceiled > 0.0 {
        ceiled as usize
    } else {
        0
    }
}

/// Keeps a caller's sample rate inside the range the bin maths is meaningful over.
fn sanitise_rate(sample_rate: Real) -> Real {
    if sample_rate.is_finite() {
        sample_rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE)
    } else {
        48_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    /// Feeds `frames` frames of stereo `source(n)` in `block` sized pushes.
    fn feed(an: &mut SpectrumAnalyser, frames: usize, block: usize, mut source: impl FnMut(usize) -> Real) {
        let mut buf = vec![0.0 as Real; block * 2];
        let mut n = 0;
        let mut left = frames;
        while left > 0 {
            let this = left.min(block);
            let (frames, _) = buf.as_chunks_mut::<2>();
            for frame in frames.iter_mut().take(this) {
                let v = source(n);
                frame[0] = v;
                frame[1] = v;
                n += 1;
            }
            an.push(&buf[..this * 2], 2);
            left -= this;
        }
    }

    fn sine(freq: Real, amp: Real) -> impl FnMut(usize) -> Real {
        move |n| amp * (core::f64::consts::TAU * f64::from(freq) * n as f64 / f64::from(FS)).sin() as Real
    }

    /// A deterministic, spectrally flat-ish source; a real RNG would be a dependency for nothing.
    fn noise(amp: Real) -> impl FnMut(usize) -> Real {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        move |_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let unit = (state >> 40) as Real / 2048.0 - 1.0;
            amp * unit
        }
    }

    fn band_of(freq: Real) -> usize {
        BAND_EDGES_HZ
            .iter()
            .position(|e| freq < *e)
            .map_or(NUM_BANDS - 1, |i| i.saturating_sub(1))
    }

    #[test]
    fn the_band_edges_are_the_original_crossover_points() {
        // spectrumReset.cpp:110 — every centre must sit inside its own band, and the edges must
        // ascend. A transcription slip here would be invisible in every other test.
        assert_eq!(BAND_EDGES_HZ.len(), NUM_BANDS + 1);
        for k in 0..NUM_BANDS {
            assert!(
                BAND_EDGES_HZ[k] < BAND_CENTRES_HZ[k] && BAND_CENTRES_HZ[k] < BAND_EDGES_HZ[k + 1],
                "centre {} is outside band {k}",
                BAND_CENTRES_HZ[k]
            );
        }
        // A quarter decade per band, on the edges and on the centres alike.
        let quarter_decade = 10.0f32.powf(0.25);
        for k in 0..NUM_BANDS {
            let ratio = BAND_EDGES_HZ[k + 1] / BAND_EDGES_HZ[k];
            assert!((ratio - quarter_decade).abs() < 0.01, "band {k} edge ratio {ratio}");
        }
        for k in 1..NUM_BANDS {
            let ratio = BAND_CENTRES_HZ[k] / BAND_CENTRES_HZ[k - 1];
            assert!((ratio - quarter_decade).abs() < 0.01, "band {k} centre ratio {ratio}");
        }
    }

    #[test]
    fn the_bin_ranges_tile_the_spectrum_without_gaps_or_overlap() {
        let an = SpectrumAnalyser::new(FS, 1024);
        assert_eq!(an.bands[0].lo_bin, 1, "bin 0 is DC and must never be counted");
        assert_eq!(
            an.bands[NUM_BANDS - 1].hi_bin,
            FFT_SIZE / 2,
            "the Nyquist bin must never be counted"
        );
        for k in 1..NUM_BANDS {
            assert_eq!(
                an.bands[k - 1].hi_bin,
                an.bands[k].lo_bin,
                "gap or overlap between bands {} and {k}",
                k - 1
            );
        }
        // The narrowest band still has enough resolution to mean something.
        assert!(
            an.bands[0].hi_bin - an.bands[0].lo_bin >= 3,
            "band 1 got only {} bins",
            an.bands[0].hi_bin - an.bands[0].lo_bin
        );
    }

    #[test]
    fn a_pure_sine_lights_the_band_that_contains_it_and_leaves_the_others_near_zero() {
        for freq in [56.23 as Real, 316.228, 1000.0, 3162.28, 5623.4] {
            let want = band_of(freq);
            let mut an = SpectrumAnalyser::new(FS, 512);
            feed(&mut an, 96_000, 512, sine(freq, 0.9));
            let b = an.bands();

            assert!(b[want] > 0.9, "{freq} Hz only reached {:.4} in band {want}", b[want]);
            for (k, v) in b.iter().enumerate() {
                if k != want {
                    assert!(*v < 0.05, "{freq} Hz leaked {v:.4} into band {k}");
                }
            }
        }
    }

    #[test]
    fn silence_decays_the_bands_at_the_documented_time_constant() {
        let mut an = SpectrumAnalyser::new(FS, HOP_SIZE);
        // Amplitude chosen so band 6 settles well below the 1.0 clamp, where the decay is visible.
        feed(&mut an, 96_000, HOP_SIZE, sine(1000.0, 0.17));
        let band = band_of(1000.0);
        let lit = an.bands()[band];
        assert!(lit > 0.3 && lit < 0.95, "test signal mis-scaled: {lit:.4}");

        // Flush the analysis window first, so what follows is pure decay with no residual signal.
        feed(&mut an, FFT_SIZE, HOP_SIZE, |_| 0.0);
        let start = an.bands()[band];

        const HOPS: usize = 9;
        feed(&mut an, HOPS * HOP_SIZE, HOP_SIZE, |_| 0.0);
        let end = an.bands()[band];

        // The level is the square root of a mean square that decays as exp(-time_constant * t),
        // so the level itself decays as exp(-time_constant * t / 2) — 0.2 s per neper.
        let secs = (HOPS * HOP_SIZE) as Real / FS;
        let expected = start * (-0.5 * TIME_CONSTANT * secs).exp();
        assert!(
            (end - expected).abs() < 0.01 * start.max(1e-6),
            "decayed to {end:.6}, expected {expected:.6} after {secs:.4} s"
        );

        // And the documented figure itself: 0.2 s knocks the level down by 1/e.
        assert!((0.2 - 2.0 / TIME_CONSTANT).abs() < 1e-6);
    }

    #[test]
    fn a_full_scale_signal_never_pushes_a_band_above_one() {
        for amp in [1.0 as Real, 0.999] {
            let mut an = SpectrumAnalyser::new(FS, 4096);
            feed(&mut an, 96_000, 4096, noise(amp));
            for (k, v) in an.bands().iter().enumerate() {
                assert!((0.0..=MAX_OUTPUT_VALUE).contains(v), "noise band {k} = {v}");
            }

            let mut an = SpectrumAnalyser::new(FS, 4096);
            feed(&mut an, 96_000, 4096, sine(1000.0, amp));
            for (k, v) in an.bands().iter().enumerate() {
                assert!((0.0..=MAX_OUTPUT_VALUE).contains(v), "sine band {k} = {v}");
            }
        }
    }

    #[test]
    fn a_block_far_smaller_than_the_fft_reads_the_same_as_a_large_one() {
        let mut tiny = SpectrumAnalyser::new(FS, 1);
        let mut huge = SpectrumAnalyser::new(FS, 200_000);
        feed(&mut tiny, 96_000, 1, sine(1000.0, 0.3));
        feed(&mut huge, 96_000, 96_000, sine(1000.0, 0.3));

        let (a, b) = (tiny.bands(), huge.bands());
        for k in 0..NUM_BANDS {
            assert!(
                (a[k] - b[k]).abs() < 1e-3,
                "band {k}: one-frame pushes gave {:.6}, a single 96000-frame push gave {:.6}",
                a[k],
                b[k]
            );
        }
        assert!(a[band_of(1000.0)] > 0.2, "the 1 kHz band never lit: {:?}", a);
    }

    #[test]
    fn a_single_push_far_larger_than_the_fft_is_handled() {
        let mut an = SpectrumAnalyser::new(FS, 200_000);
        let mut source = sine(3162.28, 0.5);
        let mut buf = vec![0.0 as Real; 200_000 * 2];
        for (n, frame) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let v = source(n);
            frame[0] = v;
            frame[1] = v;
        }
        an.push(&buf, 2);

        let b = an.bands();
        assert!(b[band_of(3162.28)] > 0.5, "band 8 = {:.4}", b[band_of(3162.28)]);
        assert!(b.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn dc_is_rejected_and_white_noise_stays_finite() {
        let mut dc = SpectrumAnalyser::new(FS, 1024);
        feed(&mut dc, 96_000, 1024, |_| 1.0);
        for (k, v) in dc.bands().iter().enumerate() {
            assert!(v.is_finite(), "DC made band {k} non-finite");
            // The original's shared `1 - z^-2` numerator has a zero at DC; so does this.
            assert!(*v < 0.01, "DC lit band {k} at {v:.4}");
        }

        let mut wn = SpectrumAnalyser::new(FS, 1024);
        feed(&mut wn, 96_000, 1024, noise(0.7));
        for (k, v) in wn.bands().iter().enumerate() {
            assert!(v.is_finite() && *v >= 0.0, "noise made band {k} = {v}");
        }
        assert!(
            wn.bands().iter().any(|v| *v > 0.05),
            "white noise lit nothing: {:?}",
            wn.bands()
        );
    }

    #[test]
    fn a_non_finite_sample_cannot_poison_the_analyser() {
        let mut an = SpectrumAnalyser::new(FS, 1024);
        feed(&mut an, 4096, 1024, |n| if n % 997 == 0 { Real::NAN } else { 0.5 });
        feed(&mut an, 96_000, 1024, sine(1000.0, 0.3));
        let b = an.bands();
        assert!(b.iter().all(|v| v.is_finite()), "{b:?}");
        assert!(b[band_of(1000.0)] > 0.2, "the analyser never recovered: {b:?}");
    }

    #[test]
    fn changing_the_sample_rate_moves_the_bins_and_keeps_the_tone_in_its_band() {
        let mut an = SpectrumAnalyser::new(44_100.0, 1024);
        let at_44k = an.bands[5].lo_bin;
        an.set_sample_rate(96_000.0);
        assert!(
            an.bands[5].lo_bin < at_44k,
            "a higher rate must map 749.89 Hz to a lower bin index"
        );
        assert_eq!(an.bands()[0], 0.0, "a rate change clears the display");

        // The fix the spec asks for: the same tone lands in the same band at any rate, which the
        // original's 44.1 kHz-only coefficients cannot manage (`spectrumReset.cpp:118-165`).
        for fs in [44_100.0 as Real, 48_000.0, 96_000.0] {
            let mut an = SpectrumAnalyser::new(fs, 1024);
            let frames = (2.0 * fs) as usize;
            let mut source = {
                let mut n = 0usize;
                move |_| {
                    let v = 0.9
                        * (core::f64::consts::TAU * 1000.0 * n as f64 / f64::from(fs)).sin() as Real;
                    n += 1;
                    v
                }
            };
            feed(&mut an, frames, 1024, &mut source);
            let b = an.bands();
            assert!(b[5] > 0.9, "1 kHz at {fs} Hz only reached {:.4}", b[5]);
        }
    }

    #[test]
    fn a_mono_stream_reads_the_same_as_the_identical_stereo_stream() {
        let mut mono = SpectrumAnalyser::new(FS, 1024);
        let mut source = sine(1000.0, 0.3);
        let buf: Vec<Real> = (0..96_000).map(&mut source).collect();
        for chunk in buf.chunks(1024) {
            mono.push(chunk, 1);
        }

        let mut stereo = SpectrumAnalyser::new(FS, 1024);
        feed(&mut stereo, 96_000, 1024, sine(1000.0, 0.3));

        let (a, b) = (mono.bands(), stereo.bands());
        for k in 0..NUM_BANDS {
            assert!((a[k] - b[k]).abs() < 1e-5, "band {k}: mono {} vs stereo {}", a[k], b[k]);
        }
    }

    #[test]
    fn reset_returns_every_band_to_zero() {
        let mut an = SpectrumAnalyser::new(FS, 1024);
        feed(&mut an, 96_000, 1024, sine(1000.0, 0.9));
        assert!(an.bands().iter().any(|v| *v > 0.5));
        an.reset();
        assert_eq!(an.bands(), [0.0; NUM_SPECTRUM_BARS]);

        // And the ring really is empty: one hop of silence must not resurrect anything.
        feed(&mut an, HOP_SIZE, HOP_SIZE, |_| 0.0);
        assert_eq!(an.bands(), [0.0; NUM_SPECTRUM_BARS]);
    }

    #[test]
    fn an_empty_or_degenerate_push_is_a_no_op() {
        let mut an = SpectrumAnalyser::new(FS, 1024);
        an.push(&[], 2);
        an.push(&[0.5, 0.5], 0);
        an.push(&[0.5], 2); // one channel short of a frame
        assert_eq!(an.bands(), [0.0; NUM_SPECTRUM_BARS]);
    }
}
