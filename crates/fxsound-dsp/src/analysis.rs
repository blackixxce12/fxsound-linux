//! Measurement, as opposed to processing: what a signal *is*, rather than what to do to it.
//!
//! Every other module in this crate exists to change audio. This one exists to describe it, and it
//! is here rather than in a test because the four things it measures are the four a listener's
//! vocabulary maps onto — tone, loudness, headroom, and the ratio between the last two — and
//! because a measurement that lives in a test file cannot be reused by the next test that needs it.
//!
//! * [`Ltas`] — long-term average spectrum on the 29 third-octave centres from 31.5 Hz to 20 kHz.
//!   Welch's method: periodic Hann, 8192-point real FFT, 50 % overlap, power averaged across every
//!   analysis and summed into bands. This is *tone*.
//! * [`LoudnessMeter`] — ITU-R BS.1770 integrated loudness, with the K-weighting pre-filter pair
//!   and both gates. This is *loudness*, and it is not peak, RMS or anything else that is easy.
//! * [`TruePeakMeter`] — the peak of the reconstructed waveform, not of the samples, found by 4×
//!   polyphase interpolation. This is *headroom*.
//! * [`Measurement::plr_db`] — peak-to-loudness ratio, true peak minus integrated loudness. This is
//!   how hard the material has been compressed, and it is the one number that says whether a
//!   dynamics stage did anything.
//!
//! [`ProgramMeter`] runs all three over one interleaved stream and hands back a [`Measurement`].
//!
//! # Contract
//!
//! The crate forbids `unsafe` and promises that nothing allocates after construction, and this
//! module keeps both. Every buffer — FFT scratch, window, ring, the block history the loudness
//! gate needs — is sized in `new`. That is not because a meter runs on the audio thread (none of
//! these do); it is so that the same code could, and so that a measurement never perturbs what it
//! is measuring.
//!
//! # What this module is not
//!
//! It is not a conformance-tested BS.1770 implementation. The filter design is derived from the
//! recommendation's own analogue prototype and is checked against the coefficient table the
//! recommendation publishes for 48 kHz (see the tests), which is a strong anchor — but no EBU Tech
//! 3341 test vector is bundled, so the absolute calibration is verified only by construction and
//! by the relative invariants a loudness meter must satisfy. For ranking one render against
//! another, which is what this module was built for, that is enough. For certifying a master, use
//! a meter someone has certified.
//!
//! The true-peak interpolator is likewise *a* 4×, 48-tap polyphase interpolator of the length and
//! rate BS.1770-4 Annex 2 calls for, but it is a Blackman-windowed sinc designed here rather than
//! the coefficient table printed in the annex. It is therefore a good true-peak estimate and not a
//! standards-conformant one.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::biquad::{BiquadCoeffs, MAX_CHANNELS, Real, Section};

/// Third-octave centres from 31.5 Hz to 20 kHz — the resolution a listener compares tone at, and
/// the same grid `tests/shipped_presets.rs` already judges an equalizer curve on.
pub const THIRD_OCTAVE_CENTRES: [Real; NUM_THIRD_OCTAVES] = [
    31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0,
    800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0,
    12500.0, 16000.0, 20000.0,
];

/// How many entries [`THIRD_OCTAVE_CENTRES`] has.
pub const NUM_THIRD_OCTAVES: usize = 29;

/// A band with no energy in it reads this, rather than `-inf`, so that arithmetic downstream stays
/// finite. 200 dB below full scale is far under any dither floor.
pub const SILENT_BAND_DB: Real = -200.0;

/// The half-width of a third-octave band, as a frequency ratio: `2^(1/6)`.
const THIRD_OCTAVE_EDGE: f64 = 1.122_462_048_309_373;

// ---------------------------------------------------------------------------------------------
// Long-term average spectrum
// ---------------------------------------------------------------------------------------------

/// Analysis length. 8192 points at 48 kHz is 5.9 Hz per bin and 171 ms per analysis — fine enough
/// that the 31.5 Hz band still gets two bins, long enough that the average is genuinely long-term.
const LTAS_FFT: usize = 8192;
/// 50 % overlap, the standard Welch hop for a Hann window.
const LTAS_HOP: usize = LTAS_FFT / 2;
const LTAS_RING_MASK: usize = LTAS_FFT - 1;

/// Long-term average spectrum, reported as 29 third-octave levels in dB relative to full scale.
///
/// Feed it mono with [`push`](Self::push) — as much or as little at a time as is convenient — and
/// read [`levels_db`](Self::levels_db) once the whole programme has gone through.
///
/// A sine at a band centre and at full scale reads −3.01 dB in that band, because that is its mean
/// square. Pink noise reads flat, white noise rises 1 dB per band.
pub struct Ltas {
    fft: Arc<dyn RealToComplex<Real>>,
    /// Windowed FFT input. `process_with_scratch` uses it as scratch, so it is refilled per hop.
    indata: Vec<Real>,
    spectrum: Vec<Complex<Real>>,
    scratch: Vec<Complex<Real>>,
    window: Vec<Real>,
    /// `2 / (N * sum(window^2))` — the noise-power normalisation, so that summing `|X_k|^2` over a
    /// band's bins gives that band's mean square. It is the correct one for broadband material and
    /// it is also correct for a tone, provided the tone's main lobe lies inside the band, which at
    /// third-octave width it always does above 31.5 Hz.
    power_norm: f64,

    ring: Vec<Real>,
    write: usize,
    filled: usize,
    since_hop: usize,

    /// `[lo, hi)` bin range per band.
    bins: [(usize, usize); NUM_THIRD_OCTAVES],
    band_power: [f64; NUM_THIRD_OCTAVES],
    analyses: u64,
    sample_rate: Real,
}

impl Ltas {
    /// Plans the FFT and allocates every buffer. This allocates; nothing after it does.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let mut planner = RealFftPlanner::<Real>::new();
        let fft = planner.plan_fft_forward(LTAS_FFT);
        let indata = fft.make_input_vec();
        let spectrum = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();

