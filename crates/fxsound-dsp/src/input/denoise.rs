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
//!    through a pair of buffers — and that bridge is ten milliseconds of latency. The library
//!    adds ten more of its own: its window spans two frames and its output is the *earlier* one.
//!    The stage therefore delays the signal by 960 frames, which is what it reports; 0.3.0
//!    reported 480, and a cross-correlation test now pins the true figure.
//! 2. It works at 48 kHz and nothing else. At any other rate the stage reports itself inactive
//!    and passes the signal through, rather than denoising the wrong bands; the capture stream is
//!    asked for 48 kHz precisely so this is rare.
//! 3. Its samples are in `[-32768, 32767]`, not `[-1, 1]`. Feeding it the usual floating-point
//!    range makes it decide the whole signal is silence, which is the failure mode that looks
//!    like "the denoiser does nothing".
//!
//! **What 0.4.0 puts on top of the network** is a control surface over its band gains — the
//! reason the library is vendored as [`fxsound_rnnoise`], whose `analyse`/`synthesise` split
//! exposes them. A [`DenoiseControl`] floors the gains (how much may be removed), pulls them to
//! that floor when the network is not hearing a voice, hands part of the gap back in proportion
//! to the voice probability, and mixes the result with the dry signal. [`DenoiseChannelMode`]
//! decides how many networks run: one per channel, one on a downmix copied to every channel, or
//! one on a downmix whose gains are applied to each channel through that channel's own
//! transform — a shared mask over a preserved image.
//!
//! Real-time safety, honestly stated: the per-channel state is allocated once, at construction,
//! on the main loop, and every reset is in place. One allocation is *not* avoidable — the
//! library plans its FFT through a thread-local cache, so the first frame denoised on the audio
//! thread builds that plan there. It happens once per thread for the life of the process, it is
//! a few kilobytes, and there is no API to pre-warm it from another thread. Everything after that
//! frame allocates nothing, and a test with a counting allocator holds it to that.
//!
//! **What a toggle costs, and why that is accepted.** [`Denoiser::set_enabled`],
//! [`Denoiser::set_level`] and [`Denoiser::set_control`] never restart anything: a snapshot that
//! re-applies `enabled = true` while the stage runs, or moves between levels, is a change of
//! numbers, and a toggle undone before the next block is never noticed. 0.3.0 restarted the
//! bridge on every one of those, which was the bug. Coming back after the stage has actually
//! stood aside for a block is different, and deliberately so: the bridge and the network are
//! re-primed from silence, and the first 960 frames out are twenty milliseconds of nothing — the
//! bridge's empty frame, then the library's reconstruction of its zeroed window. That figure is
//! not a restart cost that could be engineered away; it is the latency change itself. An
//! inactive stage delays by nothing and an active one by 960 frames, so switching on has to put
//! 960 frames of *something* into the stream, and the something is silence because the
//! alternative is worse on both counts that matter. A bridge kept running while the stage is off
//! would fill those frames with the last twenty milliseconds already heard undelayed — a repeat,
//! which is a stutter, where a gap that short is at worst a click — and it would run the
//! network, the most expensive thing in the chain, on every chain that has it off, which is most
//! of them. Two tests pin both halves: the cost is exactly the latency and not a sample more,
//! and the toggle that is undone in time costs nothing.

use crate::biquad::{MAX_CHANNELS, Real};
use crate::input::processor::{AudioProcessor, ProcessContext, StageMeter};
use fxsound_core::messages::InputDspParams;
use fxsound_core::{DenoiseChannelMode, DenoiseControl, DenoiseLevel};
use fxsound_rnnoise::{DenoiseState, NB_BANDS, Stft};

/// The only block size RNNoise has.
pub const FRAME: usize = DenoiseState::FRAME_SIZE;
/// The only rate RNNoise has.
pub const REQUIRED_RATE: Real = 48_000.0;
/// Frames of delay the stage adds while it runs: the bridge's frame and the library's own.
pub const LATENCY: usize = 2 * FRAME;
/// RNNoise's samples are 16-bit PCM carried in `f32`.
const SCALE: Real = 32_768.0;

/// The VAD-gated attenuation moves toward its target with this time constant, in frames of
/// 10 ms: 50 ms, so a pause is pulled down over a few frames rather than switched.
const VAD_ATTENUATION_FRAMES: Real = 5.0;
/// The reduction meter's time constant, in frames: 100 ms, a readout rather than a waveform.
const METER_FRAMES: Real = 10.0;

pub struct Denoiser {
    /// One network state per channel: RNNoise is mono, and a stereo microphone's two sides are
    /// two different rooms as far as it is concerned. The downmix modes use only the first.
    states: Vec<Box<DenoiseState<'static>>>,
    /// One transform per channel, for the linked mode: the shared gains applied to each
    /// channel's own spectrum.
    stfts: Vec<Stft>,
    /// The frame being filled, the frame already denoised, and the frame before the one being
    /// filled — which is the dry signal time-aligned with the denoised one, since the library's
    /// output is a frame late. All per channel, all in the `[-1, 1]` range.
    pending: Box<[[Real; FRAME]]>,
    ready: Box<[[Real; FRAME]]>,
    previous: Box<[[Real; FRAME]]>,
    /// Scratch in the library's 16-bit range: the downmix, and one channel at a time.
    downmix: [Real; FRAME],
    scratch: [Real; FRAME],
    /// Shared across channels, because every channel receives exactly the same number of samples.
    fill: usize,
    read: usize,
    /// Whether the bridge and the network have been started since the stage last became active.
    /// Cleared when it stands aside — by a block actually processed while inactive, never by the
    /// setters — so that coming back starts from silence rather than from whatever the buffers
    /// held: ten milliseconds of the last sentence would be worse than ten of nothing. What that
    /// costs, and why it is the latency and not a restart, is at the top of the module.
    primed: bool,

