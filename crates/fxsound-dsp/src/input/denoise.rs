//! RNNoise, in front of everything else.
//!
//! The one stage here that is not a filter or a dynamics processor. A gate is a time-domain
//! switch: it does nothing about noise *during* speech, which is the actual complaint — and the
//! survey of what people ship found that every product serving that case ships it as a switch and
//! never as a named voice. Five of the nine community chains surveyed put a denoiser first and
//! none put one after the limiter, so it goes here, ahead of the high-pass.
//!
//! Position is not a detail: the gate, the compressor and the de-esser all measure a level, and
//! measuring a *denoised* level is a different number. Every gate threshold in the preset draft
//! was chosen against an un-denoised floor and has to be re-voiced now that this exists.
//!
//! **Three things constrain the implementation, all of them RNNoise's.**
//!
//! 1. It works on 480-sample frames and nothing else, so a block of any other size is bridged
//!    through a pair of buffers — and that bridge *is* ten milliseconds of latency.
//! 2. It works at 48 kHz and nothing else. At any other rate the stage reports itself inactive
//!    and passes the signal through, rather than denoising the wrong bands; the capture stream is
//!    asked for 48 kHz precisely so this is rare.
//! 3. Its samples are in `[-32768, 32767]`, not `[-1, 1]`. Feeding it the usual floating-point
//!    range makes it decide the whole signal is silence, which is the failure mode that looks
//!    like "the denoiser does nothing".
//!
//! Real-time safety, honestly stated: the per-channel state is allocated once, at construction,
//! on the main loop. One allocation is *not* avoidable — `nnnoiseless` plans its FFT through a
//! thread-local cache, so the first frame denoised on the audio thread builds that plan there.
//! It happens once per thread for the life of the process, it is a few kilobytes, and there is no
//! API to pre-warm it from another thread. Everything after that frame allocates nothing.

use crate::biquad::{MAX_CHANNELS, Real};
use nnnoiseless::DenoiseState;

/// The only block size RNNoise has.
pub const FRAME: usize = DenoiseState::FRAME_SIZE;
/// The only rate RNNoise has.
pub const REQUIRED_RATE: Real = 48_000.0;
/// RNNoise's samples are 16-bit PCM carried in `f32`.
const SCALE: Real = 32_768.0;

pub struct Denoiser {
    /// One network state per channel: RNNoise is mono, and a stereo microphone's two sides are
    /// two different rooms as far as it is concerned.
    states: Vec<Box<DenoiseState<'static>>>,
    /// The frame being filled, and the frame already denoised, per channel.
    pending: Box<[[Real; FRAME]]>,
    ready: Box<[[Real; FRAME]]>,
    /// Shared across channels, because every channel receives exactly the same number of samples.
    fill: usize,
    read: usize,
    sample_rate: Real,
    enabled: bool,
    /// The last frame's voice probability, as RNNoise reports it.
    vad: Real,
}

impl std::fmt::Debug for Denoiser {
    /// The design, never the network state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("sample_rate", &self.sample_rate)
            .field("enabled", &self.enabled)
            .field("active", &self.is_active())
            .field("vad", &self.vad)
            .finish()
    }
}