        let mut window = vec![0.0 as Real; LTAS_FFT];
        let mut sum_sq = 0.0f64;
        for (i, w) in window.iter_mut().enumerate() {
            let phase = core::f64::consts::TAU * i as f64 / LTAS_FFT as f64;
            let v = 0.5 - 0.5 * phase.cos();
            *w = v as Real;
            sum_sq += v * v;
        }
        // Factor 2 because only the one-sided bins are summed: each stands for both halves of the
        // two-sided spectrum.
        let power_norm = 2.0 / (LTAS_FFT as f64 * sum_sq);

        let mut me = Self {
            fft,
            indata,
            spectrum,
            scratch,
            window,
            power_norm,
            ring: vec![0.0 as Real; LTAS_FFT],
            write: 0,
            filled: 0,
            since_hop: 0,
            bins: [(0, 0); NUM_THIRD_OCTAVES],
            band_power: [0.0; NUM_THIRD_OCTAVES],
            analyses: 0,
            sample_rate: sample_rate.max(1.0),
        };
        me.plan_bands();
        me
    }

    /// Re-plan for a new rate. Clears everything already accumulated, because levels measured at
    /// two different rates are not the same measurement.
    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sample_rate.max(1.0);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.plan_bands();
        self.reset();
    }

    #[must_use]
    pub const fn sample_rate(&self) -> Real {
        self.sample_rate
    }

    fn plan_bands(&mut self) {
        let bin_hz = f64::from(self.sample_rate) / LTAS_FFT as f64;
        let nyquist_bin = LTAS_FFT / 2;
        for (band, centre) in self.bins.iter_mut().zip(THIRD_OCTAVE_CENTRES.iter()) {
            let lo_hz = f64::from(*centre) / THIRD_OCTAVE_EDGE;
            let hi_hz = f64::from(*centre) * THIRD_OCTAVE_EDGE;
            // A bin belongs to the band whose half-open edge interval contains its centre, so the
            // 29 ranges never overlap.
            let lo = (lo_hz / bin_hz).ceil().max(1.0) as usize;
            let hi = (hi_hz / bin_hz).ceil().max(1.0) as usize;
            let lo = lo.min(nyquist_bin);
            let mut hi = hi.clamp(lo, nyquist_bin);
            // The lowest bands are narrower than one bin at some rates. Give them the one bin
            // their centre falls in rather than reporting silence.
            if hi == lo {
                hi = (lo + 1).min(nyquist_bin + 1);
            }
            *band = (lo, hi);
        }
    }

    /// Clear the accumulated average and the input history.
    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write = 0;
        self.filled = 0;
        self.since_hop = 0;
        self.band_power = [0.0; NUM_THIRD_OCTAVES];
        self.analyses = 0;
    }

    /// How many analyses have been folded into the average. Zero means [`levels_db`] has nothing
    /// to report: the stream was shorter than one 8192-sample window.
    ///
    /// [`levels_db`]: Self::levels_db
    #[must_use]
    pub const fn analyses(&self) -> u64 {
        self.analyses
    }

    /// Feed mono samples. Any length; the hop is handled internally.
    pub fn push(&mut self, samples: &[Real]) {
        for &sample in samples {
            self.ring[self.write] = sample;
            self.write = (self.write + 1) & LTAS_RING_MASK;
            self.filled = (self.filled + 1).min(LTAS_FFT);
            self.since_hop += 1;
            if self.since_hop >= LTAS_HOP && self.filled >= LTAS_FFT {
                self.since_hop = 0;
                self.analyse();
            }
        }
    }

    fn analyse(&mut self) {
        {
            let Self {
                ring,
                window,
                indata,
                write,
                ..
            } = self;
            // The ring is exactly one window long and full, so `write` indexes the oldest sample.
            let oldest = *write;
            for (i, (dst, w)) in indata.iter_mut().zip(window.iter()).enumerate() {
                *dst = ring[(oldest + i) & LTAS_RING_MASK] * *w;
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
            if fft.process_with_scratch(indata, spectrum, scratch).is_err() {
                return;
            }
        }

        let norm = self.power_norm;
        for (band, &(lo, hi)) in self.band_power.iter_mut().zip(self.bins.iter()) {
            let mut energy = 0.0f64;
            if let Some(bins) = self.spectrum.get(lo..hi) {
                for bin in bins {
                    energy += f64::from(bin.norm_sqr());
                }
            }
            *band += energy * norm;
        }
        self.analyses += 1;
    }

    /// The averaged band levels in dB relative to full scale.
    ///
    /// Returns `false` and writes [`SILENT_BAND_DB`] everywhere when nothing has been analysed.
    pub fn levels_db(&self, out: &mut [Real; NUM_THIRD_OCTAVES]) -> bool {
        if self.analyses == 0 {
            out.fill(SILENT_BAND_DB);
            return false;
        }
        let n = self.analyses as f64;
        for (dst, power) in out.iter_mut().zip(self.band_power.iter()) {
            let mean = power / n;
            let db = if mean > 0.0 {
                (10.0 * mean.log10()) as Real
            } else {
                SILENT_BAND_DB
            };
            *dst = db.max(SILENT_BAND_DB);
        }
        true
    }
}

impl core::fmt::Debug for Ltas {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ltas")
            .field("sample_rate", &self.sample_rate)
            .field("analyses", &self.analyses)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// BS.1770 loudness
// ---------------------------------------------------------------------------------------------

/// The K-weighting shelf's analogue prototype, BS.1770-4 §2.1: gain, Q and corner. These three
/// numbers, put through the bilinear transform, are what produce the recommendation's published
/// 48 kHz coefficient table — which the tests check this against.
const SHELF_GAIN_DB: f64 = 3.999_843_853_973_347;
const SHELF_Q: f64 = 0.707_175_236_955_419_6;
const SHELF_HZ: f64 = 1_681.974_450_955_533;

/// The K-weighting high-pass ("RLB") prototype, BS.1770-4 §2.2.
const HIGHPASS_Q: f64 = 0.500_327_037_323_877_3;
const HIGHPASS_HZ: f64 = 38.135_470_876_024_44;

/// BS.1770-4 §3: the offset that turns mean K-weighted power into LKFS.
const LOUDNESS_OFFSET: f64 = -0.691;
/// The absolute gate, BS.1770-4 §5.1 step 3.
pub const ABSOLUTE_GATE_LUFS: Real = -70.0;
/// The relative gate, in LU below the ungated mean of the blocks that passed the absolute one.
pub const RELATIVE_GATE_LU: Real = -10.0;
/// Gating block length, BS.1770-4 §5.1: 400 ms.
const GATE_BLOCK_MS: f64 = 400.0;
/// 75 % overlap, so a block starts every 100 ms.
const GATE_STEPS_PER_BLOCK: usize = 4;

/// ITU-R BS.1770 integrated loudness.
///
/// Both gates are implemented: the fixed −70 LUFS absolute gate that keeps silence out of the
/// average, and the −10 LU relative gate that keeps quiet passages from dragging a loud programme
/// down. Without them this would be a K-weighted RMS meter and would disagree with every other
/// loudness reading in the world.
pub struct LoudnessMeter {
    shelf: Section,
    highpass: Section,
    channels: usize,
    /// BS.1770-4 Table 3 channel weights `G`.
    weights: [f64; MAX_CHANNELS],

