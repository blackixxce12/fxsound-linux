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

pub mod apps;
pub mod atomic;
pub mod i18n;
pub mod messages;
pub mod parity;
pub mod settings;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use apps::{AppKey, AppPreset, AppRule, AppRules, MAX_ROUTES_PER_LANE};
pub use messages::{
    AppRoute, AppStream, AudioToUi, DspCompat, RouteParams, TargetVolume, UiToAudio,
};
pub use parity::{LaterFeature, ParityClass, WindowsLook, WindowsParity};
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

    /// The stored value Ambience's first slider position is saved as: the first one it can be
    /// heard at (audit report #39).
    ///
    /// The stage runs from 13 up, as the original's does, but the MUSIC2 warp only gives the
    /// reverb a real wet level from `(int)(midi × 0.34) > 12`, which 39 is the first to reach;
    /// from 13 to 38 the wet level only ramps towards it, and the tail of a −6 dBFS tone at the old
    /// positions 1, 2 and 3 (stored 13, 25, 38) played at −73, −51 and −45 dBFS against −31 at
    /// position 4. So the slider's first position is 39 and its ten positions are spread from
    /// there to 127, as Dynamic Boost's are spread below its dead top.
    ///
    /// FxSound for Linux's mapping. At «Like FxSound for Windows» = Interface and sound the
    /// Windows build's straight line comes back with the Windows stage ([`slider_to_value_in`]).
    pub const AMBIENCE_FIRST_AUDIBLE_MIDI: u8 = 39;

    /// Slider position to engine value, for one effect, as FxSound for Linux maps it:
    /// [`slider_to_value_in`] with [`DspCompat::Linux`](crate::DspCompat::Linux).
    #[inline]
    #[must_use]
    pub fn slider_to_value_for(effect: crate::Effect, slider: f32) -> f32 {
        slider_to_value_in(crate::DspCompat::Linux, effect, slider)
    }

    /// Slider position to engine value, for one effect, in the DSP `compat` plays.
    ///
    /// Identical to [`slider_to_value`] for three of the five. The other two spread their eleven
    /// positions over the stored values that sound different, so that every position is a
    /// different sound rather than several of them being the same one:
    ///
    /// - **Dynamic Boost** over `0..=DYNAMIC_BOOST_MAX_MIDI`, below its dead top, whatever
    ///   `compat` says: the Windows build's slider walks into the dead top, and nothing but the
    ///   position a value shows at changes by keeping it out.
    /// - **Ambience** (audit report #39), in FxSound for Linux: position 0 is off, positions 1 to
    ///   10 run from [`AMBIENCE_FIRST_AUDIBLE_MIDI`] to 127, and a position between 0 and 1 — which
    ///   only a stored value can give — reads straight from 0 to 39, so a Windows preset storing
    ///   13..38, all but silent, shows below position 1 where it sounds. At «Like FxSound for
    ///   Windows» = Interface and sound ([`DspCompat::Windows`](crate::DspCompat::Windows)) the
    ///   Windows build's straight line comes back with its stage (`FxAudioControls.cpp:113`
    ///   sets every effect slider to `0..10` in whole steps, and the path to MIDI is linear):
    ///   positions 1, 2 and 3 store 13, 25 and 38 again, which that stage plays as the Windows
    ///   build does.
    ///
    /// **The mapping changes no preset.** The mapping from a *stored* value to a gain is the
    /// stage's, so every `.fac` — this port's, and any imported from Windows — is read as it
    /// is. What changes is only which value the application writes when a user moves one of
    /// these sliders, and the positions it shows a stored value at.
    #[inline]
    #[must_use]
    pub fn slider_to_value_in(compat: crate::DspCompat, effect: crate::Effect, slider: f32) -> f32 {
        let slider = if slider.is_nan() {
            0.0
        } else {
            slider.clamp(0.0, SLIDER_MAX)
        };
        match effect {
            crate::Effect::DynamicBoost => {
                let top = f32::from(DYNAMIC_BOOST_MAX_MIDI) / f32::from(MIDI_MAX);
                slider / SLIDER_MAX * top
            }
            crate::Effect::Ambience if !compat.windows() => {
                let first = f32::from(AMBIENCE_FIRST_AUDIBLE_MIDI);
                let midi = if slider <= 1.0 {
                    slider * first
                } else {
                    first + (slider - 1.0) * (f32::from(MIDI_MAX) - first) / (SLIDER_MAX - 1.0)
                };
                (midi / f32::from(MIDI_MAX)).clamp(0.0, 1.0)
            }
            _ => slider_to_value(slider),
        }
    }

    /// The inverse of [`slider_to_value_for`]. A Dynamic Boost stored past the dead point shows at
    /// the top of the slider, which is where it actually sounds.
    #[inline]
    #[must_use]
    pub fn value_to_slider_for(effect: crate::Effect, value: f32) -> f32 {
        value_to_slider_in(crate::DspCompat::Linux, effect, value)
    }

    /// The inverse of [`slider_to_value_in`].
    #[inline]
    #[must_use]
    pub fn value_to_slider_in(compat: crate::DspCompat, effect: crate::Effect, value: f32) -> f32 {
        let value = if value.is_nan() {
            0.0
        } else {
            value.clamp(0.0, 1.0)
        };
        match effect {
            crate::Effect::DynamicBoost => {
                let top = f32::from(DYNAMIC_BOOST_MAX_MIDI) / f32::from(MIDI_MAX);
                (value / top * SLIDER_MAX).min(SLIDER_MAX)
            }
            crate::Effect::Ambience if !compat.windows() => {
                let first = f32::from(AMBIENCE_FIRST_AUDIBLE_MIDI);
                let midi = value * f32::from(MIDI_MAX);
                let slider = if midi <= first {
                    midi / first
                } else {
                    1.0 + (midi - first) * (SLIDER_MAX - 1.0) / (f32::from(MIDI_MAX) - first)
                };
                slider.clamp(0.0, SLIDER_MAX)
            }
            _ => value_to_slider(value),
        }
    }

    /// The stored value a slider position of `effect` is saved as.
    #[inline]
    #[must_use]
    pub fn slider_to_midi_for(effect: crate::Effect, slider: f32) -> u8 {
        slider_to_midi_in(crate::DspCompat::Linux, effect, slider)
    }

    /// [`slider_to_midi_for`] in the DSP `compat` plays ([`slider_to_value_in`]).
    #[inline]
    #[must_use]
    pub fn slider_to_midi_in(compat: crate::DspCompat, effect: crate::Effect, slider: f32) -> u8 {
        value_to_midi(slider_to_value_in(compat, effect, slider))
    }

    /// The slider position a stored value of `effect` shows at.
    #[inline]
    #[must_use]
    pub fn midi_to_slider_for(effect: crate::Effect, midi: u8) -> f32 {
        midi_to_slider_in(crate::DspCompat::Linux, effect, midi)
    }

    /// [`midi_to_slider_for`] in the DSP `compat` plays ([`value_to_slider_in`]).
    #[inline]
    #[must_use]
    pub fn midi_to_slider_in(compat: crate::DspCompat, effect: crate::Effect, midi: u8) -> f32 {
        value_to_slider_in(compat, effect, midi_to_value(midi))
    }

    /// The whole position `slider` saves as the same value as, if there is one: what the slider
    /// shows as a whole number (audit report #14).
    ///
    /// A whole position is saved as the nearest stored value, so it comes back from a preset a
    /// hair off — position 1 of Fidelity is saved as 13 and shows at 1.024 — and that is still
    /// position 1. A stored value no whole position is saved as, 20 for Surround, is not a
    /// position at all, and the slider shows it with its decimal (1.6) rather than rounding it to
    /// a position (2) that would save as something else (25).
    #[must_use]
    pub fn whole_position_for(effect: crate::Effect, slider: f32) -> Option<f32> {
        whole_position_in(crate::DspCompat::Linux, effect, slider)
    }

    /// [`whole_position_for`] in the DSP `compat` plays.
    #[must_use]
    pub fn whole_position_in(
        compat: crate::DspCompat,
        effect: crate::Effect,
        slider: f32,
    ) -> Option<f32> {
        let whole = slider.round().clamp(0.0, SLIDER_MAX);
        (slider_to_midi_in(compat, effect, whole) == slider_to_midi_in(compat, effect, slider))
            .then_some(whole)
    }

    /// The slider position of the stored value one step from `slider`, up or down: the fine step
    /// that reaches every value a preset can store (audit report #14). Values that show at the
    /// same position — Dynamic Boost's past its dead top — are stepped over; at either end the
    /// position stays where it is.
    #[must_use]
    pub fn stored_step_for(effect: crate::Effect, slider: f32, up: bool) -> f32 {
        stored_step_in(crate::DspCompat::Linux, effect, slider, up)
    }

    /// [`stored_step_for`] in the DSP `compat` plays.
    #[must_use]
    pub fn stored_step_in(
        compat: crate::DspCompat,
        effect: crate::Effect,
        slider: f32,
        up: bool,
    ) -> f32 {
        let here = midi_to_slider_in(compat, effect, slider_to_midi_in(compat, effect, slider));
        let mut midi = slider_to_midi_in(compat, effect, slider);
        loop {
            midi = match (up, midi) {
                (true, MIDI_MAX) | (false, MIDI_MIN) => return here,
                (true, m) => m + 1,
                (false, m) => m - 1,
            };
            let there = midi_to_slider_in(compat, effect, midi);
            if (up && there > here) || (!up && there < here) {
                return there;
            }
        }
    }

    /// The slider position of the stored value nearest `slider`, for a gesture that asks for
    /// exact steps rather than whole positions.
    #[inline]
    #[must_use]
    pub fn nearest_stored_position_for(effect: crate::Effect, slider: f32) -> f32 {
        nearest_stored_position_in(crate::DspCompat::Linux, effect, slider)
    }

    /// [`nearest_stored_position_for`] in the DSP `compat` plays.
    #[inline]
    #[must_use]
    pub fn nearest_stored_position_in(
        compat: crate::DspCompat,
        effect: crate::Effect,
        slider: f32,
    ) -> f32 {
        midi_to_slider_in(compat, effect, slider_to_midi_in(compat, effect, slider))
    }

    /// The effect slider's readout: a whole number at a whole position, and one decimal
    /// otherwise, so a stored value between positions is never shown as a position it is not
    /// (audit report #14).
    #[must_use]
    pub fn slider_label_for(effect: crate::Effect, slider: f32) -> String {
        slider_label_in(crate::DspCompat::Linux, effect, slider)
    }

    /// [`slider_label_for`] in the DSP `compat` plays.
    #[must_use]
    pub fn slider_label_in(compat: crate::DspCompat, effect: crate::Effect, slider: f32) -> String {
        match whole_position_in(compat, effect, slider) {
            Some(whole) => format!("{whole:.0}"),
            None => format!("{slider:.1}"),
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

    // ---- the denoiser's control surface ----
    //
    // The four numbers behind a [`crate::DenoiseLevel`]. A preset may override the level's row,
    // which is the one route by which a hand-typed value reaches them, so they are held to the
    // same ranges here that the level table itself stays inside.

    /// How far below unity a band's gain may fall, in dB of suppression. `0` is a straight wire;
    /// past 80 dB the floor sits below the network's own numerical noise and buys nothing.
    pub const DENOISE_MAX_SUPPRESSION_DB: std::ops::RangeInclusive<f32> = 0.0..=80.0;
    /// The voice probability below which a frame is treated as noise. A probability.
    pub const DENOISE_VAD_THRESHOLD: std::ops::RangeInclusive<f32> = 0.0..=1.0;
    /// How much of the gap between a band's gain and unity is given back in proportion to the
    /// voice probability. A fraction.
    pub const DENOISE_VOICE_PRESERVATION: std::ops::RangeInclusive<f32> = 0.0..=1.0;
    /// The wet/dry mix; `1` is fully denoised. A fraction.
    pub const DENOISE_WET_DRY: std::ops::RangeInclusive<f32> = 0.0..=1.0;

    // ---- per-target volume ----

    /// One remembered `channelVolumes` entry of FxSound's own node, as the linear amplitude
    /// PipeWire's `Props` carry. The top is +12 dB: a little above the +11 dB pavucontrol's
    /// slider stops at, so whatever a mixer's slider can set survives. A larger number — `pactl`
    /// can ask for one, and so can a hand edit — is clamped rather than replayed as it is,
    /// because it lands on the node the user is listening through.
    pub const TARGET_VOLUME: std::ops::RangeInclusive<f32> = 0.0..=4.0;
    /// The most channels a remembered volume keeps: `SPA_AUDIO_MAX_CHANNELS`, the most a `Props`
    /// object can carry. Past it an entry is a hand edit, and the tail goes rather than the whole
    /// entry.
    pub const TARGET_VOLUME_CHANNELS: usize = 64;

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

    /// The twenty-band ladder since 0.4.0: half an octave apart from 20 Hz to 16 kHz (audit report
    /// R4). `fxsound_dsp::eq::band_table(20)` hands these out; they live here so that the preset
    /// store, which does not depend on the engine, can move a curve between them and
    /// [`WINDOWS_TWENTY_BAND_CENTRES_HZ`].
    pub const TWENTY_BAND_CENTRES_HZ: [f32; 20] = [
        20.0, 28.4331, 40.4221, 57.4662, 81.6971, 116.145, 165.118, 234.741, 333.721, 474.436,
        674.485, 958.885, 1363.2, 1938.0, 2755.17, 3916.91, 5568.49, 7916.47, 11254.5, 16000.0,
    ];

    /// The Windows build's twenty-band ladder (`GraphicEqSet.cpp:468-478`): octave bands from
    /// 31.5 Hz with the third-octave band above each, so they come in pairs. Twenty-band curves
    /// saved before 0.4.0 and Windows twenty-band presets carry these centres.
    pub const WINDOWS_TWENTY_BAND_CENTRES_HZ: [f32; 20] = [
        20.0, 31.5, 40.0, 63.0, 80.0, 125.0, 160.0, 250.0, 315.0, 500.0, 630.0, 1000.0, 1250.0,
        2000.0, 2500.0, 4000.0, 5000.0, 8000.0, 10000.0, 16000.0,
    ];

    /// How close a centre has to be to a ladder's to count as on it: the `%g` a `.fac` stores
    /// centres with keeps six figures, so 0.01 Hz separates a ladder from a band someone moved.
    const LADDER_TOLERANCE_HZ: f32 = 0.01;

    fn on_ladder(bands: &[EqBand], ladder: &[f32]) -> bool {
        bands.len() == ladder.len()
            && bands
                .iter()
                .zip(ladder)
                .all(|(band, &hz)| (band.center_hz - hz).abs() <= LADDER_TOLERANCE_HZ)
    }

    fn move_to(bands: &mut [EqBand], ladder: &[f32]) {
        for (band, &hz) in bands.iter_mut().zip(ladder) {
            band.center_hz = hz;
        }
    }

    /// Move a twenty-band curve on the Windows ladder to the half-octave one, band for band, and
    /// say whether it moved (audit report R4).
    ///
    /// The gains stay where they are, so every boost and cut keeps its band: no centre moves by
    /// more than 0.171 of an octave, and the 3.9 dB of ripple the paired ladder gave a broad boost
    /// goes. A curve on any other centres — a band someone dragged, another count — is left alone.
    /// Wherever a curve reaches the application: a preset's own file, its autosave, an import, a
    /// voice preset saved by 0.3.0.
    pub fn move_off_the_windows_twenty_band_ladder(bands: &mut [EqBand]) -> bool {
        if on_ladder(bands, &WINDOWS_TWENTY_BAND_CENTRES_HZ) {
            move_to(bands, &TWENTY_BAND_CENTRES_HZ);
            true
        } else {
            false
        }
    }

    /// The other way, for a `.fac` written for someone else to open: a twenty-band curve on the
    /// half-octave ladder is written on the Windows one, band for band, which is what the Windows
    /// build draws and tunes twenty bands to. Anything else is left alone. Says whether it moved.
    pub fn move_onto_the_windows_twenty_band_ladder(bands: &mut [EqBand]) -> bool {
        if on_ladder(bands, &TWENTY_BAND_CENTRES_HZ) {
            move_to(bands, &WINDOWS_TWENTY_BAND_CENTRES_HZ);
            true
        } else {
            false
        }
    }

    /// Move a twenty-band curve onto the twenty-band ladder of the DSP `compat` plays, band for
    /// band, and say whether it moved: off the Windows ladder for FxSound for Linux
    /// ([`move_off_the_windows_twenty_band_ladder`]), onto it at «Like FxSound for Windows» =
    /// Interface and sound ([`move_onto_the_windows_twenty_band_ladder`], audit report R4 taken
    /// back). The gains stay on their bands, so a curve taken to one ladder and back is the curve
    /// it was, bit for bit; a curve on neither ladder, or of another count, is left alone.
    pub fn move_to_the_twenty_band_ladder_of(
        bands: &mut [EqBand],
        compat: crate::DspCompat,
    ) -> bool {
        if compat.windows() {
            move_onto_the_windows_twenty_band_ladder(bands)
        } else {
            move_off_the_windows_twenty_band_ladder(bands)
        }
    }

    /// The lowest centre a band can be tuned to: the command line's floor, and where the design
    /// stops narrowing a low band's Q (`fxsound_dsp::biquad::calc_parametric`, rule A).
    pub const TUNING_FLOOR_HZ: f32 = 20.0;
    /// The highest: the command line's ceiling, and the top of the thirty-one-band ladder.
    pub const TUNING_CEILING_HZ: f32 = 20_000.0;

    /// The spectrum edges of a band count's ladder (`GraphicEqSet.cpp:430-486`), for the five
    /// counts that have a table; `None` for any other. `fxsound_dsp::eq::band_table` hands out
    /// the same edges with the ladders themselves; they are here so that the window and the
    /// preset store, which do not run an engine, can work out the tuning ranges.
    #[must_use]
    pub const fn ladder_edges_hz(num_bands: usize) -> Option<(f32, f32)> {
        match num_bands {
            5 | 10 => Some((62.5, 16_000.0)),
            15 => Some((25.0, 16_000.0)),
            20 => Some((20.0, 16_000.0)),
            31 => Some((20.0, 20_000.0)),
            _ => None,
        }
    }

    /// The edges every band count is laid out and designed between: its table's
    /// ([`ladder_edges_hz`]), or for a count with no table — only a microphone preset made by hand
    /// or a `num_bands` edited in `settings.toml` has one, since a `.fac` is fitted onto the
    /// window's count — the ten-band table's, which a new equalizer starts with.
    ///
    /// The Windows engine keeps whatever edges the last table count left for such a count
    /// (`GraphicEqSet.cpp:495-508`), so the same preset got another Q after a 31-band preset than
    /// after a ten-band one, and the window, which works out its curve on a new equalizer, drew
    /// only the second. One pair for the engine's ladder and Q (`fxsound_dsp::eq::GraphicEq`) and
    /// the curve drawn from it, so what is drawn is what plays, whatever came before. The ten-band
    /// edges sit inside every table's, so a curve carried to or from such a count by frequency
    /// never reads past a table's ends. The wheels still tune such a count across the whole span
    /// ([`band_frequency_range`]).
    #[must_use]
    pub const fn band_edges_hz(num_bands: usize) -> (f32, f32) {
        match ladder_edges_hz(num_bands) {
            Some(edges) => edges,
            None => (62.5, 16_000.0),
        }
    }

    /// How far band `band` may be tuned in the Windows build, between the ladder's edges `min_hz`
    /// and `max_hz` (`GraphicEqGet.cpp:105-168`).
    ///
    /// The bounds sit at the geometric midpoints of the generic log-spaced grid, with `+1` below
    /// a kilohertz and `+10` above it on the low edge so that two bands never meet; the two end
    /// bands stop at the ladder's own edges. Which is why band 1 of the five- and ten-band
    /// equalizer, at 62.5 Hz, can only be turned up, and the top band, at 16 kHz, only down: each
    /// sits at an end of its own range. [`band_frequency_range`] is the port's range.
    #[must_use]
    pub fn windows_band_range(
        band: usize,
        num_bands: usize,
        min_hz: f32,
        max_hz: f32,
    ) -> (f32, f32) {
        if num_bands <= 1 {
            return (min_hz, max_hz);
        }
        let ratio = f64::from(max_hz) / f64::from(min_hz);
        let denominator = (num_bands * 2 - 2) as f64;
        let one_based = band + 1;

        let low = if band == 0 {
            min_hz
        } else {
            let exponent = ((one_based as f64 - 1.0) * 2.0 - 1.0) / denominator;
            let edge = (f64::from(min_hz) * ratio.powf(exponent)).round() as f32;
            if edge < 1000.0 {
                edge + 1.0
            } else {
                edge + 10.0
            }
        };
        let high = if one_based >= num_bands {
            max_hz
        } else {
            let exponent = (one_based as f64 * 2.0 - 1.0) / denominator;
            (f64::from(min_hz) * ratio.powf(exponent)).round() as f32
        };
        (low, high)
    }

    /// Band `band`'s tuning range in the Windows build, for the count's own ladder.
    #[must_use]
    pub fn windows_band_frequency_range(band: usize, num_bands: usize) -> (f32, f32) {
        let (min_hz, max_hz) =
            ladder_edges_hz(num_bands).unwrap_or((TUNING_FLOOR_HZ, TUNING_CEILING_HZ));
        windows_band_range(band, num_bands, min_hz, max_hz)
    }

    /// How far band `band` may be tuned here: the Windows range, except that each end band
    /// reaches half a band past the ladder's edge as well, so it can be tuned both ways (audit
    /// report R6).
    ///
    /// In the Windows build the first band of the five- and ten-band equalizer, at 62.5 Hz, sits at
    /// the bottom of its own range and the last, at 16 kHz, at the top of its: the first could
    /// only move up, the last only down, and no sub-bass below 62.5 Hz could be reached at all.
    /// Here the first band's range starts as far below its ladder's edge as its range ends above
    /// it (ten bands: 46 to 85 Hz, five: 31 to 125 Hz), and the last band's range goes as far
    /// above (ten bands: 11 768 Hz to 20 kHz), never past [`TUNING_FLOOR_HZ`] and
    /// [`TUNING_CEILING_HZ`]: the twenty- and thirty-one-band ladders already start at 20 Hz and
    /// the thirty-one-band one ends at 20 kHz. Every other band keeps the Windows range.
    ///
    /// A `.fac` written for a Windows FxSound has its end bands put back on the ladder's edges
    /// ([`move_end_bands_back_inside_the_ladder`]); one read in keeps whatever it carries.
    #[must_use]
    pub fn band_frequency_range(band: usize, num_bands: usize) -> (f32, f32) {
        let (min_hz, max_hz) =
            ladder_edges_hz(num_bands).unwrap_or((TUNING_FLOOR_HZ, TUNING_CEILING_HZ));
        let (mut low, mut high) = windows_band_range(band, num_bands, min_hz, max_hz);
        if num_bands <= 1 {
            return (low, high);
        }
        // The square root of the ratio between two neighbouring bands of the generic grid: the
        // factor by which every range already reaches either side of its band.
        let half_step =
            (f64::from(max_hz) / f64::from(min_hz)).powf(1.0 / (num_bands * 2 - 2) as f64);
        if band == 0 {
            let below = (f64::from(min_hz) / half_step).round() as f32;
            low = below.max(TUNING_FLOOR_HZ).min(low);
        }
        if band + 1 >= num_bands {
            let above = (f64::from(max_hz) * half_step).round() as f32;
            high = above.min(TUNING_CEILING_HZ).max(high);
        }
        (low, high)
    }

    /// Put a curve's first and last band back on the ladder's edges where this port let them go
    /// past, for a `.fac` written for a Windows FxSound to open, and say whether either moved
    /// (audit report R6).
    ///
    /// Only the two end bands reach further here than in the Windows build
    /// ([`band_frequency_range`]), and only past the ladder's own edges: a first band below the
    /// ladder's bottom goes back to it and a last band above its top goes back to that — on five
    /// and ten bands, 62.5 Hz and 16 kHz, where the Windows build's wheels stop. Nothing else is
    /// clamped: the factory presets themselves put inner bands outside their wheels' ranges
    /// (Metal's 1000 Hz sits below the 1010 Hz its band's wheel starts at), and the Windows build
    /// plays such a centre as it is, so a preset Windows made is exported exactly as it came. A
    /// curve of a count with no table is left alone, and so is a centre that is not a number.
    pub fn move_end_bands_back_inside_the_ladder(bands: &mut [EqBand]) -> bool {
        let Some((min_hz, max_hz)) = ladder_edges_hz(bands.len()) else {
            return false;
        };
        let mut moved = false;
        if let Some(first) = bands.first_mut()
            && first.center_hz < min_hz
        {
            first.center_hz = min_hz;
            moved = true;
        }
        if let Some(last) = bands.last_mut()
            && last.center_hz > max_hz
        {
            last.center_hz = max_hz;
            moved = true;
        }
        moved
    }

    /// The lowest centre the equalizer designs a band at (`GraphicEqSet.cpp:555-559`,
    /// `fxsound_dsp::eq::MIN_BAND_FREQ_HZ`): a band below it is played there.
    pub const BAND_FLOOR_HZ: f32 = 10.0;
    /// The highest (`fxsound_dsp::eq::MAX_BAND_FREQ_HZ`).
    pub const BAND_CEILING_HZ: f32 = 21_000.0;

    /// Keep every centre of a curve inside the equalizer's own window, [`BAND_FLOOR_HZ`] to
    /// [`BAND_CEILING_HZ`], and say whether any moved: the one limit a `.fac` exported with its
    /// end bands as they are is held to («Like FxSound for Windows» = Interface and sound, the
    /// export window's choice against [`move_end_bands_back_inside_the_ladder`]). A centre inside
    /// it stays as it is, a band at a centre the equalizer would not design is written where it is
    /// played, and a centre that is not a number is left alone.
    pub fn keep_centres_inside_the_equalizer(bands: &mut [EqBand]) -> bool {
        let mut moved = false;
        for band in bands {
            let inside = band.center_hz.clamp(BAND_FLOOR_HZ, BAND_CEILING_HZ);
            if inside != band.center_hz && !band.center_hz.is_nan() {
                band.center_hz = inside;
                moved = true;
            }
        }
        moved
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
    /// The format version the file declared when it was read (`9` in every shipped preset). What
    /// an older version does not carry is filled in as the original reads it; a preset is always
    /// written as the current version, whatever this says (0.4.0 audit #46).
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

/// How hard the denoiser is allowed to work.
///
/// A control surface over RNNoise rather than a choice between networks: the network always
/// computes its twenty-two band gains and its voice probability, and the level decides how much
/// of that opinion reaches the signal — the row is [`DenoiseLevel::control`]. `Off` is a level
/// in its own right and not the absence of one, so that a preset can name it and a setting can
/// override a preset with it.
///
/// `Medium` is the default because a 0.3.0 preset that said `rnnoise = true` meant the network
/// as it was then, and that is the row `Medium` was voiced to reproduce.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DenoiseLevel {
    /// The stage stands aside. Distinct from `rnnoise = false` only in that it can be said.
    Off,
    /// A gentle floor and most of the voice handed back: for a quiet room and a good microphone,
    /// where the network's own artefacts would cost more than the noise it removes.
    Light,
    /// What 0.3.0 did.
    #[default]
    Medium,
    /// The network's full opinion, with nothing handed back. For a mechanical keyboard or a fan
    /// that never stops.
    Strong,
}

impl DenoiseLevel {
    /// Every level, in the order the interface lists them.
    pub const ALL: [Self; 4] = [Self::Off, Self::Light, Self::Medium, Self::Strong];

    /// The English label, and so the `tr` key.
    ///
    /// `Light` is labelled **Mild**. `"Light"` is already a key in every one of the Windows
    /// build's tables, where it names the light *theme*, and a key has one translation per
    /// language: "Hell" and "Светлая" describe a palette, not a noise floor, and a port entry
    /// that said otherwise would re-label the theme switch. So the level takes a word of its
    /// own. The socket, D-Bus and file spelling is [`DenoiseLevel::key`]'s `light` regardless.
    #[inline]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Light => "Mild",
            Self::Medium => "Medium",
            Self::Strong => "Strong",
        }
    }

    /// Stable key used in files, on the control socket and on D-Bus.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Light => "light",
            Self::Medium => "medium",
            Self::Strong => "strong",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.key() == key)
    }

    /// The control surface this level stands for.
    ///
    /// The numbers are a design choice, not a measurement, and they are the one place the choice
    /// is written down: every other crate asks here rather than carrying its own copy. `Off` is
    /// all zeros, which [`DenoiseControl::is_active`] reads as "nothing to do".
    #[must_use]
    pub const fn control(self) -> DenoiseControl {
        match self {
            Self::Off => DenoiseControl {
                max_suppression_db: 0.0,
                vad_threshold: 0.0,
                voice_preservation: 0.0,
                wet_dry: 0.0,
            },
            Self::Light => DenoiseControl {
                max_suppression_db: 12.0,
                vad_threshold: 0.0,
                voice_preservation: 0.5,
                wet_dry: 1.0,
            },
            Self::Medium => DenoiseControl {
                max_suppression_db: 24.0,
                vad_threshold: 0.15,
                voice_preservation: 0.3,
                wet_dry: 1.0,
            },
            Self::Strong => DenoiseControl {
                max_suppression_db: 60.0,
                vad_threshold: 0.35,
                voice_preservation: 0.0,
                wet_dry: 1.0,
            },
        }
    }
}

