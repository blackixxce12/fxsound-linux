//! The boundary between the GUI thread and the real-time audio thread.
//!
//! Two rules shape everything here:
//!
//! 1. **Nothing crossing into the audio thread may allocate, free, lock or block.** That rules out
//!    `Vec`, `String`, `Arc` clones and anything with a non-trivial `Drop`. Every type in this
//!    module is therefore `Copy` with fixed-size arrays, so handing one to the audio thread is a
//!    memcpy into a pre-allocated slot.
//! 2. **Parameters are state, not events.** The GUI publishes a complete [`DspParams`] snapshot
//!    and the audio thread reads the most recent one; a dropped intermediate value is harmless
//!    because the next one supersedes it. Only things that must not be coalesced (a reset, a
//!    preset load that resets filter state) travel as [`DspEvent`]s in a bounded ring.
//!
//! Device switching, preset file IO and anything else that can block happens on the control
//! thread and uses [`UiToAudio`] / [`AudioToUi`], which may allocate freely.

use crate::{
    AudioDevice, AudioStatus, Detection, DeviceDirection, Effect, EqBand, NUM_SPECTRUM_BARS,
    SpectrumFrame, eq,
};

/// A complete, real-time-safe snapshot of everything the DSP engine needs.
///
/// Published by the GUI through a triple buffer and read by the audio thread once per process
/// callback. Deliberately `Copy` and free of heap-owning fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DspParams {
    /// Master bypass. When `false` the engine passes audio through untouched.
    pub power: bool,
    /// The five effect knobs on the engine's `0.0..=1.0` scale, indexed by `Effect as usize`.
    pub effects: [f32; Effect::COUNT],
    /// Whether the graphic equalizer contributes.
    pub eq_on: bool,
    /// How many entries of the band arrays are live.
    pub num_bands: u8,
    /// Per-band centre frequencies in Hz.
    pub band_center_hz: [f32; eq::MAX_BANDS],
    /// Per-band boost/cut in dB.
    pub band_boost_db: [f32; eq::MAX_BANDS],
    /// Multiplier applied to each band's derived Q.
    pub filter_q: f32,
    /// Output gain in dB.
    pub master_gain_db: f32,
    /// Left/right balance; negative is left, positive is right.
    pub balance: f32,
    /// Peak-normalisation target in dB.
    pub normalization_db: f32,
    /// Volume-levelling strength in dB.
    pub volume_leveling_db: f32,
}

impl DspParams {
    /// Overwrite the band tables from a slice, clamping to [`eq::MAX_BANDS`].
    pub fn set_bands(&mut self, bands: &[EqBand]) {
        let n = bands.len().min(eq::MAX_BANDS);
        for (i, band) in bands.iter().take(n).enumerate() {
            self.band_center_hz[i] = band.center_hz;
            self.band_boost_db[i] = band.boost_db;
        }
        self.num_bands = n as u8;
    }

    /// The live bands as a pair of slices, without allocating.
    #[must_use]
    pub fn bands(&self) -> (&[f32], &[f32]) {
        let n = usize::from(self.num_bands).min(eq::MAX_BANDS);
        (&self.band_center_hz[..n], &self.band_boost_db[..n])
    }

    #[inline]
    #[must_use]
    pub fn effect(&self, effect: Effect) -> f32 {
        self.effects[effect as usize]
    }

    #[inline]
    pub fn set_effect(&mut self, effect: Effect, value: f32) {
        self.effects[effect as usize] = value.clamp(0.0, 1.0);
    }

