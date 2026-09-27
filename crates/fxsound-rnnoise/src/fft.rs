// fxsound: this module is not upstream's. Upstream transformed through `easyfft`'s
// `real_fft_using` and `real_ifft_using`, which keep the planner, its plans and their scratch and
// input-copy buffers in a cache private to the calling thread. The first frame analysed on a
// thread therefore planned the 960-point transforms and allocated those buffers there, and in
// FxSound that thread is PipeWire's real-time data thread: switching the denoiser on for the first
// time in a session cost the next audio callback a planner, two plans, their twiddle tables and a
// round of `malloc`, any of which can wait on a lock the GUI thread holds. It happened again on
// every other data thread the stream later ran on, since the cache is per thread.
//
// So each `DenoiseFeatures` and each `Stft` plans its own pair of transforms and owns every buffer
// they use, in its constructor, on whichever thread builds it; the audio path only ever runs
// them. The arithmetic does not change: `easyfft` is a thin wrapper around this same planner, and
// a test below holds both directions to its output bit for bit.

//! The library's 960-point real FFT pair, planned and sized at construction.

use crate::{Complex, FREQ_SIZE, WINDOW_SIZE};
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::ops::{Deref, MulAssign};
use std::sync::Arc;

/// The transform of one 960-sample window: [`FREQ_SIZE`] bins from DC to Nyquist, the half of
/// the spectrum a real signal needs.
///
/// Stands in for the `easyfft::DynRealDft` upstream stored its transforms in, with the same
/// methods under the same names, so that the code built on it reads as upstream's does. Sized
/// once, in [`Spectrum::new`]; nothing here allocates afterwards.
#[derive(Clone, Debug)]
pub struct Spectrum {
    bins: Box<[Complex; FREQ_SIZE]>,
}

impl Default for Spectrum {
    fn default() -> Self {
        Self::new()
    }
}

impl Spectrum {
    /// A transform of silence.
    pub fn new() -> Self {
        Self {
            bins: Box::new([Complex::default(); FREQ_SIZE]),
        }
    }

    /// Every bin to zero, in place.
    pub fn clear(&mut self) {
        self.bins.fill(Complex::default());
    }

    /// The DC bin's real part.
    pub fn get_offset(&self) -> &f32 {
        &self.bins[0].re
    }

    /// The DC bin's real part, to change.
    pub fn get_offset_mut(&mut self) -> &mut f32 {
        &mut self.bins[0].re
    }

    /// Bins `1..479`: what `DynRealDft::get_frequency_bins` returned for a 960-point transform,
    /// which leaves out bin 479 as well as the Nyquist bin. Upstream's pitch filter was written
    /// against exactly this range, and keeping it keeps that filter's arithmetic upstream's.
    pub fn get_frequency_bins(&self) -> &[Complex] {
        &self.bins[1..(WINDOW_SIZE - 1) / 2]
    }

    /// [`Spectrum::get_frequency_bins`], to change.
    pub fn get_frequency_bins_mut(&mut self) -> &mut [Complex] {
        &mut self.bins[1..(WINDOW_SIZE - 1) / 2]
    }
}

impl Deref for Spectrum {
    type Target = [Complex];

    fn deref(&self) -> &[Complex] {
        &self.bins[..]
    }
}

impl MulAssign<f32> for Spectrum {
    fn mul_assign(&mut self, rhs: f32) {
        for bin in self.bins.iter_mut() {
            *bin *= rhs;
        }
    }
}

impl MulAssign<&[f32]> for Spectrum {
    /// Bin by bin. The gains are one per bin, [`FREQ_SIZE`] of them, as upstream's were.
    fn mul_assign(&mut self, rhs: &[f32]) {
        debug_assert_eq!(rhs.len(), FREQ_SIZE);
        for (bin, gain) in self.bins.iter_mut().zip(rhs) {
            *bin *= gain;
        }
    }
}

/// A forward and an inverse transform of [`WINDOW_SIZE`] samples, with every buffer they need.
///
/// `realfft` uses its input as working space, so both directions copy their input into a buffer
/// of their own first — which is what `easyfft` did too — and the caller's window and spectrum
/// come out as they went in.
#[derive(Clone)]
pub(crate) struct Fft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    spectrum: Vec<Complex>,
    scratch_forward: Vec<Complex>,
    scratch_inverse: Vec<Complex>,
}