/// The four numbers behind a [`DenoiseLevel`].
///
/// Applied to the network's band gains before synthesis, in this order: the gains are floored at
/// `10^(−max_suppression_db/20)`; a frame whose voice probability is below `vad_threshold` is
/// attenuated toward that floor; `voice_preservation × probability` of the remaining gap to unity
/// is handed back; and the result is mixed with the dry signal by `wet_dry`. A preset may carry a
/// row of its own instead of a level's, which is why this is a value and not just an enum.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DenoiseControl {
    /// Per-band gain floor, in dB of suppression: the floor itself is `10^(−x/20)`. Light 12,
    /// Medium 24, Strong 60.
    pub max_suppression_db: f32,
    /// Below this voice probability the frame is treated as noise.
    pub vad_threshold: f32,
    /// `0..=1`: lift the gains toward unity in proportion to the voice probability.
    pub voice_preservation: f32,
    /// `0..=1` mix; `1` is fully denoised.
    pub wet_dry: f32,
}

impl DenoiseControl {
    /// Whether this row asks the stage to do anything at all.
    ///
    /// A floor of 0 dB is a straight wire, and a mix with no wet in it is the dry signal: either
    /// one means the network's output never reaches the listener, so the stage may stand aside
    /// and save its twenty milliseconds of latency.
    #[inline]
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.max_suppression_db > 0.0 && self.wet_dry > 0.0
    }

    /// The linear gain floor, `10^(−max_suppression_db/20)`. `1.0` when nothing may be removed.
    #[inline]
    #[must_use]
    pub fn gain_floor(&self) -> f32 {
        10.0_f32.powf(-self.max_suppression_db / 20.0)
    }

    /// Force every field into its [`limits`] range, substituting `fallback`'s field for one that
    /// is not a number at all — the same asymmetry every other snapshot field keeps.
    ///
    /// The fallback is a row rather than a constant because the right answer for a corrupt
    /// override is the level it was overriding.
    pub fn sanitise(&mut self, fallback: Self) {
        use limits::finite;

        self.max_suppression_db = finite(
            self.max_suppression_db,
            limits::DENOISE_MAX_SUPPRESSION_DB,
            fallback.max_suppression_db,
        );
        self.vad_threshold = finite(
            self.vad_threshold,
            limits::DENOISE_VAD_THRESHOLD,
            fallback.vad_threshold,
        );
        self.voice_preservation = finite(
            self.voice_preservation,
            limits::DENOISE_VOICE_PRESERVATION,
            fallback.voice_preservation,
        );
        self.wet_dry = finite(self.wet_dry, limits::DENOISE_WET_DRY, fallback.wet_dry);
    }
}

