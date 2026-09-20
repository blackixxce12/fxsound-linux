//! Shared vocabulary types for the FxSound Linux port.
//!
//! Every other crate in the workspace depends on this one and on nothing else of ours, so the
//! types here are the contract: the DSP engine, the PipeWire backend, the preset store and the
//! egui front end all agree on the meaning of a value because they all name it from here.
//!
//! Values that exist in the original Windows application keep the original's units and ranges
//! exactly. Where the original carries the same quantity in more than one scale (the effect
//! knobs are stored as MIDI 0..=127, used internally as 0.0..=1.0 and shown on a 0..=10 slider)
//! all three conversions live here so no other crate has to re-derive them.

#![forbid(unsafe_code)]

pub mod atomic;
pub mod i18n;
pub mod messages;
pub mod settings;

pub use messages::{AudioToUi, UiToAudio};
pub use settings::{Settings, ThemeMode, ViewMode};

/// The five user-facing effects, in the order the GUI lays them out.
///
/// This is `DfxDsp::Effect` from `dsp/include/DfxDsp.h:38` — note that it is *not* the order the
/// values are stored in a `.fac` preset file (see [`Effect::vals_index`]) and *not* the order of
/// the per-effect bypass flags in the preset's app-dependent integers (see
/// [`Effect::app_depend_index`]). Confusing the three orderings is the easiest way to write a
/// subtly wrong port, so the mapping is spelled out once, here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Effect {
    Fidelity = 0,
    Ambience = 1,
    Surround = 2,
    DynamicBoost = 3,
    Bass = 4,
}

impl Effect {
    /// All five effects in GUI order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Fidelity,
        Self::Ambience,
        Self::Surround,
        Self::DynamicBoost,
        Self::Bass,
    ];

    pub const COUNT: usize = 5;

    /// Index into the `Main N` lines of a `.fac` preset file.
    ///
    /// The file has six `Main` slots and slot 2 is an unused hole.
    #[inline]
    #[must_use]
    pub const fn vals_index(self) -> usize {
        match self {
            Self::Fidelity => 0,
            Self::Surround => 1,
            // slot 2 is unused
            Self::Ambience => 3,
            Self::DynamicBoost => 4,
            Self::Bass => 5,
        }
    }

    /// Index into the `Integer[N]` bypass flags of a `.fac` preset file (contiguous 0..=4).
    #[inline]
    #[must_use]
    pub const fn app_depend_index(self) -> usize {
        match self {
            Self::Fidelity => 0,
            Self::Surround => 1,
            Self::Ambience => 2,
            Self::DynamicBoost => 3,
            Self::Bass => 4,
        }
    }

    /// The untranslated English label — the `TRANS` key of the Windows build's slider caption
    /// (`FxAudioControls.cpp:94`), so [`crate::i18n::tr`] finds it in every language. The first
    /// effect is "Clarity" there; "Fidelity" is the name the DSP and the preset files still use.
    #[inline]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fidelity => "Clarity",
            Self::Ambience => "Ambience",
            Self::Surround => "Surround Sound",
            Self::DynamicBoost => "Dynamic Boost",
            Self::Bass => "Bass Boost",
        }
    }

    /// The slider's help tip — the `TRANS` key of `FxAudioControls.cpp:157-161`, line break and
    /// all, so the translation tables find it.
    #[inline]
    #[must_use]
    pub const fn tooltip(self) -> &'static str {
        match self {
            Self::Fidelity => "Enhances and elevates high end\nfidelity and presence",
            Self::Ambience => "Thickens and smooths audio\nwith controlled reverberation",
            Self::Surround => "Widens the left-right balance\nfor expansive, wide sound",
            Self::DynamicBoost => {
                "Increases overall volume and balance\nwith responsive processing"
            }
            Self::Bass => "Boosts low end for full,\nimpactful response",
        }
    }

    /// Stable key used in settings files and on the control socket.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Fidelity => "fidelity",
            Self::Ambience => "ambience",
            Self::Surround => "surround",
            Self::DynamicBoost => "dynamic_boost",
            Self::Bass => "bass",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.key() == key)
    }
}

/// Conversions between the three scales the effect knobs live on.
///
/// * file: MIDI integer `0..=127`
/// * engine: `0.0..=1.0`
/// * GUI slider: `0.0..=10.0` in steps of 1
pub mod scale {
    /// Lowest MIDI value a preset may store.
    pub const MIDI_MIN: u8 = 0;
    /// Highest MIDI value a preset may store.
    pub const MIDI_MAX: u8 = 127;
    /// The GUI sliders run 0..=10 in whole steps.
    pub const SLIDER_MAX: f32 = 10.0;

