use std::borrow::Cow;

use crate::{FRAME_SIZE, FREQ_SIZE, NB_BANDS, RnnModel};

// fxsound: the result of `DenoiseState::analyse`. The registry crate kept the band gains as a
// stack local inside `process_frame`; a linked-stereo mask needs them, and a control surface
// needs to edit them before they are applied.
/// What one frame's analysis found: the network's opinion, before anything is applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Analysis {
    /// The network's voice probability, `0.0..=1.0`. Zero for a silent frame, because the network
    /// did not run.
    pub vad: f32,
    /// Whether the frame was quiet enough that the network was skipped and its state left alone.
    /// [`DenoiseState::synthesise`] then passes the frame through untouched, whatever gains it is
    /// handed, which is what `process_frame` has always done with silence.
    pub silence: bool,
    /// The band gains the library would apply: the network's output after the release floor
    /// `max(g, 0.6·previous)`. Unity for a silent frame. Edit them and hand them back to
    /// [`DenoiseState::synthesise`], or apply them to another channel through [`crate::Stft`].
    pub gains: [f32; NB_BANDS],
}

/// This is the low-level entry-point into `nnnoiseless`: by using the `DenoiseState` directly,
/// you can denoise your audio while keeping copying to a minimum.
///
/// This struct directly contains various memory buffers that are used while denoising. As such,
/// this is quite a large struct, and should probably be kept behind some kind of pointer.
///
/// # Example
///
/// ```rust
/// # use fxsound_rnnoise::DenoiseState;
/// // One second of 440Hz sine wave at 48kHz sample rate. Note that the input data consists of
/// // `f32`s, but the values should be in the range of an `i16`.
/// let sine: Vec<_> = (0..48_000)
///     .map(|x| (x as f32 * 440.0 * 2.0 * std::f32::consts::PI / 48_000.0).sin() * i16::MAX as f32)
///     .collect();
/// let mut output = Vec::new();
/// let mut out_buf = [0.0; DenoiseState::FRAME_SIZE];
/// let mut denoise = DenoiseState::new();
/// let mut first = true;
/// for chunk in sine.chunks_exact(DenoiseState::FRAME_SIZE) {
///     denoise.process_frame(&mut out_buf[..], chunk);
///
///     // We throw away the first output, as discussed in the documentation for
///     //`DenoiseState::process_frame`.
///     if !first {
///         output.extend_from_slice(&out_buf[..]);
///     }
///     first = false;
/// }
/// ```
#[derive(Clone)]
pub struct DenoiseState<'model> {
    /// Most recent gains that we applied.
    lastg: [f32; crate::NB_BANDS],
    rnn: crate::rnn::RnnState<'model>,
    feat: crate::features::DenoiseFeatures,
    // fxsound: whether the last `analyse` found silence, so that `synthesise` leaves such a frame
    // untouched exactly as the original `process_frame` did.
    silent: bool,
}

impl DenoiseState<'static> {
    /// A `DenoiseState` processes this many samples at a time.
    pub const FRAME_SIZE: usize = FRAME_SIZE;

    pub(crate) fn default() -> Self {
        DenoiseState::from_model_owned(Cow::Owned(RnnModel::default()))
    }

    /// Creates a new `DenoiseState`.
    pub fn new() -> Box<DenoiseState<'static>> {
        Box::new(Self::default())
    }

    /// Creates a new `DenoiseState` owning a custom model.
    ///
    /// The main difference between this method and `DenoiseState::with_model` is that here
    /// `DenoiseState` will own the model; this might be more convenient.
    pub fn from_model(model: RnnModel) -> Box<DenoiseState<'static>> {
        Box::new(DenoiseState::from_model_owned(Cow::Owned(model)))
    }
}