    /// Force every field into a range the DSP can design a filter from.
    ///
    /// Called once on the way to the audio thread, so that no stage downstream has to defend
    /// itself against a value no control could produce. The case that matters is a non-finite
    /// one: `f32::clamp` returns NaN unchanged, and the equality guard in
    /// `GraphicEq::set_q_multiplier` then sees `NaN != NaN`, redesigns every band, and installs
    /// NaN coefficients — from a `filter_q = nan` that a hand-edited `settings.toml` can carry,
    /// since TOML spells that as a plain float.
    ///
    /// Band centres are clamped to the equalizer's own 10 Hz–21 kHz window rather than to the
    /// ladder, because a preset is allowed to carry its own frequencies.
    pub fn sanitise(&mut self) {
        use crate::limits::{self, finite};

        let default = Self::default();
        for (slot, fallback) in self.effects.iter_mut().zip(default.effects) {
            *slot = if slot.is_finite() {
                slot.clamp(0.0, 1.0)
            } else {
                fallback
            };
        }

        self.num_bands = self.num_bands.clamp(1, eq::MAX_BANDS as u8);
        // Only the first `num_bands` entries are live — `bands()` slices to exactly that — so the
        // tail is padding that a default snapshot leaves at zero. Clamping it into the equalizer's
        // frequency window would rewrite a perfectly valid snapshot, so the padding is only
        // checked for finiteness.
        let live = usize::from(self.num_bands).min(eq::MAX_BANDS);
        for (index, slot) in self.band_center_hz.iter_mut().enumerate() {
            *slot = if index < live {
                finite(*slot, 10.0..=21_000.0, 1_000.0)
            } else if slot.is_finite() {
                *slot
            } else {
                0.0
            };
        }
        for (index, slot) in self.band_boost_db.iter_mut().enumerate() {
            *slot = if index < live {
                finite(*slot, eq::MIN_GAIN_DB..=eq::MAX_GAIN_DB, 0.0)
            } else if slot.is_finite() {
                *slot
            } else {
                0.0
            };
        }

        self.filter_q = finite(self.filter_q, limits::FILTER_Q, default.filter_q);
        self.master_gain_db = finite(
            self.master_gain_db,
            limits::MASTER_GAIN_DB,
            default.master_gain_db,
        );
        self.balance = finite(self.balance, limits::BALANCE_DB, default.balance);
        self.normalization_db = finite(
            self.normalization_db,
            limits::NORMALIZATION_DB,
            default.normalization_db,
        );
        self.volume_leveling_db = finite(
            self.volume_leveling_db,
            limits::VOLUME_LEVELING,
            default.volume_leveling_db,
        );
    }
}

impl Default for DspParams {
    fn default() -> Self {
        let mut band_center_hz = [0.0; eq::MAX_BANDS];
        for (slot, &hz) in band_center_hz.iter_mut().zip(eq::DEFAULT_CENTERS_HZ.iter()) {
            *slot = hz;
        }
        Self {
            power: true,
            effects: [0.0; Effect::COUNT],
            eq_on: true,
            num_bands: eq::DEFAULT_BANDS as u8,
            band_center_hz,
            band_boost_db: [0.0; eq::MAX_BANDS],
            filter_q: 1.0,
            master_gain_db: 0.0,
            balance: 0.0,
            normalization_db: 0.0,
            volume_leveling_db: 0.0,
        }
    }
}