    /// MIDI `0..=127` to the engine's `0.0..=1.0`. `127` maps to exactly `1.0`.
    #[inline]
    #[must_use]
    pub fn midi_to_value(midi: u8) -> f32 {
        f32::from(midi.min(MIDI_MAX)) / f32::from(MIDI_MAX)
    }

    /// Engine `0.0..=1.0` back to MIDI, reproducing the original's round-half-up truncation.
    #[inline]
    #[must_use]
    pub fn value_to_midi(value: f32) -> u8 {
        let scaled = value.clamp(0.0, 1.0) * f32::from(MIDI_MAX) + 0.5;
        // `as` on a non-negative finite f32 truncates, which is what the C++ cast does.
        (scaled as u32).min(u32::from(MIDI_MAX)) as u8
    }

    /// Engine `0.0..=1.0` to the GUI slider's `0.0..=10.0`.
    #[inline]
    #[must_use]
    pub fn value_to_slider(value: f32) -> f32 {
        value.clamp(0.0, 1.0) * SLIDER_MAX
    }

    /// GUI slider `0.0..=10.0` back to the engine's `0.0..=1.0`.
    #[inline]
    #[must_use]
    pub fn slider_to_value(slider: f32) -> f32 {
        (slider / SLIDER_MAX).clamp(0.0, 1.0)
    }

    /// The highest stored value Dynamic Boost's mapping still responds to.
    ///
    /// Two independent ceilings sit in the original's quantiser and only one of them is
    /// deliberate. `PLY_OPTIMIZER_BOOST_MAX_SCALE = 0.7` (`c_play.h:90`) caps the gain table's
    /// index at `(int)(127 × 0.7) = 88`, which is +11.60 dB — a limiter is not meant to be asked
    /// for the table's 30 dB top, so that one reads as intended. The other is an accident:
    /// `DFXP_MUSIC_MODE2_DYNAMIC_BOOST_FACTOR = 1.8` is applied *before* a clamp written to keep a
    /// 128-entry array lookup in range (`dfxpComm.cpp:709-721`), and `f32(1.8 × 70)` rounds to
    /// exactly 126.0 — so every stored value from 70 to 127, all 58 of them, lands on the same
    /// index and the same +11.60 dB.
    ///
    /// The Windows build has the identical dead top: its slider is `setRange(0, 10, 1.0)` for all
    /// five effects with no per-effect case (`FxAudioControls.cpp:113`), and the path from there
    /// to MIDI is plain linear. This is inherited, not introduced.
    pub const DYNAMIC_BOOST_MAX_MIDI: u8 = 70;

    /// Slider position to engine value, for one effect.
    ///
    /// Identical to [`slider_to_value`] for four of the five. Dynamic Boost's eleven positions are
    /// spread over `0..=DYNAMIC_BOOST_MAX_MIDI` instead of the full MIDI range, so that every one
    /// of them is a different amount of gain rather than five of them being the same one.
    ///
    /// **This changes no preset and no sound.** The mapping from a *stored* value to a gain is
    /// untouched, so every `.fac` — this port's, and any imported from Windows — produces exactly
    /// the gain it always did. What changes is only which value the application writes when a user
    /// moves that one slider, and the positions it shows a stored value at.
    #[inline]
    #[must_use]
    pub fn slider_to_value_for(effect: crate::Effect, slider: f32) -> f32 {
        match effect {
            crate::Effect::DynamicBoost => {
                let top = f32::from(DYNAMIC_BOOST_MAX_MIDI) / f32::from(MIDI_MAX);
                (slider / SLIDER_MAX).clamp(0.0, 1.0) * top
            }
            _ => slider_to_value(slider),
        }
    }

    /// The inverse of [`slider_to_value_for`]. A stored value past the dead point shows at the top
    /// of the slider, which is where it actually sounds.
    #[inline]
    #[must_use]
    pub fn value_to_slider_for(effect: crate::Effect, value: f32) -> f32 {
        match effect {
            crate::Effect::DynamicBoost => {
                let top = f32::from(DYNAMIC_BOOST_MAX_MIDI) / f32::from(MIDI_MAX);
                (value.clamp(0.0, 1.0) / top * SLIDER_MAX).min(SLIDER_MAX)
            }
            _ => value_to_slider(value),
        }
    }
}