impl Default for DenoiseControl {
    /// The default level's row, so that a snapshot built with `..Default::default()` and one
    /// built from `DenoiseLevel::default().control()` say the same thing.
    fn default() -> Self {
        DenoiseLevel::default().control()
    }
}

/// How the denoiser treats a multi-channel capture.
///
/// RNNoise is a mono network. `Independent` — one network per channel, which is what 0.3.0 did
/// and so the default — costs a network per channel and lets the two sides of a stereo
/// microphone disagree about what is voice, which smears the image. The other two run one
/// network on a downmix and differ in what they do with its answer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DenoiseChannelMode {
    /// Downmix, denoise once, and send the same signal to every channel.
    Mono,
    /// Downmix for the analysis only: the one set of band gains is applied to each channel
    /// through that channel's own transform, so the image survives and the mask is shared.
    Linked,
    /// One network per channel.
    #[default]
    Independent,
}

impl DenoiseChannelMode {
    /// Every mode, in the order the interface lists them.
    pub const ALL: [Self; 3] = [Self::Mono, Self::Linked, Self::Independent];

    /// The English label, and so the `tr` key.
    #[inline]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mono => "Mono",
            Self::Linked => "Linked stereo",
            Self::Independent => "Independent",
        }
    }

    /// Stable key used in files, on the control socket and on D-Bus.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Linked => "linked",
            Self::Independent => "independent",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.key() == key)
    }
}