    /// Samples per 100 ms step.
    step_frames: usize,
    /// Samples per 400 ms block.
    block_frames: usize,

    /// Sum of `y^2` per channel over the step being filled.
    step_sum: [f64; MAX_CHANNELS],
    step_filled: usize,
    /// The last four completed steps, per channel, as a ring.
    steps: [[f64; MAX_CHANNELS]; GATE_STEPS_PER_BLOCK],
    step_write: usize,
    steps_seen: usize,

    /// `sum_ch G_ch * z_ch` per completed gating block. Preallocated; see [`Self::new`].
    blocks: Vec<f64>,
    dropped: usize,

    sample_rate: Real,
}

impl LoudnessMeter {
    /// `capacity_seconds` sizes the block history, which is the only thing here that scales with
    /// programme length. Material longer than that still measures — the surplus blocks are counted
    /// by [`dropped_blocks`](Self::dropped_blocks) rather than silently averaged in, so a reading
    /// that overran says so instead of lying.
    ///
    /// This allocates. Nothing after it does.
    #[must_use]
    pub fn new(sample_rate: Real, channels: usize, capacity_seconds: Real) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let channels = channels.clamp(1, MAX_CHANNELS);
        let block_frames = ((f64::from(sample_rate) * GATE_BLOCK_MS / 1000.0).round() as usize)
            .max(GATE_STEPS_PER_BLOCK);
        let step_frames = (block_frames / GATE_STEPS_PER_BLOCK).max(1);
        let block_frames = step_frames * GATE_STEPS_PER_BLOCK;
        let capacity = ((f64::from(capacity_seconds.max(0.0)) * 1000.0 / (GATE_BLOCK_MS / 4.0))
            .ceil() as usize)
            .max(1);

        // BS.1770-4 Table 3: front and centre count 1.0, the surrounds 1.41, LFE not at all. The
        // layouts this port sees put LFE at index 3 when there is one, which is the assumption
        // `set_channel_weight` exists to override.
        let mut weights = [0.0f64; MAX_CHANNELS];
        for (i, weight) in weights.iter_mut().enumerate().take(channels) {
            *weight = match i {
                0..=2 => 1.0,
                3 if channels >= 6 => 0.0,
                _ => 1.41,
            };
        }
        if channels <= 2 {
            for (i, weight) in weights.iter_mut().enumerate().take(channels) {
                *weight = if i < 2 { 1.0 } else { 0.0 };
            }
        }

        let mut me = Self {
            shelf: Section::new(),
            highpass: Section::new(),
            channels,
            weights,
            step_frames,
            block_frames,
            step_sum: [0.0; MAX_CHANNELS],
            step_filled: 0,
            steps: [[0.0; MAX_CHANNELS]; GATE_STEPS_PER_BLOCK],
            step_write: 0,
            steps_seen: 0,
            blocks: Vec::with_capacity(capacity),
            dropped: 0,
            sample_rate,
        };
        me.design();
        me
    }

    /// Override one channel's BS.1770 weight `G`, for a layout whose channel order is not the one
    /// [`new`](Self::new) assumes.
    pub fn set_channel_weight(&mut self, channel: usize, weight: Real) {
        if let Some(slot) = self.weights.get_mut(channel) {
            *slot = f64::from(weight);
        }
    }

    fn design(&mut self) {
        let fs = f64::from(self.sample_rate);
        self.shelf.coeffs = high_shelf(SHELF_HZ, SHELF_Q, SHELF_GAIN_DB, fs);
        self.highpass.coeffs = highpass(HIGHPASS_HZ, HIGHPASS_Q, fs);
    }

    /// Clear the filters and everything measured so far.
    pub fn reset(&mut self) {
        self.shelf.reset();
        self.highpass.reset();
        self.step_sum = [0.0; MAX_CHANNELS];
        self.step_filled = 0;
        self.steps = [[0.0; MAX_CHANNELS]; GATE_STEPS_PER_BLOCK];
        self.step_write = 0;
        self.steps_seen = 0;
        self.blocks.clear();
        self.dropped = 0;
    }

    /// Gating blocks that did not fit the history sized at construction.
    ///
    /// Any value but zero means [`integrated_lufs`](Self::integrated_lufs) measured only the start
    /// of the programme.
    #[must_use]
    pub const fn dropped_blocks(&self) -> usize {
        self.dropped
    }

    /// Feed interleaved samples. `channels` must be the count given to [`new`](Self::new).
    pub fn push(&mut self, interleaved: &[Real]) {
        if self.channels == 0 {
            return;
        }
        for frame in interleaved.chunks_exact(self.channels) {
            for (channel, &sample) in frame.iter().enumerate() {
                let shelved = self.shelf.tick_general(channel, sample);
                let y = self.highpass.tick_general(channel, shelved);
                let y = f64::from(y);
                self.step_sum[channel] += y * y;
            }
            self.step_filled += 1;
            if self.step_filled >= self.step_frames {
                self.close_step();
            }
        }
    }

    fn close_step(&mut self) {
        self.steps[self.step_write] = self.step_sum;
        self.step_write = (self.step_write + 1) % GATE_STEPS_PER_BLOCK;
        self.steps_seen += 1;
        self.step_sum = [0.0; MAX_CHANNELS];
        self.step_filled = 0;

        if self.steps_seen < GATE_STEPS_PER_BLOCK {
            return;
        }
        let mut weighted = 0.0f64;
        for channel in 0..self.channels {
            let mut sum = 0.0f64;
            for step in &self.steps {
                sum += step[channel];
            }
            weighted += self.weights[channel] * sum / self.block_frames as f64;
        }
        if self.blocks.len() < self.blocks.capacity() {
            self.blocks.push(weighted);
        } else {
            self.dropped += 1;
        }
    }