/// One band of the graphic equalizer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqBand {
    /// Centre frequency in Hz.
    pub center_hz: f32,
    /// Boost or cut in dB.
    pub boost_db: f32,
}

impl EqBand {
    #[must_use]
    pub const fn new(center_hz: f32, boost_db: f32) -> Self {
        Self {
            center_hz,
            boost_db,
        }
    }
}

/// Hard limits on the DSP state that lives outside a preset.
///
/// These are the ranges the GUI sliders and the command line already enforce; they are collected
/// here so that the settings file and the real-time snapshot can be held to the same numbers.
/// Without a single source of truth the three disagree, and the one path that skips validation —
/// a hand-edited `settings.toml`, where TOML happily spells `nan` and `inf` — reaches the filter
/// designs with a value no slider could ever produce.
pub mod limits {
    /// `--master_gain`, and the Pro view's gain slider.
    pub const MASTER_GAIN_DB: std::ops::RangeInclusive<f32> = -20.0..=20.0;
    /// `--balance`. Negative is left.
    pub const BALANCE_DB: std::ops::RangeInclusive<f32> = -20.0..=20.0;
    /// `--volume_leveling`. An abstract amount, not decibels, despite the original's field name.
    pub const VOLUME_LEVELING: std::ops::RangeInclusive<f32> = 0.0..=4.0;
    /// `--filter_q`, the multiplier applied to each band's derived Q.
    pub const FILTER_Q: std::ops::RangeInclusive<f32> = 1.0..=3.0;
    /// Peak-normalisation target.
    pub const NORMALIZATION_DB: std::ops::RangeInclusive<f32> = -20.0..=0.0;

    // ---- the input chain ----
    //
    // Each of these is clamped again inside the stage that uses it, which is not redundancy: a
    // stage is entitled to refuse a number that would break its own design whoever sent it, and
    // these ranges exist so that a snapshot on its way to the audio thread is already sane. The
    // ranges are wider than any shipped preset, because a preset is a starting point and the user
    // is allowed past it.

    /// The high-pass corner. Below 20 Hz there is nothing to remove; above 300 Hz it is no longer
    /// a rumble filter but a tone control.
    pub const HIGHPASS_HZ: std::ops::RangeInclusive<f32> = 20.0..=300.0;
    /// Gate threshold, in dBFS of whatever [`crate::Detection`] selected.
    pub const GATE_THRESHOLD_DB: std::ops::RangeInclusive<f32> = -90.0..=0.0;
    /// Gate ratio. `1.0` is a straight wire; past 20 it is a switch, which this stage is not.
    pub const GATE_RATIO: std::ops::RangeInclusive<f32> = 1.0..=20.0;
    /// The cap on the gate's attenuation. Zero switches the stage off without a second flag.
    pub const GATE_RANGE_DB: std::ops::RangeInclusive<f32> = -90.0..=0.0;
    /// Compressor threshold, in dBFS.
    pub const COMPRESSOR_THRESHOLD_DB: std::ops::RangeInclusive<f32> = -60.0..=0.0;
    /// Compressor ratio. Past 60 the look-ahead limiter is the better tool.
    pub const COMPRESSOR_RATIO: std::ops::RangeInclusive<f32> = 1.0..=60.0;
    /// Width of the compressor's knee, centred on the threshold.
    pub const COMPRESSOR_KNEE_DB: std::ops::RangeInclusive<f32> = 0.0..=24.0;
    /// Where the de-esser splits the band. Whether it *can* be built there depends on the capture
    /// rate — see `fxsound_dsp::input::MAX_CORNER_FRACTION`.
    pub const DEESSER_HZ: std::ops::RangeInclusive<f32> = 1_000.0..=12_000.0;
    /// De-esser threshold, measured in the split band rather than in the whole signal.
    pub const DEESSER_THRESHOLD_DB: std::ops::RangeInclusive<f32> = -60.0..=0.0;
    /// Gain after every stage that measures and before the limiter.
    pub const MAKEUP_DB: std::ops::RangeInclusive<f32> = -24.0..=24.0;
    /// The level the chain's output may never exceed. Never above 0: a ceiling that permits full
    /// scale is not a ceiling.
    pub const CEILING_DB: std::ops::RangeInclusive<f32> = -24.0..=0.0;
    /// Any attack time in the chain.
    pub const ATTACK_MS: std::ops::RangeInclusive<f32> = 0.0..=200.0;
    /// Any release or hold time in the chain.
    pub const RELEASE_MS: std::ops::RangeInclusive<f32> = 0.0..=2_000.0;