/// Where the de-esser puts its band.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DeEsserMode {
    /// The corner the preset asks for, or nothing when the rate cannot carry it.
    #[default]
    Classic,
    /// The corner is chosen relative to the source's bandwidth, so a 16 kHz headset profile
    /// still gets a de-esser instead of a stage that says it is unavailable.
    Adaptive,
}

impl DeEsserMode {
    /// Every mode, in the order the interface lists them.
    pub const ALL: [Self; 2] = [Self::Classic, Self::Adaptive];

    /// The English label, and so the `tr` key.
    #[inline]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Adaptive => "Adaptive",
        }
    }

    /// Stable key used in files, on the control socket and on D-Bus.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Adaptive => "adaptive",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.key() == key)
    }
}

/// How much late reverberation the de-reverb stage removes.
///
/// `Off` by default: the stage costs five milliseconds of latency and a room that is not
/// reverberant gains nothing from it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DereverbLevel {
    #[default]
    Off,
    Light,
    Medium,
    Strong,
}

impl DereverbLevel {
    /// Every level, in the order the interface lists them.
    pub const ALL: [Self; 4] = [Self::Off, Self::Light, Self::Medium, Self::Strong];

    /// The English label, and so the `tr` key. `Light` reads **Mild** for the reason given on
    /// [`DenoiseLevel::label`].
    #[inline]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Light => "Mild",
            Self::Medium => "Medium",
            Self::Strong => "Strong",
        }
    }

    /// Stable key used in files, on the control socket and on D-Bus.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Light => "light",
            Self::Medium => "medium",
            Self::Strong => "strong",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.key() == key)
    }
}