    /// Integrated loudness in LUFS, or `None` when no gating block survived the gates — which is
    /// what silence, or material shorter than 400 ms, correctly produces.
    #[must_use]
    pub fn integrated_lufs(&self) -> Option<Real> {
        let absolute = f64::from(ABSOLUTE_GATE_LUFS);
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for &power in &self.blocks {
            if block_loudness(power) > absolute {
                sum += power;
                count += 1;
            }
        }
        if count == 0 {
            return None;
        }
        let relative = block_loudness(sum / count as f64) + f64::from(RELATIVE_GATE_LU);

        let mut sum = 0.0f64;
        let mut count = 0usize;
        for &power in &self.blocks {
            let loudness = block_loudness(power);
            if loudness > absolute && loudness > relative {
                sum += power;
                count += 1;
            }
        }
        if count == 0 {
            return None;
        }
        Some(block_loudness(sum / count as f64) as Real)
    }
}

impl core::fmt::Debug for LoudnessMeter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoudnessMeter")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("blocks", &self.blocks.len())
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

/// BS.1770-4 §5.1 step 1: block loudness from summed weighted mean square.
fn block_loudness(power: f64) -> f64 {
    if power > 0.0 {
        LOUDNESS_OFFSET + 10.0 * power.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// The shelf's "bandwidth gain" exponent.
///
/// This is the one number in the K-weighting that is not in the recommendation. BS.1770 publishes
/// the analogue prototype and a coefficient table for 48 kHz, and leaves the design in between
/// unstated; a textbook RBJ high shelf built from the same gain, Q and corner misses the published
/// table by 0.4 %, which is enough to be a different filter. The exponent below is the one that
/// reproduces the table exactly, and it comes from De Man et al., "A Detailed Look at the ITU-R
/// BS.1770 Loudness Algorithm" — the same value every open implementation uses, for the same
/// reason. `k_weighting_reproduces_the_published_48_khz_table` is what holds it honest.
const SHELF_BANDWIDTH_EXPONENT: f64 = 0.499_666_774_155;

/// The K-weighting high shelf, BS.1770-4 §2.1.
fn high_shelf(hz: f64, q: f64, gain_db: f64, fs: f64) -> BiquadCoeffs {
    let k = (core::f64::consts::PI * hz / fs).tan();
    let vh = 10.0f64.powf(gain_db / 20.0);
    let vb = vh.powf(SHELF_BANDWIDTH_EXPONENT);
    let k2 = k * k;
    let a0 = 1.0 + k / q + k2;

    BiquadCoeffs {
        b0: ((vh + vb * k / q + k2) / a0) as Real,
        b1: ((2.0 * (k2 - vh)) / a0) as Real,
        b2: ((vh - vb * k / q + k2) / a0) as Real,
        a1: ((2.0 * (k2 - 1.0)) / a0) as Real,
        a2: ((1.0 - k / q + k2) / a0) as Real,
        on: true,
    }
}

/// The K-weighting high pass — the RLB curve, BS.1770-4 §2.2.
///
/// The numerator is `1, -2, 1` exactly, un-normalised, which is what the recommendation's table
/// says and what makes this not quite a textbook Butterworth high pass.
fn highpass(hz: f64, q: f64, fs: f64) -> BiquadCoeffs {
    let k = (core::f64::consts::PI * hz / fs).tan();
    let k2 = k * k;
    let a0 = 1.0 + k / q + k2;

    BiquadCoeffs {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: ((2.0 * (k2 - 1.0)) / a0) as Real,
        a2: ((1.0 - k / q + k2) / a0) as Real,
        on: true,
    }
}

// ---------------------------------------------------------------------------------------------
// True peak
// ---------------------------------------------------------------------------------------------

/// BS.1770-4 Annex 2 asks for at least 192 kHz, which 4× reaches from 48 kHz.
const TP_PHASES: usize = 4;
/// 12 taps per phase, 48 in the prototype — the annex's length.
const TP_TAPS: usize = 12;
const TP_PROTOTYPE: usize = TP_PHASES * TP_TAPS;

/// The peak of the waveform between the samples, not of the samples.
///
/// A signal whose samples never exceed full scale can still clip a converter, and a limiter that
/// reports −0.3 dBFS from its sample peak can be handing a D/A +0.4 dBTP. That difference is the
/// entire reason this exists.
///
/// One property to know before reading a number off it: the delay line starts at zero, so material
/// that begins with a hard step — a block of DC, a render started mid-waveform — rings, and the
/// first few samples read up to 11 % high. That is the interpolator being correct about a
/// discontinuity rather than the meter being wrong, but it is a reason to feed whole programmes.
pub struct TruePeakMeter {
    /// `coeffs[phase][tap]`, each phase normalised to unity DC gain.
    coeffs: [[Real; TP_TAPS]; TP_PHASES],
    /// Per-channel delay line, `channels * TP_TAPS`, newest at `write`.
    history: Vec<Real>,
    write: usize,
    channels: usize,
    peak: Real,
}

impl TruePeakMeter {
    /// Designs the interpolator and allocates the delay lines. This allocates; nothing after does.
    #[must_use]
    pub fn new(channels: usize) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);

        // A Blackman-windowed sinc, centred on tap 24 of 48 so that phase 0 lands exactly on the
        // input samples and reproduces them bit for bit — the meter can then never read *below*
        // the sample peak, which a centre at 23.5 would allow.
        let mut prototype = [0.0f64; TP_PROTOTYPE];
        for (m, tap) in prototype.iter_mut().enumerate() {
            let offset = (m as f64 - (TP_PROTOTYPE / 2) as f64) / TP_PHASES as f64;
            let sinc = if offset.abs() < 1e-12 {
                1.0
            } else {
                let x = core::f64::consts::PI * offset;
                x.sin() / x
            };
            let phase = core::f64::consts::TAU * m as f64 / (TP_PROTOTYPE - 1) as f64;
            let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            *tap = sinc * window;
        }

        let mut coeffs = [[0.0 as Real; TP_TAPS]; TP_PHASES];
        for (p, phase) in coeffs.iter_mut().enumerate() {
            let mut sum = 0.0f64;
            for t in 0..TP_TAPS {
                sum += prototype[p + t * TP_PHASES];
            }
            let scale = if sum.abs() > 1e-12 { 1.0 / sum } else { 1.0 };
            for (t, tap) in phase.iter_mut().enumerate() {
                *tap = (prototype[p + t * TP_PHASES] * scale) as Real;
            }
        }

        Self {
            coeffs,
            history: vec![0.0 as Real; channels * TP_TAPS],
            write: 0,
            channels,
            peak: 0.0,
        }
    }

    /// Forget every sample seen so far.
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.write = 0;
        self.peak = 0.0;
    }

    /// Feed interleaved samples. `channels` must be the count given to [`new`](Self::new).
    pub fn push(&mut self, interleaved: &[Real]) {
        if self.channels == 0 {
            return;
        }
        for frame in interleaved.chunks_exact(self.channels) {
            self.write = (self.write + TP_TAPS - 1) % TP_TAPS;
            for (channel, &sample) in frame.iter().enumerate() {
                self.history[channel * TP_TAPS + self.write] = sample;
            }
            for channel in 0..self.channels {
                let base = channel * TP_TAPS;
                for phase in &self.coeffs {
                    let mut acc = 0.0 as Real;
                    for (t, tap) in phase.iter().enumerate() {
                        acc += *tap * self.history[base + (self.write + t) % TP_TAPS];
                    }
                    let magnitude = acc.abs();
                    if magnitude > self.peak {
                        self.peak = magnitude;
                    }
                }
            }
        }
    }

    /// The largest interpolated magnitude seen, in linear full-scale units.
    #[must_use]
    pub const fn peak(&self) -> Real {
        self.peak
    }

    /// The same, in dBTP. Silence reads [`SILENT_BAND_DB`].
    #[must_use]
    pub fn peak_dbtp(&self) -> Real {
        if self.peak > 0.0 {
            (20.0 * f64::from(self.peak).log10()) as Real
        } else {
            SILENT_BAND_DB
        }
    }
}

