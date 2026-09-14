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
    AudioDevice, AudioStatus, DeviceDirection, Effect, EqBand, NUM_SPECTRUM_BARS, SpectrumFrame,
    eq,
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
}

impl Default for DspParams {
    fn default() -> Self {
        let mut band_center_hz = [0.0; eq::MAX_BANDS];
        for (slot, &hz) in band_center_hz
            .iter_mut()
            .zip(eq::DEFAULT_CENTERS_HZ.iter())
        {
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
