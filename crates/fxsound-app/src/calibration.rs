//! The microphone calibration wizard's state machine (0.4.0 design §8, upstream U9).
//!
//! The wizard in `fxsound_ui::dialogs::calibration` only draws; this is what times it and turns
//! what the input lane measured into a voice preset:
//!
//! ```text
//! Intro ─Start─► Waking ─delivering─► Silence 3 s ─► Speech 5 s ─► Loud 2 s ─► Analysing ─► Result
//!                  │ 5 s                 │               │             │                      │
//!                  └───────────────────► Failed ◄────────┴─────────────┘      Retry ◄─────────┘
//! ```
//!
//! **Waking.** Start asks the engine to hold the microphone open ([`Command::KeepInputAwake`]):
//! the input lane's capture is passive while nobody records from FxSound (Input), and a Bluetooth
//! headset only switches to its microphone profile when something does. Nothing is measured until
//! the lane is attached to a microphone and processing — and a Bluetooth microphone until it
//! delivers something other than digital zeros, which its loopback carries until the profile
//! switch has finished; five seconds without that is a failure that says what was missing.
//!
//! **Measuring.** Each timed phase zeroes the input lane's capture accumulators on entry
//! ([`Command::ResetCaptureStats`]) and reads them on exit — frames, sum of squares, peak and
//! clipped samples, all taken *before* the voice chain, so the preset running now does not colour
//! the measurement. The silence phase's floor is its quietest ten-millisecond block, digital zeros
//! left out, which the lane also restarts on each reset; the phase also averages the lane's
//! spectrum for the high-pass choice.
//!
//! **Release.** The microphone is let go as soon as nothing more is to be measured: on the way to
//! Analysing, on every failure, and on Cancel, whatever the phase. [`CalibrationState`] keeps the
//! request paired itself — at most one hold outstanding, released exactly once — so a host that
//! carries out every [`Command`] it is handed cannot leave a microphone held.
//!
//! **Result.** [`Recommendation`] holds the formulas of design §8. Applying it — writing the user
//! voice preset `Calibrated — <device>` and selecting it — is the controller's
//! ([`crate::App::handle_calibration`]), since that is where the preset store is.
//!
//! Pure: no clock, no engine, no store. The host passes the time and a snapshot of the input lane
//! ([`Lane`]) to every call, which is what lets the tests script a whole run from a table of
//! meter readings.

use std::time::{Duration, Instant};

use fxsound_core::i18n::{tr, tr_args};
use fxsound_core::messages::Meters;
use fxsound_core::{DenoiseLevel, Detection, NUM_SPECTRUM_BARS, limits};
use fxsound_preset::input::{Compressor, Denoise, Gate, InputPreset};
use fxsound_ui::dialogs::{CalibrationPhase, CalibrationResultView, CalibrationView};

use crate::app::FORBIDDEN_PRESET_NAME_CHARS;

/// How long the input lane has, after Start, to be attached to a microphone and processing. A
/// Bluetooth headset switching to its hands-free profile takes one to two seconds.
pub const WAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long "Analysing…" stays up. The arithmetic takes microseconds; a page that flashed past
/// in one frame would read as a glitch between the last measurement and the result.
pub const ANALYSIS_TIME: Duration = Duration::from_millis(500);

/// What a level with nothing in it reads as, in dBFS: the bottom of every conversion here, so a
/// silent phase is a number the formulas and the settings file can carry rather than `-inf`.
pub const SILENT_DB: f32 = -120.0;

/// A level at or under this is digital zeros, not a room: the input lane's levels stop at
/// −100 dBFS, so only a capture of exact zeros gets there.
pub const DIGITAL_SILENCE_DB: f32 = -100.0;

/// How far under the silence phase's RMS the floor may be put. The floor is the phase's quietest
/// ten-millisecond block, and in a steady room every block is within a decibel or two of the RMS:
/// one further down than this is a quiet moment in a room that is not steady — a fan between
/// cycles — rather than the room a gate has to stay closed on.
pub const FLOOR_SPREAD_DB: f32 = 6.0;

/// How far over the floor the speech phase has to be for there to have been speech in it. Its
/// RMS averages the pauses in, so real speech is 15 to 30 dB over a quiet room.
pub const SPEECH_MARGIN_DB: f32 = 6.0;

/// The share of the loud phase's samples that may clip before the ceiling comes down (0.1 %).
pub const CLIPPING_LIMIT: f32 = 0.001;

/// The share of the silence's energy the two lowest spectrum bands (42–133 Hz) may carry before
/// the high-pass goes up to 120 Hz: more than this is rumble — a desk, a fan, traffic — that an
/// 80 Hz corner lets through.
pub const LOW_BAND_LIMIT: f32 = 0.30;

/// The speech level a calibrated preset aims for after its compressor, dBFS RMS.
pub const TARGET_RMS_DB: f32 = -18.0;

/// What the compressor is expected to take off speech that sits 6 dB over its threshold at 3:1.
pub const EXPECTED_COMPRESSION_DB: f32 = 4.0;

/// The makeup is held to this either way: a microphone that needs more is misconfigured in the
/// mixer, and the wizard should not hide it (upstream #505 asked for ±18).
pub const MAKEUP_LIMIT_DB: f32 = 18.0;

/// The compressor's ratio in every calibrated preset.
pub const COMPRESSOR_RATIO: f32 = 3.0;

/// The shipped voice presets a calibration starts from.
pub const HEADSET: &str = "Headset";
pub const LAPTOP_MIC: &str = "Laptop Mic";
pub const CLEAN_VOICE: &str = "Clean Voice";

// =============================================================================================
// What the host tells the machine, and what the machine asks of the host
// =============================================================================================

/// A microphone as the input lane is attached to it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Microphone {
    /// `node.name`: what a change of microphone is noticed by, and what the settings record keeps.
    pub node_name: String,
    /// `node.description`: what the wizard shows and the preset is named after.
    pub description: String,
}

impl Microphone {
    /// Whether this is a Bluetooth microphone: a node the bluez5 plugin or WirePlumber's loopback
    /// in front of it made (`bluez_input.<address>…`, and PulseAudio's `bluez_source.…`).
    ///
    /// By name, because the device list carries no bus: both generations of WirePlumber name the
    /// node this way, and the name is the one property every Bluetooth microphone has.
    #[must_use]
    pub fn is_bluetooth(&self) -> bool {
        self.node_name.starts_with("bluez_")
    }
}

/// The input lane as the host sees it at one moment.
#[derive(Debug, Clone, Copy)]
pub struct Lane<'a> {
    /// What the lane is attached to — `node.name` and description — or `None` while it has no
    /// microphone.
    pub microphone: Option<(&'a str, &'a str)>,
    /// Whether the engine says buffers are flowing through the lane.
    pub processing: bool,
    /// The lane's channel count, for the share of *samples* that clipped.
    pub channels: u16,
    /// The lane's latest meters.
    pub meters: &'a Meters,
}

impl Lane<'_> {
    fn is_on(&self, microphone: &Microphone) -> bool {
        self.microphone
            .is_some_and(|(node, _)| node == microphone.node_name)
    }

    fn microphone(&self) -> Option<Microphone> {
        self.microphone.map(|(node, description)| Microphone {
            node_name: node.to_owned(),
            description: description.to_owned(),
        })
    }
}

/// What the host has to do for the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Send `UiToAudio::KeepInputAwake`.
    KeepInputAwake(bool),
    /// Send `DspEvent::ResetCaptureStats` to the input lane.
    ResetCaptureStats,
}

/// Why a run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The input lane was attached to nothing for the whole wake-up.
    NoMicrophone,
    /// Attached, but nothing flowed through the lane in the wake-up's five seconds.
    NotStarted,
    /// A phase captured nothing, or the lane stopped, or a Bluetooth microphone handed over
    /// nothing but digital zeros — a headset still in its music profile — through the wake-up
    /// or the silence.
    NoSignal,
    /// The speech phase was no louder than the silence.
    NoSpeech,
    /// The lane moved to another microphone, or lost its microphone, in the middle of a run.
    MicrophoneChanged,
}

impl Failure {
    /// The reason, in the interface's language.
    #[must_use]
    pub fn text(self) -> String {
        tr(match self {
            Self::NoMicrophone => "No microphone is selected.",
            Self::NotStarted => "The microphone sent no sound within 5 seconds.",
            Self::NoSignal => "The microphone delivered no signal.",
            Self::NoSpeech => "No speech was heard. Speak closer to the microphone.",
            Self::MicrophoneChanged => "The microphone changed during the measurement.",
        })
    }
}

// =============================================================================================
// Measurements and the recommendation
// =============================================================================================

/// One phase's capture accumulators, as read on its way out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub frames: u64,
    /// RMS over the phase, dBFS, never under [`SILENT_DB`].
    pub rms_db: f32,
    /// Largest sample, dBFS, never under [`SILENT_DB`].
    pub peak_db: f32,
    /// Samples (not frames) at full scale.
    pub clipped: u64,
    /// The phase's quietest ten-millisecond block with anything in it, RMS in dBFS; −100 when
    /// every block was digital zeros.
    pub quietest_db: f32,
}