    sample_rate: Real,
    enabled: bool,
    level: DenoiseLevel,
    mode: DenoiseChannelMode,
    control: DenoiseControl,

    /// The last frame's voice probability, as RNNoise reports it, for channel zero or the downmix.
    vad: Real,
    /// The one-pole VAD-gated attenuation per channel, `1.0` when the network hears a voice.
    attenuation: [Real; MAX_CHANNELS],
    /// Smoothed `20·log10(rms_in / rms_out)` on channel zero, positive dB.
    reduction_db: Real,

    /// A test's way of saying what the network would say, so that a control-surface test
    /// measures the surface and not the network's opinion of a synthetic talker.
    #[cfg(test)]
    pub(crate) vad_override: Option<Real>,
    /// How many times [`Denoiser::reset`] has run. A reset is eight network states zeroed and
    /// the audio after one of them is the same silence as after two, so nothing downstream can
    /// tell the chain resetting this stage on top of its own rate change — which 0.3.0 did —
    /// from the chain leaving it alone. Counting is the only way to prove it does not.
    #[cfg(test)]
    pub(crate) resets: u32,
}

impl std::fmt::Debug for Denoiser {
    /// The design, never the network state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("sample_rate", &self.sample_rate)
            .field("enabled", &self.enabled)
            .field("level", &self.level)
            .field("mode", &self.mode)
            .field("control", &self.control)
            .field("active", &self.is_active())
            .field("vad", &self.vad)
            .field("reduction_db", &self.reduction_db)
            .finish()
    }
}

impl Denoiser {
    /// Allocate the network states. Do this on the main loop; it is the expensive part.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        Self {
            states: (0..MAX_CHANNELS).map(|_| DenoiseState::new()).collect(),
            stfts: (0..MAX_CHANNELS).map(|_| Stft::new()).collect(),
            pending: vec![[0.0; FRAME]; MAX_CHANNELS].into_boxed_slice(),
            ready: vec![[0.0; FRAME]; MAX_CHANNELS].into_boxed_slice(),
            previous: vec![[0.0; FRAME]; MAX_CHANNELS].into_boxed_slice(),
            downmix: [0.0; FRAME],
            scratch: [0.0; FRAME],
            fill: 0,
            // Empty: the first 480 frames out are silence, which is what a ten-millisecond
            // look-behind costs and is why the latency is reported.
            read: FRAME,
            primed: false,
            sample_rate: super::sane_rate(sample_rate),
            enabled: false,
            level: DenoiseLevel::default(),
            mode: DenoiseChannelMode::default(),
            control: DenoiseLevel::default().control(),
            vad: 0.0,
            attenuation: [1.0; MAX_CHANNELS],
            reduction_db: 0.0,
            #[cfg(test)]
            vad_override: None,
            #[cfg(test)]
            resets: 0,
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

    /// Switch the denoiser on. Whether it then *runs* also depends on the level and the rate —
    /// see [`Self::is_active`]. Nothing is restarted here, ever: a snapshot that re-applies `true`
    /// while the stage runs costs nothing, and neither does a toggle undone before the next
    /// block. Only once the stage has actually stood aside for a block does coming back cost
    /// anything, and then it is twenty milliseconds of silence on the next active block — the
    /// latency, re-inserted, for the reasons the module documentation gives.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// How hard the network may work. `Off` makes the stage stand aside, latency and all; the
    /// other three are rows of the control surface, and moving between them while running is a
    /// change of numbers and not a restart.
    pub fn set_level(&mut self, level: DenoiseLevel) {
        self.level = level;
    }

    /// One network per channel, one on a downmix copied to every channel, or one on a downmix
    /// whose gains are applied to each channel's own spectrum. Switching clears the per-channel
    /// transforms — in place — so the linked mode never overlap-adds a tail it did not synthesise.
    pub fn set_channels(&mut self, mode: DenoiseChannelMode) {
        if mode != self.mode {
            self.mode = mode;
            for stft in &mut self.stfts {
                stft.reset();
            }
        }
    }

    /// The control surface itself, for a preset that carries a row of its own instead of a
    /// level's. Clamped into the limits the snapshot allows, with the level's row as the
    /// fallback for a value that is not a number.
    pub fn set_control(&mut self, control: DenoiseControl) {
        let mut control = control;
        control.sanitise(self.level.control());
        self.control = control;
    }

    /// Everything the snapshot says about this stage.
    pub fn apply(&mut self, params: &InputDspParams) {
        self.set_enabled(params.rnnoise);
        self.set_level(params.denoise_level);
        self.set_channels(params.denoise_channels);
        self.set_control(params.denoise_control);
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn level(&self) -> DenoiseLevel {
        self.level
    }

    #[must_use]
    pub const fn channel_mode(&self) -> DenoiseChannelMode {
        self.mode
    }

    #[must_use]
    pub const fn control(&self) -> DenoiseControl {
        self.control
    }

    /// Whether the denoiser is actually processing.
    ///
    /// `false` while it is switched off or asked for nothing (`Off`, or a row whose floor is
    /// 0 dB or whose mix has no wet in it), and `false` at any rate other than 48 kHz. The last
    /// is the honest degradation: RNNoise's bands are defined at 48 kHz, so running it at 44.1
    /// would denoise frequencies nine percent away from the ones it was trained on. A preset that
    /// asks for it on a device that cannot have it gets a working chain and an interface that
    /// says which stages are running, not a silently different sound.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled
            && self.level != DenoiseLevel::Off
            && self.control.is_active()
            && self.sample_rate == REQUIRED_RATE
    }