/// A complete, real-time-safe snapshot of the microphone chain.
///
/// The sibling of [`DspParams`], and separate from it for the same reason the chains are separate:
/// the two share the ten-band equalizer and nothing else. Folding both into one snapshot would
/// make every output preset carry a gate threshold it has no use for, and the audio thread would
/// have to know which half of its own parameters to ignore.
///
/// Like [`DspParams`] this is `Copy`, fixed-size and free of heap-owning fields, so the existing
/// triple buffer carries it unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputDspParams {
    /// Master bypass. When `false` the chain passes audio through untouched.
    pub power: bool,

    /// High-pass corner in Hz, and its order — `0` for off, `2` or `4`.
    pub highpass_hz: f32,
    pub highpass_order: u8,

    /// Run RNNoise in front of everything else.
    ///
    /// A per-preset field rather than a global switch, because a preset that ignores the denoiser
    /// is describing half its own sound: every threshold below it is a threshold on a *denoised*
    /// level. Whether it then runs also depends on the capture rate — RNNoise exists at 48 kHz
    /// and nowhere else — so a preset asking for it on a device that cannot have it gets a
    /// working chain and an interface that says which stages are running.
    pub rnnoise: bool,

    pub gate_on: bool,
    pub gate_threshold_db: f32,
    pub gate_ratio: f32,
    /// The cap on the gate's attenuation, in dB. Negative.
    ///
    /// The field the design was missing: an expander with no cap pumps the room floor in and out
    /// at the rate of speech, which is more audible than the floor it was hiding.
    pub gate_range_db: f32,
    pub gate_attack_ms: f32,
    pub gate_release_ms: f32,
    pub gate_hold_ms: f32,
    pub gate_detection: Detection,

    /// Whether the ten-band equalizer contributes. Its bands are the same ladder the output side
    /// uses — the same `GraphicEq`, with its own state.
    pub eq_on: bool,
    pub num_bands: u8,
    pub band_center_hz: [f32; eq::MAX_BANDS],
    pub band_boost_db: [f32; eq::MAX_BANDS],
    pub filter_q: f32,

    pub deesser_on: bool,
    pub deesser_hz: f32,
    /// Measured **in the split band**, not in the whole signal, which is why it can sit at −22 dB
    /// without touching a voice that peaks at −6.
    pub deesser_threshold_db: f32,

    pub compressor_on: bool,
    pub compressor_threshold_db: f32,
    pub compressor_ratio: f32,
    pub compressor_knee_db: f32,
    pub compressor_attack_ms: f32,
    pub compressor_release_ms: f32,
    pub compressor_detection: Detection,

    /// Applied after every stage that measures and before the limiter, so that a preset's
    /// thresholds mean what they meant when it was voiced.
    pub makeup_db: f32,
    /// The limiter's ceiling. The limiter itself has no switch: makeup gain is the one control
    /// here that can manufacture a sample above full scale.
    pub ceiling_db: f32,
}

impl InputDspParams {
    /// Overwrite the band tables from a slice, clamping to [`eq::MAX_BANDS`].
    pub fn set_bands(&mut self, bands: &[EqBand]) {
        let n = bands.len().min(eq::MAX_BANDS);
        for (i, band) in bands.iter().take(n).enumerate() {
            self.band_center_hz[i] = band.center_hz;
            self.band_boost_db[i] = band.boost_db;
        }
        self.num_bands = n as u8;
    }

    /// The live bands as a pair of slices, without allocating.
    #[must_use]
    pub fn bands(&self) -> (&[f32], &[f32]) {
        let n = usize::from(self.num_bands).min(eq::MAX_BANDS);
        (&self.band_center_hz[..n], &self.band_boost_db[..n])
    }