impl Reading {
    /// The accumulators in `meters`, which have counted since the phase's reset.
    #[must_use]
    pub fn of(meters: &Meters) -> Self {
        let rms_db = if meters.capture_frames == 0 {
            SILENT_DB
        } else {
            power_db(meters.capture_sum_squares / meters.capture_frames as f64)
        };
        Self {
            frames: meters.capture_frames,
            rms_db,
            peak_db: amplitude_db(meters.capture_peak),
            clipped: meters.capture_clipped,
            quietest_db: meters.capture_floor_db,
        }
    }
}

/// `10·log10` of a mean square, held to `SILENT_DB..=+20` dB so nothing downstream sees `-inf`.
fn power_db(mean_square: f64) -> f32 {
    if mean_square.is_finite() && mean_square > 0.0 {
        ((10.0 * mean_square.log10()) as f32).clamp(SILENT_DB, 20.0)
    } else {
        SILENT_DB
    }
}

/// `20·log10` of an amplitude, held the same way.
fn amplitude_db(amplitude: f32) -> f32 {
    power_db(f64::from(amplitude) * f64::from(amplitude))
}

/// The room's floor from the silence phase: its quietest block, held between the phase's RMS and
/// [`FLOOR_SPREAD_DB`] under it.
///
/// The RMS alone would take a cough in the silence for the room, and the quietest block — deaf
/// to a cough — alone would take one quiet moment of an unsteady room for all of it. Each covers
/// the other's blind spot. The block is the phase's own: the lane's running floor estimator,
/// which this used to read, is still climbing out of whatever silence came before the phase at
/// half a decibel a second — a Bluetooth headset's zeros before its profile switch, a hardware
/// mute lifted just before Start — and put the floor the whole six decibels under the room.
#[must_use]
pub fn floor_db(silence: &Reading) -> f32 {
    let quietest = if silence.quietest_db.is_finite() {
        silence.quietest_db
    } else {
        silence.rms_db
    };
    quietest
        .max(silence.rms_db - FLOOR_SPREAD_DB)
        .min(silence.rms_db)
}

/// The two lowest spectrum bands' share of the energy, from the lane's spectrum summed over the
/// silence phase.
///
/// The spectrum is the visualizer's: each band's level is warped for the look of the bars
/// (`fxsound_dsp::spectrum::BAND_WARP`), so the warp is divided out before the bands are
/// compared. It is also taken after the voice chain, so a high-pass the running preset already
/// has hides some of the rumble it would measure; what it reports is the rumble still getting
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LowBands {
    energy: [f64; NUM_SPECTRUM_BARS],
}

impl LowBands {
    /// Add one reading of the spectrum.
    pub fn add(&mut self, spectrum: &[f32; NUM_SPECTRUM_BARS]) {
        for ((sum, level), warp) in self
            .energy
            .iter_mut()
            .zip(spectrum)
            .zip(fxsound_dsp::spectrum::BAND_WARP)
        {
            if level.is_finite() {
                let unwarped = f64::from(*level) / f64::from(warp);
                *sum += unwarped * unwarped;
            }
        }
    }

    /// The share of the two lowest bands, `0.0..=1.0`; zero when nothing was heard at all.
    #[must_use]
    pub fn share(&self) -> f32 {
        let total: f64 = self.energy.iter().sum();
        if total > 0.0 && total.is_finite() {
            ((self.energy[0] + self.energy[1]) / total) as f32
        } else {
            0.0
        }
    }
}

/// Everything a calibration measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// The room's floor ([`floor_db`]), dBFS.
    pub floor_db: f32,
    /// The silence phase's RMS, dBFS.
    pub silence_rms_db: f32,
    /// The two lowest bands' share of the silence ([`LowBands::share`]).
    pub low_band_share: f32,
    /// The speech phase's RMS, pauses and all, dBFS.
    pub speech_rms_db: f32,
    /// The speech phase's peak, dBFS.
    pub speech_peak_db: f32,
    /// The loud phase's clipped samples over all its samples, `0.0..=1.0`.
    pub clipped_ratio: f32,
}

/// What the wizard suggests, with the formulas of design §8.
#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    /// The shipped voice preset the calibrated one is written over.
    pub preset: &'static str,
    pub highpass_hz: f32,
    pub gate_threshold_db: f32,
    pub compressor_threshold_db: f32,
    pub compressor_ratio: f32,
    pub makeup_db: f32,
    pub ceiling_db: f32,
    pub denoise: DenoiseLevel,
}

impl Recommendation {
    /// Work out the recommendation from what was measured.
    ///
    /// Whole decibels, since a person reads the preset file: the gate rounded *up*, so it never
    /// comes nearer the floor than the four decibels the formula promises, the rest to the
    /// nearest. Each number is then held to the range the voice chain accepts.
    #[must_use]
    pub fn from_measurement(m: &Measurement) -> Self {
        let floor = m.floor_db;
        let speech = m.speech_rms_db;

        // 120 Hz when the silence is mostly rumble, the 80 Hz every voice gets otherwise.
        let highpass_hz = if m.low_band_share > LOW_BAND_LIMIT {
            120.0
        } else {
            80.0
        };

        // clamp(floor + 8, floor + 4, speech − 12): eight over the floor, backed off to twelve
        // under speech for a quiet talker, but never within four of the floor — a gate that
        // opens on the room is worse than one that clips a soft syllable. `f32::clamp` would
        // panic where the bounds cross, which is exactly the close case; the lower bound wins.
        let gate = (floor + 8.0).min(speech - 12.0).max(floor + 4.0);
        let gate_threshold_db = clamp_to(gate.ceil(), &limits::GATE_THRESHOLD_DB);

        let compressor_threshold_db =
            clamp_to((speech - 6.0).round(), &limits::COMPRESSOR_THRESHOLD_DB);

        // Speech lands at −18 dBFS RMS after ~4 dB of compression.
        let makeup_db = (TARGET_RMS_DB - (speech - EXPECTED_COMPRESSION_DB))
            .round()
            .clamp(-MAKEUP_LIMIT_DB, MAKEUP_LIMIT_DB);

        let ceiling_db = if m.clipped_ratio > CLIPPING_LIMIT {
            -6.0
        } else {
            -3.0
        };

        let denoise = if floor < -60.0 {
            DenoiseLevel::Off
        } else if floor < -50.0 {
            DenoiseLevel::Light
        } else if floor < -40.0 {
            DenoiseLevel::Medium
        } else {
            DenoiseLevel::Strong
        };

        // A boom a few centimetres from the mouth has a low crest: the proximity effect fills the
        // pauses and the peaks never get far over the average.
        let preset = if m.speech_peak_db - speech < 10.0 {
            HEADSET
        } else if floor > -45.0 {
            LAPTOP_MIC
        } else {
            CLEAN_VOICE
        };

        Self {
            preset,
            highpass_hz,
            gate_threshold_db,
            compressor_threshold_db,
            compressor_ratio: COMPRESSOR_RATIO,
            makeup_db,
            ceiling_db,
            denoise,
        }
    }

    /// `base` — the shipped preset named by [`Self::preset`] — with the recommended numbers
    /// written over it, as `name`.
    ///
    /// What the wizard does not measure stays the preset's: the high-pass order, the equalizer,
    /// the de-esser, the chain, the gate's and the compressor's timing. The gate and the
    /// compressor are switched on whatever the base said, with RMS detection, since that is what
    /// the thresholds were measured as. A denoiser at the level the base already had keeps the
    /// base's tuning of it.
    #[must_use]
    pub fn preset(&self, base: InputPreset, name: &str, description: String) -> InputPreset {
        let defaults = InputPreset::default();
        let mut preset = base;
        preset.name = name.to_owned();
        preset.description = description;

        preset.highpass_hz = self.highpass_hz;
        if preset.highpass_order == 0 {
            preset.highpass_order = defaults.highpass_order;
        }

        let gate = preset.gate.take().or(defaults.gate);
        preset.gate = gate.map(|gate| Gate {
            threshold_db: self.gate_threshold_db,
            detection: Detection::Rms,
            ..gate
        });
        let compressor = preset.compressor.take().or(defaults.compressor);
        preset.compressor = compressor.map(|compressor| Compressor {
            threshold_db: self.compressor_threshold_db,
            ratio: self.compressor_ratio,
            detection: Detection::Rms,
            ..compressor
        });

        preset.makeup_db = self.makeup_db;
        preset.ceiling_db = self.ceiling_db;

        if preset.denoise_level() != self.denoise {
            let channels = preset.denoise_channels();
            preset.denoise = (self.denoise != DenoiseLevel::Off).then(|| Denoise {
                level: self.denoise,
                channels,
                ..Denoise::default()
            });
        }
        preset.rnnoise = self.denoise != DenoiseLevel::Off;
        preset
    }

    /// The recommendation as the wizard lists it, one translated line per setting.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let denoise = tr(self.denoise.label());
        vec![
            tr_args("High-pass %s", &[&format!("{:.0} Hz", self.highpass_hz)]),
            tr_args("Gate %s", &[&signed_db(self.gate_threshold_db)]),
            tr_args(
                "Compressor %s, ratio %s",
                &[
                    &signed_db(self.compressor_threshold_db),
                    &format!("{:.0}:1", self.compressor_ratio),
                ],
            ),
            tr_args("Makeup gain %s", &[&signed_db(self.makeup_db)]),
            tr_args("Ceiling %s", &[&signed_db(self.ceiling_db)]),
            tr_args("Noise suppression: %s", &[&denoise]),
        ]
    }
}