impl core::fmt::Debug for TruePeakMeter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TruePeakMeter")
            .field("channels", &self.channels)
            .field("peak", &self.peak)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// The three together
// ---------------------------------------------------------------------------------------------

/// How many frames [`ProgramMeter`] downmixes at a time. Any value works; this one keeps the
/// scratch buffer small enough to stay in cache.
const DOWNMIX_CHUNK: usize = 2048;

/// Everything [`ProgramMeter`] measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    /// Third-octave levels in dB relative to full scale, on [`THIRD_OCTAVE_CENTRES`].
    pub ltas_db: [Real; NUM_THIRD_OCTAVES],
    /// BS.1770 integrated loudness, or `None` when nothing passed the gates.
    pub integrated_lufs: Option<Real>,
    /// 4×-interpolated peak, in dBTP.
    pub true_peak_dbtp: Real,
    /// Gating blocks that did not fit the meter's history. Non-zero invalidates the loudness.
    pub dropped_blocks: usize,
}

impl Measurement {
    /// Peak-to-loudness ratio: how much headroom the programme keeps above its own loudness, and
    /// therefore how hard it has been compressed. `None` when there is no loudness to subtract.
    #[must_use]
    pub fn plr_db(&self) -> Option<Real> {
        self.integrated_lufs.map(|lufs| self.true_peak_dbtp - lufs)
    }
}

/// [`Ltas`], [`LoudnessMeter`] and [`TruePeakMeter`] over one interleaved stream.
///
/// The spectrum is measured on the mono downmix — the *mean* of the channels, not the sum, so that
/// the level does not depend on the channel count.
pub struct ProgramMeter {
    ltas: Ltas,
    loudness: LoudnessMeter,
    peak: TruePeakMeter,
    channels: usize,
    mono: Vec<Real>,
}

impl ProgramMeter {
    /// This allocates. Nothing after it does. See [`LoudnessMeter::new`] for `capacity_seconds`.
    #[must_use]
    pub fn new(sample_rate: Real, channels: usize, capacity_seconds: Real) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);
        Self {
            ltas: Ltas::new(sample_rate),
            loudness: LoudnessMeter::new(sample_rate, channels, capacity_seconds),
            peak: TruePeakMeter::new(channels),
            channels,
            mono: vec![0.0 as Real; DOWNMIX_CHUNK],
        }
    }

    /// Forget everything measured so far.
    pub fn reset(&mut self) {
        self.ltas.reset();
        self.loudness.reset();
        self.peak.reset();
    }

    /// Feed one interleaved block.
    pub fn push(&mut self, interleaved: &[Real]) {
        if self.channels == 0 {
            return;
        }
        self.loudness.push(interleaved);
        self.peak.push(interleaved);

        let scale = 1.0 / self.channels as Real;
        for chunk in interleaved.chunks(DOWNMIX_CHUNK * self.channels) {
            let frames = chunk.len() / self.channels;
            for (frame, dst) in chunk
                .chunks_exact(self.channels)
                .zip(self.mono.iter_mut().take(frames))
            {
                *dst = frame.iter().sum::<Real>() * scale;
            }
            self.ltas.push(&self.mono[..frames]);
        }
    }

    /// What has been measured.
    #[must_use]
    pub fn measurement(&self) -> Measurement {
        let mut ltas_db = [SILENT_BAND_DB; NUM_THIRD_OCTAVES];
        self.ltas.levels_db(&mut ltas_db);
        Measurement {
            ltas_db,
            integrated_lufs: self.loudness.integrated_lufs(),
            true_peak_dbtp: self.peak.peak_dbtp(),
            dropped_blocks: self.loudness.dropped_blocks(),
        }
    }

    /// The spectrum analyser, for [`Ltas::analyses`].
    #[must_use]
    pub const fn ltas(&self) -> &Ltas {
        &self.ltas
    }
}

impl core::fmt::Debug for ProgramMeter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProgramMeter")
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Comparing two spectra
// ---------------------------------------------------------------------------------------------