    /// Force every field into a range the chain can design a filter from.
    ///
    /// The same contract as [`DspParams::sanitise`], including its deliberate asymmetry: a finite
    /// out-of-range value is clamped because someone meant it, and a non-finite one falls back to
    /// the default because it means nothing at all. Here the case that would hurt most is
    /// `ceiling_db = nan`: clamping it would leave NaN, and a limiter whose ceiling is NaN passes
    /// everything.
    pub fn sanitise(&mut self) {
        use crate::limits::{self, finite};

        let default = Self::default();

        self.highpass_hz = finite(self.highpass_hz, limits::HIGHPASS_HZ, default.highpass_hz);
        self.highpass_order = match self.highpass_order {
            0 => 0,
            1..=3 => 2,
            _ => 4,
        };

        self.gate_threshold_db = finite(
            self.gate_threshold_db,
            limits::GATE_THRESHOLD_DB,
            default.gate_threshold_db,
        );
        self.gate_ratio = finite(self.gate_ratio, limits::GATE_RATIO, default.gate_ratio);
        self.gate_range_db = finite(
            self.gate_range_db,
            limits::GATE_RANGE_DB,
            default.gate_range_db,
        );
        self.gate_attack_ms = finite(
            self.gate_attack_ms,
            limits::ATTACK_MS,
            default.gate_attack_ms,
        );
        self.gate_release_ms = finite(
            self.gate_release_ms,
            limits::RELEASE_MS,
            default.gate_release_ms,
        );
        self.gate_hold_ms = finite(self.gate_hold_ms, limits::RELEASE_MS, default.gate_hold_ms);

        // The band tables follow `DspParams` exactly: only the live entries are clamped into the
        // equalizer's window, because the padding is not a frequency and rewriting it would change
        // a snapshot that was already correct.
        self.num_bands = self.num_bands.clamp(1, eq::MAX_BANDS as u8);
        let live = usize::from(self.num_bands).min(eq::MAX_BANDS);
        for (index, slot) in self.band_center_hz.iter_mut().enumerate() {
            *slot = if index < live {
                finite(*slot, 10.0..=21_000.0, 1_000.0)
            } else if slot.is_finite() {
                *slot
            } else {
                0.0
            };
        }
        for (index, slot) in self.band_boost_db.iter_mut().enumerate() {
            *slot = if index < live {
                finite(*slot, eq::MIN_GAIN_DB..=eq::MAX_GAIN_DB, 0.0)
            } else if slot.is_finite() {
                *slot
            } else {
                0.0
            };
        }
        self.filter_q = finite(self.filter_q, limits::FILTER_Q, default.filter_q);

        self.deesser_hz = finite(self.deesser_hz, limits::DEESSER_HZ, default.deesser_hz);
        self.deesser_threshold_db = finite(
            self.deesser_threshold_db,
            limits::DEESSER_THRESHOLD_DB,
            default.deesser_threshold_db,
        );

        self.compressor_threshold_db = finite(
            self.compressor_threshold_db,
            limits::COMPRESSOR_THRESHOLD_DB,
            default.compressor_threshold_db,
        );
        self.compressor_ratio = finite(
            self.compressor_ratio,
            limits::COMPRESSOR_RATIO,
            default.compressor_ratio,
        );
        self.compressor_knee_db = finite(
            self.compressor_knee_db,
            limits::COMPRESSOR_KNEE_DB,
            default.compressor_knee_db,
        );
        self.compressor_attack_ms = finite(
            self.compressor_attack_ms,
            limits::ATTACK_MS,
            default.compressor_attack_ms,
        );
        self.compressor_release_ms = finite(
            self.compressor_release_ms,
            limits::RELEASE_MS,
            default.compressor_release_ms,
        );

        self.makeup_db = finite(self.makeup_db, limits::MAKEUP_DB, default.makeup_db);
        self.ceiling_db = finite(self.ceiling_db, limits::CEILING_DB, default.ceiling_db);
    }
}

impl Default for InputDspParams {
    /// Clean Voice, which is the reference the rest of the input set was voiced against.
    fn default() -> Self {
        let mut band_center_hz = [0.0; eq::MAX_BANDS];
        for (slot, &hz) in band_center_hz.iter_mut().zip(eq::DEFAULT_CENTERS_HZ.iter()) {
            *slot = hz;
        }
        Self {
            power: true,
            highpass_hz: 80.0,
            highpass_order: 2,
            rnnoise: false,
            gate_on: true,
            gate_threshold_db: -45.0,
            gate_ratio: 2.0,
            gate_range_db: -14.0,
            gate_attack_ms: 5.0,
            gate_release_ms: 150.0,
            gate_hold_ms: 200.0,
            gate_detection: Detection::Rms,
            eq_on: true,
            num_bands: eq::DEFAULT_BANDS as u8,
            band_center_hz,
            band_boost_db: [0.0; eq::MAX_BANDS],
            filter_q: 1.0,
            deesser_on: true,
            deesser_hz: 5_500.0,
            deesser_threshold_db: -22.0,
            compressor_on: true,
            compressor_threshold_db: -18.0,
            compressor_ratio: 3.0,
            compressor_knee_db: 6.0,
            compressor_attack_ms: 20.0,
            compressor_release_ms: 150.0,
            compressor_detection: Detection::Rms,
            makeup_db: 6.0,
            // −3 rather than −1: what leaves here is re-encoded downstream, Opus for a call and
            // AAC for the streaming platforms, and a lossy encoder overshoots the sample peak it
            // was handed.
            ceiling_db: -3.0,
        }
    }
}