/// The Settings pane's global noise-suppression override.
///
/// A voice preset names a [`DenoiseLevel`]; this sits over every preset at once. `Preset` — the
/// default — means "whatever the preset says", so a fresh install sounds like the preset it
/// selected and the override is something the user reaches for, not something they inherit.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NoiseSuppressionOverride {
    /// Follow the voice preset.
    #[default]
    Preset,
    Off,
    Light,
    Medium,
    Strong,
}

impl NoiseSuppressionOverride {
    /// Every choice, in the order the interface lists them.
    pub const ALL: [Self; 5] = [
        Self::Preset,
        Self::Off,
        Self::Light,
        Self::Medium,
        Self::Strong,
    ];

    /// The English label, and so the `tr` key. `Light` reads **Mild** for the reason given on
    /// [`DenoiseLevel::label`].
    #[inline]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preset => "Preset",
            Self::Off => "Off",
            Self::Light => "Mild",
            Self::Medium => "Medium",
            Self::Strong => "Strong",
        }
    }

    /// Stable key used in the settings file, on the control socket and on D-Bus.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Preset => "preset",
            Self::Off => "off",
            Self::Light => "light",
            Self::Medium => "medium",
            Self::Strong => "strong",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| choice.key() == key)
    }

    /// The level this override pins, or `None` for "follow the preset".
    #[inline]
    #[must_use]
    pub const fn level(self) -> Option<DenoiseLevel> {
        match self {
            Self::Preset => None,
            Self::Off => Some(DenoiseLevel::Off),
            Self::Light => Some(DenoiseLevel::Light),
            Self::Medium => Some(DenoiseLevel::Medium),
            Self::Strong => Some(DenoiseLevel::Strong),
        }
    }

    /// The level that takes effect: the override's, unless it says to follow `preset`.
    #[inline]
    #[must_use]
    pub const fn resolve(self, preset: DenoiseLevel) -> DenoiseLevel {
        match self.level() {
            Some(level) => level,
            None => preset,
        }
    }
}

/// The Settings pane's global denoiser channel-mode override; see [`NoiseSuppressionOverride`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DenoiseChannelsOverride {
    /// Follow the voice preset.
    #[default]
    Preset,
    Mono,
    Linked,
    Independent,
}

impl DenoiseChannelsOverride {
    /// Every choice, in the order the interface lists them.
    pub const ALL: [Self; 4] = [Self::Preset, Self::Mono, Self::Linked, Self::Independent];

    /// The English label, and so the `tr` key.
    #[inline]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preset => "Preset",
            Self::Mono => DenoiseChannelMode::Mono.label(),
            Self::Linked => DenoiseChannelMode::Linked.label(),
            Self::Independent => DenoiseChannelMode::Independent.label(),
        }
    }

    /// Stable key used in the settings file, on the control socket and on D-Bus.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Preset => "preset",
            Self::Mono => DenoiseChannelMode::Mono.key(),
            Self::Linked => DenoiseChannelMode::Linked.key(),
            Self::Independent => DenoiseChannelMode::Independent.key(),
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| choice.key() == key)
    }

    /// The mode this override pins, or `None` for "follow the preset".
    #[inline]
    #[must_use]
    pub const fn mode(self) -> Option<DenoiseChannelMode> {
        match self {
            Self::Preset => None,
            Self::Mono => Some(DenoiseChannelMode::Mono),
            Self::Linked => Some(DenoiseChannelMode::Linked),
            Self::Independent => Some(DenoiseChannelMode::Independent),
        }
    }

    /// The mode that takes effect: the override's, unless it says to follow `preset`.
    #[inline]
    #[must_use]
    pub const fn resolve(self, preset: DenoiseChannelMode) -> DenoiseChannelMode {
        match self.mode() {
            Some(mode) => mode,
            None => preset,
        }
    }
}