/// The band indices whose centres lie in `lo_hz..=hi_hz`.
///
/// Returns an empty range when nothing does.
#[must_use]
pub fn band_range(lo_hz: Real, hi_hz: Real) -> core::ops::Range<usize> {
    let mut start = NUM_THIRD_OCTAVES;
    let mut end = 0;
    for (i, centre) in THIRD_OCTAVE_CENTRES.iter().enumerate() {
        if *centre >= lo_hz && *centre <= hi_hz {
            start = start.min(i);
            end = end.max(i + 1);
        }
    }
    if start >= end { 0..0 } else { start..end }
}

/// Subtract a curve's own mean over `range`, leaving shape without level.
///
/// Every comparison in this module is between shapes, because absolute level is the one thing a
/// preset, a gain stage and a normaliser all change and none of them is what "voicing" means.
pub fn level_normalise(
    levels: &[Real; NUM_THIRD_OCTAVES],
    range: core::ops::Range<usize>,
    out: &mut [Real; NUM_THIRD_OCTAVES],
) {
    *out = *levels;
    let Some(slice) = levels.get(range.clone()) else {
        return;
    };
    if slice.is_empty() {
        return;
    }
    let mean = slice.iter().map(|v| f64::from(*v)).sum::<f64>() / slice.len() as f64;
    for value in out.iter_mut() {
        *value -= mean as Real;
    }
}

/// RMS difference in dB between two curves over `range`.
///
/// Both are level-normalised first, so this is a distance between shapes. Zero means the two
/// curves have the same tonal balance at whatever level each happens to sit.
#[must_use]
pub fn shape_distance_db(
    a: &[Real; NUM_THIRD_OCTAVES],
    b: &[Real; NUM_THIRD_OCTAVES],
    range: core::ops::Range<usize>,
) -> Real {
    let mut a_shape = [0.0 as Real; NUM_THIRD_OCTAVES];
    let mut b_shape = [0.0 as Real; NUM_THIRD_OCTAVES];
    level_normalise(a, range.clone(), &mut a_shape);
    level_normalise(b, range.clone(), &mut b_shape);

    let Some(indices) = a_shape.get(range.clone()) else {
        return 0.0;
    };
    if indices.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for i in range.clone() {
        let difference = f64::from(a_shape[i]) - f64::from(b_shape[i]);
        sum += difference * difference;
    }
    (sum / range.len() as f64).sqrt() as Real
}