    /// Clamp into `range`, and substitute `fallback` for a value that is not a number at all.
    ///
    /// `f32::clamp` propagates NaN, so it cannot be used on its own here: the point of this
    /// function is that NaN never survives it.
    ///
    /// The two cases are treated differently on purpose. A finite value outside the range is a
    /// value someone meant, just too large, so it is clamped. A non-finite one carries no
    /// intent at all and falls back to the default — clamping `+inf` would hand a user whose
    /// settings file was corrupted the *maximum* master gain, which is the worst possible
    /// reading of a value that means nothing.
    #[must_use]
    pub fn finite(value: f32, range: std::ops::RangeInclusive<f32>, fallback: f32) -> f32 {
        if value.is_finite() {
            value.clamp(*range.start(), *range.end())
        } else {
            fallback
        }
    }
}

/// Hard limits the equalizer enforces, taken from the original engine and GUI.
pub mod eq {
    use super::EqBand;

    /// `GRAPHIC_EQ_MAX_NUM_BANDS` / `SOS_MAX_NUM_SOS_SECTIONS`.
    pub const MAX_BANDS: usize = 32;
    /// What every shipped preset uses and what the GUI draws.
    pub const DEFAULT_BANDS: usize = 10;
    /// `FxEqualizer::MAX_GAIN` — the slider range is symmetric around 0 dB.
    pub const MAX_GAIN_DB: f32 = 12.0;
    pub const MIN_GAIN_DB: f32 = -MAX_GAIN_DB;

    /// The engine's own ten-band ladder (`GraphicEqSet.cpp:441-455`), used when a preset does not
    /// carry its own frequencies — which is any preset older than format version 9.
    ///
    /// Deliberately not ISO-266: the source comment says the factory and community presets were
    /// authored against this grid. Presets that do carry frequencies (all the shipped ones do)
    /// override it band for band, which is why the stock presets show 115 Hz where this says
    /// 115.734.
    pub const DEFAULT_CENTERS_HZ: [f32; DEFAULT_BANDS] = [
        62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48, 16000.0,
    ];

    /// A flat ten-band curve at the default centre frequencies.
    #[must_use]
    pub fn default_bands() -> Vec<EqBand> {
        DEFAULT_CENTERS_HZ
            .iter()
            .map(|&hz| EqBand::new(hz, 0.0))
            .collect()
    }
}

/// Everything a `.fac` preset carries, in the file's own units.
///
/// Fields the shipping application reads and discards are still kept so that a preset can be
/// written back out byte-for-byte identically.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    /// Display name. In the file this is a bare line; on disk it is also the file stem.
    pub name: String,
    /// File format version (`9` in every shipped preset).
    pub version: f32,
    /// The six `Main` slots as raw MIDI values, slot 2 unused.
    pub main_midi: [u8; 6],
    /// The seven app-dependent integers: five bypass flags, headphone mode, music mode.
    pub app_ints: [i32; 7],
    /// The single element's seven parameters — always zero, round-tripped verbatim.
    pub element_params: [i32; 7],
    /// Equalizer curve.
    pub eq_bands: Vec<EqBand>,
    /// Whether the equalizer block is marked on.
    pub eq_on: bool,
}

impl Preset {
    /// Read one effect's knob value on the engine's `0.0..=1.0` scale.
    #[inline]
    #[must_use]
    pub fn effect(&self, effect: Effect) -> f32 {
        scale::midi_to_value(self.main_midi[effect.vals_index()])
    }

    /// Write one effect's knob value from the engine's `0.0..=1.0` scale.
    #[inline]
    pub fn set_effect(&mut self, effect: Effect, value: f32) {
        self.main_midi[effect.vals_index()] = scale::value_to_midi(value);
        // The original forces the bypass flag to follow the value on every set, so a preset
        // saved by this port matches one saved by the Windows build.
        self.app_ints[effect.app_depend_index()] = i32::from(value != 0.0);
    }

    /// `true` when the effect contributes to the signal, which the shipping app derives from the
    /// value rather than from the stored bypass flag.
    #[inline]
    #[must_use]
    pub fn is_effect_on(&self, effect: Effect) -> bool {
        self.main_midi[effect.vals_index()] != 0
    }
}