    /// Frames of delay the stage adds while it is running: [`LATENCY`], the bridge's frame plus
    /// the library's own. Zero when it is not.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        if self.is_active() { LATENCY } else { 0 }
    }

    /// The last frame's voice probability, `0.0..=1.0`, as the network reports it — for channel
    /// zero in the independent mode and for the downmix otherwise. The gate reads it as a
    /// side-chain when a preset asks (`vad_gate`).
    #[must_use]
    pub const fn voice_probability(&self) -> Real {
        self.vad
    }

    /// What a meter shows: decibels the stage is taking away, `20·log10(rms_in / rms_out)` per
    /// frame on channel zero, smoothed over about a hundred milliseconds. Zero when inactive.
    #[must_use]
    pub const fn reduction_db(&self) -> Real {
        self.reduction_db
    }

    /// Forget everything, in place. Allocates nothing: the network states are zeroed where they
    /// stand, which is what makes this callable from the audio thread on a preset change, a
    /// bypass, or the engine's non-finite recovery.
    pub fn reset(&mut self) {
        #[cfg(test)]
        {
            self.resets += 1;
        }
        for state in &mut self.states {
            state.reset();
        }
        for stft in &mut self.stfts {
            stft.reset();
        }
        self.clear_bridge();
        self.primed = self.is_active();
    }

    fn clear_bridge(&mut self) {
        for frame in self.pending.iter_mut() {
            frame.fill(0.0);
        }
        for frame in self.ready.iter_mut() {
            frame.fill(0.0);
        }
        for frame in self.previous.iter_mut() {
            frame.fill(0.0);
        }
        self.fill = 0;
        self.read = FRAME;
        self.vad = 0.0;
        self.attenuation = [1.0; MAX_CHANNELS];
        self.reduction_db = 0.0;
    }

    /// A whole interleaved block, in place.
    ///
    /// Channels past [`MAX_CHANNELS`] make the stage do nothing at all, rather than denoising the
    /// first eight and passing the rest through: every other stage here can treat an unsupported
    /// channel as untouched because the difference is a gain, but this one's difference is *time*.
    /// Delaying eight channels by twenty milliseconds and not the ninth would tear the frame apart.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if !self.is_active() || channels == 0 || channels > MAX_CHANNELS || buffer.is_empty() {
            self.primed = false;
            return;
        }
        if !self.primed {
            // Coming back: start from silence, not from what the buffers held when the stage
            // last stood aside. In place, so this is cheap enough for the audio thread.
            self.reset();
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
                // the library reaches `hint::unreachable_unchecked` inside its FFT — undefined
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

    /// One 480-sample frame per channel, through the network and the control surface.
    fn denoise(&mut self, channels: usize) {
        let channels = channels.min(MAX_CHANNELS);
        match self.mode {
            DenoiseChannelMode::Independent => self.denoise_independent(channels),
            DenoiseChannelMode::Mono => self.denoise_downmixed(channels, false),
            DenoiseChannelMode::Linked => self.denoise_downmixed(channels, true),
        }

        // The dry frame that lines up with what was just synthesised is the one *before* the
        // frame just analysed, because the library's output is a frame late.
        self.mix_and_meter(channels);
        for channel in 0..channels {
            self.previous[channel] = self.pending[channel];
        }
    }

    fn denoise_independent(&mut self, channels: usize) {
        for channel in 0..channels {
            let (Some(state), Some(pending), Some(ready)) = (
                self.states.get_mut(channel),
                self.pending.get(channel),
                self.ready.get_mut(channel),
            ) else {
                continue;
            };
            for (dst, src) in self.scratch.iter_mut().zip(pending) {
                *dst = src * SCALE;
            }
            let analysis = state.analyse(&self.scratch);
            let mut gains = analysis.gains;
            #[cfg(test)]
            let vad = self.vad_override.unwrap_or(analysis.vad);
            #[cfg(not(test))]
            let vad = analysis.vad;
            let vad = sane_vad(vad);
            shape_gains(
                &mut gains,
                vad,
                &self.control,
                &mut self.attenuation[channel],
            );
            state.synthesise(&gains, ready);
            unscale(ready);
            if channel == 0 {
                self.vad = vad;
            }
        }
    }

    /// The two downmix modes: one network on the mean of the first two channels, then either
    /// the network's own output to every channel (`Mono`) or its gains through each channel's
    /// own transform (`Linked`). Channels beyond the first two share the pair's gains.
    fn denoise_downmixed(&mut self, channels: usize, linked: bool) {
        let analysed = channels.min(2);
        let weight = SCALE / analysed as Real;
        self.downmix.fill(0.0);
        for pending in self.pending.iter().take(analysed) {
            for (dst, src) in self.downmix.iter_mut().zip(pending) {
                *dst += src * weight;
            }
        }
        let Some(state) = self.states.first_mut() else {
            return;
        };
        let analysis = state.analyse(&self.downmix);
        let mut gains = analysis.gains;
        #[cfg(test)]
        let vad = self.vad_override.unwrap_or(analysis.vad);
        #[cfg(not(test))]
        let vad = analysis.vad;
        let vad = sane_vad(vad);
        shape_gains(&mut gains, vad, &self.control, &mut self.attenuation[0]);
        self.vad = vad;

        if linked {
            for channel in 0..channels {
                let (Some(stft), Some(pending), Some(ready)) = (
                    self.stfts.get_mut(channel),
                    self.pending.get(channel),
                    self.ready.get_mut(channel),
                ) else {
                    continue;
                };
                for (dst, src) in self.scratch.iter_mut().zip(pending) {
                    *dst = src * SCALE;
                }
                stft.push(&self.scratch);
                stft.synthesise(&gains, ready);
                unscale(ready);
            }
        } else {
            let Some(first) = self.ready.first_mut() else {
                return;
            };
            state.synthesise(&gains, first);
            unscale(first);
            let first = *first;
            for ready in self.ready.iter_mut().take(channels).skip(1) {
                *ready = first;
            }
            // In this mode the dry signal is the downmix too: mixing each channel's own dry
            // half back in would undo the one thing the mode promises, identical channels.
            let mut mono_previous = [0.0; FRAME];
            for previous in self.previous.iter().take(analysed) {
                for (dst, src) in mono_previous.iter_mut().zip(previous) {
                    *dst += src / analysed as Real;
                }
            }
            for previous in self.previous.iter_mut().take(channels) {
                *previous = mono_previous;
            }
        }
    }

    /// Wet/dry on the time-aligned pair, and the reduction meter from channel zero.
    fn mix_and_meter(&mut self, channels: usize) {
        let wet = self.control.wet_dry.clamp(0.0, 1.0);
        let dry = 1.0 - wet;
        let (mut in_energy, mut out_energy) = (0.0_f64, 0.0_f64);
        for channel in 0..channels {
            let (Some(ready), Some(previous)) =
                (self.ready.get_mut(channel), self.previous.get(channel))
            else {
                continue;
            };
            for (out, &before) in ready.iter_mut().zip(previous) {
                *out = before * dry + *out * wet;
                if channel == 0 {
                    in_energy += f64::from(before * before);
                    out_energy += f64::from(*out * *out);
                }
            }
        }
        // Nothing to measure in silence; a meter that read forty decibels of reduction on a
        // muted microphone would be reporting the arithmetic of `1e-9 / 1e-12`.
        // Ten times the log of an energy ratio is twenty times the log of the RMS ratio — the
        // figure the strip shows. The first draft halved it, and every test on the meter was a
        // lower bound; one now holds it to the ratio measured from outside.
        let target = if in_energy > 1.0e-10 && out_energy > 0.0 {
            (10.0 * (in_energy / out_energy).log10()).max(0.0) as Real
        } else {
            0.0
        };
        self.reduction_db += (target - self.reduction_db) / METER_FRAMES;
        if !self.reduction_db.is_finite() {
            self.reduction_db = 0.0;
        }
    }

    /// The stage's meter, for the chain: the reduction, whether it is running, and the voice
    /// probability as the extra number.
    #[must_use]
    pub fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: self.reduction_db,
            running: self.is_active(),
            aux: self.vad,
        }
    }
}