impl<'model> DenoiseState<'model> {
    /// Creates a new `DenoiseState` using a custom model.
    ///
    /// The main difference between this method and `DenoiseState::from_model` is that here
    /// `DenoiseState` will borrow the model; this might create some lifetime-related pain, but
    /// it means that the same model can be shared between multiple `DenoiseState`s.
    pub fn with_model(model: &'model RnnModel) -> Box<DenoiseState<'model>> {
        Box::new(DenoiseState::from_model_owned(Cow::Borrowed(model)))
    }

    pub(crate) fn from_model_owned(model: Cow<'model, RnnModel>) -> DenoiseState<'model> {
        DenoiseState {
            lastg: [0.0; NB_BANDS],
            rnn: crate::rnn::RnnState::new(model),
            feat: crate::features::DenoiseFeatures::new(),
            silent: false,
        }
    }

    // fxsound: an in-place reset. The registry crate had none, so starting afresh meant building
    // a new state — several heap allocations — which a real-time caller cannot afford.
    /// Forgets everything the state has heard: the GRU states, the feature history, the pitch
    /// memory, the overlap-add tail and the gain floor. Afterwards the next frame is processed
    /// exactly as a freshly built state would process it. Allocates nothing.
    pub fn reset(&mut self) {
        self.lastg = [0.0; NB_BANDS];
        self.rnn.reset();
        self.feat.reset();
        self.silent = false;
    }

    // fxsound: the first half of `process_frame`, returning what it found.
    /// Analyses one frame: shifts it into the input history through the high-pass filter,
    /// computes the features, runs the network and the pitch filter, and returns the voice
    /// probability and the band gains the library would apply.
    ///
    /// `input` has the length and the range [`DenoiseState::process_frame`] documents. The
    /// transform is left ready for [`DenoiseState::synthesise`], which must be called once
    /// before the next `analyse` if the frame's output is wanted.
    pub fn analyse(&mut self, input: &[f32]) -> Analysis {
        let mut g = [0.0; NB_BANDS];
        let mut vad_prob = [0.0];

        self.feat.shift_and_filter_input(input);
        let silence = self.feat.compute_frame_features();
        self.silent = silence;
        if silence {
            return Analysis {
                vad: 0.0,
                silence: true,
                gains: [1.0; NB_BANDS],
            };
        }

        self.rnn
            .compute(&mut g[..], &mut vad_prob[..], self.feat.features());
        self.feat.pitch_filter(&g);
        for (gain, last) in g.iter_mut().zip(self.lastg.iter_mut()) {
            *gain = gain.max(0.6 * *last);
            *last = *gain;
        }
        Analysis {
            vad: vad_prob[0],
            silence: false,
            gains: g,
        }
    }

    // fxsound: the second half of `process_frame`, taking the gains from outside.
    /// Applies `gains` — the ones [`DenoiseState::analyse`] returned, or an edited copy — to the
    /// frame that call analysed, and overlap-adds the result into `output`, which has the length
    /// and range [`DenoiseState::process_frame`] documents.
    ///
    /// A frame `analyse` found silent is passed through untouched, whatever `gains` says: the
    /// network did not run on it, and this is what `process_frame` has always done with silence.
    pub fn synthesise(&mut self, gains: &[f32; NB_BANDS], output: &mut [f32]) {
        if !self.silent {
            let mut gf = [1.0; FREQ_SIZE];
            crate::interp_band_gain(&mut gf[..], &gains[..]);
            self.feat.apply_gain(&gf);
        }
        self.feat.frame_synthesis(output);
    }

    /// Processes a chunk of samples.
    ///
    /// Both `output` and `input` should be slices of length `DenoiseState::FRAME_SIZE`, and they
    /// are assumed to be in 16-bit, 48kHz signed PCM format. Note that although the input and
    /// output are `f32`s, they are supposed to come from 16-bit integers. In particular, they
    /// should be in the range `[-32768.0, 32767.0]` instead of the range `[-1.0, 1.0]` which
    /// is more common for floating-point PCM.
    ///
    /// The current output of `process_frame` depends on the current input, but also on the
    /// preceding inputs. Because of this, you might prefer to discard the very first output; it
    /// will contain some fade-in artifacts.
    ///
    /// Exactly [`DenoiseState::analyse`] followed by [`DenoiseState::synthesise`] with the gains
    /// it returned; the return value is the voice probability.
    pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32 {
        let analysis = self.analyse(input);
        self.synthesise(&analysis.gains, output);
        analysis.vad
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stft;

    // fxsound: the registry crate asserted this with `static_assertions`, which is not vendored.
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn the_state_is_send_and_sync() {
        assert_send_sync::<DenoiseState<'static>>();
    }

    /// Deterministic noise in the 16-bit range the library wants, with a hum under it so that
    /// the network has something to remove and its gains are not all near unity.
    fn fixture(frames: usize) -> Vec<f32> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..frames * FRAME_SIZE)
            .map(|n| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let hiss = ((state >> 40) as f32 / 8_388_608.0 - 1.0) * 600.0;
                let hum = (n as f32 * std::f32::consts::TAU * 50.0 / 48_000.0).sin() * 1_500.0;
                let voice = (n as f32 * std::f32::consts::TAU * 180.0 / 48_000.0).sin() * 4_000.0
                    + (n as f32 * std::f32::consts::TAU * 540.0 / 48_000.0).sin() * 1_500.0;
                let syllable = ((n as f32 / 48_000.0) * 4.0).fract();
                let envelope = if syllable < 0.6 { 1.0 } else { 0.05 };
                hiss + hum + voice * envelope
            })
            .collect()
    }