impl Default for Preset {
    fn default() -> Self {
        Self {
            name: String::from("Unnamed"),
            version: 9.0,
            main_midi: [0; 6],
            // music mode 2 is the only one the engine honours
            app_ints: [0, 0, 0, 0, 0, 0, 2],
            element_params: [0; 7],
            eq_bands: eq::default_bands(),
            eq_on: true,
        }
    }
}

/// Which quantity a dynamics stage compares its threshold against.
///
/// This lives in the shared vocabulary rather than inside the DSP because it is part of what a
/// preset *says*, not how a stage is built. **Peak and RMS detection of the same signal against
/// the same threshold differ by three to seven decibels of gain reduction**, so a voice preset
/// that records "threshold −18 dB, ratio 3:1" without recording this has recorded two different
/// sounds. Every threshold in the shipped input set is an RMS threshold; say so wherever they are
/// written down.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Detection {
    /// The rectified sample. Catches every transient, so a plosive or a keyboard strike reaches
    /// the threshold even when the programme is quiet. What a limiter wants.
    Peak,
    /// A short running mean of the square. Tracks how loud the voice *sounds* rather than how tall
    /// its tallest sample is, which is what makes a compressor even out delivery instead of
    /// chasing consonants.
    #[default]
    Rms,
}

/// Which way audio flows through a device FxSound can attach to.
///
/// The Windows build only ever sat in front of a *playback* endpoint. The Linux port can also sit
/// behind a *capture* device — a microphone — and publish the processed signal as a virtual
/// source, so a device is one or the other and FxSound runs in exactly one direction at a time.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum DeviceDirection {
    /// A playback device: FxSound is a virtual sink in front of it.
    #[default]
    Output,
    /// A capture device: FxSound is a virtual source fed by it.
    Input,
}

impl DeviceDirection {
    /// The English word the UI uses for the section header and the tray tooltip.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Output => "Output",
            Self::Input => "Input",
        }
    }
}

/// A device the user can pick for FxSound to attach to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// PipeWire node id — not stable across restarts.
    pub id: u32,
    /// `node.name`, stable, what settings persist.
    pub name: String,
    /// `node.description`, what the combo box shows.
    pub description: String,
    /// Whether this is the session's current default for its direction.
    pub is_default: bool,
    /// Playback device or capture device.
    pub direction: DeviceDirection,
    /// What kind of device this is — `speaker`, `headphone`, `hdmi`, and so on.
    ///
    /// Derived from PipeWire's `device.form-factor`, `device.icon-name` and `device.bus`. Carried
    /// here so the application layer can record it against a device the user has chosen; the
    /// backend is the only place that can work it out, and it does not outlive the device list.
    pub form_factor: String,
}

/// Live state the audio engine publishes for the GUI to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioStatus {
    /// `true` while buffers are actually flowing.
    pub processing: bool,
    /// Sample rate currently negotiated with PipeWire.
    pub sample_rate: u32,
    /// Channel count currently negotiated.
    pub channels: u16,
    /// Seconds of audio processed since the counter was last reset.
    pub processed_secs: u64,

    // ---- how the ring between the two nodes is coping ------------------------------------
    //
    // Collected correctly since the port began and then thrown away: they crossed no process
    // boundary and appeared in no message, so nothing could assert on them and nobody could see
    // them. All three are cumulative since the stream was built.
    /// Frames the producer had to throw away because the consumer had stalled.
    pub dropped_frames: u64,
    /// Frames of silence the consumer was handed because the ring had run dry.
    pub underrun_frames: u64,
    /// Times the ring was re-primed after running dry — each one is an audible gap.
    pub resyncs: u64,
    /// Times the two nodes negotiated different formats, which mutes audio until the supervisor
    /// rebuilds them.
    pub format_mismatches: u64,
}

impl Default for AudioStatus {
    fn default() -> Self {
        Self {
            processing: false,
            sample_rate: 48_000,
            channels: 2,
            processed_secs: 0,
            dropped_frames: 0,
            underrun_frames: 0,
            resyncs: 0,
            format_mismatches: 0,
        }
    }
}

/// Number of spectrum bars the visualizer draws (`FxVisualizer::NUM_BARS`).
pub const NUM_SPECTRUM_BARS: usize = 10;