/// Things the audio thread must act on exactly once, rather than by reading the latest state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspEvent {
    /// Clear every filter's history — used when a preset changes the band layout.
    ResetFilterState,
    /// Zero the spectrum analyser so the visualizer restarts from silence.
    ResetSpectrum,
    /// Zero the processed-audio-time accumulator.
    ResetProcessedTime,
}

/// What the audio thread publishes for the GUI, once per process callback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meters {
    /// Smoothed per-band magnitudes for the visualizer, each `0.0..=1.0`.
    pub spectrum: SpectrumFrame,
    /// Post-processing peak level of the left channel, `0.0..=1.0`.
    pub peak_left: f32,
    /// Post-processing peak level of the right channel, `0.0..=1.0`.
    pub peak_right: f32,
    /// Samples processed since the last reset, per channel.
    pub processed_samples: u64,
    /// Sample rate the engine is currently running at.
    pub sample_rate: u32,
    /// `true` while the engine is receiving non-silent buffers.
    pub active: bool,
    /// Gain reduction of the three input stages, in dB, as positive numbers.
    ///
    /// Zero in the output direction, which has none of these stages. They live on the shared
    /// snapshot rather than on a second one because the transport carries exactly one meter
    /// structure and splitting it would double the plumbing to save twelve bytes.
    pub gate_reduction_db: f32,
    pub compressor_reduction_db: f32,
    pub deesser_reduction_db: f32,
    /// Whether the two stages that can be *asked for* and still not run are running.
    ///
    /// The de-esser needs a rate that can carry its crossover, and RNNoise exists at 48 kHz and
    /// nowhere else. A preset that asks for either on a device that cannot have it gets a working
    /// chain, and this is how the interface knows to say so rather than leaving the user to
    /// wonder why the sound did not change.
    pub deesser_running: bool,
    pub denoiser_running: bool,
    /// The denoiser's opinion of whether the last frame was voice, `0.0..=1.0`. Zero when it is
    /// not running.
    pub voice_probability: f32,
}

impl Default for Meters {
    fn default() -> Self {
        Self {
            spectrum: [0.0; NUM_SPECTRUM_BARS],
            peak_left: 0.0,
            peak_right: 0.0,
            processed_samples: 0,
            sample_rate: 48_000,
            active: false,
            gate_reduction_db: 0.0,
            compressor_reduction_db: 0.0,
            deesser_reduction_db: 0.0,
            deesser_running: false,
            denoiser_running: false,
            voice_probability: 0.0,
        }
    }
}

/// Control-thread requests. These may allocate and may block; they never reach the RT thread.
#[derive(Debug, Clone, PartialEq)]
pub enum UiToAudio {
    /// Attach FxSound to this device (`node.name`): in front of an output as a virtual sink, or
    /// behind an input as a virtual source. Changing direction tears the nodes down and rebuilds
    /// them the other way round; FxSound runs in one direction at a time.
    SelectDevice {
        node_name: String,
        direction: DeviceDirection,
    },
    /// Re-scan the PipeWire graph for devices.
    RescanDevices,
    /// Make FxSound's virtual device the session default for its direction, or hand it back.
    SetAsDefault(bool),
    /// Tear down and rebuild the PipeWire nodes, e.g. after the server restarted.
    Restart,
    /// Stop the audio engine and let the process exit.
    Shutdown,
}