impl Fft {
    /// Plans both transforms and allocates their buffers. Do this before real time starts.
    pub(crate) fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(WINDOW_SIZE);
        let inverse = planner.plan_fft_inverse(WINDOW_SIZE);
        Self {
            window: forward.make_input_vec(),
            spectrum: forward.make_output_vec(),
            scratch_forward: forward.make_scratch_vec(),
            scratch_inverse: inverse.make_scratch_vec(),
            forward,
            inverse,
        }
    }

    /// `output` = the unnormalised forward transform of `input`.
    pub(crate) fn forward(&mut self, input: &[f32; WINDOW_SIZE], output: &mut Spectrum) {
        self.window.copy_from_slice(input);
        // The lengths were fixed by the plan this struct was built with, so this cannot fail; if
        // it somehow did, a transform of silence is the answer that does no harm.
        if self
            .forward
            .process_with_scratch(
                &mut self.window,
                &mut output.bins[..],
                &mut self.scratch_forward,
            )
            .is_err()
        {
            output.clear();
        }
    }

    /// `output` = the unnormalised inverse transform of `input`.
    pub(crate) fn inverse(&mut self, input: &Spectrum, output: &mut [f32; WINDOW_SIZE]) {
        self.spectrum.copy_from_slice(&input.bins[..]);
        if self
            .inverse
            .process_with_scratch(&mut self.spectrum, output, &mut self.scratch_inverse)
            .is_err()
        {
            output.fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use easyfft::dyn_size::realfft::{DynRealFft, DynRealIfft};

    /// Deterministic noise with some structure in it, in the 16-bit range the library works in.
    fn window() -> [f32; WINDOW_SIZE] {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        std::array::from_fn(|n| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let hiss = ((state >> 40) as f32 / 8_388_608.0 - 1.0) * 3_000.0;
            hiss + (n as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 9_000.0
        })
    }

    #[test]
    fn the_forward_transform_is_easyffts_bit_for_bit() {
        // `easyfft`'s allocating `real_fft` plans through the same planner as `real_fft_using`
        // did, so this is the transform upstream computed.
        let input = window();
        let want = input.real_fft();
        let mut fft = Fft::new();
        let mut got = Spectrum::new();
        fft.forward(&input, &mut got);
        assert_eq!(want.len(), got.len());
        for (k, (w, g)) in want.iter().zip(got.iter()).enumerate() {
            assert_eq!(
                (w.re.to_bits(), w.im.to_bits()),
                (g.re.to_bits(), g.im.to_bits()),
                "bin {k}: {w} against {g}"
            );
        }
        assert_eq!(input, window(), "the caller's window came back changed");
    }

    #[test]
    fn the_inverse_transform_is_easyffts_bit_for_bit() {
        let input = window();
        let spectrum = input.real_fft();
        let want = spectrum.real_ifft();

        let mut fft = Fft::new();
        let mut transformed = Spectrum::new();
        fft.forward(&input, &mut transformed);
        let before = transformed.clone();
        let mut got = [0.0; WINDOW_SIZE];
        fft.inverse(&transformed, &mut got);
        for (n, (w, g)) in want.iter().zip(&got).enumerate() {
            assert_eq!(w.to_bits(), g.to_bits(), "sample {n}: {w} against {g}");
        }
        assert!(
            before.iter().zip(transformed.iter()).all(|(a, b)| a == b),
            "the inverse transform used the caller's spectrum as working space"
        );
    }

    #[test]
    fn the_frequency_bins_are_the_range_upstreams_pitch_filter_was_written_against() {
        let dft = window().real_fft();
        let mut ours = Spectrum::new();
        Fft::new().forward(&window(), &mut ours);
        assert_eq!(
            dft.get_frequency_bins().len(),
            ours.get_frequency_bins().len()
        );
        assert_eq!(dft.get_frequency_bins(), ours.get_frequency_bins());
        assert_eq!(dft.get_offset(), ours.get_offset());
    }
}