/// One frame of spectrum data, sized for the visualizer.
pub type SpectrumFrame = [f32; NUM_SPECTRUM_BARS];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_round_trips_for_every_stored_value() {
        for m in scale::MIDI_MIN..=scale::MIDI_MAX {
            let v = scale::midi_to_value(m);
            assert_eq!(scale::value_to_midi(v), m, "midi {m} did not round trip");
        }
    }

    #[test]
    fn midi_endpoints_are_exact() {
        assert_eq!(scale::midi_to_value(0), 0.0);
        assert_eq!(scale::midi_to_value(127), 1.0);
        assert_eq!(scale::value_to_midi(1.0), 127);
        assert_eq!(scale::value_to_midi(0.0), 0);
    }

    #[test]
    fn slider_scale_is_a_clean_factor_of_ten() {
        assert_eq!(scale::value_to_slider(1.0), 10.0);
        assert_eq!(scale::slider_to_value(10.0), 1.0);
        assert_eq!(scale::slider_to_value(5.0), 0.5);
    }

    #[test]
    fn the_three_orderings_do_not_collide() {
        let mut vals: Vec<_> = Effect::ALL.iter().map(|e| e.vals_index()).collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![0, 1, 3, 4, 5], "Main 2 must stay a hole");

        let mut app: Vec<_> = Effect::ALL.iter().map(|e| e.app_depend_index()).collect();
        app.sort_unstable();
        assert_eq!(app, vec![0, 1, 2, 3, 4], "bypass flags are contiguous");
    }

    #[test]
    fn setting_an_effect_updates_its_bypass_flag() {
        let mut p = Preset::default();
        p.set_effect(Effect::Bass, 0.5);
        assert_eq!(p.app_ints[Effect::Bass.app_depend_index()], 1);
        assert!(p.is_effect_on(Effect::Bass));
        p.set_effect(Effect::Bass, 0.0);
        assert_eq!(p.app_ints[Effect::Bass.app_depend_index()], 0);
        assert!(!p.is_effect_on(Effect::Bass));
    }

    #[test]
    fn sanitising_a_snapshot_leaves_nothing_non_finite() {
        use crate::messages::DspParams;

        let mut params = DspParams {
            filter_q: f32::NAN,
            master_gain_db: f32::INFINITY,
            balance: f32::NEG_INFINITY,
            volume_leveling_db: f32::NAN,
            normalization_db: f32::NAN,
            num_bands: 200,
            ..DspParams::default()
        };
        params.effects[0] = f32::NAN;
        params.band_boost_db[0] = f32::INFINITY;
        params.band_center_hz[0] = f32::NAN;

        params.sanitise();

        let default = DspParams::default();
        assert_eq!(params.filter_q, default.filter_q, "NaN survives f32::clamp");
        assert_eq!(params.master_gain_db, default.master_gain_db);
        assert_eq!(params.balance, default.balance);
        assert_eq!(params.volume_leveling_db, default.volume_leveling_db);
        assert_eq!(params.effects[0], default.effects[0]);
        assert_eq!(params.band_boost_db[0], 0.0);
        assert!(params.band_center_hz[0].is_finite());
        assert_eq!(usize::from(params.num_bands), eq::MAX_BANDS);
        assert!(
            params.effects.iter().all(|v| v.is_finite())
                && params.band_boost_db.iter().all(|v| v.is_finite())
                && params.band_center_hz.iter().all(|v| v.is_finite())
        );
    }

    #[test]
    fn clamping_a_finite_value_is_left_alone() {
        use crate::messages::DspParams;

        let before = DspParams {
            filter_q: 2.0,
            master_gain_db: -6.0,
            balance: 3.0,
            volume_leveling_db: 1.5,
            ..DspParams::default()
        };
        let mut after = before;
        after.sanitise();
        assert_eq!(
            after, before,
            "sanitising must be a no-op on a valid snapshot"
        );
    }

    #[test]
    fn a_finite_value_outside_its_range_is_clamped_rather_than_defaulted() {
        use crate::messages::DspParams;

        let mut params = DspParams {
            master_gain_db: 500.0,
            balance: -99.0,
            filter_q: 0.2,
            volume_leveling_db: 12.0,
            ..DspParams::default()
        };
        params.band_boost_db[0] = 40.0;
        params.sanitise();

        assert_eq!(params.master_gain_db, 20.0);
        assert_eq!(params.balance, -20.0);
        assert_eq!(params.filter_q, 1.0);
        assert_eq!(params.volume_leveling_db, 4.0);
        assert_eq!(params.band_boost_db[0], eq::MAX_GAIN_DB);
    }
}
