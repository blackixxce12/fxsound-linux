// fxsound: this module is not in the registry crate. It is the library's own transform pair —
// the high-pass filter, the Vorbis window, the 960-point real FFT and the overlap-add — with
// nothing between analysis and synthesis but a set of band gains handed in from outside. A
// linked-stereo denoiser runs one network on a downmix and applies its gains to each channel
// through one of these, so that the channels share a mask and keep their own signal.

//! Per-channel analysis and synthesis without the network.

use crate::features::{clear_dft, overlap_add, transform_input};
use crate::{Complex, FRAME_SIZE, FREQ_SIZE, NB_BANDS, WINDOW_SIZE};
use easyfft::dyn_size::realfft::DynRealDft;

/// The library's STFT and overlap-add on their own: [`push`](Stft::push) a frame, then
/// [`synthesise`](Stft::synthesise) it with a set of band gains — typically the ones a
/// [`DenoiseState`](crate::DenoiseState) returned from [`analyse`](crate::DenoiseState::analyse)
/// on a downmix of several channels.
///
/// The output is the frame *before* the one just pushed, exactly as `DenoiseState` delays its
/// output: the window spans two frames and the overlap-add completes the earlier one. With unity
/// gains the round trip is an identity (after the library's own high-pass filter and its zeroing
/// of the bins above 20 kHz). What this does *not* do is the pitch filter, which needs a pitch
/// search of its own; a channel synthesised this way gets the mask's suppression and not the
/// comb between the harmonics.
///
/// Sized at construction; nothing here allocates afterwards.
#[derive(Clone)]
pub struct Stft {
    /// The previous frame and the current one, high-pass filtered: the analysis window.
    input_mem: [f32; WINDOW_SIZE],
    mem_hp_x: [f32; 2],
    synthesis_mem: [f32; FRAME_SIZE],
    window_buf: [f32; WINDOW_SIZE],
    x: DynRealDft<f32>,
    ex: [f32; NB_BANDS],
}

impl Default for Stft {
    fn default() -> Self {
        Self::new()
    }
}

impl Stft {
    /// Allocates the transform. Do this before real time starts.
    pub fn new() -> Self {
        Self {
            input_mem: [0.0; WINDOW_SIZE],
            mem_hp_x: [0.0; 2],
            synthesis_mem: [0.0; FRAME_SIZE],
            window_buf: [0.0; WINDOW_SIZE],
            x: DynRealDft::new(0.0, &[Complex::default(); FREQ_SIZE - 1], WINDOW_SIZE),
            ex: [0.0; NB_BANDS],
        }
    }

    /// Forgets the input history, the filter state and the overlap-add tail, in place.
    pub fn reset(&mut self) {
        self.input_mem.fill(0.0);
        self.mem_hp_x = [0.0; 2];
        self.synthesis_mem.fill(0.0);
        self.window_buf.fill(0.0);
        clear_dft(&mut self.x);
        self.ex.fill(0.0);
    }

    /// Shifts one frame of [`FRAME_SIZE`] samples — in the 16-bit range the library works in —
    /// into the window through the library's high-pass filter, and transforms the window.
    pub fn push(&mut self, input: &[f32]) {
        assert!(input.len() == FRAME_SIZE);
        let new_idx = self.input_mem.len() - FRAME_SIZE;
        self.input_mem.copy_within(FRAME_SIZE.., 0);
        crate::util::BIQUAD_HP.filter(&mut self.input_mem[new_idx..], &mut self.mem_hp_x, input);
        transform_input(
            &self.input_mem,
            0,
            &mut self.window_buf,
            &mut self.x,
            &mut self.ex,
        );
    }

    /// Applies `gains` to the transform of the last pushed window and overlap-adds the result
    /// into `output`, which receives [`FRAME_SIZE`] samples. Call it once per
    /// [`push`](Stft::push).
    pub fn synthesise(&mut self, gains: &[f32; NB_BANDS], output: &mut [f32]) {
        let mut gf = [1.0; FREQ_SIZE];
        crate::interp_band_gain(&mut gf[..], &gains[..]);
        self.x *= &gf[..];
        overlap_add(
            &self.x,
            &mut self.window_buf,
            &mut self.synthesis_mem,
            output,
        );
    }