    /// The registry crate's `process_frame`, verbatim, on the private fields. The refactor into
    /// `analyse` + `synthesise` is held to this, sample for sample.
    fn reference_process_frame(
        state: &mut DenoiseState<'_>,
        output: &mut [f32],
        input: &[f32],
    ) -> f32 {
        let mut g = [0.0; NB_BANDS];
        let mut gf = [1.0; FREQ_SIZE];
        let mut vad_prob = [0.0];

        state.feat.shift_and_filter_input(input);
        let silence = state.feat.compute_frame_features();
        if !silence {
            state
                .rnn
                .compute(&mut g[..], &mut vad_prob[..], state.feat.features());
            state.feat.pitch_filter(&g);
            for i in 0..NB_BANDS {
                g[i] = g[i].max(0.6 * state.lastg[i]);
                state.lastg[i] = g[i];
            }
            crate::interp_band_gain(&mut gf[..], &g[..]);
            state.feat.apply_gain(&gf);
        }

        state.feat.frame_synthesis(output);
        vad_prob[0]
    }

    #[test]
    fn process_frame_is_analyse_followed_by_synthesise_sample_for_sample() {
        let input = fixture(40);
        let mut reference = DenoiseState::new();
        let mut split = DenoiseState::new();
        let mut want = [0.0; FRAME_SIZE];
        let mut got = [0.0; FRAME_SIZE];
        for (n, frame) in input.chunks_exact(FRAME_SIZE).enumerate() {
            let vad_want = reference_process_frame(&mut reference, &mut want, frame);
            let vad_got = split.process_frame(&mut got, frame);
            assert_eq!(vad_want.to_bits(), vad_got.to_bits(), "frame {n}: vad");
            for (i, (w, g)) in want.iter().zip(&got).enumerate() {
                assert_eq!(
                    w.to_bits(),
                    g.to_bits(),
                    "frame {n} sample {i}: {w} against {g}"
                );
            }
        }
    }

    #[test]
    fn a_reset_state_computes_exactly_what_a_fresh_one_does() {
        let input = fixture(30);
        let mut used = DenoiseState::new();
        let mut scratch = [0.0; FRAME_SIZE];
        for frame in input.chunks_exact(FRAME_SIZE).take(20) {
            used.process_frame(&mut scratch, frame);
        }
        used.reset();

        let mut fresh = DenoiseState::new();
        let mut want = [0.0; FRAME_SIZE];
        let mut got = [0.0; FRAME_SIZE];
        for (n, frame) in input.chunks_exact(FRAME_SIZE).enumerate() {
            let vad_want = fresh.process_frame(&mut want, frame);
            let vad_got = used.process_frame(&mut got, frame);
            assert_eq!(vad_want.to_bits(), vad_got.to_bits(), "frame {n}: vad");
            for (i, (w, g)) in want.iter().zip(&got).enumerate() {
                assert_eq!(
                    w.to_bits(),
                    g.to_bits(),
                    "frame {n} sample {i}: {w} against {g}"
                );
            }
        }
    }