/// `value` held to `range`.
fn clamp_to(value: f32, range: &std::ops::RangeInclusive<f32>) -> f32 {
    value.clamp(*range.start(), *range.end())
}

/// `+6 dB`, `−42 dB`, `0 dB`: whole decibels with a sign and a real minus, as a gain reads.
#[must_use]
pub fn signed_db(db: f32) -> String {
    let rounded = db.round();
    if rounded > 0.0 {
        format!("+{rounded:.0} dB")
    } else if rounded < 0.0 {
        format!("−{:.0} dB", -rounded)
    } else {
        "0 dB".to_owned()
    }
}

/// The name of the voice preset a calibration of `description` writes: `Calibrated — <device>`.
///
/// Held to what a typed name may be — no character the name editor filters out, no control
/// characters, at most [`crate::app::MAX_PRESET_NAME_CHARS`] characters and
/// [`fxsound_preset::MAX_NAME_BYTES`] bytes — so the preset can be renamed, exported and copied to
/// Windows like one the user named.
/// Not translated: a preset's name is written as it is in every language, like the shipped ones.
#[must_use]
pub fn calibrated_preset_name(description: &str) -> String {
    let cleaned: String = description
        .chars()
        .filter(|c| !FORBIDDEN_PRESET_NAME_CHARS.contains(*c) && !c.is_control())
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = if cleaned.is_empty() {
        "Calibrated".to_owned()
    } else {
        format!("Calibrated — {cleaned}")
    };
    // At most sixty-four characters and the 126 bytes a Windows FxSound reads a name in (0.4.0
    // audit #15): "Calibrated — " and a Cyrillic device name reach the bytes first.
    fxsound_preset::new_preset_name(&name)
}

/// A finished calibration: the microphone, what was measured on it and what that suggests.
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    pub microphone: Microphone,
    pub measurement: Measurement,
    pub recommendation: Recommendation,
}

impl Calibration {
    /// The result table and the recommendation's lines, for the wizard.
    #[must_use]
    pub fn view(&self) -> CalibrationResultView {
        let m = &self.measurement;
        CalibrationResultView {
            floor_db: m.floor_db,
            speech_rms_db: m.speech_rms_db,
            speech_peak_db: m.speech_peak_db,
            clipped_percent: m.clipped_ratio * 100.0,
            preset: self.recommendation.preset.to_owned(),
            lines: self.recommendation.lines(),
        }
    }

    /// The preset file's description: where its numbers came from, in English like the shipped
    /// set's descriptions.
    #[must_use]
    pub fn description(&self) -> String {
        let m = &self.measurement;
        format!(
            "Written by the calibration wizard over {} for {}: floor {:.0} dBFS, speech {:.0} dBFS \
             RMS with peaks at {:.0} dBFS.",
            self.recommendation.preset,
            self.microphone.description,
            m.floor_db,
            m.speech_rms_db,
            m.speech_peak_db,
        )
    }
}

// =============================================================================================
// The state machine
// =============================================================================================

/// Where the machine is. [`CalibrationPhase`] is the wizard's page; this adds the wake-up, which
/// the wizard shows as the silence page before its countdown starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Intro,
    Waking,
    Silence,
    Speech,
    Loud,
    Analysing,
    Result,
    Failed,
}

/// The calibration wizard's state (0.4.0 design §8), owned by the controller while the wizard is
/// open.
#[derive(Debug, Clone)]
pub struct CalibrationState {
    stage: Stage,
    /// When the current stage began.
    since: Instant,
    /// The microphone being calibrated: the lane's at Start, and followed while nothing is being
    /// measured.
    microphone: Microphone,
    /// Whether there is a microphone attached to measure, as of the last look at the lane.
    can_measure: bool,
    /// The lane's short-window level, for the wizard's live bar; `-inf` while nothing flows.
    level_db: f32,
    /// A `KeepInputAwake(true)` is outstanding.
    awake: bool,
    silence: Option<Reading>,
    speech: Option<Reading>,
    loud: Option<Reading>,
    low_bands: LowBands,
    /// The lane's channel count during the loud phase.
    channels: u16,
    result: Option<Calibration>,
    failure: Option<Failure>,
}

impl CalibrationState {
    /// The introduction, for whatever microphone `lane` is attached to.
    #[must_use]
    pub fn open(lane: &Lane<'_>, now: Instant) -> Self {
        Self {
            stage: Stage::Intro,
            since: now,
            microphone: lane.microphone().unwrap_or_default(),
            can_measure: lane.microphone.is_some(),
            level_db: f32::NEG_INFINITY,
            awake: false,
            silence: None,
            speech: None,
            loud: None,
            low_bands: LowBands::default(),
            channels: 1,
            result: None,
            failure: None,
        }
    }

    /// The wizard's page.
    #[must_use]
    pub const fn phase(&self) -> CalibrationPhase {
        match self.stage {
            Stage::Intro => CalibrationPhase::Intro,
            Stage::Waking | Stage::Silence => CalibrationPhase::Silence,
            Stage::Speech => CalibrationPhase::Speech,
            Stage::Loud => CalibrationPhase::Loud,
            Stage::Analysing => CalibrationPhase::Analysing,
            Stage::Result => CalibrationPhase::Result,
            Stage::Failed => CalibrationPhase::Failed,
        }
    }