/// Which way audio flows through a device FxSound can attach to.
///
/// The Windows build only ever sat in front of a *playback* endpoint. The Linux port can also sit
/// behind a *capture* device — a microphone — and publish the processed signal as a virtual
/// source. A device is one or the other, and FxSound keeps one lane per direction: a pair of
/// nodes, a chain and a claim on the session default for each, which can run at the same time
/// and are enabled and detached independently. Which lane the window is *editing* is a separate
/// notion (`Settings::device_direction`) that the engine never sees.
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
    /// Both directions, outputs first — the order every device list and every per-lane table
    /// keeps.
    pub const ALL: [Self; 2] = [Self::Output, Self::Input];

    /// The English word the UI uses for the section header and the tray tooltip.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Output => "Output",
            Self::Input => "Input",
        }
    }

    /// Stable key used on the control socket and on D-Bus — the same spelling the settings file
    /// uses, so one parser serves all three.
    #[inline]
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Output => "output",
            Self::Input => "input",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|direction| direction.key() == key)
    }

    /// The other direction.
    #[inline]
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Output => Self::Input,
            Self::Input => Self::Output,
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
    fn every_stored_value_of_every_effect_comes_back_from_its_slider_position() {
        // Audit report #14: a value a preset stores has a slider position that saves it again,
        // so loading a preset and saving it changes nothing. The one exception is Dynamic Boost
        // past its dead top, where every value sounds the same and shows at the top.
        for effect in Effect::ALL {
            for midi in scale::MIDI_MIN..=scale::MIDI_MAX {
                let shown = scale::midi_to_slider_for(effect, midi);
                let saved = scale::slider_to_midi_for(effect, shown);
                if effect == Effect::DynamicBoost && midi > scale::DYNAMIC_BOOST_MAX_MIDI {
                    assert_eq!(saved, scale::DYNAMIC_BOOST_MAX_MIDI, "{effect:?} {midi}");
                } else {
                    assert_eq!(saved, midi, "{effect:?}: {midi} shows at {shown}");
                }
            }
        }
    }

    #[test]
    fn every_whole_position_of_every_effect_is_a_different_stored_value() {
        for effect in Effect::ALL {
            let saved: Vec<u8> = (0..=10)
                .map(|p| scale::slider_to_midi_for(effect, p as f32))
                .collect();
            let mut distinct = saved.clone();
            distinct.dedup();
            assert_eq!(distinct, saved, "{effect:?}: {saved:?}");
            assert_eq!(saved[0], 0, "{effect:?}: position 0 is off");
        }
    }

    #[test]
    fn ambiences_ten_positions_run_from_its_first_audible_value_to_the_top() {
        // Audit report #39: positions 1..3 used to store 13, 25 and 38, a reverb 40 dB and more
        // under the dry signal. Position 1 is now the first value it can be heard at.
        let saved: Vec<u8> = (0..=10)
            .map(|p| scale::slider_to_midi_for(Effect::Ambience, p as f32))
            .collect();
        assert_eq!(
            saved,
            [0, 39, 49, 59, 68, 78, 88, 98, 107, 117, 127],
            "position by position"
        );
        // A Windows preset's 13..38 shows below position 1, where it sounds, and 0 stays off.
        for midi in 1..scale::AMBIENCE_FIRST_AUDIBLE_MIDI {
            let shown = scale::midi_to_slider_for(Effect::Ambience, midi);
            assert!(shown > 0.0 && shown < 1.0, "{midi} shows at {shown}");
        }
        // The factory presets' 39..64 sit from position 1 up.
        assert_eq!(scale::midi_to_slider_for(Effect::Ambience, 39), 1.0);
        let at_64 = scale::midi_to_slider_for(Effect::Ambience, 64);
        assert!((at_64 - 3.557).abs() < 0.01, "{at_64}");
    }

    #[test]
    fn at_interface_and_sound_ambiences_positions_are_the_windows_builds_straight_line() {
        // Audit report #39 at «Like FxSound for Windows» = Interface and sound: the slider's
        // mapping comes back with the stage, so positions 1-3 store 13, 25 and 38 again, as
        // `FxAudioControls.cpp:113`'s 0..10 slider does.
        let windows = DspCompat::Windows;
        let saved: Vec<u8> = (0..=10)
            .map(|p| scale::slider_to_midi_in(windows, Effect::Ambience, p as f32))
            .collect();
        assert_eq!(saved, [0, 13, 25, 38, 51, 64, 76, 89, 102, 114, 127]);
        let at_39 = scale::midi_to_slider_in(windows, Effect::Ambience, 39);
        assert!((at_39 - 3.071).abs() < 0.001, "{at_39}");
        assert_eq!(scale::slider_label_in(windows, Effect::Ambience, 1.0), "1");
        // FxSound for Linux's is untouched, and so is every other effect's.
        for effect in Effect::ALL {
            for tenth in 0..=100 {
                let slider = tenth as f32 / 10.0;
                assert_eq!(
                    scale::slider_to_value_in(DspCompat::Linux, effect, slider),
                    scale::slider_to_value_for(effect, slider),
                    "{effect:?} at {slider}"
                );
                if effect != Effect::Ambience {
                    assert_eq!(
                        scale::slider_to_value_in(windows, effect, slider),
                        scale::slider_to_value_for(effect, slider),
                        "{effect:?} at {slider}"
                    );
                }
            }
        }
    }

    #[test]
    fn at_interface_and_sound_every_stored_value_comes_back_and_is_one_fine_step_from_the_next() {
        let windows = DspCompat::Windows;
        for effect in Effect::ALL {
            let top = if effect == Effect::DynamicBoost {
                scale::DYNAMIC_BOOST_MAX_MIDI
            } else {
                scale::MIDI_MAX
            };
            for midi in 0..=top {
                let shown = scale::midi_to_slider_in(windows, effect, midi);
                assert_eq!(
                    scale::slider_to_midi_in(windows, effect, shown),
                    midi,
                    "{effect:?}: {midi} shows at {shown}"
                );
            }
            let mut slider = 0.0;
            for midi in 1..=top {
                slider = scale::stored_step_in(windows, effect, slider, true);
                assert_eq!(scale::slider_to_midi_in(windows, effect, slider), midi);
                assert_eq!(
                    scale::nearest_stored_position_in(windows, effect, slider),
                    slider
                );
            }
        }
    }

    #[test]
    fn a_stored_value_between_positions_is_shown_with_its_decimal() {
        // Audit report #14: General's Surround is 20, between positions 1 (13) and 2 (25). It
        // used to show as "2", and touching the slider saved 25.
        let surround = scale::midi_to_slider_for(Effect::Surround, 20);
        assert_eq!(scale::slider_label_for(Effect::Surround, surround), "1.6");
        assert_eq!(scale::whole_position_for(Effect::Surround, surround), None);
        // A whole position comes back from its stored value a hair off and is still that position.
        let one = scale::midi_to_slider_for(Effect::Fidelity, 13);
        assert!(
            one != 1.0,
            "the fixture needs a position that comes back off by a hair"
        );
        assert_eq!(scale::slider_label_for(Effect::Fidelity, one), "1");
        assert_eq!(scale::slider_label_for(Effect::Fidelity, 7.0), "7");
        // Next to a position but not it: the decimal says so.
        let beside_two = scale::midi_to_slider_for(Effect::Fidelity, 26);
        assert_eq!(scale::slider_label_for(Effect::Fidelity, beside_two), "2.0");
    }

    #[test]
    fn a_fine_step_reaches_every_stored_value_one_at_a_time() {
        for effect in Effect::ALL {
            let top = if effect == Effect::DynamicBoost {
                scale::DYNAMIC_BOOST_MAX_MIDI
            } else {
                scale::MIDI_MAX
            };
            let mut slider = 0.0;
            for midi in 1..=top {
                slider = scale::stored_step_for(effect, slider, true);
                assert_eq!(
                    scale::slider_to_midi_for(effect, slider),
                    midi,
                    "{effect:?} up"
                );
            }
            assert_eq!(
                scale::stored_step_for(effect, slider, true),
                slider,
                "{effect:?} top"
            );
            for midi in (0..top).rev() {
                slider = scale::stored_step_for(effect, slider, false);
                assert_eq!(
                    scale::slider_to_midi_for(effect, slider),
                    midi,
                    "{effect:?} down"
                );
            }
            assert_eq!(scale::stored_step_for(effect, 0.0, false), 0.0);
        }
    }

    fn twenty(ladder: &[f32; 20]) -> Vec<EqBand> {
        ladder
            .iter()
            .enumerate()
            .map(|(i, &hz)| EqBand::new(hz, i as f32 * 0.5 - 4.0))
            .collect()
    }

    #[test]
    fn a_curve_on_the_windows_twenty_band_ladder_moves_to_the_half_octave_one_band_for_band() {
        // Audit report R4: the gains stay on their bands, only the centres move.
        let mut bands = twenty(&eq::WINDOWS_TWENTY_BAND_CENTRES_HZ);
        let gains: Vec<f32> = bands.iter().map(|b| b.boost_db).collect();
        assert!(eq::move_off_the_windows_twenty_band_ladder(&mut bands));
        let centres: Vec<f32> = bands.iter().map(|b| b.center_hz).collect();
        assert_eq!(centres, eq::TWENTY_BAND_CENTRES_HZ);
        assert_eq!(bands.iter().map(|b| b.boost_db).collect::<Vec<_>>(), gains);
        // No band moves by more than a sixth of an octave (0.1705, at 10 kHz).
        for (old, new) in eq::WINDOWS_TWENTY_BAND_CENTRES_HZ
            .iter()
            .zip(eq::TWENTY_BAND_CENTRES_HZ)
        {
            assert!(
                (new / old).log2().abs() < 1.0 / 6.0 + 0.004,
                "{old} -> {new}"
            );
        }
        // Moved once, it stays where it is.
        assert!(!eq::move_off_the_windows_twenty_band_ladder(&mut bands));
    }

    #[test]
    fn a_curve_on_any_other_centres_is_left_where_it_is() {
        // A band someone dragged is not the Windows ladder any more.
        let mut dragged = twenty(&eq::WINDOWS_TWENTY_BAND_CENTRES_HZ);
        dragged[7].center_hz = 260.0;
        let before = dragged.clone();
        assert!(!eq::move_off_the_windows_twenty_band_ladder(&mut dragged));
        assert_eq!(dragged, before);
        // Nor is a curve of another count, or the ten-band default.
        let mut ten = eq::default_bands();
        assert!(!eq::move_off_the_windows_twenty_band_ladder(&mut ten));
        assert!(!eq::move_onto_the_windows_twenty_band_ladder(&mut ten));
        assert_eq!(ten, eq::default_bands());
        // `%g` writes six figures: a centre read back from a file is the ladder's to 0.01 Hz.
        let mut read_back = twenty(&eq::WINDOWS_TWENTY_BAND_CENTRES_HZ);
        read_back[12].center_hz = 1250.004;
        assert!(eq::move_off_the_windows_twenty_band_ladder(&mut read_back));
    }

    #[test]
    fn a_half_octave_curve_goes_back_onto_the_windows_ladder_for_a_file_windows_will_read() {
        let mut bands = twenty(&eq::TWENTY_BAND_CENTRES_HZ);
        let gains: Vec<f32> = bands.iter().map(|b| b.boost_db).collect();
        assert!(eq::move_onto_the_windows_twenty_band_ladder(&mut bands));
        let centres: Vec<f32> = bands.iter().map(|b| b.center_hz).collect();
        assert_eq!(centres, eq::WINDOWS_TWENTY_BAND_CENTRES_HZ);
        assert_eq!(bands.iter().map(|b| b.boost_db).collect::<Vec<_>>(), gains);
        assert!(eq::move_off_the_windows_twenty_band_ladder(&mut bands));
        assert_eq!(
            bands,
            twenty(&eq::TWENTY_BAND_CENTRES_HZ),
            "and back, exactly"
        );
    }

    /// The five counts the window offers, which are the ones with a ladder of their own.
    const TABLE_COUNTS: [usize; 5] = [5, 10, 15, 20, 31];

    #[test]
    fn the_windows_tuning_ranges_are_the_spec_tables() {
        // docs/spec/04-equalizer-visualizer.md §A4, from `GraphicEqGet.cpp:105-168`.
        let five = [
            (62.5_f32, 125.0_f32),
            (126.0, 500.0),
            (501.0, 2000.0),
            (2010.0, 8000.0),
            (8010.0, 16000.0),
        ];
        for (band, expected) in five.into_iter().enumerate() {
            assert_eq!(eq::windows_band_frequency_range(band, 5), expected);
        }
        let ten = [
            (62.5_f32, 85.0_f32),
            (86.0, 157.0),
            (158.0, 292.0),
            (293.0, 540.0),
            (541.0, 1000.0),
            (1010.0, 1852.0),
            (1862.0, 3429.0),
            (3439.0, 6350.0),
            (6360.0, 11758.0),
            (11768.0, 16000.0),
        ];
        for (band, expected) in ten.into_iter().enumerate() {
            assert_eq!(eq::windows_band_frequency_range(band, 10), expected);
        }
        assert_eq!(eq::windows_band_frequency_range(0, 15), (25.0, 31.0));
        assert_eq!(eq::windows_band_frequency_range(14, 15), (12713.0, 16000.0));
        assert_eq!(eq::windows_band_frequency_range(19, 20), (13429.0, 16000.0));
        assert_eq!(eq::windows_band_frequency_range(30, 31), (17835.0, 20000.0));
    }

    #[test]
    fn the_first_and_last_band_of_five_and_ten_can_be_tuned_both_ways() {
        // Audit report R6: on Windows band 1 sits at the bottom of 62.5..85 Hz and band 10 at the
        // top of 11768..16000 Hz.
        assert_eq!(eq::band_frequency_range(0, 10), (46.0, 85.0));
        assert_eq!(eq::band_frequency_range(9, 10), (11768.0, 20000.0));
        assert_eq!(eq::band_frequency_range(0, 5), (31.0, 125.0));
        assert_eq!(eq::band_frequency_range(4, 5), (8010.0, 20000.0));
        for count in [5, 10] {
            let first = eq::band_frequency_range(0, count);
            let last = eq::band_frequency_range(count - 1, count);
            assert!(first.0 < 62.5 && 62.5 < first.1, "{count}: {first:?}");
            assert!(last.0 < 16000.0 && 16000.0 < last.1, "{count}: {last:?}");
        }
    }

    #[test]
    fn every_band_but_the_two_at_the_ends_keeps_its_windows_range() {
        for count in TABLE_COUNTS.into_iter().chain([2, 7, 12]) {
            for band in 1..count - 1 {
                assert_eq!(
                    eq::band_frequency_range(band, count),
                    eq::windows_band_frequency_range(band, count),
                    "{count} bands, band {band}"
                );
            }
        }
    }

    #[test]
    fn the_wider_ranges_stay_between_20_hz_and_20_khz_and_never_meet() {
        for count in TABLE_COUNTS {
            for band in 0..count {
                let (low, high) = eq::band_frequency_range(band, count);
                let (windows_low, windows_high) = eq::windows_band_frequency_range(band, count);
                assert!(low <= windows_low && high >= windows_high, "{count}/{band}");
                assert!(low >= eq::TUNING_FLOOR_HZ && high <= eq::TUNING_CEILING_HZ);
                if band + 1 < count {
                    let (next_low, _) = eq::band_frequency_range(band + 1, count);
                    assert!(high < next_low, "{count} bands: {band} reaches {band}+1");
                }
            }
        }
        // Ladders that already start at 20 Hz or end at 20 kHz keep those ends; the rest reach
        // as far as the floor or the ceiling allows.
        assert_eq!(eq::band_frequency_range(0, 20), (20.0, 24.0));
        assert_eq!(eq::band_frequency_range(0, 31), (20.0, 22.0));
        assert_eq!(eq::band_frequency_range(30, 31), (17835.0, 20000.0));
        assert_eq!(eq::band_frequency_range(0, 15), (20.0, 31.0));
        assert_eq!(eq::band_frequency_range(14, 15), (12713.0, 20000.0));
        assert_eq!(eq::band_frequency_range(19, 20), (13429.0, 19077.0));
    }

    #[test]
    fn a_curve_for_windows_has_its_end_bands_put_back_on_the_ladders_edges() {
        // Audit report R6: a `.fac` Windows reads carries end bands its wheels can show.
        let mut ten = eq::default_bands();
        ten[0].center_hz = 46.0;
        ten[9].center_hz = 20000.0;
        ten[4].boost_db = 3.0;
        assert!(eq::move_end_bands_back_inside_the_ladder(&mut ten));
        assert_eq!(ten[0].center_hz, 62.5);
        assert_eq!(ten[9].center_hz, 16000.0);
        assert_eq!(ten[4].boost_db, 3.0, "gains are not touched");

        let mut five: Vec<EqBand> = [31.0, 250.0, 1000.0, 4000.0, 20000.0]
            .iter()
            .map(|&hz| EqBand::new(hz, 1.0))
            .collect();
        assert!(eq::move_end_bands_back_inside_the_ladder(&mut five));
        let centres: Vec<f32> = five.iter().map(|b| b.center_hz).collect();
        assert_eq!(centres, [62.5, 250.0, 1000.0, 4000.0, 16000.0]);

        // Fifteen bands reach 20 Hz and 20 kHz here and stop at 25 Hz and 16 kHz on Windows.
        let mut fifteen: Vec<EqBand> = (0..15)
            .map(|i| EqBand::new(100.0 * (i + 1) as f32, 0.0))
            .collect();
        fifteen[0].center_hz = 20.0;
        fifteen[14].center_hz = 19_000.0;
        assert!(eq::move_end_bands_back_inside_the_ladder(&mut fifteen));
        assert_eq!(
            (fifteen[0].center_hz, fifteen[14].center_hz),
            (25.0, 16000.0)
        );
    }

    #[test]
    fn a_curve_whose_end_bands_are_inside_the_ladder_is_exported_as_it_is() {
        let mut ten = eq::default_bands();
        assert!(!eq::move_end_bands_back_inside_the_ladder(&mut ten));
        assert_eq!(ten, eq::default_bands());
        let mut windows_twenty = twenty(&eq::WINDOWS_TWENTY_BAND_CENTRES_HZ);
        let before = windows_twenty.clone();
        assert!(!eq::move_end_bands_back_inside_the_ladder(
            &mut windows_twenty
        ));
        assert_eq!(windows_twenty, before);
        // Inner bands outside their wheels' ranges are the factory presets' own (Metal's 1000 Hz
        // is below band 6's 1010 Hz), and so is an end band turned inwards past its range.
        let mut metal = eq::default_bands();
        metal[5].center_hz = 1000.0;
        metal[0].center_hz = 100.0;
        metal[9].center_hz = 11000.0;
        let before = metal.clone();
        assert!(!eq::move_end_bands_back_inside_the_ladder(&mut metal));
        assert_eq!(metal, before);
        // A count with no table has no ladder to go back inside, and a NaN is not a place.
        let mut twelve: Vec<EqBand> = (0..12).map(|i| EqBand::new(5.0 + i as f32, 0.0)).collect();
        let before = twelve.clone();
        assert!(!eq::move_end_bands_back_inside_the_ladder(&mut twelve));
        assert_eq!(twelve, before);
        let mut broken = eq::default_bands();
        broken[0].center_hz = f32::NAN;
        assert!(!eq::move_end_bands_back_inside_the_ladder(&mut broken));
        assert!(broken[0].center_hz.is_nan());
        assert!(!eq::move_end_bands_back_inside_the_ladder(&mut []));
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

    // ---- the microphone's mode enums ------------------------------------------------------

    /// What the settings file, the socket and D-Bus all agree on: a value's `key()` is exactly
    /// what serde writes for it, so one parser serves all three.
    fn serde_spelling<T: serde::Serialize>(value: T) -> String {
        #[derive(serde::Serialize)]
        struct Wrapper<T> {
            v: T,
        }
        let text = toml::to_string(&Wrapper { v: value }).expect("serialise");
        text.trim()
            .strip_prefix("v = \"")
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("a bare string")
            .to_owned()
    }

    #[test]
    fn every_mode_key_round_trips_and_matches_its_serde_spelling() {
        for level in DenoiseLevel::ALL {
            assert_eq!(DenoiseLevel::from_key(level.key()), Some(level));
            assert_eq!(serde_spelling(level), level.key());
        }
        for mode in DenoiseChannelMode::ALL {
            assert_eq!(DenoiseChannelMode::from_key(mode.key()), Some(mode));
            assert_eq!(serde_spelling(mode), mode.key());
        }
        for mode in DeEsserMode::ALL {
            assert_eq!(DeEsserMode::from_key(mode.key()), Some(mode));
            assert_eq!(serde_spelling(mode), mode.key());
        }
        for level in DereverbLevel::ALL {
            assert_eq!(DereverbLevel::from_key(level.key()), Some(level));
            assert_eq!(serde_spelling(level), level.key());
        }
        for choice in NoiseSuppressionOverride::ALL {
            assert_eq!(
                NoiseSuppressionOverride::from_key(choice.key()),
                Some(choice)
            );
            assert_eq!(serde_spelling(choice), choice.key());
        }
        for choice in DenoiseChannelsOverride::ALL {
            assert_eq!(
                DenoiseChannelsOverride::from_key(choice.key()),
                Some(choice)
            );
            assert_eq!(serde_spelling(choice), choice.key());
        }
        for direction in DeviceDirection::ALL {
            assert_eq!(DeviceDirection::from_key(direction.key()), Some(direction));
            assert_eq!(serde_spelling(direction), direction.key());
        }
    }

    #[test]
    fn an_unknown_key_parses_as_nothing_rather_than_as_a_default() {
        assert_eq!(DenoiseLevel::from_key("Medium"), None, "keys are lowercase");
        assert_eq!(DenoiseLevel::from_key(""), None);
        assert_eq!(DenoiseChannelMode::from_key("linked_stereo"), None);
        assert_eq!(DeEsserMode::from_key("auto"), None);
        assert_eq!(DereverbLevel::from_key("on"), None);
        assert_eq!(NoiseSuppressionOverride::from_key("default"), None);
        assert_eq!(DenoiseChannelsOverride::from_key("stereo"), None);
        assert_eq!(DeviceDirection::from_key("Output"), None);
    }

    #[test]
    fn the_keys_of_one_enum_are_distinct_and_so_are_its_labels() {
        fn distinct<'a>(items: impl Iterator<Item = &'a str>) -> bool {
            let mut seen = std::collections::HashSet::new();
            items.into_iter().all(|item| seen.insert(item))
        }
        assert!(distinct(DenoiseLevel::ALL.iter().map(|l| l.key())));
        assert!(distinct(DenoiseLevel::ALL.iter().map(|l| l.label())));
        assert!(distinct(DenoiseChannelMode::ALL.iter().map(|m| m.key())));
        assert!(distinct(DenoiseChannelMode::ALL.iter().map(|m| m.label())));
        assert!(distinct(DeEsserMode::ALL.iter().map(|m| m.key())));
        assert!(distinct(DereverbLevel::ALL.iter().map(|l| l.key())));
        assert!(distinct(
            NoiseSuppressionOverride::ALL.iter().map(|c| c.key())
        ));
        assert!(distinct(
            NoiseSuppressionOverride::ALL.iter().map(|c| c.label())
        ));
        assert!(distinct(
            DenoiseChannelsOverride::ALL.iter().map(|c| c.key())
        ));
        assert!(distinct(
            DenoiseChannelsOverride::ALL.iter().map(|c| c.label())
        ));
    }

    #[test]
    fn the_defaults_are_what_a_0_3_0_file_meant() {
        // `rnnoise = true` meant the network as it then was, which is the Medium row; one network
        // per channel; the corner the preset asked for; no de-reverb.
        assert_eq!(DenoiseLevel::default(), DenoiseLevel::Medium);
        assert_eq!(
            DenoiseChannelMode::default(),
            DenoiseChannelMode::Independent
        );
        assert_eq!(DeEsserMode::default(), DeEsserMode::Classic);
        assert_eq!(DereverbLevel::default(), DereverbLevel::Off);
        assert_eq!(
            NoiseSuppressionOverride::default(),
            NoiseSuppressionOverride::Preset
        );
        assert_eq!(
            DenoiseChannelsOverride::default(),
            DenoiseChannelsOverride::Preset
        );
        assert_eq!(DeviceDirection::default(), DeviceDirection::Output);
    }

    #[test]
    fn the_level_table_is_the_one_in_the_design_record() {
        let row = |level: DenoiseLevel| {
            let c = level.control();
            (
                c.max_suppression_db,
                c.vad_threshold,
                c.voice_preservation,
                c.wet_dry,
            )
        };
        assert_eq!(row(DenoiseLevel::Off), (0.0, 0.0, 0.0, 0.0));
        assert_eq!(row(DenoiseLevel::Light), (12.0, 0.0, 0.5, 1.0));
        assert_eq!(row(DenoiseLevel::Medium), (24.0, 0.15, 0.3, 1.0));
        assert_eq!(row(DenoiseLevel::Strong), (60.0, 0.35, 0.0, 1.0));
        // And every row is already inside the limits it will be clamped to, so a level never
        // changes on its way to the audio thread.
        for level in DenoiseLevel::ALL {
            let mut checked = level.control();
            checked.sanitise(DenoiseLevel::Strong.control());
            assert_eq!(checked, level.control(), "{level:?}");
        }
    }

    #[test]
    fn off_is_the_only_level_whose_row_is_inactive() {
        for level in DenoiseLevel::ALL {
            assert_eq!(
                level.control().is_active(),
                level != DenoiseLevel::Off,
                "{level:?}"
            );
        }
        // Either half of the product switches the stage off: a floor at unity removes nothing, and
        // a mix with no wet in it plays the dry signal.
        let mut dry = DenoiseLevel::Strong.control();
        dry.wet_dry = 0.0;
        assert!(!dry.is_active());
        let mut wire = DenoiseLevel::Strong.control();
        wire.max_suppression_db = 0.0;
        assert!(!wire.is_active());
    }

    #[test]
    fn the_gain_floor_is_the_decibel_inverse_of_the_suppression() {
        assert_eq!(DenoiseLevel::Off.control().gain_floor(), 1.0);
        let strong = DenoiseLevel::Strong.control().gain_floor();
        assert!(
            (strong - 0.001).abs() < 1e-6,
            "60 dB is a thousandth: {strong}"
        );
        let medium = DenoiseLevel::Medium.control().gain_floor();
        assert!((medium - 0.063_095_7).abs() < 1e-5, "24 dB: {medium}");
    }

    #[test]
    fn the_default_control_row_is_the_default_levels_row() {
        assert_eq!(DenoiseControl::default(), DenoiseLevel::default().control());
    }

    #[test]
    fn a_control_row_deserialises_from_a_partial_table() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            denoise: DenoiseControl,
        }
        // Only one field spelled out: the rest come from the default row rather than failing the
        // parse, so a preset can override the one number it cares about.
        let parsed: Wrapper =
            toml::from_str("[denoise]\nmax_suppression_db = 40.0\n").expect("parse");
        assert_eq!(parsed.denoise.max_suppression_db, 40.0);
        assert_eq!(
            parsed.denoise.vad_threshold,
            DenoiseControl::default().vad_threshold
        );
    }

    #[test]
    fn a_corrupt_control_row_falls_back_to_its_level_and_an_excessive_one_is_clamped() {
        let mut corrupt = DenoiseControl {
            max_suppression_db: f32::NAN,
            vad_threshold: f32::INFINITY,
            voice_preservation: f32::NEG_INFINITY,
            wet_dry: f32::NAN,
        };
        corrupt.sanitise(DenoiseLevel::Light.control());
        assert_eq!(corrupt, DenoiseLevel::Light.control(), "NaN survives clamp");

        let mut excessive = DenoiseControl {
            max_suppression_db: 500.0,
            vad_threshold: 3.0,
            voice_preservation: -1.0,
            wet_dry: 2.0,
        };
        excessive.sanitise(DenoiseLevel::Light.control());
        assert_eq!(
            excessive,
            DenoiseControl {
                max_suppression_db: *limits::DENOISE_MAX_SUPPRESSION_DB.end(),
                vad_threshold: 1.0,
                voice_preservation: 0.0,
                wet_dry: 1.0,
            }
        );
    }

    #[test]
    fn an_override_of_preset_follows_the_preset_and_anything_else_pins_it() {
        for level in DenoiseLevel::ALL {
            assert_eq!(NoiseSuppressionOverride::Preset.resolve(level), level);
            assert_eq!(
                NoiseSuppressionOverride::Strong.resolve(level),
                DenoiseLevel::Strong
            );
            assert_eq!(
                NoiseSuppressionOverride::Off.resolve(level),
                DenoiseLevel::Off
            );
        }
        assert_eq!(NoiseSuppressionOverride::Preset.level(), None);
        assert_eq!(
            NoiseSuppressionOverride::Light.level(),
            Some(DenoiseLevel::Light)
        );
        for mode in DenoiseChannelMode::ALL {
            assert_eq!(DenoiseChannelsOverride::Preset.resolve(mode), mode);
            assert_eq!(
                DenoiseChannelsOverride::Mono.resolve(mode),
                DenoiseChannelMode::Mono
            );
        }
        assert_eq!(DenoiseChannelsOverride::Preset.mode(), None);
        assert_eq!(
            DenoiseChannelsOverride::Linked.mode(),
            Some(DenoiseChannelMode::Linked)
        );
        // The override's keys are the mode's keys plus `preset`, so a CLI that accepts one accepts
        // the other.
        for mode in DenoiseChannelMode::ALL {
            assert_eq!(
                DenoiseChannelsOverride::from_key(mode.key()).and_then(|c| c.mode()),
                Some(mode)
            );
        }
        for level in DenoiseLevel::ALL {
            assert_eq!(
                NoiseSuppressionOverride::from_key(level.key()).and_then(|c| c.level()),
                Some(level)
            );
        }
    }

    #[test]
    fn each_direction_knows_the_other() {
        assert_eq!(DeviceDirection::Output.other(), DeviceDirection::Input);
        assert_eq!(DeviceDirection::Input.other(), DeviceDirection::Output);
        assert_eq!(
            DeviceDirection::ALL,
            [DeviceDirection::Output, DeviceDirection::Input],
            "outputs first, the order every device list keeps"
        );
    }
}