    #[test]
    fn a_silent_frame_is_reported_and_passed_through() {
        let mut state = DenoiseState::new();
        let silence = [0.0; FRAME_SIZE];
        let analysis = state.analyse(&silence);
        assert!(analysis.silence);
        assert_eq!(analysis.vad, 0.0);
        assert!(analysis.gains.iter().all(|g| *g == 1.0));

        // Whatever gains are handed back, a silent frame is not touched.
        let mut out = [1.0; FRAME_SIZE];
        state.synthesise(&[0.0; NB_BANDS], &mut out);
        assert!(out.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn the_stft_is_the_librarys_own_synthesis_path_without_the_pitch_filter() {
        // What a channel of a linked-stereo pair runs: the library's analysis transform, the
        // shared gains, the library's overlap-add — and not the pitch filter, which needs a pitch
        // search of its own and is the documented trade-off of the linked mode. So the reference
        // here is the library's pipeline with that one step left out, on the private fields, and
        // the `Stft` has to match it bit for bit.
        let input = fixture(40);
        let mut analyser = DenoiseState::new();
        let mut reference = DenoiseState::new();
        let mut stft = Stft::new();
        let mut want = [0.0; FRAME_SIZE];
        let mut got = [0.0; FRAME_SIZE];
        for (n, frame) in input.chunks_exact(FRAME_SIZE).enumerate() {
            let analysis = analyser.analyse(frame);
            analyser.synthesise(&analysis.gains, &mut got);

            reference.feat.shift_and_filter_input(frame);
            let silence = reference.feat.compute_frame_features();
            assert_eq!(silence, analysis.silence, "frame {n}");
            if !silence {
                let mut g = [0.0; NB_BANDS];
                let mut vad = [0.0];
                reference
                    .rnn
                    .compute(&mut g[..], &mut vad[..], reference.feat.features());
                for (gain, last) in g.iter_mut().zip(reference.lastg.iter_mut()) {
                    *gain = gain.max(0.6 * *last);
                    *last = *gain;
                }
                assert_eq!(g, analysis.gains, "frame {n}: the gains are the network's");
                let mut gf = [1.0; FREQ_SIZE];
                crate::interp_band_gain(&mut gf[..], &g[..]);
                reference.feat.apply_gain(&gf);
            }
            reference.feat.frame_synthesis(&mut want);

            stft.push(frame);
            stft.synthesise(&analysis.gains, &mut got);
            for (i, (w, g)) in want.iter().zip(&got).enumerate() {
                assert_eq!(
                    w.to_bits(),
                    g.to_bits(),
                    "frame {n} sample {i}: {w} against {g}"
                );
            }
        }
    }

    #[test]
    fn the_pitch_filter_is_the_only_difference_between_the_stft_and_the_state() {
        // The size of the trade-off, measured rather than assumed: with the network's own gains,
        // the difference between the `Stft` output and the state's is the pitch filter's comb,
        // and on this fixture — strong harmonics, where the comb does the most — it carries
        // about −17 dB of the output's energy. The bar is looser than that figure so that a
        // retrained model does not fail it; what it guards is the other direction, a `Stft`
        // that drifted into being a different denoiser rather than the same one without the
        // comb.
        let input = fixture(60);
        let mut state = DenoiseState::new();
        let mut stft = Stft::new();
        let mut from_state = [0.0; FRAME_SIZE];
        let mut from_stft = [0.0; FRAME_SIZE];
        let (mut energy, mut difference) = (0.0_f64, 0.0_f64);
        for (n, frame) in input.chunks_exact(FRAME_SIZE).enumerate() {
            let analysis = state.analyse(frame);
            state.synthesise(&analysis.gains, &mut from_state);
            stft.push(frame);
            stft.synthesise(&analysis.gains, &mut from_stft);
            if n < 4 {
                continue;
            }
            for (a, b) in from_state.iter().zip(&from_stft) {
                energy += f64::from(a * a);
                difference += f64::from((a - b) * (a - b));
            }
        }
        let ratio_db = 10.0 * (difference / energy).log10();
        assert!(
            ratio_db < -12.0,
            "the pitch filter accounts for {ratio_db:.1} dB of the output, which is more than a \
             shared mask can call a trade-off"
        );
    }
}