    /// Whether the machine is doing something over time — waking the lane, measuring, analysing
    /// — so the host has to keep calling [`Self::tick`] and redrawing at the meters' pace.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(
            self.stage,
            Stage::Waking | Stage::Silence | Stage::Speech | Stage::Loud | Stage::Analysing
        )
    }

    /// Whether the microphone is being held open.
    #[must_use]
    pub const fn is_awake(&self) -> bool {
        self.awake
    }

    /// Why the run failed, in the Failed phase.
    #[must_use]
    pub const fn failure(&self) -> Option<Failure> {
        self.failure
    }

    /// The finished calibration, in the Result phase.
    #[must_use]
    pub const fn result(&self) -> Option<&Calibration> {
        self.result.as_ref()
    }

    /// Start — or Retry, which is the same from the top: hold the microphone open and wait for
    /// the lane. Does nothing while a run is going, or without a microphone to measure.
    pub fn start(&mut self, now: Instant) -> Vec<Command> {
        let startable = matches!(self.stage, Stage::Intro | Stage::Result | Stage::Failed);
        if !startable || !self.can_measure {
            return Vec::new();
        }
        self.silence = None;
        self.speech = None;
        self.loud = None;
        self.low_bands = LowBands::default();
        self.result = None;
        self.failure = None;
        self.enter(Stage::Waking, now);
        let mut commands = Vec::new();
        if !self.awake {
            self.awake = true;
            commands.push(Command::KeepInputAwake(true));
        }
        commands
    }

    /// Leave the wizard, whatever it was doing: let go of the microphone if it is held.
    pub fn cancel(&mut self) -> Vec<Command> {
        self.release()
    }

    /// Look at the lane again at `now`: follow the microphone, take the level, and move on when a
    /// phase is over or something went wrong.
    pub fn tick(&mut self, now: Instant, lane: &Lane<'_>) -> Vec<Command> {
        self.can_measure = lane.microphone.is_some();
        self.level_db = if lane.processing && lane.microphone.is_some() {
            lane.meters.input_rms_db
        } else {
            f32::NEG_INFINITY
        };
        let elapsed = now.saturating_duration_since(self.since);

        match self.stage {
            // Between runs the wizard is about whatever the lane is on now; a result stays about
            // the microphone it measured.
            Stage::Intro | Stage::Failed => {
                if let Some((node, description)) = lane.microphone
                    && (node != self.microphone.node_name
                        || description != self.microphone.description)
                {
                    self.microphone = Microphone {
                        node_name: node.to_owned(),
                        description: description.to_owned(),
                    };
                }
                Vec::new()
            }
            Stage::Result => Vec::new(),
            Stage::Waking => {
                if lane.processing
                    && let Some(microphone) = lane.microphone()
                {
                    // A Bluetooth headset's loopback runs before the headset has switched to its
                    // hands-free profile, and carries digital zeros until it has: a silence phase
                    // begun then averaged a second or two of them into the room and read the
                    // floor up to eleven decibels low. It begins with the first sound instead.
                    let sounding = lane.meters.input_rms_db > DIGITAL_SILENCE_DB;
                    if microphone.is_bluetooth() && !sounding {
                        return if elapsed >= WAKE_TIMEOUT {
                            self.fail(Failure::NoSignal)
                        } else {
                            Vec::new()
                        };
                    }
                    self.microphone = microphone;
                    self.enter(Stage::Silence, now);
                    vec![Command::ResetCaptureStats]
                } else if elapsed >= WAKE_TIMEOUT {
                    self.fail(if lane.microphone.is_some() {
                        Failure::NotStarted
                    } else {
                        Failure::NoMicrophone
                    })
                } else {
                    Vec::new()
                }
            }
            Stage::Silence | Stage::Speech | Stage::Loud => self.measure(now, elapsed, lane),
            Stage::Analysing => {
                if elapsed >= ANALYSIS_TIME {
                    self.analyse(now);
                }
                Vec::new()
            }
        }
    }

    /// One tick of a timed phase.
    fn measure(&mut self, now: Instant, elapsed: Duration, lane: &Lane<'_>) -> Vec<Command> {
        if !lane.is_on(&self.microphone) {
            return self.fail(Failure::MicrophoneChanged);
        }
        if self.stage == Stage::Silence && lane.processing {
            self.low_bands.add(&lane.meters.spectrum);
        }
        let length = Duration::from_secs_f32(self.phase().seconds());
        if elapsed < length {
            return Vec::new();
        }

        // The phase is over: its accumulators have counted since its reset.
        let reading = Reading::of(lane.meters);
        if !lane.processing || reading.frames == 0 {
            return self.fail(Failure::NoSignal);
        }
        match self.stage {
            Stage::Silence => {
                // A headset still in its music profile hands the loopback digital zeros: nothing
                // measured on it means anything (U9).
                if self.microphone.is_bluetooth() && floor_db(&reading) < DIGITAL_SILENCE_DB {
                    return self.fail(Failure::NoSignal);
                }
                self.silence = Some(reading);
                self.enter(Stage::Speech, now);
                vec![Command::ResetCaptureStats]
            }
            Stage::Speech => {
                let floor = self.silence.as_ref().map_or(SILENT_DB, floor_db);
                if reading.rms_db < DIGITAL_SILENCE_DB {
                    return self.fail(Failure::NoSignal);
                }
                if reading.rms_db < floor + SPEECH_MARGIN_DB {
                    return self.fail(Failure::NoSpeech);
                }
                self.speech = Some(reading);
                self.enter(Stage::Loud, now);
                vec![Command::ResetCaptureStats]
            }
            _ => {
                self.loud = Some(reading);
                self.channels = lane.channels.max(1);
                self.enter(Stage::Analysing, now);
                // Nothing more to listen to.
                self.release()
            }
        }
    }

    /// Turn the three readings into the result.
    fn analyse(&mut self, now: Instant) {
        let (Some(silence), Some(speech), Some(loud)) = (self.silence, self.speech, self.loud)
        else {
            self.fail(Failure::NoSignal);
            return;
        };
        let samples = loud.frames.saturating_mul(u64::from(self.channels)).max(1);
        let measurement = Measurement {
            floor_db: floor_db(&silence),
            silence_rms_db: silence.rms_db,
            low_band_share: self.low_bands.share(),
            speech_rms_db: speech.rms_db,
            speech_peak_db: speech.peak_db,
            clipped_ratio: (loud.clipped as f64 / samples as f64).clamp(0.0, 1.0) as f32,
        };
        self.result = Some(Calibration {
            microphone: self.microphone.clone(),
            recommendation: Recommendation::from_measurement(&measurement),
            measurement,
        });
        self.enter(Stage::Result, now);
    }

    fn enter(&mut self, stage: Stage, now: Instant) {
        self.stage = stage;
        self.since = now;
    }

    /// Stop with `failure`, letting go of the microphone.
    fn fail(&mut self, failure: Failure) -> Vec<Command> {
        self.stage = Stage::Failed;
        self.failure = Some(failure);
        self.release()
    }

    /// The `KeepInputAwake(false)` that pairs an outstanding `true`, once.
    fn release(&mut self) -> Vec<Command> {
        if std::mem::take(&mut self.awake) {
            vec![Command::KeepInputAwake(false)]
        } else {
            Vec::new()
        }
    }

    /// What the wizard draws at `now`.
    #[must_use]
    pub fn view(&self, now: Instant) -> CalibrationView {
        let elapsed = now.saturating_duration_since(self.since).as_secs_f32();
        let phase = self.phase();
        let (seconds_left, phase_fraction) = match self.stage {
            // The countdown waits for the lane.
            Stage::Waking => (phase.seconds(), 0.0),
            Stage::Silence | Stage::Speech | Stage::Loud => {
                let length = phase.seconds();
                let gone = elapsed.min(length);
                (length - gone, gone / length)
            }
            Stage::Analysing => (0.0, (elapsed / ANALYSIS_TIME.as_secs_f32()).min(1.0)),
            Stage::Result => (0.0, 1.0),
            Stage::Intro | Stage::Failed => (0.0, 0.0),
        };
        CalibrationView {
            phase,
            seconds_left,
            phase_fraction,
            level_db: self.level_db,
            result: self.result.as_ref().map(Calibration::view),
            device: self.microphone.description.clone(),
            failure: self.failure.map(Failure::text).unwrap_or_default(),
            can_measure: self.can_measure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIC: (&str, &str) = ("alsa_input.usb-fifine", "fifine Microphone Analogue Stereo");
    const OTHER: (&str, &str) = (
        "alsa_input.pci-0000_00_1f.3",
        "Built-in Audio Analogue Stereo",
    );
    const HEADPHONES: (&str, &str) = ("bluez_input.00:11:22:33:44:55", "WH-1000XM4");

    /// What the input lane's accumulators hold after `seconds` of a signal at `rms_db` RMS
    /// peaking at `peak_db`, with `clipped` samples at full scale, at 48 kHz — its quietest block
    /// at `floor_db`, and the lane's running floor estimator settled there too.
    fn counted(seconds: f32, rms_db: f32, peak_db: f32, clipped: u64, floor_db: f32) -> Meters {
        let frames = (seconds * 48_000.0) as u64;
        Meters {
            capture_frames: frames,
            capture_sum_squares: 10_f64.powf(f64::from(rms_db) / 10.0) * frames as f64,
            capture_peak: 10_f32.powf(peak_db / 20.0),
            capture_clipped: clipped,
            capture_floor_db: floor_db,
            noise_floor_db: floor_db,
            input_rms_db: rms_db,
            ..Meters::default()
        }
    }

    /// A quiet home office: a −55 dBFS room, its quietest block a couple of decibels under it.
    fn quiet_room() -> Meters {
        counted(3.0, -55.0, -40.0, 0, -57.0)
    }

    /// Normal speech from a desk microphone: −28 dBFS RMS with the pauses, peaks at −8.
    fn talking() -> Meters {
        counted(5.0, -28.0, -8.0, 0, -56.0)
    }

    /// Loud speech that never reaches full scale.
    fn shouting() -> Meters {
        counted(2.0, -16.0, -2.0, 0, -56.0)
    }

    fn lane<'a>(microphone: Option<(&'a str, &'a str)>, meters: &'a Meters) -> Lane<'a> {
        Lane {
            microphone,
            processing: true,
            channels: 1,
            meters,
        }
    }

    fn at(t0: Instant, seconds: f32) -> Instant {
        t0 + Duration::from_secs_f32(seconds)
    }

    /// The wizard opened on `microphone` at `t0`, Start pressed and the lane found processing at
    /// once: the silence phase has just begun. The commands so far are returned with it.
    fn measuring(microphone: (&str, &str), t0: Instant) -> (CalibrationState, Vec<Command>) {
        let idle = Meters::default();
        let mut wizard = CalibrationState::open(&lane(Some(microphone), &idle), t0);
        let mut commands = wizard.start(t0);
        commands.extend(wizard.tick(t0, &lane(Some(microphone), &idle)));
        assert_eq!(wizard.phase(), CalibrationPhase::Silence);
        (wizard, commands)
    }

    /// A whole run on `microphone` with these three readings, each handed over at the end of its
    /// phase, and the analysis let finish. Returns the wizard and every command it gave.
    fn run(
        microphone: (&str, &str),
        silence: &Meters,
        speech: &Meters,
        loud: &Meters,
    ) -> (CalibrationState, Vec<Command>) {
        let t0 = Instant::now();
        let (mut wizard, mut commands) = measuring(microphone, t0);
        commands.extend(wizard.tick(at(t0, 3.0), &lane(Some(microphone), silence)));
        commands.extend(wizard.tick(at(t0, 8.0), &lane(Some(microphone), speech)));
        commands.extend(wizard.tick(at(t0, 10.0), &lane(Some(microphone), loud)));
        commands.extend(wizard.tick(at(t0, 10.5), &lane(Some(microphone), loud)));
        (wizard, commands)
    }

    fn measured(floor_db: f32, speech_rms_db: f32, speech_peak_db: f32) -> Measurement {
        Measurement {
            floor_db,
            silence_rms_db: floor_db + 2.0,
            low_band_share: 0.1,
            speech_rms_db,
            speech_peak_db,
            clipped_ratio: 0.0,
        }
    }

    fn recommend(m: &Measurement) -> Recommendation {
        Recommendation::from_measurement(m)
    }

    // ---- the run ----------------------------------------------------------------------------

    #[test]
    fn a_whole_run_holds_the_microphone_resets_each_phase_on_entry_and_lets_go_after_the_last() {
        let (wizard, commands) = run(MIC, &quiet_room(), &talking(), &shouting());
        assert_eq!(
            commands,
            [
                Command::KeepInputAwake(true),
                Command::ResetCaptureStats,
                Command::ResetCaptureStats,
                Command::ResetCaptureStats,
                Command::KeepInputAwake(false),
            ]
        );
        assert_eq!(wizard.phase(), CalibrationPhase::Result);
        assert!(!wizard.is_awake());
        assert!(!wizard.is_live());

        let result = wizard.result().expect("a result");
        assert_eq!(result.microphone.node_name, MIC.0);
        let m = result.measurement;
        assert!((m.silence_rms_db + 55.0).abs() < 0.01, "{m:?}");
        assert!(
            (m.floor_db + 57.0).abs() < 0.01,
            "the quietest block, two under the RMS: {m:?}"
        );
        assert!((m.speech_rms_db + 28.0).abs() < 0.01, "{m:?}");
        assert!((m.speech_peak_db + 8.0).abs() < 0.01, "{m:?}");
        assert_eq!(m.clipped_ratio, 0.0);

        let view = wizard.view(Instant::now());
        assert_eq!(view.phase, CalibrationPhase::Result);
        let shown = view.result.as_ref().expect("the table");
        assert_eq!(shown.preset, CLEAN_VOICE);
        assert_eq!(shown.lines.len(), 6);
        assert!(view.can_start(), "Retry is live");
        assert!(view.can_apply());
    }

    #[test]
    fn the_countdown_waits_for_the_lane_then_runs_down_each_phase() {
        let t0 = Instant::now();
        let idle = Meters::default();
        let mut wizard = CalibrationState::open(&lane(Some(MIC), &idle), t0);
        wizard.start(t0);
        let asleep = Lane {
            processing: false,
            ..lane(Some(MIC), &idle)
        };
        wizard.tick(at(t0, 1.5), &asleep);
        let view = wizard.view(at(t0, 1.5));
        assert_eq!(
            view.phase,
            CalibrationPhase::Silence,
            "the silence page, waiting"
        );
        assert_eq!((view.seconds_left, view.phase_fraction), (3.0, 0.0));
        assert_eq!(
            view.level_db,
            f32::NEG_INFINITY,
            "no level while nothing flows"
        );
        assert_eq!(view.seconds_text(), "3");

        // The lane wakes at 1.5 s: the three seconds start from there.
        let room = quiet_room();
        wizard.tick(at(t0, 1.5), &lane(Some(MIC), &room));
        let view = wizard.view(at(t0, 2.25));
        assert!((view.seconds_left - 2.25).abs() < 1e-3, "{view:?}");
        assert!((view.phase_fraction - 0.25).abs() < 1e-3, "{view:?}");
        assert_eq!(
            view.level_db, -55.0,
            "the live level is the lane's short-window RMS"
        );

        wizard.tick(at(t0, 4.5), &lane(Some(MIC), &room));
        let view = wizard.view(at(t0, 5.75));
        assert_eq!(view.phase, CalibrationPhase::Speech);
        assert!((view.seconds_left - 3.75).abs() < 1e-3, "{view:?}");
        assert_eq!(
            view.seconds_text(),
            "4",
            "rounded up, never 0 while it runs"
        );

        let speech = talking();
        wizard.tick(at(t0, 9.5), &lane(Some(MIC), &speech));
        assert_eq!(wizard.view(at(t0, 9.5)).phase, CalibrationPhase::Loud);
        let loud = shouting();
        wizard.tick(at(t0, 11.5), &lane(Some(MIC), &loud));
        let view = wizard.view(at(t0, 11.75));
        assert_eq!(view.phase, CalibrationPhase::Analysing);
        assert!((view.phase_fraction - 0.5).abs() < 1e-3, "{view:?}");
        wizard.tick(at(t0, 11.75), &lane(Some(MIC), &loud));
        assert_eq!(
            wizard.phase(),
            CalibrationPhase::Analysing,
            "half a second on the page"
        );
        wizard.tick(at(t0, 12.0), &lane(Some(MIC), &loud));
        assert_eq!(wizard.phase(), CalibrationPhase::Result);
    }

    #[test]
    fn a_lane_that_does_not_wake_in_five_seconds_fails_saying_which_part_was_missing() {
        let idle = Meters::default();
        for (microphone, failure) in [
            (None, Failure::NoMicrophone),
            (Some(MIC), Failure::NotStarted),
        ] {
            let t0 = Instant::now();
            let mut wizard = CalibrationState::open(&lane(Some(MIC), &idle), t0);
            assert_eq!(wizard.start(t0), [Command::KeepInputAwake(true)]);
            let asleep = Lane {
                microphone,
                processing: false,
                channels: 1,
                meters: &idle,
            };
            assert!(
                wizard.tick(at(t0, 4.9), &asleep).is_empty(),
                "still waiting"
            );
            assert_eq!(
                wizard.tick(at(t0, 5.0), &asleep),
                [Command::KeepInputAwake(false)],
                "{failure:?} lets the microphone go"
            );
            assert_eq!(wizard.phase(), CalibrationPhase::Failed);
            assert_eq!(wizard.failure(), Some(failure));
            assert_eq!(wizard.view(at(t0, 5.0)).failure, failure.text());
        }
        assert_eq!(Failure::NoMicrophone.text(), "No microphone is selected.");
    }

    #[test]
    fn a_bluetooth_microphone_that_hands_over_digital_zeros_fails_after_the_silence() {
        let zeros = counted(3.0, -300.0, -300.0, 0, -100.0);
        let t0 = Instant::now();
        let (mut wizard, _) = measuring(HEADPHONES, t0);
        assert_eq!(
            wizard.tick(at(t0, 3.0), &lane(Some(HEADPHONES), &zeros)),
            [Command::KeepInputAwake(false)]
        );
        assert_eq!(wizard.failure(), Some(Failure::NoSignal));
        assert_eq!(
            Failure::NoSignal.text(),
            "The microphone delivered no signal."
        );

        // The same zeros from a wired interface with a hardware gate are a silent room: the run
        // goes on to the speech.
        let (mut wired, _) = measuring(MIC, t0);
        assert_eq!(
            wired.tick(at(t0, 3.0), &lane(Some(MIC), &zeros)),
            [Command::ResetCaptureStats]
        );
        assert_eq!(wired.phase(), CalibrationPhase::Speech);
    }

    #[test]
    fn a_bluetooth_microphone_whose_floor_estimator_is_still_climbing_is_not_taken_for_silence() {
        // The headset delivered zeros until it switched profile, so the lane's running estimator
        // is still near its bottom; the phase's own blocks say there was a room.
        let climbing = Meters {
            noise_floor_db: -99.0,
            ..counted(3.0, -62.0, -50.0, 0, -63.0)
        };
        let t0 = Instant::now();
        let (mut wizard, _) = measuring(HEADPHONES, t0);
        wizard.tick(at(t0, 3.0), &lane(Some(HEADPHONES), &climbing));
        assert_eq!(wizard.phase(), CalibrationPhase::Speech);
    }

    #[test]
    fn a_bluetooth_headset_is_measured_from_its_first_sound_and_not_through_its_profile_switch() {
        // Start holds the microphone open, WirePlumber switches the headset to its hands-free
        // profile, and the loopback carries digital zeros until the switch is done — processing,
        // as far as the lane is concerned. The silence phase used to begin at once and average
        // a second and a half of those zeros into a −58 dBFS room, with the running estimator
        // pulled to its bottom by them: a floor of −67 where the room is −59, denoising Off
        // instead of Light and a gate under the room's own level. It waits for the first sound.
        let t0 = Instant::now();
        let idle = Meters::default();
        let mut wizard = CalibrationState::open(&lane(Some(HEADPHONES), &idle), t0);
        assert_eq!(wizard.start(t0), [Command::KeepInputAwake(true)]);
        let switching = Meters {
            input_rms_db: DIGITAL_SILENCE_DB,
            noise_floor_db: DIGITAL_SILENCE_DB,
            ..Meters::default()
        };
        for seconds in [0.1, 0.8, 1.5] {
            assert!(
                wizard
                    .tick(at(t0, seconds), &lane(Some(HEADPHONES), &switching))
                    .is_empty(),
                "at {seconds} s the headset is still switching"
            );
            let view = wizard.view(at(t0, seconds));
            assert_eq!(view.phase, CalibrationPhase::Silence);
            assert_eq!(view.seconds_left, 3.0, "the countdown waits");
        }
        let arriving = Meters {
            input_rms_db: -58.0,
            noise_floor_db: -99.5,
            ..Meters::default()
        };
        assert_eq!(
            wizard.tick(at(t0, 1.5), &lane(Some(HEADPHONES), &arriving)),
            [Command::ResetCaptureStats],
            "the phase starts with the first sound"
        );

        // Three seconds of the room itself, the estimator still climbing out of the zeros.
        let room = Meters {
            noise_floor_db: -98.0,
            ..counted(3.0, -58.0, -46.0, 0, -59.0)
        };
        wizard.tick(at(t0, 4.5), &lane(Some(HEADPHONES), &room));
        wizard.tick(at(t0, 9.5), &lane(Some(HEADPHONES), &talking()));
        wizard.tick(at(t0, 11.5), &lane(Some(HEADPHONES), &shouting()));
        wizard.tick(at(t0, 12.0), &lane(Some(HEADPHONES), &shouting()));
        let result = wizard.result().expect("a result");
        assert_eq!(result.measurement.floor_db, -59.0, "{result:?}");
        assert_eq!(result.recommendation.denoise, DenoiseLevel::Light);
        assert_eq!(result.recommendation.gate_threshold_db, -51.0);
    }

    #[test]
    fn a_bluetooth_headset_that_sends_only_zeros_through_the_wake_up_delivered_no_signal() {
        let t0 = Instant::now();
        let idle = Meters::default();
        let mut wizard = CalibrationState::open(&lane(Some(HEADPHONES), &idle), t0);
        wizard.start(t0);
        let zeros = Meters {
            input_rms_db: DIGITAL_SILENCE_DB,
            ..Meters::default()
        };
        assert!(
            wizard
                .tick(at(t0, 4.9), &lane(Some(HEADPHONES), &zeros))
                .is_empty()
        );
        assert_eq!(
            wizard.tick(at(t0, 5.0), &lane(Some(HEADPHONES), &zeros)),
            [Command::KeepInputAwake(false)]
        );
        assert_eq!(wizard.failure(), Some(Failure::NoSignal));

        // A wired microphone's zeros are a silent room, and the phase begins with them.
        let mut wired = CalibrationState::open(&lane(Some(MIC), &idle), t0);
        wired.start(t0);
        assert_eq!(
            wired.tick(at(t0, 0.1), &lane(Some(MIC), &zeros)),
            [Command::ResetCaptureStats]
        );
    }

    #[test]
    fn a_hardware_mute_lifted_just_before_start_does_not_pull_the_floor_down() {
        // A wired microphone muted on its own switch until a moment before Start: the lane's
        // running estimator followed the mute to −100 dBFS and climbs back at half a decibel a
        // second, so at the end of the silence it still reads −95 against a −55 dBFS room. The
        // floor used to come out six under the room's RMS on that account; the phase's own
        // quietest block is the room.
        let unmuted = Meters {
            noise_floor_db: -95.0,
            ..counted(3.0, -55.0, -40.0, 0, -56.0)
        };
        let (wizard, _) = run(MIC, &unmuted, &talking(), &shouting());
        let result = wizard.result().expect("a result");
        assert_eq!(result.measurement.floor_db, -56.0, "{result:?}");
    }

    #[test]
    fn speech_no_louder_than_the_room_fails_and_silent_speech_is_no_signal() {
        let t0 = Instant::now();
        for (speech, failure) in [
            (counted(5.0, -52.0, -40.0, 0, -56.0), Failure::NoSpeech),
            (counted(5.0, -300.0, -300.0, 0, -100.0), Failure::NoSignal),
        ] {
            let (mut wizard, _) = measuring(MIC, t0);
            wizard.tick(at(t0, 3.0), &lane(Some(MIC), &quiet_room()));
            assert_eq!(
                wizard.tick(at(t0, 8.0), &lane(Some(MIC), &speech)),
                [Command::KeepInputAwake(false)]
            );
            assert_eq!(wizard.failure(), Some(failure));
        }
        // Six decibels over the floor is enough to go on.
        let (mut wizard, _) = measuring(MIC, t0);
        wizard.tick(at(t0, 3.0), &lane(Some(MIC), &quiet_room()));
        wizard.tick(
            at(t0, 8.0),
            &lane(Some(MIC), &counted(5.0, -51.0, -30.0, 0, -57.0)),
        );
        assert_eq!(wizard.phase(), CalibrationPhase::Loud);
    }

    #[test]
    fn a_phase_that_counted_nothing_or_ended_on_a_stopped_lane_fails_as_no_signal() {
        let t0 = Instant::now();
        let (mut wizard, _) = measuring(MIC, t0);
        wizard.tick(at(t0, 3.0), &lane(Some(MIC), &Meters::default()));
        assert_eq!(wizard.failure(), Some(Failure::NoSignal));

        let (mut wizard, _) = measuring(MIC, t0);
        wizard.tick(at(t0, 3.0), &lane(Some(MIC), &quiet_room()));
        let speech = talking();
        let stopped = Lane {
            processing: false,
            ..lane(Some(MIC), &speech)
        };
        assert_eq!(
            wizard.tick(at(t0, 8.0), &stopped),
            [Command::KeepInputAwake(false)]
        );
        assert_eq!(wizard.failure(), Some(Failure::NoSignal));
    }

    #[test]
    fn a_microphone_that_changes_or_goes_in_the_middle_of_a_run_fails_it() {
        let t0 = Instant::now();
        for moved in [Some(OTHER), None] {
            let (mut wizard, _) = measuring(MIC, t0);
            wizard.tick(at(t0, 3.0), &lane(Some(MIC), &quiet_room()));
            assert_eq!(
                wizard.tick(at(t0, 4.0), &lane(moved, &talking())),
                [Command::KeepInputAwake(false)]
            );
            assert_eq!(wizard.failure(), Some(Failure::MicrophoneChanged));
        }
    }

    /// Drive a fresh wizard on [`MIC`] to `stage`, returning it and every command so far.
    fn at_stage(stage: Stage, t0: Instant) -> (CalibrationState, Vec<Command>) {
        let idle = Meters::default();
        let mut wizard = CalibrationState::open(&lane(Some(MIC), &idle), t0);
        let mut commands = Vec::new();
        let script: [(f32, Meters); 5] = [
            (0.0, idle),
            (3.0, quiet_room()),
            (8.0, talking()),
            (10.0, shouting()),
            (10.5, shouting()),
        ];
        let steps = match stage {
            Stage::Intro => return (wizard, commands),
            Stage::Waking => 0,
            Stage::Silence => 1,
            Stage::Speech => 2,
            Stage::Loud => 3,
            Stage::Analysing => 4,
            Stage::Result => 5,
            Stage::Failed => {
                commands.extend(wizard.start(t0));
                let asleep = Lane {
                    microphone: None,
                    processing: false,
                    channels: 1,
                    meters: &idle,
                };
                commands.extend(wizard.tick(at(t0, 5.0), &asleep));
                return (wizard, commands);
            }
        };
        commands.extend(wizard.start(t0));
        for (seconds, meters) in script.iter().take(steps) {
            commands.extend(wizard.tick(at(t0, *seconds), &lane(Some(MIC), meters)));
        }
        assert_eq!(wizard.stage, stage);
        (wizard, commands)
    }

    fn held(commands: &[Command]) -> i32 {
        commands
            .iter()
            .map(|command| match command {
                Command::KeepInputAwake(true) => 1,
                Command::KeepInputAwake(false) => -1,
                Command::ResetCaptureStats => 0,
            })
            .sum()
    }

    #[test]
    fn cancel_at_every_phase_leaves_the_microphone_released_exactly_once() {
        let t0 = Instant::now();
        for stage in [
            Stage::Intro,
            Stage::Waking,
            Stage::Silence,
            Stage::Speech,
            Stage::Loud,
            Stage::Analysing,
            Stage::Result,
            Stage::Failed,
        ] {
            let (mut wizard, mut commands) = at_stage(stage, t0);
            let was_held = wizard.is_awake();
            assert_eq!(
                held(&commands),
                i32::from(was_held),
                "{stage:?}: the hold is outstanding exactly while the machine says so"
            );
            let cancel = wizard.cancel();
            assert_eq!(
                cancel,
                if was_held {
                    vec![Command::KeepInputAwake(false)]
                } else {
                    Vec::new()
                },
                "{stage:?}"
            );
            commands.extend(cancel);
            commands.extend(wizard.cancel());
            assert_eq!(held(&commands), 0, "{stage:?}: released, and only once");
            assert!(
                commands
                    .iter()
                    .filter(|c| **c == Command::KeepInputAwake(false))
                    .count()
                    <= 1,
                "{stage:?}"
            );
        }
    }

    #[test]
    fn the_microphone_is_held_only_while_it_is_being_listened_to() {
        let t0 = Instant::now();
        for stage in [Stage::Waking, Stage::Silence, Stage::Speech, Stage::Loud] {
            assert!(at_stage(stage, t0).0.is_awake(), "{stage:?}");
        }
        for stage in [Stage::Intro, Stage::Analysing, Stage::Result, Stage::Failed] {
            assert!(!at_stage(stage, t0).0.is_awake(), "{stage:?}");
        }
    }

    #[test]
    fn retry_holds_the_microphone_again_and_measures_from_the_top() {
        let t0 = Instant::now();
        for stage in [Stage::Result, Stage::Failed] {
            let (mut wizard, mut commands) = at_stage(stage, t0);
            let again = at(t0, 20.0);
            // The failure here was the microphone going; Retry waits for one to be back.
            let idle = Meters::default();
            wizard.tick(again, &lane(Some(MIC), &idle));
            let retry = wizard.start(again);
            assert_eq!(retry, [Command::KeepInputAwake(true)], "{stage:?}");
            commands.extend(retry);
            assert_eq!(held(&commands), 1);
            assert!(wizard.result().is_none() && wizard.failure().is_none());
            let view = wizard.view(again);
            assert_eq!(view.phase, CalibrationPhase::Silence);
            assert!(view.result.is_none() && view.failure.is_empty());
        }
    }

    #[test]
    fn start_does_nothing_without_a_microphone_or_in_the_middle_of_a_run() {
        let t0 = Instant::now();
        let idle = Meters::default();
        let mut alone = CalibrationState::open(&lane(None, &idle), t0);
        assert!(alone.start(t0).is_empty());
        assert_eq!(alone.phase(), CalibrationPhase::Intro);
        assert!(!alone.view(t0).can_start());

        for stage in [Stage::Waking, Stage::Silence, Stage::Loud, Stage::Analysing] {
            let (mut wizard, _) = at_stage(stage, t0);
            assert!(wizard.start(at(t0, 1.0)).is_empty(), "{stage:?}");
            assert_eq!(wizard.stage, stage);
        }
    }

    #[test]
    fn between_runs_the_wizard_follows_the_lane_and_a_result_keeps_the_microphone_it_measured() {
        let t0 = Instant::now();
        let idle = Meters::default();
        let mut wizard = CalibrationState::open(&lane(Some(MIC), &idle), t0);
        wizard.tick(t0, &lane(Some(OTHER), &idle));
        assert_eq!(wizard.view(t0).device, OTHER.1);
        wizard.tick(t0, &lane(None, &idle));
        assert_eq!(wizard.view(t0).device, OTHER.1, "the last one stays named");
        assert!(!wizard.view(t0).can_start(), "but Start waits for one");

        let (mut done, _) = at_stage(Stage::Result, t0);
        done.tick(at(t0, 11.0), &lane(Some(OTHER), &idle));
        assert_eq!(done.view(t0).device, MIC.1);
        assert_eq!(done.result().expect("result").microphone.node_name, MIC.0);
    }

    #[test]
    fn rumble_in_the_silence_moves_the_high_pass_to_120_hz() {
        let mut room = quiet_room();
        // As the visualizer shows it: the two lowest bands, warped down by 0.6, still well up.
        room.spectrum = [0.3, 0.3, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05];
        let (wizard, _) = run(MIC, &room, &talking(), &shouting());
        let result = wizard.result().expect("a result");
        assert!(result.measurement.low_band_share > LOW_BAND_LIMIT);
        assert_eq!(result.recommendation.highpass_hz, 120.0);

        let (flat, _) = run(MIC, &quiet_room(), &talking(), &shouting());
        assert_eq!(
            flat.result().expect("result").recommendation.highpass_hz,
            80.0
        );
    }

    #[test]
    fn the_clipped_share_counts_samples_on_every_channel() {
        let t0 = Instant::now();
        let (mut wizard, _) = measuring(MIC, t0);
        wizard.tick(at(t0, 3.0), &lane(Some(MIC), &quiet_room()));
        wizard.tick(at(t0, 8.0), &lane(Some(MIC), &talking()));
        // 2 s at 48 kHz in stereo is 192 000 samples; 192 of them clipped is 0.1 %.
        let loud = counted(2.0, -12.0, 0.0, 192, -56.0);
        let stereo = Lane {
            channels: 2,
            ..lane(Some(MIC), &loud)
        };
        wizard.tick(at(t0, 10.0), &stereo);
        wizard.tick(at(t0, 10.5), &stereo);
        let m = wizard.result().expect("result").measurement;
        assert!((m.clipped_ratio - 0.001).abs() < 1e-6, "{m:?}");
        assert!((wizard.view(t0).result.expect("table").clipped_percent - 0.1).abs() < 1e-4);
    }

    // ---- the formulas (design §8) --------------------------------------------------------------

    #[test]
    fn the_gate_is_eight_over_the_floor_backed_off_to_twelve_under_speech_but_never_within_four() {
        // Room to spare: eight over the floor.
        assert_eq!(
            recommend(&measured(-60.0, -25.0, -5.0)).gate_threshold_db,
            -52.0
        );
        // A quiet talker: twelve under the speech, which is less than eight over the floor.
        assert_eq!(
            recommend(&measured(-60.0, -43.0, -30.0)).gate_threshold_db,
            -55.0
        );
        // Speech close to the floor: never nearer the floor than four, even where the two bounds
        // cross.
        assert_eq!(
            recommend(&measured(-60.0, -52.0, -40.0)).gate_threshold_db,
            -56.0
        );
        // Rounded up to a whole decibel, so the four are kept.
        assert_eq!(
            recommend(&measured(-60.4, -52.0, -40.0)).gate_threshold_db,
            -56.0
        );
        // And held to what the gate accepts.
        assert_eq!(
            recommend(&measured(-120.0, -30.0, -10.0)).gate_threshold_db,
            -90.0
        );
    }

    #[test]
    fn the_compressor_starts_six_under_the_speaking_level_at_three_to_one() {
        let r = recommend(&measured(-60.0, -25.0, -5.0));
        assert_eq!(
            (r.compressor_threshold_db, r.compressor_ratio),
            (-31.0, 3.0)
        );
        assert_eq!(
            recommend(&measured(-90.0, -70.0, -50.0)).compressor_threshold_db,
            -60.0
        );
    }

    #[test]
    fn the_makeup_brings_compressed_speech_to_minus_eighteen_and_stops_at_eighteen_either_way() {
        // −25 RMS, −4 of compression: −29, so +11 to reach −18.
        assert_eq!(recommend(&measured(-60.0, -25.0, -5.0)).makeup_db, 11.0);
        assert_eq!(recommend(&measured(-60.0, -14.0, -2.0)).makeup_db, 0.0);
        assert_eq!(recommend(&measured(-80.0, -50.0, -30.0)).makeup_db, 18.0);
        assert_eq!(recommend(&measured(-40.0, 10.0, 12.0)).makeup_db, -18.0);
    }

    #[test]
    fn the_ceiling_comes_down_to_minus_six_only_past_a_thousandth_of_the_samples_clipping() {
        let mut m = measured(-60.0, -25.0, -5.0);
        m.clipped_ratio = 0.001;
        assert_eq!(recommend(&m).ceiling_db, -3.0);
        m.clipped_ratio = 0.0011;
        assert_eq!(recommend(&m).ceiling_db, -6.0);
    }

    #[test]
    fn the_denoiser_follows_the_floor_in_ten_decibel_steps() {
        for (floor, level) in [
            (-75.0, DenoiseLevel::Off),
            (-60.5, DenoiseLevel::Off),
            (-60.0, DenoiseLevel::Light),
            (-50.5, DenoiseLevel::Light),
            (-50.0, DenoiseLevel::Medium),
            (-40.5, DenoiseLevel::Medium),
            (-40.0, DenoiseLevel::Strong),
            (-30.0, DenoiseLevel::Strong),
        ] {
            assert_eq!(
                recommend(&measured(floor, floor + 25.0, floor + 45.0)).denoise,
                level,
                "{floor}"
            );
        }
    }

    #[test]
    fn the_high_pass_goes_to_120_hz_only_past_thirty_percent_of_the_silence_in_the_lowest_bands() {
        let mut m = measured(-60.0, -25.0, -5.0);
        m.low_band_share = 0.30;
        assert_eq!(recommend(&m).highpass_hz, 80.0);
        m.low_band_share = 0.31;
        assert_eq!(recommend(&m).highpass_hz, 120.0);
    }

    #[test]
    fn a_low_crest_is_a_headset_a_loud_room_a_laptop_and_anything_else_clean_voice() {
        // Peaks 9 dB over the RMS: a boom close to the mouth, whatever the room.
        assert_eq!(recommend(&measured(-40.0, -20.0, -11.0)).preset, HEADSET);
        // A crest of 10 is not low.
        assert_eq!(recommend(&measured(-44.0, -20.0, -10.0)).preset, LAPTOP_MIC);
        assert_eq!(recommend(&measured(-45.0, -20.0, -2.0)).preset, CLEAN_VOICE);
        assert_eq!(recommend(&measured(-65.0, -25.0, -5.0)).preset, CLEAN_VOICE);
    }

    #[test]
    fn the_floor_is_the_quietest_block_held_between_the_silences_rms_and_six_under_it() {
        let reading = |rms_db: f32, quietest_db: f32| Reading {
            frames: 1,
            rms_db,
            peak_db: rms_db,
            clipped: 0,
            quietest_db,
        };
        assert_eq!(floor_db(&reading(-50.0, -53.0)), -53.0);
        // A cough in the silence: the RMS rose, the quietest block did not.
        assert_eq!(floor_db(&reading(-40.0, -53.0)), -46.0);
        // One quiet moment in a room that is not steady.
        assert_eq!(floor_db(&reading(-60.0, -100.0)), -66.0);
        // A block over the RMS — the phase's zeros left out of one and not the other — is not
        // believed either.
        assert_eq!(floor_db(&reading(-60.0, -58.0)), -60.0);
        assert_eq!(floor_db(&reading(-60.0, f32::NAN)), -60.0);
    }

    #[test]
    fn the_lowest_bands_share_divides_out_the_visualizers_warp() {
        let mut bands = LowBands::default();
        assert_eq!(bands.share(), 0.0, "nothing heard is no rumble");
        // Every band at its own warp: equal energy in all ten once unwarped.
        let mut even = [0.0; NUM_SPECTRUM_BARS];
        for (level, warp) in even.iter_mut().zip(fxsound_dsp::spectrum::BAND_WARP) {
            *level = 0.1 * warp;
        }
        bands.add(&even);
        bands.add(&even);
        assert!((bands.share() - 0.2).abs() < 1e-6, "{}", bands.share());
    }

    fn shipped(name: &str) -> InputPreset {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/presets/Input")
            .join(format!("{name}.toml"));
        InputPreset::load(&path).expect("a shipped voice preset")
    }

    #[test]
    fn the_numbers_go_over_the_shipped_preset_and_everything_else_is_kept() {
        let base = shipped(HEADSET);
        let mut m = measured(-48.0, -24.0, -16.0);
        m.clipped_ratio = 0.01;
        let r = recommend(&m);
        assert_eq!(r.preset, HEADSET);
        let preset = r.preset(base.clone(), "Calibrated — Boom", "measured".to_owned());

        assert_eq!(preset.name, "Calibrated — Boom");
        assert_eq!(preset.description, "measured");
        assert_eq!(preset.highpass_hz, 80.0);
        assert_eq!(
            preset.highpass_order, base.highpass_order,
            "the order is the preset's"
        );
        let gate = preset.gate.clone().expect("a gate");
        assert_eq!(gate.threshold_db, -40.0);
        assert_eq!(gate.detection, Detection::Rms);
        assert_eq!(
            gate.range_db,
            base.gate.as_ref().expect("base gate").range_db
        );
        let compressor = preset.compressor.clone().expect("a compressor");
        assert_eq!((compressor.threshold_db, compressor.ratio), (-30.0, 3.0));
        assert_eq!(
            compressor.attack_ms,
            base.compressor.as_ref().expect("base compressor").attack_ms
        );
        assert_eq!((preset.makeup_db, preset.ceiling_db), (10.0, -6.0));
        assert_eq!(preset.denoise_level(), DenoiseLevel::Medium);
        assert!(preset.rnnoise);
        assert_eq!(preset.eq, base.eq);
        assert_eq!(preset.deesser, base.deesser);
        assert_eq!(preset.chain, base.chain);

        // What the chain would run, and nothing the engine would have to clamp.
        let params = preset.to_params();
        let mut sanitised = params;
        sanitised.sanitise();
        assert_eq!(params, sanitised);
        assert_eq!(params.gate_threshold_db, -40.0);
        assert!(params.gate_on && params.compressor_on);
    }

    #[test]
    fn the_denoiser_table_follows_the_recommendation_and_off_removes_it() {
        let laptop = shipped(LAPTOP_MIC);
        assert_eq!(laptop.denoise_level(), DenoiseLevel::Medium);
        let mut same = recommend(&measured(-45.0, -20.0, -2.0));
        assert_eq!(same.denoise, DenoiseLevel::Medium);
        let kept = same.preset(laptop.clone(), "a", String::new());
        assert_eq!(
            kept.denoise, laptop.denoise,
            "the base's own tuning of the level"
        );

        same.denoise = DenoiseLevel::Off;
        let off = same.preset(laptop.clone(), "a", String::new());
        assert_eq!((off.denoise.clone(), off.rnnoise), (None, false));

        let clean = shipped(CLEAN_VOICE);
        let strong = recommend(&measured(-30.0, -10.0, 8.0));
        assert_eq!(strong.denoise, DenoiseLevel::Strong);
        let on = strong.preset(clean, "a", String::new());
        assert_eq!(on.denoise_level(), DenoiseLevel::Strong);
        assert!(on.rnnoise);
    }

    #[test]
    fn a_base_with_stages_switched_off_gets_them_from_the_reference_voice() {
        let bare = InputPreset {
            gate: None,
            compressor: None,
            highpass_order: 0,
            ..InputPreset::default()
        };
        let preset = recommend(&measured(-60.0, -25.0, -5.0)).preset(bare, "a", String::new());
        assert!(preset.gate.is_some() && preset.compressor.is_some());
        assert_eq!(preset.highpass_order, InputPreset::default().highpass_order);
    }

    #[test]
    fn the_calibrated_name_is_one_the_preset_name_editor_would_have_accepted() {
        assert_eq!(
            calibrated_preset_name("fifine Microphone Analogue Stereo"),
            "Calibrated — fifine Microphone Analogue Stereo"
        );
        assert_eq!(
            calibrated_preset_name("USB Audio: \"Pro\"  <Mic> / 2\t"),
            "Calibrated — USB Audio Pro Mic 2"
        );
        assert_eq!(calibrated_preset_name(" ?* "), "Calibrated");
        let long = calibrated_preset_name(&"Very Long Microphone Name ".repeat(5));
        assert!(
            long.chars().count() <= crate::app::MAX_PRESET_NAME_CHARS,
            "{long}"
        );
        assert!(!long.ends_with(' '));
        // 0.4.0 audit #15: a Cyrillic device name reaches the bytes a Windows FxSound reads a name
        // in before it reaches sixty-four characters.
        let cyrillic = calibrated_preset_name(&"Микрофон ".repeat(8));
        assert!(
            cyrillic.len() <= fxsound_preset::MAX_NAME_BYTES,
            "{cyrillic}"
        );
        assert!(cyrillic.starts_with("Calibrated — Микрофон"));
        assert!(!cyrillic.ends_with(' '));
    }

    #[test]
    fn every_line_names_its_setting_and_carries_its_number() {
        let mut m = measured(-55.0, -25.0, -5.0);
        m.low_band_share = 0.5;
        assert_eq!(
            recommend(&m).lines(),
            [
                "High-pass 120 Hz",
                "Gate −47 dB",
                "Compressor −31 dB, ratio 3:1",
                "Makeup gain +11 dB",
                "Ceiling −3 dB",
                "Noise suppression: Mild",
            ]
        );
    }

    #[test]
    fn a_gain_reads_with_its_sign_and_a_real_minus() {
        assert_eq!(signed_db(6.2), "+6 dB");
        assert_eq!(signed_db(-41.6), "−42 dB");
        assert_eq!(signed_db(-0.3), "0 dB");
        assert_eq!(signed_db(0.0), "0 dB");
    }

    #[test]
    fn only_a_bluez_node_is_a_bluetooth_microphone() {
        let named = |node_name: &str| Microphone {
            node_name: node_name.to_owned(),
            description: String::new(),
        };
        assert!(named("bluez_input.00:11:22:33:44:55").is_bluetooth());
        assert!(named("bluez_input.00_11_22_33_44_55.0").is_bluetooth());
        assert!(named("bluez_source.00_11_22_33_44_55.headset_head_unit").is_bluetooth());
        assert!(!named("alsa_input.usb-Blue_Yeti").is_bluetooth());
    }

    #[test]
    fn every_language_fits_each_reason_in_its_two_lines_and_each_setting_on_its_line() {
        use eframe::egui;
        use fxsound_core::i18n::{Catalogue, LANGUAGES};
        use fxsound_ui::dialogs::calibration::{failure_rect, line_rect, wizard};
        use fxsound_ui::dialogs::{content_rect, small_font};

        // The English keys, as `Failure::text` passes them to `tr`; each numeric line at its
        // widest value, and the noise-suppression line at every level below.
        let failure_keys = [
            "No microphone is selected.",
            "The microphone sent no sound within 5 seconds.",
            "The microphone delivered no signal.",
            "No speech was heard. Speak closer to the microphone.",
            "The microphone changed during the measurement.",
        ];
        let lines: [(&str, &[&str]); 5] = [
            ("High-pass %s", &["120 Hz"]),
            ("Gate %s", &["−90 dB"]),
            ("Compressor %s, ratio %s", &["−60 dB", "3:1"]),
            ("Makeup gain %s", &["+18 dB"]),
            ("Ceiling %s", &["−6 dB"]),
        ];

        let ctx = egui::Context::default();
        ctx.set_fonts(fxsound_ui::theme::font_definitions());
        let result = content_rect(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            wizard::RESULT_WINDOW_SIZE,
        ));
        let page = content_rect(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            wizard::WINDOW_SIZE,
        ));
        let mut problems = Vec::new();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter();
            // The measure is real: three lines of text do not fit in two.
            let three = painter
                .layout(
                    "word ".repeat(40),
                    small_font(),
                    egui::Color32::PLACEHOLDER,
                    failure_rect(page).width(),
                )
                .size()
                .y;
            assert!(three > failure_rect(page).height(), "{three}");
            for language in &LANGUAGES {
                let table = (language.code != "en").then(|| Catalogue::for_language(language));
                let say = |key: &str| {
                    table
                        .as_ref()
                        .and_then(|t| t.get(key))
                        .unwrap_or(key)
                        .to_owned()
                };
                for key in &failure_keys {
                    let text = say(key);
                    let height = painter
                        .layout(
                            text.clone(),
                            small_font(),
                            egui::Color32::PLACEHOLDER,
                            failure_rect(page).width(),
                        )
                        .size()
                        .y;
                    if height > failure_rect(page).height() {
                        problems.push(format!("{}: {text:?} is {height:.0} tall", language.code));
                    }
                }
                let numbers = lines.iter().map(|(key, values)| {
                    values
                        .iter()
                        .fold(say(key), |text, value| text.replacen("%s", value, 1))
                });
                // Each level, translated as `Recommendation::lines` translates it: which is the
                // widest depends on the language — "Off" is, in French and Polish.
                let levels = DenoiseLevel::ALL.iter().map(|level| {
                    say("Noise suppression: %s").replacen("%s", &say(level.label()), 1)
                });
                for text in numbers.chain(levels) {
                    let width = painter
                        .layout_no_wrap(text.clone(), small_font(), egui::Color32::PLACEHOLDER)
                        .size()
                        .x;
                    if width > line_rect(result, 0).width() {
                        problems.push(format!("{}: {text:?} is {width:.0} wide", language.code));
                    }
                }
            }
        })
        .drop_without_applying_deltas();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