impl Denoiser {
    /// Allocate the network states. Do this on the main loop; it is the expensive part.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        Self {
            states: (0..MAX_CHANNELS).map(|_| DenoiseState::new()).collect(),
            pending: vec![[0.0; FRAME]; MAX_CHANNELS].into_boxed_slice(),
            ready: vec![[0.0; FRAME]; MAX_CHANNELS].into_boxed_slice(),
            fill: 0,
            // Empty: the first 480 frames out are silence, which is what a ten-millisecond
            // look-behind costs and is why the latency is reported.
            read: FRAME,
            sample_rate: super::sane_rate(sample_rate),
            enabled: false,
            vad: 0.0,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = super::sane_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.reset();
    }

    /// Switch the denoiser on. Whether it then *runs* also depends on the rate — see
    /// [`Self::is_active`].
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            self.reset();
        }
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the denoiser is actually processing.
    ///
    /// `false` while it is switched off, and `false` at any rate other than 48 kHz. The second is
    /// the honest degradation: RNNoise's bands are defined at 48 kHz, so running it at 44.1 would
    /// denoise frequencies nine percent away from the ones it was trained on. A preset that asks
    /// for it on a device that cannot have it gets a working chain and an interface that says
    /// which stages are running, not a silently different sound.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.sample_rate == REQUIRED_RATE
    }

    /// Frames of delay the stage adds while it is running: RNNoise's whole frame, and nothing
    /// else. Zero when it is not.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        if self.is_active() { FRAME } else { 0 }
    }

    /// The last frame's voice probability, `0.0..=1.0`, as the network reports it.
    ///
    /// Read but not yet acted on: the obvious use is to hold the gate open on the network's
    /// opinion rather than on a level alone, and that changes what every gate threshold means. It
    /// belongs with the voicing of the input presets, not ahead of it.
    #[must_use]
    pub const fn voice_probability(&self) -> Real {
        self.vad
    }

    pub fn reset(&mut self) {
        for state in &mut self.states {
            *state = DenoiseState::new();
        }
        for frame in self.pending.iter_mut() {
            frame.fill(0.0);
        }
        for frame in self.ready.iter_mut() {
            frame.fill(0.0);
        }
        self.fill = 0;
        self.read = FRAME;
        self.vad = 0.0;
    }

    /// A whole interleaved block, in place.
    ///
    /// Channels past [`MAX_CHANNELS`] make the stage do nothing at all, rather than denoising the
    /// first eight and passing the rest through: every other stage here can treat an unsupported
    /// channel as untouched because the difference is a gain, but this one's difference is *time*.
    /// Delaying eight channels by ten milliseconds and not the ninth would tear the frame apart.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.is_active() || channels == 0 || channels > MAX_CHANNELS || buffer.is_empty() {
            return;
        }

        for frame in buffer.chunks_exact_mut(channels) {
            // Take the incoming sample *before* overwriting it with the denoised one. Two passes
            // over the frame would be tidier to read and would feed the network its own output,
            // which is a mistake that survives every test except one that compares the stage
            // against the library sample for sample. It did not survive that one.
            let reading = self.read < FRAME;
            for (channel, sample) in frame.iter_mut().enumerate() {
                let incoming = *sample;
                *sample = if reading {
                    self.ready[channel][self.read]
                } else {
                    0.0
                };
                // Sanitised here rather than trusted from upstream, and this is the one place
                // in the chain where that is not belt-and-braces: handed a non-finite sample,
                // `nnnoiseless` reaches `hint::unreachable_unchecked` inside its FFT — undefined
                // behaviour, caught here only because the debug check happened to be on. Every
                // other stage would merely produce a NaN. The engine already sanitises its input
                // block, so in the running application nothing ever gets this far; the guard is
                // for the day someone calls this stage from somewhere else.
                self.pending[channel][self.fill] =
                    if incoming.is_finite() { incoming } else { 0.0 };
            }
            if reading {
                self.read += 1;
            }
            self.fill += 1;

            if self.fill == FRAME {
                self.denoise(channels);
                self.fill = 0;
                self.read = 0;
            }
        }
    }

    /// One 480-sample frame per channel, scaled into and out of RNNoise's 16-bit world.
    fn denoise(&mut self, channels: usize) {
        for channel in 0..channels.min(MAX_CHANNELS) {
            let Some(state) = self.states.get_mut(channel) else {
                continue;
            };
            let (Some(pending), Some(ready)) =
                (self.pending.get_mut(channel), self.ready.get_mut(channel))
            else {
                continue;
            };
            for sample in pending.iter_mut() {
                *sample *= SCALE;
            }
            let vad = state.process_frame(ready, pending);
            for sample in ready.iter_mut() {
                *sample /= SCALE;
                // The engine checks its own output, but a stage that can hand the next one an
                // infinity is a stage that has already lost the block.
                if !sample.is_finite() {
                    *sample = 0.0;
                }
            }
            if channel == 0 {
                self.vad = if vad.is_finite() {
                    vad.clamp(0.0, 1.0)
                } else {
                    0.0
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: Real = 48_000.0;

    fn denoiser() -> Denoiser {
        let mut d = Denoiser::new(FS);
        d.set_enabled(true);
        d
    }

    /// White-ish noise, deterministic, so a test can say "this got quieter".
    fn noise(frames: usize, amplitude: Real) -> Vec<Real> {
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

    fn rms(samples: &[Real]) -> Real {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|x| x * x).sum::<Real>() / samples.len() as Real).sqrt()
    }

    #[test]
    fn a_microphones_own_floor_is_what_it_takes_away() {
        // Measured, not assumed, and the numbers are worth knowing before anyone tries to judge
        // this stage by ear: on a fifty-hertz hum under broadband hiss — which is what a desk
        // microphone in a room with a computer in it actually picks up — RNNoise takes about
        // 44 dB off. On *pure white noise* it takes about one decibel off, at every level tried.
        //
        // That is not a defect and must not be "fixed". The network was trained to tell speech
        // from noise; undifferentiated broadband hiss with no speech in it gives it nothing to
        // separate, so its band gains stay near unity. Anyone reaching for white noise to test a
        // denoiser will conclude it is broken, and they will be wrong.
        let hum_and_hiss: Vec<Real> = noise(FRAME * 100, 0.02)
            .iter()
            .enumerate()
            .map(|(n, x)| x + (n as Real * std::f32::consts::TAU * 50.0 / FS).sin() * 0.05)
            .collect();
        let mut block = hum_and_hiss.clone();
        let before = rms(&hum_and_hiss[FRAME * 4..]);
        denoiser().process(&mut block, 1);
        let reduction = 20.0 * (before / rms(&block[FRAME * 8..]).max(1e-12)).log10();
        assert!(
            reduction > 20.0,
            "a room floor should be most of what leaves; only {reduction} dB went"
        );

        let white = noise(FRAME * 100, 0.05);
        let mut block = white.clone();
        denoiser().process(&mut block, 1);
        let white_reduction =
            20.0 * (rms(&white[FRAME * 4..]) / rms(&block[FRAME * 8..]).max(1e-12)).log10();
        assert!(
            white_reduction < 10.0,
            "white noise lost {white_reduction} dB — if this ever becomes large, the network \
             changed and every gate threshold voiced against it has to be looked at again"
        );
    }

    #[test]
    fn the_stage_is_exactly_the_library_with_a_bridge_around_it() {
        // The scaling failure does not crash and does not warn: fed the usual [-1, 1] range,
        // RNNoise decides every frame is silence, leaves the signal alone, and the stage appears
        // to work while doing nothing. So this compares the stage against the library driven by
        // hand — same input, same scaling, same frames — and demands the samples match. It pins
        // the bridge at the same time: a wrong block boundary shows up here as a mismatch rather
        // than as a subtly different sound.
        //
        // (The first draft of this test asserted that a steady tone comes back at the level it
        // went in. It does not, and should not: a 220 Hz sine is not speech, and RNNoise removed
        // it completely. That is the network working, not the stage failing.)
        let mut reference = DenoiseState::new();
        let input = noise(FRAME * 5, 0.05);
        let mut expected = Vec::with_capacity(input.len());
        let (frames, _) = input.as_chunks::<FRAME>();
        for chunk in frames {
            let scaled: Vec<Real> = chunk.iter().map(|x| x * SCALE).collect();
            let mut out = [0.0; FRAME];
            reference.process_frame(&mut out, &scaled);
            expected.extend(out.iter().map(|x| x / SCALE));
        }

        let mut block = input;
        let mut stage = denoiser();
        stage.process(&mut block, 1);

        // The stage is one frame behind, which is the latency it reports.
        for (n, (got, want)) in block[FRAME..].iter().zip(&expected).enumerate() {
            assert!(
                (got - want).abs() < 1e-9,
                "sample {n}: {got} against {want}"
            );
        }
    }

    #[test]
    fn the_delay_is_one_rnnoise_frame_and_it_is_reported() {
        let mut d = denoiser();
        let mut block = noise(FRAME * 6, 0.05);
        d.process(&mut block, 1);

        assert_eq!(d.latency_frames(), FRAME);
        assert!(
            block[..FRAME].iter().all(|x| *x == 0.0),
            "the priming frame should be silence"
        );
        assert!(
            block[FRAME..].iter().any(|x| x.abs() > 1e-9),
            "nothing came out after the priming frame"
        );
        // That the samples *after* the priming frame are the right ones, in the right order, is
        // what `the_stage_is_exactly_the_library_with_a_bridge_around_it` checks. A denoiser has
        // no reason to preserve the shape of a ramp, so nothing here tries to measure one.
    }

    #[test]
    fn a_rate_it_cannot_work_at_passes_the_signal_through_untouched() {
        // RNNoise's bands are defined at 48 kHz. At 44.1 it would denoise frequencies nine
        // percent from where it was trained, which is worse than not denoising.
        let mut d = Denoiser::new(44_100.0);
        d.set_enabled(true);
        assert!(d.is_enabled());
        assert!(!d.is_active());
        assert_eq!(d.latency_frames(), 0);

        let input = noise(4_800, 0.05);
        let mut block = input.clone();
        d.process(&mut block, 1);
        assert_eq!(block, input);

        // And at the rate it does have, it runs.
        d.set_sample_rate(48_000.0);
        assert!(d.is_active());
        assert_eq!(d.latency_frames(), FRAME);
    }

    #[test]
    fn switched_off_it_is_not_in_the_signal_path_at_all() {
        let mut d = Denoiser::new(FS);
        let input = noise(4_800, 0.05);
        let mut block = input.clone();
        d.process(&mut block, 1);
        assert_eq!(block, input, "a disabled denoiser moved the signal");
        assert_eq!(
            d.latency_frames(),
            0,
            "and it must not claim latency either"
        );
    }

    #[test]
    fn channels_keep_their_own_network_and_stay_in_step() {
        // Two channels, one loud and one silent. The silent one must come back silent — a shared
        // network would leak one into the other — and both must be delayed by the same frame, or
        // a stereo capture is torn in two.
        let mut d = denoiser();
        let mono = noise(FRAME * 6, 0.2);
        let mut block = Vec::with_capacity(mono.len() * 2);
        for &x in &mono {
            block.push(x);
            block.push(0.0);
        }
        d.process(&mut block, 2);

        let right: Vec<Real> = block.iter().skip(1).step_by(2).copied().collect();
        assert!(
            rms(&right[FRAME..]) < 1e-6,
            "the silent channel picked up {} from its neighbour",
            rms(&right[FRAME..])
        );
        let left: Vec<Real> = block.iter().step_by(2).copied().collect();
        assert!(
            left[..FRAME].iter().all(|x| *x == 0.0),
            "the two channels did not prime together"
        );
    }

    #[test]
    fn more_channels_than_it_supports_make_it_stand_aside_completely() {
        // Every other stage treats an unsupported channel as untouched, because there the
        // difference is a gain. Here it is *time*: delaying eight channels and not the ninth
        // would tear the frame apart, so the stage does nothing rather than something wrong.
        let mut d = denoiser();
        let input: Vec<Real> = (0..(MAX_CHANNELS + 1) * FRAME * 2)
            .map(|n| (n % 97) as Real / 100.0)
            .collect();
        let mut block = input.clone();
        d.process(&mut block, MAX_CHANNELS + 1);
        assert_eq!(block, input);
    }

    #[test]
    fn a_non_finite_sample_does_not_come_out_the_other_side() {
        let mut d = denoiser();
        let mut block = vec![Real::INFINITY; FRAME * 3];
        d.process(&mut block, 1);
        assert!(block.iter().all(|x| x.is_finite()));

        d.reset();
        let mut block = noise(FRAME * 4, 0.05);
        d.process(&mut block, 1);
        assert!(
            block.iter().all(|x| x.is_finite()),
            "the network stayed poisoned after a reset"
        );
    }
}