impl AudioProcessor for Denoiser {
    fn prepare(&mut self, sample_rate: Real) {
        self.set_sample_rate(sample_rate);
    }

    fn apply(&mut self, params: &InputDspParams) {
        Denoiser::apply(self, params);
    }

    fn reset(&mut self) {
        Denoiser::reset(self);
    }

    fn is_active(&self) -> bool {
        Denoiser::is_active(self)
    }

    fn latency_frames(&self) -> usize {
        Denoiser::latency_frames(self)
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        Denoiser::process(self, buffer, ctx.channels);
    }

    fn meter(&self) -> StageMeter {
        Denoiser::meter(self)
    }
}

/// Back into the `[-1, 1]` range, with the guard every stage keeps on what it hands the next one:
/// the engine checks its own output, but a stage that can hand the next one an infinity is a
/// stage that has already lost the block.
fn unscale(frame: &mut [Real; FRAME]) {
    for sample in frame.iter_mut() {
        *sample /= SCALE;
        if !sample.is_finite() {
            *sample = 0.0;
        }
    }
}

fn sane_vad(vad: Real) -> Real {
    if vad.is_finite() {
        vad.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The control surface, applied to the network's band gains before synthesis, in the order the
/// design lists: floor, then the VAD-gated pull toward the floor, then voice preservation.
///
/// `attenuation` is the one-pole state of the second step, `1.0` when released. The pull never
/// takes a band below the floor — `max_suppression_db` is a maximum, and a second multiplication
/// by the floor would make it a maximum of twice itself.
fn shape_gains(
    gains: &mut [Real; NB_BANDS],
    vad: Real,
    control: &DenoiseControl,
    attenuation: &mut Real,
) {
    let floor = control.gain_floor().clamp(0.0, 1.0);
    let target = if vad < control.vad_threshold {
        floor
    } else {
        1.0
    };
    *attenuation += (target - *attenuation) / VAD_ATTENUATION_FRAMES;
    if !attenuation.is_finite() {
        *attenuation = 1.0;
    }
    let preserve = control.voice_preservation.clamp(0.0, 1.0) * vad;
    for gain in gains.iter_mut() {
        let floored = gain.max(floor);
        let pulled = (floored * *attenuation).max(floor);
        *gain = pulled + preserve * (1.0 - pulled);
        if !gain.is_finite() {
            *gain = 1.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::detector::db_to_linear;

    const FS: Real = 48_000.0;

    fn denoiser() -> Denoiser {
        let mut d = Denoiser::new(FS);
        d.set_enabled(true);
        d
    }

    fn with_level(level: DenoiseLevel) -> Denoiser {
        let mut d = denoiser();
        d.set_level(level);
        d.set_control(level.control());
        d
    }

    /// White-ish noise, deterministic, so a test can say "this got quieter".
    fn noise(frames: usize, amplitude: Real) -> Vec<Real> {
        noise_from(0x2545_f491_4f6c_dd1d_u64, frames, amplitude)
    }

    fn noise_from(seed: u64, frames: usize, amplitude: Real) -> Vec<Real> {
        let mut state = seed;
        (0..frames)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as Real / 8_388_608.0 - 1.0) * amplitude
            })
            .collect()
    }

    /// What a desk microphone in a room with a computer in it actually picks up: mains hum with
    /// broadband hiss over it — the fixture RNNoise takes forty decibels off.
    fn hum_and_hiss(frames: usize) -> Vec<Real> {
        noise(frames, 0.02)
            .iter()
            .enumerate()
            .map(|(n, x)| x + (n as Real * std::f32::consts::TAU * 50.0 / FS).sin() * 0.05)
            .collect()
    }

    /// A stand-in for a voice: harmonics under a syllabic envelope.
    fn speech_like(frames: usize, amplitude: Real) -> Vec<Real> {
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

    /// A talker in a room: the voice over hum and hiss, so the network has something to remove
    /// and a test can tell handing the voice back from doing nothing.
    fn noisy_speech(frames: usize) -> Vec<Real> {
        let voice = speech_like(frames, db_to_linear(-14.0));
        let hiss = noise(frames, 0.02);
        (0..frames)
            .map(|n| {
                voice[n] + hiss[n] + (n as Real * std::f32::consts::TAU * 50.0 / FS).sin() * 0.03
            })
            .collect()
    }

    fn rms(samples: &[Real]) -> Real {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|x| x * x).sum::<Real>() / samples.len() as Real).sqrt()
    }

    fn reduction_on(fixture: &[Real], d: &mut Denoiser) -> Real {
        let mut block = fixture.to_vec();
        d.process(&mut block, 1);
        let before = rms(&fixture[FRAME * 4..fixture.len() - LATENCY]);
        let after = rms(&block[FRAME * 4 + LATENCY..]);
        20.0 * (before / after.max(1e-12)).log10()
    }

    #[test]
    fn a_microphones_own_floor_is_what_it_takes_away() {
        // Measured, not assumed, and the numbers are worth knowing before anyone tries to judge
        // this stage by ear: on a fifty-hertz hum under broadband hiss — which is what a desk
        // microphone in a room with a computer in it actually picks up — RNNoise takes tens of
        // decibels off. On *pure white noise* it takes about one decibel off, at every level
        // tried.
        //
        // That is not a defect and must not be "fixed". The network was trained to tell speech
        // from noise; undifferentiated broadband hiss with no speech in it gives it nothing to
        // separate, so its band gains stay near unity. Anyone reaching for white noise to test a
        // denoiser will conclude it is broken, and they will be wrong.
        let reduction = reduction_on(
            &hum_and_hiss(FRAME * 100),
            &mut with_level(DenoiseLevel::Strong),
        );
        assert!(
            reduction > 20.0,
            "a room floor should be most of what leaves; only {reduction} dB went"
        );

        let white_reduction = reduction_on(
            &noise(FRAME * 100, 0.05),
            &mut with_level(DenoiseLevel::Strong),
        );
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
        // hand — same input, same scaling, same frames, the same control surface on the gains —
        // and demands the samples match. It pins the bridge at the same time: a wrong block
        // boundary shows up here as a mismatch rather than as a subtly different sound.
        let control = DenoiseLevel::Medium.control();
        let mut reference = DenoiseState::new();
        let mut attenuation = 1.0;
        let input = hum_and_hiss(FRAME * 8);
        let mut expected = Vec::with_capacity(input.len());
        let (frames, _) = input.as_chunks::<FRAME>();
        let mut dry_previous = [0.0; FRAME];
        for chunk in frames {
            let scaled: Vec<Real> = chunk.iter().map(|x| x * SCALE).collect();
            let analysis = reference.analyse(&scaled);
            let mut gains = analysis.gains;
            shape_gains(
                &mut gains,
                sane_vad(analysis.vad),
                &control,
                &mut attenuation,
            );
            let mut out = [0.0; FRAME];
            reference.synthesise(&gains, &mut out);
            for (o, d) in out.iter().zip(&dry_previous) {
                expected.push(o / SCALE * control.wet_dry + d * (1.0 - control.wet_dry));
            }
            dry_previous = *chunk;
        }

        let mut block = input;
        let mut stage = with_level(DenoiseLevel::Medium);
        stage.process(&mut block, 1);

        // The stage is one bridge frame behind the library, which is itself a frame behind.
        for (n, (got, want)) in block[FRAME..].iter().zip(&expected).enumerate() {
            assert!(
                (got - want).abs() < 1e-7,
                "sample {n}: {got} against {want}"
            );
        }
    }

    #[test]
    fn the_delay_is_two_frames_and_it_is_reported() {
        // Cross-correlation on white noise, which the network barely touches, so the lag at
        // which the output lines up with the input is unambiguous. 0.3.0 reported 480 — the
        // bridge — and left out the library's own frame; a recording application told that
        // figure drifted out of lip sync by ten milliseconds.
        let mut d = with_level(DenoiseLevel::Medium);
        let input = noise(FRAME * 30, 0.1);
        let mut block = input.clone();
        d.process(&mut block, 1);

        let (mut best_lag, mut best) = (0, Real::MIN);
        let window = FRAME * 10;
        let start = FRAME * 12;
        for lag in (LATENCY - FRAME)..=(LATENCY + FRAME) {
            let score: Real = (start..start + window)
                .map(|n| block[n] * input[n - lag])
                .sum();
            if score > best {
                best = score;
                best_lag = lag;
            }
        }
        assert_eq!(
            best_lag, LATENCY,
            "the signal lags by {best_lag}, not {LATENCY}"
        );
        assert_eq!(d.latency_frames(), LATENCY);
        assert!(
            block[..FRAME].iter().all(|x| *x == 0.0),
            "the priming frame should be silence"
        );
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
        assert_eq!(d.latency_frames(), LATENCY);
    }

    #[test]
    fn switched_off_or_asked_for_nothing_it_is_not_in_the_signal_path_at_all() {
        let input = noise(4_800, 0.05);
        let mut d = Denoiser::new(FS);
        let mut block = input.clone();
        d.process(&mut block, 1);
        assert_eq!(block, input, "a disabled denoiser moved the signal");
        assert_eq!(
            d.latency_frames(),
            0,
            "and it must not claim latency either"
        );

        let mut d = with_level(DenoiseLevel::Off);
        let mut block = input.clone();
        d.process(&mut block, 1);
        assert_eq!(block, input, "`Off` is a level that does nothing");
        assert_eq!(d.latency_frames(), 0);

        let mut d = with_level(DenoiseLevel::Medium);
        d.set_control(DenoiseControl {
            wet_dry: 0.0,
            ..DenoiseLevel::Medium.control()
        });
        let mut block = input.clone();
        d.process(&mut block, 1);
        assert_eq!(block, input, "a mix with no wet in it is the dry signal");
        assert_eq!(d.latency_frames(), 0);
    }

    #[test]
    fn the_levels_take_away_more_in_the_order_they_are_named() {
        let fixture = hum_and_hiss(FRAME * 100);
        let mut reductions = Vec::new();
        for level in DenoiseLevel::ALL {
            reductions.push(reduction_on(&fixture, &mut with_level(level)));
        }
        assert!(reductions[0].abs() < 0.01, "Off took {} dB", reductions[0]);
        for pair in reductions.windows(2) {
            assert!(
                pair[1] > pair[0] + 1.0,
                "the levels are not monotone: {reductions:?}"
            );
        }
        assert!(
            reductions[1] > 3.0 && reductions[1] < 14.0,
            "Light is a twelve-decibel floor with half the voice handed back: {reductions:?}"
        );
    }

    #[test]
    fn half_wet_sits_between_dry_and_wet() {
        let fixture = hum_and_hiss(FRAME * 60);
        let full = reduction_on(&fixture, &mut with_level(DenoiseLevel::Strong));
        let mut half = with_level(DenoiseLevel::Strong);
        half.set_control(DenoiseControl {
            wet_dry: 0.5,
            ..DenoiseLevel::Strong.control()
        });
        let half = reduction_on(&fixture, &mut half);
        assert!(
            half > 1.0 && half < full - 1.0,
            "half wet should sit between 0 and {full} dB, got {half}"
        );
        // Half the dry signal survives, so the reduction cannot exceed six decibels — the
        // arithmetic of the mix, and a useful sanity bound on the alignment: a dry path that
        // was not time-aligned with the wet one would add rather than mix, and read differently.
        assert!(half < 6.5, "half wet cannot take more than 6 dB: {half}");
    }

    #[test]
    fn full_voice_preservation_hands_a_confident_voice_back() {
        // The network hears a voice, the row says keep everything the network is sure of, and
        // the signal comes out within a decibel of how it went in — noise and all, because the
        // preservation is the user's choice to keep the voice intact at the noise's expense. The
        // VAD is forced rather than trusted so the test measures the surface, not the network's
        // opinion of a synthetic talker.
        let fixture = noisy_speech(FRAME * 60);
        let mut d = with_level(DenoiseLevel::Strong);
        d.set_control(DenoiseControl {
            voice_preservation: 1.0,
            ..DenoiseLevel::Strong.control()
        });
        d.vad_override = Some(1.0);
        let reduction = reduction_on(&fixture, &mut d);
        assert!(
            reduction.abs() < 1.0,
            "a preserved voice moved by {reduction} dB"
        );

        // And without the preservation, the same forced VAD leaves the strong row free to work.
        let mut d = with_level(DenoiseLevel::Strong);
        d.vad_override = Some(1.0);
        let unpreserved = reduction_on(&fixture, &mut d);
        assert!(
            unpreserved > reduction + 0.5,
            "preservation made no difference: {unpreserved} against {reduction}"
        );
    }

    /// Per-frame gain on one channel of an interleaved block, in dB, after the stage settled.
    fn frame_gains(input: &[Real], output: &[Real], channels: usize, channel: usize) -> Vec<Real> {
        let frames = input.len() / channels;
        (6..(frames - LATENCY) / FRAME - 1)
            .map(|k| {
                let start = k * FRAME;
                let before: Vec<Real> = (start..start + FRAME)
                    .map(|n| input[n * channels + channel])
                    .collect();
                let after: Vec<Real> = (start + LATENCY..start + LATENCY + FRAME)
                    .map(|n| output[n * channels + channel])
                    .collect();
                20.0 * (rms(&after).max(1e-9) / rms(&before).max(1e-9)).log10()
            })
            .collect()
    }

    fn stereo_fixture() -> Vec<Real> {
        let frames = FRAME * 80;
        let voice = speech_like(frames, db_to_linear(-14.0));
        let left_noise = noise_from(0x2545_f491_4f6c_dd1d, frames, 0.02);
        let right_noise = noise_from(0x9e37_79b9_7f4a_7c15, frames, 0.02);
        let mut block = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let hum = (n as Real * std::f32::consts::TAU * 50.0 / FS).sin() * 0.03;
            block.push(voice[n] + left_noise[n] + hum);
            block.push(voice[n] * 0.8 + right_noise[n] + hum);
        }
        block
    }

    #[test]
    fn linked_stereo_applies_the_same_envelope_to_both_sides() {
        // The point of the mode. A voice in the middle with a different noise on each side: two
        // independent networks disagree about what to remove and the image wanders; one mask
        // over both keeps the two sides moving together.
        let input = stereo_fixture();
        let spread = |mode: DenoiseChannelMode| {
            let mut d = with_level(DenoiseLevel::Strong);
            d.set_channels(mode);
            let mut block = input.clone();
            d.process(&mut block, 2);
            let left = frame_gains(&input, &block, 2, 0);
            let right = frame_gains(&input, &block, 2, 1);
            left.iter()
                .zip(&right)
                .map(|(l, r)| (l - r).abs())
                .fold(0.0_f32, Real::max)
        };
        let linked = spread(DenoiseChannelMode::Linked);
        let independent = spread(DenoiseChannelMode::Independent);
        assert!(
            linked < 1.0,
            "linked: the two sides' gains parted by {linked} dB in some frame"
        );
        assert!(
            independent > linked,
            "independent networks should drift more than a shared mask: {independent} against \
             {linked}"
        );
    }

    #[test]
    fn mono_writes_identical_channels() {
        let input = stereo_fixture();
        let mut d = with_level(DenoiseLevel::Medium);
        d.set_channels(DenoiseChannelMode::Mono);
        let mut block = input;
        d.process(&mut block, 2);
        for (n, pair) in block.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(pair[0].to_bits(), pair[1].to_bits(), "frame {n}: {pair:?}");
        }
        assert!(
            rms(&block[LATENCY * 2..]) > 1.0e-3,
            "and something came through"
        );
    }

    #[test]
    fn a_level_change_while_running_does_not_restart_the_bridge() {
        // 0.3.0 restarted the bridge on every toggle: ten milliseconds of silence for changing
        // a number. A level is a row of the control surface and nothing else.
        let mut d = with_level(DenoiseLevel::Medium);
        let mut block = hum_and_hiss(FRAME * 10);
        d.process(&mut block, 1);
        d.set_level(DenoiseLevel::Strong);
        d.set_control(DenoiseLevel::Strong.control());
        d.set_enabled(true);
        let mut block = hum_and_hiss(FRAME * 4);
        d.process(&mut block, 1);
        assert!(
            block[..FRAME].iter().any(|x| x.abs() > 1.0e-6),
            "the bridge was restarted: the first frame after the change is silent"
        );
    }

    #[test]
    fn a_toggle_undone_before_the_next_block_costs_nothing() {
        // `set_enabled` itself never restarts anything: what re-primes the bridge is a block
        // actually processed while the stage stands aside. Two snapshots in the same gap between
        // blocks — off, then on again — are never noticed, and the bridge stays continuous.
        let mut d = with_level(DenoiseLevel::Medium);
        let mut block = hum_and_hiss(FRAME * 10);
        d.process(&mut block, 1);
        d.set_enabled(false);
        d.set_enabled(true);
        let mut block = hum_and_hiss(FRAME * 4);
        d.process(&mut block, 1);
        assert!(
            block[..FRAME].iter().any(|x| x.abs() > 1.0e-6),
            "the bridge was restarted by a toggle that was undone in time"
        );
    }

    #[test]
    fn coming_back_from_standing_aside_starts_from_silence() {
        // The half of the contract that says what the silence is *for*: nothing of the last
        // sentence comes out on the way back in. The test after this one says what it costs.
        let mut d = with_level(DenoiseLevel::Medium);
        let mut block = hum_and_hiss(FRAME * 10);
        d.process(&mut block, 1);
        d.set_enabled(false);
        let mut block = hum_and_hiss(FRAME * 2);
        d.process(&mut block, 1);
        d.set_enabled(true);
        let mut block = vec![0.0; FRAME * 3];
        d.process(&mut block, 1);
        assert!(
            block.iter().all(|x| x.abs() < 1.0e-6),
            "the last sentence was played back on the way in"
        );
    }

    #[test]
    fn coming_back_costs_the_latency_once_and_not_a_sample_more() {
        // The design says `set_enabled` no longer restarts the bridge. The honest reading for
        // off→on is that coming back after standing aside costs exactly the latency — the 960
        // frames the transition has to insert — and nothing beyond it: the bridge's frame of
        // zeros, the library's near-silent reconstruction of its zeroed window, and then the
        // signal, lagging by the latency and by no more. White noise, which the network barely
        // touches, with the VAD pinned high so the control surface does not pull the gains and
        // make "quiet" look like "late".
        let mut d = with_level(DenoiseLevel::Medium);
        d.vad_override = Some(1.0);
        let mut block = noise(FRAME * 10, 0.1);
        d.process(&mut block, 1);
        d.set_enabled(false);
        let mut block = noise(FRAME * 2, 0.1);
        d.process(&mut block, 1);
        d.set_enabled(true);

        let input = noise_from(0x9e37_79b9_7f4a_7c15, FRAME * 14, 0.1);
        let mut block = input.clone();
        d.process(&mut block, 1);

        assert!(
            block[..FRAME].iter().all(|x| *x == 0.0),
            "the bridge came back with something in it"
        );
        // The library's first frame after a reset overlap-adds its zeroed window with nothing:
        // silence but for the leakage of the band gains' filter, well down and not off.
        let level = rms(&input[..FRAME]);
        let leak = rms(&block[FRAME..LATENCY]);
        assert!(
            leak < level * 0.05,
            "the library's first frame carried {leak} against an input of {level}"
        );
        // From the latency on, the signal is there. How much of it is the network's opinion of
        // white noise from a cold start and not this test's concern; what bounds it from below
        // is the control surface, which floors Medium's gains at −24 dB and hands 0.3 of the gap
        // back at the pinned VAD, so even a network that heard nothing but noise leaves at least
        // a third of the level. A fifth is that with margin, and four times the leak bound.
        let after = rms(&block[LATENCY..LATENCY + FRAME]);
        assert!(
            after > level * 0.2,
            "the frame at the latency should carry the signal: {after} against {level}"
        );

        let (mut best_lag, mut best) = (0, Real::MIN);
        let start = LATENCY + FRAME;
        let window = FRAME * 8;
        for lag in (LATENCY - FRAME)..=(LATENCY + FRAME) {
            let score: Real = (start..start + window)
                .map(|n| block[n] * input[n - lag])
                .sum();
            if score > best {
                best = score;
                best_lag = lag;
            }
        }
        assert_eq!(
            best_lag, LATENCY,
            "after coming back the signal lags by {best_lag}, not {LATENCY}"
        );
    }

    #[test]
    fn channels_keep_their_own_network_and_stay_in_step() {
        // Two channels, one loud and one silent. The silent one must come back silent — a shared
        // network would leak one into the other — and both must be delayed by the same frame, or
        // a stereo capture is torn in two.
        let mut d = with_level(DenoiseLevel::Medium);
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
        let mut d = with_level(DenoiseLevel::Medium);
        let input: Vec<Real> = (0..(MAX_CHANNELS + 1) * FRAME * 2)
            .map(|n| (n % 97) as Real / 100.0)
            .collect();
        let mut block = input.clone();
        d.process(&mut block, MAX_CHANNELS + 1);
        assert_eq!(block, input);
    }

    #[test]
    fn a_non_finite_sample_does_not_come_out_the_other_side() {
        let mut d = with_level(DenoiseLevel::Medium);
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

    #[test]
    fn the_meter_reads_the_decibels_actually_taken_away() {
        // The meter is `20·log10(rms_in / rms_out)`. Its first draft computed that from the two
        // energies with the wrong constant and read half the figure, and nothing noticed: every
        // other assertion on the meter is a lower bound. So the stage's own reading is held to
        // the ratio measured from outside, on a stationary floor where the two cannot disagree
        // by more than the meter's smoothing. The input is aligned by the stage's own latency.
        let mut d = with_level(DenoiseLevel::Strong);
        let input = hum_and_hiss(FRAME * 80);
        let mut block = input.clone();
        d.process(&mut block, 1);
        let (from, to) = (FRAME * 40, FRAME * 80);
        let energy = |signal: &[Real]| signal.iter().map(|x| f64::from(x * x)).sum::<f64>();
        let outside = 10.0
            * (energy(&input[from - LATENCY..to - LATENCY]) / energy(&block[from..to])).log10();
        let outside = outside as Real;
        assert!(
            outside > 10.0,
            "premise: Strong takes a floor well down, not {outside} dB"
        );
        // Three decibels of room: the meter averages decibels frame by frame and the outside
        // figure is a ratio of sums, and on a floor whose reduction wanders from frame to frame
        // the two differ by that spread. The halved draft read half of this, not a spread.
        assert!(
            (d.reduction_db() - outside).abs() < 3.0,
            "the meter reads {} dB against {outside} dB measured from outside",
            d.reduction_db()
        );
    }

    #[test]
    fn the_meter_reads_the_reduction_and_nothing_in_silence() {
        let mut d = with_level(DenoiseLevel::Strong);
        let mut block = hum_and_hiss(FRAME * 60);
        d.process(&mut block, 1);
        assert!(
            d.reduction_db() > 10.0,
            "the meter did not see the floor go: {}",
            d.reduction_db()
        );
        // The meter falls with its own time constant — `METER_FRAMES` per e-fold — from
        // whatever it read, plus the two frames still in the bridge. Forty frames of silence
        // was enough when the meter read half the reduction and is not now that it reads all
        // of it, so the budget is derived from the reading rather than guessed.
        let reading = d.reduction_db();
        let frames = ((reading / 0.1).ln() / -(1.0 - 1.0 / METER_FRAMES).ln()).ceil() as usize + 4;
        let mut block = vec![0.0; FRAME * frames];
        d.process(&mut block, 1);
        assert!(
            d.reduction_db() < 0.5,
            "the meter reads {} dB on a muted microphone after {frames} frames",
            d.reduction_db()
        );
    }
}