/// The power-weighted mean frequency of a third-octave curve, in Hz.
///
/// Reported, never asserted on: it collapses a whole spectrum to one number and two very different
/// curves can share it. It is useful only as a direction — "this preset moved the weight upward".
#[must_use]
pub fn spectral_centroid_hz(levels_db: &[Real; NUM_THIRD_OCTAVES]) -> Real {
    let mut weighted = 0.0f64;
    let mut total = 0.0f64;
    for (level, centre) in levels_db.iter().zip(THIRD_OCTAVE_CENTRES.iter()) {
        let power = 10.0f64.powf(f64::from(*level) / 10.0);
        weighted += power * f64::from(*centre);
        total += power;
    }
    if total > 0.0 {
        (weighted / total) as Real
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: Real = 48_000.0;

    fn sine(freq: Real, amp: Real, phase: Real, n: usize) -> Vec<Real> {
        (0..n)
            .map(|i| {
                let t = core::f64::consts::TAU * f64::from(freq) * i as f64 / f64::from(RATE);
                (f64::from(amp) * (t + f64::from(phase)).sin()) as Real
            })
            .collect()
    }

    /// A deterministic 32-bit xorshift, so a test that fails fails the same way twice.
    struct Rng(u32);
    impl Rng {
        fn next_f32(&mut self) -> Real {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 as f64 / f64::from(u32::MAX) * 2.0 - 1.0) as Real
        }
    }

    // ---- K-weighting ----

    /// The one hard anchor available offline: BS.1770-4 prints its filter as a coefficient table
    /// for 48 kHz, and the analogue prototype in this module has to reproduce it.
    ///
    /// If this fails, the loudness numbers are not BS.1770's, whatever else is right.
    #[test]
    fn k_weighting_reproduces_the_published_48_khz_table() {
        let shelf = high_shelf(SHELF_HZ, SHELF_Q, SHELF_GAIN_DB, 48_000.0);
        // BS.1770-4 Table 1.
        let expected = [
            1.535_124_9,
            -2.691_696_2,
            1.198_392_8,
            -1.690_659_3,
            0.732_480_8,
        ];
        let got = [shelf.b0, shelf.b1, shelf.b2, shelf.a1, shelf.a2];
        for (got, expected) in got.iter().zip(expected.iter()) {
            assert!(
                (got - expected).abs() < 1e-5,
                "shelf {got:?} != published {expected:?}"
            );
        }

        let hp = highpass(HIGHPASS_HZ, HIGHPASS_Q, 48_000.0);
        // BS.1770-4 Table 2.
        let expected = [1.0, -2.0, 1.0, -1.990_047_5, 0.990_072_3];
        let got = [hp.b0, hp.b1, hp.b2, hp.a1, hp.a2];
        for (got, expected) in got.iter().zip(expected.iter()) {
            assert!(
                (got - expected).abs() < 1e-5,
                "highpass {got:?} != published {expected:?}"
            );
        }
    }

    /// The shape the recommendation describes in words, independent of the table above: flat and
    /// low below the RLB corner, roughly unity through the midrange, and a +4 dB plateau on top.
    #[test]
    fn k_weighting_has_the_response_the_recommendation_describes() {
        fn gain_db(freq: Real) -> Real {
            let mut shelf = Section::new();
            let mut hp = Section::new();
            shelf.coeffs = high_shelf(SHELF_HZ, SHELF_Q, SHELF_GAIN_DB, f64::from(RATE));
            hp.coeffs = highpass(HIGHPASS_HZ, HIGHPASS_Q, f64::from(RATE));
            // Long enough that the transient has gone and a whole number of cycles is measured.
            let n = 48_000;
            let input = sine(freq, 1.0, 0.0, n);
            let mut sum = 0.0f64;
            for (i, x) in input.iter().enumerate() {
                let y = hp.tick_general(0, shelf.tick_general(0, *x));
                if i >= n / 2 {
                    sum += f64::from(y) * f64::from(y);
                }
            }
            let rms = (sum / (n / 2) as f64).sqrt();
            // The input's RMS is 1/sqrt(2).
            (20.0 * (rms * core::f64::consts::SQRT_2).log10()) as Real
        }

        assert!(gain_db(20.0) < -10.0, "20 Hz: {}", gain_db(20.0));
        assert!(
            (-0.5..=1.5).contains(&gain_db(1000.0)),
            "1 kHz: {}",
            gain_db(1000.0)
        );
        let top = gain_db(10_000.0);
        assert!((3.5..=4.5).contains(&top), "10 kHz: {top}");
    }

    // ---- Loudness ----

    #[test]
    fn silence_has_no_loudness() {
        let mut meter = LoudnessMeter::new(RATE, 2, 10.0);
        meter.push(&vec![0.0 as Real; 2 * 48_000]);
        assert_eq!(meter.integrated_lufs(), None);
    }

    #[test]
    fn material_shorter_than_one_gating_block_has_no_loudness() {
        let mut meter = LoudnessMeter::new(RATE, 1, 10.0);
        // 300 ms: three 100 ms steps, one short of a 400 ms block.
        meter.push(&sine(1000.0, 0.5, 0.0, 14_400));
        assert_eq!(meter.integrated_lufs(), None);
    }

    #[test]
    fn doubling_the_amplitude_adds_six_decibels() {
        let mut quiet = LoudnessMeter::new(RATE, 2, 10.0);
        let mut loud = LoudnessMeter::new(RATE, 2, 10.0);
        let mono = sine(1000.0, 0.1, 0.0, 96_000);
        let stereo: Vec<Real> = mono.iter().flat_map(|s| [*s, *s]).collect();
        let doubled: Vec<Real> = stereo.iter().map(|s| s * 2.0).collect();
        quiet.push(&stereo);
        loud.push(&doubled);
        let difference =
            loud.integrated_lufs().expect("loud") - quiet.integrated_lufs().expect("quiet");
        assert!(
            (difference - 6.0206).abs() < 0.01,
            "doubling gave {difference} dB"
        );
    }

    /// BS.1770 sums channels rather than averaging them, so the same signal in both channels of a
    /// stereo programme is 3 dB louder than in one channel of a mono one. A meter that averaged
    /// would report the same number twice and nobody would notice until it disagreed with every
    /// other meter.
    #[test]
    fn a_second_channel_adds_three_decibels() {
        let mono_signal = sine(1000.0, 0.25, 0.0, 96_000);
        let mut mono = LoudnessMeter::new(RATE, 1, 10.0);
        mono.push(&mono_signal);

        let mut stereo = LoudnessMeter::new(RATE, 2, 10.0);
        let doubled: Vec<Real> = mono_signal.iter().flat_map(|s| [*s, *s]).collect();
        stereo.push(&doubled);

        let difference =
            stereo.integrated_lufs().expect("stereo") - mono.integrated_lufs().expect("mono");
        assert!(
            (difference - 3.0103).abs() < 0.01,
            "the second channel gave {difference} dB"
        );
    }

    /// Without the relative gate a long quiet tail drags the reading down. This is the check that
    /// the gate is actually wired in, not merely written.
    #[test]
    fn the_relative_gate_keeps_a_quiet_tail_out_of_the_average() {
        let loud = sine(1000.0, 0.5, 0.0, 96_000);
        // 30 dB down: below the −10 LU relative gate, well above the −70 LUFS absolute one.
        let quiet: Vec<Real> = sine(1000.0, 0.5 * 0.0316, 0.0, 480_000);

        let mut without_tail = LoudnessMeter::new(RATE, 1, 60.0);
        without_tail.push(&loud);

        let mut with_tail = LoudnessMeter::new(RATE, 1, 60.0);
        with_tail.push(&loud);
        with_tail.push(&quiet);

        let a = without_tail.integrated_lufs().expect("loud alone");
        let b = with_tail.integrated_lufs().expect("loud plus tail");
        assert!(
            (a - b).abs() < 0.5,
            "a ten-second tail 30 dB down moved the reading from {a} to {b}"
        );
    }

    #[test]
    fn a_reading_that_overran_its_history_says_so() {
        let mut meter = LoudnessMeter::new(RATE, 1, 1.0);
        meter.push(&sine(1000.0, 0.5, 0.0, 48_000 * 5));
        assert!(meter.dropped_blocks() > 0);
    }

    // ---- True peak ----

    /// Unity gain at DC in every phase, checked on the coefficients rather than on a signal.
    ///
    /// It cannot be checked on a signal: a sinc interpolator fed a step from silence rings, so a
    /// DC block pushed into a fresh meter legitimately reads about 11 % high on its first few
    /// samples. That overshoot is the interpolator being right about a discontinuity, not the
    /// meter being wrong — but it means the only clean statement of "passes DC" is this one.
    #[test]
    fn every_phase_passes_direct_current_unchanged() {
        let meter = TruePeakMeter::new(1);
        for (p, phase) in meter.coeffs.iter().enumerate() {
            let sum: f64 = phase.iter().map(|t| f64::from(*t)).sum();
            assert!((sum - 1.0).abs() < 1e-6, "phase {p} sums to {sum}");
        }
    }

    /// Phase 0 has to be the identity, or the meter could read below the sample peak.
    #[test]
    fn the_first_phase_is_the_identity() {
        let meter = TruePeakMeter::new(1);
        for (t, tap) in meter.coeffs[0].iter().enumerate() {
            let expected = if t == 6 { 1.0 } else { 0.0 };
            assert!(
                (f64::from(*tap) - expected).abs() < 1e-6,
                "phase 0 tap {t} is {tap}, expected {expected}"
            );
        }
    }

    /// The case the whole meter exists for: a 12 kHz sine sampled at its half-way points never
    /// shows more than 0.707 in any sample, and reconstructs to 1.0.
    #[test]
    fn an_inter_sample_peak_is_found() {
        let quarter = (core::f64::consts::FRAC_PI_4) as Real;
        let samples = sine(12_000.0, 1.0, quarter, 4096);
        let sample_peak = samples.iter().fold(0.0 as Real, |a, s| a.max(s.abs()));
        assert!(
            (sample_peak - core::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3,
            "the test signal is wrong: sample peak {sample_peak}"
        );

        let mut meter = TruePeakMeter::new(1);
        meter.push(&samples);
        assert!(
            meter.peak() > 0.97,
            "true peak read {} where the waveform reaches 1.0",
            meter.peak()
        );
        assert!(meter.peak() < 1.05, "true peak overshot: {}", meter.peak());
    }

    /// Phase 0 is the identity by construction, so the meter can never read below the sample peak.
    #[test]
    fn the_true_peak_is_never_below_the_sample_peak() {
        let mut rng = Rng(0x2b7e_1516);
        let samples: Vec<Real> = (0..8192).map(|_| rng.next_f32() * 0.8).collect();
        let sample_peak = samples.iter().fold(0.0 as Real, |a, s| a.max(s.abs()));
        let mut meter = TruePeakMeter::new(1);
        meter.push(&samples);
        assert!(
            meter.peak() >= sample_peak - 1e-4,
            "true peak {} below sample peak {sample_peak}",
            meter.peak()
        );
    }

    #[test]
    fn channels_are_measured_independently() {
        // Left silent, right a 100 Hz sine at 0.6 — low enough that interpolation has nothing to
        // find between the samples, so the reading is the amplitude and nothing else.
        let right = sine(100.0, 0.6, 0.0, 4096);
        let frames: Vec<Real> = right.iter().flat_map(|s| [0.0 as Real, *s]).collect();
        let mut meter = TruePeakMeter::new(2);
        meter.push(&frames);
        assert!((meter.peak() - 0.6).abs() < 2e-3, "read {}", meter.peak());
    }

    // ---- LTAS ----

    #[test]
    fn a_full_scale_sine_reads_its_own_mean_square() {
        let mut ltas = Ltas::new(RATE);
        ltas.push(&sine(1000.0, 1.0, 0.0, 48_000));
        let mut levels = [0.0 as Real; NUM_THIRD_OCTAVES];
        assert!(ltas.levels_db(&mut levels));

        let band = THIRD_OCTAVE_CENTRES
            .iter()
            .position(|c| (*c - 1000.0).abs() < 0.5)
            .expect("a 1 kHz band");
        assert!(
            (levels[band] + 3.01).abs() < 0.3,
            "1 kHz band read {} dB, expected -3.01",
            levels[band]
        );
        // Two bands away there should be nothing but window leakage.
        assert!(
            levels[band - 2] < -50.0,
            "leakage two bands down: {}",
            levels[band - 2]
        );
    }

    #[test]
    fn white_noise_rises_one_decibel_a_band() {
        let mut rng = Rng(0x9e37_79b9);
        let mut ltas = Ltas::new(RATE);
        let samples: Vec<Real> = (0..480_000).map(|_| rng.next_f32() * 0.3).collect();
        ltas.push(&samples);
        let mut levels = [0.0 as Real; NUM_THIRD_OCTAVES];
        assert!(ltas.levels_db(&mut levels));

        // Compare across a decade in the middle, where the bands are many bins wide and the
        // estimate is stable: 200 Hz to 2 kHz is ten third-octaves, so ten decibels.
        let lo = band_range(200.0, 200.0).start;
        let hi = band_range(2000.0, 2000.0).start;
        let rise = levels[hi] - levels[lo];
        assert!(
            (rise - 10.0).abs() < 1.0,
            "white noise rose {rise} dB over a decade, expected 10"
        );
    }

    #[test]
    fn nothing_analysed_reports_nothing() {
        let ltas = Ltas::new(RATE);
        let mut levels = [0.0 as Real; NUM_THIRD_OCTAVES];
        assert!(!ltas.levels_db(&mut levels));
        assert!(levels.iter().all(|v| *v == SILENT_BAND_DB));
    }

    // ---- Shape comparison ----

    #[test]
    fn shape_distance_ignores_level() {
        let mut a = [0.0 as Real; NUM_THIRD_OCTAVES];
        for (i, value) in a.iter_mut().enumerate() {
            *value = i as Real * 0.5;
        }
        let b: [Real; NUM_THIRD_OCTAVES] = core::array::from_fn(|i| a[i] + 17.0);
        let range = band_range(40.0, 16_000.0);
        assert!(shape_distance_db(&a, &b, range.clone()) < 1e-3);

        // A real difference is not ignored: tilt one curve by 1 dB per band against the other.
        let c: [Real; NUM_THIRD_OCTAVES] = core::array::from_fn(|i| a[i] + i as Real);
        assert!(shape_distance_db(&a, &c, range) > 1.0);
    }

    #[test]
    fn the_band_range_covers_what_it_says() {
        let range = band_range(40.0, 16_000.0);
        assert_eq!(THIRD_OCTAVE_CENTRES[range.start], 40.0);
        assert_eq!(THIRD_OCTAVE_CENTRES[range.end - 1], 16_000.0);
        assert!(band_range(21_000.0, 22_000.0).is_empty());
    }

    #[test]
    fn the_centroid_follows_the_energy() {
        let mut low = [SILENT_BAND_DB; NUM_THIRD_OCTAVES];
        low[2] = 0.0;
        let mut high = [SILENT_BAND_DB; NUM_THIRD_OCTAVES];
        high[26] = 0.0;
        assert!(spectral_centroid_hz(&low) < spectral_centroid_hz(&high));
        assert!((spectral_centroid_hz(&low) - THIRD_OCTAVE_CENTRES[2]).abs() < 1.0);
    }

    // ---- The three together ----

    #[test]
    fn the_program_meter_agrees_with_its_parts() {
        let mono = sine(1000.0, 0.5, 0.0, 96_000);
        let stereo: Vec<Real> = mono.iter().flat_map(|s| [*s, *s]).collect();

        let mut program = ProgramMeter::new(RATE, 2, 10.0);
        program.push(&stereo);
        let measured = program.measurement();

        let mut loudness = LoudnessMeter::new(RATE, 2, 10.0);
        loudness.push(&stereo);
        assert_eq!(measured.integrated_lufs, loudness.integrated_lufs());

        let mut peak = TruePeakMeter::new(2);
        peak.push(&stereo);
        assert!((measured.true_peak_dbtp - peak.peak_dbtp()).abs() < 1e-4);

        let plr = measured.plr_db().expect("a PLR");
        assert!(plr.is_finite());
        assert_eq!(measured.dropped_blocks, 0);
    }
}