/// Control-thread notifications for the GUI.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioToUi {
    /// The set of selectable devices changed. Carries both directions; each entry says which.
    Devices(Vec<AudioDevice>),
    /// The engine's connection state or negotiated format changed.
    Status(AudioStatus),
    /// The PipeWire connection dropped; the control thread is retrying.
    Disconnected { reason: String },
    /// Something the user needs to be told about, in already-translated text.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_corrupt_input_snapshot_cannot_reach_a_filter_design() {
        // Every field non-finite at once, which is what a hand-edited settings file or a truncated
        // preset can produce. The contract is the one `DspParams` keeps: a non-finite value is not
        // clamped, it is replaced, because clamping carries an intent the value never had. The
        // case that would hurt most here is the ceiling — a limiter whose ceiling is NaN compares
        // false against everything and passes the lot.
        let mut params = InputDspParams {
            highpass_hz: f32::NAN,
            gate_threshold_db: f32::NEG_INFINITY,
            gate_ratio: f32::NAN,
            gate_range_db: f32::NAN,
            gate_attack_ms: f32::NAN,
            gate_release_ms: f32::NAN,
            gate_hold_ms: f32::NAN,
            filter_q: f32::NAN,
            deesser_hz: f32::NAN,
            deesser_threshold_db: f32::NAN,
            compressor_threshold_db: f32::NAN,
            compressor_ratio: f32::NAN,
            compressor_knee_db: f32::NAN,
            compressor_attack_ms: f32::NAN,
            compressor_release_ms: f32::NAN,
            makeup_db: f32::NAN,
            ceiling_db: f32::NAN,
            band_center_hz: [f32::NAN; eq::MAX_BANDS],
            band_boost_db: [f32::INFINITY; eq::MAX_BANDS],
            ..InputDspParams::default()
        };
        params.sanitise();

        let default = InputDspParams::default();
        assert_eq!(params.ceiling_db, default.ceiling_db);
        assert_eq!(params.gate_ratio, default.gate_ratio);
        assert_eq!(params.highpass_hz, default.highpass_hz);
        assert_eq!(params.makeup_db, default.makeup_db);
        let (centers, boosts) = params.bands();
        assert!(centers.iter().all(|hz| hz.is_finite() && *hz > 0.0));
        assert!(boosts.iter().all(|db| db.is_finite()));
    }

    #[test]
    fn an_out_of_range_input_value_is_clamped_rather_than_replaced() {
        // The other half of the asymmetry: +40 dB of makeup is a number someone meant, just too
        // large, so it becomes the largest allowed rather than the default.
        let mut params = InputDspParams {
            makeup_db: 40.0,
            gate_ratio: 500.0,
            ceiling_db: 6.0,
            ..InputDspParams::default()
        };
        params.sanitise();
        assert_eq!(params.makeup_db, *crate::limits::MAKEUP_DB.end());
        assert_eq!(params.gate_ratio, *crate::limits::GATE_RATIO.end());
        assert_eq!(
            params.ceiling_db, 0.0,
            "a ceiling may never permit clipping"
        );
    }

    #[test]
    fn the_high_pass_order_is_one_of_the_three_the_chain_can_build() {
        for (asked, built) in [(0, 0), (1, 2), (2, 2), (3, 2), (4, 4), (9, 4), (255, 4)] {
            let mut params = InputDspParams {
                highpass_order: asked,
                ..InputDspParams::default()
            };
            params.sanitise();
            assert_eq!(params.highpass_order, built, "order {asked}");
        }
    }

    #[test]
    fn the_padding_past_the_live_bands_is_left_where_it_is() {
        // The same rule `DspParams` follows: only the live entries are frequencies, so clamping
        // the tail into the equalizer's window would rewrite a snapshot that was already correct.
        let mut params = InputDspParams {
            num_bands: 3,
            ..InputDspParams::default()
        };
        params.band_center_hz[7] = 0.0;
        params.sanitise();
        assert_eq!(params.band_center_hz[7], 0.0);
    }
}