    /// The band energies of the last pushed window, as the library computes them for its own
    /// features.
    pub fn band_energies(&self) -> &[f32; NB_BANDS] {
        &self.ex
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three tones well above the library's high-pass corner and below the 20 kHz it zeroes.
    fn tones(frames: usize) -> Vec<f32> {
        (0..frames * FRAME_SIZE)
            .map(|n| {
                let t = n as f32 / 48_000.0;
                ((t * std::f32::consts::TAU * 1_000.0).sin()
                    + (t * std::f32::consts::TAU * 3_100.0).sin() * 0.5
                    + (t * std::f32::consts::TAU * 9_000.0).sin() * 0.25)
                    * 8_000.0
            })
            .collect()
    }

    #[test]
    fn with_unity_gains_it_is_an_identity_delayed_by_one_frame() {
        // The transform pair is an identity; what surrounds it is the library's own input
        // high-pass filter, which shifts a 1 kHz tone by about two degrees — three percent of its
        // amplitude, sample for sample — so the reference is the input through that same filter,
        // and the window-overlap-add round trip has to reproduce it to float precision.
        let input = tones(12);
        let mut filtered = vec![0.0; input.len()];
        crate::util::BIQUAD_HP.filter(&mut filtered, &mut [0.0; 2], &input);

        let mut stft = Stft::new();
        let mut out = [0.0; FRAME_SIZE];
        let unity = [1.0; NB_BANDS];
        for (n, frame) in input.chunks_exact(FRAME_SIZE).enumerate() {
            stft.push(frame);
            stft.synthesise(&unity, &mut out);
            // The first output completes nothing, because the window spans two frames; the
            // second carries the ringing of the signal's own onset — a step out of silence has
            // energy above the 20 kHz the library zeroes — and settles to within 5e-6 by the
            // third. What is measured from there is the transform pair.
            if n < 2 {
                continue;
            }
            let want = &filtered[(n - 1) * FRAME_SIZE..n * FRAME_SIZE];
            for (i, (got, want)) in out.iter().zip(want).enumerate() {
                assert!(
                    (got - want).abs() < 8_000.0 * 1.0e-4,
                    "frame {n} sample {i}: {got} against {want}"
                );
            }
        }
    }

    #[test]
    fn a_zero_gain_takes_the_band_out() {
        // Gains are gains: a band at zero silences what is in it. The second half of the check
        // is that a band left at unity is left alone, which is what makes this a mask.
        let input = tones(12);
        let mut stft = Stft::new();
        let mut out = [0.0; FRAME_SIZE];
        // Band 13 spans 4–4.8 kHz and 16 spans 6.8–8 kHz; the 9 kHz tone sits in band 17–18.
        let mut gains = [1.0; NB_BANDS];
        gains[17] = 0.0;
        gains[18] = 0.0;
        gains[19] = 0.0;
        let energy_at = |hz: f32, samples: &[f32]| {
            let (mut re, mut im) = (0.0_f32, 0.0_f32);
            for (n, &x) in samples.iter().enumerate() {
                let phase = n as f32 * std::f32::consts::TAU * hz / 48_000.0;
                re += x * phase.cos();
                im -= x * phase.sin();
            }
            (re * re + im * im).sqrt() * 2.0 / samples.len() as f32
        };
        let mut collected = Vec::new();
        for (n, frame) in input.chunks_exact(FRAME_SIZE).enumerate() {
            stft.push(frame);
            stft.synthesise(&gains, &mut out);
            if n >= 2 {
                collected.extend_from_slice(&out);
            }
        }
        let low = energy_at(1_000.0, &collected);
        let high = energy_at(9_000.0, &collected);
        assert!(
            (low - 8_000.0).abs() < 8_000.0 * 0.02,
            "1 kHz came back at {low}"
        );
        assert!(
            high < 2_000.0 * 0.05,
            "9 kHz survived a zero gain at {high}"
        );
    }

    #[test]
    fn a_reset_transform_starts_from_silence() {
        let input = tones(6);
        let mut stft = Stft::new();
        let mut out = [0.0; FRAME_SIZE];
        for frame in input.chunks_exact(FRAME_SIZE) {
            stft.push(frame);
            stft.synthesise(&[1.0; NB_BANDS], &mut out);
        }
        stft.reset();
        stft.push(&[0.0; FRAME_SIZE]);
        stft.synthesise(&[1.0; NB_BANDS], &mut out);
        assert!(out.iter().all(|x| *x == 0.0), "the tail survived the reset");
    }
}
